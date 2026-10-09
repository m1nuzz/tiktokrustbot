use teloxide::prelude::*;
use teloxide::dispatching::dialogue::{InMemStorage, Dialogue};
use teloxide::types::{ParseMode, ChatId, InlineKeyboardMarkup, InlineKeyboardButton};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use crate::database::DatabasePool;
use crate::handlers::admin::{is_admin, is_admin_id};
use tokio::sync::Semaphore;
use tokio::time::{sleep, Duration};

type MyDialogue = Dialogue<BroadcastState, InMemStorage<BroadcastState>>;
type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Callback data for the broadcast scope picker. Short on purpose: Telegram
/// caps callback data at 64 bytes.
pub const BROADCAST_SCOPE_THIS: &str = "broadcast_scope_this";
pub const BROADCAST_SCOPE_ALL: &str = "broadcast_scope_all";
pub const BROADCAST_CONFIRM: &str = "broadcast_confirm";
pub const BROADCAST_CANCEL: &str = "broadcast_cancel";

/// Who receives the broadcast: only the bot where the admin pressed the
/// button, or one message per human across all connected bots (sent from the
/// bot of their most recent activity, so nobody gets it twice).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BroadcastScope {
    ThisBot,
    All,
}

impl BroadcastScope {
    fn confirm_label(self) -> &'static str {
        match self {
            BroadcastScope::ThisBot => "✅ Send to this bot",
            BroadcastScope::All => "✅ Send to all bots",
        }
    }

    fn report_label(self) -> &'static str {
        match self {
            BroadcastScope::ThisBot => "this bot",
            BroadcastScope::All => "all bots",
        }
    }
}

#[derive(Clone, Default, Debug)]
pub enum BroadcastState {
    #[default]
    Idle,
    WaitingForScope,
    WaitingForMessage { scope: BroadcastScope },
    WaitingForConfirmation { message: String, scope: BroadcastScope },
    WaitingForAddPremiumUserId,
    WaitingForSetPrice,
}

/// Target send rate: 20-22 msg/s stays under the ~30/s Bot API limit with
/// headroom for young bots with lower implicit limits.
pub const BROADCAST_TARGET_PER_SEC: u32 = 22;
/// Max concurrent send tasks: bounds memory while the pacer bounds rate.
pub const BROADCAST_MAX_CONCURRENT: usize = 8;

/// Milliseconds between task starts for a target sends-per-second rate.
/// Pure so tests pin the arithmetic: 22/s -> 45ms.
pub fn broadcast_pace_interval_ms(per_sec: u32) -> u64 {
    1000 / per_sec.max(1) as u64
}

pub fn broadcast_now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Shared flood-wait flag: any worker that hits 429/RetryAfter parks every
/// other worker until the wait lapses. Stores unix seconds.
pub fn broadcast_pause_set(pause_until: &AtomicU64, wait_secs: u64) {
    let until = broadcast_now_unix().saturating_add(wait_secs).saturating_add(1);
    // Only move the deadline forward, never backward.
    let _ = pause_until.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
        if until > cur { Some(until) } else { None }
    });
}

pub fn broadcast_pause_remaining(pause_until: &AtomicU64) -> u64 {
    pause_until
        .load(Ordering::Relaxed)
        .saturating_sub(broadcast_now_unix())
}

fn scope_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("📍 Only this bot", BROADCAST_SCOPE_THIS),
        InlineKeyboardButton::callback("🌐 All connected bots", BROADCAST_SCOPE_ALL),
    ]])
}

pub async fn start_broadcast(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
) -> HandlerResult {
    if !is_admin(&msg).await {
        bot.send_message(msg.chat.id, "⛔ Admins only.")
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        return Ok(());
    }

    bot.send_message(msg.chat.id, "📢 Who gets this broadcast?")
        .reply_markup(scope_keyboard())
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

    dialogue.update(BroadcastState::WaitingForScope)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    Ok(())
}

