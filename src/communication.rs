use crate::data_types::*;

use std::{
    cmp::Ordering,
    error::Error,
    fmt::{Display, Formatter, Result as FmtResult},
    str::FromStr,
    time::Duration,
};

use takparse::{Direction, Move, MoveKind, Piece, Square};

use futures::Future;

use tokio::sync::{
    mpsc::UnboundedSender,
    oneshot::{channel, Sender},
};

pub type MasterSender = UnboundedSender<SentRequest>;

#[derive(Debug)]
pub enum Request {
    Client(String),
    Protocol(u32),
    Login(String, String),
    Ping,
    Seek(crate::SeekParameters),
    Play(u32, Move),
}

#[derive(Debug)]
pub struct SentRequest(
    pub Request,
    pub Sender<Result<(), Box<dyn Error + Send + Sync>>>,
);

impl Request {
    pub fn send(
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
                        None => 'A',
                        Some(Color::White) => 'W',
                        Some(Color::Black) => 'B',
                    },
                    params.half_komi,
                    params.flat_count,
                    params.cap_count,
                    if params.rated { '0' } else { '1' },
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
pub enum Message {
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
                    owner: token()?.into(),
                    params: {
                        let size = token()?.parse()?;
                        let initial_time = Duration::from_secs(token()?.parse()?);
                        let increment = Duration::from_secs(token()?.parse()?);
                        SeekParameters {
                            color: match token()? {
                                "A" => None,
                                "W" => Some(Color::White),
                                "B" => Some(Color::Black),
                                _ => Err("unknown color")?,
                            },
                            params: GameParameters {
                                size,
                                initial_time,
                                increment,
                                half_komi: token()?.parse()?,
                                flat_count: token()?.parse()?,
                                cap_count: token()?.parse()?,
                                rated: token()? == "0",
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
                        rated: token()? == "0",
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
