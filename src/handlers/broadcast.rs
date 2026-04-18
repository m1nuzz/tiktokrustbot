use teloxide::prelude::*;
use teloxide::dispatching::dialogue::{InMemStorage, Dialogue};
use teloxide::types::{ParseMode, ChatId, InlineKeyboardMarkup, InlineKeyboardButton};
use std::sync::Arc;
use crate::database::DatabasePool;
use crate::handlers::admin::is_admin;
use tokio::time::{sleep, Duration};

type MyDialogue = Dialogue<BroadcastState, InMemStorage<BroadcastState>>;
type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Default, Debug)]
pub enum BroadcastState {
    #[default]
    Idle,
    WaitingForMessage,
    WaitingForConfirmation { message: String },  // New state!
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

    bot.send_message(
        msg.chat.id,
        "📢 Send broadcast message (HTML supported).\n/cancel to abort."
    )
    .await
    .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

    dialogue.update(BroadcastState::WaitingForMessage)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    Ok(())
}

pub async fn receive_broadcast_message(
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
                InlineKeyboardButton::callback("✅ Send to all", "broadcast_confirm"),
                InlineKeyboardButton::callback("❌ Cancel", "broadcast_cancel"),
            ]
        ]);

        bot.send_message(msg.chat.id, "Send this message to all users?")
            .reply_markup(keyboard)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        dialogue.update(BroadcastState::WaitingForConfirmation {
            message: text.to_string(),
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
    db_pool: Arc<DatabasePool>,
    message: String,
) -> HandlerResult {
    if let Some(data) = &q.data {
        // Delete buttons
        if let Some(msg) = &q.message {
            let _ = bot.edit_message_reply_markup(msg.chat().id, msg.id()).await;
        }

        if data == "broadcast_cancel" {
            bot.answer_callback_query(q.id)
                .text("❌ Broadcast cancelled")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

            dialogue.exit()
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            return Ok(());
        }

        if data == "broadcast_confirm" {
            bot.answer_callback_query(q.id)
                .text("🚀 Starting broadcast...")
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

            if let Some(msg) = &q.message {
                bot.send_message(msg.chat().id, "🚀 Broadcasting...")
                    .await
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

                // Get users
                let users = db_pool.execute_with_timeout(|conn| {
                    let mut stmt = conn.prepare("SELECT telegram_id FROM users")?;
                    let users_iter = stmt.query_map([], |row| row.get::<_, i64>(0))?;
                    let mut users = Vec::new();
                    for user_result in users_iter {
                        users.push(user_result?);
                    }
                    Ok(users)
                }).await;

                match users {
                    Ok(users) => {
                        let total = users.len();
                        let mut sent = 0;
                        let mut failed = 0;

                        for (idx, user_id) in users.iter().enumerate() {
                            // Rate limit: 25 msg/sec
                            if idx > 0 && idx % 25 == 0 {
                                sleep(Duration::from_secs(1)).await;
                            }

                            match bot.send_message(ChatId(*user_id), &message)
                                .parse_mode(ParseMode::Html)
                                .await
                            {
                                Ok(_) => sent += 1,
                                Err(e) => {
                                    log::warn!("Failed to send to {}: {}", user_id, e);
                                    failed += 1;

                                    if let Some(secs) = extract_flood_wait(&e.to_string()) {
                                        log::info!("FLOOD_WAIT_{} - sleeping", secs);
                                        sleep(Duration::from_secs(secs.min(30))).await;
                                    }
                                }
                            }
                        }

                        let report = format!(
                            "✅ Broadcast completed!\n📊 Sent: {}/{}\n❌ Failed: {}",
                            sent, total, failed
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