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
}