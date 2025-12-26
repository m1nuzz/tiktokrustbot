use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardMarkup, InlineKeyboardButton};
use teloxide::utils::command::BotCommands;

use crate::commands::Command;
use crate::database::DatabasePool;
use std::sync::Arc;
use crate::handlers::ui::BTN_SETTINGS;


pub async fn command_handler(bot: Bot, msg: Message, cmd: Command, db_pool: Arc<DatabasePool>) -> Result<(), anyhow::Error> {
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
            let keyboard = InlineKeyboardMarkup::new(vec![vec![
                InlineKeyboardButton::callback(BTN_SETTINGS, "settings"),
            ]]);
            bot.send_message(msg.chat.id, "Welcome! Send me a TikTok link.").reply_markup(keyboard).await?;
        }
        Command::Help => {
            bot.send_message(msg.chat.id, Command::descriptions().to_string()).await?;
        }
    };
    Ok(())
}