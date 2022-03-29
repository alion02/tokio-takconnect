# tokio-takconnect
A work-in-progress, asynchronous Playtak client.

## Example project
```rs
use std::{
    io::{stdin, BufRead},
    thread::spawn,
    time::Duration,
};

use tokio::{select, sync::mpsc::unbounded_channel};
use tokio_takconnect::{connect_guest, Color, GameParameters, GameUpdate, SeekParameters};

#[tokio::main]
async fn main() {
    let mut client = connect_guest().await.unwrap();

    loop {
        client
            .seek(
                SeekParameters::new(
                    None,
                    Color::Any,
                    GameParameters::new(
                        5,
                        Duration::from_secs(600),
                        Duration::from_secs(20),
                        0,
                        21,
                        1,
                        false,
                        false,
                    )
                    .unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        let mut game = client.game().await.unwrap();

        let (tx, mut rx) = unbounded_channel();
        spawn(move || {
            for line in stdin().lock().lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });

        println!("Game start.");

        loop {
            select! {
                update = game.update() => {
                    match update.unwrap() {
                        GameUpdate::Played(m) => println!("Opponent plays: {m}"),
                        GameUpdate::Ended(_) => {
                            println!("Game over!");
                            break;
                        }
                        _ => (),
                    }
                }
                input = rx.recv() => {
                    match input.unwrap().parse() {
                        Ok(m) => game.play(m).await.unwrap(),
                        Err(e) => println!("Failed to parse move: {e}"),
                    }
                }
            }
        }
    }
}
```
