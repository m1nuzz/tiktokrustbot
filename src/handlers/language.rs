use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{KeyboardButton, KeyboardMarkup};

use crate::database::DatabasePool;
use crate::handlers::ui::{BTN_AUTO_DETECT, BTN_BACK};
use crate::i18n::{self, MsgKey};

/// Reply keyboard: auto-detect first, then one button per supported language
/// (two per row).
pub fn language_keyboard() -> KeyboardMarkup {
    let mut rows: Vec<Vec<KeyboardButton>> = vec![vec![KeyboardButton::new(BTN_AUTO_DETECT)]];
    let mut row: Vec<KeyboardButton> = Vec::new();
    for (_, label) in i18n::LANG_BUTTONS {
        row.push(KeyboardButton::new(*label));
        if row.len() == 2 {
            rows.push(std::mem::take(&mut row));
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows.push(vec![KeyboardButton::new(BTN_BACK)]);
    KeyboardMarkup::new(rows).resize_keyboard()
}

/// /language command: show the language picker.
pub async fn language_command_handler(
    bot: Bot,
    msg: Message,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let lang = msg.from.as_ref().and_then(|u| u.language_code.as_deref());
    bot.send_message(msg.chat.id, i18n::t(MsgKey::LanguageChoose, lang))
        .reply_markup(language_keyboard())
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    Ok(())
}

/// A /language keyboard button was pressed: persist the override and confirm
/// in the newly selected language.
pub async fn language_button_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let text = match msg.text() {
        Some(text) => text,
        None => return Ok(()),
    };
    if text == BTN_AUTO_DETECT {
        let user_id = msg.chat.id.0;
        if let Err(e) = db_pool.clear_user_lang(user_id).await {
            log::error!("Failed to clear language for user {}: {}", user_id, e);
        }
        db_pool.log_funnel_event(user_id, "language_autodetect").await;
        let tg_lang = msg.from.as_ref().and_then(|u| u.language_code.as_deref());
        let lang = db_pool.get_effective_lang(user_id, tg_lang).await;
        bot.send_message(msg.chat.id, i18n::t(MsgKey::LanguageSet, Some(lang.as_str())))
            .reply_markup(crate::handlers::command::get_main_reply_keyboard())
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        return Ok(());
    }
    let code = match i18n::lang_code_for_button(text) {
        Some(code) => code,
        None => return Ok(()),
    };

    let user_id = msg.chat.id.0;
    if let Err(e) = db_pool.set_user_lang(user_id, code).await {
        log::error!("Failed to save language for user {}: {}", user_id, e);
    }
    db_pool.log_funnel_event(user_id, "language_set").await;

    bot.send_message(msg.chat.id, i18n::t(MsgKey::LanguageSet, Some(code)))
        .reply_markup(crate::handlers::command::get_main_reply_keyboard())
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    Ok(())
}

/// Settings → Language: the same picker the /language command shows,
/// auto-detect first, announced in the user's current effective language.
pub async fn language_menu_handler(
    bot: Bot,
    msg: Message,
    db_pool: Arc<DatabasePool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let user_id = msg.chat.id.0;
    let tg_lang = msg.from.as_ref().and_then(|u| u.language_code.as_deref());
    let lang = db_pool.get_effective_lang(user_id, tg_lang).await;
    bot.send_message(msg.chat.id, i18n::t(MsgKey::LanguageChoose, Some(lang.as_str())))
        .reply_markup(language_keyboard())
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    Ok(())
}
