use regex::Regex;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, WebAppInfo};

use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::time::Instant; // Import Instant and Duration here
use tokio::time::{Duration, timeout};
use uuid::Uuid; // Import HashMap here

use crate::database::DatabasePool;
use crate::handlers::admin::is_admin;
use crate::handlers::subscription::check_subscription;
use crate::handlers::ui::{BTN_SETTINGS, is_menu_button};
use crate::mtproto_uploader::MTProtoUploader;
use crate::telegram_bot_api_uploader::{
    send_audio_with_progress_botapi, send_video_with_progress_botapi,
};
use crate::utils::progress_bar::ProgressBar;
use crate::utils::task_manager::TaskManager;
use crate::yt_dlp_interface::YoutubeFetcher;

// For rate limiting per chat
lazy_static::lazy_static! {
    static ref LAST_SEND: Arc<tokio::sync::Mutex<HashMap<i64, Instant>>> = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    static ref URL_PROCESSING: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>> = Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));
}

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(600); // 10 minutes
const TELEGRAM_BOT_API_FILE_LIMIT: u64 = 48 * 1024 * 1024; // 48MB

// Add this function at the beginning of the file
fn extract_url_from_text(text: &str) -> Option<String> {
    // Regex for searching TikTok URL
    let re = Regex::new(r"https?://(?:www\.|vm\.|vt\.)?tiktok\.com/[^\s]+").unwrap();

    if let Some(captures) = re.find(text) {
        Some(captures.as_str().to_string())
    } else {
        None
    }
}

async fn get_subscription_required(
    db_pool: &DatabasePool,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let result = db_pool
        .execute_with_timeout(|conn| {
            match conn.query_row(
                "SELECT value FROM settings WHERE key = 'subscription_required'",
                [],
                |row| Ok(row.get::<_, String>(0)? == "true"),
            ) {
                Ok(value) => Ok(value),
                Err(_) => Ok(true), // Default to true
            }
        })
        .await?;
    Ok(result)
}

