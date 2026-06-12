use anyhow::Error;
use std::collections::HashSet;
use std::sync::Arc;
use std::env;
use teloxide::prelude::*;
use tokio::sync::Mutex;

use crate::commands::AdminCommand;
use crate::commands::Command;
use crate::database::DatabasePool;
use crate::handlers::ui::{BTN_ADMIN_PANEL, BTN_BACK, BTN_FORMAT, BTN_SETTINGS, BTN_SUBSCRIPTION};
use crate::handlers::{
    BTN_BROADCAST, BroadcastState, admin_panel_text_handler, all_users_text_handler,
    back_text_handler, command_handler, format_text_handler, handle_broadcast_confirmation,
    link_handler, receive_broadcast_message, settings_text_handler, start_broadcast,
    stats_text_handler, subscription_text_handler, top10_text_handler, premium_users_text_handler,
    add_premium_user_handler,
};
use crate::mtproto_uploader::MTProtoUploader;
use crate::utils::task_manager::TaskManager;
use crate::yt_dlp_interface::{YoutubeFetcher, ensure_binaries, is_executable_present};
use teloxide::dispatching::dialogue;
use teloxide::dptree;
use teloxide::types::CallbackQuery;
type MyDialogue = teloxide::dispatching::dialogue::Dialogue<
    BroadcastState,
    teloxide::dispatching::dialogue::InMemStorage<BroadcastState>,
>;

// For deduplication
lazy_static::lazy_static! {
    static ref PROCESSING: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
}

#[cfg(not(target_os = "android"))]
#[cfg(target_os = "android")]
use robius_directories::ProjectDirs;

mod auto_update;
mod commands;
mod config;
mod database;
mod handlers;
pub mod mtproto_uploader;
pub mod peers;
mod telegram_bot_api_uploader;
mod utils;
mod web_server;
mod yt_dlp_interface;

