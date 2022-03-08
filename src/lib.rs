use std::{
    borrow::Borrow,
    collections::{HashSet, VecDeque},
    error::Error,
    hash::{Hash, Hasher},
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
        "Guest".into(),
        token,
        "unknown",
        true,
        Duration::from_millis(2_000),
    )
    .await
    .unwrap()
}

async fn internal_connect(
    pseudo_username: String,
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

        let queue = Arc::new(Mutex::new(VecDeque::<Sender<Message>>::new()));
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
            let mut seeks = HashSet::new();
            let mut games = HashSet::new();

            while let Some(Ok(text)) = stream.1.next().await {
                let text = text.to_text().unwrap().strip_suffix(|_| true).unwrap();
                let (command, rest) = text.split_once([' ', '#', ':']).unwrap_or((text, ""));
                let mut tokens = rest.split(' ');
                let mut token = || tokens.next().unwrap();

                let message = match command {
                    "OK" => Message::Ok,
                    "NOK" => Message::NotOk,
                    "Seek" => match token() {
                        "new" => Message::AddSeek(Seek {
                            id: token().parse().unwrap(),
                            seeker: token().into(),
                            params: {
                                let size = token().parse().unwrap();
                                let initial_time = Duration::from_secs(token().parse().unwrap());
                                let increment = Duration::from_secs(token().parse().unwrap());
                                SeekParameters {
                                    color: match token() {
                                        "A" => Color::Any,
                                        "W" => Color::White,
                                        "B" => Color::Black,
                                        _ => unreachable!(),
                                    },
                                    params: GameParameters {
                                        size,
                                        initial_time,
                                        increment,
                                        half_komi: token().parse().unwrap(),
                                        flat_count: token().parse().unwrap(),
                                        cap_count: token().parse().unwrap(),
                                        unrated: token() == "1",
                                        tournament: token() == "1",
                                    },
                                    opponent: match token() {
                                        "" => None,
                                        name => Some(name.into()),
                                    },
                                }
                            },
                        }),
                        "remove" => Message::RemoveSeek(token().parse().unwrap()),
                        _ => unreachable!(),
                    },
                    "GameList" => match token() {
                        "Add" => Message::AddGame(Game {
                            id: token().parse().unwrap(),
                            white: token().into(),
                            black: token().into(),
                            params: GameParameters {
                                size: token().parse().unwrap(),
                                initial_time: Duration::from_secs(token().parse().unwrap()),
                                increment: Duration::from_secs(token().parse().unwrap()),
                                half_komi: token().parse().unwrap(),
                                flat_count: token().parse().unwrap(),
                                cap_count: token().parse().unwrap(),
                                unrated: token() == "1",
                                tournament: token() == "1",
                            },
                        }),
                        "Remove" => Message::RemoveGame(token().parse().unwrap()),
                        _ => unreachable!(),
                    },
                    "Game" => match token() {
                        "Start" => Message::StartGame(token().parse().unwrap()),
                        id => todo!(),
                    },
                    "Online" => Message::Online(token().parse().unwrap()),
                    "Welcome" => Message::LoggedIn(rest.strip_suffix(|_| true).unwrap().into()),
                    "Welcome!" | "Login" => Message::Message(text.into()),
                    "Message" => Message::Message(rest.into()),
                    "Error" => Message::Error(rest.into()),
                    _ => Message::Unknown(text.into()),
                };

                match message {
                    Message::Ok | Message::NotOk | Message::LoggedIn(_) => {
                        if queue.lock().pop_front().unwrap().send(message).is_err() {
                            warn!("Confirmation message \"{text}\" was discarded");
                        }
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
                    Message::StartGame(id) => todo!(),
                    Message::Online(count) => debug!("Online: {count}"),
                    Message::Message(text) => debug!("Ignoring server message \"{text}\""),
                    Message::Error(text) => warn!("Ignoring error message \"{text}\""),
                    Message::Unknown(text) => warn!("Ignoring unknown message \"{text}\""),
                };
            }
        });
    }

    let client = Client {
        username: pseudo_username,
        password,
        tx: tx.clone(),
    };

    info!(
        "Logging in as {}",
        if client.is_guest() {
            "a guest".into()
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
        client_name.into()
    };

    let (a, b, c) = join!(
        client.send(format!("Client {client_name}")),
        client.send("Protocol 1".into()),
        client.send(format!("Login {} {}", client.username, client.password)),
    );

    if a != Message::Ok {
        warn!("Playtak rejected client name \"{client_name}\"");
    }
    if b != Message::Ok {
        return Err("Playtak rejected protocol upgrade to version 1".into());
    }
    let _username = match c {
        Message::LoggedIn(name) => Ok(name),
        _ => Err("Failed to log in with the provided credentials"),
    }?;

    info!("Pinging every {ping_interval:?}");

    spawn(async move {
        let mut interval = interval(ping_interval);
        loop {
            interval.tick().await;
            let channel = channel();
            let time = Instant::now();
            if tx.send(("PING".into(), channel.0)).is_ok() {
                spawn(async move {
                    if channel.1.await.unwrap() != Message::Ok {
                        warn!("Playtak rejected PING");
                    };
                    debug!("Ping: {}ms", time.elapsed().as_millis());
                });
            } else {
                break;
            }
        }
    });

    info!("Client ready");

    Ok(client)
}

