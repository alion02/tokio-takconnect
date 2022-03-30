use std::{
    borrow::Borrow,
    error::Error,
    fmt::{Display, Formatter, Result as FmtResult},
    hash::{Hash, Hasher},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use takparse::Move;

use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedReceiver;

#[derive(Debug)]
pub struct Seek {
    pub(crate) id: u32,
    pub(crate) owner: String,
    pub(crate) params: SeekParameters,
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
    pub(crate) opponent: Option<String>,
    pub(crate) color: Color,
    pub(crate) params: GameParameters,
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
    pub(crate) id: u32,
    pub(crate) white: String,
    pub(crate) black: String,
    pub(crate) params: GameParameters,
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
    pub(crate) size: u32,
    pub(crate) initial_time: Duration,
    pub(crate) increment: Duration,
    pub(crate) half_komi: i32,
    pub(crate) flat_count: u32,
    pub(crate) cap_count: u32,
    pub(crate) unrated: bool,
    pub(crate) tournament: bool,
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
pub struct ActiveGame {
    pub(crate) update_rx: UnboundedReceiver<GameUpdate>,
    pub(crate) data: Arc<Mutex<ActiveGameData>>,
    pub(crate) id: u32,
}

impl ActiveGame {
    pub async fn update(&mut self) -> Result<GameUpdate, Box<dyn Error + Send + Sync>> {
        Ok(self.update_rx.recv().await.ok_or(ConnectionClosed)?)
    }

    pub async fn play(&self, m: Move) -> Result<(), Box<dyn Error + Send + Sync>> {
        todo!()
        // Request::Play(self.id, m).send(&self.client.tx)?.await
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
pub(crate) struct ConnectionClosed;

impl Display for ConnectionClosed {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        "Connection closed".fmt(f)
    }
}

impl Error for ConnectionClosed {}

#[derive(Debug)]
pub(crate) struct ActiveGameData {
    pub(crate) white_remaining: Duration,
    pub(crate) black_remaining: Duration,
    pub(crate) last_sync: Option<Instant>,
}
