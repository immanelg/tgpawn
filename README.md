# tgpawn
Daily anonymous chess in Telegram.

## Running
Get your API ID, API hash and Bot Token.
```sh
export RUST_LOG="info,tgpawn=debug"
export RUST_BACKTRACE=1

export DATABASE_URL="database.sqlite3"
export SESSION_FILE="app.session"

export TG_BOT_TOKEN="1234567qwerty"
export TG_API_ID="12345" 
export TG_API_HASH="12345qwerty"
cargo run
```
