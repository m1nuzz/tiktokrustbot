use regex::Regex;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, WebAppInfo};

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use tokio::time::Instant;
use tokio::time::{Duration, timeout};
use uuid::Uuid;

use crate::database::DatabasePool;
use crate::handlers::admin::is_admin;
use crate::handlers::subscription::check_subscription;
use crate::handlers::ui::is_menu_button;
use crate::mtproto_uploader::MTProtoUploader;
use crate::telegram_bot_api_uploader::{
    send_audio_with_progress_botapi, send_video_with_progress_botapi,
};
use crate::utils::progress_bar::ProgressBar;
use crate::utils::task_manager::TaskManager;
use crate::utils::temp_file::TempFileGuard;
use crate::yt_dlp_interface::YoutubeFetcher;

// To track active link processing and avoid double-triggering
lazy_static::lazy_static! {
    static ref LAST_SEND: Arc<tokio::sync::Mutex<HashMap<i64, Instant>>> = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    static ref URL_PROCESSING: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>> = Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));
}

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes
const TELEGRAM_BOT_API_FILE_LIMIT: u64 = 48 * 1024 * 1024; // 48MB

// Add this function at the beginning of the file
fn extract_url_from_text(text: &str) -> Option<String> {
    // Regex for searching TikTok, Instagram or YouTube URL
    let tiktok_re = Regex::new(r"https?://(?:www\.|vm\.|vt\.)?tiktok\.com/[^\s]+").unwrap();
    let instagram_re = Regex::new(r"https?://(?:www\.)?instagram\.com/(?:reels?|p|tv)/[^\s]+").unwrap();
    let youtube_re = Regex::new(r"https?://(?:www\.)?(?:youtube\.com/shorts/|youtube\.com/watch\?v=|youtu\.be/)[^\s]+").unwrap();

    if let Some(mat) = tiktok_re.find(text) {
        Some(mat.as_str().to_string())
    } else if let Some(mat) = instagram_re.find(text) {
        Some(mat.as_str().to_string())
    } else if let Some(mat) = youtube_re.find(text) {
        Some(mat.as_str().to_string())
    } else {
        None
    }
}

fn get_localized_ad_button_text(lang_code: Option<&str>) -> &'static str {
    match lang_code {
        Some("ru") => "🚀 Скачать видео (Бесплатно)",
        Some("es") => "🚀 Descargar video (Gratis)",
        Some("zh") | Some("zh-hans") | Some("zh-hant") => "🚀 下载视频 (免费)",
        Some("ar") => "🚀 تحميل الفيديو (مجاني)",
        _ => "🚀 Download video (Free)",
    }
}

fn get_localized_premium_button_text(lang_code: Option<&str>) -> &'static str {
    match lang_code {
        Some("ru") => "⭐️ Убрать рекламу (Premium)",
        Some("es") => "⭐️ Quitar anuncios (Premium)",
        Some("zh") | Some("zh-hans") | Some("zh-hant") => "⭐️ 移除广告 (Premium)",
        Some("ar") => "⭐️ إزالة الإعلانات (Premium)",
        _ => "⭐️ Remove ads (Premium)",
    }
}

