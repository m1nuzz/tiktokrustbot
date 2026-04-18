use teloxide::prelude::*;
use teloxide::types::{KeyboardMarkup, KeyboardButton};
use crate::handlers::admin::is_admin;
use crate::handlers::ui::{BTN_ADMIN_PANEL, BTN_FORMAT, BTN_SETTINGS, BTN_BACK};
use std::sync::Arc;
use crate::database::DatabasePool;

pub async fn settings_text_handler(
    bot: Bot,
    msg: Message
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut rows = vec![
        vec![KeyboardButton::new(BTN_FORMAT)],
    ];

    if is_admin(&msg).await {
        rows.push(vec![KeyboardButton::new(BTN_ADMIN_PANEL)]);
    }

    rows.push(vec![KeyboardButton::new(BTN_BACK)]);

    let keyboard = KeyboardMarkup::new(rows).resize_keyboard();

    bot.send_message(msg.chat.id, BTN_SETTINGS)
        .reply_markup(keyboard)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

    Ok(())
}

pub async fn format_text_handler(
    bot: Bot,
    msg: Message
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let keyboard = KeyboardMarkup::new(vec![
        vec![
            KeyboardButton::new("h265"),
            KeyboardButton::new("h264"),
            KeyboardButton::new("audio"),
        ],
        vec![KeyboardButton::new(BTN_BACK)],
    ])
    .resize_keyboard();

    let text = "h265: best quality, but may not work on some devices.\nh264: worse quality, but works on many devices.\naudio: audio only";

    bot.send_message(msg.chat.id, text)
        .reply_markup(keyboard)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

    Ok(())
}

pub async fn back_text_handler(
    bot: Bot,
    msg: Message
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    bot.send_message(msg.chat.id, "Returning to main menu...")
        .reply_markup(crate::handlers::command::get_main_reply_keyboard())
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

    Ok(())
}

pub async fn subscription_text_handler(
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
        let current_value: String = conn.query_row(
            "SELECT value FROM settings WHERE key = 'subscription_required'",
            [],
            |row| row.get(0),
        )?;
        let new_value = !(current_value == "true");
        conn.execute(
            "UPDATE settings SET value = ?1 WHERE key = 'subscription_required'",
            rusqlite::params![new_value.to_string()],
        )?;
        Ok(new_value)
    }).await;

    match result {
        Ok(new_value) => {
            let status = if new_value { "enabled" } else { "disabled" };
            log::info!("Subscription setting changed to {} in database", status);
            bot.send_message(msg.chat.id, format!("Mandatory subscription is now {}", status))
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
        Err(e) => {
            log::error!("ToggleSubscription DB error: {}", e);
            bot.send_message(msg.chat.id, "Failed to toggle subscription setting.")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
    }

    Ok(())
}
