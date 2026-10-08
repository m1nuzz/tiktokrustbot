use rusqlite::{Connection, Result as SqliteResult, params, OptionalExtension};
use tokio::sync::{Semaphore, Mutex};
use tokio::time::{timeout, Duration};
use std::sync::Arc;
use lru::LruCache;
use std::num::NonZeroUsize;

pub struct DatabasePool {
    db_path: String,
    connection_semaphore: Arc<Semaphore>,
    // LRU cache with limit of 1000 users, keyed by (bot_id, telegram_id):
    // quality is per-bot, so a bare telegram_id key would leak one bot's
    // preference into another bot's downloads.
    user_cache: Arc<Mutex<LruCache<(String, i64), UserInfo>>>,
}

#[derive(Clone)]
pub struct UserInfo {
    pub quality_preference: String,
    pub last_updated: tokio::time::Instant,
}

/// How a download request was unlocked (for delivery-reason stats).
/// Verified = the valued-impression gate, the only way to unlock today.
/// Timer = legacy 20s age-rule backstop (removed, kept so historic rows keep
/// their delivery reason); Admin = historic admin-bypass rows, kept only
/// because rows delivered that way are still counted in the stats - it is no
/// longer issued anywhere, admins go through the same gate as everyone else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimVia {
    Verified,
    Timer,
    Admin,
}

/// Lease window pushed forward by every client heartbeat while the mini-app is
/// open. The sweeper expires a row only once this window has lapsed, so an
/// open webapp keeps its session claimable for as long as the user stays on it
/// instead of being cut off at a fixed 30 minutes.
/// Bot identity for pre-multi-bot rows and single-bot call sites: every
/// `bot_id` column defaults to this, and Phase-2 runtime replaces these
/// uses with the dispatching bot's id (grep PRIMARY_BOT_ID for the list).
pub const PRIMARY_BOT_ID: &str = "primary";

pub const SESSION_LEASE_SECS: i64 = 1800;

/// Hard ceiling for one session, measured from the row's creation: every
/// heartbeat is clamped to it, so no row stays claimable forever even if a
/// client keeps pinging. `completed` and `expired` remain the only exits and
/// both are terminal.
///
/// Tradeoff: 3 days is far longer than any real funnel step, and the whole
/// point of the ceiling is that a valued postback Monetag sends minutes after
/// the user left for the advertiser's page is still honoured while they are
/// away. A session that is genuinely never finished therefore keeps its row
/// (and its ad) alive for up to 3 days - the cost is a small amount of dead
/// rows in the funnel, paid for so no earned reward is thrown away.
pub const SESSION_LEASE_CEILING_SECS: i64 = 3 * 24 * 60 * 60;

/// Rows one sweeper tick may notify, oldest first. Without a bound a service
/// start would flush every accumulated backlog into chats in a single burst;
/// whatever does not fit is notified by the following ticks.
pub const EXPIRY_BATCH_LIMIT: u32 = 200;

/// A session ymid the mini-app must use for its ad event, and whether it is an
/// existing row the press may continue or a row minted for this press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionYmid {
    pub ymid: String,
    pub reused: bool,
}

/// Mini-app funnel stage that doubles as the authoritative request-time marker
/// for "an ad was requested for this ymid". Shared with the client contract.
pub const AD_REQUESTED_EVENT: &str = "ad_requested";

/// Per-day aggregates for the weekly admin report.
#[derive(Debug, Clone)]
pub struct WeeklyDayStats {
    pub date: String,
    pub unique_users: i64,
    pub new_users: i64,
    pub downloads: i64,
    pub blocks: i64,
}

/// One day of the conversion funnel. Drop-off reasons come from
/// `pending_downloads` statuses; button-level steps from `funnel_events`.
#[derive(Debug, Clone)]
pub struct FunnelDayStats {
    pub date: String,
    pub started: i64,
    pub link_sent: i64,
    pub ad_watched: i64,
    pub claimed: i64,
    pub delivered: i64,
    pub expired: i64,
    pub failed: i64,
    pub delivered_paid: i64,
    pub delivered_free: i64,
}

#[derive(Debug, Clone)]
pub struct RichDailyStats {
    pub date: String,
    pub unique_users: i64,
    pub unique_users_delta: i64,
    pub unique_downloaders: i64,
    pub total_downloads: i64,
    pub ad_impressions: i64,
    pub new_users: i64,
    pub returning_users: i64,
    pub payments_count: i64,
    pub revenue_xtr: i64,
    pub invoices_sent: i64,
    pub peak_hour: Option<(u32, i64)>,
    pub top_downloaders: Vec<(i64, i64)>,
    pub last_active_users: Vec<(i64, String)>,
}

/// SQL fragment excluding admin ids from a telegram-id column.
/// Ids are formatted as plain integers (parsed i64, injection-impossible),
/// so no query parameters are needed. Empty list disables the filter.
fn admin_filter_sql(column: &str, exclude_admins: &[i64]) -> String {
    if exclude_admins.is_empty() {
        String::new()
    } else {
        let ids = exclude_admins
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!(" AND {} NOT IN ({})", column, ids)
    }
}

impl DatabasePool {
    pub fn new(db_path: String, max_connections: usize) -> Self {
        Self {
            db_path,
            connection_semaphore: Arc::new(Semaphore::new(max_connections)),
            // LRU cache automatically removes least recently used entries when limit reached
            user_cache: Arc::new(Mutex::new(
                LruCache::new(NonZeroUsize::new(1000).unwrap())
            )),
        }
    }