fn get_localized_choice_text(lang_code: Option<&str>) -> &'static str {
    match lang_code {
        Some("ru") => "📥 Ваше видео готово к загрузке!\nВыберите вариант скачивания:",
        Some("es") => "📥 ¡Tu video está listo para descargar!\nElige una opción de descarga:",
        Some("zh") | Some("zh-hans") | Some("zh-hant") => "📥 您的视频已准备好下载！\n请选择下载选项：",
        Some("ar") => "📥 الفيديو الخاص بك جاهز للتنزيل!\nاختر خيار التنزيل:",
        _ => "📥 Your video is ready for download!\nChoose a download option:",
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

    // Update user activity
    let _ = db_pool.execute_with_timeout(move |conn| {
        conn.execute("INSERT OR IGNORE INTO users (telegram_id) VALUES (?1)", [user_id])?;
        conn.execute("UPDATE users SET last_active = CURRENT_TIMESTAMP WHERE telegram_id = ?1", [user_id])?;
        Ok(())
    }).await;

    let text = match msg.text() {
        Some(text) => text,
        None => return Ok(()),
    };

    if is_menu_button(text) {
        return Ok(());
    }

    let url = match extract_url_from_text(text) {
        Some(url) => url,
        None => return Ok(()),
    };

    // Deduplication
    {
        let mut urls = URL_PROCESSING.lock().await;
        if urls.contains(&url) {
            bot.send_message(msg.chat.id, "⏳ This video is already being processed.").await?;
            return Ok(());
        }
        urls.insert(url.clone());
    }

    // Mini App Ad invitation logic
    let is_user_admin = is_admin(&msg).await;
    let is_premium = db_pool.is_user_premium(user_id as i64).await;

    let ads_enabled = {
        let module_enabled = std::env::var("MONETAG_MODULE_ENABLED").map(|v| v.to_lowercase() == "true").unwrap_or(true);
        let global_ads = db_pool.get_setting("ads_enabled").await.map(|val| val == "true").unwrap_or(true);
        let is_test_mode = std::env::var("TEST_MODE").map(|v| v.to_lowercase() == "true").unwrap_or(false);

        if !module_enabled || !global_ads {
            log::info!("Ads disabled globally or by module flag");
            false
        } else if is_user_admin {
            let admin_ads = db_pool.get_setting("admin_ads_enabled").await.map(|val| val == "true").unwrap_or(false);
            log::info!("Ads check for admin: admin_ads={}, test_mode={}", admin_ads, is_test_mode);
            admin_ads || is_test_mode
        } else if is_premium {
            log::info!("Ads disabled: User {} has Premium", user_id);
            false
        } else {
            true
        }
    };

    if ads_enabled {
        let webapp_url = std::env::var("WEBAPP_URL").unwrap_or_default();
        if !webapp_url.is_empty() {
            if let Ok(url_obj) = webapp_url.parse::<reqwest::Url>() {
                let ymid = match db_pool.create_pending_download(user_id as i64, &url).await {
                    Ok(id) => id,
                    Err(e) => {
                        log::error!("Failed to create pending download: {}", e);
                        bot.send_message(msg.chat.id, "❌ Error initializing download.").await?;
                        return Ok(());
                    }
                };

                let mut final_url = url_obj;
                final_url.query_pairs_mut().append_pair("ymid", &ymid);

                let lang = msg.from.as_ref().and_then(|u| u.language_code.as_deref());
                
                let ad_btn_text = get_localized_ad_button_text(lang);
                let prem_btn_text = get_localized_premium_button_text(lang);
                let choice_text = get_localized_choice_text(lang);

                let keyboard = InlineKeyboardMarkup::new(vec![
                    vec![InlineKeyboardButton::web_app(ad_btn_text, WebAppInfo { url: final_url })],
                    vec![InlineKeyboardButton::callback(prem_btn_text, "buy_premium")],
                ]);

                // Send a friendly choice message instead of an invoice
                let _ = bot.send_message(msg.chat.id, choice_text)
                    .reply_markup(keyboard)
                    .await;

                // Stop processing
                {
                    let mut urls = URL_PROCESSING.lock().await;
                    urls.remove(&url);
                }
                return Ok(());
            }
        }
    }

    // Proceed to download
    process_video_request(
        bot,
        user_id as i64,
        url,
        fetcher,
        mtproto_uploader,
        db_pool,
        task_manager,
        upload_semaphore,
        msg.chat.username().map(|s| s.to_string()).or_else(|| msg.from.as_ref().and_then(|u| u.username.clone())),
        msg.chat.id,
    ).await
}

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
    // Get user quality preference
    let quality_preference = db_pool.get_user_quality(user_id).await.unwrap_or_else(|_| "best".to_string());
    let fingerprint = crate::handlers::fingerprint::get_current_fingerprint(db_pool.clone()).await;
    let is_audio = quality_preference == "audio";

    // Acquire upload permit
    let _permit = upload_semaphore.acquire().await.map_err(|e| anyhow::anyhow!("Semaphore error: {}", e))?;

    let subscription_required = get_subscription_required(&db_pool).await.unwrap_or(true);
    if subscription_required {
        if !check_subscription(&bot, user_id).await.unwrap_or(false) {
            let admins: Vec<i64> = std::env::var("ADMIN_IDS").unwrap_or_default()
                .split(',').filter_map(|s| s.trim().parse().ok()).collect();

            if !admins.contains(&user_id) {
                bot.send_message(chat_id, "To use the bot, please subscribe to our channels.").await?;
                {
                    let mut urls = URL_PROCESSING.lock().await;
                    urls.remove(&url);
                }
                return Ok(());
            }
        }
    }

    let mut progress_bar = ProgressBar::new(bot.clone(), chat_id);
    progress_bar.start("🎬 Starting...").await?;
    progress_bar.update(5, Some("⬇️ Downloading...")).await?;

    let mut retries = 0;
    let download_result = loop {
        let file_stem = format!("output/{}", Uuid::new_v4());
        let fut = fetcher.download_video_from_url(url.clone(), &file_stem, &quality_preference, fingerprint.clone(), &mut progress_bar);

        match timeout(DOWNLOAD_TIMEOUT, fut).await {
            Ok(Ok(path)) => break Ok(path),
            Ok(Err(e)) => {
                retries += 1;
                if retries >= 3 { break Err(e); }
                tokio::time::sleep(Duration::from_millis(1000 * 2_u64.pow(retries - 1))).await;
            }
            Err(_) => {
                retries += 1;
                if retries >= 3 { break Err(anyhow::anyhow!("Download timeout")); }
                tokio::time::sleep(Duration::from_millis(1000 * 2_u64.pow(retries - 1))).await;
            }
        }
    };

    let path = match download_result {
        Ok(p) => p,
        Err(e) => {
            progress_bar.delete().await?;
            bot.send_message(chat_id, format!("❌ Error: {}", e)).await?;
            {
                let mut urls = URL_PROCESSING.lock().await;
                urls.remove(&url);
            }
            return Ok(());
        }
    };

    let _guard = TempFileGuard::new(path.clone());
    let file_size = fs::metadata(&path)?.len();

    if file_size > TELEGRAM_BOT_API_FILE_LIMIT {
        progress_bar.update(85, Some("📤 Uploading (Large)...")).await?;
        let res = if is_audio {
            mtproto_uploader.upload_audio(user_id, username, &path, "", &mut progress_bar).await
        } else {
            mtproto_uploader.upload_video(user_id, username, &path, "", &mut progress_bar).await
        };
        if res.is_ok() {
            progress_bar.update(100, Some("✅ Done!")).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            progress_bar.delete().await?;
        }
    } else {
        let mut retries = 0;
        let send_res = loop {
            let res = if is_audio {
                send_audio_with_progress_botapi(&bot.token(), chat_id, &path, None, &mut progress_bar).await
            } else {
                send_video_with_progress_botapi(&bot.token(), chat_id, &path, None, &mut progress_bar).await
            };
            match res {
                Ok(_) => break Ok(()),
                Err(e) => {
                    retries += 1;
                    if retries >= 3 { break Err(e); }
                    tokio::time::sleep(Duration::from_millis(1000 * 2_u64.pow(retries - 1))).await;
                }
            }
        };
        if send_res.is_err() {
            progress_bar.delete().await?;
            bot.send_message(chat_id, "❌ Upload failed.").await?;
        }
    }

    // Final logging
    let video_url = url.clone();
    let _ = db_pool.execute_with_timeout(move |conn| {
        conn.execute("INSERT OR IGNORE INTO users (telegram_id) VALUES (?1)", [user_id])?;
        conn.execute("INSERT INTO downloads (user_telegram_id, video_url) VALUES (?1, ?2)", (user_id, video_url))?;
        Ok(())
    }).await;

    {
        let mut urls = URL_PROCESSING.lock().await;
        urls.remove(&url);
    }
    Ok(())
}
