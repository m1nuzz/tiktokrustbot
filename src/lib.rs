pub mod auto_update;
pub mod commands;
pub mod config;
pub mod database;
pub mod handlers;
pub mod mtproto_uploader;
pub mod peers;
pub mod telegram_bot_api_uploader;
pub mod utils;
pub mod web_server;
pub mod yt_dlp_interface;

pub use handlers::payments;
use teloxide::prelude::*;
use handlers::broadcast::BroadcastState;
use handlers::{
    receive_broadcast_message, add_premium_user_handler, start_broadcast,
    handle_broadcast_confirmation, admin_panel_text_handler, command_handler,
    settings_text_handler, format_text_handler, subscription_text_handler,
    back_text_handler, link_handler, BTN_BROADCAST,
    all_users_text_handler, stats_text_handler, top10_text_handler, premium_users_text_handler,
    daily_stats_text_handler, admin_ads_text_handler,
};
use handlers::ui::{BTN_ADMIN_PANEL, BTN_BACK, BTN_FORMAT, BTN_SETTINGS, BTN_SUBSCRIPTION};
use database::DatabasePool;
use mtproto_uploader::MTProtoUploader;
use yt_dlp_interface::YoutubeFetcher;
use utils::task_manager::TaskManager;
use std::sync::Arc;
use std::collections::HashSet;
use tokio::sync::Mutex;
use teloxide::dispatching::dialogue;
use teloxide::dptree;
use teloxide::types::CallbackQuery;
use teloxide::dispatching::DpHandlerDescription;
use commands::{AdminCommand, Command};

pub type MyDialogue = dialogue::Dialogue<
    BroadcastState,
    dialogue::InMemStorage<BroadcastState>,
>;

// For deduplication
lazy_static::lazy_static! {
    static ref PROCESSING: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
}