    /// Execute database operation with timeout and proper error handling
    pub async fn execute_with_timeout<F, R>(&self, operation: F) -> Result<R, anyhow::Error>
    where
        F: FnOnce(&Connection) -> SqliteResult<R> + Send + 'static,
        R: Send + 'static,
    {
        let _permit = timeout(
            Duration::from_secs(5),
            self.connection_semaphore.acquire()
        ).await??;
        
        let db_path = self.db_path.clone();
        let result = timeout(
            Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                let conn = Connection::open(&db_path)?;
                
                // Optimize SQLite for concurrent access
                conn.execute_batch(
                    "PRAGMA journal_mode = WAL;
                     PRAGMA synchronous = NORMAL;
                     PRAGMA cache_size = 32000;
                     PRAGMA temp_store = MEMORY;
                     PRAGMA busy_timeout = 5000;"
                )?;
                
                operation(&conn)
            })
        ).await?;
        
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => Err(anyhow::anyhow!(e)),
            Err(e) => Err(anyhow::anyhow!("Timeout: {}", e)),
        }
    }

    /// Get user quality preference with caching
     pub async fn get_user_quality(&self, bot_id: &str, user_id: i64) -> Result<String, anyhow::Error> {
        let bot_owned = bot_id.to_string();
        let cache_key = (bot_owned.clone(), user_id);
        // Check LRU cache
        {
            let mut cache = self.user_cache.lock().await;
            if let Some(user_info) = cache.get(&cache_key) {
                // Cache is valid for 5 minutes
                if user_info.last_updated.elapsed() < Duration::from_secs(300) {
                    log::info!("Using cached quality preference for user {}: {}", user_id, user_info.quality_preference);
                    return Ok(user_info.quality_preference.clone());
                }
                log::info!("Cache expired for user {}, removing from cache", user_id);
                // LRU automatically moves the element to the front when accessed with get,
                // so we need to remove and re-add if it's expired
                cache.pop(&cache_key);
            }
        }

        // Load from DB
        let quality = self.execute_with_timeout(move |conn| {
            match conn.query_row(
                "SELECT quality_preference FROM users WHERE bot_id = ?1 AND telegram_id = ?2",
                params![bot_owned, user_id],
                |row| Ok(row.get::<_, String>(0)?)
            ) {
                Ok(quality) => {
                    log::info!("Retrieved quality preference from DB for user {}: {}", user_id, quality);
                    Ok(quality)
                },
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    log::info!("No quality preference found for user {}, using default", user_id);
                    Ok("best".to_string()) // Default value
                },
                Err(e) => {
                    log::error!("Error retrieving quality preference for user {} from DB: {}", user_id, e);
                    Ok("best".to_string()) // Default value
                }
            }
        }).await?;

        // Update LRU cache (put automatically evicts old entries)
        {
            let mut cache = self.user_cache.lock().await;
            log::info!("Caching quality preference for user {}: {}", user_id, quality);
            cache.put(
                (bot_id.to_string(), user_id),
                UserInfo {
                    quality_preference: quality.clone(),
                    last_updated: tokio::time::Instant::now(),
                }
            );
        }
        
        Ok(quality)
    }

    /// Invalidate user quality cache
    pub async fn invalidate_user_quality_cache(&self, bot_id: &str, user_id: i64) {
        let mut cache = self.user_cache.lock().await;
        cache.pop(&(bot_id.to_string(), user_id));
        log::info!("Invalidated cached quality preference for user {}", user_id);
    }

    /// Manual /language override for a user, if set.
    pub async fn get_user_lang(&self, bot_id: &str, user_id: i64) -> Result<Option<String>, anyhow::Error> {
        let bot_owned = bot_id.to_string();
        self.execute_with_timeout(move |conn| {
            // Double Option: missing row -> None via `.optional()`, NULL cell
            // (a cleared override) -> None via `FromSql for Option`. Both mean
            // "no manual override", so flatten to one.
            let lang: Option<Option<String>> = conn.query_row(
                "SELECT lang FROM users WHERE bot_id = ?1 AND telegram_id = ?2",
                params![bot_owned, user_id],
                |row| row.get(0)
            ).optional()?;
            Ok(lang.flatten())
        }).await.map_err(|e| anyhow::anyhow!("Failed to get language for user {}: {}", user_id, e))
    }

    /// Persist the manual /language override for a user.
    pub async fn set_user_lang(&self, bot_id: &str, user_id: i64, lang: &str) -> Result<(), anyhow::Error> {
        let bot_owned = bot_id.to_string();
        let lang_owned = lang.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute("INSERT OR IGNORE INTO users (bot_id, telegram_id) VALUES (?1, ?2)", params![bot_owned, user_id])?;
            conn.execute("UPDATE users SET lang = ?1 WHERE bot_id = ?2 AND telegram_id = ?3", params![lang_owned, bot_owned, user_id])?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to set language for user {}: {}", user_id, e))
    }
    /// Drop the manual override so the device tag wins again (auto-detect).
    /// Missing rows are fine: clearing what was never set changes nothing.
    pub async fn clear_user_lang(&self, bot_id: &str, user_id: i64) -> Result<(), anyhow::Error> {
        let bot_owned = bot_id.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute("INSERT OR IGNORE INTO users (bot_id, telegram_id) VALUES (?1, ?2)", params![bot_owned, user_id])?;
            conn.execute("UPDATE users SET lang = NULL WHERE bot_id = ?1 AND telegram_id = ?2", params![bot_owned, user_id])?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to clear language for user {}: {}", user_id, e))
    }

    /// Effective locale: the stored /language override wins, else the Telegram
    /// device tag, else English. Never fails.
    pub async fn get_effective_lang(&self, bot_id: &str, user_id: i64, tg_lang: Option<&str>) -> String {
        let stored = self.get_user_lang(bot_id, user_id).await.ok().flatten();
        crate::i18n::resolve_lang(stored.as_deref().or(tg_lang)).to_string()
    }

    /// Get a setting from the settings table
    pub async fn get_setting(&self, key: &str) -> Result<String, anyhow::Error> {
        let key_owned = key.to_string();
        self.execute_with_timeout(move |conn| {
            conn.query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key_owned],
                |row| row.get(0)
            )
        }).await.map_err(|e| anyhow::anyhow!("Failed to get setting {}: {}", key, e))
    }

    /// Set a setting in the settings table
    pub async fn set_setting(&self, key: &str, value: &str) -> Result<(), anyhow::Error> {
        let key_owned = key.to_string();
        let value_owned = value.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                params![key_owned, value_owned],
            )?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to set setting {}: {}", key, e))
    }

    /// Register a bot by its stable numeric id (token prefix). Idempotent:
    /// re-registering refreshes the username. Tokens never reach the database.
    pub async fn register_bot(&self, bot_id: &str, username: &str) -> Result<(), anyhow::Error> {
        let bot_owned = bot_id.to_string();
        let name_owned = username.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO bots (bot_id, username) VALUES (?1, ?2) ON CONFLICT(bot_id) DO UPDATE SET username = excluded.username",
                params![bot_owned, name_owned],
            )?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to register bot {}: {}", bot_id, e))
    }

    /// All known bots, oldest first. The aggregate admin view iterates this.
    pub async fn list_bots(&self) -> Result<Vec<(String, String)>, anyhow::Error> {
        self.execute_with_timeout(|conn| {
            let mut stmt = conn.prepare("SELECT bot_id, username FROM bots ORDER BY created_at, bot_id")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        }).await.map_err(|e| anyhow::anyhow!("Failed to list bots: {}", e))
    }

    /// Per-bot setting override. Missing rows fall back to the global
    /// `settings` table, then to the caller default (see resolve below).
    pub async fn set_bot_setting(&self, bot_id: &str, key: &str, value: &str) -> Result<(), anyhow::Error> {
        let bot_owned = bot_id.to_string();
        let key_owned = key.to_string();
        let value_owned = value.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO bot_settings (bot_id, key, value) VALUES (?1, ?2, ?3)",
                params![bot_owned, key_owned, value_owned],
            )?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to set setting {} for bot {}: {}", key, bot_id, e))
    }

    /// Effective setting value: per-bot override wins, else the global
    /// setting, else `default`. Never fails: every layer degrades gracefully.
    pub async fn resolve_bot_setting(&self, bot_id: &str, key: &str, default: &str) -> String {
        let bot_owned = bot_id.to_string();
        let key_owned = key.to_string();
        let per_bot: Option<String> = self.execute_with_timeout(move |conn| {
            conn.query_row(
                "SELECT value FROM bot_settings WHERE bot_id = ?1 AND key = ?2",
                params![bot_owned, key_owned],
                |row| row.get(0),
            ).optional()
        }).await.ok().flatten();
        if let Some(value) = per_bot {
            return value;
        }
        self.get_setting(key).await.unwrap_or_else(|_| default.to_string())
    }

    /// Create a pending download record and return its unique ID (ymid).
    /// The row starts with a live lease so it is claimable from the first
    /// second, before the client sends its first heartbeat.
     pub async fn create_pending_download(&self, bot_id: &str, user_id: i64, video_url: &str) -> Result<String, anyhow::Error> {
        let id = uuid::Uuid::new_v4().to_string();
        let id_owned = id.clone();
        let video_url_owned = video_url.to_string();
        let bot_owned = bot_id.to_string();
        let lease_secs = SESSION_LEASE_SECS;
        
        self.execute_with_timeout(move |conn| {
            conn.execute(
                &format!(
                    "INSERT INTO pending_downloads (id, bot_id, user_id, video_url, lease_expires_at) VALUES (?1, ?2, ?3, ?4, datetime('now', '+{} seconds'))",
                    lease_secs
                ),
                params![id_owned, bot_owned, user_id, video_url_owned],
            )?;
            Ok(())
        }).await.map(|_| id).map_err(|e| anyhow::anyhow!("Failed to create pending download: {}", e))
    }

    /// Mark a pending row as verified and return the number of rows affected, so
    /// the caller can tell "this postback moved the row" from "the row was
    /// already verified, terminal, or unknown" instead of logging a success
    /// that changed nothing.
    pub async fn mark_as_verified_with_logging(&self, id: &str) -> Result<usize, anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            let rows = conn.execute(
                "UPDATE pending_downloads SET status = 'verified' WHERE id = ?1 AND status = 'pending'",
                params![id_owned],
            )?;
            Ok(rows)
        }).await.map_err(|e| anyhow::anyhow!("Failed to verify download {}: {}", id, e))
    }

    /// Mark a still-claimable row as terminally failed (download/upload error
    /// or job budget exceeded) and return how many rows actually moved, so the
    /// caller can tell "this job failed" from "the row was already terminal".
    ///
    /// The status guard is the point: without it a row could be flipped to
    /// `failed` twice - once by the job and once by the sweeper - and the row
    /// that owns the single failure notification must be unique. Notified
    /// immediately so the sweeper skips it.
    pub async fn mark_pending_failed(&self, id: &str) -> Result<usize, anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            let rows = conn.execute(
                "UPDATE pending_downloads SET status = 'failed', notified_at = CURRENT_TIMESTAMP WHERE id = ?1 AND status IN ('pending', 'verified')",
                params![id_owned],
            )?;
            Ok(rows)
        }).await.map_err(|e| anyhow::anyhow!("Failed to mark {} as failed: {}", id, e))
    }

    /// Record that a download job really began for this row, returning how
    /// many rows moved (0 = unknown ymid).
    ///
    /// This is the gate for the failure message: the sweeper tells a lapsed
    /// session from a failed download by exactly this column, so a row that
    /// never started a job can never be told the video failed to download.
    pub async fn mark_job_started(&self, id: &str) -> Result<usize, anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            let rows = conn.execute(
                "UPDATE pending_downloads SET job_started_at = COALESCE(job_started_at, CURRENT_TIMESTAMP) WHERE id = ?1",
                params![id_owned],
            )?;
            Ok(rows)
        }).await.map_err(|e| anyhow::anyhow!("Failed to mark job started for {}: {}", id, e))
    }

    /// Stamp the authoritative "an ad was requested for this ymid" marker.
    ///
    /// Written at REQUEST time, from the beacon the client fires immediately
    /// before every show_*() call. It deliberately is not the impression
    /// journal: the impression postback lags the ad by seconds, so a predicate
    /// built on it would let a second press inside that window reuse a ymid
    /// that already carried an ad - two ad events on one ymid, which Monetag
    /// prices at zero. Returns how many rows moved (0 = unknown ymid).
    pub async fn mark_ad_requested(&self, id: &str) -> Result<usize, anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            let rows = conn.execute(
                "UPDATE pending_downloads SET ad_requested_at = COALESCE(ad_requested_at, CURRENT_TIMESTAMP) WHERE id = ?1",
                params![id_owned],
            )?;
            Ok(rows)
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to mark ad requested for {}: {}", id, e))
    }

    /// Resolve the ymid an ad event must run under.
    ///
    /// Monetag requires a unique ymid per ad event, so a press may only keep the
    /// ymid it was given while that ymid has never carried an ad. A ymid that
    /// already has `ad_requested_at`, or that is terminal, is spent and gets a
    /// fresh row instead - the same video for the same user, so the funnel and
    /// the per-ymid joins keep working.
    ///
    /// A fresh UNUSED session is never displaced: if the user already has one
    /// (typically the very row this press opened), that row is returned as-is
    /// and no second row is created. This is also the publisher-side frequency
    /// cap - Monetag ignores zone frequency settings, so one live session per
    /// user is ours to enforce.
    ///
    /// `user_id` is what makes the copy language-preserving: the effective
    /// locale comes from that user's stored /language override (else the ?lang=
    /// tag the bot button appends), so no language column is needed.
    ///
    /// The whole decision runs in one transaction: two concurrent presses of the
    /// same entry ymid cannot both reuse it.
    pub async fn resolve_session_ymid(&self, entry_ymid: &str) -> Result<Option<SessionYmid>, anyhow::Error> {
        let entry_owned = entry_ymid.to_string();
        self.execute_with_timeout(move |conn| {
            let entry: Option<(i64, String)> = conn
                .query_row(
                    "SELECT user_id, video_url FROM pending_downloads WHERE id = ?1",
                    params![entry_owned],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;

            let (user_id, video_url) = match entry {
                Some(row) => row,
                None => return Ok(None),
            };

            // A ymid is reusable while it is still claimable, was never notified
            // about, and never carried an ad. `ad_requested_at IS NULL` is the
            // load-bearing part.
            let reusable = |id: &str| -> Result<bool, rusqlite::Error> {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pending_downloads WHERE id = ?1 AND status IN ('pending', 'verified') AND notified_at IS NULL AND ad_requested_at IS NULL)",
                    params![id],
                    |row| row.get(0),
                )
            };

            if reusable(&entry_owned)? {
                return Ok(Some(SessionYmid {
                    ymid: entry_owned,
                    reused: true,
                }));
            }

            // The user's own newest unused session wins over minting: one live
            // session per user, and no orphan row in the common double-press case.
            let sibling: Option<String> = conn
                .query_row(
                    &format!(
                        "SELECT id FROM pending_downloads WHERE user_id = ?1 AND id <> ?2 AND status IN ('pending', 'verified') AND notified_at IS NULL AND ad_requested_at IS NULL AND COALESCE(lease_expires_at, datetime(created_at, '+{} seconds')) > datetime('now') ORDER BY created_at DESC, id DESC LIMIT 1",
                        SESSION_LEASE_SECS
                    ),
                    params![user_id, entry_owned],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(ymid) = sibling {
                return Ok(Some(SessionYmid {
                    ymid,
                    reused: true,
                }));
            }

            let id = uuid::Uuid::new_v4().to_string();
            conn.execute(
                &format!(
                    "INSERT INTO pending_downloads (id, user_id, video_url, entry_ymid, lease_expires_at) VALUES (?1, ?2, ?3, ?4, datetime('now', '+{} seconds'))",
                    SESSION_LEASE_SECS
                ),
                params![id, user_id, video_url, entry_owned],
            )?;
            Ok(Some(SessionYmid {
                ymid: id,
                reused: false,
            }))
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to resolve session ymid for {}: {}", entry_ymid, e))
    }

    /// Record every Monetag postback hit, even unknown ymids and non_valued:
    /// the journal backing the Ads revenue counters and mismatch alerts.
    pub async fn log_postback(
        &self,
        ymid: &str,
        event_type: Option<&str>,
        reward_event_type: &str,
        estimated_price: Option<f64>,
        request_var: Option<&str>,
        sub_zone_id: Option<&str>,
        zone_id: Option<&str>,
        telegram_user_id: Option<&str>,
    ) -> Result<(), anyhow::Error> {
        let ymid_owned = ymid.to_string();
        let event_owned = event_type.map(|s| s.to_string());
        let reward_owned = reward_event_type.to_string();
        let request_owned = request_var.map(|s| s.chars().take(64).collect::<String>());
        let sub_owned = sub_zone_id.map(|s| s.chars().take(64).collect::<String>());
        let zone_owned = zone_id.map(|s| s.chars().take(64).collect::<String>());
        let telegram_owned = telegram_user_id.map(|s| s.chars().take(64).collect::<String>());
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO monetag_postbacks (ymid, event_type, reward_event_type, estimated_price, request_var, sub_zone_id, zone_id, telegram_user_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![ymid_owned, event_owned, reward_owned, estimated_price, request_owned, sub_owned, zone_owned, telegram_owned],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to log postback: {}", e))
    }

    /// Client-side ad-funnel telemetry from the mini-app. Correlated with
    /// `monetag_postbacks` by `ymid` to tell "no fill" from "ad blocked here".
    /// Long strings are truncated in SQL so a hostile client cannot bloat the
    /// database through this endpoint.
    ///
    /// The `ad_requested` stage is also load-bearing, not just telemetry: the
    /// client fires it immediately before every show_*() call, so this is the
    /// REQUEST-time signal that marks a ymid as spent (Monetag requires a unique
    /// ymid per ad event). Reusing this existing fire-and-forget beacon instead
    /// of adding an endpoint is deliberate: it already survives the webapp being
    /// closed by Telegram, and the popup path must fire inside the user gesture,
    /// so nothing there can be awaited before show().
    pub async fn log_mini_app_event(
        &self,
        ymid: &str,
        event: &str,
        platform: Option<&str>,
        sdk_host: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<(), anyhow::Error> {
        let ymid_owned = ymid.chars().take(64).collect::<String>();
        let event_owned = event.chars().take(64).collect::<String>();
        let platform_owned = platform.map(|s| s.chars().take(16).collect::<String>());
        let host_owned = sdk_host.map(|s| s.chars().take(64).collect::<String>());
        let ua_owned = user_agent.map(|s| s.chars().take(256).collect::<String>());
        let ad_requested = event_owned == AD_REQUESTED_EVENT;
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO mini_app_events (ymid, event, platform, sdk_host, user_agent) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![ymid_owned, event_owned, platform_owned, host_owned, ua_owned],
            )?;
            if ad_requested {
                conn.execute(
                    "UPDATE pending_downloads SET ad_requested_at = COALESCE(ad_requested_at, CURRENT_TIMESTAMP) WHERE id = ?1",
                    params![ymid_owned],
                )?;
            }
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to log mini-app event: {}", e))
    }

    /// Whether Monetag ever recorded an ad view for this ymid.
    /// The mini-app cannot tell "zone out of inventory" from "ad blocked on
    /// this device" — one SDK rejection covers both. An absent impression is
    /// the server-side proof that nothing was ever presented for this ymid.
    /// `event_type IS NULL` counts too: SSP configs that omit the macro still
    /// journal the row, and it can only exist after a real ad view.
    pub async fn has_ad_impression(&self, ymid: &str) -> Result<bool, anyhow::Error> {
        let ymid_owned = ymid.to_string();
        self.execute_with_timeout(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM monetag_postbacks WHERE ymid = ?1 AND (event_type = 'impression' OR event_type IS NULL)",
                params![ymid_owned],
                |row| row.get(0),
            )?;
            Ok(count > 0)
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to check impressions for {}: {}", ymid, e))
    }

    /// Whether a valued impression was journaled for this ymid (payment
    /// proof). Stricter than has_ad_impression: the reward must be valued,
    /// so a non-valued impression alone never unlocks a download.
    pub async fn has_valued_impression(&self, id: &str) -> Result<bool, anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM monetag_postbacks WHERE ymid = ?1 AND reward_event_type = 'valued' AND (event_type = 'impression' OR event_type IS NULL)",
                params![id_owned],
                |row| row.get(0),
            )?;
            Ok(count > 0)
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to check valued impressions for {}: {}", id, e))
    }

    /// The delivery gate, and the only one: a download unlocks if and only if
    /// Monetag's backend confirmed a valued impression for this ymid.
    ///
    /// That postback IS the reward. Monetag decides whether an ad was valued
    /// and their postback docs state that only valued events should trigger a
    /// user reward, so our own watch floor, click expectation and age timers
    /// were guesses about their pipeline that could only ever refuse a reward
    /// Monetag had already paid for. Nothing client-provided is trusted here:
    /// no duration, no attestation, no click.
    ///
    /// The claim itself is the single-use atomic UPDATE in
    /// `claim_verified_download`, so concurrent callers cannot both deliver.
    /// Claim a download for a session where an ad was really displayed.
    ///
    /// The proof is a journaled Monetag impression of ANY reward type, not a
    /// `valued` one: the product rule is that finishing the ad earns the video,
    /// and Monetag's `valued` / `non_valued` split decides what we are PAID for,
    /// not whether the user may have what they watched through. A `valued`
    /// impression is still tracked separately for revenue.
    ///
    /// The row must be `verified` - a real ad display is what verifies it - so
    /// an empty `ymid` or a hand-made request cannot mint a download.
    pub async fn claim_if_ad_presented(
        &self,
        id: &str,
    ) -> Result<(i64, String, ClaimVia), anyhow::Error> {
        if !self.has_ad_impression(id).await.unwrap_or(false) {
            return Err(anyhow::anyhow!("no ad impression for {}", id));
        }
        self.claim_verified_download(id)
            .await
            .map(|(u, url)| (u, url, ClaimVia::Verified))
            .map_err(|e| anyhow::anyhow!("claim failed for {}: {}", id, e))
    }

    /// (valued, non_valued) postback counts for the last `days` days.
    pub async fn get_postback_stats(&self, days: i64) -> Result<(i64, i64), anyhow::Error> {
        self.execute_with_timeout(move |conn| {
            let since = format!("date('now', '-{} days')", days - 1);
            let valued: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM monetag_postbacks WHERE date(created_at) >= {since} AND reward_event_type = 'valued'"),
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let non_valued: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM monetag_postbacks WHERE date(created_at) >= {since} AND reward_event_type != 'valued'"),
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            Ok((valued, non_valued))
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get postback stats: {}", e))
    }

    /// Claim a verified download and trigger completion. The UPDATE repeats the
    /// `status = 'verified'` predicate on purpose: it is the single-use gate,
    /// and only the caller that flips exactly one row owns the delivery.
    pub async fn claim_verified_download(&self, id: &str) -> Result<(i64, String), anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            let (user_id, url): (i64, String) = conn.query_row(
                "SELECT user_id, video_url FROM pending_downloads WHERE id = ?1 AND status = 'verified'",
                params![id_owned],
                |row| Ok((row.get(0)?, row.get(1)?))
            )?;

            let claimed = conn.execute(
                "UPDATE pending_downloads SET status = 'completed' WHERE id = ?1 AND status = 'verified'",
                params![id_owned],
            )?;

            if claimed == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }

            Ok((user_id, url))
        }).await.map_err(|e| anyhow::anyhow!("Failed to claim verified download {}: {}", id, e))
    }

    /// Get user_id for a specific ymid
    pub async fn get_user_id_by_ymid(&self, id: &str) -> Result<i64, anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            let user_id: i64 = conn.query_row(
                "SELECT user_id FROM pending_downloads WHERE id = ?1",
                params![id_owned],
                |row| row.get(0)
            )?;
            Ok(user_id)
        }).await.map_err(|e| anyhow::anyhow!("Ymid {} not found: {}", id, e))
    }

    /// Owning bot of a session ymid, if the row exists. Unknown ymid is not
    /// an error: callers fall back to the primary bot.
    pub async fn get_bot_id_by_ymid(&self, id: &str) -> Result<Option<String>, anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            conn.query_row(
                "SELECT bot_id FROM pending_downloads WHERE id = ?1",
                params![id_owned],
                |row| row.get(0),
            ).optional()
        }).await.map_err(|e| anyhow::anyhow!("Failed to get bot for ymid {}: {}", id, e))
    }

    /// Get status for a pending download by ymid
    pub async fn get_pending_download_status(&self, id: &str) -> Result<Option<String>, anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            let status: Option<String> = conn.query_row(
                "SELECT status FROM pending_downloads WHERE id = ?1",
                params![id_owned],
                |row| row.get(0)
            ).optional()?;
            Ok(status)
        }).await.map_err(|e| anyhow::anyhow!("Failed to get status for {}: {}", id, e))
    }

    /// Push an open session's lease `lease_secs` into the future and return
    /// `(status, lease_expires_at)`, or `None` when there is nothing to extend:
    /// an unknown ymid, or a row that is already terminal. The deadline is
    /// clamped to `SESSION_LEASE_CEILING_SECS` from creation, and it is handed
    /// back so the client learns the deadline from the server instead of
    /// running a timer of its own.
    ///
    /// Closing the webapp deliberately does NOT release the lease. Telegram
    /// fires `visibilitychange`/`pagehide` the moment the user leaves for the
    /// advertiser's page, so a release-on-close would make the row terminal
    /// while the user is away and the valued postback Monetag sends after that
    /// close would be refused - exactly the "the video is in the chat on
    /// return" case. Stopping the heartbeat only stops extending; the sweeper
    /// still expires the row once the lease lapses.
    pub async fn refresh_session_lease(&self, id: &str, lease_secs: i64) -> Result<Option<(String, String)>, anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute(
                &format!(
                    "UPDATE pending_downloads SET lease_expires_at = min(datetime('now', '+{lease_secs} seconds'), datetime(created_at, '+{ceiling_secs} seconds')) WHERE id = ?1 AND status IN ('pending', 'verified') AND notified_at IS NULL",
                    ceiling_secs = SESSION_LEASE_CEILING_SECS,
                ),
                params![&id_owned],
            )?;
            // Re-read instead of trusting the UPDATE count: the row must come
            // back with a lease, which is what proves it was extended.
            let row: Option<(String, String)> = conn.query_row(
                "SELECT status, lease_expires_at FROM pending_downloads WHERE id = ?1 AND status IN ('pending', 'verified') AND lease_expires_at IS NOT NULL",
                params![id_owned],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            Ok(row)
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to refresh lease for {}: {}", id, e))
    }

    /// Status and lease deadline of one row in a single read, so a client
    /// never pairs a status from one moment with a lease from another.
    /// The lease is NULL for rows created before the lease column existed.
    pub async fn get_session_state(&self, id: &str) -> Result<Option<(String, Option<String>)>, anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            let row: Option<(String, Option<String>)> = conn.query_row(
                "SELECT status, lease_expires_at FROM pending_downloads WHERE id = ?1",
                params![id_owned],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            Ok(row)
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read session state for {}: {}", id, e))
    }

    /// Record that a user blocked the bot (MyChatMember -> Banned).
     pub async fn record_block(&self, bot_id: &str, user_id: i64) -> Result<(), anyhow::Error> {
        let bot_owned = bot_id.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO blocks (bot_id, telegram_id) VALUES (?1, ?2)",
                params![bot_owned, user_id],
            )?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to record block for user {}: {}", user_id, e))
    }

    /// Per-day aggregates for the last `days` days, oldest first.
    /// Pass admin ids to exclude admin (test/self) traffic.
    pub async fn get_weekly_stats(&self, days: i64, exclude_admins: &[i64]) -> Result<Vec<WeeklyDayStats>, anyhow::Error> {
        let excl_u = admin_filter_sql("telegram_id", exclude_admins);
        let excl_d = admin_filter_sql("user_telegram_id", exclude_admins);
        let mut out = Vec::new();
        for d in (0..days).rev() {
            let excl_u = excl_u.clone();
            let excl_d = excl_d.clone();
            let day = self.execute_with_timeout(move |conn| {
                let day_sql = format!("date('now', '-{} days')", d);
                let unique_users: i64 = conn.query_row(
                    &format!("SELECT COUNT(DISTINCT telegram_id) FROM users WHERE date(last_active) = {day_sql}{excl_u}"),
                    [], |r| r.get(0)).unwrap_or(0);
                let new_users: i64 = conn.query_row(
                    &format!("SELECT COUNT(*) FROM users WHERE date(created_at) = {day_sql}{excl_u}"),
                    [], |r| r.get(0)).unwrap_or(0);
                let downloads: i64 = conn.query_row(
                    &format!("SELECT COUNT(*) FROM downloads WHERE date(download_date) = {day_sql}{excl_d}"),
                    [], |r| r.get(0)).unwrap_or(0);
                let blocks: i64 = conn.query_row(
                    &format!("SELECT COUNT(*) FROM blocks WHERE date(blocked_at) = {day_sql}"),
                    [], |r| r.get(0)).unwrap_or(0);
                Ok((unique_users, new_users, downloads, blocks))
            }).await?;
            let date = (chrono::Local::now() - chrono::Duration::days(d)).format("%Y-%m-%d").to_string();
            out.push(WeeklyDayStats { date, unique_users: day.0, new_users: day.1, downloads: day.2, blocks: day.3 });
        }
        Ok(out)
    }

    /// Conversion funnel for the last `days` days, oldest first.
    /// S0 registrations, S1 links sent, S2 ads watched, S3 claimed,
    /// S4 delivered, plus drop-off reasons. Admin traffic excluded via
    /// `exclude_admins` (the funnel diagnoses real users).
    pub async fn get_funnel_stats(&self, days: i64, exclude_admins: &[i64]) -> Result<Vec<FunnelDayStats>, anyhow::Error> {
        let excl_u = admin_filter_sql("telegram_id", exclude_admins);
        let excl_p = admin_filter_sql("user_id", exclude_admins);
        let excl_d = admin_filter_sql("user_telegram_id", exclude_admins);
        let excl_f = admin_filter_sql("f.user_telegram_id", exclude_admins);
        let mut out = Vec::new();
        for d in (0..days).rev() {
            let excl_u = excl_u.clone();
            let excl_p = excl_p.clone();
            let excl_d = excl_d.clone();
            let excl_f = excl_f.clone();
            let day = self.execute_with_timeout(move |conn| {
                let day_sql = format!("date('now', '-{} days')", d);
                let started: i64 = conn.query_row(
                    &format!("SELECT COUNT(*) FROM users WHERE date(created_at) = {day_sql}{excl_u}"),
                    [], |r| r.get(0)).unwrap_or(0);
                let link_sent: i64 = conn.query_row(
                    &format!("SELECT COUNT(DISTINCT user_id) FROM pending_downloads WHERE date(created_at) = {day_sql}{excl_p}"),
                    [], |r| r.get(0)).unwrap_or(0);
                let ad_watched: i64 = conn.query_row(
                    &format!("SELECT COUNT(DISTINCT user_id) FROM pending_downloads WHERE date(created_at) = {day_sql} AND status IN ('verified', 'completed'){excl_p}"),
                    [], |r| r.get(0)).unwrap_or(0);
                let claimed: i64 = conn.query_row(
                    &format!("SELECT COUNT(DISTINCT user_id) FROM pending_downloads WHERE date(created_at) = {day_sql} AND status = 'completed'{excl_p}"),
                    [], |r| r.get(0)).unwrap_or(0);
                let delivered: i64 = conn.query_row(
                    &format!("SELECT COUNT(DISTINCT user_telegram_id) FROM downloads WHERE date(download_date) = {day_sql}{excl_d}"),
                    [], |r| r.get(0)).unwrap_or(0);
                let expired: i64 = conn.query_row(
                    &format!("SELECT COUNT(*) FROM pending_downloads WHERE date(created_at) = {day_sql} AND status = 'expired'{excl_p}"),
                    [], |r| r.get(0)).unwrap_or(0);
                let failed: i64 = conn.query_row(
                    &format!("SELECT COUNT(*) FROM pending_downloads WHERE date(created_at) = {day_sql} AND status = 'failed'{excl_p}"),
                    [], |r| r.get(0)).unwrap_or(0);
                let delivered_paid: i64 = conn.query_row(
                    &format!("SELECT COUNT(*) FROM funnel_events f WHERE date(f.created_at) = {day_sql} AND f.event = 'delivered_valued'{excl_f}"),
                    [], |r| r.get(0)).unwrap_or(0);
                let delivered_free: i64 = conn.query_row(
                    &format!("SELECT COUNT(*) FROM funnel_events f WHERE date(f.created_at) = {day_sql} AND f.event IN ('delivered_timer', 'delivered_admin'){excl_f}"),
                    [], |r| r.get(0)).unwrap_or(0);
                Ok((started, link_sent, ad_watched, claimed, delivered, expired, failed, delivered_paid, delivered_free))
            }).await?;
            let date = (chrono::Local::now() - chrono::Duration::days(d)).format("%Y-%m-%d").to_string();
            out.push(FunnelDayStats {
                date, started: day.0, link_sent: day.1, ad_watched: day.2,
                claimed: day.3, delivered: day.4, expired: day.5, failed: day.6,
                delivered_paid: day.7, delivered_free: day.8,
            });
        }
        Ok(out)
    }

    /// Fire-and-forget funnel event (button presses, non-link texts, ...).
    /// Never fails the caller: errors are logged and swallowed.
     pub async fn log_funnel_event(&self, bot_id: &str, user_id: i64, event: &str) {
        let bot_owned = bot_id.to_string();
        let event_owned = event.to_string();
        let result: Result<(), anyhow::Error> = self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO funnel_events (bot_id, user_telegram_id, event) VALUES (?1, ?2, ?3)",
                params![bot_owned, user_id, event_owned],
            )?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to log funnel event: {}", e));
        if let Err(e) = result {
            log::warn!("Funnel event dropped: {}", e);
        }
    }

    /// Top /start ref_codes by registrations in the last `days` days.
    /// `None` means a bare t.me link with no payload.
    pub async fn get_ref_stats(&self, days: i64) -> Result<Vec<(Option<String>, i64)>, anyhow::Error> {
        self.execute_with_timeout(move |conn| {
            let since = format!("date('now', '-{} days')", days - 1);
            let mut stmt = conn.prepare(&format!(
                "SELECT ref_code, COUNT(*) FROM users WHERE date(created_at) >= {since} GROUP BY ref_code ORDER BY COUNT(*) DESC LIMIT 10"
            ))?;
            let rows: Vec<(Option<String>, i64)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        }).await.map_err(|e| anyhow::anyhow!("Failed to get ref stats: {}", e))
    }

    /// Expire sessions whose LEASE has lapsed and return the rows that still need
    /// exactly one user notification, oldest first and bounded by `limit`.
    ///
    /// The predicate is the lease, not the row age: a heartbeat keeps an open
    /// session claimable, so a user still sitting on the webapp is never
    /// expired. `notified_at IS NULL` is the notify-once guard (a row already
    /// messaged is never returned again) and the LIMIT keeps a service start
    /// from flushing a backlog into chats in one burst.
    ///
    /// Each row is `(ymid, user_id, job_started)`: `job_started` says whether
    /// a download job really began for this row and is what picks the message -
    /// a session that never earned its ad is not a failed download. The row is
    /// never deleted; the funnel and the per-ymid joins depend on it.
    pub async fn expire_stale_pending(&self, limit: u32) -> Result<Vec<(String, i64, bool)>, anyhow::Error> {
        self.execute_with_timeout(move |conn| {
            let lease_secs = SESSION_LEASE_SECS;
            let candidates: Vec<(String, i64, bool)> = {
                let mut stmt = conn.prepare(&format!(
                    "SELECT id, user_id, job_started_at IS NOT NULL FROM pending_downloads WHERE status IN ('pending', 'verified') AND notified_at IS NULL AND COALESCE(lease_expires_at, datetime(created_at, '+{lease_secs} seconds')) < datetime('now') ORDER BY created_at ASC LIMIT ?1"
                ))?;
                stmt.query_map(params![limit as i64], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?
                .filter_map(|row| row.ok())
                .collect()
            };
            let mut expired_rows: Vec<(String, i64, bool)> = Vec::new();
            for (id, user_id, job_started) in candidates {
                // The lease and the notify-once flag are re-checked in the
                // write itself: a heartbeat that landed between the SELECT and
                // here must keep the row, and a second sweeper tick must not
                // notify the same row again.
                let expired = conn.execute(
                    &format!(
                        "UPDATE pending_downloads SET status = 'expired', notified_at = CURRENT_TIMESTAMP WHERE id = ?1 AND status IN ('pending', 'verified') AND notified_at IS NULL AND COALESCE(lease_expires_at, datetime(created_at, '+{lease_secs} seconds')) < datetime('now')"
                    ),
                    params![&id],
                )?;
                if expired == 1 {
                    expired_rows.push((id, user_id, job_started));
                }
            }
            Ok(expired_rows)
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to expire stale pending downloads: {}", e))
    }

    /// Check if user has active premium status **on one bot**.
    pub async fn is_user_premium(&self, bot_id: &str, user_id: i64) -> bool {
        let bot_owned = bot_id.to_string();
        let result = self.execute_with_timeout(move |conn| {
            let is_premium: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM users WHERE bot_id = ?1 AND telegram_id = ?2 AND premium_until > datetime('now'))",
                params![bot_owned, user_id],
                |row| row.get(0)
            )?;
            Ok(is_premium)
        }).await;

        result.unwrap_or(false)
    }

    /// Set or extend premium status for user **on one bot**.
    pub async fn set_user_premium(&self, bot_id: &str, user_id: i64, days: i64) -> Result<(), anyhow::Error> {
        let bot_owned = bot_id.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO users (bot_id, telegram_id, premium_until) 
                 VALUES (?1, ?2, datetime('now', '+' || ?3 || ' days'))
                 ON CONFLICT(bot_id, telegram_id) DO UPDATE SET 
                 premium_until = datetime(MAX(COALESCE(premium_until, datetime('now')), datetime('now')), '+' || ?3 || ' days')",
                params![bot_owned, user_id, days],
            )?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to set premium for user {} on bot {}: {}", user_id, bot_id, e))
    }

    /// Get list of users with active premium status **on one bot**.
    pub async fn get_premium_users(&self, bot_id: &str) -> Result<Vec<(i64, String, String)>, anyhow::Error> {
        let bot_owned = bot_id.to_string();
        self.execute_with_timeout(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT telegram_id, premium_until, COALESCE(last_active, 'N/A')
                 FROM users 
                 WHERE bot_id = ?1 AND premium_until > datetime('now')
                 ORDER BY premium_until DESC"
            )?;
            let users_iter = stmt.query_map(params![bot_owned], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut users = Vec::new();
            for user_result in users_iter {
                users.push(user_result?);
            }
            Ok(users)
        }).await.map_err(|e| anyhow::anyhow!("Failed to query premium users: {}", e))
    }

    /// Log a successful payment
    pub async fn log_payment(&self, bot_id: &str, user_id: i64, amount: i64, payload: &str) -> Result<(), anyhow::Error> {
        let bot_owned = bot_id.to_string();
        let payload = payload.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO payments (bot_id, user_id, amount, payload) VALUES (?1, ?2, ?3, ?4)",
                params![bot_owned, user_id, amount, payload],
            )?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to log payment: {}", e))
    }

    /// Log an invoice sent
    pub async fn log_invoice(&self, bot_id: &str, user_id: i64, amount: i64, payload: &str) -> Result<(), anyhow::Error> {
        let bot_owned = bot_id.to_string();
        let payload = payload.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO invoices (bot_id, user_id, amount, payload) VALUES (?1, ?2, ?3, ?4)",
                params![bot_owned, user_id, amount, payload],
            )?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to log invoice: {}", e))
    }

    /// Get rich daily statistics
    /// Rich daily stats. Pass admin ids in `exclude_admins` to get the same
    /// numbers without admin (test/self) traffic; empty slice = everyone.
    pub async fn get_rich_daily_stats(&self, exclude_admins: &[i64]) -> Result<RichDailyStats, anyhow::Error> {
        let excl_u = admin_filter_sql("telegram_id", exclude_admins);
        let excl_d = admin_filter_sql("user_telegram_id", exclude_admins);
        let excl_p = admin_filter_sql("user_id", exclude_admins);
        self.execute_with_timeout(move |conn| {
            // Basic counts today
            let unique_users: i64 = conn.query_row(
                &format!("SELECT COUNT(DISTINCT telegram_id) FROM users WHERE date(last_active) = date('now'){excl_u}"),
                [], |r| r.get(0)).unwrap_or(0);
            
            let yesterday_users: i64 = conn.query_row(
                &format!("SELECT COUNT(DISTINCT telegram_id) FROM users WHERE date(last_active) = date('now', '-1 day'){excl_u}"),
                [], |r| r.get(0)).unwrap_or(0);
            
            let unique_downloaders: i64 = conn.query_row(
                &format!("SELECT COUNT(DISTINCT user_telegram_id) FROM downloads WHERE date(download_date) = date('now'){excl_d}"),
                [], |r| r.get(0)).unwrap_or(0);

            let total_downloads: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM downloads WHERE date(download_date) = date('now'){excl_d}"),
                [], |r| r.get(0)).unwrap_or(0);

            let ad_impressions: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM pending_downloads WHERE date(created_at) = date('now'){excl_p}"),
                [], |r| r.get(0)).unwrap_or(0);

            let new_users: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM users WHERE date(created_at) = date('now'){excl_u}"),
                [], |r| r.get(0)).unwrap_or(0);

            // Payments & Revenue
            let payments_count: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM payments WHERE date(timestamp) = date('now'){excl_p}"),
                [], |r| r.get(0)).unwrap_or(0);

            let revenue_xtr: i64 = conn.query_row(
                &format!("SELECT COALESCE(SUM(amount), 0) FROM payments WHERE date(timestamp) = date('now'){excl_p}"),
                [], |r| r.get(0)).unwrap_or(0);

            let invoices_sent: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM invoices WHERE date(timestamp) = date('now'){excl_p}"),
                [], |r| r.get(0)).unwrap_or(0);

            // Peak hour
            let peak_hour_data = conn.query_row(
                &format!("SELECT strftime('%H', download_date) as hr, COUNT(*) as cnt 
                 FROM downloads WHERE date(download_date) = date('now'){excl_d}
                 GROUP BY hr ORDER BY cnt DESC LIMIT 1"),
                [], |r| Ok((r.get::<_, String>(0)?.parse::<u32>().unwrap_or(0), r.get::<_, i64>(1)?))
            ).ok();

            // Top 10 downloaders
            let mut stmt = conn.prepare(
                &format!("SELECT user_telegram_id, COUNT(*) as cnt 
                 FROM downloads WHERE date(download_date) = date('now'){excl_d}
                 GROUP BY user_telegram_id ORDER BY cnt DESC LIMIT 10")
            )?;
            let top_downloaders = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .filter_map(|r| r.ok()).collect();

            // 10 Last active
            let mut stmt = conn.prepare(
                &format!("SELECT telegram_id, strftime('%H:%M', last_active) 
                 FROM users WHERE date(last_active) = date('now'){excl_u}
                 ORDER BY last_active DESC LIMIT 10")
            )?;
            let last_active_users = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .filter_map(|r| r.ok()).collect();

            Ok(RichDailyStats {
                date: chrono::Local::now().format("%Y-%m-%d").to_string(),
                unique_users,
                unique_users_delta: unique_users - yesterday_users,
                unique_downloaders,
                total_downloads,
                ad_impressions,
                new_users,
                returning_users: unique_users - new_users,
                payments_count,
                revenue_xtr,
                invoices_sent,
                peak_hour: peak_hour_data,
                top_downloaders,
                last_active_users,
            })
        }).await.map_err(|e| anyhow::anyhow!("Failed to get rich daily stats: {}", e))
    }
}

