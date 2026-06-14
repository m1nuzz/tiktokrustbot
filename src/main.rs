use anyhow::Error;
use std::collections::HashSet;
use std::sync::Arc;
use std::env;
use teloxide::prelude::*;
use tokio::sync::Mutex;

use tiktokdownloader::database::DatabasePool;
use tiktokdownloader::handlers::broadcast::BroadcastState;
use tiktokdownloader::mtproto_uploader::MTProtoUploader;
use tiktokdownloader::utils::task_manager::TaskManager;
use tiktokdownloader::yt_dlp_interface::{ensure_binaries, is_executable_present, YoutubeFetcher};
use tiktokdownloader::build_handler;
use teloxide::dispatching::dialogue;
use teloxide::dptree;

// For deduplication
lazy_static::lazy_static! {
    static ref PROCESSING: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
}

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
    if let Err(e) = tiktokdownloader::config::load_environment() {
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

    if !is_executable_present(&yt_dlp_path) {
        return Err(anyhow::Error::msg("yt-dlp not available"));
    }

    let auto_updater = Arc::new(tiktokdownloader::auto_update::AutoUpdater::new(libraries_dir.clone(), 30));
    let _ = auto_updater.check_for_updates().await;

    let updater_clone = Arc::clone(&auto_updater);
    tokio::spawn(async move {
        let _ = updater_clone.start_periodic_checks().await;
    });

    if let Err(e) = tiktokdownloader::database::init_database() {
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

    let ffprobe_path = libraries_dir.join("ffmpeg").join(if cfg!(target_os = "windows") { "ffprobe.exe" } else { "ffprobe" });
    let ffmpeg_path = libraries_dir.join("ffmpeg").join(if cfg!(target_os = "windows") { "ffmpeg.exe" } else { "ffmpeg" });

    let mtproto_uploader = match MTProtoUploader::new(&raw_token, ffprobe_path, ffmpeg_path).await {
        Ok(uploader) => Arc::new(uploader),
        Err(e) => return Err(anyhow::anyhow!("{}", e)),
    };

    let db_path = tiktokdownloader::database::get_database_path();
    log::info!("🗄️ Using database at: {}", db_path);
    let db_pool = Arc::new(DatabasePool::new(db_path, 3));

    // Sync settings from .env to database
    if let Ok(sub_req) = env::var("SUBSCRIPTION_REQUIRED") {
        let val = if sub_req.to_lowercase() == "true" { "true" } else { "false" };
        let _ = db_pool.set_setting("subscription_required", val).await;
    }
    if let Ok(ads_en) = env::var("ADS_ENABLED") {
        let val = if ads_en.to_lowercase() == "true" { "true" } else { "false" };
        let _ = db_pool.set_setting("ads_enabled", val).await;
    }

    let task_manager = Arc::new(tokio::sync::Mutex::new(TaskManager::new(2)));
    let upload_semaphore = Arc::new(tokio::sync::Semaphore::new(2));

    // --- Web Server Configuration ---
    let web_server_state = tiktokdownloader::web_server::AppState {
        db: db_pool.clone(),
        bot: bot.clone(),
        fetcher: fetcher.clone(),
        mtproto_uploader: mtproto_uploader.clone(),
        task_manager: task_manager.clone(),
        upload_semaphore: upload_semaphore.clone(),
    };

    let web_port: u16 = env::var("WEB_SERVER_PORT")
        .unwrap_or_else(|_| "8088".to_string())
        .parse()
        .unwrap_or(8088);

    tokio::spawn(async move {
        tiktokdownloader::web_server::start_web_server(web_server_state, web_port).await;
    });

    let handler = build_handler();

    log::info!("Bot initialized in {:.2?}", start_time.elapsed());
    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![
            dialogue::InMemStorage::<BroadcastState>::new(), 
            fetcher, 
            mtproto_uploader, 
            db_pool, 
            task_manager.clone(), 
            upload_semaphore
        ])
        .enable_ctrlc_handler()
        .build();

    tokio::select! {
        _ = dispatcher.dispatch() => {},
        _ = tokio::signal::ctrl_c() => log::info!("Received Ctrl+C, shutting down..."),
    }
    let mut tm = task_manager.lock().await;
    tm.shutdown().await;
    log::info!("Bot shutdown complete");
    Ok(())
}
