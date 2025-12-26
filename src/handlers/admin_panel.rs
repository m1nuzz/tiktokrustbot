use teloxide::prelude::*;
use teloxide::types::{KeyboardMarkup, KeyboardButton};
use crate::handlers::admin::is_admin;
use crate::handlers::ui::{BTN_ADMIN_PANEL, BTN_BACK};
use crate::database::DatabasePool;
use std::sync::Arc;

pub async fn admin_panel_text_handler(bot: Bot, msg: Message) -> Result<(), anyhow::Error> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.").await?;
        return Ok(());
    }

    let keyboard = KeyboardMarkup::new(vec![
        vec![KeyboardButton::new("Stats"), KeyboardButton::new("Top 10")],
        vec![KeyboardButton::new("All users")],
        vec![KeyboardButton::new(BTN_BACK)],
    ])
    .resize_keyboard();

    bot.send_message(msg.chat.id, BTN_ADMIN_PANEL)
        .reply_markup(keyboard)
        .await?;

    Ok(())
}

// Новые обработчики
pub async fn stats_text_handler(bot: Bot, msg: Message, db_pool: Arc<DatabasePool>) -> Result<(), anyhow::Error> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.").await?;
        return Ok(());
    }

    let result = db_pool.execute_with_timeout(|conn| {
        let total_users: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        let total_downloads: i64 = conn.query_row("SELECT COUNT(*) FROM downloads", [], |row| row.get(0))?;
        Ok((total_users, total_downloads))
    }).await;

    match result {
        Ok((total_users, total_downloads)) => {
            let response = format!(
                "📊 Statistics\n\n\
                 👥 Total users: {}\n\
                 📥 Total downloads: {}",
                total_users, total_downloads
            );
            bot.send_message(msg.chat.id, response).await?;
        }
        Err(e) => {
            log::error!("Stats DB error: {}", e);
            bot.send_message(msg.chat.id, "Failed to retrieve statistics.").await?;
        }
    }

    Ok(())
}

pub async fn top10_text_handler(bot: Bot, msg: Message, db_pool: Arc<DatabasePool>) -> Result<(), anyhow::Error> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.").await?;
        return Ok(());
    }

    let result = db_pool.execute_with_timeout(|conn| {
        let mut stmt = conn.prepare(
            "SELECT user_telegram_id, COUNT(*) as count 
             FROM downloads 
             GROUP BY user_telegram_id 
             ORDER BY count DESC 
             LIMIT 10"
        )?;
        
        let users_iter = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut users = Vec::new();
        for user_result in users_iter {
            users.push(user_result?);
        }
        Ok(users)
    }).await;

    match result {
        Ok(users) => {
            let mut response = String::from("🏆 Top 10 Users\n\n");
            for (index, (user_id, count)) in users.iter().enumerate() {
                response.push_str(&format!("{}. User {} - {} downloads\n", index + 1, user_id, count));
            }
            
            bot.send_message(msg.chat.id, response).await?;
        }
        Err(e) => {
            log::error!("Top 10 DB error: {}", e);
            bot.send_message(msg.chat.id, "Failed to retrieve top users.").await?;
        }
    }

    Ok(())
}

pub async fn all_users_text_handler(bot: Bot, msg: Message, db_pool: Arc<DatabasePool>) -> Result<(), anyhow::Error> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.").await?;
        return Ok(());
    }

    let result = db_pool.execute_with_timeout(|conn| {
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        
        let mut stmt = conn.prepare(
            "SELECT telegram_id, last_active 
             FROM users 
             ORDER BY last_active DESC 
             LIMIT 50"
        )?;
        
        let users_iter = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut users = Vec::new();
        for user_result in users_iter {
            users.push(user_result?);
        }
        Ok((count, users))
    }).await;

    match result {
        Ok((total_count, users)) => {
            let mut response = format!("👥 All Users - Total: {}\n\nShowing last 50:\n\n", total_count);
            for (user_id, last_active) in users.iter() {
                response.push_str(&format!("• User {}: {}\n", user_id, last_active));
            }
            
            bot.send_message(msg.chat.id, response).await?;
        }
        Err(e) => {
            log::error!("All users DB error: {}", e);
            bot.send_message(msg.chat.id, "Failed to retrieve users list.").await?;
        }
    }

    Ok(())
}