fn build_bot(is_test_mode: bool, client: reqwest::Client) -> (Bot, String) {
    let token_var = if is_test_mode { "TELEGRAM_TEST_TOKEN" } else { "TELOXIDE_TOKEN" };
    
    let raw_token = std::env::var(token_var)
        .unwrap_or_else(|_| {
            if is_test_mode {
                std::env::var("TELOXIDE_TOKEN").expect("Neither TELEGRAM_TEST_TOKEN nor TELOXIDE_TOKEN is set")
            } else {
                panic!("TELOXIDE_TOKEN must be set")
            }
        })
        .trim()
        .to_string();

    let bot_token = if is_test_mode {
        log::warn!("⚠️  RUNNING IN TEST MODE (Telegram Test Server)");
        format!("{}/test", raw_token)
    } else {
        raw_token.clone()
    };

    (Bot::with_client(bot_token, client), raw_token)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // 1. Initialize logger IMMEDIATELY
    if std::env::var("RUST_LOG").is_err() {
        unsafe { std::env::set_var("RUST_LOG", "info"); }
    }
    pretty_env_logger::init();
    log::info!("Starting TikTok downloader bot...");
    let start_time = std::time::Instant::now();

    // 2. Load environment
    if let Err(e) = crate::config::load_environment() {
        eprintln!("Warning: Failed to load environment: {}", e);
    }

    let exe_dir = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Failed to get parent directory of executable"))?
        .to_path_buf();

    let libraries_dir = exe_dir.join("lib");
    let output_dir = exe_dir.join("downloads");

    if let Err(e) = ensure_binaries(&libraries_dir, &output_dir).await {
        log::error!("Failed to ensure binaries: {}", e);
        return Err(e.into());
    }

    let yt_dlp_path = libraries_dir.join(if cfg!(target_os = "windows") { "yt-dlp.exe" } else { "yt-dlp" });
    let ffmpeg_dir = libraries_dir.join("ffmpeg");
    let ffmpeg_path = ffmpeg_dir.join(if cfg!(target_os = "windows") { "ffmpeg.exe" } else { "ffmpeg" });
    let ffprobe_path = ffmpeg_dir.join(if cfg!(target_os = "windows") { "ffprobe.exe" } else { "ffprobe" });

    if !is_executable_present(&yt_dlp_path) {
        return Err(anyhow::Error::msg("yt-dlp not available"));
    }

    let auto_updater = Arc::new(auto_update::AutoUpdater::new(libraries_dir.clone(), 30));
    let _ = auto_updater.check_for_updates().await;

    let updater_clone = Arc::clone(&auto_updater);
    tokio::spawn(async move {
        let _ = updater_clone.start_periodic_checks().await;
    });

    if let Err(e) = database::init_database() {
        log::error!("Failed to initialize the database: {}", e);
        return Err(e.into());
    }

    let fetcher = Arc::new(YoutubeFetcher::new(yt_dlp_path, output_dir.clone(), ffmpeg_dir.clone())?);

    // --- Bot Configuration ---
    let reqwest_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .connect_timeout(std::time::Duration::from_secs(10))
        .tcp_nodelay(true)
        .build()
        .expect("Failed to create HTTP client");

    let is_test_mode = env::var("TEST_MODE")
        .unwrap_or_else(|_| "false".to_string())
        .to_lowercase() == "true";

    let (bot, raw_token) = build_bot(is_test_mode, reqwest_client.clone());

    // Diagnostic check
    match bot.get_me().await {
        Ok(me) => log::info!("✅ Successfully connected to Telegram as @{}", me.user.username.unwrap_or_default()),
        Err(e) => {
            log::error!("❌ Auth Error: {}. Please check your token and TEST_MODE setting.", e);
            return Err(e.into());
        }
    }

    let mtproto_uploader = match MTProtoUploader::new(&raw_token, ffprobe_path, ffmpeg_path).await {
        Ok(uploader) => Arc::new(uploader),
        Err(e) => return Err(anyhow::anyhow!("{}", e)),
    };

    let db_pool = Arc::new(DatabasePool::new(crate::database::get_database_path(), 3));

    // Sync settings from .env to database
    if let Ok(sub_req) = env::var("SUBSCRIPTION_REQUIRED") {
        let val = if sub_req.to_lowercase() == "true" { "true" } else { "false" };
        let _ = db_pool.set_setting("subscription_required", val).await;
        log::info!("Synced subscription_required from .env: {}", val);
    }
    if let Ok(ads_en) = env::var("ADS_ENABLED") {
        let val = if ads_en.to_lowercase() == "true" { "true" } else { "false" };
        let _ = db_pool.set_setting("ads_enabled", val).await;
        log::info!("Synced ads_enabled from .env: {}", val);
    }

    let task_manager = Arc::new(tokio::sync::Mutex::new(TaskManager::new(2)));
    let upload_semaphore = Arc::new(tokio::sync::Semaphore::new(2));

    let web_state = crate::web_server::AppState {
        db: db_pool.clone(),
        bot: bot.clone(),
        fetcher: fetcher.clone(),
        mtproto_uploader: mtproto_uploader.clone(),
        task_manager: task_manager.clone(),
        upload_semaphore: upload_semaphore.clone(),
    };
    
    let web_port = env::var("WEB_SERVER_PORT").unwrap_or_else(|_| "8088".to_string()).parse().unwrap_or(8088);
    
    tokio::spawn(async move {
        crate::web_server::start_web_server(web_state, web_port).await;
    });

    let handler = dialogue::enter::<Update, dialogue::InMemStorage<BroadcastState>, BroadcastState, _>()
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
                    if !crate::handlers::admin::is_admin(&msg).await {
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
                            crate::handlers::fingerprint::fingerprint_list_handler(bot, msg, &ytdlp).await?;
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
            crate::handlers::fingerprint::set_fingerprint_handler(bot, msg, db_pool, fp, &ytdlp).await
        }))
        .branch(Update::filter_message().filter_command::<Command>().endpoint(command_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_SETTINGS)).endpoint(settings_text_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_FORMAT)).endpoint(format_text_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_ADMIN_PANEL)).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
            admin_panel_text_handler(bot, msg, db_pool).await
        }))
        .branch(Update::filter_callback_query().filter(|q: CallbackQuery| q.data == Some("buy_premium".to_string())).endpoint(|bot: Bot, q: CallbackQuery| async move {
            let _ = bot.answer_callback_query(q.id).await;
            crate::handlers::payments::send_premium_invoice(bot, q.from.id.into()).await
        }))
        .branch(Update::filter_pre_checkout_query().endpoint(crate::handlers::payments::handle_pre_checkout))
        .branch(Update::filter_message().filter(|msg: Message| msg.successful_payment().is_some()).endpoint(crate::handlers::payments::handle_successful_payment))
        .branch(Update::filter_message().filter(|msg: Message| msg.text().map_or(false, |t| t.starts_with(crate::handlers::ui::BTN_TOGGLE_ADS))).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
            let curr = db_pool.get_setting("ads_enabled").await.map(|v| v == "true").unwrap_or(true);
            let _ = db_pool.set_setting("ads_enabled", if !curr { "true" } else { "false" }).await;
            admin_panel_text_handler(bot, msg, db_pool).await
        }))
        .branch(Update::filter_message().filter(|msg: Message| msg.text().map_or(false, |t| t.starts_with(crate::handlers::ui::BTN_TOGGLE_SUCCESS_NOTIFS))).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
            let curr = db_pool.get_setting("notify_success").await.map(|v| v == "true").unwrap_or(true);
            let _ = db_pool.set_setting("notify_success", if !curr { "true" } else { "false" }).await;
            admin_panel_text_handler(bot, msg, db_pool).await
        }))
        .branch(Update::filter_message().filter(|msg: Message| msg.text().map_or(false, |t| t.starts_with(crate::handlers::ui::BTN_TOGGLE_FAIL_NOTIFS))).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
            let curr = db_pool.get_setting("notify_fail").await.map(|v| v == "true").unwrap_or(true);
            let _ = db_pool.set_setting("notify_fail", if !curr { "true" } else { "false" }).await;
            admin_panel_text_handler(bot, msg, db_pool).await
        }))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("Stats")).endpoint(stats_text_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("Top 10")).endpoint(top10_text_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("All users")).endpoint(all_users_text_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("💎 Premium Users")).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
            premium_users_text_handler(bot, msg, db_pool).await
        }))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_SUBSCRIPTION)).endpoint(subscription_text_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_BACK)).endpoint(back_text_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("h265")).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
            let id = msg.chat.id.0;
            let _ = db_pool.execute_with_timeout(move |c| c.execute("UPDATE users SET quality_preference = 'h265' WHERE telegram_id = ?1", [&id])).await;
            db_pool.invalidate_user_quality_cache(id).await;
            bot.send_message(msg.chat.id, "Quality: h265").reply_markup(crate::handlers::command::get_main_reply_keyboard()).await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("h264")).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
            let id = msg.chat.id.0;
            let _ = db_pool.execute_with_timeout(move |c| c.execute("UPDATE users SET quality_preference = 'h264' WHERE telegram_id = ?1", [&id])).await;
            db_pool.invalidate_user_quality_cache(id).await;
            bot.send_message(msg.chat.id, "Quality: h264").reply_markup(crate::handlers::command::get_main_reply_keyboard()).await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("audio")).endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
            let id = msg.chat.id.0;
            let _ = db_pool.execute_with_timeout(move |c| c.execute("UPDATE users SET quality_preference = 'audio' WHERE telegram_id = ?1", [&id])).await;
            db_pool.invalidate_user_quality_cache(id).await;
            bot.send_message(msg.chat.id, "Quality: audio").reply_markup(crate::handlers::command::get_main_reply_keyboard()).await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }))
        .branch(Update::filter_message().filter(|msg: Message| msg.text().map(|t| !crate::handlers::ui::is_system_button(t)).unwrap_or(false)).endpoint(|bot: Bot, msg: Message, fetcher: Arc<YoutubeFetcher>, mtproto_uploader: Arc<MTProtoUploader>, db_pool: Arc<DatabasePool>, task_manager: Arc<tokio::sync::Mutex<TaskManager>>, upload_semaphore: Arc<tokio::sync::Semaphore>| async move {
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
        }));

    log::info!("Bot initialized in {:.2?}", start_time.elapsed());
    let mut dispatcher = Dispatcher::builder(bot, handler).dependencies(dptree::deps![dialogue::InMemStorage::<BroadcastState>::new(), fetcher, mtproto_uploader, db_pool, task_manager.clone(), upload_semaphore]).enable_ctrlc_handler().build();
    tokio::select! {
        _ = dispatcher.dispatch() => {},
        _ = tokio::signal::ctrl_c() => log::info!("Received Ctrl+C, shutting down..."),
    }
    let mut tm = task_manager.lock().await;
    tm.shutdown().await;
    log::info!("Bot shutdown complete");
    Ok(())
}
