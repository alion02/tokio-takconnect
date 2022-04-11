use std::{
    borrow::Borrow,
    error::Error,
    fmt::{Display, Formatter, Result as FmtResult},
    hash::{Hash, Hasher},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

pub use takparse::{Color, Move};

use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::communication::{MasterSender, Request};

#[derive(Debug, Clone)]
pub struct Seek {
    pub(crate) id: u32,
    pub(crate) owner: String,
    pub(crate) params: SeekParameters,
}

impl Seek {
    /// The name of the owner of the seek.
    #[must_use]
    pub fn owner(&self) -> &str {
        self.owner.as_ref()
    }

    /// The parameters of the seek.
    #[must_use]
    pub fn params(&self) -> &SeekParameters {
        &self.params
    }
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

#[derive(Debug, Clone)]
pub struct SeekParameters {
    pub(crate) opponent: Option<String>,
    pub(crate) color: Option<Color>,
    pub(crate) params: GameParameters,
}

impl SeekParameters {
    pub fn new(
        opponent: Option<String>,
        color: Option<Color>,
        params: GameParameters,
    ) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            opponent,
            color,
            params,
        })
    }

    /// The name of the opponent if the seek is private.
    #[must_use]
    pub fn opponent(&self) -> Option<&str> {
        self.opponent.as_ref().map(String::as_str)
    }

    /// The seek color if it's not random.
    #[must_use]
    pub fn color(&self) -> Option<Color> {
        self.color
    }

    /// The game parameters of the seek.
    #[must_use]
    pub fn params(&self) -> &GameParameters {
        &self.params
    }
}

#[derive(Debug, Clone)]
pub struct Game {
    pub(crate) id: u32,
    pub(crate) white: String,
    pub(crate) black: String,
    pub(crate) params: GameParameters,
}

impl Game {
    /// The name of the white player.
    #[must_use]
    pub fn white(&self) -> &str {
        self.white.as_ref()
    }

    /// The name of the black player.
    #[must_use]
    pub fn black(&self) -> &str {
        self.black.as_ref()
    }

    /// The parameters of the game.
    #[must_use]
    pub fn params(&self) -> &GameParameters {
        &self.params
    }
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

#[derive(Debug, Clone)]
pub struct GameParameters {
    pub(crate) size: u32,
    pub(crate) initial_time: Duration,
    pub(crate) increment: Duration,
    pub(crate) half_komi: i32,
    pub(crate) flat_count: u32,
    pub(crate) cap_count: u32,
    pub(crate) rated: bool,
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
        rated: bool,
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
                rated,
                tournament,
            })
        }
    }

    /// The board size.
    #[must_use]
    pub fn size(&self) -> u32 {
        self.size
    }

    /// The initial amount of time each player starts with.
    #[must_use]
    pub fn initial_time(&self) -> Duration {
        self.initial_time
    }

    /// The amount of time each player gets whenever they make a move.
    #[must_use]
    pub fn increment(&self) -> Duration {
        self.increment
    }

    /// The halved number of flats the black player gets on top of their flat count at the end of the game.
    /// That is, a value of `4` means the black player gets two flats added, `5` means two and a half and so on.
    #[must_use]
    pub fn half_komi(&self) -> i32 {
        self.half_komi
    }

    /// The initial number of flatstones each player has.
    #[must_use]
    pub fn flat_count(&self) -> u32 {
        self.flat_count
    }

    /// The initial number of capstones each player has.
    #[must_use]
    pub fn cap_count(&self) -> u32 {
        self.cap_count
    }

    /// Whether the game may count for rating points.
    /// Games where at least one player is a guest ignore this and are always unrated.
    /// Additionally, some game parameters may affect whether a game is rated.
    #[must_use]
    pub fn rated(&self) -> bool {
        self.rated
    }

    /// Whether this is a tournament game. Tournament games are marked as such in match history, and also allow more time to reconnect.
    #[must_use]
    pub fn tournament(&self) -> bool {
        self.tournament
    }
}

#[derive(Debug)]
pub struct ActiveGame {
    pub(crate) tx: MasterSender,
    pub(crate) update_rx: UnboundedReceiver<Update>,
    pub(crate) data: Arc<Mutex<ActiveGameData>>,
    pub(crate) game: Game,
}

impl ActiveGame {
    pub async fn update(&mut self) -> Result<Update, Box<dyn Error + Send + Sync>> {
        Ok(self.update_rx.recv().await.ok_or(ConnectionClosed)?)
    }

    pub async fn play(&self, m: Move) -> Result<(), Box<dyn Error + Send + Sync>> {
        Request::Play(self.game.id, m).send(&self.tx)?.await
    }

    pub fn clock(&self) -> Clock {
        todo!()
    }

    /// The underlying game this active game represents.
    #[must_use]
    pub fn game(&self) -> &Game {
        &self.game
    }
}

#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum Update {
    Played(Move),
    TurnStarted(Color),
    GameEnded(GameResult),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameResult(GameResultInner);

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
pub(crate) struct ActiveGameData {
    pub(crate) white_remaining: Duration,
    pub(crate) black_remaining: Duration,
    pub(crate) last_sync: Option<Instant>,
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
pub struct Clock;

impl Clock {
    pub fn my_time(&self) -> Duration {
        todo!()
    }

    pub fn opp_time(&self) -> Duration {
        todo!()
    }
}
