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
use sqlx::{Connection, Executor, Pool};
use std::error;
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

struct State {
    db: Pool<Sqlite>,
    client: Client,
    boards: HashMap<i64, Chess>,
}

fn packed_chat(id: i64) -> PackedChat {
    PackedChat {
        id,
        ty: grammers_session::PackedType::User,
        access_hash: None,
    }
}

fn parse_move(notation: &str, board: &impl Position) -> Option<Move> {
    if let Some(m) = San::from_ascii(notation.as_bytes())
        .ok()
        .and_then(|san| san.to_move(board).ok())
    {
        return Some(m);
    }

    Uci::from_ascii(notation.as_bytes())
        .ok()
        .and_then(|uci| uci.to_move(board).ok())
}

async fn on_start(state: &mut State, user_id: i64) -> Result<()> {
    let maybe_playing_game: Option<(i64, i64, i64, bool, i64, String)> = sqlx::query_as(
        "select id, w_id, b_id, winner, termination, fen from games where (w_id = $1 or b_id = $1) and ended = 0",
    ).bind(user_id).fetch_optional(&state.db).await?;

    if maybe_playing_game.is_some() {
        debug!("already in game {user_id}");
        state
            .client
            .send_message(
                packed_chat(user_id),
                "You are already playing. Type `resign` to leave.",
            )
            .await?;
        return Ok(());
    };

    let maybe_pairable: Option<(i64, Option<i64>, Option<i64>)> = sqlx::query_as("select id, w_id, b_id from games where (b_id is null or w_id is null) and ended = 0 limit 1")
        .fetch_optional(&state.db).await?;
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
            "update games set w_id = $1, b_id = $2 where games.id = $3 returning id, w_id, b_id",
        )
        .bind(w_id)
        .bind(b_id)
        .bind(id)
        .fetch_one(&state.db)
        .await?;
        let (white, black) = (packed_chat(w_id), packed_chat(b_id));
        state
            .client
            .send_message(white, "You are white. Your turn!")
            .await?;
        state
            .client
            .send_message(black, "You are black. Waiting for opponent's move.")
            .await?;
    } else {
        let (id,) = sqlx::query_as::<_, (i64,)>("insert into games (w_id, b_id, winner, ended, fen) values ($1, null, null, 0, $2) returning id").bind(user_id).bind(STARTING_FEN).fetch_one(&state.db).await?;
        debug!("create new game {id}");
        state
            .client
            .send_message(
                packed_chat(user_id),
                "Created a new game. Waiting for an opponent to join.",
            )
            .await?;
    }
    Ok(())
}

async fn on_move(state: &mut State, user_id: i64, notation: &str) -> Result<()> {
    let mut tx = state.db.begin().await?;

    let maybe_playing_game: Option<(i64, i64, i64, bool, i64, String)> = sqlx::query_as(
        "select id, w_id, b_id, winner, termination, fen from games where (w_id = $1 or b_id = $1) and ended = 0",
    ).bind(user_id).fetch_optional(&mut *tx).await?;

    debug!("get ongoing game for {user_id}: got {maybe_playing_game:?}");

    let Some((id, w_id, b_id, _winner, _termination, fen)) = maybe_playing_game else {
        state
            .client
            .send_message(packed_chat(user_id), "Type `start` to join a game")
            .await?;
        return Ok(());
    };
    let board = state.boards.entry(id).or_insert_with(|| {
        fen.parse::<Fen>()
            .expect("fen from db")
            .into_position(CastlingMode::Standard)
            .expect("valid initial position")
    });
    if !(board.turn() == Color::White && user_id == w_id
        || board.turn() == Color::Black && user_id == b_id)
    {
        state
            .client
            .send_message(packed_chat(user_id), "Not your turn!")
            .await?;
        return Ok(());
    }
    let Some(m) = parse_move(notation, board) else {
        state
            .client
            .send_message(packed_chat(user_id), "This is not a valid move")
            .await?;
        return Ok(());
    };
    if !board.is_legal(&m) {
        state
            .client
            .send_message(packed_chat(user_id), "This move is not legal")
            .await?;
        return Ok(());
    }
    board.play_unchecked(&m);
    debug!("playing move {m}");

    let ended = board.is_game_over();
    let fen = Fen::from_position(board.clone(), shakmaty::EnPassantMode::Always).to_string();
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
        .execute(&mut *tx).await?;

    sqlx::query(
        "update games set ended = $1, winner = $2, termination = $3, fen = $4 where id = $1",
    )
    .bind(ended)
    .bind(winner)
    .bind(termination)
    .bind(&fen)
    .bind(id)
    .execute(&mut *tx)
    .await?;

    for &c in [packed_chat(w_id), packed_chat(b_id)].iter() {
        // show fen image
        state
            .client
            .send_message(c, format!("Played {m}, FEN is now {fen}"))
            .await?;
        if ended {
            state
                .client
                .send_message(packed_chat(user_id), format!("Game is over"))
                .await?;
        }
    }
    if ended {
        state.boards.remove(&id);
    }
    Ok(())
}

async fn on_resign(state: &mut State, user_id: i64) -> Result<()> {
    let maybe_playing_game: Option<(i64, i64, i64, bool, i64, String)> = sqlx::query_as(
        "select id, w_id, b_id, winner, termination, fen from games where (w_id = $1 or b_id = $1) and ended = 0",
    ).bind(user_id).fetch_optional(&state.db).await?;

    debug!("get ongoing game for {user_id}: got {maybe_playing_game:?}");

    if let Some(_) = maybe_playing_game {
        error!("todo: resign");
    } else {
        error!("reject: need to join a game");
    }
    Ok(())
}

async fn handle_update(state: &mut State, update: Update) -> Result<()> {
    match update {
        Update::NewMessage(message) if !message.outgoing() => {
            let chat = message.chat();
            let user_id = chat.id();
            let user_name = chat.name();
            let text = message.text();

            // let c = packed_chat(user_id);

            info!("message by {user_id} {user_name}: {text}");

            sqlx::query("insert or ignore into users (id) values ($1)")
                .bind(user_id)
                .execute(&state.db)
                .await?;

            debug!("insert user {user_id}");

            let maybe_playing_game: Option<(i64, i64, i64, bool, i64, String)> = sqlx::query_as(
                "select id, w_id, b_id, winner, termination, fen from games where (w_id = $1 or b_id = $1) and ended = 0",
            ).bind(user_id).fetch_optional(&state.db).await?;

            debug!("get ongoing game for {user_id}: got {maybe_playing_game:?}");

            match text.as_ref() {
                "/start" => {
                    on_start(state, user_id).await?;
                }
                "/resign" => {
                    on_resign(state, user_id).await?;
                }
                notation => {
                    on_move(state, user_id, notation).await?;
                }
            }
        }
        _ => {
            debug!("unhandled update {update:?}");
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
            catch_up: false,
            ..Default::default()
        },
    })
    .await?;

    if !client.is_authorized().await? {
        client.bot_sign_in(&token).await?;
        client.session().save_to_file(SESSION_FILE)?;
        info!("signed in");
    }

    let mut state = State { client, db, boards };

    info!("waiting for messages");

    loop {
        let update = match state.client.next_update().await {
            Ok(u) => u,
            Err(e) => {
                error!("cannot get update: {}", e);
                continue;
            }
        };
        match update {
            Some(update) => {
                if let Err(e) = handle_update(&mut state, update).await {
                    error!("error while handling update {e}");
                }
            }
            None => break,
        }
    }

    info!("exiting");
    state.client.session().save_to_file(SESSION_FILE)?;

    Ok(())
}

fn main() -> Result<()> {
    runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async_main())
}
