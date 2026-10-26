use std::sync::Arc;

use teloxide::dispatching::dialogue::{Dialogue, InMemStorage};
use teloxide::prelude::*;
use teloxide::types::{ChatId, KeyboardButton, KeyboardMarkup};

use crate::BotCtx;
use crate::database::DatabasePool;
use crate::handlers::admin::{is_admin, is_first_admin, reload_admin_cache, resolve_telegram_ids};
use crate::handlers::broadcast::BroadcastState;
use crate::handlers::ui::{
    BTN_ADD_ADMIN, BTN_ADMIN_LIST, BTN_ADMIN_MANAGEMENT, BTN_ADMIN_PANEL, BTN_PREMIUM_GRANT_ALL,
    BTN_PREMIUM_GRANT_FIRST_ONLY, BTN_PREMIUM_GRANT_TOGGLE, BTN_REMOVE_ADMIN,
};

type MyDialogue = Dialogue<BroadcastState, InMemStorage<BroadcastState>>;
type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub const PREMIUM_GRANT_POLICY_KEY: &str = "premium_grant_first_admin_only";

pub async fn premium_grant_first_admin_only(db_pool: &DatabasePool) -> bool {
    db_pool
        .get_setting(PREMIUM_GRANT_POLICY_KEY)
        .await
        .map(|value| value != "false")
        .unwrap_or(true)
}

/// Non-first admins may grant premium only when the first admin explicitly
/// switched the policy to "All admins".
pub async fn can_grant_premium(db_pool: &DatabasePool, user_id: i64) -> bool {
    if !premium_grant_first_admin_only(db_pool).await {
        return true;
    }
    is_first_admin(db_pool, user_id).await
}

pub async fn can_access_premium_management(db_pool: &DatabasePool, user_id: i64) -> bool {
    if is_first_admin(db_pool, user_id).await {
        return true;
    }
    !premium_grant_first_admin_only(db_pool).await
}

fn user_id_from_msg(msg: &Message) -> i64 {
    msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0)
}

fn admin_management_keyboard(first_admin: bool, first_only: bool) -> KeyboardMarkup {
    let mut rows = Vec::new();
    if first_admin {
        rows.push(vec![
            KeyboardButton::new(BTN_ADD_ADMIN),
            KeyboardButton::new(BTN_REMOVE_ADMIN),
        ]);
    }
    rows.push(vec![KeyboardButton::new(BTN_ADMIN_LIST)]);
    if first_admin {
        let policy = if first_only {
            BTN_PREMIUM_GRANT_FIRST_ONLY
        } else {
            BTN_PREMIUM_GRANT_ALL
        };
        rows.push(vec![KeyboardButton::new(format!(
            "{}{}",
            BTN_PREMIUM_GRANT_TOGGLE, policy
        ))]);
    }
    rows.push(vec![KeyboardButton::new(BTN_ADMIN_PANEL)]);
    KeyboardMarkup::new(rows).resize_keyboard()
}

async fn send_admin_management(
    bot: &Bot,
    chat_id: ChatId,
    db_pool: &DatabasePool,
    first_admin: bool,
    first_only: bool,
) -> HandlerResult {
    let admin_count = db_pool
        .list_admins()
        .await
        .map(|ids| ids.len())
        .unwrap_or(0);
    let policy = if first_only {
        BTN_PREMIUM_GRANT_FIRST_ONLY
    } else {
        BTN_PREMIUM_GRANT_ALL
    };
    let text = format!(
        "{} \nAdmins: {}\nPremium grants: {}",
        BTN_ADMIN_MANAGEMENT, admin_count, policy
    );

    bot.send_message(chat_id, text)
        .reply_markup(admin_management_keyboard(first_admin, first_only))
        .await?;
    Ok(())
}

pub async fn admin_management_menu_handler(
    bot: Bot,
    msg: Message,
    _ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await?;
        return Ok(());
    }

    let user_id = user_id_from_msg(&msg);
    let first_admin = is_first_admin(&db_pool, user_id).await;
    let first_only = premium_grant_first_admin_only(&db_pool).await;
    send_admin_management(&bot, msg.chat.id, &db_pool, first_admin, first_only).await
}

pub async fn admin_list_handler(
    bot: Bot,
    msg: Message,
    _ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await?;
        return Ok(());
    }

    let admins = db_pool.list_admins().await?;
    let mut response = format!("📋 Admins — {}\n", admins.len());
    if admins.is_empty() {
        response.push_str("No admins found.");
    } else {
        for id in admins {
            response.push_str(&format!("{}\n", id));
        }
    }
    bot.send_message(msg.chat.id, response).await?;
    Ok(())
}

pub async fn add_admins_prompt_handler(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    _ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await?;
        return Ok(());
    }
    let user_id = user_id_from_msg(&msg);
    if !is_first_admin(&db_pool, user_id).await {
        bot.send_message(msg.chat.id, "❌ Only the first admin can manage admins.")
            .await?;
        return Ok(());
    }

    bot.send_message(
        msg.chat.id,
        "Send one or more Telegram IDs or @usernames separated by spaces or commas.\n/cancel to abort.",
    )
    .await?;
    dialogue.update(BroadcastState::WaitingForAddAdmins).await?;
    Ok(())
}

pub async fn remove_admins_prompt_handler(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    _ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await?;
        return Ok(());
    }
    let user_id = user_id_from_msg(&msg);
    if !is_first_admin(&db_pool, user_id).await {
        bot.send_message(msg.chat.id, "❌ Only the first admin can manage admins.")
            .await?;
        return Ok(());
    }

    bot.send_message(
        msg.chat.id,
        "Send one or more admin Telegram IDs or @usernames to remove, separated by spaces or commas.\n/cancel to abort.",
    )
    .await?;
    dialogue
        .update(BroadcastState::WaitingForRemoveAdmins)
        .await?;
    Ok(())
}

