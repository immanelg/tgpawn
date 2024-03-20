use anyhow::Result;
use grammers_client::types::Chat;
use grammers_client::{Client, Config, InitParams, Update};
use grammers_session::{Session, PackedChat};
use log::{error, info};
use shakmaty::san::San;
use shakmaty::{Chess, Move, Position};
use sqlx::{Connection, Executor};
use tokio::sync::mpsc::{UnboundedSender, UnboundedReceiver};
use std::pin::pin;
use std::{collections::HashMap, env};
use tokio::{runtime, task};

const SESSION_FILE: &str = "app.session";

async fn async_main() -> Result<()> {
    env_logger::init();

    let api_id = env::var("TG_API_ID")
        .expect("provide TG_API_ID")
        .parse()
        .expect("api id invalid");
    let api_hash = env::var("TG_API_HASH")
        .expect("provide TG_API_HASH")
        .to_string();
    let token = env::var("TG_BOT_TOKEN")
        .expect("provide TG_BOT_TOKEN")
        .to_string();

    info!("startup");
    const DB_URL: &str = "sqlite://database.sqlite3";

    use sqlx::migrate::MigrateDatabase;

    if !sqlx::Sqlite::database_exists(DB_URL).await? {
        sqlx::Sqlite::create_database(DB_URL).await?;
    }

    info!("opening db");

    let mut db = sqlx::SqliteConnection::connect(DB_URL).await.expect("open sqlite3 db");

    let create_schema = sqlx::query("
        create table if not exists users (
            id integer primary key
        );
        create table if not exists games (
            id integer primary key,
            w_id integer,
            b_id integer,
            winner text,
            ended boolean,
            fen text not null,
            foreign key (w_id) references users (id)
            foreign key (b_id) references users (id)
        );
        create table if not exists moves (
            game_id integer,
            ply integer not null,
            src text not null,
            dest text not null,
            foreign key (game_id) references games (id)
        );
    ");

    create_schema.execute(&mut db).await.expect("create db schema");

    let mut boards = HashMap::<i64, Chess>::new();

    // let mut board = Chess::default();
    // let san = "Nf3".parse::<San>().unwrap();
    // board.play(&san.to_move(&board).unwrap());

    info!("Connecting to Telegram...");
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
    info!("Connected!");

    if !client.is_authorized().await? {
        info!("Signing in...");
        client.bot_sign_in(&token).await?;
        client.session().save_to_file(SESSION_FILE)?;
        info!("Signed in!");
    }

    info!("Waiting for messages...");

    loop {
        let update = match client.next_update().await {
            Ok(u) => u,
            Err(e) => {
                error!("cannot get update: {}", e);
                continue;
            }
        };
        match update {
            Some(Update::NewMessage(message)) if !message.outgoing() => {
                let chat = message.chat();
                let user_id = chat.id();
                let user_name = chat.name();
                let text = message.text();

                info!("Message {user_id} {user_name}: {text}");

                sqlx::query("insert or ignore into users values ($1)")
                    .bind(user_id).execute(&mut db).await.unwrap();

                let user_ongoing_game: Option<(i64, i64, i64, String, String)> = sqlx::query_as("select (id, w_id, b_id, winner, fen) from games where (w_id = $1 or b_id = $1) and ended = 0")
                    .bind(user_id).fetch_optional(&mut db).await.unwrap();

                match text.as_ref() {
                    "play" => {
                        if user_ongoing_game == None {
                            todo!("create game");
                        } else {
                            todo!("reject: already in game");
                        }
                    }
                    "resign" => {
                        if let Some((id, w_id, b_id, winner, fen)) = user_ongoing_game {
                            todo!("resign");
                        } else {
                            todo!("reject: need to join a game");
                        }
                    }
                    _m => {
                        if let Some((id, w_id, b_id, winner, fen)) = user_ongoing_game {
                            todo!("parse and make move (uci || san)");
                        } else {
                            todo!("reject: need to join a game");
                        }
                    }
                }

                let c = PackedChat { id: user_id, ty: grammers_session::PackedType::User, access_hash: None };

                dbg!(&chat);
                dbg!(&message);
                client.send_message(c, message.text()).await.unwrap();
            }
            None => break,
            _ => {}
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
