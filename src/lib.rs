mod communication;
pub mod data_types;

use communication::*;
use data_types::*;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{SinkExt, StreamExt};

use parking_lot::Mutex;
use tokio::{
    join, select, spawn,
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver},
        oneshot::{channel, Sender},
    },
    time::interval,
};
use tokio_tungstenite::connect_async;

use rand::{distributions::Uniform, prelude::StdRng, Rng, SeedableRng};

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};

pub async fn connect_guest() -> Result<Client, Box<dyn Error>> {
    let token = StdRng::from_entropy()
        .sample_iter(Uniform::from(b'a'..=b'z'))
        .take(20)
        .map(char::from)
        .collect::<String>();

    connect("Guest".into(), token).await
}

pub async fn connect_as(username: String, password: String) -> Result<Client, Box<dyn Error>> {
    connect(username, password).await
}

async fn connect(id: String, token: String) -> Result<Client, Box<dyn Error>> {
    internal_connect(id, token, "unknown", true, Duration::from_millis(2_000)).await
}

async fn internal_connect(
    pseudo_username: String,
    password: String,
    client_name: &str,
    append_lib_name: bool,
    ping_interval: Duration,
) -> Result<Client, Box<dyn Error>> {
    let (tx, mut rx) = unbounded_channel::<SentRequest>();
    let (start_tx, start_rx) = unbounded_channel();

    {
        info!("Establishing WebSocket Secure connnection to Playtak server");
        let mut stream = connect_async("wss://playtak.com/ws")
            .await
            .unwrap()
            .0
            .split();

        let queue = Arc::new(Mutex::new(VecDeque::<SentRequest>::new()));
        {
            let queue = queue.clone();
            spawn(async move {
                while let Some(request) = rx.recv().await {
                    let s = request.0.to_string();
                    debug!("Sending \"{s}\"");
                    {
                        let mut queue = queue.lock();
                        let backlog = queue.len();
                        if backlog != 0 {
                            debug!("Sending a command while awaiting response(s) to {backlog} previous command(s)");
                        }
                        queue.push_back(request);
                    }
                    stream.0.send(s.into()).await.unwrap();
                }

                debug!("Closing Playtak connection");
                stream.0.close().await.unwrap();
            });
        }
        spawn(async move {
            let mut seeks = HashSet::<Seek>::new();
            let mut games = HashSet::<Game>::new();

            let mut active_games = HashMap::new();

            let mut username = None;

            while let Some(Ok(text)) = stream.1.next().await {
                let is_me = |seeker: &String| seeker == username.as_ref().unwrap();

                let text = text.to_text().unwrap().strip_suffix(|_| true).unwrap();
                let mut message = text.parse().unwrap();

                {
                    let mut queue = queue.lock();
                    if !queue.is_empty() {
                        let (response, returned) = match message {
                            Message::Ok => (Some(Ok(())), None),
                            Message::NotOk => (Some(Err("Rejected".into())), None),
                            Message::Error(e) => (Some(Err(e.into())), None),
                            Message::LoggedIn(name) => {
                                username = Some(name);
                                (Some(Ok(())), None)
                            }
                            Message::AddSeek(ref seek) if is_me(&seek.owner) => {
                                (Some(Ok(())), Some(message))
                            }
                            // Message::RemoveSeek(id)
                            //     if matches!(request, Request::Seek(_))
                            //         && is_me(&seeks.get(&id).unwrap().seeker) =>
                            // {
                            //     (None, Some(message))
                            // }
                            _ => (None, Some(message)),
                        };

                        if let Some(result) = response {
                            queue.pop_front().unwrap().1.send(result).unwrap();
                        }

                        if let Some(returned_message) = returned {
                            message = returned_message;
                        } else {
                            continue;
                        }
                    }
                }

                match message {
                    Message::Ok | Message::NotOk | Message::LoggedIn(_) => {
                        warn!("Confirmation message \"{text}\" was discarded");
                    }
                    Message::AddSeek(seek) => {
                        debug!("Adding {seek:?}");
                        if !seeks.insert(seek) {
                            error!("Seek ID collision detected")
                        }
                    }
                    Message::RemoveSeek(id) => {
                        debug!("Removing seek {id}");
                        if !seeks.remove(&id) {
                            error!("Attempted to remove nonexistent seek")
                        }
                    }
                    Message::AddGame(game) => {
                        debug!("Adding {game:?}");
                        if !games.insert(game) {
                            error!("Game ID collision detected")
                        }
                    }
                    Message::RemoveGame(id) => {
                        debug!("Removing game {id}");
                        if !games.remove(&id) {
                            error!("Attempted to remove nonexistent game")
                        }
                    }
                    Message::StartGame(id) => {
                        let (update_tx, update_rx) = unbounded_channel();
                        let initial_time = games.get(&id).unwrap().params.initial_time;
                        let data = Arc::new(Mutex::new(ActiveGameData {
                            white_remaining: initial_time,
                            black_remaining: initial_time,
                            last_sync: None,
                        }));
                        active_games.insert(id, (update_tx, data.clone()));
                        start_tx
                            .send((update_rx, data, games.get(&id).unwrap().clone()))
                            .unwrap();
                    }
                    Message::SyncClocks(id, white_remaining, black_remaining) => {
                        *active_games.get(&id).unwrap().1.lock() = ActiveGameData {
                            white_remaining,
                            black_remaining,
                            last_sync: Some(Instant::now()),
                        };
                    }
                    Message::Play(id, m) => {
                        active_games
                            .get(&id)
                            .unwrap()
                            .0
                            .send(GameUpdate::Played(m))
                            .unwrap();
                    }
                    Message::GameOver(id, result) => {
                        active_games
                            .remove(&id)
                            .unwrap()
                            .0
                            .send(GameUpdate::Ended(result))
                            .unwrap();
                    }
                    Message::Online(count) => debug!("Online: {count}"),
                    Message::Message(text) => debug!("Ignoring server message \"{text}\""),
                    Message::Error(text) => warn!("Ignoring error message \"{text}\""),
                    Message::Unknown(text) => warn!("Ignoring unknown message \"{text}\""),
                };
            }

            debug!("Connection closed");
        });
    }

    let (_ping_tx, mut ping_rx) = channel::<()>();

    info!(
        "Logging in as {}",
        if pseudo_username == "Guest" {
            "a guest".into()
        } else {
            format!("\"{}\"", pseudo_username)
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
        client_name.into()
    };

    let [a, b, c] = [
        Request::Client(client_name),
        Request::Protocol(1),
        Request::Login(pseudo_username, password),
    ]
    .map(|r| r.send(&tx).unwrap());
    let (a, b, c) = join!(a, b, c);

    if a.is_err() {
        warn!("Playtak rejected provided client name");
    }
    if b.is_err() {
        Err("Playtak rejected protocol upgrade to version 1")?;
    }
    if c.is_err() {
        Err("Failed to log in with the provided credentials")?;
    };

    info!("Pinging every {ping_interval:?}");

    {
        let tx = tx.clone();
        spawn(async move {
            let mut interval = interval(ping_interval);
            loop {
                select! {
                    biased;
                    _ = &mut ping_rx => {
                        break;
                    }
                    _ = interval.tick() => {
                        let time = Instant::now();
                        let rx = Request::Ping.send(&tx).unwrap();
                        spawn(async move {
                            if rx.await.is_err() {
                                warn!("Ping failed");
                            } else {
                                debug!("Ping: {}ms", time.elapsed().as_millis());
                            }
                        });
                    }
                }
            }
        });
    }

    info!("Client ready");

    Ok(Client {
        tx,
        _ping_tx,
        start_rx,
        data: todo!(),
        username: todo!(),
    })
}

#[derive(Debug)]
pub struct Client {
    tx: MasterSender,
    _ping_tx: Sender<()>,
    start_rx: UnboundedReceiver<(
        UnboundedReceiver<GameUpdate>,
        Arc<Mutex<ActiveGameData>>,
        Game,
    )>,
    data: Arc<ClientData>,
    username: String,
}

impl Client {
    pub async fn seek(&self, seek: SeekParameters) -> Result<(), Box<dyn Error + Send + Sync>> {
        Request::Seek(seek).send(&self.tx)?.await
    }

    pub async fn game(&mut self) -> Result<ActiveGame, Box<dyn Error + Send + Sync>> {
        let (update_rx, data, game) = self.start_rx.recv().await.ok_or(ConnectionClosed)?;
        Ok(ActiveGame {
            update_rx,
            data,
            game,
        })
    }
}

#[derive(Debug)]
struct ClientData {
    seeks: Mutex<HashSet<Seek>>,
    games: Mutex<HashSet<Game>>,
    active_games: Mutex<HashSet<ActiveGame>>,
}

impl ClientData {
    fn new() -> Self {
        Self {
            seeks: Mutex::default(),
            games: Mutex::default(),
            active_games: Mutex::default(),
        }
    }
}
