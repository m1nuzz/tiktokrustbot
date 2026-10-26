use regex::Regex;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, WebAppInfo};

use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::{Arc, Mutex};
use tokio::time::Instant;
use tokio::time::{Duration, timeout};
use uuid::Uuid;

use crate::BotCtx;
use crate::database::DatabasePool;
use crate::handlers::admin::{is_admin, is_admin_id};
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
    /// Pending YouTube quality picks: inline-keyboard session id -> request.
    /// In-memory only: a restart drops pending menus (the user just re-sends
    /// the link), and nothing in pending_downloads is reserved for them.
    static ref YT_PICKS: Mutex<HashMap<String, YtPick>> = Mutex::new(HashMap::new());
}

/// One outstanding YouTube quality menu. `quality_override` semantics live in
/// `process_video_request`; the numeric height ("1080") or "audio" travels in
/// the callback data itself, and this struct only reproduces the chat context.
struct YtPick {
    url: String,
    bot_id: String,
    user_id: i64,
    username: Option<String>,
    chat_id: i64,
    lang: Option<String>,
    created_at: std::time::Instant,
}

impl YtPick {
    /// Menus older than this are dropped lazily on the next insert/lookup;
    /// the inline buttons stay tappable (the callback answers with
    /// "session expired" instead of a silent stop).
    const TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

    fn is_fresh(&self) -> bool {
        self.created_at.elapsed() < Self::TTL
    }
}

/// One queued link message plus every dependency the handler needs. Ordering
/// lives in `lib.rs`; this struct only carries the request context.
#[derive(Clone)]
pub struct LinkJob {
    pub bot: Bot,
    pub msg: Message,
    pub ctx: BotCtx,
    pub fetcher: Arc<YoutubeFetcher>,
    pub mtproto_uploader: Arc<MTProtoUploader>,
    pub db_pool: Arc<DatabasePool>,
    pub task_manager: Arc<tokio::sync::Mutex<TaskManager>>,
    pub upload_semaphore: Arc<tokio::sync::Semaphore>,
    pub download_semaphore: Arc<tokio::sync::Semaphore>,
    pub dedupe_key: String,
}

/// Position of one link inside a burst of forwarded messages. `position` is
/// 1-based and follows `Message.message_id` ascending (top message first).
#[derive(Clone, Debug)]
pub struct BatchInfo {
    pub id: String,
    pub position: i64,
    pub total: i64,
    pub source_message_id: i64,
}

/// How the current batch item finished. The ad-choice waiter uses this to
/// replace its own message with a short status line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchOutcome {
    Delivered,
    Skipped,
    Failed,
}

/// ymid -> waiter for the link handler that is currently blocked on the ad
/// choice message. Only the ad flow registers here; premium/admin/direct
/// downloads never wait.
lazy_static::lazy_static! {
    static ref BATCH_WAITERS: Mutex<HashMap<String, tokio::sync::oneshot::Sender<BatchOutcome>>> =
        Mutex::new(HashMap::new());
    /// batch_id -> notifier used to wake rows waiting for earlier items.
    static ref BATCH_NOTIFIERS: Mutex<HashMap<String, Arc<tokio::sync::Notify>>> =
        Mutex::new(HashMap::new());
    /// ymid -> batch_id, so a finishing row can notify the right batch.
    static ref YMID_BATCH: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());
}

/// Fill the `{current}` / `{total}` placeholders in a batch status string.
/// `i18n::t` returns `&'static str`, so the substitution must allocate.
fn batch_text(key: MsgKey, lang: Option<&str>, position: i64, total: i64) -> String {
    i18n::t(key, lang)
        .replace("{current}", &position.to_string())
        .replace("{total}", &total.to_string())
}

fn remove_batch_waiter(ymid: &str) {
    BATCH_WAITERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(ymid);
}

fn is_batch_waiting(ymid: &str) -> bool {
    BATCH_WAITERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains_key(ymid)
}

/// Safety net for early `?` returns inside [`process_video_request`] (for
/// example a failed progress-bar send). The normal success/failure paths
/// notify first and remove the waiter, so this drop-time Failed is a no-op
/// there; it only matters when the job exits before reaching either path.
struct BatchFailureGuard(Option<String>);

impl Drop for BatchFailureGuard {
    fn drop(&mut self) {
        notify_batch_outcome(self.0.as_deref(), BatchOutcome::Failed);
    }
}

