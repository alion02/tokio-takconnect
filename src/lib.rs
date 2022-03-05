use std::{
    collections::VecDeque,
    error::Error,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{
    stream::{self, SplitSink},
    Future, FutureExt, SinkExt, StreamExt,
};

use parking_lot::Mutex;
use tokio::{
    join, spawn,
    sync::{
        mpsc::{unbounded_channel, UnboundedSender},
        oneshot::{channel, Sender},
    },
    time::interval,
};
use tokio_tungstenite::connect_async;

use rand::{distributions::Uniform, prelude::StdRng, Rng, SeedableRng};

use log::{debug, error, info, trace, warn};

pub async fn connect_guest() -> Client {
    connect().await
}

async fn connect() -> Client {
    let token = StdRng::from_entropy()
        .sample_iter(Uniform::from(b'a'..=b'z'))
        .take(20)
        .map(char::from)
        .collect::<String>();

    internal_connect(
        "Guest".to_string(),
        token,
        "unknown",
        true,
        Duration::from_millis(2_000),
    )
    .await
    .unwrap()
}

async fn internal_connect(
    username: String,
    password: String,
    client_name: &str,
    append_lib_name: bool,
    ping_interval: Duration,
) -> Result<Client, Box<dyn Error>> {
    let (tx, mut rx) = unbounded_channel::<(String, _)>();

    {
        info!("Establishing WebSocket Secure connnection to Playtak server");
        let mut stream = connect_async("wss://playtak.com/ws")
            .await
            .unwrap()
            .0
            .split();

        let queue = Arc::new(Mutex::new(VecDeque::<Sender<String>>::new()));
        {
            let queue = queue.clone();
            spawn(async move {
                while let Some((message, tx)) = rx.recv().await {
                    {
                        let mut queue = queue.lock();
                        let backlog = queue.len();
                        if backlog != 0 {
                            debug!("Sending a command while awaiting response(s) to {backlog} previous command(s)");
                        }
                        queue.push_back(tx);
                    }
                    stream.0.send(message.into()).await.unwrap(); // Is this cancellation safe?
                }
            });
        }
        spawn(async move {
            while let Some(Ok(message)) = stream.1.next().await {
                let message = message.to_text().unwrap().strip_suffix(|_| true).unwrap();
                let (command, rest) = message.split_once([' ', '#', ':']).unwrap_or((message, ""));

                match command {
                    "OK" | "NOK" | "Welcome" => {
                        if let Err(message) =
                            queue.lock().pop_front().unwrap().send(message.to_string())
                        {
                            warn!("Confirmation message \"{message}\" was discarded");
                        }
                    }
                    "Welcome!" | "Login" => {
                        debug!("Ignoring redundant message \"{message}\"");
                    }
                    "Error" => {
                        warn!("Ignoring error message \"{message}\"");
                    }
                    _ => {
                        warn!("Ignoring unknown message \"{message}\"");
                    }
                };
            }
        });
    }

    let client = Client {
        username,
        password,
        tx: tx.clone(),
    };

    info!(
        "Logging in as {}",
        if client.is_guest() {
            "a guest".to_string()
        } else {
            format!("\"{}\"", client.username)
        }
    );

    let client_name = if append_lib_name {
        format!(
            "{}+{}-{}",
            client_name,
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        )
    } else {
        client_name.to_string()
    };

    let (a, b, c) = join!(
        client.send(format!("Client {client_name}")),
        client.send("Protocol 1".to_string()),
        client.send(format!("Login {} {}", client.username, client.password)),
    );
    if a != "OK" {
        warn!("Playtak rejected client name \"{client_name}\"");
    }
    if b != "OK" {
        return Err("Playtak rejected protocol upgrade to version 1".into());
    }
    let _guest_id = c
        .strip_prefix(format!("Welcome {}", client.username).as_str())
        .map(|c| c.strip_suffix(|_| true))
        .flatten()
        .ok_or("Failed to log in with the provided credentials")?;

    info!("Pinging every {ping_interval:?}");

    spawn(async move {
        let mut interval = interval(ping_interval);
        loop {
            interval.tick().await;
            let channel = channel();
            let time = Instant::now();
            tx.send(("PING".to_string(), channel.0)).unwrap();
            spawn(async move {
                if channel.1.await.unwrap() != "OK" {
                    warn!("Playtak rejected PING");
                };
                debug!("Ping: {}ms", time.elapsed().as_millis());
            });
        }
    });

    info!("Client ready");

    Ok(client)
}

pub struct Client {
    username: String,
    password: String,
    tx: UnboundedSender<(String, Sender<String>)>,
}

impl Client {
    pub fn is_guest(&self) -> bool {
        self.username == "Guest"
    }

    async fn send(&self, s: String) -> String {
        let channel = channel();
        self.tx.send((s, channel.0)).unwrap();
        channel.1.await.unwrap()
    }
}
