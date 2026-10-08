use teloxide::prelude::*;
use teloxide::types::{KeyboardMarkup, KeyboardButton};
use teloxide::utils::command::BotCommands;

use crate::commands::Command;
use crate::database::DatabasePool;
use crate::handlers::language::language_command_handler;
use crate::i18n::{self, MsgKey};
use std::sync::Arc;

pub fn get_main_reply_keyboard() -> KeyboardMarkup {
    KeyboardMarkup::new(vec![vec![
        KeyboardButton::new("⚙️ Settings"),
    ]])
    .resize_keyboard()
}

/// Parse a /start command, returning the optional deep-link payload.
/// Accepts `/start`, `/start <payload>`, `/start@BotName` and
/// `/start@BotName <payload>`. Returns None for anything else.
pub fn parse_start_payload(text: &str) -> Option<Option<String>> {
    let mut parts = text.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let base = cmd.split('@').next().unwrap_or("");
    if base != "/start" {
        return None;
    }
    Some(
        parts
            .next()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    )
}


/// Register the sender and store first-touch referral attribution.
/// Extracted from the handler so the attribution rule is testable without a Bot:
/// a deep link that is NOT a referral code must never reach this, or the bot
/// attributes its own deep link to the campaign that owns `ref_code`.
pub async fn record_start_attribution(
    db_pool: &DatabasePool,
    bot_id: &str,
    user_id: i64,
    payload: Option<String>,
) -> Result<(), anyhow::Error> {
    let bot_owned = bot_id.to_string();
    db_pool
        .execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO users (bot_id, telegram_id) VALUES (?1, ?2)",
                rusqlite::params![bot_owned, user_id],
            )?;
            conn.execute(
                "UPDATE users SET last_active = CURRENT_TIMESTAMP WHERE bot_id = ?1 AND telegram_id = ?2",
                rusqlite::params![bot_owned, user_id],
            )?;
            // First-touch attribution only: never overwrite an existing ref_code.
            if let Some(r) = payload {
                conn.execute(
                    "UPDATE users SET ref_code = ?1 WHERE bot_id = ?2 AND telegram_id = ?3 AND ref_code IS NULL",
                    rusqlite::params![r, bot_owned, user_id],
                )?;
            }
            Ok(())
        })
        .await
}

/// /start entry point (takes precedence over the bare Command::Start arm so
/// the deep-link payload survives parsing). Registers the user, stores the
/// first-touch ref_code for traffic attribution, and sends the welcome.
pub async fn start_with_payload_handler(
    bot: Bot,
    msg: Message,
    ctx: crate::BotCtx,
    db_pool: Arc<DatabasePool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let user_id = msg.chat.id.0;
    let bot_id = ctx.bot_id.as_str();
    let payload = msg
        .text()
        .and_then(parse_start_payload)
        .flatten();

    if let Err(e) = record_start_attribution(&db_pool, bot_id, user_id, payload.clone()).await {
        log::error!("Failed to update user activity: {}", e);
    }
    if let Some(r) = &payload {
        log::info!("User {} started with ref '{}'", user_id, r);
    }
    db_pool.log_funnel_event(bot_id, user_id, "start").await;

    let tg_lang = msg
        .from
        .as_ref()
        .and_then(|u| u.language_code.as_deref());
    let lang = db_pool.get_effective_lang(bot_id, user_id, tg_lang).await;

    bot.send_message(msg.chat.id, i18n::t(MsgKey::Welcome, Some(lang.as_str())))
        .reply_markup(get_main_reply_keyboard())
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    Ok(())
}