type MasterSender = UnboundedSender<(String, Sender<Message>)>;

async fn send(tx: &MasterSender, s: String) -> Message {
    let channel = channel();
    tx.send((s, channel.0)).unwrap();
    channel.1.await.unwrap()
}

#[derive(Debug)]
pub struct Client {
    // hi, it's past alion, remember that you don't receive moves you send
    username: String,
    password: String,
    tx: MasterSender,
}

impl Client {
    pub fn is_guest(&self) -> bool {
        self.username == "Guest"
    }

    async fn send(&self, s: String) -> Message {
        send(&self.tx, s).await
    }

    pub async fn seek(&self, seek: SeekParameters) -> Result<(), Box<dyn Error>> {
        let params = seek.params;
        match self
            .send(format!(
                "Seek {} {} {} {} {} {} {} {} {} {}",
                params.size,
                params.initial_time.as_secs(),
                params.increment.as_secs(),
                match seek.color {
                    Color::Any => 'A',
                    Color::White => 'W',
                    Color::Black => 'B',
                },
                params.half_komi,
                params.flat_count,
                params.cap_count,
                if params.unrated { '1' } else { '0' },
                if params.tournament { '1' } else { '0' },
                seek.opponent.unwrap_or_default(),
            ))
            .await
        {
            Message::Ok => Ok(()),
            Message::NotOk => Err("Playtak rejected the seek".into()),
            _ => unreachable!(),
        }
    }
}

#[derive(Debug)]
pub struct Seek {
    id: u32,
    seeker: String,
    params: SeekParameters,
}

impl PartialEq for Seek {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Seek {}

impl Hash for Seek {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl Borrow<u32> for Seek {
    fn borrow(&self) -> &u32 {
        &self.id
    }
}

#[derive(Debug)]
pub struct SeekParameters {
    opponent: Option<String>,
    color: Color,
    params: GameParameters,
}

impl SeekParameters {
    pub fn new(
        opponent: Option<String>,
        color: Color,
        params: GameParameters,
    ) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            opponent,
            color,
            params,
        })
    }
}

#[derive(Debug)]
pub struct Game {
    id: u32,
    white: String,
    black: String,
    params: GameParameters,
}

impl PartialEq for Game {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Game {}

impl Hash for Game {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl Borrow<u32> for Game {
    fn borrow(&self) -> &u32 {
        &self.id
    }
}

#[derive(Debug)]
pub struct GameParameters {
    size: u32,
    initial_time: Duration,
    increment: Duration,
    half_komi: i32,
    flat_count: u32,
    cap_count: u32,
    unrated: bool,
    tournament: bool,
}

impl GameParameters {
    pub fn new(
        size: u32,
        initial_time: Duration,
        increment: Duration,
        half_komi: i32,
        flat_count: u32,
        cap_count: u32,
        unrated: bool,
        tournament: bool,
    ) -> Result<Self, Box<dyn Error>> {
        if size > 8
            || size < 3
            || initial_time.subsec_nanos() != 0
            || increment.subsec_nanos() != 0
            || half_komi > 8
            || half_komi < 0
        {
            Err("Game parameters not supported by Playtak".into())
        } else {
            Ok(Self {
                size,
                initial_time,
                increment,
                half_komi,
                flat_count,
                cap_count,
                unrated,
                tournament,
            })
        }
    }
}

#[derive(Debug)]
pub enum Color {
    Any,
    White,
    Black,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    Ok,
    NotOk,
    LoggedIn(String),
    AddSeek(Seek),
    RemoveSeek(u32),
    AddGame(Game),
    RemoveGame(u32),
    StartGame(u32),
    Online(u32),
    Message(String),
    Error(String),
    Unknown(String),
}