pub async fn add_admins_handler(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        return Ok(());
    }
    let user_id = user_id_from_msg(&msg);
    if !is_first_admin(&db_pool, user_id).await {
        bot.send_message(msg.chat.id, "❌ Only the first admin can manage admins.")
            .await?;
        dialogue.exit().await?;
        return Ok(());
    }

    let Some(text) = msg.text() else {
        return Ok(());
    };
    if text == "/cancel" {
        dialogue.exit().await?;
        return admin_management_menu_handler(bot, msg, ctx, db_pool).await;
    }
    if text == BTN_ADMIN_PANEL {
        dialogue.exit().await?;
        return crate::handlers::admin_panel::admin_panel_text_handler(bot, msg, ctx, db_pool)
            .await;
    }

    let ids = match resolve_telegram_ids(&bot, text).await {
        Ok(ids) => ids,
        Err(e) => {
            bot.send_message(msg.chat.id, format!("❌ {}\n\nSend again or /cancel.", e))
                .await?;
            return Ok(());
        }
    };

    let added = match db_pool.add_admins(&ids).await {
        Ok(added) => added,
        Err(e) => {
            log::error!("Failed to add admins: {}", e);
            bot.send_message(msg.chat.id, "❌ Database error.").await?;
            return Ok(());
        }
    };
    if let Err(e) = reload_admin_cache(&db_pool).await {
        log::error!("Failed to reload admin cache: {}", e);
    }

    let total = db_pool
        .list_admins()
        .await
        .map(|ids| ids.len())
        .unwrap_or(0);
    dialogue.exit().await?;
    if added == 0 {
        bot.send_message(msg.chat.id, "ℹ️ No new admins added.")
            .await?;
    } else {
        bot.send_message(
            msg.chat.id,
            format!("✅ Added {} admin(s). Total admins: {}.", added, total),
        )
        .await?;
    }
    admin_management_menu_handler(bot, msg, ctx, db_pool).await
}

pub async fn remove_admins_handler(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        return Ok(());
    }
    let user_id = user_id_from_msg(&msg);
    if !is_first_admin(&db_pool, user_id).await {
        bot.send_message(msg.chat.id, "❌ Only the first admin can manage admins.")
            .await?;
        dialogue.exit().await?;
        return Ok(());
    }

    let Some(text) = msg.text() else {
        return Ok(());
    };
    if text == "/cancel" {
        dialogue.exit().await?;
        return admin_management_menu_handler(bot, msg, ctx, db_pool).await;
    }
    if text == BTN_ADMIN_PANEL {
        dialogue.exit().await?;
        return crate::handlers::admin_panel::admin_panel_text_handler(bot, msg, ctx, db_pool)
            .await;
    }

    let ids = match resolve_telegram_ids(&bot, text).await {
        Ok(ids) => ids,
        Err(e) => {
            bot.send_message(msg.chat.id, format!("❌ {}\n\nSend again or /cancel.", e))
                .await?;
            return Ok(());
        }
    };

    let removed = match db_pool.remove_admins(&ids).await {
        Ok(removed) => removed,
        Err(e) => {
            let message = e.to_string();
            if message.contains("cannot remove last admin") {
                bot.send_message(msg.chat.id, "❌ Cannot remove the last admin.")
                    .await?;
            } else {
                log::error!("Failed to remove admins: {}", e);
                bot.send_message(msg.chat.id, "❌ Database error.").await?;
            }
            dialogue.exit().await?;
            return admin_management_menu_handler(bot, msg, ctx, db_pool).await;
        }
    };
    if let Err(e) = reload_admin_cache(&db_pool).await {
        log::error!("Failed to reload admin cache: {}", e);
    }

    dialogue.exit().await?;
    let removed_self = removed > 0 && ids.contains(&user_id);
    if removed == 0 {
        bot.send_message(msg.chat.id, "ℹ️ No admins removed (not found).")
            .await?;
    } else {
        bot.send_message(msg.chat.id, format!("✅ Removed {} admin(s).", removed))
            .await?;
        if removed_self {
            bot.send_message(
                msg.chat.id,
                "⚠️ You removed yourself. Admin access ends now.",
            )
            .reply_markup(crate::handlers::command::get_main_reply_keyboard())
            .await?;
            return Ok(());
        }
    }
    admin_management_menu_handler(bot, msg, ctx, db_pool).await
}

pub async fn toggle_premium_grant_policy_handler(
    bot: Bot,
    msg: Message,
    _ctx: BotCtx,
    db_pool: Arc<DatabasePool>,
) -> HandlerResult {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "This option is for admins only.")
            .await?;
        return Ok(());
    }
    let user_id = user_id_from_msg(&msg);
    if !is_first_admin(&db_pool, user_id).await {
        bot.send_message(
            msg.chat.id,
            "❌ Only the first admin can change this setting.",
        )
        .await?;
        return Ok(());
    }

    let current = premium_grant_first_admin_only(&db_pool).await;
    let next = !current;
    db_pool
        .set_setting(
            PREMIUM_GRANT_POLICY_KEY,
            if next { "true" } else { "false" },
        )
        .await?;

    let policy = if next {
        BTN_PREMIUM_GRANT_FIRST_ONLY
    } else {
        BTN_PREMIUM_GRANT_ALL
    };
    bot.send_message(msg.chat.id, format!("✅ Premium grants: {}", policy))
        .await?;
    send_admin_management(&bot, msg.chat.id, &db_pool, true, next).await
}