pub async fn link_handler(
    bot: Bot,
    msg: Message,
    fetcher: Arc<YoutubeFetcher>,
    mtproto_uploader: Arc<MTProtoUploader>,
    db_pool: Arc<DatabasePool>,
    task_manager: Arc<tokio::sync::Mutex<TaskManager>>,
    upload_semaphore: Arc<tokio::sync::Semaphore>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let user_id = msg.chat.id.0;

    // Update user activity using the database pool
    let result = db_pool
        .execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO users (telegram_id) VALUES (?1)",
                [user_id],
            )?;
            conn.execute(
                "UPDATE users SET last_active = CURRENT_TIMESTAMP WHERE telegram_id = ?1",
                [user_id],
            )?;
            Ok(())
        })
        .await;

    if let Err(e) = result {
        log::error!("Failed to update user activity: {}", e);
    }

    let text = match msg.text() {
        Some(text) => text,
        None => return Ok(()),
    };

    if is_menu_button(text) {
        return Ok(());
    }

    if text.contains("tiktok.com") {
        // Extract only the URL, not the entire text!
        let url = match extract_url_from_text(text) {
            Some(url) => url,
            None => {
                bot.send_message(msg.chat.id, "❌ Could not extract TikTok URL from message.")
                    .await?;
                return Ok(());
            }
        };

        // URL Deduplication
        {
            let mut urls = URL_PROCESSING.lock().await;
            if urls.contains(&url) {
                bot.send_message(msg.chat.id, "⏳ This video is already being processed.")
                    .await?;
                return Ok(());
            }
            urls.insert(url.clone());
        }

        // Rate Limiting per chat
        {
            let mut last_send = LAST_SEND.lock().await;
            if let Some(last_time) = last_send.get(&user_id) {
                let elapsed = last_time.elapsed();
                if elapsed < Duration::from_secs(2) {
                    let wait_time = Duration::from_secs(2) - elapsed;
                    log::info!(
                        "Rate limiting: waiting {:?} for chat {}",
                        wait_time,
                        user_id
                    );
                    tokio::time::sleep(wait_time).await;
                }
            }
            last_send.insert(user_id, Instant::now());
        }

        // Mini App Ad invitation
        let is_user_admin = is_admin(&msg).await;
        let is_premium = db_pool.is_user_premium(user_id as i64).await;

        let ads_enabled = {
            let module_enabled = std::env::var("MONETAG_MODULE_ENABLED")
                .map(|v| v.to_lowercase() == "true")
                .unwrap_or(true);
            
            let is_test_mode = std::env::var("TEST_MODE")
                .map(|v| v.to_lowercase() == "true")
                .unwrap_or(false);

            if !module_enabled {
                log::info!("Ads disabled: Module is disabled in .env");
                false
            } else if is_user_admin {
                // If user is admin, check if admin ads are explicitly enabled via DB
                let admin_ads = db_pool.get_setting("admin_ads_enabled").await.map(|val| val == "true").unwrap_or(false);
                if admin_ads {
                    log::info!("Ads enabled for admin: admin_ads_enabled is true");
                    true
                } else if is_test_mode {
                    log::info!("Ads enabled for admin (Legacy Test Mode): TEST_MODE is true");
                    true
                } else {
                    log::info!("Ads disabled: User is admin and ads are not forced");
                    false
                }
            } else if is_premium {
                log::info!("Ads disabled: User {} has Premium", user_id);
                false
            } else {
                let db_status = db_pool.get_setting("ads_enabled").await.map(|val| val == "true").unwrap_or(true);
                log::info!("Ads status from DB: {}", db_status);
                db_status
            }
        };

        if ads_enabled {
            let webapp_url = std::env::var("WEBAPP_URL").unwrap_or_default();
            log::info!("WEBAPP_URL for ads is: '{}'", webapp_url);
            if !webapp_url.is_empty() {
                if let Ok(url_obj) = webapp_url.parse::<reqwest::Url>() {
                    // Create pending download
                    let ymid = match db_pool.create_pending_download(user_id as i64, &url).await {
                        Ok(id) => id,
                        Err(e) => {
                            log::error!("Failed to create pending download: {}", e);
                            bot.send_message(msg.chat.id, "❌ Error initializing download. Please try again.").await?;
                            return Ok(());
                        }
                    };

                    // Construct URL with ymid
                    let mut final_url = url_obj.clone();
                    final_url.query_pairs_mut().append_pair("ymid", &ymid);

                    log::info!("Sending ad invitation with ymid {} to user {}", ymid, user_id);
                    
                    let keyboard = InlineKeyboardMarkup::new(vec![
                        vec![InlineKeyboardButton::web_app("🇷🇺 Получить видео", WebAppInfo { url: final_url.clone() })],
                        vec![InlineKeyboardButton::web_app("🇺🇸 Get the video", WebAppInfo { url: final_url.clone() })],
                        vec![InlineKeyboardButton::web_app("🇪🇸 Obtener video", WebAppInfo { url: final_url.clone() })],
                        vec![InlineKeyboardButton::web_app("🇨🇳 获取视频", WebAppInfo { url: final_url.clone() })],
                        vec![InlineKeyboardButton::web_app("🇸🇦 الحصول на видео", WebAppInfo { url: final_url })],
                    ]);

                    let ad_text = "👇 CLICK BUTTON 👇";
                    
                    match bot.send_message(msg.chat.id, ad_text)
                        .reply_markup(keyboard)
                        .await {
                            Ok(_) => {
                                log::info!("Ad invitation sent successfully to {}", user_id);
                                
                                // Send Premium Invoice directly
                                let _ = crate::handlers::payments::send_premium_invoice(bot.clone(), msg.chat.id, db_pool.clone()).await;

                                // STOP HERE. Do not download yet.
                                // Cleanup URL_PROCESSING since we're not actually processing it yet (it's pending)
                                {
                                    let mut urls = URL_PROCESSING.lock().await;
                                    urls.remove(&url);
                                }
                                return Ok(());
                            },
                            Err(e) => {
                                log::error!("CRITICAL: Failed to send ad invitation: {}. Proceeding to download to avoid broken UX.", e);
                            }
                        }
                } else {
                    log::error!("Failed to parse WEBAPP_URL: '{}'", webapp_url);
                }
            } else {
                log::warn!("WEBAPP_URL is empty, skipping ad invitation.");
            }
        }

        // Proceed normally (admins or ads disabled or premium)
        process_video_request(
            bot,
            user_id,
            url,
            fetcher,
            mtproto_uploader,
            db_pool,
            task_manager,
            upload_semaphore,
            msg.chat.username().map(|s| s.to_string()).or_else(|| msg.from.and_then(|u| u.username.clone())),
            msg.chat.id
        ).await?;

    } else {
        let keyboard = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            BTN_SETTINGS,
            "settings",
        )]]);
        bot.send_message(msg.chat.id, "Please send a valid TikTok link.")
            .reply_markup(keyboard)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    }

    Ok(())
}