/// Build the main dispatcher handler tree for testing and production
pub fn build_handler() -> Handler<'static, Result<(), Box<dyn std::error::Error + Send + Sync>>, DpHandlerDescription> {
    dptree::entry()
        // Payment handlers must be outside dialogue::enter because PreCheckoutQuery has no ChatId
        .branch(Update::filter_pre_checkout_query().endpoint(handlers::payments::handle_pre_checkout))
        .branch(Update::filter_message().filter(|msg: Message| msg.successful_payment().is_some()).endpoint(handlers::payments::handle_successful_payment))
        .branch(
            dialogue::enter::<Update, dialogue::InMemStorage<BroadcastState>, BroadcastState, _>()
                .branch(
                    Update::filter_message()
                        .branch(dptree::case![BroadcastState::WaitingForMessage].endpoint(receive_broadcast_message))
                        .branch(dptree::case![BroadcastState::WaitingForAddPremiumUserId].endpoint(add_premium_user_handler))
                        .branch(dptree::case![BroadcastState::Idle]
                                .filter(|msg: Message| msg.text().map(|t| t == BTN_BROADCAST).unwrap_or(false))
                                .endpoint(start_broadcast))
                        .branch(dptree::case![BroadcastState::Idle]
                                .filter(|msg: Message| msg.text().map(|t| t == "➕ Add Premium User").unwrap_or(false))
                                .endpoint(|bot: Bot, dialogue: MyDialogue, msg: Message| async move {
                                    bot.send_message(msg.chat.id, "👤 Send the numeric Telegram ID to grant 30 days of Premium (or /cancel):").await?;
                                    dialogue.update(BroadcastState::WaitingForAddPremiumUserId).await?;
                                    Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                                }))
                )
                .branch(
                    Update::filter_callback_query()
                        .filter(|q: CallbackQuery| q.data.as_ref().map_or(false, |data| data == "broadcast_confirm" || data == "broadcast_cancel"))
                        .endpoint(|bot: Bot, dialogue: MyDialogue, q: CallbackQuery, db_pool: Arc<DatabasePool>| async move {
                            if let Ok(Some(BroadcastState::WaitingForConfirmation { message })) = dialogue.get().await {
                                handle_broadcast_confirmation(bot, dialogue, q, db_pool, message).await
                            } else {
                                Ok(())
                            }
                        })
                )
                .branch(
                    Update::filter_message()
                        .filter_command::<AdminCommand>()
                        .endpoint(|bot: Bot, msg: Message, cmd: AdminCommand, db_pool: Arc<DatabasePool>| async move {
                            if !handlers::admin::is_admin(&msg).await {
                                bot.send_message(msg.chat.id, "This command is for admins only.").await?;
                                return Ok(());
                            }

                            match cmd {
                                AdminCommand::AddChannel { id_name } => {
                                    let parts: Vec<&str> = id_name.splitn(2, ',').collect();
                                    if parts.len() == 2 {
                                        let id = parts[0].to_string();
                                        let name = parts[1].to_string();
                                        let db = db_pool.clone();
                                        let _ = db.execute_with_timeout(move |conn| {
                                            conn.execute("INSERT OR REPLACE INTO channels (channel_id, channel_name) VALUES (?1, ?2)", [id, name])
                                        }).await;
                                        bot.send_message(msg.chat.id, "✅ Channel added.").await?;
                                    }
                                }
                                AdminCommand::DelChannel { id } => {
                                    let db = db_pool.clone();
                                    let _ = db.execute_with_timeout(move |conn| {
                                        conn.execute("DELETE FROM channels WHERE channel_id = ?1", [id])
                                    }).await;
                                    bot.send_message(msg.chat.id, "✅ Channel deleted.").await?;
                                }
                                AdminCommand::ListChannels => {
                                    let res = db_pool.execute_with_timeout(|conn| {
                                        let mut stmt = conn.prepare("SELECT channel_id, channel_name FROM channels")?;
                                        let iter = stmt.query_map([], |row| Ok(format!("{} ({})", row.get::<_, String>(1)?, row.get::<_, String>(0)?)))?;
                                        Ok(iter.map(|r| r.unwrap()).collect::<Vec<_>>())
                                    }).await;
                                    if let Ok(list) = res {
                                        bot.send_message(msg.chat.id, format!("📋 Channels:\n{}", list.join("\n"))).await?;
                                    }
                                }
                                AdminCommand::ToggleSubscription => {
                                    let res = db_pool.execute_with_timeout(|conn| {
                                        let curr: String = conn.query_row("SELECT value FROM settings WHERE key = 'subscription_required'", [], |r| r.get(0))?;
                                        let next = if curr == "true" { "false" } else { "true" };
                                        conn.execute("UPDATE settings SET value = ?1 WHERE key = 'subscription_required'", [next])?;
                                        Ok(next == "true")
                                    }).await;
                                    if let Ok(now) = res {
                                        bot.send_message(msg.chat.id, format!("✅ Subscription: {}", if now { "ON" } else { "OFF" })).await?;
                                    }
                                }
                                AdminCommand::Fingerprint => {
                                    let exe_dir = std::env::current_exe()?.parent().unwrap().to_path_buf();
                                    let ytdlp = exe_dir.join("lib").join("yt-dlp").to_string_lossy().to_string();
                                    handlers::fingerprint::fingerprint_list_handler(bot, msg, &ytdlp).await?;
                                }
                                AdminCommand::FakePayment => {
                                    if let Some(user) = msg.from {
                                        let _ = db_pool.set_user_premium(user.id.0 as i64, 30).await;
                                        bot.send_message(msg.chat.id, "✅ [TEST] Premium activated!").await?;
                                    }
                                }
                                AdminCommand::ResetPremium => {
                                    if let Some(user) = msg.from {
                                        let user_id = user.id.0 as i64;
                                        let _ = db_pool.execute_with_timeout(move |conn| {
                                            conn.execute("UPDATE users SET premium_until = datetime('now', '-1 day') WHERE telegram_id = ?1", [user_id])
                                        }).await;
                                        bot.send_message(msg.chat.id, "🔄 [TEST] Premium status has been reset (expired).").await?;
                                    }
                                }
                            }
                            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                        })
                )
                .branch(Update::filter_message().filter(|msg: Message| msg.text().map_or(false, |t| t.starts_with("/setfingerprint-"))).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
                    let fp = msg.text().unwrap().trim_start_matches("/setfingerprint-").to_string();
                    let exe_dir = std::env::current_exe()?.parent().unwrap().to_path_buf();
                    let ytdlp = exe_dir.join("lib").join("yt-dlp").to_string_lossy().to_string();
                    handlers::fingerprint::set_fingerprint_handler(bot, msg, db_pool, fp, &ytdlp).await
                }))
                .branch(Update::filter_message().filter_command::<Command>().endpoint(command_handler))
                .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_SETTINGS)).endpoint(settings_text_handler))
                .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_FORMAT)).endpoint(format_text_handler))
                .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_ADMIN_PANEL)).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
                    admin_panel_text_handler(bot, msg, db_pool).await
                }))
                .branch(Update::filter_callback_query().filter(|q: CallbackQuery| q.data == Some("buy_premium".to_string())).endpoint(|bot: Bot, q: CallbackQuery, db_pool: Arc<DatabasePool>| async move {
                    let _ = bot.answer_callback_query(q.id).await;
                    handlers::payments::send_premium_invoice(bot, q.from.id.into(), db_pool, None).await
                }))
                .branch(Update::filter_message().filter(|msg: Message| msg.text().map_or(false, |t| t.starts_with(handlers::ui::BTN_TOGGLE_ADS))).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
                    let curr = db_pool.get_setting("ads_enabled").await.map(|v| v == "true").unwrap_or(true);
                    let _ = db_pool.set_setting("ads_enabled", if !curr { "true" } else { "false" }).await;
                    admin_panel_text_handler(bot, msg, db_pool).await
                }))
                .branch(Update::filter_message().filter(|msg: Message| msg.text().map_or(false, |t| t.starts_with("🔔 Admin Ads:"))).endpoint(admin_ads_text_handler))
                .branch(Update::filter_message().filter(|msg: Message| msg.text().map_or(false, |t| t.starts_with(handlers::ui::BTN_TOGGLE_SUCCESS_NOTIFS))).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
                    let curr = db_pool.get_setting("notify_success").await.map(|v| v == "true").unwrap_or(true);
                    let _ = db_pool.set_setting("notify_success", if !curr { "true" } else { "false" }).await;
                    admin_panel_text_handler(bot, msg, db_pool).await
                }))
                .branch(Update::filter_message().filter(|msg: Message| msg.text().map_or(false, |t| t.starts_with(handlers::ui::BTN_TOGGLE_FAIL_NOTIFS))).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
                    let curr = db_pool.get_setting("notify_fail").await.map(|v| v == "true").unwrap_or(true);
                    let _ = db_pool.set_setting("notify_fail", if !curr { "true" } else { "false" }).await;
                    admin_panel_text_handler(bot, msg, db_pool).await
                }))
                .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("📊 Stats")).endpoint(stats_text_handler))
                .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("📈 Daily Stats")).endpoint(daily_stats_text_handler))
                .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("🏆 Top 10")).endpoint(top10_text_handler))
                .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("👥 All users")).endpoint(all_users_text_handler))
                .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("💎 Premium Users")).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
                    premium_users_text_handler(bot, msg, db_pool).await
                }))
                .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_SUBSCRIPTION)).endpoint(subscription_text_handler))
                .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_BACK)).endpoint(back_text_handler))
                .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("h265")).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
                    let id = msg.chat.id.0;
                    let _ = db_pool.execute_with_timeout(move |c| c.execute("UPDATE users SET quality_preference = 'h265' WHERE telegram_id = ?1", [&id])).await;
                    db_pool.invalidate_user_quality_cache(id).await;
                    bot.send_message(msg.chat.id, "Quality: h265").reply_markup(handlers::command::get_main_reply_keyboard()).await?;
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
                }))
                .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("h264")).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
                    let id = msg.chat.id.0;
                    let _ = db_pool.execute_with_timeout(move |c| c.execute("UPDATE users SET quality_preference = 'h264' WHERE telegram_id = ?1", [&id])).await;
                    db_pool.invalidate_user_quality_cache(id).await;
                    bot.send_message(msg.chat.id, "Quality: h264").reply_markup(handlers::command::get_main_reply_keyboard()).await?;
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
                }))
                .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("audio")).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
                    let id = msg.chat.id.0;
                    let _ = db_pool.execute_with_timeout(move |c| c.execute("UPDATE users SET quality_preference = 'audio' WHERE telegram_id = ?1", [&id])).await;
                    db_pool.invalidate_user_quality_cache(id).await;
                    bot.send_message(msg.chat.id, "Quality: audio").reply_markup(handlers::command::get_main_reply_keyboard()).await?;
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
                }))
                .branch(Update::filter_message().filter(|msg: Message| msg.text().map(|t| !handlers::ui::is_system_button(t)).unwrap_or(false)).endpoint(|bot: Bot, msg: Message, fetcher: Arc<YoutubeFetcher>, mtproto_uploader: Arc<MTProtoUploader>, db_pool: Arc<DatabasePool>, task_manager: Arc<tokio::sync::Mutex<TaskManager>>, upload_semaphore: Arc<tokio::sync::Semaphore>| async move {
                    let key = format!("{}:{}:{}", msg.chat.id.0, msg.id.0, msg.text().unwrap_or(""));
                    {
                        let mut p = PROCESSING.lock().await;
                        if p.contains(&key) { return Ok(()); }
                        p.insert(key.clone());
                    }
                    tokio::spawn(async move {
                        let _ = link_handler(bot.clone(), msg.clone(), fetcher, mtproto_uploader, db_pool, task_manager, upload_semaphore).await;
                        PROCESSING.lock().await.remove(&key);
                    });
                    Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                }))
        )
}
