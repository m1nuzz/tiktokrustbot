use anyhow::Error;
use std::collections::HashSet;
use std::sync::Arc;
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
    stats_text_handler, subscription_text_handler, top10_text_handler,
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
mod yt_dlp_interface;

#[tokio::main]
async fn main() -> Result<(), Error> {
    // --- Logging Setup ---
    use log::LevelFilter;
    use std::env;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::sync::Mutex as StdMutex; // Renamed to avoid conflict with tokio::sync::Mutex

    // 1. Get console log level from env
    let console_level_str = env::var("CONSOLE_LOG_LEVEL").unwrap_or_else(|_| "INFO".to_string());
    let console_level = match console_level_str.to_uppercase().as_str() {
        "ERROR" => LevelFilter::Error,
        _ => LevelFilter::Info, // Default to Info
    };

    // 2. Get file log level from env
    let file_level_str = env::var("FILE_LOG_LEVEL").unwrap_or_else(|_| "OFF".to_string());
    let file_level_config = match file_level_str.to_uppercase().as_str() {
        "ERROR" => Some(LevelFilter::Error),
        "ALL" | "INFO" => Some(LevelFilter::Info),
        _ => None, // OFF
    };

    // 3. Determine the most verbose level needed overall for the logger to process
    let max_level = std::cmp::max(console_level, file_level_config.unwrap_or(LevelFilter::Off));

    // 4. Setup file handle if needed
    let log_file = if file_level_config.is_some() {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open("bot_errors.log")?;
        Some(Arc::new(StdMutex::new(file))) // Use StdMutex here
    } else {
        None
    };

    // 5. Build the logger
    let mut builder = pretty_env_logger::formatted_builder();
    builder
        .filter(None, max_level) // Set logger to the most verbose level required
        .format(move |buf, record| {
            let formatted_record = format!(
                "{} [{}] {}: {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                record.target(),
                record.args()
            );

            // Write to console if level is sufficient
            if record.level() <= console_level {
                writeln!(buf, "{}", formatted_record)?;
            }

            // Write to file if level is sufficient
            if let Some(file_level) = file_level_config {
                if record.level() <= file_level {
                    if let Some(file_handle) = &log_file {
                        if let Ok(mut guard) = file_handle.lock() {
                            let _ = writeln!(guard, "{}", formatted_record);
                        }
                    }
                }
            }
            Ok(())
        })
        .init();

    log::info!("Starting TikTok downloader bot...");
    let start_time = std::time::Instant::now();

    if let Err(e) = crate::config::load_environment() {
        log::error!("Failed to load environment: {}", e);
        return Err(e.into());
    }

    let exe_dir = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Failed to get parent directory of executable"))?
        .to_path_buf();
    log::info!("Executable directory: {:?}", exe_dir);

    // Dynamic directory for libraries (yt-dlp and ffmpeg)
    let libraries_dir = exe_dir.join("lib");

    // Dynamic directory for output
    let output_dir = exe_dir.join("downloads");

    // Ensure required binaries are present before starting the async runtime
    if let Err(e) = ensure_binaries(&libraries_dir, &output_dir).await {
        log::error!("Failed to ensure binaries: {}", e);
        return Err(e.into());
    }

    log::info!("Libraries directory: {:?}", libraries_dir.canonicalize()?);
    log::info!(
        "Contents of libraries directory: {:?}",
        std::fs::read_dir(&libraries_dir)?
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>()
    );

    let yt_dlp_path = libraries_dir.join(if cfg!(target_os = "windows") {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    });
    let ffmpeg_dir = libraries_dir.join("ffmpeg");
    let ffmpeg_path = ffmpeg_dir.join(if cfg!(target_os = "windows") {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    });
    let ffprobe_path = ffmpeg_dir.join(if cfg!(target_os = "windows") {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });

    if !is_executable_present(&yt_dlp_path) {
        log::error!(
            "yt-dlp not found at {:?} after attempted download",
            yt_dlp_path
        );
        return Err(anyhow::Error::msg("yt-dlp not available"));
    } else {
        log::info!("yt-dlp found at {:?}", yt_dlp_path);
    }

    if !is_executable_present(&ffmpeg_path) {
        log::error!(
            "ffmpeg not found at {:?} after attempted download",
            ffmpeg_path
        );
        return Err(anyhow::Error::msg("ffmpeg not available"));
    }

    if !is_executable_present(&ffprobe_path) {
        log::error!(
            "ffprobe not found at {:?} after attempted download",
            ffprobe_path
        );
        return Err(anyhow::Error::msg("ffprobe not available"));
    }

    // Configure auto-update AFTER ensure_binaries
    let auto_updater = Arc::new(auto_update::AutoUpdater::new(libraries_dir.clone(), 30)); // Check every 30 minutes

    // Initial check for updates
    if let Err(e) = auto_updater.check_for_updates().await {
        log::warn!("Initial update check failed: {}", e);
    }

    // Run periodic check in the background
    let updater_clone = Arc::clone(&auto_updater);
    tokio::spawn(async move {
        if let Err(e) = updater_clone.start_periodic_checks().await {
            log::error!("Periodic update checker failed: {}", e);
        }
    });

    log::info!("Auto-update functionality initialized");

    if let Err(e) = database::init_database() {
        log::error!("Failed to initialize the database: {}", e);
        return Err(e.into());
    }
    log::info!("Database initialized successfully.");

    let fetcher = Arc::new(YoutubeFetcher::new(
        yt_dlp_path,
        output_dir.clone(),
        ffmpeg_dir.clone(),
    )?);
    let bot_token = env::var("TELOXIDE_TOKEN").expect("TELOXIDE_TOKEN must be set");
    let mtproto_uploader =
        match MTProtoUploader::new(&bot_token, ffprobe_path.clone(), ffmpeg_path.clone()).await {
            Ok(uploader) => Arc::new(uploader),
            Err(e) => {
                log::error!("Failed to create MTProtoUploader: {}", e);
                return Err(anyhow::anyhow!("{}", e));
            }
        };

    // Create database pool and task manager
    let db_pool = Arc::new(DatabasePool::new(
        crate::database::get_database_path(),
        3, // Maximum 3 simultaneous database connections
    ));

    let task_manager = Arc::new(tokio::sync::Mutex::new(TaskManager::new(2))); // For progress tasks
    let upload_semaphore = Arc::new(tokio::sync::Semaphore::new(2)); // Maximum 2 simultaneous uploads

    // Create custom HTTP client with longer timeout for long polling
    let reqwest_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60)) // Total timeout for request
        .connect_timeout(std::time::Duration::from_secs(10)) // Connection timeout
        .tcp_nodelay(true)
        .build()
        .expect("Failed to create HTTP client");

    let bot = Bot::with_client(bot_token, reqwest_client);

    let handler = dialogue::enter::<Update, dialogue::InMemStorage<BroadcastState>, BroadcastState, _>()
        .branch(
            Update::filter_message()
                .branch(
                    dptree::case![BroadcastState::WaitingForMessage]
                        .endpoint(receive_broadcast_message)
                )
                .branch(
                    dptree::case![BroadcastState::Idle]
                        .filter(|msg: Message| {
                            msg.text().map(|t| t == BTN_BROADCAST).unwrap_or(false)
                        })
                        .endpoint(start_broadcast)
                )
        )
        .branch(
            Update::filter_callback_query()
                .filter(|q: CallbackQuery| {
                    q.data.as_ref().map_or(false, |data| {
                        data == "broadcast_confirm" || data == "broadcast_cancel"
                    })
                })
                .endpoint(|bot: Bot, dialogue: MyDialogue, q: CallbackQuery, db_pool: Arc<DatabasePool>| async move {
                    match dialogue.get().await {
                        Ok(Some(BroadcastState::WaitingForConfirmation { message })) => {
                            handle_broadcast_confirmation(bot, dialogue, q, db_pool, message).await
                        }
                        _ => Ok(()),
                    }
                })
        )
        .branch(
            Update::filter_message()
                .filter_command::<AdminCommand>()
                .endpoint(|bot: Bot, msg: Message, cmd: AdminCommand, db_pool: Arc<DatabasePool>| async move {
                    // Get the path to yt-dlp
                    let exe_dir = std::env::current_exe()?
                        .parent()
                        .ok_or_else(|| anyhow::anyhow!("Failed to get parent directory"))?
                        .to_path_buf();
                    let ytdlp_path = exe_dir.join("lib").join("yt-dlp");
                    let ytdlp_path_str = ytdlp_path.to_string_lossy().to_string();

                    // Check if it's an admin
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
                                let id_cloned_for_format = id.clone();
                                let name_cloned_for_format = name.clone();

                                let result = db_pool
                                    .execute_with_timeout(move |conn| {
                                        conn.execute(
                                            "INSERT OR REPLACE INTO channels (channel_id, channel_name) VALUES (?1, ?2)",
                                            rusqlite::params![id, name],
                                        )
                                    })
                                    .await;

                                match result {
                                    Ok(_) => {
                                        bot.send_message(
                                            msg.chat.id,
                                            format!("✅ Channel '{}' added: {}", name_cloned_for_format, id_cloned_for_format),
                                        )
                                        .await?;
                                    }
                                    Err(e) => {
                                        log::error!("AddChannel DB error: {}", e);
                                        bot.send_message(msg.chat.id, "Failed to add channel.").await?;
                                    }
                                }
                            } else {
                                bot.send_message(msg.chat.id, "Usage: /addchannel <id>,<name>").await?;
                            }
                        }
                        AdminCommand::DelChannel { id } => {
                            let id_cloned_for_format = id.clone();
                            let result = db_pool
                                .execute_with_timeout(move |conn| {
                                    conn.execute("DELETE FROM channels WHERE channel_id = ?1", rusqlite::params![id])
                                })
                                .await;

                            match result {
                                Ok(changes) => {
                                    if changes > 0 {
                                        bot.send_message(
                                            msg.chat.id,
                                            format!("✅ Channel deleted: {}", id_cloned_for_format),
                                        )
                                        .await?;
                                    } else {
                                        bot.send_message(
                                            msg.chat.id,
                                            format!("❌ Channel not found: {}", id_cloned_for_format),
                                        )
                                        .await?;
                                    }
                                }
                                Err(e) => {
                                    log::error!("DelChannel DB error: {}", e);
                                    bot.send_message(msg.chat.id, "Failed to delete channel.").await?;
                                }
                            }
                        }
                        AdminCommand::ListChannels => {
                            let result = db_pool
                                .execute_with_timeout(|conn| {
                                    let mut stmt = conn.prepare("SELECT channel_id, channel_name FROM channels")?;
                                    let channels_iter = stmt.query_map([], |row| {
                                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                                    })?;
                                    let mut channels = Vec::new();
                                    for channel_result in channels_iter {
                                        channels.push(channel_result?);
                                    }
                                    Ok(channels)
                                })
                                .await;

                            match result {
                                Ok(channels) => {
                                    let mut response = String::from("📋 Subscription channels:\n");
                                    for (id, name) in channels {
                                        response.push_str(&format!("- {} ({})\n", name, id));
                                    }
                                    bot.send_message(msg.chat.id, response).await?;
                                }
                                Err(e) => {
                                    log::error!("ListChannels DB error: {}", e);
                                    bot.send_message(msg.chat.id, "Failed to list channels.").await?;
                                }
                            }
                        }
                        AdminCommand::ToggleSubscription => {
                            let result = db_pool
                                .execute_with_timeout(|conn| {
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
                                })
                                .await;

                            match result {
                                Ok(new_value) => {
                                    let status = if new_value { "enabled" } else { "disabled" };
                                    bot.send_message(
                                        msg.chat.id,
                                        format!("✅ Mandatory subscription is now {}", status),
                                    )
                                    .await?;
                                }
                                Err(e) => {
                                    log::error!("ToggleSubscription DB error: {}", e);
                                    bot.send_message(msg.chat.id, "Failed to toggle subscription setting.").await?;
                                }
                            }
                        }
                        AdminCommand::Fingerprint => {
                            crate::handlers::fingerprint::fingerprint_list_handler(bot, msg, &ytdlp_path_str).await?;
                        }
                    }

                    Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                })
        )
        .branch(
            Update::filter_message()
                .filter(|msg: Message| msg.text().map_or(false, |t| t.starts_with("/setfingerprint-")))
                .endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
                    let text = msg.text().unwrap_or_default();
                    if !crate::handlers::admin::is_admin(&msg).await {
                        bot.send_message(msg.chat.id, "❌ This command is for admins only.").await?;
                        return Ok(());
                    }
                    let fingerprint = text.trim_start_matches("/setfingerprint-").to_string();

                    let exe_dir = std::env::current_exe()?.parent().ok_or_else(|| anyhow::anyhow!("Failed to get parent directory"))?.to_path_buf();
                    let ytdlp_path = exe_dir.join("lib").join("yt-dlp");
                    let ytdlp_path_str = ytdlp_path.to_string_lossy().to_string();

                    crate::handlers::fingerprint::set_fingerprint_handler(bot, msg, db_pool, fingerprint, &ytdlp_path_str).await
                })
        )
        .branch(Update::filter_message().filter_command::<Command>().endpoint(command_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_SETTINGS)).endpoint(settings_text_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_FORMAT)).endpoint(format_text_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_ADMIN_PANEL)).endpoint(admin_panel_text_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("Stats")).endpoint(stats_text_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("Top 10")).endpoint(top10_text_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some("All users")).endpoint(all_users_text_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_SUBSCRIPTION)).endpoint(subscription_text_handler))
        .branch(Update::filter_message().filter(|msg: Message| msg.text() == Some(BTN_BACK)).endpoint(back_text_handler))
        .branch(Update::filter_message()
            .filter(|msg: Message| msg.text() == Some("h265"))
            .endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
                let user_id = msg.chat.id.0;
                db_pool.execute_with_timeout(move |conn| {
                    conn.execute("UPDATE users SET quality_preference = 'h265' WHERE telegram_id = ?1", &[&user_id])
                }).await?;
                db_pool.invalidate_user_quality_cache(user_id).await;

                bot.send_message(msg.chat.id, "Quality set to h265")
                    .reply_markup(crate::handlers::command::get_main_reply_keyboard())
                    .await?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            }))
        .branch(Update::filter_message()
            .filter(|msg: Message| msg.text() == Some("h264"))
            .endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
                let user_id = msg.chat.id.0;
                db_pool.execute_with_timeout(move |conn| {
                    conn.execute("UPDATE users SET quality_preference = 'h264' WHERE telegram_id = ?1", &[&user_id])
                }).await?;
                db_pool.invalidate_user_quality_cache(user_id).await;

                bot.send_message(msg.chat.id, "Quality set to h264")
                    .reply_markup(crate::handlers::command::get_main_reply_keyboard())
                    .await?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            }))
        .branch(Update::filter_message()
            .filter(|msg: Message| msg.text() == Some("audio"))
            .endpoint(|bot: Bot, msg: Message, db_pool: Arc<DatabasePool>| async move {
                let user_id = msg.chat.id.0;
                db_pool.execute_with_timeout(move |conn| {
                    conn.execute("UPDATE users SET quality_preference = 'audio' WHERE telegram_id = ?1", &[&user_id])
                }).await?;
                db_pool.invalidate_user_quality_cache(user_id).await;

                bot.send_message(msg.chat.id, "Quality set to audio")
                    .reply_markup(crate::handlers::command::get_main_reply_keyboard())
                    .await?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            }))
        .branch(Update::filter_message()
            .filter(|msg: Message| {
                msg.text().map(|t| !crate::handlers::ui::is_system_button(t)).unwrap_or(false)
            })
            .endpoint(|bot: Bot, msg: Message, fetcher: Arc<YoutubeFetcher>, mtproto_uploader: Arc<MTProtoUploader>, db_pool: Arc<DatabasePool>, task_manager: Arc<tokio::sync::Mutex<TaskManager>>, upload_semaphore: Arc<tokio::sync::Semaphore>| async move {
                let message_key = format!("{}:{}:{}",
                    msg.chat.id.0,
                    msg.id.0,
                    msg.text().unwrap_or("")
                );

                // Check if already being processed
                {
                    let mut processing = PROCESSING.lock().await;
                    if processing.contains(&message_key) {
                        log::debug!("Skipping duplicate message {}", message_key);
                        return Ok(());
                    }
                    processing.insert(message_key.clone());
                }

                tokio::spawn(async move {
                    let result = link_handler(
                        bot.clone(),
                        msg.clone(),
                        fetcher,
                        mtproto_uploader,
                        db_pool,
                        task_manager,
                        upload_semaphore,
                    ).await;

                    // Remove from processing after completion
                    {
                        let mut processing = PROCESSING.lock().await;
                        processing.remove(&message_key);
                    }

                    if let Err(e) = result {
                        log::error!("Link handler error: {}", e);
                        // Optionally: send an error message to the user
                        let _ = bot.send_message(msg.chat.id, "Failed to process video").await;
                    }
                });

                // IMMEDIATELY return Ok() so that Telegram does not retry the update
                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            })
        );

    log::info!(
        "Bot initialization completed in {:.2?}",
        start_time.elapsed()
    );
    log::info!("Starting to dispatch updates...");

    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![
            dialogue::InMemStorage::<BroadcastState>::new(), // Added for FSM dialogue
            fetcher,
            mtproto_uploader,
            db_pool,
            task_manager.clone(),
            upload_semaphore
        ])
        .enable_ctrlc_handler()
        .build();

    // Run dispatcher with graceful shutdown
    tokio::select! {
        _ = dispatcher.dispatch() => {},
        _ = tokio::signal::ctrl_c() => {
            log::info!("Received Ctrl+C, shutting down...");
        }
    }

    // Cleanup on shutdown
    {
        let mut tm = task_manager.lock().await;
        tm.shutdown().await;
    }

    log::info!("Bot shutdown complete");
    Ok(())
}
