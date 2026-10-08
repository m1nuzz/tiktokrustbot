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

/// One configured bot: everything the loops below need per token.
struct ConfiguredBot {
    id: String,
    username: String,
    bot: Bot,
    uploader: Arc<MTProtoUploader>,
}

/// Bot tokens to run. `BOT_TOKENS` (comma-separated) enables multi-bot;
/// otherwise the legacy single token (TEST_MODE aware). Usernames align by
/// position via `BOT_USERNAMES`; empty slots fall back to get_me.
fn resolve_bot_tokens(is_test_mode: bool) -> (Vec<String>, Vec<String>) {
    if is_test_mode {
        return (Vec::new(), Vec::new());
    }
    let tokens: Vec<String> = env::var("BOT_TOKENS")
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let usernames: Vec<String> = env::var("BOT_USERNAMES")
        .map(|s| {
            s.split(',')
                .map(|u| u.trim().trim_start_matches('@').to_string())
                .collect()
        })
        .unwrap_or_default();
    (tokens, usernames)
}

/// Numeric Telegram bot id: the token prefix. Stable without API calls;
/// doubles as the `bot_id` stored on every row this bot writes.
fn bot_id_from_token(token: &str) -> Option<String> {
    let id = token.split(':').next().unwrap_or("");
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
        Some(id.to_string())
    } else {
        None
    }
}

