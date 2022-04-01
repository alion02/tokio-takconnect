use std::{
    borrow::Borrow,
    cmp::Ordering,
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    fmt::{Display, Formatter, Result as FmtResult},
    hash::{Hash, Hasher},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use takparse::{Direction, Move, MoveKind, Piece, Square};

use futures::{
    stream::{self, SplitSink},
    Future, FutureExt, SinkExt, StreamExt,
};

use parking_lot::Mutex;
use tokio::{
    join, select, spawn,
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        oneshot::{channel, Sender},
    },
    time::interval,
};
use tokio_tungstenite::connect_async;

use rand::{distributions::Uniform, prelude::StdRng, Rng, SeedableRng};

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
                    if let Some(SentRequest(_request, _)) = queue.front_mut() {
                        let (response, returned) = match message {
                            Message::Ok => (Some(Ok(())), None),
                            Message::NotOk => (Some(Err("Rejected".into())), None),
                            Message::LoggedIn(name) => {
                                username = Some(name);
                                (Some(Ok(())), None)
                            }
                            Message::AddSeek(ref seek) if is_me(&seek.seeker) => {
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
                        start_tx.send((update_rx, data, id)).unwrap();
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
    })
}

type MasterSender = UnboundedSender<SentRequest>;

#[derive(Debug)]
pub struct Client {
    tx: MasterSender,
    _ping_tx: Sender<()>,
    start_rx: UnboundedReceiver<(
        UnboundedReceiver<GameUpdate>,
        Arc<Mutex<ActiveGameData>>,
        u32,
    )>,
}

impl Client {
    pub async fn seek(&self, seek: SeekParameters) -> Result<(), Box<dyn Error + Send + Sync>> {
        Request::Seek(seek).send(&self.tx)?.await
    }

    pub async fn game(&mut self) -> Result<ActiveGame<'_>, Box<dyn Error + Send + Sync>> {
        let (update_rx, data, id) = self.start_rx.recv().await.ok_or(ConnectionClosed)?;
        Ok(ActiveGame {
            client: self,
            update_rx,
            data,
            id,
        })
    }
}

#[derive(Debug)]
pub struct ActiveGame<'a> {
    client: &'a Client,
    update_rx: UnboundedReceiver<GameUpdate>,
    data: Arc<Mutex<ActiveGameData>>,
    id: u32,
}

impl<'a> ActiveGame<'a> {
    pub async fn update(&mut self) -> Result<GameUpdate, Box<dyn Error + Send + Sync>> {
        Ok(self.update_rx.recv().await.ok_or(ConnectionClosed)?)
    }

    pub async fn play(&self, m: Move) -> Result<(), Box<dyn Error + Send + Sync>> {
        Request::Play(self.id, m).send(&self.client.tx)?.await
    }
}

#[non_exhaustive]
#[derive(Debug)]
pub enum GameUpdate {
    Played(Move),
    Ended(GameResult),
}

#[derive(Debug, PartialEq, Eq)]
pub struct GameResult(GameResultInner);

#[derive(Debug, PartialEq, Eq)]
enum GameResultInner {
    RoadWhite,
    RoadBlack,
    FlatWhite,
    FlatBlack,
    OtherWhite,
    OtherBlack,
    OtherDecisive,
    Draw,
}

impl FromStr for GameResult {
    type Err = Box<dyn Error>;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use GameResultInner::*;
        Ok(Self(match s {
            "R-0" => RoadWhite,
            "0-R" => RoadBlack,
            "F-0" => FlatWhite,
            "0-F" => FlatBlack,
            "1-0" => OtherWhite,
            "0-1" => OtherBlack,
            "1/2-1/2" => Draw,
            _ => Err("malformed game result")?,
        }))
    }
}

#[derive(Debug)]
struct ConnectionClosed;

impl Display for ConnectionClosed {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        "Connection closed".fmt(f)
    }
}

impl Error for ConnectionClosed {}

#[derive(Debug)]
struct ActiveGameData {
    white_remaining: Duration,
    black_remaining: Duration,
    last_sync: Option<Instant>,
}

