use regex::Regex;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, WebAppInfo};

use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::{Arc, Mutex};
use tokio::time::Instant;
use tokio::time::{Duration, timeout};
use uuid::Uuid;

use crate::database::DatabasePool;
use crate::handlers::admin::is_admin;
use crate::handlers::subscription::check_subscription;
use crate::handlers::ui::is_menu_button;
use crate::i18n::{self, MsgKey};
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
    static ref URL_PROCESSING: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
}

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes per download attempt
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes per upload attempt
const JOB_BUDGET: Duration = Duration::from_secs(1800); // 30 minutes for the whole job (Tier 2)
const TELEGRAM_BOT_API_FILE_LIMIT: u64 = 48 * 1024 * 1024; // 48MB

/// Owns a URL claim and releases it even when processing exits with an error.
struct UrlProcessingGuard {
    url: String,
}

impl UrlProcessingGuard {
    fn try_acquire(url: &str) -> Option<Self> {
        let mut urls = URL_PROCESSING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if urls.insert(url.to_owned()) {
            Some(Self {
                url: url.to_owned(),
            })
        } else {
            None
        }
    }
}

impl Drop for UrlProcessingGuard {
    fn drop(&mut self) {
        let mut urls = URL_PROCESSING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        urls.remove(&self.url);
    }
}

/// Tier 1: classify a terminal download error for an immediate, localised
/// reply instead of leaking the raw tool output to the user.
fn classify_download_error(err: &anyhow::Error) -> MsgKey {
    let msg = err.to_string().to_lowercase();
    if msg.contains("unsupported url") {
        MsgKey::UnsupportedFormat
    } else if msg.contains("timed out") || msg.contains("timeout") {
        MsgKey::DownloadTimeout
    } else {
        MsgKey::DownloadFailed
    }
}

/// Terminal-failure path: record it, mark the pending row failed, and tell
/// the user in their own language. Internal details stay in the log.
#[allow(clippy::too_many_arguments)]
async fn fail_request(
    bot: &Bot,
    chat_id: ChatId,
    db_pool: &DatabasePool,
    ymid: &Option<String>,
    url: &str,
    err: &anyhow::Error,
    key: MsgKey,
    lang: Option<&str>,
    progress_bar: &mut ProgressBar,
) {
    log::error!("Download job failed for {} (ymid {:?}): {}", url, ymid, err);
    if let Some(id) = ymid {
        if let Err(e) = db_pool.mark_pending_failed(id).await {
            log::error!("Failed to mark {} as failed: {}", id, e);
        }
    }
    let _ = progress_bar.delete().await;
    let _ = bot.send_message(chat_id, i18n::t(key, lang)).await;
}

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

