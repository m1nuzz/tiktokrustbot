use rusqlite::{Connection, Result as SqliteResult, params, OptionalExtension};
use tokio::sync::{Semaphore, Mutex};
use tokio::time::{timeout, Duration};
use std::sync::Arc;
use lru::LruCache;
use std::num::NonZeroUsize;

pub struct DatabasePool {
    db_path: String,
    connection_semaphore: Arc<Semaphore>,
    // LRU cache with limit of 1000 users
    user_cache: Arc<Mutex<LruCache<i64, UserInfo>>>,
}

#[derive(Clone)]
pub struct UserInfo {
    pub quality_preference: String,
    pub last_updated: tokio::time::Instant,
}

/// How a download request was unlocked (for delivery-reason stats).
/// Verified = strict gate (valued impression plus watch plus click/90s);
/// Timer = legacy 20s age-rule backstop (removed, kept for historic rows);
/// Admin = admin bypass (no ad needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimVia {
    Verified,
    Timer,
    Admin,
}

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
    pub async fn get_user_quality(&self, user_id: i64) -> Result<String, anyhow::Error> {
        // Check LRU cache
        {
            let mut cache = self.user_cache.lock().await;
            if let Some(user_info) = cache.get(&user_id) {
                // Cache is valid for 5 minutes
                if user_info.last_updated.elapsed() < Duration::from_secs(300) {
                    log::info!("Using cached quality preference for user {}: {}", user_id, user_info.quality_preference);
                    return Ok(user_info.quality_preference.clone());
                }
                log::info!("Cache expired for user {}, removing from cache", user_id);
                // LRU automatically moves the element to the front when accessed with get,
                // so we need to remove and re-add if it's expired
                cache.pop(&user_id);
            }
        }

        // Load from DB
        let quality = self.execute_with_timeout(move |conn| {
            match conn.query_row(
                "SELECT quality_preference FROM users WHERE telegram_id = ?1",
                params![user_id],
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
                user_id,
                UserInfo {
                    quality_preference: quality.clone(),
                    last_updated: tokio::time::Instant::now(),
                }
            );
        }
        
        Ok(quality)
    }

    /// Invalidate user quality cache
    pub async fn invalidate_user_quality_cache(&self, user_id: i64) {
        let mut cache = self.user_cache.lock().await;
        cache.pop(&user_id);
        log::info!("Invalidated cached quality preference for user {}", user_id);
    }

    /// Manual /language override for a user, if set.
    pub async fn get_user_lang(&self, user_id: i64) -> Result<Option<String>, anyhow::Error> {
        self.execute_with_timeout(move |conn| {
            let lang: Option<String> = conn.query_row(
                "SELECT lang FROM users WHERE telegram_id = ?1",
                params![user_id],
                |row| row.get(0)
            ).optional()?;
            Ok(lang)
        }).await.map_err(|e| anyhow::anyhow!("Failed to get language for user {}: {}", user_id, e))
    }

    /// Persist the manual /language override for a user.
    pub async fn set_user_lang(&self, user_id: i64, lang: &str) -> Result<(), anyhow::Error> {
        let lang_owned = lang.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute("INSERT OR IGNORE INTO users (telegram_id) VALUES (?1)", params![user_id])?;
            conn.execute("UPDATE users SET lang = ?1 WHERE telegram_id = ?2", params![lang_owned, user_id])?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to set language for user {}: {}", user_id, e))
    }

    /// Effective locale: the stored /language override wins, else the Telegram
    /// device tag, else English. Never fails.
    pub async fn get_effective_lang(&self, user_id: i64, tg_lang: Option<&str>) -> String {
        let stored = self.get_user_lang(user_id).await.ok().flatten();
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

    /// Create a pending download record and return its unique ID (ymid)
    pub async fn create_pending_download(&self, user_id: i64, video_url: &str) -> Result<String, anyhow::Error> {
        let id = uuid::Uuid::new_v4().to_string();
        let id_owned = id.clone();
        let video_url_owned = video_url.to_string();
        
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url) VALUES (?1, ?2, ?3)",
                params![id_owned, user_id, video_url_owned],
            )?;
            Ok(())
        }).await.map(|_| id).map_err(|e| anyhow::anyhow!("Failed to create pending download: {}", e))
    }

    /// Mark a pending download as verified (ad watched but not yet claimed)
    pub async fn mark_as_verified(&self, id: &str) -> Result<(), anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "UPDATE pending_downloads SET status = 'verified' WHERE id = ?1 AND status = 'pending'",
                params![id_owned],
            )?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to verify download {}: {}", id, e))
    }

    /// Mark as verified and return number of rows affected for better debugging
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

    /// Mark a pending row as terminally failed (download/upload error or job
    /// budget exceeded). Notified immediately so the sweeper skips it.
    pub async fn mark_pending_failed(&self, id: &str) -> Result<(), anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "UPDATE pending_downloads SET status = 'failed', notified_at = CURRENT_TIMESTAMP WHERE id = ?1",
                params![id_owned],
            )?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to mark {} as failed: {}", id, e))
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
    ) -> Result<(), anyhow::Error> {
        let ymid_owned = ymid.to_string();
        let event_owned = event_type.map(|s| s.to_string());
        let reward_owned = reward_event_type.to_string();
        let request_owned = request_var.map(|s| s.chars().take(64).collect::<String>());
        let sub_owned = sub_zone_id.map(|s| s.chars().take(64).collect::<String>());
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO monetag_postbacks (ymid, event_type, reward_event_type, estimated_price, request_var, sub_zone_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![ymid_owned, event_owned, reward_owned, estimated_price, request_owned, sub_owned],
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
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO mini_app_events (ymid, event, platform, sdk_host, user_agent) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![ymid_owned, event_owned, platform_owned, host_owned, ua_owned],
            )?;
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

    /// Whether any click postback was journaled for this ymid (any reward:
    /// a click proves the user engaged the creative, even an unpaid one).
    pub async fn has_click_for_ymid(&self, id: &str) -> Result<bool, anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM monetag_postbacks WHERE ymid = ?1 AND event_type = 'click'",
                params![id_owned],
                |row| row.get(0),
            )?;
            Ok(count > 0)
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to check clicks for {}: {}", id, e))
    }

    /// Unix time of the first valued postback for this ymid, if any.
    /// CAST is required: strftime('%s', ...) yields TEXT in SQLite and
    /// rusqlite cannot decode TEXT into i64.
    pub async fn first_valued_at(&self, id: &str) -> Result<Option<i64>, anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            let ts: Option<i64> = conn
                .query_row(
                    "SELECT MIN(CAST(strftime('%s', created_at) AS INTEGER)) FROM monetag_postbacks WHERE ymid = ?1 AND reward_event_type = 'valued'",
                    params![id_owned],
                    |row| row.get(0),
                )
                .optional()
                .flatten()
                .flatten();
            Ok(ts)
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read first valued for {}: {}", id, e))
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

    /// Strict delivery gate. A download unlocks iff ALL hold:
    /// 1. a valued impression exists (payment proof),
    /// 2. at least WATCH_SECS passed since the first valued (watch the ad),
    /// 3. a click exists OR CLICK_WAIT_SECS passed since the first valued
    ///    (click-less formats like popup can never send one).
    /// No timer backstop: without any valued event the download stays locked.
    /// Atomic per attempt via claim_verified_download.
    pub async fn claim_if_ready(&self, id: &str) -> Result<(i64, String, ClaimVia), anyhow::Error> {
        const WATCH_SECS: i64 = 15;
        const CLICK_WAIT_SECS: i64 = 90;
        if !self.has_valued_impression(id).await.unwrap_or(false) {
            return Err(anyhow::anyhow!("not valued yet for {}", id));
        }
        let first = self
            .first_valued_at(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no valued timestamp for {}", id))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let age = now.saturating_sub(first);
        if age < WATCH_SECS {
            return Err(anyhow::anyhow!("watch window not elapsed for {}", id));
        }
        if age < CLICK_WAIT_SECS && !self.has_click_for_ymid(id).await.unwrap_or(false) {
            return Err(anyhow::anyhow!("no click yet for {}", id));
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

    /// Claim a verified download and trigger completion
    pub async fn claim_verified_download(&self, id: &str) -> Result<(i64, String), anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            let (user_id, url): (i64, String) = conn.query_row(
                "SELECT user_id, video_url FROM pending_downloads WHERE id = ?1 AND status = 'verified'",
                params![id_owned],
                |row| Ok((row.get(0)?, row.get(1)?))
            )?;
            
            conn.execute(
                "UPDATE pending_downloads SET status = 'completed' WHERE id = ?1",
                params![id_owned],
            )?;
            
            Ok((user_id, url))
        }).await.map_err(|e| anyhow::anyhow!("Failed to claim verified download {}: {}", id, e))
    }

    /// Claim a download regardless of status (for admins or bypassing)
    pub async fn claim_any_download(&self, id: &str) -> Result<(i64, String), anyhow::Error> {
        let id_owned = id.to_string();
        self.execute_with_timeout(move |conn| {
            let (user_id, url): (i64, String) = conn.query_row(
                "SELECT user_id, video_url FROM pending_downloads WHERE id = ?1 AND (status = 'verified' OR status = 'pending')",
                params![id_owned],
                |row| Ok((row.get(0)?, row.get(1)?))
            )?;
            
            conn.execute(
                "UPDATE pending_downloads SET status = 'completed' WHERE id = ?1",
                params![id_owned],
            )?;
            
            Ok((user_id, url))
        }).await.map_err(|e| anyhow::anyhow!("Failed to bypass-claim download {}: {}", id, e))
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

    /// Record that a user blocked the bot (MyChatMember -> Banned).
    pub async fn record_block(&self, user_id: i64) -> Result<(), anyhow::Error> {
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO blocks (telegram_id) VALUES (?1)",
                params![user_id],
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
    pub async fn log_funnel_event(&self, user_id: i64, event: &str) {
        let event_owned = event.to_string();
        let result: Result<(), anyhow::Error> = self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO funnel_events (user_telegram_id, event) VALUES (?1, ?2)",
                params![user_id, event_owned],
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

    /// Expire abandoned download requests older than `older_than_secs` and
    /// return the ones that still need a user notification, marking them
    /// notified atomically so a second run never double-notifies.
    pub async fn expire_stale_pending(&self, older_than_secs: i64) -> Result<Vec<(String, i64)>, anyhow::Error> {
        self.execute_with_timeout(move |conn| {
            let cutoff = format!("datetime('now', '-{} seconds')", older_than_secs);
            let mut stmt = conn.prepare(&format!(
                "SELECT id, user_id FROM pending_downloads WHERE status IN ('pending', 'verified') AND notified_at IS NULL AND created_at < {cutoff}"
            ))?;
            let rows: Vec<(String, i64)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .filter_map(|r| r.ok()).collect();
            for (id, _) in &rows {
                conn.execute(
                    "UPDATE pending_downloads SET status = 'expired', notified_at = CURRENT_TIMESTAMP WHERE id = ?1",
                    params![id],
                )?;
            }
            Ok(rows)
        }).await.map_err(|e| anyhow::anyhow!("Failed to expire stale pending downloads: {}", e))
    }

    /// Check if user has active premium status
    pub async fn is_user_premium(&self, user_id: i64) -> bool {
        let result = self.execute_with_timeout(move |conn| {
            let is_premium: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM users WHERE telegram_id = ?1 AND premium_until > datetime('now'))",
                params![user_id],
                |row| row.get(0)
            )?;
            Ok(is_premium)
        }).await;

        result.unwrap_or(false)
    }

    /// Set or extend premium status for user
    pub async fn set_user_premium(&self, user_id: i64, days: i64) -> Result<(), anyhow::Error> {
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO users (telegram_id, premium_until) 
                 VALUES (?1, datetime('now', '+' || ?2 || ' days'))
                 ON CONFLICT(telegram_id) DO UPDATE SET 
                 premium_until = datetime(MAX(COALESCE(premium_until, datetime('now')), datetime('now')), '+' || ?2 || ' days')",
                params![user_id, days],
            )?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to set premium for user {}: {}", user_id, e))
    }

    /// Get list of users with active premium status
    pub async fn get_premium_users(&self) -> Result<Vec<(i64, String, String)>, anyhow::Error> {
        self.execute_with_timeout(|conn| {
            let mut stmt = conn.prepare(
                "SELECT telegram_id, premium_until, COALESCE(last_active, 'N/A')
                 FROM users 
                 WHERE premium_until > datetime('now')
                 ORDER BY premium_until DESC"
            )?;
            let users_iter = stmt.query_map([], |row| {
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
    pub async fn log_payment(&self, user_id: i64, amount: i64, payload: &str) -> Result<(), anyhow::Error> {
        let payload = payload.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO payments (user_id, amount, payload) VALUES (?1, ?2, ?3)",
                params![user_id, amount, payload],
            )?;
            Ok(())
        }).await.map_err(|e| anyhow::anyhow!("Failed to log payment: {}", e))
    }

    /// Log an invoice sent
    pub async fn log_invoice(&self, user_id: i64, amount: i64, payload: &str) -> Result<(), anyhow::Error> {
        let payload = payload.to_string();
        self.execute_with_timeout(move |conn| {
            conn.execute(
                "INSERT INTO invoices (user_id, amount, payload) VALUES (?1, ?2, ?3)",
                params![user_id, amount, payload],
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    async fn setup_test_db() -> (DatabasePool, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap().to_string();
        let pool = DatabasePool::new(db_path.clone(), 1);
        
        // Initialize all necessary tables
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
            conn.execute(
                "CREATE TABLE monetag_postbacks (id INTEGER PRIMARY KEY, ymid TEXT NOT NULL, event_type TEXT DEFAULT NULL, reward_event_type TEXT NOT NULL, estimated_price REAL DEFAULT NULL, request_var TEXT DEFAULT NULL, sub_zone_id TEXT DEFAULT NULL, created_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
                (),
            )?;
            conn.execute(
                "CREATE TABLE blocks (id INTEGER PRIMARY KEY, telegram_id BIGINT NOT NULL, blocked_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
                (),
            )?;
            conn.execute(
                "CREATE TABLE funnel_events (id INTEGER PRIMARY KEY, user_telegram_id BIGINT NOT NULL, event TEXT NOT NULL, created_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
                (),
            )?;
            conn.execute(
                "CREATE TABLE downloads (id INTEGER PRIMARY KEY, user_telegram_id BIGINT, video_url TEXT NOT NULL, download_date DATETIME DEFAULT CURRENT_TIMESTAMP)",
                (),
            )?;
            Ok(())
        }).await.unwrap();
        
        (pool, temp_file)
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
        assert_eq!(pool.get_effective_lang(user_id, Some("ru")).await, "ru");
        assert_eq!(pool.get_effective_lang(user_id, None).await, "en");

        // Persist override: it wins over the device tag.
        pool.set_user_lang(user_id, "uk").await.unwrap();
        assert_eq!(pool.get_user_lang(user_id).await.unwrap(), Some("uk".to_string()));
        assert_eq!(pool.get_effective_lang(user_id, Some("ru")).await, "uk");
    }

    #[tokio::test]
    async fn test_record_block_and_weekly_stats() {
        let (pool, _file) = setup_test_db().await;
        pool.record_block(111).await.unwrap();
        pool.record_block(222).await.unwrap();

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

        let first = pool.expire_stale_pending(1800).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, "stale");
        assert_eq!(first[0].1, 2);

        // Second run: nothing left to notify.
        let second = pool.expire_stale_pending(1800).await.unwrap();
        assert!(second.is_empty());

        // Stale row is expired now, fresh row still pending.
        let status = pool.get_pending_download_status("stale").await.unwrap();
        assert_eq!(status, Some("expired".to_string()));
        let status = pool.get_pending_download_status("fresh").await.unwrap();
        assert_eq!(status, Some("pending".to_string()));
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
        pool.log_funnel_event(1, "start").await;
        pool.log_funnel_event(2, "text_no_link").await;
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
        pool.log_funnel_event(1, "delivered_valued").await;
        pool.log_funnel_event(2, "delivered_timer").await;
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
        assert!(!pool.is_user_premium(user_id).await);

        // Activate premium
        pool.set_user_premium(user_id, 30).await.unwrap();

        // Now is premium
        assert!(pool.is_user_premium(user_id).await);

        // Check premium users list
        let premium_users = pool.get_premium_users().await.unwrap();
        assert_eq!(premium_users.len(), 1);
        assert_eq!(premium_users[0].0, user_id);
    }

    #[tokio::test]
    async fn test_premium_extension() {
        let (pool, _file) = setup_test_db().await;
        let user_id = 987654321i64;

        // Set initial premium
        pool.set_user_premium(user_id, 30).await.unwrap();
        let first_expiry = pool.get_premium_users().await.unwrap()[0].1.clone();

        // Extend premium
        pool.set_user_premium(user_id, 30).await.unwrap();
        let second_expiry = pool.get_premium_users().await.unwrap()[0].1.clone();

        // Second expiry should be later than first
        assert!(second_expiry > first_expiry);
    }

    #[tokio::test]
    async fn test_get_premium_users_filtering() {
        let (pool, _file) = setup_test_db().await;
        
        // Add active premium user
        pool.set_user_premium(1, 30).await.unwrap();
        
        // Add expired premium user
        pool.execute_with_timeout(|conn| {
            conn.execute(
                "INSERT INTO users (telegram_id, premium_until) VALUES (?1, datetime('now', '-1 day'))",
                params![2i64],
            )?;
            Ok(())
        }).await.unwrap();

        let premium_users = pool.get_premium_users().await.unwrap();
        assert_eq!(premium_users.len(), 1);
        assert_eq!(premium_users[0].0, 1);
    }

    /// Seed one pending row plus postback journals for claim_if_ready tests.
    /// `postbacks` holds (event_type, reward, age_secs): age 0 means now.
    async fn setup_gate_row(
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
        for (event, reward, age) in postbacks {
            let id_owned = id.to_string();
            let event_owned = event.to_string();
            let reward_owned = reward.to_string();
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

    #[tokio::test]
    async fn test_claim_if_ready_valued_click_ok() {
        let (pool, _file) = setup_test_db().await;
        // Valued impression 20s old (watch floor passed) plus an unpaid click.
        setup_gate_row(
            &pool,
            "gate-click",
            "verified",
            30,
            &[("impression", "valued", 20), ("click", "non_valued", 5)],
        )
        .await;
        let (user_id, url, via) = pool.claim_if_ready("gate-click").await.unwrap();
        assert_eq!(user_id, 7);
        assert_eq!(url, "http://v");
        assert_eq!(via, ClaimVia::Verified);
        assert_eq!(
            pool.get_pending_download_status("gate-click")
                .await
                .unwrap(),
            Some("completed".to_string())
        );
    }

    #[tokio::test]
    async fn test_claim_if_ready_valued_young_no_click_err() {
        let (pool, _file) = setup_test_db().await;
        // Valued impression just landed: the 15s watch floor still holds.
        setup_gate_row(
            &pool,
            "gate-young",
            "verified",
            5,
            &[("impression", "valued", 0)],
        )
        .await;
        assert!(pool.claim_if_ready("gate-young").await.is_err());
        assert_eq!(
            pool.get_pending_download_status("gate-young")
                .await
                .unwrap(),
            Some("verified".to_string())
        );
    }

    #[tokio::test]
    async fn test_claim_if_ready_valued_old_no_click_ok() {
        let (pool, _file) = setup_test_db().await;
        // Valued impression 95s old: the 90s click wait elapsed, so click-less
        // formats (popup) deliver without any click postback.
        setup_gate_row(
            &pool,
            "gate-old",
            "verified",
            100,
            &[("impression", "valued", 95)],
        )
        .await;
        let (_, _, via) = pool.claim_if_ready("gate-old").await.unwrap();
        assert_eq!(via, ClaimVia::Verified);
    }

    #[tokio::test]
    async fn test_claim_if_ready_no_valued_err() {
        let (pool, _file) = setup_test_db().await;
        // Only a non-valued impression: no payment proof, stays locked.
        setup_gate_row(
            &pool,
            "gate-free",
            "verified",
            100,
            &[("impression", "non_valued", 95)],
        )
        .await;
        assert!(pool.claim_if_ready("gate-free").await.is_err());
    }

    #[tokio::test]
    async fn test_claim_if_ready_backstop_gone() {
        let (pool, _file) = setup_test_db().await;
        // 20s-old pending row with zero postbacks: the deleted timer backstop
        // would have delivered this, the strict gate must not.
        setup_gate_row(&pool, "gate-stale", "pending", 20, &[]).await;
        assert!(pool.claim_if_ready("gate-stale").await.is_err());
        assert_eq!(
            pool.get_pending_download_status("gate-stale")
                .await
                .unwrap(),
            Some("pending".to_string())
        );
    }
}