/// Wake the link handler that is waiting on this row. No-op for direct
/// downloads (`None`) and for rows that never entered the ad flow.
/// Get or create the notifier for a batch. All rows of the batch share it.
fn batch_notifier(batch_id: &str) -> Arc<tokio::sync::Notify> {
    let mut map = BATCH_NOTIFIERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.entry(batch_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
        .clone()
}

/// Remember which batch a ymid belongs to, so its completion can wake waiters.
fn register_batch_ymid(ymid: &str, batch_id: &str) {
    YMID_BATCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(ymid.to_string(), batch_id.to_string());
}

/// Wake the link handler that is waiting on this row. No-op for direct
/// downloads (`None`) and for rows that never entered the ad flow.
pub fn notify_batch_outcome(ymid: Option<&str>, outcome: BatchOutcome) {
    let Some(id) = ymid else {
        return;
    };
    let waiter = BATCH_WAITERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(id);
    if let Some(tx) = waiter {
        let _ = tx.send(outcome);
    }

    // Event-driven wake-up for batch ordering. Look up the batch for this ymid
    // and notify all rows waiting on it. Clean up the mapping afterwards.
    let batch_id = YMID_BATCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(id);
    if let Some(batch_id) = batch_id {
        let notifier = batch_notifier(&batch_id);
        notifier.notify_waiters();
        // Best-effort cleanup of the notifier once the batch is finished.
        // We cannot easily know here whether other rows remain, so keep it;
        // memory growth is bounded by the number of in-flight batches.
    }
}

/// Evicts stale YT picks; called with the map lock already held.
fn prune_stale_yt_picks(map: &mut HashMap<String, YtPick>) {
    map.retain(|_, pick| pick.is_fresh());
}

/// True when the parsed URL is a YouTube link (the only platform served with
/// the quality picker). TikTok and Instagram keep the ad-flow / default path.
fn is_youtube_url(url: &str) -> bool {
    url.contains("youtube.com/") || url.contains("youtu.be/")
}

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes per download attempt
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes per upload attempt
/// Wall-clock ceiling for one download+upload job. It must end strictly
/// before the session lease window (`SESSION_LEASE_SECS`), so a job that is
/// going to fail fails on its own terms while its row is still unambiguous,
/// instead of racing the sweeper into deciding the same row's fate twice.
///
/// Headroom: 5 minutes (1500s against a 1800s lease). User-visible cost: a
/// download that would have finished at minute 26 now fails at minute 25. That
/// is the deliberate trade for one notification per row - the alternative was
/// a user getting both a "download failed" and an "ad session expired" for the
/// same video.
const JOB_BUDGET_SECS: u64 = 1500;
const JOB_BUDGET: Duration = Duration::from_secs(JOB_BUDGET_SECS);
/// Safety net for a batch item whose ad choice is never answered. The normal
/// exit is the explicit "Skip this video" button; this only keeps one abandoned
/// chat from pinning its FIFO queue forever. 30 minutes is longer than the job
/// budget, so a job that did start always gets to report its own outcome.
const BATCH_WAIT_TIMEOUT: Duration = Duration::from_secs(1800);
const _: () = assert!(
    JOB_BUDGET_SECS < crate::database::SESSION_LEASE_SECS as u64,
    "the job budget must end before the session lease window"
);
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
fn classify_download_error(err: &anyhow::Error, url: &str) -> MsgKey {
    let msg = err.to_string().to_lowercase();
    if msg.contains("unsupported url") {
        return MsgKey::UnsupportedFormat;
    }
    if msg.contains("timed out") || msg.contains("timeout") {
        return MsgKey::DownloadTimeout;
    }
    // Platform-level blocks: yt-dlp surfaces them as bot checks, 403s and
    // rate limits, not as "this video is gone". Tell the user the platform is
    // temporarily unavailable instead of implying the link itself is broken.
    let blocked = msg.contains("sign in to confirm")
        || msg.contains("not a bot")
        || msg.contains("captcha")
        || msg.contains("http error 403")
        || msg.contains("403: forbidden")
        || msg.contains("429")
        || msg.contains("too many requests")
        || msg.contains("rate limit")
        || msg.contains("rate-limit")
        || msg.contains("login required")
        || msg.contains("checkpoint");
    if blocked && (url.contains("youtube.com/") || url.contains("youtu.be/")) {
        return MsgKey::YtBlocked;
    }
    if blocked && url.contains("instagram.com/") {
        return MsgKey::IgBlocked;
    }
    MsgKey::DownloadFailed
}

/// Which message an expired session earns.
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
    let batch_row = ymid.as_deref().map(is_batch_waiting).unwrap_or(false);
    if let Some(id) = ymid {
        match db_pool.mark_pending_failed(id).await {
            // 0 rows means the row was already terminal: somebody else owns
            // this row's single notification, so do not log a success here.
            Ok(0) => log::warn!("Row {} was already terminal, not re-marked failed", id),
            Ok(_) => {}
            Err(e) => log::error!("Failed to mark {} as failed: {}", id, e),
        }
    }
    let _ = progress_bar.delete().await;
    // A batch row already has a choice message: the waiter edits that message
    // to "❌ Видео N из M — не удалось скачать" instead of sending a second
    // generic failure bubble.
    if !batch_row {
        let _ = bot.send_message(chat_id, i18n::t(key, lang)).await;
    }
    notify_batch_outcome(ymid.as_deref(), BatchOutcome::Failed);
}

