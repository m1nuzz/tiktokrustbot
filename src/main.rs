use teloxide::prelude::*;
use std::sync::Arc;
use anyhow::Error;

use crate::commands::Command;
use crate::database::DatabasePool;
use crate::handlers::{
    admin_command_handler, callback_handler, command_handler, link_handler,
};
use crate::yt_dlp_interface::{YoutubeFetcher, is_executable_present, ensure_binaries};
use crate::mtproto_uploader::MTProtoUploader;
use crate::utils::task_manager::TaskManager;
use teloxide::dptree;


#[cfg(not(target_os = "android"))]

#[cfg(target_os = "android")]
use robius_directories::ProjectDirs;

mod commands;
mod config;
mod database;
mod handlers;
pub mod mtproto_uploader;
mod yt_dlp_interface;
mod utils;
mod telegram_bot_api_uploader;
pub mod peers;
mod auto_update;

#[tokio::main]
async fn main() -> Result<(), Error> {
    // --- Logging Setup ---
    use log::LevelFilter;
    use std::sync::Mutex;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::env;

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
    let max_level = std::cmp::max(
        console_level,
        file_level_config.unwrap_or(LevelFilter::Off)
    );

    // 4. Setup file handle if needed
    let log_file = if file_level_config.is_some() {
        let file = OpenOptions::new().create(true).write(true).append(true).open("bot_errors.log")?;
        Some(Arc::new(Mutex::new(file)))
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

    let exe_dir = std::env::current_exe()?.parent().ok_or_else(|| anyhow::anyhow!("Failed to get parent directory of executable"))?.to_path_buf();
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
    log::info!("Contents of libraries directory: {:?}", std::fs::read_dir(&libraries_dir)?.map(|e| e.unwrap().file_name()).collect::<Vec<_>>());

    let yt_dlp_path = libraries_dir.join(if cfg!(target_os = "windows") { "yt-dlp.exe" } else { "yt-dlp" });
    let ffmpeg_dir = libraries_dir.join("ffmpeg");
    let ffmpeg_path = ffmpeg_dir.join(if cfg!(target_os = "windows") { "ffmpeg.exe" } else { "ffmpeg" });
    let ffprobe_path = ffmpeg_dir.join(if cfg!(target_os = "windows") { "ffprobe.exe" } else { "ffprobe" });

    if !is_executable_present(&yt_dlp_path) {
        log::error!("yt-dlp not found at {:?} after attempted download", yt_dlp_path);
        return Err(anyhow::Error::msg("yt-dlp not available"));
    } else {
        log::info!("yt-dlp found at {:?}", yt_dlp_path);
    }

    if !is_executable_present(&ffmpeg_path) {
        log::error!("ffmpeg not found at {:?} after attempted download", ffmpeg_path);
        return Err(anyhow::Error::msg("ffmpeg not available"));
    }

    if !is_executable_present(&ffprobe_path) {
        log::error!("ffprobe not found at {:?} after attempted download", ffprobe_path);
        return Err(anyhow::Error::msg("ffprobe not available"));
    }

    // Настройка автообновления ПОСЛЕ ensure_binaries
    let auto_updater = Arc::new(auto_update::AutoUpdater::new(libraries_dir.clone(), 30)); // Проверка каждые 30 минут
    
    // Первоначальная проверка обновлений
    if let Err(e) = auto_updater.check_for_updates().await {
        log::warn!("Initial update check failed: {}", e);
    }

    // Запускаем периодическую проверку в фоне
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

    let fetcher = Arc::new(YoutubeFetcher::new(yt_dlp_path, output_dir.clone(), ffmpeg_dir.clone())?);
    let bot_token = env::var("TELOXIDE_TOKEN").expect("TELOXIDE_TOKEN must be set");
    let mtproto_uploader = match MTProtoUploader::new(&bot_token, ffprobe_path.clone(), ffmpeg_path.clone()).await {
        Ok(uploader) => Arc::new(uploader),
        Err(e) => {
            log::error!("Failed to create MTProtoUploader: {}", e);
            return Err(anyhow::anyhow!("{}", e));
        }
    };

    // Create database pool and task manager
    let db_pool = Arc::new(DatabasePool::new(
        crate::database::get_database_path(),
        3 // Maximum 3 simultaneous database connections
    ));
    
    let task_manager = Arc::new(tokio::sync::Mutex::new(TaskManager::new(2))); // For progress tasks
    let upload_semaphore = Arc::new(tokio::sync::Semaphore::new(2)); // Maximum 2 simultaneous uploads

    let bot = Bot::from_env();

    let handler = dptree::entry()
        .branch(Update::filter_message()
            .filter_async(|msg: Message| async move {
                msg.text().map_or(false, |text| text.starts_with("/addchannel") || text.starts_with("/delchannel") || text.starts_with("/listchannels"))
            })
            .endpoint(admin_command_handler)
        )
        .branch(Update::filter_message().filter_command::<Command>().endpoint(command_handler))
        .branch(Update::filter_message().endpoint(link_handler))
        .branch(Update::filter_callback_query().endpoint(callback_handler));

    log::info!("Bot initialization completed in {:.2?}", start_time.elapsed());
    log::info!("Starting to dispatch updates...");

    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![fetcher, mtproto_uploader, db_pool, task_manager.clone(), upload_semaphore])
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