/// Core logic for downloading and sending video
pub async fn process_video_request(
    bot: Bot,
    user_id: i64,
    url: String,
    fetcher: Arc<YoutubeFetcher>,
    mtproto_uploader: Arc<MTProtoUploader>,
    db_pool: Arc<DatabasePool>,
    _task_manager: Arc<tokio::sync::Mutex<TaskManager>>,
    upload_semaphore: Arc<tokio::sync::Semaphore>,
    username: Option<String>,
    chat_id: ChatId,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Get user quality preference with caching
    let quality_preference = db_pool
        .get_user_quality(user_id)
        .await
        .unwrap_or_else(|_| "best".to_string());

    let fingerprint =
        crate::handlers::fingerprint::get_current_fingerprint(db_pool.clone()).await;
    if let Some(ref fp) = fingerprint {
        log::info!("🔐 Using TLS fingerprint for download: {}", fp);
    }

    let is_audio = quality_preference == "audio";
    log::info!(
        "Quality preference: {}, is_audio: {}",
        quality_preference,
        is_audio
    );

    // Get upload permit to limit concurrent uploads
    let _upload_permit = upload_semaphore
        .acquire()
        .await
        .map_err(|e| anyhow::anyhow!("Semaphore error: {}", e))?;

    let subscription_required = get_subscription_required(&db_pool).await.unwrap_or(true);
    log::debug!("Subscription required setting: {}", subscription_required);

    if subscription_required {
        // Check if user is admin - slightly different here because we don't have the Message object
        // but we can check the database or the bot can just try to check subscription
        if !check_subscription(&bot, user_id).await.unwrap_or(false) {
            // Need to double check if it's admin via DB if subscription fails
            let admins: Vec<i64> = std::env::var("ADMIN_IDS")
                .unwrap_or_default()
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect();
            
            if !admins.contains(&user_id) {
                bot.send_message(
                    chat_id,
                    "To use the bot, please subscribe to our channels.",
                )
                .await?;

                // Cleanup URL_PROCESSING
                {
                    let mut urls = URL_PROCESSING.lock().await;
                    urls.remove(&url);
                }
                return Ok(());
            }
        }
    }

    // Create a single ProgressBar instance to be used for the entire operation
    let mut progress_bar = ProgressBar::new(bot.clone(), chat_id);
    progress_bar.start("🎬 Starting...").await?;

    // Update the progress bar to show that download is starting
    progress_bar
        .update(5, Some("⬇️ Starting download..."))
        .await?;

    // Manual retry loop for download
    let mut retries = 0;
    let download_result = loop {
        let file_stem = format!("output/{}", Uuid::new_v4());
        let download_future = fetcher.download_video_from_url(
            url.clone(),
            &file_stem,
            &quality_preference,
            fingerprint.clone(),
            &mut progress_bar,
        );

        match timeout(DOWNLOAD_TIMEOUT, download_future).await {
            Ok(Ok(path)) => break Ok(path),
            Ok(Err(e)) => {
                retries += 1;
                if retries >= 3 {
                    break Err(e);
                }
                let delay_ms = (1000 * 2_u64.pow(retries - 1)).min(30000);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            Err(e) => {
                // timeout
                retries += 1;
                if retries >= 3 {
                    break Err(anyhow::Error::new(e));
                }
                let delay_ms = (1000 * 2_u64.pow(retries - 1)).min(30000);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }
    };

    let path = match download_result {
        Ok(path) => path,
        Err(e) => {
            progress_bar.delete().await?;
            let error_message = if e.to_string().contains("Sign in required") {
                "🔒 Video requires sign in to TikTok - currently unavailable for download".to_string()
            } else if e.to_string().contains("Video unavailable") || e.to_string().contains("Requested format is not available") {
                "🚫 Video is unavailable or has been removed".to_string()
            } else if e.to_string().contains("Private video") {
                "🔒 Video is private and cannot be downloaded".to_string()
            } else if e.to_string().contains("This video is age-restricted") {
                "🔞 Video is age-restricted and cannot be downloaded".to_string()
            } else if e.to_string().contains("Failed to parse") || e.to_string().contains("JSON") {
                "🔧 Error processing TikTok API response. Please try again later.".to_string()
            } else if e.to_string().contains("timeout") {
                "⏰ Download timeout - please try again".to_string()
            } else {
                format!("❌ Failed to download video: {}", e.to_string().chars().take(100).collect::<String>())
            };

            bot.send_message(chat_id, error_message).await?;
            // Cleanup URL_PROCESSING
            {
                let mut urls = URL_PROCESSING.lock().await;
                urls.remove(&url);
            }
            return Ok(());
        }
    };

    // Create RAII wrapper for file cleanup
    let _temp_file_guard = TempFile::new(path.clone());

    let file_size = fs::metadata(&path)?.len();

    if file_size > TELEGRAM_BOT_API_FILE_LIMIT {
        progress_bar.update(85, Some("📤 Starting upload...")).await?;

        let upload_result = if is_audio {
            mtproto_uploader.upload_audio(user_id, username.clone(), &path, "", &mut progress_bar).await
        } else {
            mtproto_uploader.upload_video(user_id, username.clone(), &path, "", &mut progress_bar).await
        };

        match upload_result {
            Ok(_) => {
                progress_bar.update(100, Some("✅ Done!")).await?;
                tokio::time::sleep(Duration::from_millis(500)).await;
                progress_bar.delete().await?;
            }
            Err(e) => {
                progress_bar.delete().await?;
                let error_msg = if let Some(wait_seconds) = crate::utils::retry::extract_flood_wait(&e.to_string()) {
                    format!("⏳ Rate limited. Please wait {} seconds and try again.", wait_seconds)
                } else {
                    "❌ Upload failed - please try again later".to_string()
                };
                bot.send_message(chat_id, error_msg).await?;
            }
        }
    } else {
        let mut retries = 0;
        let send_result = loop {
            let send_future: Pin<Box<dyn Future<Output = Result<(), anyhow::Error>> + Send>> =
                Box::pin(async {
                    if is_audio {
                        send_audio_with_progress_botapi(&bot.token(), chat_id, &path, None, &mut progress_bar).await
                    } else {
                        send_video_with_progress_botapi(&bot.token(), chat_id, &path, None, &mut progress_bar).await
                    }
                });

            match timeout(UPLOAD_TIMEOUT, send_future).await {
                Ok(Ok(val)) => break Ok(val),
                Ok(Err(e)) => {
                    retries += 1;
                    if retries >= 3 { break Err(e); }
                    tokio::time::sleep(Duration::from_millis(1000 * 2_u64.pow(retries - 1))).await;
                }
                Err(e) => {
                    retries += 1;
                    if retries >= 3 { break Err(anyhow::Error::new(e)); }
                    tokio::time::sleep(Duration::from_millis(1000 * 2_u64.pow(retries - 1))).await;
                }
            }
        };

        match send_result {
            Ok(_) => {}
            Err(_) => {
                progress_bar.delete().await?;
                bot.send_message(chat_id, "❌ Send failed after retries").await?;
            }
        }
    }

    // Logging
    let video_url = url.clone();
    let db_pool_cloned = db_pool.clone();
    let _ = db_pool_cloned.execute_with_timeout(move |conn| {
        conn.execute("INSERT OR IGNORE INTO users (telegram_id) VALUES (?1)", [user_id])?;
        conn.execute("UPDATE users SET last_active = CURRENT_TIMESTAMP WHERE telegram_id = ?1", [user_id])?;
        conn.execute("INSERT INTO downloads (user_telegram_id, video_url) VALUES (?1, ?2)", (user_id, video_url))?;
        Ok(())
    }).await;

    // Cleanup URL_PROCESSING
    {
        let mut urls = URL_PROCESSING.lock().await;
        urls.remove(&url);
    }

    Ok(())
}

// RAII for automatic file cleanup
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if std::thread::panicking() { return; }
        let _ = std::fs::remove_file(&mut self.path);
    }
}
