use teloxide::prelude::*;
use teloxide::types::{KeyboardMarkup, KeyboardButton};
use crate::handlers::admin::is_admin;
use crate::handlers::ui::{
    BTN_ADMIN_PANEL, BTN_SUBSCRIPTION, BTN_BACK,
    BTN_TOGGLE_ADS, BTN_TOGGLE_SUCCESS_NOTIFS, BTN_TOGGLE_FAIL_NOTIFS
};
use crate::database::DatabasePool;
use std::sync::Arc;

pub const BTN_BROADCAST: &str = "📢 Broadcast";

pub async fn admin_panel_text_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        return Ok(());
    }

    let ads_enabled = db_pool.get_setting("ads_enabled").await.map(|v| v == "true").unwrap_or(true);
    let notify_success = db_pool.get_setting("notify_success").await.map(|v| v == "true").unwrap_or(true);
    let notify_fail = db_pool.get_setting("notify_fail").await.map(|v| v == "true").unwrap_or(true);

    let keyboard = KeyboardMarkup::new(vec![
        vec![KeyboardButton::new("📊 Stats"), KeyboardButton::new("📢 Broadcast")],
        vec![KeyboardButton::new("🏆 Top 10"), KeyboardButton::new("👥 All users")],
        vec![KeyboardButton::new("💎 Premium Users"), KeyboardButton::new("➕ Add Premium User")],
        vec![KeyboardButton::new(BTN_SUBSCRIPTION)],
        vec![
            KeyboardButton::new(format!("{}{}", BTN_TOGGLE_ADS, if ads_enabled { "ON ✅" } else { "OFF ❌" })),
        ],
        vec![
            KeyboardButton::new(format!("{}{}", BTN_TOGGLE_SUCCESS_NOTIFS, if notify_success { "ON ✅" } else { "OFF ❌" })),
            KeyboardButton::new(format!("{}{}", BTN_TOGGLE_FAIL_NOTIFS, if notify_fail { "ON ✅" } else { "OFF ❌" })),
        ],
        vec![KeyboardButton::new(BTN_BACK)],
    ])
    .resize_keyboard();

    bot.send_message(msg.chat.id, BTN_ADMIN_PANEL)
        .reply_markup(keyboard)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

    Ok(())
}

pub async fn add_premium_user_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        return Ok(());
    }

    if let Some(text) = msg.text() {
        if text == "/cancel" {
            bot.send_message(msg.chat.id, "❌ Cancelled.")
                .reply_markup(crate::handlers::command::get_main_reply_keyboard())
                .await?;
            return Ok(());
        }

        match text.parse::<i64>() {
            Ok(user_id) => {
                match db_pool.set_user_premium(user_id, 30).await {
                    Ok(_) => {
                        bot.send_message(msg.chat.id, format!("✅ User {} granted 30 days of Premium!", user_id))
                            .reply_markup(crate::handlers::command::get_main_reply_keyboard())
                            .await?;
                    }
                    Err(e) => {
                        log::error!("Failed to add premium manually: {}", e);
                        bot.send_message(msg.chat.id, "❌ Database error.")
                            .await?;
                    }
                }
            }
            Err(_) => {
                bot.send_message(msg.chat.id, "⚠️ Please send a valid numeric Telegram ID (or /cancel):")
                    .await?;
            }
        }
    }

    Ok(())
}

pub async fn premium_users_text_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        return Ok(());
    }

    let result = db_pool.get_premium_users().await;

    match result {
        Ok(users) => {
            let mut response = format!("💎 Premium Users - Total: {}\n\n", users.len());
            for (user_id, premium_until, last_active) in users.iter() {
                response.push_str(&format!(
                    "👤 User: {} | 📅 Until: {} | 🕒 Last active: {}\n",
                    user_id, premium_until, last_active
                ));
            }
            if users.is_empty() {
                response.push_str("No active premium users found.");
            }
            bot.send_message(msg.chat.id, response)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
        Err(e) => {
            log::error!("Premium users DB error: {}", e);
            bot.send_message(msg.chat.id, "Failed to retrieve premium users list.")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
    }

    Ok(())
}

// New handlers
pub async fn stats_text_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
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
            bot.send_message(msg.chat.id, response)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
        Err(e) => {
            log::error!("Stats DB error: {}", e);
            bot.send_message(msg.chat.id, "Failed to retrieve statistics.")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
    }

    Ok(())
}

pub async fn top10_text_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
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

            bot.send_message(msg.chat.id, response)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
        Err(e) => {
            log::error!("Top 10 DB error: {}", e);
            bot.send_message(msg.chat.id, "Failed to retrieve top users.")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
    }

    Ok(())
}

pub async fn all_users_text_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        return Ok(());
    }

    // SQL query with LEFT JOIN and COUNT
    let result = db_pool.execute_with_timeout(|conn| {
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;

        let mut stmt = conn.prepare(
            "SELECT u.telegram_id, u.last_active, COUNT(d.id) as download_count
             FROM users u
             LEFT JOIN downloads d ON u.telegram_id = d.user_telegram_id
             GROUP BY u.telegram_id, u.last_active
             ORDER BY download_count DESC
             LIMIT 50"
        )?;

        let users_iter = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,      // telegram_id
                row.get::<_, String>(1)?,   // last_active
                row.get::<_, i64>(2)?       // download_count
            ))
        })?;

        let mut users = Vec::new();
        for user_result in users_iter {
            users.push(user_result?);
        }
        Ok((count, users))
    }).await;

    match result {
        Ok((total_count, users)) => {
            let mut response = format!("📊 All Users - Total: {} (last 50)\n\n", total_count);
            for (user_id, last_active, downloads) in users.iter() {
                response.push_str(&format!(
                    "👤 User: {} | 📥 Downloads: {} | 🕒 {}\n",
                    user_id, downloads, last_active
                ));
            }
            bot.send_message(msg.chat.id, response)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
        Err(e) => {
            log::error!("All users DB error: {}", e);
            bot.send_message(msg.chat.id, "Failed to retrieve users list.")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
    }

    Ok(())
}