/// Single source of truth for "should this user see the ad flow?".
/// Mirrors the admin-panel toggles: global `ads_enabled` gates regular users,
/// `admin_ads_enabled` (or TEST_MODE) forces ads for admins to test.
/// Used by both the bot handler and the mini-app status endpoint so they
/// can never disagree with each other.
pub async fn ads_enabled_for(db_pool: &DatabasePool, user_id: i64, is_user_admin: bool) -> bool {
    let module_enabled = std::env::var("MONETAG_MODULE_ENABLED").map(|v| v.to_lowercase() == "true").unwrap_or(true);
    let global_ads = db_pool.get_setting("ads_enabled").await.map(|val| val == "true").unwrap_or(true);
    let is_test_mode = std::env::var("TEST_MODE").map(|v| v.to_lowercase() == "true").unwrap_or(false);
    let admin_ads = db_pool.get_setting("admin_ads_enabled").await.map(|val| val == "true").unwrap_or(false);

    if is_user_admin && (admin_ads || is_test_mode) {
        log::info!("Ads enabled for admin (forced by setting or test mode)");
        true
    } else if !module_enabled || !global_ads {
        log::info!("Ads disabled globally or by module flag");
        false
    } else if is_user_admin {
        // Admin but has personal ads OFF and not in test mode
        false
    } else if db_pool.is_user_premium(user_id).await {
        log::info!("Ads disabled: User {} has Premium", user_id);
        false
    } else {
        true
    }
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

    // Effective locale: manual /language override wins, else Telegram tag.
    // Computed before URL parsing: the guide reply below needs it too.
    let tg_lang = msg.from.as_ref().and_then(|u| u.language_code.as_deref());
    let lang = db_pool.get_effective_lang(user_id as i64, tg_lang).await;

    let url = match extract_url_from_text(text) {
        Some(url) => {
            db_pool.log_funnel_event(user_id as i64, "link_received").await;
            url
        }
        None => {
            // Not a link: guide the user instead of staying silent.
            db_pool.log_funnel_event(user_id as i64, "text_no_link").await;
            bot.send_message(msg.chat.id, i18n::t(MsgKey::SendLinkGuide, Some(lang.as_str()))).await?;
            return Ok(());
        }
    };

    // Deduplication. The guard releases the URL on every exit path, including errors.
    let _url_processing_guard = match UrlProcessingGuard::try_acquire(&url) {
        Some(guard) => guard,
        None => {
            bot.send_message(msg.chat.id, i18n::t(MsgKey::AlreadyProcessing, Some(lang.as_str()))).await?;
            return Ok(());
        }
    };

    // Mini App Ad invitation logic (single source of truth, shared with /api/ads-status)
    let is_user_admin = is_admin(&msg).await;
    let ads_enabled = ads_enabled_for(&db_pool, user_id as i64, is_user_admin).await;

    if ads_enabled {
        let webapp_url = std::env::var("WEBAPP_URL").unwrap_or_default();
        if !webapp_url.is_empty() {
            if let Ok(url_obj) = webapp_url.parse::<reqwest::Url>() {
                let ymid = match db_pool.create_pending_download(user_id as i64, &url).await {
                    Ok(id) => id,
                    Err(e) => {
                        log::error!("Failed to create pending download: {}", e);
                        bot.send_message(msg.chat.id, i18n::t(MsgKey::ErrorInitDownload, Some(lang.as_str()))).await?;
                        return Ok(());
                    }
                };

                let mut final_url = url_obj;
                final_url.query_pairs_mut().append_pair("ymid", &ymid);

                let ad_btn_text = i18n::t(MsgKey::AdButton, Some(lang.as_str()));
                let prem_btn_text = i18n::t(MsgKey::PremiumButton, Some(lang.as_str()));
                let choice_text = i18n::t(MsgKey::ChoiceText, Some(lang.as_str()));

                let keyboard = InlineKeyboardMarkup::new(vec![
                    vec![InlineKeyboardButton::web_app(ad_btn_text, WebAppInfo { url: final_url })],
                    vec![InlineKeyboardButton::callback(prem_btn_text, "buy_premium")],
                ]);

                // Send a friendly choice message instead of an invoice
                let _ = bot.send_message(msg.chat.id, choice_text)
                    .reply_markup(keyboard)
                    .await;

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
        Some(lang),
        None,
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
    lang: Option<String>,
    ymid: Option<String>,
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
                bot.send_message(chat_id, i18n::t(MsgKey::SubscribeRequired, lang.as_deref())).await?;
                return Ok(());
            }
        }
    }

    let mut progress_bar = ProgressBar::new(bot.clone(), chat_id);
    progress_bar.start("🎬 Starting...").await?;
    progress_bar.update(5, Some("⬇️ Downloading...")).await?;

    // Whole-job budget (Tier 2): download attempts + upload must finish
    // within 30 minutes, otherwise the job is dropped and the user is told
    // to retry. Per-attempt DOWNLOAD_TIMEOUT / UPLOAD_TIMEOUT still apply.
    let job = async {
        // TikTok photo posts (carousels): yt-dlp rejects /photo/ URLs, so
        // probe tikwm once first (it also resolves vt/vM short links).
        // Instagram/YouTube skip the probe and go straight to yt-dlp.
        if url.contains("tiktok.com") {
            let meta = fetcher.probe_tikwm(&url).await;
            if !meta.images.is_empty() {
                let stem = format!("output/{}", Uuid::new_v4());
                crate::handlers::photo::handle_photo_post(
                    &bot, chat_id, &fetcher, &meta, &stem, is_audio, &mut progress_bar,
                )
                .await?;
                progress_bar.update(100, Some("✅ Done!")).await?;
                tokio::time::sleep(Duration::from_millis(500)).await;
                progress_bar.delete().await?;
                return Ok::<(), anyhow::Error>(());
            }
        }

        let mut retries = 0;
        let path = loop {
            let file_stem = format!("output/{}", Uuid::new_v4());
            let fut = fetcher.download_video_from_url(url.clone(), &file_stem, &quality_preference, fingerprint.clone(), &mut progress_bar);

            match timeout(DOWNLOAD_TIMEOUT, fut).await {
                Ok(Ok(path)) => break path,
                Ok(Err(e)) => {
                    retries += 1;
                    if retries >= 3 {
                        return Err(e);
                    }
                    tokio::time::sleep(Duration::from_millis(1000 * 2_u64.pow(retries - 1))).await;
                }
                Err(_) => {
                    retries += 1;
                    if retries >= 3 {
                        return Err(anyhow::anyhow!("Download timeout"));
                    }
                    tokio::time::sleep(Duration::from_millis(1000 * 2_u64.pow(retries - 1))).await;
                }
            }
        };

        let _guard = TempFileGuard::new(path.clone());
        let file_size = fs::metadata(&path)?.len();

        if file_size > TELEGRAM_BOT_API_FILE_LIMIT {
            progress_bar.update(85, Some("📤 Uploading (Large)...")).await?;
            let upload_result = match timeout(UPLOAD_TIMEOUT, async {
                if is_audio {
                    mtproto_uploader.upload_audio(user_id, username, &path, "", &mut progress_bar).await
                } else {
                    mtproto_uploader.upload_video(user_id, username, &path, "", &mut progress_bar).await
                }
            }).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(anyhow::anyhow!("Large upload failed: {}", e)),
                Err(_) => Err(anyhow::anyhow!(
                    "Large upload timed out after {} seconds",
                    UPLOAD_TIMEOUT.as_secs()
                )),
            };

            match upload_result {
                Ok(()) => {
                    progress_bar.update(100, Some("✅ Done!")).await?;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    progress_bar.delete().await?;
                }
                Err(e) => {
                    return Err(e);
                }
            }
        } else {
            let send_res = match timeout(UPLOAD_TIMEOUT, async {
                let mut retries = 0;
                loop {
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
                }
            }).await {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!(
                    "Upload timed out after {} seconds",
                    UPLOAD_TIMEOUT.as_secs()
                )),
            };
            if let Err(e) = send_res {
                return Err(e);
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    match timeout(JOB_BUDGET, job).await {
        Ok(Ok(())) => {
            // Final logging (success only: failed uploads no longer count).
            let video_url = url.clone();
            let _ = db_pool.execute_with_timeout(move |conn| {
                conn.execute("INSERT OR IGNORE INTO users (telegram_id) VALUES (?1)", [user_id])?;
                conn.execute("INSERT INTO downloads (user_telegram_id, video_url) VALUES (?1, ?2)", (user_id, video_url))?;
                Ok(())
            }).await;

            Ok(())
        }
        Ok(Err(e)) => {
            // Tier 1: the bot knows why it failed — say so immediately.
            let key = classify_download_error(&e);
            fail_request(&bot, chat_id, &db_pool, &ymid, &url, &e, key, lang.as_deref(), &mut progress_bar).await;
            Ok(())
        }
        Err(_) => {
            // Tier 2: something unforeseen hung the job — the budget killed
            // it, tell the user to retry.
            let e = anyhow::anyhow!("Job budget of {} seconds exceeded", JOB_BUDGET.as_secs());
            fail_request(&bot, chat_id, &db_pool, &ymid, &url, &e, MsgKey::DownloadTimeout, lang.as_deref(), &mut progress_bar).await;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_errors_classified_for_immediate_reply() {
        let unsupported = anyhow::anyhow!("yt-dlp failed: ERROR: Unsupported URL: https://www.tiktok.com/@u/photo/1");
        assert_eq!(classify_download_error(&unsupported), crate::i18n::MsgKey::UnsupportedFormat);

        let timed_out = anyhow::anyhow!("Download timeout");
        assert_eq!(classify_download_error(&timed_out), crate::i18n::MsgKey::DownloadTimeout);

        let upload_timed_out = anyhow::anyhow!("Upload timed out after 300 seconds");
        assert_eq!(classify_download_error(&upload_timed_out), crate::i18n::MsgKey::DownloadTimeout);

        let generic = anyhow::anyhow!("yt-dlp failed: ERROR: Video unavailable");
        assert_eq!(classify_download_error(&generic), crate::i18n::MsgKey::DownloadFailed);
    }

    async fn ads_test_db() -> (DatabasePool, tempfile::NamedTempFile) {
        let temp_file = tempfile::NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap().to_string();
        let pool = DatabasePool::new(db_path, 1);
        pool.execute_with_timeout(|conn| {
            conn.execute(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, telegram_id BIGINT UNIQUE NOT NULL, last_active DATETIME DEFAULT CURRENT_TIMESTAMP, created_at DATETIME DEFAULT CURRENT_TIMESTAMP, quality_preference TEXT DEFAULT 'h264', premium_until DATETIME, lang TEXT DEFAULT NULL, ref_code TEXT DEFAULT NULL)",
                (),
            )?;
            conn.execute(
                "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                (),
            )?;
            conn.execute(
                "CREATE TABLE pending_downloads (id TEXT PRIMARY KEY, user_id BIGINT NOT NULL, video_url TEXT NOT NULL, status TEXT DEFAULT 'pending', created_at DATETIME DEFAULT CURRENT_TIMESTAMP, notified_at DATETIME DEFAULT NULL)",
                (),
            )?;
            Ok(())
        })
        .await
        .unwrap();
        (pool, temp_file)
    }

    #[tokio::test]
    async fn ads_decision_matrix() {
        let (pool, _file) = ads_test_db().await;
        unsafe {
            std::env::set_var("ADMIN_IDS", "999");
            std::env::set_var("MONETAG_MODULE_ENABLED", "true");
            std::env::remove_var("TEST_MODE");
        }
        pool.set_setting("ads_enabled", "true").await.unwrap();
        pool.set_setting("admin_ads_enabled", "false").await.unwrap();

        // Regular user, ads globally on -> sees ads.
        assert!(ads_enabled_for(&pool, 111, false).await);
        // Admin, global on, no override -> skips ads.
        assert!(!ads_enabled_for(&pool, 999, true).await);
        // Premium user, global on -> skips ads.
        pool.set_user_premium(222, 30).await.unwrap();
        assert!(!ads_enabled_for(&pool, 222, false).await);

        // Global OFF: regular users see nothing...
        pool.set_setting("ads_enabled", "false").await.unwrap();
        assert!(!ads_enabled_for(&pool, 111, false).await);
        // ...but an admin with Admin Ads ON gets the test flow (reported scenario).
        pool.set_setting("admin_ads_enabled", "true").await.unwrap();
        assert!(ads_enabled_for(&pool, 999, true).await);
        // Admin without the override still skips.
        pool.set_setting("admin_ads_enabled", "false").await.unwrap();
        assert!(!ads_enabled_for(&pool, 999, true).await);

        // ymid resolves to the requesting user (what /api/ads-status uses).
        let ymid = pool
            .create_pending_download(111, "https://vt.tiktok.com/x")
            .await
            .unwrap();
        assert_eq!(pool.get_user_id_by_ymid(&ymid).await.unwrap(), 111);

        unsafe {
            std::env::remove_var("ADMIN_IDS");
            std::env::remove_var("MONETAG_MODULE_ENABLED");
        }
    }


    #[test]
    fn processing_guard_allows_retry_after_previous_request_finishes() {
        let url = format!("https://vt.tiktok.com/test-{}", Uuid::new_v4());
        let guard = UrlProcessingGuard::try_acquire(&url)
            .expect("first request should claim the URL");

        assert!(
            UrlProcessingGuard::try_acquire(&url).is_none(),
            "a duplicate request must be rejected while the first request is active"
        );

        drop(guard);

        assert!(
            UrlProcessingGuard::try_acquire(&url).is_some(),
            "the URL must be claimable again after the first request exits"
        );
    }
}
