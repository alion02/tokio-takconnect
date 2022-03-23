use std::{
    borrow::Borrow,
    collections::{HashSet, VecDeque},
    error::Error,
    hash::{Hash, Hasher},
    str::FromStr,
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
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
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

        let queue = Arc::new(Mutex::new(VecDeque::<Interceptor>::new()));
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
                let mut message = text.parse().unwrap();

                'process_message: loop {
                    {
                        let mut queue = queue.lock();
                        let mut i = 0;
                        while i < queue.len() {
                            let interceptor = &mut queue[i];
                            let result = interceptor(message);

                            if result.finished {
                                queue.remove(i);
                            } else {
                                i += 1;
                            }

                            if let Some(returned_message) = result.message {
                                message = returned_message;
                            } else {
                                break 'process_message;
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
                        Message::StartGame(id) => todo!(),
                        Message::Online(count) => debug!("Online: {count}"),
                        Message::Message(text) => debug!("Ignoring server message \"{text}\""),
                        Message::Error(text) => warn!("Ignoring error message \"{text}\""),
                        Message::Unknown(text) => warn!("Ignoring unknown message \"{text}\""),
                    };
                    break;
                }
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

            let time = Instant::now();
            let mut rx = send(&tx, "PING".into());
            spawn(async move {
                if rx.recv().await.unwrap() != Message::Ok {
                    warn!("Playtak rejected PING");
                }
                debug!("Ping: {}ms", time.elapsed().as_millis());
            });
        }
    });

    info!("Client ready");

    Ok(client)
}

type MasterSender = UnboundedSender<(String, Interceptor)>;

fn send(tx: &MasterSender, s: String) -> UnboundedReceiver<Message> {
    let (response_tx, rx) = unbounded_channel();

    tx.send((
        s,
        interceptor(move |m| match m {
            Message::Ok | Message::NotOk | Message::LoggedIn(_) => {
                response_tx.send(m).unwrap();
                InterceptionResult::finish()
            }
            _ => InterceptionResult::ignore(m),
        }),
    ))
    .ok()
    .unwrap();

    rx
}

trait InterceptorTrait: FnMut(Message) -> InterceptionResult + Send {}
impl<T: FnMut(Message) -> InterceptionResult + Send> InterceptorTrait for T {}

type Interceptor = Box<dyn InterceptorTrait>;

fn interceptor<F: 'static + InterceptorTrait>(f: F) -> Interceptor {
    Box::new(f)
}

#[derive(Debug)]
struct InterceptionResult {
    message: Option<Message>,
    finished: bool,
}

impl InterceptionResult {
    fn ignore(message: Message) -> Self {
        Self {
            message: Some(message),
            finished: false,
        }
    }

    fn consume() -> Self {
        Self {
            message: None,
            finished: false,
        }
    }

    fn give_up(message: Message) -> Self {
        Self {
            message: Some(message),
            finished: true,
        }
    }

    fn finish() -> Self {
        Self {
            message: None,
            finished: true,
        }
    }
}

#[derive(Debug)]
pub struct Client {
    // hi, it's past alion, remember that you don't receive moves you send
    username: String,
    password: String,
    tx: MasterSender,
}

impl Client {
    fn is_guest(&self) -> bool {
        self.username == "Guest"
    }

    async fn send(&self, s: String) -> Message {
        send(&self.tx, s).recv().await.unwrap()
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
enum Message {
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

impl FromStr for Message {
    type Err = Box<dyn Error>;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (command, rest) = s.split_once([' ', '#', ':']).unwrap_or((s, ""));
        let mut tokens = rest.split(' ');
        let mut token = || tokens.next().ok_or("unexpected end of tokens");

        Ok(match command {
            "OK" => Message::Ok,
            "NOK" => Message::NotOk,
            "Seek" => match token()? {
                "new" => Message::AddSeek(Seek {
                    id: token()?.parse()?,
                    seeker: token()?.into(),
                    params: {
                        let size = token()?.parse()?;
                        let initial_time = Duration::from_secs(token()?.parse()?);
                        let increment = Duration::from_secs(token()?.parse()?);
                        SeekParameters {
                            color: match token()? {
                                "A" => Color::Any,
                                "W" => Color::White,
                                "B" => Color::Black,
                                _ => Err("unknown color")?,
                            },
                            params: GameParameters {
                                size,
                                initial_time,
                                increment,
                                half_komi: token()?.parse()?,
                                flat_count: token()?.parse()?,
                                cap_count: token()?.parse()?,
                                unrated: token()? == "1",
                                tournament: token()? == "1",
                            },
                            opponent: match token()? {
                                "" => None,
                                name => Some(name.into()),
                            },
                        }
                    },
                }),
                "remove" => Message::RemoveSeek(token()?.parse()?),
                _ => Err("unexpected \"Seek\" message sub-type")?,
            },
            "GameList" => match token()? {
                "Add" => Message::AddGame(Game {
                    id: token()?.parse()?,
                    white: token()?.into(),
                    black: token()?.into(),
                    params: GameParameters {
                        size: token()?.parse()?,
                        initial_time: Duration::from_secs(token()?.parse()?),
                        increment: Duration::from_secs(token()?.parse()?),
                        half_komi: token()?.parse()?,
                        flat_count: token()?.parse()?,
                        cap_count: token()?.parse()?,
                        unrated: token()? == "1",
                        tournament: token()? == "1",
                    },
                }),
                "Remove" => Message::RemoveGame(token()?.parse()?),
                _ => Err("unexpected \"GameList\" message sub-type")?,
            },
            "Game" => match token()? {
                "Start" => Message::StartGame(token()?.parse()?),
                id => todo!(),
            },
            "Online" => Message::Online(token()?.parse()?),
            "Welcome" => Message::LoggedIn(
                rest.strip_suffix(|c| c == '!')
                    .ok_or("missing exclamation mark after username")?
                    .into(),
            ),
            "Welcome!" | "Login" => Message::Message(s.into()),
            "Message" => Message::Message(rest.into()),
            "Error" => Message::Error(rest.into()),
            _ => Message::Unknown(s.into()),
        })
    }
}