/// Shared test schema bootstrap. Lives outside the test module because the
/// web_server's valued-postback tests need the same tables.
#[cfg(test)]
pub(crate) async fn setup_test_db() -> (DatabasePool, tempfile::NamedTempFile) {
    let temp_file = tempfile::NamedTempFile::new().unwrap();
    let db_path = temp_file.path().to_str().unwrap().to_string();
    let pool = DatabasePool::new(db_path.clone(), 1);

    // Initialize all necessary tables (mirrors production schema in old.rs,
    // including the multi-bot identity: composite (bot_id, telegram_id)).
    pool.execute_with_timeout(|conn| {
        conn.execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, telegram_id BIGINT NOT NULL, bot_id TEXT NOT NULL DEFAULT 'primary', last_active DATETIME DEFAULT CURRENT_TIMESTAMP, created_at DATETIME DEFAULT CURRENT_TIMESTAMP, quality_preference TEXT DEFAULT 'h264', premium_until DATETIME, lang TEXT DEFAULT NULL, ref_code TEXT DEFAULT NULL, UNIQUE(bot_id, telegram_id))",
            (),
        )?;
        conn.execute(
            "CREATE TABLE bots (bot_id TEXT PRIMARY KEY, username TEXT NOT NULL, created_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
            (),
        )?;
        conn.execute(
            "CREATE TABLE bot_settings (bot_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (bot_id, key))",
            (),
        )?;
        conn.execute(
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            (),
        )?;
        conn.execute(
            "CREATE TABLE pending_downloads (id TEXT PRIMARY KEY, user_id BIGINT NOT NULL, video_url TEXT NOT NULL, status TEXT DEFAULT 'pending', created_at DATETIME DEFAULT CURRENT_TIMESTAMP, notified_at DATETIME DEFAULT NULL, lease_expires_at DATETIME DEFAULT NULL, job_started_at DATETIME DEFAULT NULL, entry_ymid TEXT DEFAULT NULL, ad_requested_at DATETIME DEFAULT NULL, bot_id TEXT NOT NULL DEFAULT 'primary')",
            (),
        )?;
        conn.execute(
            "CREATE TABLE monetag_postbacks (id INTEGER PRIMARY KEY, ymid TEXT NOT NULL, event_type TEXT DEFAULT NULL, reward_event_type TEXT NOT NULL, estimated_price REAL DEFAULT NULL, request_var TEXT DEFAULT NULL, sub_zone_id TEXT DEFAULT NULL, zone_id TEXT DEFAULT NULL, telegram_user_id TEXT DEFAULT NULL, created_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
            (),
        )?;
        conn.execute(
            "CREATE TABLE mini_app_events (id INTEGER PRIMARY KEY, ymid TEXT NOT NULL, event TEXT NOT NULL, platform TEXT DEFAULT NULL, sdk_host TEXT DEFAULT NULL, user_agent TEXT DEFAULT NULL, created_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
            (),
        )?;
        conn.execute(
            "CREATE TABLE blocks (id INTEGER PRIMARY KEY, telegram_id BIGINT NOT NULL, blocked_at DATETIME DEFAULT CURRENT_TIMESTAMP, bot_id TEXT NOT NULL DEFAULT 'primary')",
            (),
        )?;
        conn.execute(
            "CREATE TABLE funnel_events (id INTEGER PRIMARY KEY, user_telegram_id BIGINT NOT NULL, event TEXT NOT NULL, created_at DATETIME DEFAULT CURRENT_TIMESTAMP, bot_id TEXT NOT NULL DEFAULT 'primary')",
            (),
        )?;
        conn.execute(
            "CREATE TABLE downloads (id INTEGER PRIMARY KEY, user_telegram_id BIGINT, video_url TEXT NOT NULL, download_date DATETIME DEFAULT CURRENT_TIMESTAMP, bot_id TEXT NOT NULL DEFAULT 'primary')",
            (),
        )?;
        Ok(())
    }).await.unwrap();

    (pool, temp_file)
}