pub async fn command_handler(
    bot: Bot,
    msg: Message,
    cmd: Command,
    ctx: crate::BotCtx,
    db_pool: Arc<DatabasePool>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let user_id = msg.chat.id.0;
    let bot_id = ctx.bot_id.as_str();
    let bot_owned = ctx.bot_id.clone();
    let result = db_pool.execute_with_timeout(move |conn| {
        conn.execute("INSERT OR IGNORE INTO users (bot_id, telegram_id) VALUES (?1, ?2)", rusqlite::params![bot_owned, user_id])?;
        conn.execute("UPDATE users SET last_active = CURRENT_TIMESTAMP WHERE bot_id = ?1 AND telegram_id = ?2", rusqlite::params![bot_owned, user_id])?;
        Ok(())
    }).await;

    if let Err(e) = result {
        log::error!("Failed to update user activity: {}", e);
    }

    let tg_lang = msg
        .from
        .as_ref()
        .and_then(|u| u.language_code.as_deref());
    let lang = db_pool.get_effective_lang(bot_id, user_id, tg_lang).await;

    match cmd {
        Command::Start => {
            bot.send_message(msg.chat.id, i18n::t(MsgKey::Welcome, Some(lang.as_str())))
                .reply_markup(get_main_reply_keyboard())
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
        Command::Help => {
            bot.send_message(msg.chat.id, Command::descriptions().to_string())
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
        Command::Language => {
            language_command_handler(bot, msg).await?;
        }
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_payload_parsing() {
        assert_eq!(parse_start_payload("/start"), Some(None));
        assert_eq!(
            parse_start_payload("/start ref123"),
            Some(Some("ref123".to_string()))
        );
        assert_eq!(
            parse_start_payload("/start   spaced  "),
            Some(Some("spaced".to_string()))
        );
        assert_eq!(parse_start_payload("/start@tikyoubot"), Some(None));
        assert_eq!(
            parse_start_payload("/start@tikyoubot promo42"),
            Some(Some("promo42".to_string()))
        );
        assert_eq!(parse_start_payload("/help"), None);
        assert_eq!(parse_start_payload("hello"), None);
        assert_eq!(parse_start_payload("/startsomething"), None);
    }

    async fn ref_code_of(pool: &crate::database::DatabasePool, bot_id: &str, user_id: i64) -> Option<String> {
        let bot_owned = bot_id.to_string();
        pool.execute_with_timeout(move |conn| {
            conn.query_row(
                "SELECT ref_code FROM users WHERE bot_id = ?1 AND telegram_id = ?2",
                rusqlite::params![bot_owned, user_id],
                |row| row.get::<_, Option<String>>(0),
            )
        })
        .await
        .unwrap()
    }

    /// A referral payload is still recorded, and first-touch wins: this is the
    /// branch `/start premium` must be diverted away from.
    #[tokio::test]
    async fn start_attribution_records_the_ref_code_once() {
        let (pool, _file) = crate::database::setup_test_db().await;

        record_start_attribution(&pool, "primary", 71, Some("site".to_string()))
            .await
            .unwrap();
        assert_eq!(ref_code_of(&pool, "primary", 71).await, Some("site".to_string()));

        // A later press must never overwrite the first touch.
        record_start_attribution(&pool, "primary", 71, Some("other".to_string()))
            .await
            .unwrap();
        assert_eq!(ref_code_of(&pool, "primary", 71).await, Some("site".to_string()));

        // Same human on another bot is a different row: no ref leaks across.
        record_start_attribution(&pool, "bbb", 71, Some("other-bot".to_string()))
            .await
            .unwrap();
        assert_eq!(ref_code_of(&pool, "bbb", 71).await, Some("other-bot".to_string()));
        assert_eq!(ref_code_of(&pool, "primary", 71).await, Some("site".to_string()));
    }

    /// A `/start` with no payload registers the user without inventing
    /// attribution for them.
    #[tokio::test]
    async fn start_without_a_payload_writes_no_ref_code() {
        let (pool, _file) = crate::database::setup_test_db().await;

        record_start_attribution(&pool, "primary", 72, None).await.unwrap();
        assert_eq!(ref_code_of(&pool, "primary", 72).await, None);
    }
}