#[derive(Debug)]
struct Seek {
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
struct Game {
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

#[derive(Debug)]
enum Request {
    Client(String),
    Protocol(u32),
    Login(String, String),
    Ping,
    Seek(SeekParameters),
    Play(u32, Move),
}

#[derive(Debug)]
struct SentRequest(Request, Sender<Result<(), Box<dyn Error + Send + Sync>>>);

impl Request {
    fn send(
        self,
        tx: &MasterSender,
    ) -> Result<
        impl Future<Output = Result<(), Box<dyn Error + Send + Sync>>>,
        Box<dyn Error + Send + Sync>,
    > {
        let c = channel();
        tx.send(SentRequest(self, c.0))?;
        Ok(async move { c.1.await? })
    }
}

impl Display for Request {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::Client(name) => write!(f, "Client {name}"),
            Self::Protocol(version) => write!(f, "Protocol {version}"),
            Self::Login(name, secret) => write!(f, "Login {name} {secret}"),
            Self::Ping => "PING".fmt(f),
            Self::Seek(seek) => {
                let params = &seek.params;
                write!(
                    f,
                    "Seek {} {} {} {} {} {} {} {} {} ",
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
                )?;
                seek.opponent.iter().try_for_each(|o| o.fmt(f))
            }
            Self::Play(id, m) => {
                let write_square = |f: &mut Formatter, s: Square| {
                    write!(
                        f,
                        "{}{}",
                        (b'A' + s.column()) as char,
                        (b'1' + s.row()) as char
                    )
                };

                let square = m.square();

                write!(f, "Game#{id} ")?;

                match m.kind() {
                    MoveKind::Place(piece) => {
                        "P ".fmt(f)?;
                        write_square(f, square)?;

                        match piece {
                            Piece::Flat => "",
                            Piece::Wall => " W",
                            Piece::Cap => " C",
                        }
                        .fmt(f)
                    }
                    MoveKind::Spread(direction, pattern) => {
                        "M ".fmt(f)?;
                        write_square(f, square)?;

                        ' '.fmt(f)?;
                        write_square(f, square.shift(direction, pattern.count_squares() as i8))?;

                        pattern
                            .drop_counts()
                            .try_for_each(|count| write!(f, " {count}"))
                    }
                }
            }
        }
    }
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
    SyncClocks(u32, Duration, Duration),
    Play(u32, Move),
    GameOver(u32, GameResult),
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
            "Game" => {
                match token()? {
                    "Start" => Message::StartGame(token()?.parse()?),
                    id => {
                        let id = id.parse()?;
                        match token()? {
                            "Timems" => Message::SyncClocks(
                                id,
                                Duration::from_millis(token()?.parse()?),
                                Duration::from_millis(token()?.parse()?),
                            ),
                            "Over" => Message::GameOver(id, token()?.parse()?),
                            "Abandoned." => {
                                Message::GameOver(id, GameResult(GameResultInner::OtherDecisive))
                            }
                            move_type @ ("P" | "M") => {
                                let square = token()?.to_ascii_lowercase().parse()?;
                                Message::Play(
                                    id,
                                    Move::new(
                                        square,
                                        if move_type == "P" {
                                            MoveKind::Place(match tokens.next() {
                                                None => Piece::Flat,
                                                Some("W") => Piece::Wall,
                                                Some("C") => Piece::Cap,
                                                _ => Err("unexpected piece")?,
                                            })
                                        } else {
                                            let target_square: Square =
                                                token()?.to_ascii_lowercase().parse()?;
                                            let direction = match (
                                                target_square.column().cmp(&square.column()),
                                                target_square.row().cmp(&square.row()),
                                            ) {
                                                (Ordering::Less, Ordering::Equal) => Direction::Left,
                                                (Ordering::Greater, Ordering::Equal) => {
                                                    Direction::Right
                                                }
                                                (Ordering::Equal, Ordering::Less) => Direction::Down,
                                                (Ordering::Equal, Ordering::Greater) => Direction::Up,
                                                _ => Err("start and end squares don't form a straight line")?
                                            };
                                            MoveKind::Spread(
                                                direction,
                                                tokens.collect::<String>().parse()?,
                                            )
                                        },
                                    ),
                                )
                            }
                            _ => Message::Unknown(s.into()),
                        }
                    }
                }
            }
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