/// Seed one pending row plus postback journals for gate tests.
/// `postbacks` holds (event_type, reward, age_secs): age 0 means now.
#[cfg(test)]
pub(crate) async fn setup_gate_row(
    pool: &DatabasePool,
    id: &str,
    status: &str,
    pending_age_secs: i64,
    postbacks: &[(&str, &str, i64)],
) {
    let id_owned = id.to_string();
    let status_owned = status.to_string();
    pool.execute_with_timeout(move |conn| {
        conn.execute(
            "INSERT INTO pending_downloads (id, user_id, video_url, status, created_at) VALUES (?1, 7, 'http://v', ?2, datetime('now', ?3))",
            params![id_owned, status_owned, format!("-{} seconds", pending_age_secs)],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    // Own every value before the loop: the async DB closure below must
    // capture owned data, never borrows into the caller's slice.
    let rows: Vec<(String, String, i64)> = postbacks
        .iter()
        .map(|(e, r, a)| (e.to_string(), r.to_string(), *a))
        .collect();
    for (event_owned, reward_owned, age) in rows {
        let id_owned = id.to_string();
        pool.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO monetag_postbacks (ymid, event_type, reward_event_type, created_at) VALUES (?1, ?2, ?3, datetime('now', ?4))",
                params![id_owned, event_owned, reward_owned, format!("-{} seconds", age)],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seconds between a row's creation and its lease deadline, computed by
    /// SQLite so the assertions never do timestamp arithmetic themselves.
    async fn lease_span_secs(pool: &DatabasePool, ymid: &str) -> i64 {
        let ymid_owned = ymid.to_string();
        pool.execute_with_timeout(move |conn| {
            conn.query_row(
                "SELECT CAST(strftime('%s', lease_expires_at) AS INTEGER) - CAST(strftime('%s', created_at) AS INTEGER) FROM pending_downloads WHERE id = ?1",
                params![ymid_owned],
                |row| row.get(0),
            )
        })
        .await
        .unwrap()
    }

    async fn shrink_lease_to_one_minute(pool: &DatabasePool, ymid: &str) {
        let ymid_owned = ymid.to_string();
        pool.execute_with_timeout(move |conn| {
            conn.execute(
                "UPDATE pending_downloads SET lease_expires_at = datetime('now', '+1 minute') WHERE id = ?1",
                params![ymid_owned],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_settings_get_set() {
        let (pool, _file) = setup_test_db().await;
        
        pool.set_setting("test_key", "test_value").await.unwrap();
        let value = pool.get_setting("test_key").await.unwrap();
        assert_eq!(value, "test_value");
        
        pool.set_setting("test_key", "new_value").await.unwrap();
        let value = pool.get_setting("test_key").await.unwrap();
        assert_eq!(value, "new_value");
    }

    #[tokio::test]
    async fn test_user_lang_override_and_effective() {
        let (pool, _file) = setup_test_db().await;
        let user_id = 555123456i64;

        // No override yet: device tag wins, missing tag means English.
        assert_eq!(pool.get_effective_lang("primary", user_id, Some("ru")).await, "ru");
        assert_eq!(pool.get_effective_lang("primary", user_id, None).await, "en");

        // Persist override: it wins over the device tag.
        pool.set_user_lang("primary", user_id, "uk").await.unwrap();
        assert_eq!(pool.get_user_lang("primary", user_id).await.unwrap(), Some("uk".to_string()));
        assert_eq!(pool.get_effective_lang("primary", user_id, Some("ru")).await, "uk");

        // Clearing restores device detection, even for a user that never set one.
        pool.clear_user_lang("primary", user_id).await.unwrap();
        assert_eq!(pool.get_user_lang("primary", user_id).await.unwrap(), None);
        assert_eq!(pool.get_effective_lang("primary", user_id, Some("ru")).await, "ru");
        pool.clear_user_lang("primary", 999888777).await.unwrap();
    }

    #[tokio::test]
    async fn test_lang_and_quality_are_per_bot() {
        let (pool, _file) = setup_test_db().await;
        let user_id = 777888999i64;

        // Same human, two bots: overrides and quality never cross.
        pool.set_user_lang("aaa", user_id, "uk").await.unwrap();
        assert_eq!(pool.get_user_lang("aaa", user_id).await.unwrap(), Some("uk".to_string()));
        assert_eq!(pool.get_user_lang("bbb", user_id).await.unwrap(), None);
        assert_eq!(pool.get_effective_lang("bbb", user_id, Some("ru")).await, "ru");
        assert_eq!(pool.get_effective_lang("aaa", user_id, Some("ru")).await, "uk");

        // Clearing on one bot leaves the other bot's override alone.
        pool.clear_user_lang("bbb", user_id).await.unwrap();
        assert_eq!(pool.get_user_lang("aaa", user_id).await.unwrap(), Some("uk".to_string()));

        // Quality reads are per-bot too (cache key includes the bot).
        // Both rows exist by now (the lang writes above created them)
        // with the DB default h264.
        assert_eq!(pool.get_user_quality("aaa", user_id).await.unwrap(), "h264");
        assert_eq!(pool.get_user_quality("bbb", user_id).await.unwrap(), "h264");
        // A quality change on one bot never crosses to the other.
        pool.execute_with_timeout(move |conn| {
            conn.execute(
                "UPDATE users SET quality_preference = 'h265' WHERE bot_id = 'aaa' AND telegram_id = ?1",
                params![user_id],
            )?;
            Ok(())
        }).await.unwrap();
        pool.invalidate_user_quality_cache("aaa", user_id).await;
        assert_eq!(pool.get_user_quality("aaa", user_id).await.unwrap(), "h265");
        assert_eq!(pool.get_user_quality("bbb", user_id).await.unwrap(), "h264");
    }

    #[tokio::test]
    async fn bots_register_list_and_refresh_username() {
        let (pool, _file) = setup_test_db().await;
        pool.register_bot("111", "alpha_bot").await.unwrap();
        pool.register_bot("222", "beta_bot").await.unwrap();
        let bots = pool.list_bots().await.unwrap();
        assert_eq!(bots.len(), 2);
        pool.register_bot("111", "alpha_renamed").await.unwrap();
        let bots = pool.list_bots().await.unwrap();
        assert_eq!(bots.len(), 2);
        assert!(bots.contains(&("111".to_string(), "alpha_renamed".to_string())));
    }

    #[tokio::test]
    async fn legacy_writes_default_to_primary_bot() {
        let (pool, _file) = setup_test_db().await;
        pool.execute_with_timeout(|conn| {
            conn.execute("INSERT INTO users (telegram_id) VALUES (42)", ())?;
            conn.execute("INSERT INTO pending_downloads (id, user_id, video_url) VALUES ('y1', 42, 'http://v')", ())?;
            conn.execute("INSERT INTO funnel_events (user_telegram_id, event) VALUES (42, 'start')", ())?;
            conn.execute("INSERT INTO blocks (telegram_id) VALUES (42)", ())?;
            conn.execute("INSERT INTO downloads (user_telegram_id, video_url) VALUES (42, 'http://v')", ())?;
            Ok(())
        }).await.unwrap();
        let bots: Vec<String> = pool.execute_with_timeout(|conn| {
            let mut out = Vec::new();
            for (table, cond) in [
                ("users", "telegram_id = 42"),
                ("pending_downloads", "id = 'y1'"),
                ("funnel_events", "user_telegram_id = 42"),
                ("blocks", "telegram_id = 42"),
                ("downloads", "user_telegram_id = 42"),
            ] {
                let b: String = conn.query_row(
                    &format!("SELECT bot_id FROM {table} WHERE {cond}"),
                    [],
                    |row| row.get(0),
                )?;
                out.push(b);
            }
            Ok(out)
        }).await.unwrap();
        assert!(bots.iter().all(|b| b == "primary"), "every legacy write lands on primary: {bots:?}");
    }

    #[tokio::test]
    async fn same_user_coexists_on_two_bots_but_not_twice_on_one() {
        let (pool, _file) = setup_test_db().await;
        pool.execute_with_timeout(|conn| {
            conn.execute("INSERT INTO users (telegram_id, bot_id) VALUES (42, 'aaa')", ())?;
            conn.execute("INSERT INTO users (telegram_id, bot_id) VALUES (42, 'bbb')", ())?;
            Ok(())
        }).await.unwrap();
        let dup = pool.execute_with_timeout(|conn| {
            conn.execute("INSERT INTO users (telegram_id, bot_id) VALUES (42, 'aaa')", ())
        }).await;
        assert!(dup.is_err(), "same (bot, user) twice must be rejected");
    }

    #[tokio::test]
    async fn bot_setting_falls_back_to_global_then_default() {
        let (pool, _file) = setup_test_db().await;
        assert_eq!(pool.resolve_bot_setting("aaa", "price", "50").await, "50");
        pool.set_setting("price", "75").await.unwrap();
        assert_eq!(pool.resolve_bot_setting("aaa", "price", "50").await, "75");
        pool.set_bot_setting("aaa", "price", "99").await.unwrap();
        assert_eq!(pool.resolve_bot_setting("aaa", "price", "50").await, "99");
        assert_eq!(pool.resolve_bot_setting("bbb", "price", "50").await, "75");
    }

    #[tokio::test]
    async fn test_record_block_and_weekly_stats() {
        let (pool, _file) = setup_test_db().await;
        pool.record_block("primary", 111).await.unwrap();
        pool.record_block("primary", 222).await.unwrap();

        let week = pool.get_weekly_stats(7, &[]).await.unwrap();
        assert_eq!(week.len(), 7);
        // Oldest first, newest last.
        assert!(week.first().unwrap().date < week.last().unwrap().date);
        let today = week.last().unwrap();
        assert_eq!(today.blocks, 2);
    }

    #[tokio::test]
    async fn test_expire_stale_pending_notifies_once() {
        let (pool, _file) = setup_test_db().await;
        pool.execute_with_timeout(|conn| {
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url) VALUES ('fresh', 1, 'http://x')",
                (),
            )?;
            // Stale row: 31 minutes old, never notified.
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, created_at) VALUES ('stale', 2, 'http://y', datetime('now', '-31 minutes'))",
                (),
            )?;
            Ok(())
        }).await.unwrap();

        let first = pool.expire_stale_pending(EXPIRY_BATCH_LIMIT).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, "stale");
        assert_eq!(first[0].1, 2);

        // Second run: nothing left to notify.
        let second = pool.expire_stale_pending(EXPIRY_BATCH_LIMIT).await.unwrap();
        assert!(second.is_empty());

        // Stale row is expired now, fresh row still pending.
        let status = pool.get_pending_download_status("stale").await.unwrap();
        assert_eq!(status, Some("expired".to_string()));
        let status = pool.get_pending_download_status("fresh").await.unwrap();
        assert_eq!(status, Some("pending".to_string()));
    }

    /// The row survives because its lease is alive, and the heartbeat moves
    /// that lease forward: the sweeper must leave the row alone.
    #[tokio::test]
    async fn heartbeat_extends_the_lease_and_the_sweeper_leaves_the_row_alone() {
        let (pool, _file) = setup_test_db().await;
        let ymid = pool.create_pending_download("primary", 42, "http://v").await.unwrap();
        assert_eq!(lease_span_secs(&pool, &ymid).await, SESSION_LEASE_SECS);

        // Pretend the last heartbeat was a while ago, so the deadline is close.
        shrink_lease_to_one_minute(&pool, &ymid).await;
        let (_, close_deadline) = pool.get_session_state(&ymid).await.unwrap().unwrap();
        let close_deadline = close_deadline.unwrap();

        let (status, renewed) = pool
            .refresh_session_lease(&ymid, SESSION_LEASE_SECS)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, "pending");
        assert!(
            renewed > close_deadline,
            "a heartbeat must push the deadline forward: {} is not later than {}",
            renewed,
            close_deadline
        );

        let swept = pool.expire_stale_pending(EXPIRY_BATCH_LIMIT).await.unwrap();
        assert!(swept.is_empty(), "a live lease must not be expired");
        assert_eq!(
            pool.get_pending_download_status(&ymid).await.unwrap(),
            Some("pending".to_string())
        );
    }

    /// A lapsed lease is expired and notified exactly once, and the row is
    /// kept (never deleted) so the funnel and per-ymid joins still work.
    #[tokio::test]
    async fn lapsed_lease_is_expired_exactly_once_and_the_row_is_kept() {
        let (pool, _file) = setup_test_db().await;
        pool.execute_with_timeout(|conn| {
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, lease_expires_at) VALUES ('lapsed', 5, 'http://v', datetime('now', '-1 minute'))",
                (),
            )?;
            Ok(())
        }).await.unwrap();

        let first = pool.expire_stale_pending(EXPIRY_BATCH_LIMIT).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, "lapsed");

        let second = pool.expire_stale_pending(EXPIRY_BATCH_LIMIT).await.unwrap();
        assert!(second.is_empty(), "notify-once: the second tick is silent");

        let (status, lease) = pool.get_session_state("lapsed").await.unwrap().unwrap();
        assert_eq!(status, "expired");
        assert!(lease.is_some(), "the row itself must survive, not be deleted");
    }

    /// Stale-state probe: a heartbeat for a row that is already delivered is a
    /// no-op. It must not extend anything and must not resurrect the row.
    #[tokio::test]
    async fn heartbeat_for_a_completed_row_changes_nothing() {
        let (pool, _file) = setup_test_db().await;
        pool.execute_with_timeout(|conn| {
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, status, lease_expires_at) VALUES ('done', 6, 'http://v', 'completed', datetime('now', '+10 minutes'))",
                (),
            )?;
            Ok(())
        }).await.unwrap();
        let (_, deadline_before) = pool.get_session_state("done").await.unwrap().unwrap();

        assert!(
            pool.refresh_session_lease("done", SESSION_LEASE_SECS)
                .await
                .unwrap()
                .is_none(),
            "a terminal row has no lease left to extend"
        );
        assert!(
            pool.refresh_session_lease("never-existed", SESSION_LEASE_SECS)
                .await
                .unwrap()
                .is_none(),
            "an unknown ymid has no lease"
        );

        let (status, deadline_after) = pool.get_session_state("done").await.unwrap().unwrap();
        assert_eq!(status, "completed");
        assert_eq!(
            deadline_after,
            deadline_before,
            "the deadline of a delivered row must be untouched"
        );
        assert!(pool.expire_stale_pending(EXPIRY_BATCH_LIMIT).await.unwrap().is_empty());
    }

    /// The ceiling holds: no amount of pinging can push a row's lease past
    /// the ceiling from its creation.
    #[tokio::test]
    async fn heartbeat_cannot_push_the_lease_past_the_ceiling() {
        let (pool, _file) = setup_test_db().await;
        pool.execute_with_timeout(|conn| {
            // 258300s = 2d23h45m, so a fresh 30-minute lease overshoots the 3-day
            // ceiling and the `min()` clamp is what sets this row's deadline. Two
            // traps: an age just under the ceiling leaves it less than the lease
            // away, so the lease is the earlier of the two and the clamp is
            // unobservable; and SQLite rejects a multi-unit modifier ("-2 days 23
            // hours 45 minutes" returns NULL), which silently gives the row a
            // NULL created_at, hence a NULL ceiling and a NULL lease - so the age
            // must stay single-unit.
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, created_at) VALUES ('almost-at-ceiling', 8, 'http://v', datetime('now', '-258300 seconds'))",
                (),
            )?;
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url) VALUES ('brand-new', 9, 'http://v')",
                (),
            )?;
            Ok(())
        }).await.unwrap();

        for ymid in ["brand-new", "almost-at-ceiling"] {
            assert!(
                pool.refresh_session_lease(ymid, SESSION_LEASE_SECS)
                    .await
                    .unwrap()
                    .is_some(),
                "{} must still be claimable",
                ymid
            );
        }
        assert!(lease_span_secs(&pool, "brand-new").await >= SESSION_LEASE_SECS);
        assert_eq!(
            lease_span_secs(&pool, "almost-at-ceiling").await,
            SESSION_LEASE_CEILING_SECS,
            "a session 2d23h45m old is clamped to the ceiling, not extended by 30 min"
        );
        // The invariant itself, checked for a clamped and an unclamped row alike.
        for ymid in ["brand-new", "almost-at-ceiling"] {
            assert!(
                lease_span_secs(&pool, ymid).await <= SESSION_LEASE_CEILING_SECS,
                "{} must never be pushed past the ceiling",
                ymid
            );
        }
        assert!(pool.expire_stale_pending(EXPIRY_BATCH_LIMIT).await.unwrap().is_empty());
    }

    /// The message split depends on this column and nothing else: a row whose
    /// download never started is a lapsed session, a row with a started job
    /// is a genuine failure.
    #[tokio::test]
    async fn expiry_reports_whether_a_download_job_really_started() {
        let (pool, _file) = setup_test_db().await;
        pool.execute_with_timeout(|conn| {
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, lease_expires_at) VALUES ('never-earned', 1, 'http://x', datetime('now', '-1 minute'))",
                (),
            )?;
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, lease_expires_at, job_started_at) VALUES ('job-running', 2, 'http://y', datetime('now', '-1 minute'), CURRENT_TIMESTAMP)",
                (),
            )?;
            Ok(())
        }).await.unwrap();

        assert_eq!(pool.mark_job_started("never-earned").await.unwrap(), 1);
        pool.execute_with_timeout(|conn| {
            conn.execute(
                "UPDATE pending_downloads SET lease_expires_at = datetime('now', '-1 minute'), job_started_at = NULL WHERE id = 'never-earned'",
                (),
            )?;
            Ok(())
        }).await.unwrap();

        let mut swept = pool.expire_stale_pending(EXPIRY_BATCH_LIMIT).await.unwrap();
        swept.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(swept.len(), 2);
        assert_eq!(swept[0].0, "job-running");
        assert!(swept[0].2, "a started job reports job_started");
        assert_eq!(swept[1].0, "never-earned");
        assert!(!swept[1].2, "a row with no job reports job_started = false");
    }

    /// A backlog must never be flushed into chats in one burst: the tick is
    /// bounded and the rest waits for the next one.
    #[tokio::test]
    async fn expiry_batch_is_bounded_and_the_rest_waits_for_later_ticks() {
        let (pool, _file) = setup_test_db().await;
        let total = EXPIRY_BATCH_LIMIT as usize + 5;
        pool.execute_with_timeout(move |conn| {
            for i in 0..total {
                conn.execute(
                    "INSERT INTO pending_downloads (id, user_id, video_url, created_at, lease_expires_at) VALUES (?1, ?2, 'http://v', datetime('now', ?3), datetime('now', '-1 minute'))",
                    params![format!("row-{:05}", i), i as i64, format!("-{} minutes", total - i)],
                )?;
            }
            Ok(())
        }).await.unwrap();

        let first = pool.expire_stale_pending(EXPIRY_BATCH_LIMIT).await.unwrap();
        assert_eq!(first.len(), EXPIRY_BATCH_LIMIT as usize);
        // Oldest first: row-00000 is the oldest of the batch.
        assert_eq!(first[0].0, "row-00000");

        let second = pool.expire_stale_pending(EXPIRY_BATCH_LIMIT).await.unwrap();
        assert_eq!(second.len(), 5, "the remainder goes out on the next tick");
        assert!(
            pool.expire_stale_pending(EXPIRY_BATCH_LIMIT).await.unwrap().is_empty()
        );
    }

    /// A row can be failed twice only if the write is unguarded: the sweeper
    /// and a slow job must not both own the same notification.
    #[tokio::test]
    async fn mark_pending_failed_only_moves_a_non_terminal_row() {
        let (pool, _file) = setup_test_db().await;
        pool.execute_with_timeout(|conn| {
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, status) VALUES ('delivered', 1, 'http://x', 'completed')",
                (),
            )?;
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, status) VALUES ('live', 2, 'http://y', 'pending')",
                (),
            )?;
            Ok(())
        }).await.unwrap();

        assert_eq!(pool.mark_pending_failed("delivered").await.unwrap(), 0);
        assert_eq!(
            pool.get_pending_download_status("delivered").await.unwrap(),
            Some("completed".to_string()),
            "a delivered row must not be flipped back to failed"
        );

        assert_eq!(pool.mark_pending_failed("live").await.unwrap(), 1);
        assert_eq!(pool.mark_pending_failed("live").await.unwrap(), 0);
        assert_eq!(
            pool.get_pending_download_status("live").await.unwrap(),
            Some("failed".to_string())
        );
    }

    #[tokio::test]
    async fn test_ref_stats_breakdown() {
        let (pool, _file) = setup_test_db().await;
        pool.execute_with_timeout(|conn| {
            conn.execute("INSERT INTO users (telegram_id, ref_code) VALUES (1, 'site')", ())?;
            conn.execute("INSERT INTO users (telegram_id, ref_code) VALUES (2, 'site')", ())?;
            conn.execute("INSERT INTO users (telegram_id) VALUES (3)", ())?;
            // Old user outside the window: must not count.
            conn.execute("INSERT INTO users (telegram_id, ref_code, created_at) VALUES (4, 'old', datetime('now', '-30 days'))", ())?;
            Ok(())
        }).await.unwrap();

        let refs = pool.get_ref_stats(7).await.unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0], (Some("site".to_string()), 2));
        assert_eq!(refs[1], (None, 1));
    }

    #[tokio::test]
    async fn test_funnel_events_and_stats_with_admin_filter() {
        let (pool, _file) = setup_test_db().await;
        // Users 1..3: full journey for user 1, link-only for user 2,
        // silent starter for user 3. User 999 is admin noise.
        for u in [1i64, 2, 3, 999] {
            pool.execute_with_timeout(move |conn| {
                conn.execute(
                    "INSERT INTO users (telegram_id) VALUES (?1)",
                    params![u],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        }
        pool.log_funnel_event("primary", 1, "start").await;
        pool.log_funnel_event("primary", 2, "text_no_link").await;
        pool.execute_with_timeout(|conn| {
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, status) VALUES ('a', 1, 'http://x', 'completed')",
                (),
            )?;
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, status) VALUES ('b', 2, 'http://y', 'expired')",
                (),
            )?;
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, status) VALUES ('c', 999, 'http://z', 'completed')",
                (),
            )?;
            conn.execute(
                "INSERT INTO downloads (user_telegram_id, video_url) VALUES (1, 'http://x')",
                (),
            )?;
            conn.execute(
                "INSERT INTO downloads (user_telegram_id, video_url) VALUES (999, 'http://z')",
                (),
            )?;
            Ok(())
        })
        .await
        .unwrap();

        // Funnel events recorded (fire-and-forget API).
        let n: i64 = pool
            .execute_with_timeout(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM funnel_events WHERE user_telegram_id = 1 AND event = 'start'",
                    [],
                    |r| r.get(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(n, 1);

        // All traffic: S0=4 starters.
        let all = &pool.get_funnel_stats(7, &[]).await.unwrap()[6];
        assert_eq!(all.started, 4);
        assert_eq!(all.link_sent, 3);
        assert_eq!(all.delivered, 2);

        // Without admin: admin rows vanish from every stage.
        let clean = &pool.get_funnel_stats(7, &[999]).await.unwrap()[6];
        assert_eq!(clean.started, 3);
        assert_eq!(clean.link_sent, 2);
        assert_eq!(clean.ad_watched, 1);
        assert_eq!(clean.claimed, 1);
        assert_eq!(clean.delivered, 1);
        assert_eq!(clean.expired, 1);
        assert_eq!(clean.failed, 0);
        assert_eq!(clean.delivered_paid, 0);
        assert_eq!(clean.delivered_free, 0);

        // Delivery reasons split paid vs free giveaways.
        pool.log_funnel_event("primary", 1, "delivered_valued").await;
        pool.log_funnel_event("primary", 2, "delivered_timer").await;
        let split = &pool.get_funnel_stats(7, &[999]).await.unwrap()[6];
        assert_eq!(split.delivered_paid, 1);
        assert_eq!(split.delivered_free, 1);
    }

    #[tokio::test]
    async fn test_get_nonexistent_setting() {
        let (pool, _file) = setup_test_db().await;
        let result = pool.get_setting("ghost").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_premium_activation_and_check() {
        let (pool, _file) = setup_test_db().await;
        let user_id = 123456789i64;

        // Initially not premium
        assert!(!pool.is_user_premium("aaa", user_id).await);

        // Activate premium
        pool.set_user_premium("aaa", user_id, 30).await.unwrap();

        // Now is premium
        assert!(pool.is_user_premium("aaa", user_id).await);

        // Check premium users list
        let premium_users = pool.get_premium_users("aaa").await.unwrap();
        assert_eq!(premium_users.len(), 1);
        assert_eq!(premium_users[0].0, user_id);
    }

    #[tokio::test]
    async fn test_premium_is_invisible_to_another_bot() {
        let (pool, _file) = setup_test_db().await;
        let user_id = 777i64;
        pool.set_user_premium("aaa", user_id, 30).await.unwrap();
        assert!(pool.is_user_premium("aaa", user_id).await);
        assert!(!pool.is_user_premium("bbb", user_id).await);
    }

    #[tokio::test]
    async fn test_premium_extension() {
        let (pool, _file) = setup_test_db().await;
        let user_id = 987654321i64;

        // Set initial premium
        pool.set_user_premium("aaa", user_id, 30).await.unwrap();
        let first_expiry = pool.get_premium_users("aaa").await.unwrap()[0].1.clone();

        // Extend premium
        pool.set_user_premium("aaa", user_id, 30).await.unwrap();
        let second_expiry = pool.get_premium_users("aaa").await.unwrap()[0].1.clone();

        // Second expiry should be later than first
        assert!(second_expiry > first_expiry);
    }

    #[tokio::test]
    async fn test_get_premium_users_filtering() {
        let (pool, _file) = setup_test_db().await;
        
        // Add active premium user
        pool.set_user_premium("aaa", 1, 30).await.unwrap();
        
        // Add expired premium user
        pool.execute_with_timeout(|conn| {
            conn.execute(
                "INSERT INTO users (telegram_id, premium_until) VALUES (?1, datetime('now', '-1 day'))",
                params![2i64],
            )?;
            Ok(())
        }).await.unwrap();

        let premium_users = pool.get_premium_users("aaa").await.unwrap();
        assert_eq!(premium_users.len(), 1);
        assert_eq!(premium_users[0].0, 1);
    }

    #[tokio::test]
    async fn bot_id_resolves_per_ymid_and_misses_unknown() {
        let (pool, _file) = setup_test_db().await;
        pool.execute_with_timeout(|conn| {
            conn.execute("INSERT INTO pending_downloads (id, user_id, video_url, bot_id) VALUES ('known', 7, 'http://v', 'zzz')", ())?;
            conn.execute("INSERT INTO pending_downloads (id, user_id, video_url) VALUES ('legacy', 7, 'http://v')", ())?;
            Ok(())
        }).await.unwrap();
        assert_eq!(
            pool.get_bot_id_by_ymid("known").await.unwrap(),
            Some("zzz".to_string())
        );
        assert_eq!(
            pool.get_bot_id_by_ymid("legacy").await.unwrap(),
            Some("primary".to_string())
        );
        assert_eq!(pool.get_bot_id_by_ymid("nope").await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_claim_if_ad_presented_valued_impression_delivers() {
        let (pool, _file) = setup_test_db().await;
        setup_gate_row(
            &pool,
            "gate-valued",
            "verified",
            30,
            &[("impression", "valued", 20), ("click", "non_valued", 5)],
        )
        .await;
        let (user_id, url, via) = pool.claim_if_ad_presented("gate-valued").await.unwrap();
        assert_eq!(user_id, 7);
        assert_eq!(url, "http://v");
        assert_eq!(via, ClaimVia::Verified);
        assert_eq!(
            pool.get_pending_download_status("gate-valued")
                .await
                .unwrap(),
            Some("completed".to_string())
        );
    }

    /// The deleted rules in one test: a valued impression that landed now,
    /// with no click and no age to spare, still delivers. Monetag already
    /// priced this impression, so our own timers must not refuse it.
    #[tokio::test]
    async fn test_claim_if_ad_presented_delivers_without_click_or_age() {
        let (pool, _file) = setup_test_db().await;
        setup_gate_row(
            &pool,
            "gate-fresh",
            "verified",
            1,
            &[("impression", "valued", 0)],
        )
        .await;
        let (user_id, url, via) = pool.claim_if_ad_presented("gate-fresh").await.unwrap();
        assert_eq!(user_id, 7);
        assert_eq!(url, "http://v");
        assert_eq!(via, ClaimVia::Verified);
        assert_eq!(
            pool.get_pending_download_status("gate-fresh")
                .await
                .unwrap(),
            Some("completed".to_string())
        );
    }

    #[tokio::test]
    async fn test_claim_if_ad_presented_no_postback_err() {
        let (pool, _file) = setup_test_db().await;
        setup_gate_row(&pool, "gate-silent", "verified", 600, &[]).await;
        assert!(pool.claim_if_ad_presented("gate-silent").await.is_err());
        assert_eq!(
            pool.get_pending_download_status("gate-silent")
                .await
                .unwrap(),
            Some("verified".to_string())
        );
    }

    #[tokio::test]
    async fn test_claim_if_ad_presented_non_valued_impression_still_unlocks() {
        let (pool, _file) = setup_test_db().await;
        // Monetag priced this display at zero, but the user still sat through
        // the ad, so the video is owed. Revenue is tracked separately; the
        // reward split must not decide what the user gets.
        setup_gate_row(
            &pool,
            "gate-free",
            "verified",
            600,
            &[("impression", "non_valued", 595)],
        )
        .await;
        let (user_id, url, via) = pool.claim_if_ad_presented("gate-free").await.unwrap();
        assert_eq!(user_id, 7);
        assert_eq!(url, "http://v");
        assert_eq!(via, ClaimVia::Verified);
        assert_eq!(
            pool.get_pending_download_status("gate-free").await.unwrap(),
            Some("completed".to_string())
        );
    }

    #[tokio::test]
    async fn test_claim_if_ad_presented_a_click_alone_never_unlocks() {
        let (pool, _file) = setup_test_db().await;
        // A click is a duplicate of a display we must already have journaled,
        // so on its own it is not proof that an ad ran.
        setup_gate_row(
            &pool,
            "gate-click-only",
            "verified",
            600,
            &[("click", "valued", 599)],
        )
        .await;
        assert!(pool.claim_if_ad_presented("gate-click-only").await.is_err());
        assert_eq!(
            pool.get_pending_download_status("gate-click-only").await.unwrap(),
            Some("verified".to_string())
        );
    }

    #[tokio::test]
    async fn test_claim_if_ad_presented_backstop_gone() {
        let (pool, _file) = setup_test_db().await;
        // 20s-old pending row with zero postbacks: the deleted timer backstop
        // would have delivered this, the valued gate must not.
        setup_gate_row(&pool, "gate-stale", "pending", 20, &[]).await;
        assert!(pool.claim_if_ad_presented("gate-stale").await.is_err());
        assert_eq!(
            pool.get_pending_download_status("gate-stale")
                .await
                .unwrap(),
            Some("pending".to_string())
        );
    }

    /// The postback path marks the row verified before it claims, so a row
    /// still sitting in `pending` is not deliverable yet.
    #[tokio::test]
    async fn test_claim_if_ad_presented_requires_verified_status() {
        let (pool, _file) = setup_test_db().await;
        setup_gate_row(
            &pool,
            "gate-pending",
            "pending",
            5,
            &[("impression", "valued", 0)],
        )
        .await;
        assert!(pool.claim_if_ad_presented("gate-pending").await.is_err());
        assert_eq!(
            pool.get_pending_download_status("gate-pending")
                .await
                .unwrap(),
            Some("pending".to_string())
        );
    }

    /// Replay probe: the atomic claim is single-use, so a valued postback that
    /// lands on an already-delivered row can neither redeliver nor resurrect
    /// the row.
    #[tokio::test]
    async fn test_claim_if_ad_presented_completed_row_never_delivers_twice() {
        let (pool, _file) = setup_test_db().await;
        setup_gate_row(
            &pool,
            "gate-once",
            "verified",
            10,
            &[("impression", "valued", 5)],
        )
        .await;
        assert!(pool.claim_if_ad_presented("gate-once").await.is_ok());

        // Duplicate postbacks and a second client POST both arrive here.
        assert!(pool.claim_if_ad_presented("gate-once").await.is_err());
        assert!(pool.claim_if_ad_presented("gate-once").await.is_err());
        assert_eq!(
            pool.get_pending_download_status("gate-once").await.unwrap(),
            Some("completed".to_string())
        );
    }

    /// Race probe: two claims at once, exactly one wins.
    #[tokio::test]
    async fn test_claim_if_ad_presented_two_concurrent_claims_have_one_winner() {
        let (pool, _file) = setup_test_db().await;
        setup_gate_row(
            &pool,
            "gate-race",
            "verified",
            10,
            &[("impression", "valued", 5)],
        )
        .await;
        let pool = Arc::new(pool);
        let first_pool = pool.clone();
        let second_pool = pool.clone();

        let (first, second) = tokio::join!(
            first_pool.claim_if_ad_presented("gate-race"),
            second_pool.claim_if_ad_presented("gate-race")
        );

        let winners = [first, second].iter().filter(|r| r.is_ok()).count();
        assert_eq!(winners, 1, "exactly one of two concurrent claims must win");
        assert_eq!(
            pool.get_pending_download_status("gate-race").await.unwrap(),
            Some("completed".to_string())
        );
    }

    /// Honesty probe: the affected-row count is what tells the postback
    /// handler "this moved the row" from "this changed nothing".
    #[tokio::test]
    async fn test_mark_as_verified_reports_row_changes_only() {
        let (pool, _file) = setup_test_db().await;
        setup_gate_row(&pool, "verify-me", "pending", 5, &[]).await;
        setup_gate_row(&pool, "verify-terminal", "completed", 5, &[]).await;

        assert_eq!(
            pool.mark_as_verified_with_logging("verify-me")
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            pool.mark_as_verified_with_logging("verify-me")
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            pool.mark_as_verified_with_logging("verify-terminal")
                .await
                .unwrap(),
            0
        );
        // An unknown ymid is not a failure: the UPDATE simply matches no row.
        // Reporting that as an error would make the caller bail into DbError and
        // lose the truthful distinction, because the row itself is what says why
        // nothing changed - here: there is no row at all (a `None` status, which
        // the caller classifies as an unknown ymid rather than a terminal one).
        assert_eq!(
            pool.mark_as_verified_with_logging("no-such-ymid")
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            pool.get_pending_download_status("no-such-ymid").await.unwrap(),
            None
        );
    }

    /// The two reads the mini-app's heartbeat and status probe depend on have a
    /// fixed shape, and both "no deadline" cases must stay distinguishable from a
    /// live one: the client treats an absent deadline as "keep waiting quietly"
    /// and only ever compares a deadline the server handed it.
    #[tokio::test]
    async fn session_state_reports_status_and_deadline_in_one_read() {
        let (pool, _file) = setup_test_db().await;
        let live = pool.create_pending_download("primary", 11, "http://v").await.unwrap();
        pool.execute_with_timeout(|conn| {
            // A row from before the lease column existed: the status is real,
            // the deadline simply does not exist yet.
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, status) VALUES ('pre-lease-row', 12, 'http://v', 'pending')",
                (),
            )?;
            Ok(())
        })
        .await
        .unwrap();

        let (status, deadline) = pool.get_session_state(&live).await.unwrap().unwrap();
        assert_eq!(status, "pending");
        assert!(
            deadline.is_some(),
            "a row created by the current build must carry a deadline"
        );

        let (legacy_status, legacy_deadline) =
            pool.get_session_state("pre-lease-row").await.unwrap().unwrap();
        assert_eq!(legacy_status, "pending");
        assert!(
            legacy_deadline.is_none(),
            "a row without a lease reports none - never a computed guess"
        );

        // An unknown ymid reports no row at all, which is what the status
        // endpoint turns into `not_found`.
        assert!(
            pool.get_session_state("never-existed")
                .await
                .unwrap()
                .is_none()
        );

        // Positive control: the heartbeat is what fills that gap in.
        assert!(pool
            .refresh_session_lease("pre-lease-row", SESSION_LEASE_SECS)
            .await
            .unwrap()
            .is_some());
        assert!(pool
            .get_session_state("pre-lease-row")
            .await
            .unwrap()
            .unwrap()
            .1
            .is_some());
    }

    /// The failure message hangs off this one column, so it is written once and
    /// never moved: a repeated mark (the job's own error path after the sweeper
    /// already looked) keeps the original start time, and an unknown ymid moves
    /// nothing at all.
    #[tokio::test]
    async fn job_start_marker_is_written_once_and_is_row_scoped() {
        let (pool, _file) = setup_test_db().await;
        let ymid = pool.create_pending_download("primary", 21, "http://v").await.unwrap();

        assert_eq!(pool.mark_job_started("no-such-ymid").await.unwrap(), 0);
        assert_eq!(
            row_count(&pool, "no-such-ymid").await,
            0,
            "an unknown ymid must not be invented"
        );

        assert_eq!(pool.mark_job_started(&ymid).await.unwrap(), 1);
        let first = job_started_at(&pool, &ymid).await;
        assert!(first.is_some(), "the first mark stamps the column");

        assert_eq!(pool.mark_job_started(&ymid).await.unwrap(), 1);
        assert_eq!(
            job_started_at(&pool, &ymid).await,
            first,
            "a repeated mark must not move the original start time"
        );
    }

    async fn job_started_at(pool: &DatabasePool, ymid: &str) -> Option<String> {
        let ymid_owned = ymid.to_string();
        pool.execute_with_timeout(move |conn| {
            conn.query_row(
                "SELECT job_started_at FROM pending_downloads WHERE id = ?1",
                params![ymid_owned],
                |row| row.get::<_, Option<String>>(0),
            )
        })
        .await
        .unwrap()
    }

    async fn row_count(pool: &DatabasePool, ymid: &str) -> i64 {
        let ymid_owned = ymid.to_string();
        pool.execute_with_timeout(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM pending_downloads WHERE id = ?1",
                params![ymid_owned],
                |row| row.get(0),
            )
        })
        .await
        .unwrap()
    }

    async fn user_rows(pool: &DatabasePool, user_id: i64) -> i64 {
        pool.execute_with_timeout(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM pending_downloads WHERE user_id = ?1",
                params![user_id],
                |row| row.get(0),
            )
        })
        .await
        .unwrap()
    }

    async fn entry_ymid_of(pool: &DatabasePool, ymid: &str) -> Option<String> {
        let ymid_owned = ymid.to_string();
        pool.execute_with_timeout(move |conn| {
            conn.query_row(
                "SELECT entry_ymid FROM pending_downloads WHERE id = ?1",
                params![ymid_owned],
                |row| row.get(0),
            )
            .optional()
        })
        .await
        .unwrap()
    }

    /// The reuse predicate is `ad_requested_at IS NULL`, and nothing else. The
    /// impression postback lags the ad by seconds, so a predicate built on the
    /// impression journal would let a second press inside that window reuse a
    /// ymid that already carried an ad: two ad events on one ymid, priced at
    /// zero by Monetag. This test fails the moment anyone swaps the marker.
    #[tokio::test]
    async fn a_second_press_inside_the_postback_lag_window_mints_a_new_ymid() {
        let (pool, _file) = setup_test_db().await;
        let entry = pool.create_pending_download("primary", 31, "http://v").await.unwrap();

        // The ad was requested, but no impression postback has landed yet - the
        // exact window in which an impression-based check would wrongly reuse.
        pool.mark_ad_requested(&entry).await.unwrap();
        assert!(!pool.has_ad_impression(&entry).await.unwrap());

        let rotated = pool.resolve_session_ymid(&entry).await.unwrap().unwrap();
        assert!(!rotated.reused, "a spent ymid must never be reused");
        assert_ne!(rotated.ymid, entry, "one ad event, one ymid");

        // Even once the impression journal has caught up, the request-time
        // marker keeps the ymid spent: the two signals are independent.
        pool.mark_ad_requested(&rotated.ymid).await.unwrap();
        pool.log_postback(&rotated.ymid, Some("impression"), "valued", None, None, None, None, None)
            .await
            .unwrap();
        let again = pool
            .resolve_session_ymid(&rotated.ymid)
            .await
            .unwrap()
            .unwrap();
        assert!(!again.reused);
        assert_ne!(again.ymid, rotated.ymid);
    }

    /// The common case must stay cheap: a fresh press on a ymid that has never
    /// carried an ad reuses that exact row, and creates nothing.
    #[tokio::test]
    async fn rotating_a_live_unused_session_is_a_no_op() {
        let (pool, _file) = setup_test_db().await;
        let entry = pool.create_pending_download("primary", 32, "http://v").await.unwrap();
        let before = user_rows(&pool, 32).await;

        let resolved = pool.resolve_session_ymid(&entry).await.unwrap().unwrap();
        assert_eq!(resolved.ymid, entry, "a live unused session is kept as-is");
        assert!(resolved.reused);
        assert_eq!(user_rows(&pool, 32).await, before, "no second row");
        assert_eq!(row_count(&pool, &entry).await, 1);
    }

    /// The minted row is the same video for the same user, and remembers the
    /// button ymid it came from so a press can be joined to its session. The
    /// copied user is also what preserves the language: the effective locale is
    /// resolved from that user's stored override, else the ?lang= tag.
    #[tokio::test]
    async fn a_spent_entry_ymid_mints_a_row_for_the_same_user_and_video() {
        let (pool, _file) = setup_test_db().await;
        pool.set_user_lang("primary", 33, "uk").await.unwrap();
        let entry = pool.create_pending_download("primary", 33, "http://the-video").await.unwrap();
        pool.mark_ad_requested(&entry).await.unwrap();

        let rotated = pool.resolve_session_ymid(&entry).await.unwrap().unwrap();
        assert!(!rotated.reused);
        assert_eq!(pool.get_user_id_by_ymid(&rotated.ymid).await.unwrap(), 33);
        assert_eq!(entry_ymid_of(&pool, &rotated.ymid).await, Some(entry.clone()));

        let (minted_user, minted_url): (i64, String) = pool
            .execute_with_timeout({
                let ymid = rotated.ymid.clone();
                move |conn| {
                    conn.query_row(
                        "SELECT user_id, video_url FROM pending_downloads WHERE id = ?1",
                        params![ymid],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                }
            })
            .await
            .unwrap();
        assert_eq!(minted_user, 33);
        assert_eq!(minted_url, "http://the-video");
        assert_eq!(
            pool.get_effective_lang("primary", minted_user, None).await,
            "uk",
            "the copied user carries the language"
        );
        // A fresh row must be claimable from the first second.
        assert_eq!(
            lease_span_secs(&pool, &rotated.ymid).await,
            SESSION_LEASE_SECS
        );
    }

    /// A terminal entry ymid is spent even if it never carried an ad (it was
    /// delivered or it lapsed), and a second press of the SAME button ymid that
    /// happens while the user still has a fresh unused session gets that session
    /// instead of a new row.
    #[tokio::test]
    async fn a_terminal_entry_row_is_replaced_and_a_sibling_session_is_preferred() {
        let (pool, _file) = setup_test_db().await;
        pool.execute_with_timeout(|conn| {
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, status, lease_expires_at) VALUES ('done', 34, 'http://v', 'completed', datetime('now', '+10 minutes'))",
                (),
            )?;
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, lease_expires_at) VALUES ('sibling', 34, 'http://v', datetime('now', '+10 minutes'))",
                (),
            )?;
            Ok(())
        })
        .await
        .unwrap();

        let resolved = pool.resolve_session_ymid("done").await.unwrap().unwrap();
        assert_eq!(
            resolved.ymid, "sibling",
            "the user's fresh unused session wins over minting"
        );
        assert!(resolved.reused);
        assert_eq!(user_rows(&pool, 34).await, 2, "still no orphan row");

        // With no usable sibling left, the terminal row is replaced by a mint.
        pool.mark_ad_requested("sibling").await.unwrap();
        let minted = pool.resolve_session_ymid("done").await.unwrap().unwrap();
        assert!(!minted.reused);
        assert_ne!(minted.ymid, "done");
        assert_eq!(user_rows(&pool, 34).await, 3);
    }

    /// An unknown entry ymid has nothing to copy, so the client must keep the
    /// ymid it already has instead of being handed a broken session.
    #[tokio::test]
    async fn resolving_an_unknown_entry_ymid_yields_nothing() {
        let (pool, _file) = setup_test_db().await;
        assert!(pool
            .resolve_session_ymid("never-existed")
            .await
            .unwrap()
            .is_none());
    }

    async fn ad_requested_at(pool: &DatabasePool, ymid: &str) -> Option<String> {
        let ymid_owned = ymid.to_string();
        pool.execute_with_timeout(move |conn| {
            conn.query_row(
                "SELECT ad_requested_at FROM pending_downloads WHERE id = ?1",
                params![ymid_owned],
                |row| row.get::<_, Option<String>>(0),
            )
        })
        .await
        .unwrap()
    }

    /// The request-time marker rides the existing funnel beacon, and it is
    /// first-write-wins so a second show() call cannot move it.
    #[tokio::test]
    async fn the_ad_requested_beacon_stamps_the_marker_once() {
        let (pool, _file) = setup_test_db().await;
        let ymid = pool.create_pending_download("primary", 35, "http://v").await.unwrap();

        // Nothing is stamped before the client asks for an ad.
        assert!(ad_requested_at(&pool, &ymid).await.is_none());

        pool.log_mini_app_event(&ymid, AD_REQUESTED_EVENT, Some("ios"), None, None)
            .await
            .unwrap();
        let first = ad_requested_at(&pool, &ymid).await;
        assert!(first.is_some(), "the request-time beacon stamps the row");

        // A later stage must not move the marker.
        pool.log_mini_app_event(&ymid, "ad_resolve", Some("ios"), None, None)
            .await
            .unwrap();
        assert_eq!(
            ad_requested_at(&pool, &ymid).await,
            first,
            "first write wins"
        );
    }
}