use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;
use rusqlite::{Result, params};
use std::env;
use std::sync::Arc;

use crate::commands::AdminCommand;
use crate::database::DatabasePool;

pub async fn is_admin(msg: &Message) -> bool {
    let admin_ids_str = env::var("ADMIN_IDS").unwrap_or_default();
    let admin_ids: Vec<i64> = admin_ids_str
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    
    admin_ids.contains(&msg.chat.id.0)
}

pub async fn admin_command_handler(bot: Bot, msg: Message, db_pool: Arc<DatabasePool>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This command is for admins only.").await?;
        return Ok(())
    }

    let text = msg.text().unwrap_or_default();
    let cmd = match AdminCommand::parse(text, "admin") {
        Ok(cmd) => cmd,
        Err(_) => {
            bot.send_message(msg.chat.id, "Unknown admin command or invalid format.").await?;
            return Ok(())
        }
    };

    match cmd {
        AdminCommand::AddChannel(id_name) => {
            let parts: Vec<&str> = id_name.splitn(2, ' ').collect();
            if parts.len() == 2 {
                let id = parts[0].to_string();
                let name = parts[1].to_string();
                let id_cloned_for_format = id.clone();
                let name_cloned_for_format = name.clone();
                
                let result = db_pool.execute_with_timeout(move |conn| {
                    conn.execute("INSERT OR REPLACE INTO channels (channel_id, channel_name) VALUES (?1, ?2)", params![id, name])
                }).await;

                match result {
                    Ok(_) => {
                        bot.send_message(msg.chat.id, format!("Channel {} ({}) added.", name_cloned_for_format, id_cloned_for_format)).await?;
                    }
                    Err(e) => {
                        log::error!("AddChannel DB error: {}", e);
                        bot.send_message(msg.chat.id, "Failed to add channel.").await?;
                    }
                }
            } else {
                bot.send_message(msg.chat.id, "Usage: /addchannel <id> <name>").await?;
            }
        }
        AdminCommand::DelChannel(id) => {
            let id_cloned_for_format = id.clone();
            
            let result = db_pool.execute_with_timeout(move |conn| {
                conn.execute("DELETE FROM channels WHERE channel_id = ?1", params![id])
            }).await;

            match result {
                Ok(changes) => {
                    if changes > 0 {
                        bot.send_message(msg.chat.id, format!("Channel {} deleted.", id_cloned_for_format)).await?;
                    }
                    else {
                        bot.send_message(msg.chat.id, format!("Channel {} not found.", id_cloned_for_format)).await?;
                    }
                }
                Err(e) => {
                    log::error!("DelChannel DB error: {}", e);
                    bot.send_message(msg.chat.id, "Failed to delete channel.").await?;
                }
            }
        }
        AdminCommand::ListChannels => {
            let result = db_pool.execute_with_timeout(|conn| {
                let mut stmt = conn.prepare("SELECT channel_id, channel_name FROM channels")?;
                let channels_iter = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
                let mut channels = Vec::new();
                for channel_result in channels_iter {
                    channels.push(channel_result?);
                }
                Ok(channels)
            }).await;

            match result {
                Ok(channels) => {
                    let mut response = String::from("Subscription channels:\n");
                    for (id, name) in channels {
                        response.push_str(&format!("- {} ({})\n", name, id));
                    }
                    bot.send_message(msg.chat.id, response).await?;
                }
                Err(e) => {
                    log::error!("ListChannels DB error: {}", e);
                    bot.send_message(msg.chat.id, "Failed to list channels.").await?;
                }
            }
        }
        AdminCommand::ToggleSubscription => {
            let result = db_pool.execute_with_timeout(|conn| {
                let current_value: String = conn.query_row(
                    "SELECT value FROM settings WHERE key = 'subscription_required'",
                    [],
                    |row| row.get(0),
                )?;
                let new_value = !(current_value == "true");
                conn.execute(
                    "UPDATE settings SET value = ?1 WHERE key = 'subscription_required'",
                    params![new_value.to_string()],
                )?;
                Ok(new_value)
            }).await;

            match result {
                Ok(new_value) => {
                    let status = if new_value { "enabled" } else { "disabled" };
                    bot.send_message(msg.chat.id, format!("Mandatory subscription is now {}", status)).await?;
                }
                Err(e) => {
                    log::error!("ToggleSubscription DB error: {}", e);
                    bot.send_message(msg.chat.id, "Failed to toggle subscription setting.").await?;
                }
            }
        }
    }

    Ok(())
}