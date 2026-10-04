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


/// /start entry point (takes precedence over the bare Command::Start arm so
/// the deep-link payload survives parsing). Registers the user, stores the
/// first-touch ref_code for traffic attribution, and sends the welcome.
pub async fn start_with_payload_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let user_id = msg.chat.id.0;
    let payload = msg
        .text()
        .and_then(parse_start_payload)
        .flatten();

    let ref_owned = payload.clone();
    let result = db_pool.execute_with_timeout(move |conn| {
        conn.execute("INSERT OR IGNORE INTO users (telegram_id) VALUES (?1)", [user_id])?;
        conn.execute("UPDATE users SET last_active = CURRENT_TIMESTAMP WHERE telegram_id = ?1", [user_id])?;
        // First-touch attribution only: never overwrite an existing ref_code.
        if let Some(r) = ref_owned {
            conn.execute(
                "UPDATE users SET ref_code = ?1 WHERE telegram_id = ?2 AND ref_code IS NULL",
                rusqlite::params![r, user_id],
            )?;
        }
        Ok(())
    }).await;

    if let Err(e) = result {
        log::error!("Failed to update user activity: {}", e);
    }
    if let Some(r) = &payload {
        log::info!("User {} started with ref '{}'", user_id, r);
    }
    db_pool.log_funnel_event(user_id, "start").await;

    let tg_lang = msg
        .from
        .as_ref()
        .and_then(|u| u.language_code.as_deref());
    let lang = db_pool.get_effective_lang(user_id, tg_lang).await;

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
    db_pool: Arc<DatabasePool>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let user_id = msg.chat.id.0;
    let result = db_pool.execute_with_timeout(move |conn| {
        conn.execute("INSERT OR IGNORE INTO users (telegram_id) VALUES (?1)", [user_id])?;
        conn.execute("UPDATE users SET last_active = CURRENT_TIMESTAMP WHERE telegram_id = ?1", [user_id])?;
        Ok(())
    }).await;

    if let Err(e) = result {
        log::error!("Failed to update user activity: {}", e);
    }

    let tg_lang = msg
        .from
        .as_ref()
        .and_then(|u| u.language_code.as_deref());
    let lang = db_pool.get_effective_lang(user_id, tg_lang).await;

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
}