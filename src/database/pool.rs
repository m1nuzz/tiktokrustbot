use rusqlite::{Connection, Result as SqliteResult, params};
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    async fn setup_test_db() -> (DatabasePool, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap().to_string();
        let pool = DatabasePool::new(db_path, 1);
        
        // Initialize settings table
        pool.execute_with_timeout(|conn| {
            conn.execute(
                "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
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
    async fn test_get_nonexistent_setting() {
        let (pool, _file) = setup_test_db().await;
        let result = pool.get_setting("ghost").await;
        assert!(result.is_err());
    }
}