// Add this function at the beginning of the file
fn extract_url_from_text(text: &str) -> Option<String> {
    // Regex for searching TikTok, Instagram or YouTube URL
    let tiktok_re = Regex::new(r"https?://(?:www\.|vm\.|vt\.)?tiktok\.com/[^\s]+").unwrap();
    let instagram_re =
        Regex::new(r"https?://(?:www\.)?instagram\.com/(?:reels?|p|tv)/[^\s]+").unwrap();
    let youtube_re = Regex::new(
        r"https?://(?:www\.)?(?:youtube\.com/shorts/|youtube\.com/watch\?v=|youtu\.be/)[^\s]+",
    )
    .unwrap();

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

/// Single source of truth for "should this user see the ad flow?".
/// Mirrors the admin-panel toggles: global `ads_enabled` gates regular users,
/// `admin_ads_enabled` (or TEST_MODE) forces ads for admins to test.
/// Used by both the bot handler and the mini-app status endpoint so they
/// can never disagree with each other.
pub async fn ads_enabled_for(
    db_pool: &DatabasePool,
    bot_id: &str,
    user_id: i64,
    is_user_admin: bool,
) -> bool {
    let module_enabled = std::env::var("MONETAG_MODULE_ENABLED")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(true);
    let global_ads = db_pool
        .get_setting("ads_enabled")
        .await
        .map(|val| val == "true")
        .unwrap_or(true);
    let is_test_mode = std::env::var("TEST_MODE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let admin_ads = db_pool
        .get_setting("admin_ads_enabled")
        .await
        .map(|val| val == "true")
        .unwrap_or(false);

    if is_user_admin && (admin_ads || is_test_mode) {
        log::info!("Ads enabled for admin (forced by setting or test mode)");
        true
    } else if !module_enabled || !global_ads {
        log::info!("Ads disabled globally or by module flag");
        false
    } else if is_user_admin {
        // Admin but has personal ads OFF and not in test mode
        false
    } else if db_pool.is_user_premium(bot_id, user_id).await {
        log::info!("Ads disabled: User {} has Premium", user_id);
        false
    } else {
        true
    }
}

pub async fn link_handler(
    job: LinkJob,
    batch: Option<BatchInfo>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let LinkJob {
        bot,
        msg,
        ctx,
        fetcher,
        mtproto_uploader,
        db_pool,
        task_manager,
        upload_semaphore,
        download_semaphore,
        ..
    } = job;
    let user_id = msg.chat.id.0;
    let bot_id = ctx.bot_id.as_str();

    // Update user activity (per-bot row: identity is (bot_id, telegram_id)).
    let bot_id_owned = ctx.bot_id.clone();
    let _ = db_pool.execute_with_timeout(move |conn| {
        conn.execute("INSERT OR IGNORE INTO users (bot_id, telegram_id) VALUES (?1, ?2)", rusqlite::params![bot_id_owned, user_id])?;
        conn.execute("UPDATE users SET last_active = CURRENT_TIMESTAMP WHERE bot_id = ?1 AND telegram_id = ?2", rusqlite::params![bot_id_owned, user_id])?;
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
    let lang = db_pool
        .get_effective_lang(bot_id, user_id as i64, tg_lang)
        .await;

    let url = match extract_url_from_text(text) {
        Some(url) => {
            db_pool
                .log_funnel_event(bot_id, user_id as i64, "link_received")
                .await;
            url
        }
        None => {
            // Not a link: guide the user instead of staying silent.
            db_pool
                .log_funnel_event(bot_id, user_id as i64, "text_no_link")
                .await;
            bot.send_message(
                msg.chat.id,
                i18n::t(MsgKey::SendLinkGuide, Some(lang.as_str())),
            )
            .await?;
            return Ok(());
        }
    };

    // Subscription gate runs before the ad choice, the YouTube picker and any
    // pending row. In a burst only the first item speaks, so the user gets one
    // message instead of one per link; the rest return silently.
    let is_user_admin = is_admin(&msg).await;
    if !check_subscription(&bot, &db_pool, user_id as i64, is_user_admin)
        .await
        .unwrap_or(true)
    {
        if batch.as_ref().map_or(true, |info| info.position == 1) {
            bot.send_message(
                msg.chat.id,
                i18n::t(MsgKey::SubscribeRequired, Some(lang.as_str())),
            )
            .await?;
        }
        return Ok(());
    }

    // Deduplication. The guard releases the URL on every exit path, including errors.
    let _url_processing_guard = match UrlProcessingGuard::try_acquire(&url) {
        Some(guard) => guard,
        None => {
            bot.send_message(
                msg.chat.id,
                i18n::t(MsgKey::AlreadyProcessing, Some(lang.as_str())),
            )
            .await?;
            return Ok(());
        }
    };

    // YouTube quality picker: probe available heights and show buttons.
    // Not routed through the Monetag mini-app: the SDK only pays for TikTok/IG
    // traffic, so an ad session here would make the user watch ads for money
    // the funnel never receives. The chosen quality rides in the callback data.
    if is_youtube_url(&url) {
        let heights = fetcher.probe_youtube_heights(&url).await;
        if !heights.is_empty() {
            let key = {
                let mut picks = YT_PICKS
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                prune_stale_yt_picks(&mut picks);

                let key = Uuid::new_v4().to_string();
                picks.insert(
                    key.clone(),
                    YtPick {
                        url: url.clone(),
                        bot_id: bot_id.to_string(),
                        user_id: user_id as i64,
                        username: msg
                            .chat
                            .username()
                            .map(|s| s.to_string())
                            .or_else(|| msg.from.as_ref().and_then(|u| u.username.clone())),
                        chat_id: msg.chat.id.0,
                        lang: Some(lang.clone()),
                        created_at: std::time::Instant::now(),
                    },
                );
                key
            };

            // Buttons run top-down from the tallest height; climbs beyond
            // eight rows look broken in a chat window, so cap the menu there.
            let mut rows: Vec<Vec<InlineKeyboardButton>> = heights
                .iter()
                .rev()
                .take(8)
                .map(|h| {
                    vec![InlineKeyboardButton::callback(
                        format!("{}p", h),
                        format!("ytq|{}|{}", key, h),
                    )]
                })
                .collect();
            rows.push(vec![InlineKeyboardButton::callback(
                i18n::t(MsgKey::YtAudioOnly, Some(lang.as_str())),
                format!("yta|{}", key),
            )]);

            bot.send_message(
                msg.chat.id,
                i18n::t(MsgKey::YtQualityPick, Some(lang.as_str())),
            )
            .reply_markup(InlineKeyboardMarkup::new(rows))
            .await?;
            return Ok(());
        }
        // Probe failed (private/live streams etc.): default download flow.
    }

    // Mini App Ad invitation logic (single source of truth, shared with /api/ads-status)
    let ads_enabled = ads_enabled_for(&db_pool, bot_id, user_id as i64, is_user_admin).await;

    if ads_enabled {
        let webapp_url = std::env::var("WEBAPP_URL").unwrap_or_default();
        if !webapp_url.is_empty() {
            if let Ok(url_obj) = webapp_url.parse::<reqwest::Url>() {
                let ymid = match db_pool
                    .create_pending_download_with_batch(
                        bot_id,
                        user_id as i64,
                        &url,
                        batch.as_ref().map(|b| b.id.as_str()),
                        batch.as_ref().map(|b| b.position),
                        batch.as_ref().map(|b| b.total),
                    )
                    .await
                {
                    Ok(id) => id,
                    Err(e) => {
                        log::error!("Failed to create pending download: {}", e);
                        bot.send_message(
                            msg.chat.id,
                            i18n::t(MsgKey::ErrorInitDownload, Some(lang.as_str())),
                        )
                        .await?;
                        return Ok(());
                    }
                };

                if let Some(info) = batch.as_ref() {
                    register_batch_ymid(&ymid, &info.id);
                }

                let mut final_url = url_obj;
                final_url.query_pairs_mut().append_pair("ymid", &ymid);
                // The mini-app localises itself server-side from this tag
                // (stored override wins, else the tag already resolved here).
                final_url.query_pairs_mut().append_pair("lang", &lang);

                let ad_btn_text = i18n::t(MsgKey::AdButton, Some(lang.as_str()));
                let prem_btn_text = i18n::t(MsgKey::PremiumButton, Some(lang.as_str()));
                let choice_text = i18n::t(MsgKey::ChoiceText, Some(lang.as_str()));

                let mut rows = vec![
                    vec![InlineKeyboardButton::web_app(
                        ad_btn_text,
                        WebAppInfo { url: final_url },
                    )],
                    vec![InlineKeyboardButton::callback(prem_btn_text, "buy_premium")],
                ];
                if batch.is_some() {
                    rows.push(vec![InlineKeyboardButton::callback(
                        i18n::t(MsgKey::BatchSkipButton, Some(lang.as_str())),
                        format!("batch_skip|{}", ymid),
                    )]);
                }

                let message_text = match &batch {
                    Some(info) => format!(
                        "{}\n\n{}",
                        batch_text(
                            MsgKey::BatchProgress,
                            Some(lang.as_str()),
                            info.position,
                            info.total
                        ),
                        choice_text
                    ),
                    None => choice_text.to_string(),
                };

                // Register before sending: a very fast tap must find the waiter.
                let waiter = if batch.is_some() {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    BATCH_WAITERS
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(ymid.clone(), tx);
                    Some(rx)
                } else {
                    None
                };

                let sent = match bot
                    .send_message(msg.chat.id, message_text)
                    .reply_markup(InlineKeyboardMarkup::new(rows))
                    .await
                {
                    Ok(sent) => sent,
                    Err(e) => {
                        remove_batch_waiter(&ymid);
                        return Err(e.into());
                    }
                };

                // A real burst is handled strictly one item at a time: the next
                // choice message only appears after this one is delivered,
                // skipped or failed. Single links keep the old immediate flow.
                if let (Some(info), Some(waiter)) = (batch, waiter) {
                    let outcome = match timeout(BATCH_WAIT_TIMEOUT, waiter).await {
                        Ok(Ok(outcome)) => outcome,
                        Ok(Err(_)) => BatchOutcome::Failed,
                        Err(_) => {
                            let _ = db_pool.mark_pending_skipped(&ymid).await;
                            BatchOutcome::Skipped
                        }
                    };
                    remove_batch_waiter(&ymid);

                    let status_key = match outcome {
                        BatchOutcome::Delivered => MsgKey::BatchDelivered,
                        BatchOutcome::Skipped => MsgKey::BatchSkipped,
                        BatchOutcome::Failed => MsgKey::BatchFailed,
                    };
                    let status_text =
                        batch_text(status_key, Some(lang.as_str()), info.position, info.total);
                    let _ = bot
                        .edit_message_text(sent.chat.id, sent.id, status_text)
                        .await;
                    // Remove the ad/premium/skip buttons from the finished item.
                    let _ = bot.edit_message_reply_markup(sent.chat.id, sent.id).await;
                }

                return Ok(());
            }
        }
    }

    // Proceed to download
    process_video_request(
        bot,
        bot_id,
        user_id as i64,
        url,
        fetcher,
        mtproto_uploader,
        db_pool,
        task_manager,
        upload_semaphore,
        download_semaphore,
        msg.chat
            .username()
            .map(|s| s.to_string())
            .or_else(|| msg.from.as_ref().and_then(|u| u.username.clone())),
        msg.chat.id,
        Some(lang),
        None,
        None,
    )
    .await
}

/// Handle the per-item "Skip this video" button. Only a still-pending row can
/// be skipped: once an ad impression verified it or a claim completed it, the
/// download is already in flight and the waiter will get its own outcome.
pub async fn handle_batch_skip(
    bot: Bot,
    q: CallbackQuery,
    db_pool: Arc<DatabasePool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let data = q.data.clone().unwrap_or_default();
    let Some(ymid) = data.strip_prefix("batch_skip|").map(|s| s.to_string()) else {
        return Ok(());
    };

    let _ = bot.answer_callback_query(q.id).await;
    match db_pool.mark_pending_skipped(&ymid).await {
        Ok(rows) if rows > 0 => notify_batch_outcome(Some(ymid.as_str()), BatchOutcome::Skipped),
        Ok(_) => {
            // Already verified/completed: the ad was watched or the download
            // started, so the skip would race the delivery. Let it finish.
        }
        Err(e) => log::error!("Failed to skip batch row {}: {}", ymid, e),
    }
    Ok(())
}

/// Handles a tap on one of the YouTube quality-picker inline buttons.
///
/// The menu stores its request in [`YT_PICKS`] keyed by a UUID; the callback
/// data carries only that key plus the chosen height (or the audio marker).
/// A tap consumes the pick: one menu can be answered once, and a stale pick
/// (restart, 30-minute TTL, pruned map) just answers with the expiry text.
pub async fn handle_yt_pick_callback(
    bot: Bot,
    q: CallbackQuery,
    _ctx: BotCtx,
    fetcher: Arc<YoutubeFetcher>,
    mtproto_uploader: Arc<MTProtoUploader>,
    db_pool: Arc<DatabasePool>,
    task_manager: Arc<tokio::sync::Mutex<TaskManager>>,
    upload_semaphore: Arc<tokio::sync::Semaphore>,
    download_semaphore: Arc<tokio::sync::Semaphore>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = bot.answer_callback_query(q.id.clone()).await;

    let Some(data) = q.data.as_deref() else {
        return Ok(());
    };
    let quality = if data.starts_with("ytq|") {
        data.strip_prefix("ytq|")
            .and_then(|rest| rest.split('|').nth(1))
            .map(|h| h.to_string())
    } else if data.starts_with("yta|") {
        Some("audio".to_string())
    } else {
        None
    };
    let key = data
        .strip_prefix("ytq|")
        .or_else(|| data.strip_prefix("yta|"))
        .map(|rest| rest.split('|').next().unwrap_or(rest).to_string());

    let (Some(quality), Some(key)) = (quality, key) else {
        log::warn!("YT pick callback with unexpected data: {}", data);
        return Ok(());
    };

    let pick = {
        let mut picks = YT_PICKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_stale_yt_picks(&mut picks);
        picks.remove(&key)
    };
    let Some(pick) = pick.filter(YtPick::is_fresh) else {
        // The menu only lives in memory; a restart, the 30-minute TTL or a
        // foreign bot process dropped it, so there is nothing to deliver.
        log::info!("YT pick {} missing or stale; ignoring tap", key);
        return Ok(());
    };

    log::info!(
        "YT pick accepted: user {} -> {} (quality {})",
        pick.user_id,
        pick.url,
        quality
    );

    process_video_request(
        bot,
        &pick.bot_id,
        pick.user_id,
        pick.url.clone(),
        fetcher,
        mtproto_uploader,
        db_pool,
        task_manager,
        upload_semaphore,
        download_semaphore,
        pick.username.clone(),
        ChatId(pick.chat_id),
        pick.lang.clone(),
        None,
        Some(quality),
    )
    .await
}

/// Delete partial files of one failed download attempt. Stems are per-attempt
/// UUIDs, so a prefix match can only hit this attempt's files; unknown output
/// names (yt-dlp extensions) are why this globs instead of removing one path.
async fn remove_attempt_partials(file_stem: &str) {
    let stem = file_stem.strip_prefix("output/").unwrap_or(file_stem);
    // The bot runs under systemd with cwd = "/", so a relative "output" used
    // to read the filesystem root and match nothing; resolve the real dir
    // from the executable's location instead.
    let output_dir = crate::yt_dlp_interface::fetcher::default_downloads_dir().join("output");
    let Ok(mut dir) = tokio::fs::read_dir(&output_dir).await else {
        return;
    };
    while let Ok(Some(entry)) = dir.next_entry().await {
        if entry.file_name().to_string_lossy().starts_with(stem) {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

pub async fn process_video_request(
    bot: Bot,
    bot_id: &str,
    user_id: i64,
    url: String,
    fetcher: Arc<YoutubeFetcher>,
    mtproto_uploader: Arc<MTProtoUploader>,
    db_pool: Arc<DatabasePool>,
    _task_manager: Arc<tokio::sync::Mutex<TaskManager>>,
    upload_semaphore: Arc<tokio::sync::Semaphore>,
    download_semaphore: Arc<tokio::sync::Semaphore>,
    username: Option<String>,
    chat_id: ChatId,
    lang: Option<String>,
    ymid: Option<String>,
    quality_override: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // If this is a batch row, every exit path must wake the ad-choice waiter.
    // Success and fail_request notify first; this guard covers early `?` exits.
    let _batch_failure_guard = BatchFailureGuard(ymid.clone());

    // Acknowledge immediately so the user sees feedback while the burst is
    // still being collected / the job waits for its turn.
    let mut progress_bar = ProgressBar::new(bot.clone(), chat_id);
    log::info!("Sending Starting... for user {}", user_id);
    progress_bar.start("🎬 Starting...").await?;

    // A burst is delivered in message order even when several of its videos
    // are claimed at once: wait here until every earlier row of the same
    // burst has finished (completed/failed/expired) before sending this one.
    if let Some(ymid) = ymid.as_deref() {
        if let Ok(Some((batch_id, position, _total))) = db_pool.get_batch_info(ymid).await {
            let notifier = batch_notifier(&batch_id);
            let deadline = std::time::Instant::now() + Duration::from_secs(1800);
            while db_pool
                .earlier_downloads_unfinished(&batch_id, position)
                .await
                .unwrap_or(false)
            {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                let notified = notifier.notified();
                if !db_pool
                    .earlier_downloads_unfinished(&batch_id, position)
                    .await
                    .unwrap_or(false)
                {
                    break;
                }
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                }
            }
        }
    }

    // Get user quality preference: the YT picker callbacks pass an explicit
    // numeric height ("1080") or "audio" instead of the stored default.
    let quality_preference = match quality_override {
        Some(q) => q,
        None => db_pool
            .get_user_quality(bot_id, user_id)
            .await
            .unwrap_or_else(|_| "best".to_string()),
    };
    let fingerprint = crate::handlers::fingerprint::get_current_fingerprint(db_pool.clone()).await;
    let is_audio = quality_preference == "audio";

    // Acquire download permit
    let _download_permit = download_semaphore
        .acquire()
        .await
        .map_err(|e| anyhow::anyhow!("Semaphore error: {}", e))?;
    if let Some(ymid) = ymid.as_deref() {
        let _ = db_pool.mark_pending_downloading(ymid).await;
    }

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
                    &bot,
                    chat_id,
                    &fetcher,
                    &meta,
                    &stem,
                    is_audio,
                    &mut progress_bar,
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
            let fut = fetcher.download_video_from_url(
                url.clone(),
                &file_stem,
                &quality_preference,
                fingerprint.clone(),
                &mut progress_bar,
            );

            match timeout(DOWNLOAD_TIMEOUT, fut).await {
                Ok(Ok(path)) => break path,
                Ok(Err(e)) => {
                    remove_attempt_partials(&file_stem).await;
                    retries += 1;
                    if retries >= 3 {
                        return Err(e);
                    }
                    tokio::time::sleep(Duration::from_millis(1000 * 2_u64.pow(retries - 1))).await;
                }
                Err(_) => {
                    remove_attempt_partials(&file_stem).await;
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

        // Mark download finished so the next batch item can start downloading
        // while this one uploads.
        if let Some(ymid) = ymid.as_deref() {
            let _ = db_pool.mark_pending_downloaded(ymid, &path).await;
            if let Ok(Some((batch_id, _, _))) = db_pool.get_batch_info(ymid).await {
                batch_notifier(&batch_id).notify_waiters();
            }
        }

        // Wait for earlier rows to finish uploading before starting our upload.
        if let Some(ymid) = ymid.as_deref() {
            if let Ok(Some((batch_id, position, _total))) = db_pool.get_batch_info(ymid).await {
                let notifier = batch_notifier(&batch_id);
                let deadline = std::time::Instant::now() + Duration::from_secs(1800);
                while db_pool
                    .earlier_batch_rows_unfinished(&batch_id, position)
                    .await
                    .unwrap_or(false)
                {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    let notified = notifier.notified();
                    if !db_pool
                        .earlier_batch_rows_unfinished(&batch_id, position)
                        .await
                        .unwrap_or(false)
                    {
                        break;
                    }
                    tokio::select! {
                        _ = notified => {}
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    }
                }
            }
        }

        // Acquire upload permit
        let _upload_permit = upload_semaphore
            .acquire()
            .await
            .map_err(|e| anyhow::anyhow!("Semaphore error: {}", e))?;

        if file_size > TELEGRAM_BOT_API_FILE_LIMIT {
            progress_bar
                .update(85, Some("📤 Uploading (Large)..."))
                .await?;
            let upload_result = match timeout(UPLOAD_TIMEOUT, async {
                if is_audio {
                    mtproto_uploader
                        .upload_audio(user_id, username, &path, "", &mut progress_bar)
                        .await
                } else {
                    mtproto_uploader
                        .upload_video(user_id, username, &path, "", &mut progress_bar)
                        .await
                }
            })
            .await
            {
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
                        send_audio_with_progress_botapi(
                            &bot.token(),
                            chat_id,
                            &path,
                            None,
                            &mut progress_bar,
                        )
                        .await
                    } else {
                        send_video_with_progress_botapi(
                            &bot.token(),
                            chat_id,
                            &path,
                            None,
                            &mut progress_bar,
                        )
                        .await
                    };
                    match res {
                        Ok(_) => break Ok(()),
                        Err(e) => {
                            retries += 1;
                            if retries >= 3 {
                                break Err(e);
                            }
                            tokio::time::sleep(Duration::from_millis(
                                1000 * 2_u64.pow(retries - 1),
                            ))
                            .await;
                        }
                    }
                }
            })
            .await
            {
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
            let bot_id_owned = bot_id.to_string();
            let _ = db_pool.execute_with_timeout(move |conn| {
                conn.execute("INSERT OR IGNORE INTO users (bot_id, telegram_id) VALUES (?1, ?2)", rusqlite::params![bot_id_owned, user_id])?;
                conn.execute("INSERT INTO downloads (bot_id, user_telegram_id, video_url) VALUES (?1, ?2, ?3)", rusqlite::params![bot_id_owned, user_id, video_url])?;
                Ok(())
            }).await;

            notify_batch_outcome(ymid.as_deref(), BatchOutcome::Delivered);
            Ok(())
        }
        Ok(Err(e)) => {
            // Tier 1: the bot knows why it failed — say so immediately.
            let key = classify_download_error(&e, &url);
            fail_request(
                &bot,
                chat_id,
                &db_pool,
                &ymid,
                &url,
                &e,
                key,
                lang.as_deref(),
                &mut progress_bar,
            )
            .await;
            Ok(())
        }
        Err(_) => {
            // Tier 2: something unforeseen hung the job — the budget killed
            // it, tell the user to retry.
            let e = anyhow::anyhow!("Job budget of {} seconds exceeded", JOB_BUDGET.as_secs());
            fail_request(
                &bot,
                chat_id,
                &db_pool,
                &ymid,
                &url,
                &e,
                MsgKey::DownloadTimeout,
                lang.as_deref(),
                &mut progress_bar,
            )
            .await;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use serial_test::serial;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn batch_notifier_wakes_on_notify() {
        register_batch_ymid("ymid-notify-test", "batch-notify-test");
        let notifier = batch_notifier("batch-notify-test");
        let notified = notifier.notified();
        notify_batch_outcome(Some("ymid-notify-test"), BatchOutcome::Delivered);
        timeout(Duration::from_millis(100), notified)
            .await
            .expect("notifier should wake promptly");
    }

    #[test]
    fn batch_status_fills_current_and_total() {
        let text = batch_text(MsgKey::BatchProgress, Some("ru"), 2, 3);
        assert!(text.contains('2'), "current missing: {text}");
        assert!(text.contains('3'), "total missing: {text}");
        assert!(!text.contains("{current}"));
        assert!(!text.contains("{total}"));
    }

    #[test]
    fn batch_text_replaces_placeholders_for_all_keys_and_langs() {
        let keys = [
            MsgKey::BatchProgress,
            MsgKey::BatchDelivered,
            MsgKey::BatchSkipped,
            MsgKey::BatchFailed,
            MsgKey::BatchSkipButton,
        ];
        for &key in &keys {
            for &lang in i18n::SUPPORTED_LANGS {
                let text = batch_text(key, Some(lang), 7, 9);
                assert!(!text.contains("{current}"), "lang {lang}: {text}");
                assert!(!text.contains("{total}"), "lang {lang}: {text}");
                if key != MsgKey::BatchSkipButton {
                    assert!(
                        text.contains('7'),
                        "current not substituted for {lang}: {text}"
                    );
                    assert!(
                        text.contains('9'),
                        "total not substituted for {lang}: {text}"
                    );
                } else {
                    assert!(!text.is_empty(), "empty button text for {lang}");
                }
            }
            let text = batch_text(key, None, 7, 9);
            assert!(!text.contains("{current}"), "{text}");
            assert!(!text.contains("{total}"), "{text}");
        }
    }

    #[test]
    fn notify_batch_outcome_wakes_and_removes_waiter() {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        BATCH_WAITERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert("ymid-wake".to_string(), tx);
        assert!(is_batch_waiting("ymid-wake"));

        notify_batch_outcome(Some("ymid-wake"), BatchOutcome::Delivered);

        assert!(!is_batch_waiting("ymid-wake"));
        assert_eq!(rx.try_recv().unwrap(), BatchOutcome::Delivered);
        notify_batch_outcome(Some("ymid-wake"), BatchOutcome::Failed);
    }

    #[test]
    fn notify_batch_outcome_none_is_noop() {
        notify_batch_outcome(None, BatchOutcome::Failed);
    }

    #[test]
    fn batch_failure_guard_notifies_failed_on_drop() {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        BATCH_WAITERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert("ymid-guard".to_string(), tx);

        {
            let _guard = BatchFailureGuard(Some("ymid-guard".to_string()));
            assert!(rx.try_recv().is_err());
        }

        assert!(!is_batch_waiting("ymid-guard"));
        assert_eq!(rx.try_recv().unwrap(), BatchOutcome::Failed);
    }

    #[test]
    fn batch_failure_guard_none_is_noop() {
        let (tx, mut rx) = tokio::sync::oneshot::channel::<BatchOutcome>();
        let _guard = BatchFailureGuard(None);
        drop(_guard);
        assert!(rx.try_recv().is_err());
        drop(tx);
    }

    #[test]
    fn download_errors_classified_for_immediate_reply() {
        let unsupported = anyhow::anyhow!(
            "yt-dlp failed: ERROR: Unsupported URL: https://www.tiktok.com/@u/photo/1"
        );
        assert_eq!(
            classify_download_error(&unsupported, "https://example.com"),
            crate::i18n::MsgKey::UnsupportedFormat
        );

        let timed_out = anyhow::anyhow!("Download timeout");
        assert_eq!(
            classify_download_error(&timed_out, "https://example.com"),
            crate::i18n::MsgKey::DownloadTimeout
        );

        let upload_timed_out = anyhow::anyhow!("Upload timed out after 300 seconds");
        assert_eq!(
            classify_download_error(&upload_timed_out, "https://example.com"),
            crate::i18n::MsgKey::DownloadTimeout
        );

        let generic = anyhow::anyhow!("yt-dlp failed: ERROR: Video unavailable");
        assert_eq!(
            classify_download_error(&generic, "https://example.com"),
            crate::i18n::MsgKey::DownloadFailed
        );

        let yt_block = anyhow::anyhow!("yt-dlp failed: Sign in to confirm you're not a bot");
        assert_eq!(
            classify_download_error(&yt_block, "https://youtu.be/abc"),
            crate::i18n::MsgKey::YtBlocked
        );
        let ig_block = anyhow::anyhow!("Instagram login required");
        assert_eq!(
            classify_download_error(&ig_block, "https://www.instagram.com/reel/abc/"),
            crate::i18n::MsgKey::IgBlocked
        );
        // A blocked-looking message from an unrelated platform keeps the
        // generic text: the warning names a platform for a reason.
        assert_eq!(
            classify_download_error(&yt_block, "https://www.tiktok.com/@u/video/1"),
            crate::i18n::MsgKey::DownloadFailed
        );
    }

    /// One message per row: the job budget has to end before the lease window
    /// does, otherwise a slow download is killed by the budget and swept in the
    /// same instant.
    #[test]
    fn job_budget_ends_before_the_session_lease_window() {
        assert!(
            JOB_BUDGET_SECS < crate::database::SESSION_LEASE_SECS as u64,
            "JOB_BUDGET must be strictly below the lease window"
        );
        assert!(
            JOB_BUDGET.as_secs() == JOB_BUDGET_SECS,
            "the duration and the compared constant must be the same value"
        );
    }

    async fn ads_test_db() -> (DatabasePool, tempfile::NamedTempFile) {
        let temp_file = tempfile::NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap().to_string();
        let pool = DatabasePool::new(db_path, 1);
        pool.execute_with_timeout(|conn| {
            conn.execute(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, telegram_id BIGINT NOT NULL, bot_id TEXT NOT NULL DEFAULT 'primary', last_active DATETIME DEFAULT CURRENT_TIMESTAMP, created_at DATETIME DEFAULT CURRENT_TIMESTAMP, quality_preference TEXT DEFAULT 'h264', premium_until DATETIME, lang TEXT DEFAULT NULL, ref_code TEXT DEFAULT NULL, UNIQUE(bot_id, telegram_id))",
                (),
            )?;
            conn.execute(
                "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                (),
            )?;
            conn.execute(
                "CREATE TABLE pending_downloads (id TEXT PRIMARY KEY, bot_id TEXT NOT NULL DEFAULT 'primary', user_id BIGINT NOT NULL, video_url TEXT NOT NULL, status TEXT DEFAULT 'pending', created_at DATETIME DEFAULT CURRENT_TIMESTAMP, notified_at DATETIME DEFAULT NULL, lease_expires_at DATETIME DEFAULT NULL, job_started_at DATETIME DEFAULT NULL, batch_id TEXT DEFAULT NULL, batch_position INTEGER DEFAULT NULL, batch_total INTEGER DEFAULT NULL)",
                (),
            )?;
            Ok(())
        })
        .await
        .unwrap();
        (pool, temp_file)
    }

    #[tokio::test]
    #[serial]
    async fn ads_decision_matrix() {
        let (pool, _file) = ads_test_db().await;
        unsafe {
            std::env::set_var("ADMIN_IDS", "999");
            std::env::set_var("MONETAG_MODULE_ENABLED", "true");
            std::env::remove_var("TEST_MODE");
        }
        pool.set_setting("ads_enabled", "true").await.unwrap();
        pool.set_setting("admin_ads_enabled", "false")
            .await
            .unwrap();

        // Regular user, ads globally on -> sees ads.
        assert!(ads_enabled_for(&pool, crate::database::PRIMARY_BOT_ID, 111, false).await);
        // Admin, global on, no override -> skips ads.
        assert!(!ads_enabled_for(&pool, crate::database::PRIMARY_BOT_ID, 999, true).await);
        // Premium user, global on -> skips ads.
        pool.set_user_premium(crate::database::PRIMARY_BOT_ID, 222, 30)
            .await
            .unwrap();
        assert!(!ads_enabled_for(&pool, crate::database::PRIMARY_BOT_ID, 222, false).await);
        // Same human on another bot has no premium there -> still sees ads.
        assert!(ads_enabled_for(&pool, "bbb", 222, false).await);

        // Global OFF: regular users see nothing...
        pool.set_setting("ads_enabled", "false").await.unwrap();
        assert!(!ads_enabled_for(&pool, crate::database::PRIMARY_BOT_ID, 111, false).await);
        // ...but an admin with Admin Ads ON gets the test flow (reported scenario).
        pool.set_setting("admin_ads_enabled", "true").await.unwrap();
        assert!(ads_enabled_for(&pool, crate::database::PRIMARY_BOT_ID, 999, true).await);
        // Admin without the override still skips.
        pool.set_setting("admin_ads_enabled", "false")
            .await
            .unwrap();
        assert!(!ads_enabled_for(&pool, crate::database::PRIMARY_BOT_ID, 999, true).await);

        // ymid resolves to the requesting user (what /api/ads-status uses).
        let ymid = pool
            .create_pending_download(
                crate::database::PRIMARY_BOT_ID,
                111,
                "https://vt.tiktok.com/x",
            )
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
        let guard =
            UrlProcessingGuard::try_acquire(&url).expect("first request should claim the URL");

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
