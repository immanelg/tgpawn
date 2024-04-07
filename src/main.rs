use anyhow::Result;
use grammers_client::types::Chat;
use grammers_client::{Client, Config, InitParams, Update};
use grammers_session::{PackedChat, Session};
use log::{debug, error, info};
use shakmaty::fen::Fen;
use shakmaty::san::San;
use shakmaty::uci::Uci;
use shakmaty::{CastlingMode, Chess, Color, Move, Outcome, Position};
use sqlx::sqlite::{Sqlite, SqlitePool};
use sqlx::{Connection, Executor};
use std::pin::pin;
use std::{collections::HashMap, env};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::{runtime, task};

const SESSION_FILE: &str = "app.session";

const STARTING_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

enum Termination {
    Timeout = 0,
    Resign = 1,
    Checkmate = 2,
    Draw = 3,
}

fn packed_chat(id: i64) -> PackedChat {
    PackedChat {
        id,
        ty: grammers_session::PackedType::User,
        access_hash: None,
    }
}

fn parse_move(notation: &str, board: &impl Position) -> Option<Move> {
    let m = San::from_ascii(notation.as_bytes())
        .ok()
        .and_then(|san| san.to_move(board).ok());
    if m.is_some() {
        return m;
    }
    Uci::from_ascii(notation.as_bytes())
        .ok()
        .and_then(|uci| uci.to_move(board).ok())
}

async fn handle_update(client: &mut Client, db: sqlx::Pool<Sqlite>, update: Update) -> Result<()> {
    match update {
        Update::NewMessage(message) if !message.outgoing() => {
            let chat = message.chat();
            let user_id = chat.id();
            let user_name = chat.name();
            let text = message.text();

            let c = packed_chat(user_id);

            info!("message by {user_id} {user_name}: {text}");

            sqlx::query("insert or ignore into users (id) values ($1)")
                .bind(user_id)
                .execute(&db)
            .await?;

            debug!("insert user {user_id}");

            let maybe_playing_game: Option<(i64, i64, i64, bool, i64, String)> = sqlx::query_as(
                "select id, w_id, b_id, winner, termination, fen from games where (w_id = $1 or b_id = $1) and ended = 0",
            ).bind(user_id).fetch_optional(&db).await?;

            debug!("get ongoing game for {user_id}: got {maybe_playing_game:?}");

            match text.as_ref() {
                "nuke" => {
                    sqlx::query("delete from games").execute(&db).await.unwrap();
                    sqlx::query("delete from users").execute(&db).await.unwrap();
                    sqlx::query("delete from moves").execute(&db).await.unwrap();
                }
                "start" => {
                    // TODO: accept initial position?
                    // TODO: ratings?
                    if maybe_playing_game.is_some() {
                        debug!("already in game {user_id}");
                        client
                            .send_message(c, "You are already playing. Type `resign` to leave.")
                            .await
                            .unwrap();
                        return;
                    };

                    let maybe_pairable: Option<(i64, Option<i64>, Option<i64>)> = sqlx::query_as("select id, w_id, b_id from games where (b_id is null or w_id is null) and ended = 0 limit 1")
                        .fetch_optional(&db).await.unwrap();
                    debug!("maybe_pairable? {maybe_pairable:?}");

                    if let Some((id, w_id, b_id)) = maybe_pairable {
                        let (w_id, b_id) = match (w_id, b_id) {
                            (Some(w_id), None) => (w_id, user_id),
                            (None, Some(b_id)) => (user_id, b_id),
                            _ => {
                                panic!("oh how surprising! you are stupid! {maybe_pairable:?}")
                            }
                        };
                        let (_id, w_id, b_id) = sqlx::query_as::<_, (i64, i64, i64)>(
                            "update games set w_id = $1, b_id = $2 where games.id = $3 returning id, w_id, b_id"
                        )
                            .bind(w_id)
                            .bind(b_id)
                            .bind(id)
                            .fetch_one(&db)
                        .await?;
                        let (white, black) = (packed_chat(w_id), packed_chat(b_id));
                        client
                            .send_message(white, "You are white. Your turn!")
                        .await?;
                        client
                            .send_message(black, "You are black. Waiting for opponent's move.")
                        .await?;
                    } else {
                        let (id,) = sqlx::query_as::<_, (i64,)>("insert into games (w_id, b_id, winner, ended, fen) values ($1, null, null, 0, $2) returning id").bind(user_id).bind(STARTING_FEN).fetch_one(&db).await?;
                        debug!("create new game {id}");
                        client
                            .send_message(
                                c,
                                "Created a new game. Waiting for an opponent to join.",
                            )
                            .await
                            .unwrap();
                    }
                }
                "resign" => {
                    if let Some(_) = maybe_playing_game {
                        todo!("resign");
                    } else {
                        todo!("reject: need to join a game");
                    }
                }
                notation => {
                    let Some((id, w_id, b_id, _winner, _termination, fen)) = maybe_playing_game
                    else {
                        client
                            .send_message(c, "Type `start` to join a game")
                            .await
                            .unwrap();
                        return;
                    };
                    let board = boards.entry(id).or_insert_with(|| {
                        fen.parse::<Fen>()
                            .expect("fen from db")
                            .into_position(CastlingMode::Standard)
                            .expect("valid initial position")
                    });
                    if !(board.turn() == Color::White && user_id == w_id || board.turn() == Color::Black && user_id == b_id) {
                        client.send_message(chat, "Not your turn!").await?;
                        return;
                    }
                    let Some(m) = parse_move(notation, board) else {
                        client
                            .send_message(chat, "This is not a valid move")
                        .await?;
                        return;
                    };
                    if !board.is_legal(&m) {
                        client.send_message(chat, "This move is not legal").await?;
                        return;
                    }
                    board.play_unchecked(&m);
                    debug!("playing move {m}");

                    let ended = board.is_game_over();
                    let fen =
                    Fen::from_position(board.clone(), shakmaty::EnPassantMode::Always)
                        .to_string();
                    let winner = if ended {
                        Some(if board.turn().is_white() { w_id } else { b_id })
                    } else {
                        None
                    };
                    let termination = board.outcome().and_then(|o| match o {
                        Outcome::Draw => None,
                        Outcome::Decisive {
                            winner: Color::Black,
                        } => Some(true),
                        Outcome::Decisive {
                            winner: Color::White,
                        } => Some(false),
                    });

                    sqlx::query(
                        "insert into moves (game_id, ply, uci) values ($1, (select count(*) from moves where game_id = $1), $2)"
                    )
                        .bind(id)
                        .bind(m.to_uci(CastlingMode::Standard).to_string())
                        .execute(&db).await?;

                    sqlx::query(
                        "update games set ended = $1, winner = $2, termination = $3, fen = $4 where id = $1")
                        .bind(ended)
                        .bind(winner)
                        .bind(termination)
                        .bind(&fen)
                        .bind(id)
                        .execute(&db).await?;

                    for &c in [packed_chat(w_id), packed_chat(b_id)].iter() {
                        // show fen image
                        client.send_message(c, format!("Played {m}, FEN is now {fen}")).await?;
                        if ended {
                            client.send_message(c, format!("Game is over")).await?;
                        }
                    }
                    if ended {
                        boards.remove(&id);
                    }
                }
            }
        }
        _ => {
            info!("unhandled update {update:?}");
        }
    }
    Ok(())
}

