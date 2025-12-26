use teloxide::prelude::*;
use teloxide::types::{KeyboardMarkup, KeyboardButton};
use crate::handlers::admin::is_admin;
use crate::handlers::ui::{BTN_ADMIN_PANEL, BTN_FORMAT, BTN_SETTINGS, BTN_BACK};

pub async fn settings_text_handler(bot: Bot, msg: Message) -> Result<(), anyhow::Error> {
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
        .await?;

    Ok(())
}

pub async fn format_text_handler(bot: Bot, msg: Message) -> Result<(), anyhow::Error> {
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
        .await?;
    
    Ok(())
}

pub async fn back_text_handler(bot: Bot, msg: Message) -> Result<(), anyhow::Error> {
    bot.send_message(msg.chat.id, "Returning to main menu...")
        .reply_markup(crate::handlers::command::get_main_reply_keyboard())
        .await?;
    
    Ok(())
}
