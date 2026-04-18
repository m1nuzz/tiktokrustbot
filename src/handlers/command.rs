use teloxide::prelude::*;
use teloxide::types::{KeyboardMarkup, KeyboardButton};
use teloxide::utils::command::BotCommands;

use crate::commands::Command;
use crate::database::DatabasePool;
use std::sync::Arc;

pub fn get_main_reply_keyboard() -> KeyboardMarkup {
    KeyboardMarkup::new(vec![vec![
        KeyboardButton::new("⚙️ Settings"),
    ]])
    .resize_keyboard()
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

    match cmd {
        Command::Start => {
            bot.send_message(msg.chat.id, "Welcome! Send me a TikTok link.")
                .reply_markup(get_main_reply_keyboard())
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
        Command::Help => {
            bot.send_message(msg.chat.id, Command::descriptions().to_string())
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
    };
    Ok(())
}