async fn async_main() -> Result<()> {
    env_logger::init();

    let api_id = env!("TG_API_ID").parse().expect("api id invalid");
    let api_hash = env!("TG_API_HASH").to_string();
    let token = env!("TG_BOT_TOKEN").to_string();

    info!("startup");

    const DATABASE_URL: &str = "database.sqlite3";

    use sqlx::migrate::MigrateDatabase;

    if !Sqlite::database_exists(DATABASE_URL).await? {
        info!("create database {}", DATABASE_URL);
        Sqlite::create_database(DATABASE_URL).await?;
    }

    let db = SqlitePool::connect(DATABASE_URL).await?;
    db.execute(include_str!("./schema.sql")).await?;

    let mut boards = HashMap::<i64, Chess>::new();

    info!("connecting to Telegram");
    let client = Client::connect(Config {
        session: Session::load_file_or_create(SESSION_FILE)?,
        api_id,
        api_hash: api_hash.clone(),
        params: InitParams {
            catch_up: true,
            ..Default::default()
        },
    })
    .await?;

    if !client.is_authorized().await? {
        info!("Signing in...");
        client.bot_sign_in(&token).await?;
        client.session().save_to_file(SESSION_FILE)?;
        info!("Signed in!");
    }

    info!("waiting for messages");

    loop {
        let update = match client.next_update().await {
            Ok(u) => u,
            Err(e) => {
                error!("cannot get update: {}", e);
                continue;
            }
        };
        match update {
            Some(update) => handle_update(&mut client, db, update),
            None => break,
        }
        
    }

    info!("Saving session file and exiting...");
    client.session().save_to_file(SESSION_FILE)?;
    Ok(())
}

fn main() -> Result<()> {
    runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async_main())
}
