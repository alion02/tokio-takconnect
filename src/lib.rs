use std::{
    collections::VecDeque,
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
use tokio_tungstenite::{connect_async, WebSocketStream};

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
        Duration::from_millis(2_000),
    )
    .await
}

async fn internal_connect(
    username: String,
    password: String,
    client_name: &str,
    ping_interval: Duration,
) -> Client {
    let (tx, mut rx) = unbounded_channel::<(String, _)>();

    {
        info!("Establishing WebSocket Secure connnection to Playtak server");
        let mut stream = connect_async("wss://playtak.com/ws")
            .await
            .unwrap()
            .0
            .split();

        let queue = Arc::new(Mutex::new(VecDeque::<Sender<PlaytakResponse>>::new()));
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

                match message {
                    "OK" | "NOK" => {
                        queue
                            .lock()
                            .pop_front()
                            .unwrap()
                            .send(if message == "OK" {
                                PlaytakResponse::Ok(())
                            } else {
                                PlaytakResponse::Err(())
                            })
                            .unwrap();
                    }
                    _ => {
                        info!("Ignoring unknown message \"{message}\"");
                    }
                };
            }
        });
    }

    {
        let tx = tx.clone();
        spawn(async move {
            let mut interval = interval(ping_interval);
            loop {
                interval.tick().await;
                let channel = channel();
                let time = Instant::now();
                tx.send(("PING".to_string(), channel.0)).unwrap();
                spawn(async move {
                    channel.1.await.unwrap().unwrap();
                    debug!("Ping: {}ms", time.elapsed().as_millis());
                });
            }
        });
    }

    let client = Client {
        username,
        password,
        tx,
    };

    // {
    //     assert_eq!(
    //         rx.next().await.unwrap(),
    //         "Welcome!",
    //         "server sent unrecognized welcome message"
    //     );
    //     assert_eq!(
    //         rx.next().await.unwrap(),
    //         "Login or Register",
    //         "server forgot to tell us to login or register"
    //     );
    // }

    // tx.send_all(&mut stream::iter(
    //     [
    //         format!(
    //             "Client {}+{}-{}",
    //             client,
    //             env!("CARGO_PKG_NAME"),
    //             env!("CARGO_PKG_VERSION")
    //         ),
    //         "Protocol 1".to_string(),
    //         format!("Login {username} {password}"),
    //     ]
    //     .into_iter()
    //     .map(|s| Ok(s.into())),
    // ))
    // .await
    // .unwrap();

    info!(
        "Logging in as {}",
        if client.is_guest() {
            "a guest".to_string()
        } else {
            format!("\"{}\"", client.username)
        }
    );

    let (a, b, c) = join!(
        client.send(format!(
            "Client {}+{}-{}",
            client_name,
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        )),
        client.send("Protocol 1".to_string()),
        client.send(format!("Login {} {}", client.username, client.password)),
    );
    [a, b, c].into_iter().for_each(|r| r.unwrap());

    info!("Client ready");

    client
}

pub struct Client {
    username: String,
    password: String,
    tx: UnboundedSender<(String, Sender<PlaytakResponse>)>,
}

impl Client {
    pub fn is_guest(&self) -> bool {
        self.username == "Guest"
    }

    fn send(&self, s: String) -> impl Future<Output = PlaytakResponse> {
        let channel = channel();
        self.tx.send((s, channel.0)).unwrap();
        channel.1.map(|r| r.unwrap())
    }
}

type PlaytakResponse = Result<(), ()>;

// impl Client {
//     pub fn quitter(&self) -> Quitter {
//         Quitter::new(self.stream.try_clone().unwrap())
//     }

//     pub fn quit(&self) {
//         self.quitter().quit();
//     }

//     fn read(&mut self) -> String {
//         let mut s = String::new();
//         self.rx.read_line(&mut s);
//         s
//     }

//     pub fn seek(&mut self, opt: SeekOptions) -> Seek {
//         writeln!(self.tx, "Seek ");
//         self.tx.flush();
//     }
// }

// pub struct Quitter {
//     stream: TcpStream,
// }

// impl Quitter {
//     fn new(stream: TcpStream) -> Self {
//         Self { stream }
//     }

//     pub fn quit(mut self) {
//         writeln!(self.stream, "quit").unwrap();
//         self.stream.flush().unwrap();
//         self.stream.shutdown(Shutdown::Both).unwrap();
//     }
// }