/// Full per-bot acquisition for the multi path: auth check, username,
/// uploader with its own session file. Errors skip just this bot.
async fn setup_one_bot(
    token: String,
    username_override: Option<String>,
    session_file: std::path::PathBuf,
    reqwest_client: reqwest::Client,
    ffprobe_path: std::path::PathBuf,
    ffmpeg_path: std::path::PathBuf,
) -> anyhow::Result<ConfiguredBot> {
    let bot_id = bot_id_from_token(&token)
        .ok_or_else(|| anyhow::anyhow!("bot token has no numeric id prefix"))?;
    let bot = Bot::with_client(token.clone(), reqwest_client);
    let me_username = match bot.get_me().await {
        Ok(me) => {
            log::info!(
                "✅ Successfully connected to Telegram as @{} (bot id {})",
                me.user.username.clone().unwrap_or_default(),
                bot_id
            );
            me.user.username.unwrap_or_default()
        }
        Err(e) => {
            log::error!("❌ Auth Error for bot id {}: {}. Check its token.", bot_id, e);
            return Err(anyhow::anyhow!("auth failed for bot id {}", bot_id));
        }
    };
    let username = username_override
        .filter(|s| !s.is_empty())
        .unwrap_or(me_username);
    let uploader =
        MTProtoUploader::new_with_session(&token, session_file, ffprobe_path, ffmpeg_path)
            .await
            .map_err(|e| anyhow::anyhow!("uploader init failed for bot id {}: {}", bot_id, e))?;
    Ok(ConfiguredBot {
        id: bot_id,
        username,
        bot,
        uploader: Arc::new(uploader),
    })
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

    let (multi_tokens, multi_usernames) = resolve_bot_tokens(is_test_mode);

    let ffprobe_path = libraries_dir.join("ffmpeg").join(if cfg!(target_os = "windows") { "ffprobe.exe" } else { "ffprobe" });
    let ffmpeg_path = libraries_dir.join("ffmpeg").join(if cfg!(target_os = "windows") { "ffmpeg.exe" } else { "ffmpeg" });

    // Per-bot acquisition. Single token (or test mode) follows the legacy
    // path byte-for-byte, including the historic session filename; BOT_TOKENS
    // loops the same steps with per-bot session files. Either way the rest of
    // main sees one uniform Vec. List the original bot's token FIRST so legacy
    // 'primary' rows keep routing to it.
    let mut configured: Vec<ConfiguredBot> = Vec::new();
    if multi_tokens.is_empty() {
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

        let bot_id = bot_id_from_token(&raw_token)
            .unwrap_or_else(|| tiktokdownloader::database::PRIMARY_BOT_ID.to_string());
        // Legacy username source only: falling back to get_me here would
        // light up the premium button on deployments that left BOT_USERNAME
        // unset on purpose.
        let bot_username = env::var("BOT_USERNAME").unwrap_or_default();
        configured.push(ConfiguredBot {
            id: bot_id,
            username: bot_username,
            bot,
            uploader: mtproto_uploader,
        });
    } else {
        let mut seen = HashSet::new();
        for (i, token) in multi_tokens.iter().enumerate() {
            if !seen.insert(token.clone()) {
                log::warn!("Duplicate token at BOT_TOKENS position {i}, skipping");
                continue;
            }
            let username_override = multi_usernames.get(i).cloned().filter(|s| !s.is_empty());
            let session_file = libraries_dir.join(format!(
                "telegram-{}.session",
                token.split(':').next().unwrap_or("unknown")
            ));
            match setup_one_bot(
                token.clone(),
                username_override,
                session_file,
                reqwest_client.clone(),
                ffprobe_path.clone(),
                ffmpeg_path.clone(),
            )
            .await
            {
                Ok(cfg) => configured.push(cfg),
                Err(e) => log::error!("Skipping bot at BOT_TOKENS position {i}: {}", e),
            }
        }
    }
    if configured.is_empty() {
        return Err(anyhow::anyhow!("No usable bot tokens: set TELOXIDE_TOKEN or BOT_TOKENS"));
    }

    let db_path = tiktokdownloader::database::get_database_path();
    log::info!("🗄️ Using database at: {}", db_path);
    let db_pool = Arc::new(DatabasePool::new(db_path, 3));

    // Register every configured bot so per-bot rows join to a username.
    // A failed registration never blocks startup: delivery routing falls back.
    for cfg in &configured {
        if let Err(e) = db_pool.register_bot(&cfg.id, &cfg.username).await {
            log::error!("Failed to register bot {} (@{}): {}", cfg.id, cfg.username, e);
        }
    }

    // Sync settings from .env to database (only as initial defaults, don't overwrite admin panel values)
    if let Ok(sub_req) = env::var("SUBSCRIPTION_REQUIRED") {
        if db_pool.get_setting("subscription_required").await.is_err() {
            let val = if sub_req.to_lowercase() == "true" { "true" } else { "false" };
            let _ = db_pool.set_setting("subscription_required", val).await;
        }
    }
    if let Ok(ads_en) = env::var("ADS_ENABLED") {
        if db_pool.get_setting("ads_enabled").await.is_err() {
            let val = if ads_en.to_lowercase() == "true" { "true" } else { "false" };
            let _ = db_pool.set_setting("ads_enabled", val).await;
        }
    }

    let task_manager = Arc::new(tokio::sync::Mutex::new(TaskManager::new(2)));
    let upload_semaphore = Arc::new(tokio::sync::Semaphore::new(2));

    // --- Web Server Configuration ---
    // One map entry per configured bot. The first entry doubles as the
    // primary uploader for the shared download paths until Phase 2в routes
    // them per ymid.
    let mut bots = std::collections::HashMap::new();
    for cfg in &configured {
        bots.insert(
            cfg.id.clone(),
            tiktokdownloader::web_server::BotInfo {
                id: std::sync::Arc::new(cfg.id.clone()),
                username: std::sync::Arc::new(cfg.username.clone()),
                bot: cfg.bot.clone(),
            },
        );
    }
    let primary_uploader = configured
        .first()
        .map(|cfg| cfg.uploader.clone())
        .expect("at least one bot is configured");
    let web_server_state = tiktokdownloader::web_server::AppState {
        db: db_pool.clone(),
        bots,
        fetcher: fetcher.clone(),
        mtproto_uploader: primary_uploader,
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

    // Expiry sweeper for abandoned sessions: every 60 seconds, expire the
    // sessions whose LEASE has lapsed - the lease is pushed forward by the
    // client heartbeat while the mini-app is open, so a user still on the
    // webapp is never cut off - and notify each user exactly once, in their
    // own language, with the message that matches what actually happened.
    //
    // The message matters: the rows selected here are the ones whose ad was
    // never valued and where no download was ever attempted, so telling them
    // "couldn't download the video" was a lie 30 minutes after giving up. A
    // row is only told the download failed when a download job really started
    // for it (`job_started_at`). Rows are never deleted: the funnel and the
    // per-ymid joins depend on them. Legacy rows were retired silently at
    // startup migration.
    let sweep_db = db_pool.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            match sweep_db
                .expire_stale_pending(tiktokdownloader::database::EXPIRY_BATCH_LIMIT)
                .await
            {
                Ok(stale) => {
                    for (ymid, user_id, job_started) in stale {
                        // Silent on purpose. This used to message the user; the
                        // data showed it was wrong, because practically every
                        // expired session was one where NO ad was displayed -
                        // so it told people who never saw an ad to resend a link
                        // that could not help. The row still becomes `expired`
                        // so the funnel stays honest; only the notification is
                        // gone. Rows are kept, not deleted: that table is the
                        // record of what happened and costs ~3.5 MB.
                        log::warn!(
                            "Expiring abandoned session {} for user {} (download started: {})",
                            ymid,
                            user_id,
                            job_started
                        );
                    }
                }
                Err(e) => log::error!("Expiry sweeper failed: {}", e),
            }
        }
    });

    // One dispatcher per bot sharing everything except identity, dialogue
    // state and uploader. BroadcastState must NOT be shared: dialogue state
    // is per conversation, and conversations belong to one bot.

    log::info!("Bot initialized in {:.2?}", start_time.elapsed());
    let mut dispatch_tasks = Vec::new();
    for cfg in configured {
        let handler = build_handler();
        let mut dispatcher = Dispatcher::builder(cfg.bot, handler)
            .dependencies(dptree::deps![
                dialogue::InMemStorage::<BroadcastState>::new(),
                fetcher.clone(),
                cfg.uploader,
                db_pool.clone(),
                task_manager.clone(),
                upload_semaphore.clone()
            ])
            .enable_ctrlc_handler()
            .build();
        let bot_id = cfg.id.clone();
        dispatch_tasks.push(tokio::spawn(async move {
            log::info!("Dispatcher running for bot id {}", bot_id);
            dispatcher.dispatch().await;
        }));
    }

    let _ = tokio::signal::ctrl_c().await;
    log::info!("Received Ctrl+C, shutting down...");
    for task in dispatch_tasks {
        let _ = task.await;
    }
    let mut tm = task_manager.lock().await;
    tm.shutdown().await;
    log::info!("Bot shutdown complete");
    Ok(())
}