/// Text arriving while the scope picker is open: /cancel aborts, anything
/// else re-shows the picker (there is no message yet to preview).
pub async fn handle_scope_text(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
) -> HandlerResult {
    if let Some(text) = msg.text() {
        if text == "/cancel" {
            bot.send_message(msg.chat.id, "❌ Cancelled.")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            dialogue.exit()
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            return Ok(());
        }
    }

    bot.send_message(msg.chat.id, "📢 Pick who gets the broadcast first:")
        .reply_markup(scope_keyboard())
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    Ok(())
}

pub async fn handle_scope_selection(
    bot: Bot,
    dialogue: MyDialogue,
    q: CallbackQuery,
) -> HandlerResult {
    let admin = q.from.id.0 as i64;
    if !is_admin_id(admin) {
        bot.answer_callback_query(q.id)
            .text("⛔ Admins only.")
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        return Ok(());
    }

    let scope = match q.data.as_deref() {
        Some(BROADCAST_SCOPE_THIS) => BroadcastScope::ThisBot,
        Some(BROADCAST_SCOPE_ALL) => BroadcastScope::All,
        _ => return Ok(()),
    };

    // Stale buttons (picker already closed) must not resurrect a flow.
    match dialogue.get().await {
        Ok(Some(BroadcastState::WaitingForScope)) => {}
        _ => {
            bot.answer_callback_query(q.id)
                .text("Press 📢 Broadcast again to start over.")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            return Ok(());
        }
    }

    if let Some(msg) = &q.message {
        let _ = bot.edit_message_reply_markup(msg.chat().id, msg.id()).await;
    }
    bot.answer_callback_query(q.id.clone())
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

    let chat_id = q.message.as_ref().map(|m| m.chat().id);
    if let Some(chat_id) = chat_id {
        bot.send_message(
            chat_id,
            "📢 Send broadcast message (HTML supported).\n/cancel to abort.",
        )
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    }

    dialogue.update(BroadcastState::WaitingForMessage { scope })
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    Ok(())
}

pub async fn receive_broadcast_message(
    bot: Bot,
    dialogue: MyDialogue,
    msg: Message,
    scope: BroadcastScope,
) -> HandlerResult {
    if let Some(text) = msg.text() {
        if text == "/cancel" {
            bot.send_message(msg.chat.id, "❌ Cancelled.")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            dialogue.exit()
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            return Ok(());
        }

        // Show preview to admin
        bot.send_message(msg.chat.id, "📝 Preview:")
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        bot.send_message(msg.chat.id, text)
            .parse_mode(ParseMode::Html)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        // Confirmation buttons
        let keyboard = InlineKeyboardMarkup::new(vec![
            vec![
                InlineKeyboardButton::callback(scope.confirm_label(), BROADCAST_CONFIRM),
                InlineKeyboardButton::callback("❌ Cancel", BROADCAST_CANCEL),
            ]
        ]);

        bot.send_message(msg.chat.id, "Send this message?")
            .reply_markup(keyboard)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        dialogue.update(BroadcastState::WaitingForConfirmation {
            message: text.to_string(),
            scope,
        })
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    }

    Ok(())
}

pub async fn handle_broadcast_confirmation(
    bot: Bot,
    dialogue: MyDialogue,
    q: CallbackQuery,
    ctx: crate::BotCtx,
    db_pool: Arc<DatabasePool>,
    bots: Arc<HashMap<String, Bot>>,
    message: String,
    scope: BroadcastScope,
) -> HandlerResult {
    if let Some(data) = &q.data {
        // Delete buttons
        if let Some(msg) = &q.message {
            let _ = bot.edit_message_reply_markup(msg.chat().id, msg.id()).await;
        }

        if data == BROADCAST_CANCEL {
            bot.answer_callback_query(q.id)
                .text("❌ Broadcast cancelled")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

            dialogue.exit()
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            return Ok(());
        }

        if data == BROADCAST_CONFIRM {
            bot.answer_callback_query(q.id)
                .text("🚀 Starting broadcast...")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

            if let Some(msg) = &q.message {
                bot.send_message(msg.chat().id, "🚀 Broadcasting...")
                    .await
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

                // Recipients as (telegram_id, bot_id): own bot only, or one
                // row per human across all bots from most recent activity.
                let recipients = match scope {
                    BroadcastScope::ThisBot => {
                        db_pool
                            .get_broadcast_recipients_this_bot(ctx.bot_id.as_str())
                            .await
                    }
                    BroadcastScope::All => db_pool.get_broadcast_recipients_all().await,
                };

                match recipients {
                    Ok(recipients) => {
                        let total = recipients.len();
                        let sent = Arc::new(AtomicUsize::new(0));
                        let failed = Arc::new(AtomicUsize::new(0));
                        let pause_until = Arc::new(AtomicU64::new(0));
                        let semaphore = Arc::new(Semaphore::new(BROADCAST_MAX_CONCURRENT));
                        let pace_ms = broadcast_pace_interval_ms(BROADCAST_TARGET_PER_SEC);
                        let mut handles = Vec::with_capacity(total);

                        for (user_id, bot_id) in recipients {
                            // A global 429 parks every worker: wait it out
                            // before starting more tasks.
                            while broadcast_pause_remaining(&pause_until) > 0 {
                                sleep(Duration::from_secs(1)).await;
                            }

                            let Some(send_bot) = bots.get(&bot_id).cloned() else {
                                log::warn!(
                                    "Broadcast: no connected bot for bot_id {} (user {}), skipping",
                                    bot_id, user_id
                                );
                                failed.fetch_add(1, Ordering::Relaxed);
                                continue;
                            };

                            let permit = semaphore.clone().acquire_owned().await
                                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
                            let text = message.clone();
                            let sent_c = sent.clone();
                            let failed_c = failed.clone();
                            let pause_c = pause_until.clone();
                            handles.push(tokio::spawn(async move {
                                let _permit = permit;
                                while broadcast_pause_remaining(&pause_c) > 0 {
                                    sleep(Duration::from_secs(1)).await;
                                }
                                match send_bot.send_message(ChatId(user_id), &text)
                                    .parse_mode(ParseMode::Html)
                                    .await
                                {
                                    Ok(_) => {
                                        sent_c.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(e) => {
                                        log::warn!("Failed to send to {}: {}", user_id, e);
                                        failed_c.fetch_add(1, Ordering::Relaxed);
                                        // Typed match first (teloxide 429):
                                        // string fallback keeps MTProto-style
                                        // texts working if the shape changes.
                                        match &e {
                                            teloxide_core::errors::RequestError::RetryAfter(d) => {
                                                let secs = d.as_secs().max(1);
                                                log::info!("429 RetryAfter({}s) - pausing all senders", secs);
                                                broadcast_pause_set(&pause_c, secs.min(300));
                                            }
                                            _ => {
                                                if let Some(secs) = extract_flood_wait(&e.to_string()) {
                                                    log::info!("FLOOD_WAIT_{} - pausing all senders", secs);
                                                    broadcast_pause_set(&pause_c, secs.min(300));
                                                }
                                            }
                                        }
                                    }
                                }
                            }));
                            // Pacer: at most ~22 task starts per second.
                            sleep(Duration::from_millis(pace_ms)).await;
                        }

                        for h in handles {
                            let _ = h.await;
                        }

                        let report = format!(
                            "✅ Broadcast to {} completed!\n📊 Sent: {}/{}\n❌ Failed: {}",
                            scope.report_label(),
                            sent.load(Ordering::Relaxed),
                            total,
                            failed.load(Ordering::Relaxed),
                        );
                        bot.send_message(msg.chat().id, report)
                            .await
                            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
                    }
                    Err(e) => {
                        log::error!("DB error: {}", e);
                        bot.send_message(msg.chat().id, "❌ Database error.")
                            .await
                            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
                    }
                }
            }

            dialogue.exit()
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
    }

    Ok(())
}

fn extract_flood_wait(error_str: &str) -> Option<u64> {
    use regex::Regex;
    let re = Regex::new(r"FLOOD_WAIT_(\d+)").unwrap();
    re.captures(error_str)
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse().ok())
}