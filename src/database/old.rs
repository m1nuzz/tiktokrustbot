use rusqlite::{Connection, Result};
use std::env;

pub fn get_database_path() -> String {
    // First, check for the DATABASE_PATH environment variable.
    if let Ok(db_path) = env::var("DATABASE_PATH") {
        return db_path;
    }

    // If the environment variable is not set, default to a path next to the executable.
    let mut path = env::current_exe().expect("Failed to get current exe path");
    path.pop(); // Remove the executable name, leaving the directory.
    path.push("tiktok_downloader.db"); // Add the db file name.
    path.to_str().expect("Failed to construct database path").to_string()
}

#[cfg(test)]
fn update_user_activity(user_id: i64) -> Result<()> {
    let db_path = get_database_path();
    let conn = Connection::open(db_path)?;
    conn.execute("INSERT OR IGNORE INTO users (telegram_id) VALUES (?1)", [user_id])?;
    conn.execute("UPDATE users SET last_active = CURRENT_TIMESTAMP WHERE telegram_id = ?1", [user_id])?;
    Ok(())
}

#[cfg(test)]
fn log_download(telegram_id: i64, video_url: &str) -> Result<()> {
    let db_path = get_database_path();
    let conn = Connection::open(db_path)?;
    // Update user activity first (to ensure the user exists in the database)
    update_user_activity(telegram_id)?;
    conn.execute("INSERT INTO downloads (user_telegram_id, video_url) VALUES (?1, ?2)", (telegram_id, video_url))?;
    Ok(())
}

/// True when `table.column` already exists. Makes migrations idempotent.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for col in cols {
        if col? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// ADD COLUMN only when missing. Logs loudly instead of swallowing the error,
/// which is what hid the missing `users.created_at` column in production.
fn ensure_column(conn: &Connection, table: &str, column: &str, ddl: &str) -> Result<()> {
    if column_exists(conn, table, column)? {
        return Ok(());
    }
    log::info!("Migrating {table}.{column} ...");
    conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {ddl}"), [])?;
    log::info!("Migrated {table}.{column}");
    Ok(())
}

pub fn init_database() -> Result<()> {
    let db_path = get_database_path();
    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, telegram_id BIGINT UNIQUE NOT NULL, last_active DATETIME DEFAULT CURRENT_TIMESTAMP, created_at DATETIME DEFAULT CURRENT_TIMESTAMP, quality_preference TEXT DEFAULT 'h264', premium_until DATETIME, lang TEXT DEFAULT NULL)",
        (),
    )?;
    // Add columns if they don't exist (idempotent, errors propagate loudly).
    // NOTE: created_at uses DEFAULT NULL (not CURRENT_TIMESTAMP) because
    // SQLite refuses ADD COLUMN with a volatile default on non-empty tables
    // ("Cannot add a column with non-constant default"); the backfill below
    // fills real values. Fresh DBs still get CURRENT_TIMESTAMP via CREATE.
    ensure_column(&conn, "users", "last_active", "last_active DATETIME DEFAULT CURRENT_TIMESTAMP")?;
    ensure_column(&conn, "users", "created_at", "created_at DATETIME DEFAULT NULL")?;
    ensure_column(&conn, "users", "quality_preference", "quality_preference TEXT DEFAULT 'h264'")?;
    ensure_column(&conn, "users", "premium_until", "premium_until DATETIME")?;
    ensure_column(&conn, "users", "lang", "lang TEXT DEFAULT NULL")?;
    ensure_column(&conn, "users", "ref_code", "ref_code TEXT DEFAULT NULL")?;

    // Backfill created_at for rows that predate the column (NULL after the
    // ADD COLUMN above) or carry a naive ALTER timestamp (always *after*
    // last_active). Genuine registrations always satisfy
    // created_at <= last_active, so this only touches broken rows and is
    // safe to run on every startup.
    let backfilled = conn.execute(
        "UPDATE users SET created_at = last_active WHERE created_at IS NULL OR created_at > last_active",
        [],
    )?;
    if backfilled > 0 {
        log::warn!("Backfilled created_at for {backfilled} users");
    }

    // Create the table with the new format
    conn.execute(
        "CREATE TABLE IF NOT EXISTS downloads (id INTEGER PRIMARY KEY, user_telegram_id BIGINT, video_url TEXT NOT NULL, download_date DATETIME DEFAULT CURRENT_TIMESTAMP)",
        (),
    )?;
    
    // Check if the old format table exists
    let has_old_format: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='downloads' AND sql LIKE '%user_id INTEGER%'",
        (),
        |row| row.get(0)
    ).unwrap_or(0) > 0;
    
    if has_old_format {
        // Check if we need to migrate (if there's data in the old format)
        let has_data: bool = conn.query_row(
            "SELECT COUNT(*) FROM downloads",
            (),
            |row| row.get(0)
        ).unwrap_or(0) > 0;
        
        if has_data {
            // Create a temporary table with the new structure
            conn.execute(
                "CREATE TEMPORARY TABLE downloads_migrated AS SELECT d.id, u.telegram_id as user_telegram_id, d.video_url, d.download_date FROM downloads d JOIN users u ON d.user_id = u.id",
                (),
            )?;
            
            // Drop the old table
            conn.execute("DROP TABLE downloads", ())?;
            
            // Recreate with new format
            conn.execute(
                "CREATE TABLE downloads (id INTEGER PRIMARY KEY, user_telegram_id BIGINT, video_url TEXT NOT NULL, download_date DATETIME DEFAULT CURRENT_TIMESTAMP)",
                (),
            )?;
            
            // Copy data from temporary table
            conn.execute(
                "INSERT INTO downloads (id, user_telegram_id, video_url, download_date) SELECT id, user_telegram_id, video_url, download_date FROM downloads_migrated",
                (),
            )?;
        } else {
            // If no data in old format, just drop and recreate
            conn.execute("DROP TABLE downloads", ())?;
            conn.execute(
                "CREATE TABLE downloads (id INTEGER PRIMARY KEY, user_telegram_id BIGINT, video_url TEXT NOT NULL, download_date DATETIME DEFAULT CURRENT_TIMESTAMP)",
                (),
            )?;
        }
    }
    conn.execute(
        "CREATE TABLE IF NOT EXISTS admins (id INTEGER PRIMARY KEY, admin_telegram_id BIGINT UNIQUE NOT NULL)",
        (),
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS channels (id INTEGER PRIMARY KEY, channel_id TEXT UNIQUE NOT NULL, channel_name TEXT)",
        (),
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        (),
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS pending_downloads (id TEXT PRIMARY KEY, user_id BIGINT NOT NULL, video_url TEXT NOT NULL, status TEXT DEFAULT 'pending', created_at DATETIME DEFAULT CURRENT_TIMESTAMP, notified_at DATETIME DEFAULT NULL, lease_expires_at DATETIME DEFAULT NULL, job_started_at DATETIME DEFAULT NULL, entry_ymid TEXT DEFAULT NULL, ad_requested_at DATETIME DEFAULT NULL)",
        (),
    )?;
    // Column ensure + retirement run after the CREATE above so fresh
    // databases (where the table does not exist yet) don't error.
    ensure_column(&conn, "pending_downloads", "notified_at", "notified_at DATETIME DEFAULT NULL")?;
    // Heartbeat-extended lease: the deadline a client heartbeat pushes
    // forward while the mini-app is open, and the record that a download job
    // really started. Both are what let the sweeper expire a lapsed session
    // instead of a session the user is still sitting on, and what lets it tell
    // "never earned the ad" from "the download failed".
    ensure_column(&conn, "pending_downloads", "lease_expires_at", "lease_expires_at DATETIME DEFAULT NULL")?;
    ensure_column(&conn, "pending_downloads", "job_started_at", "job_started_at DATETIME DEFAULT NULL")?;
    // One ymid per ad event. `ad_requested_at` is stamped the moment the client
    // asks for an ad and is what marks a ymid as spent: the impression postback
    // lags the ad by seconds, so an impression-based test would let a second
    // press reuse a ymid that already carried an ad. `entry_ymid` records which
    // bot-button ymid a freshly minted session came from, so a press can be
    // joined to the session that served it.
    ensure_column(&conn, "pending_downloads", "entry_ymid", "entry_ymid TEXT DEFAULT NULL")?;
    ensure_column(&conn, "pending_downloads", "ad_requested_at", "ad_requested_at DATETIME DEFAULT NULL")?;
    // Silently retire download requests abandoned before this startup (the
    // user never finished watching the ad). They are marked notified so the
    // expiry sweeper never messages them; only new rows get notified. The
    // predicate is the LEASE, not the row age, so a session whose webapp is
    // still open (its heartbeat keeps pushing the lease) is never retired
    // behind the user's back. Runs after the CREATE above so fresh databases
    // don't error.
    let retired = conn.execute(
        &format!(
            "UPDATE pending_downloads SET status = 'expired', notified_at = CURRENT_TIMESTAMP WHERE status IN ('pending', 'verified') AND notified_at IS NULL AND COALESCE(lease_expires_at, datetime(created_at, '+{} seconds')) < datetime('now')",
            super::pool::SESSION_LEASE_SECS
        ),
        [],
    )?;
    if retired > 0 {
        log::warn!("Retired {retired} stale pending downloads without notification");
    }
    conn.execute(
        "CREATE TABLE IF NOT EXISTS blocks (id INTEGER PRIMARY KEY, telegram_id BIGINT NOT NULL, blocked_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
        (),
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS funnel_events (id INTEGER PRIMARY KEY, user_telegram_id BIGINT NOT NULL, event TEXT NOT NULL, created_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
        (),
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS monetag_postbacks (id INTEGER PRIMARY KEY, ymid TEXT NOT NULL, event_type TEXT DEFAULT NULL, reward_event_type TEXT NOT NULL, estimated_price REAL DEFAULT NULL, created_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
        (),
    )?;
    // Placement attribution for postback revenue (which SDK call earned it).
    // Runs after the CREATE above so fresh databases don't error.
    ensure_column(&conn, "monetag_postbacks", "request_var", "request_var TEXT DEFAULT NULL")?;
    ensure_column(&conn, "monetag_postbacks", "sub_zone_id", "sub_zone_id TEXT DEFAULT NULL")?;
    // Client-side ad-funnel telemetry. The mini-app cannot tell "no fill" from
    // "ad blocked on this device", so every stage transition is reported here
    // and correlated with monetag_postbacks by ymid.
    conn.execute(
        "CREATE TABLE IF NOT EXISTS mini_app_events (id INTEGER PRIMARY KEY, ymid TEXT NOT NULL, event TEXT NOT NULL, platform TEXT DEFAULT NULL, sdk_host TEXT DEFAULT NULL, user_agent TEXT DEFAULT NULL, created_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
        (),
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS payments (id INTEGER PRIMARY KEY, user_id BIGINT NOT NULL, amount INTEGER NOT NULL, payload TEXT, timestamp DATETIME DEFAULT CURRENT_TIMESTAMP)",
        (),
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS invoices (id INTEGER PRIMARY KEY, user_id BIGINT NOT NULL, amount INTEGER NOT NULL, payload TEXT, timestamp DATETIME DEFAULT CURRENT_TIMESTAMP)",
        (),
    )?;
    
    // Add indexes for performance
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_users_last_active ON users(last_active)", ());
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_downloads_date ON downloads(download_date)", ());
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_pending_date ON pending_downloads(created_at)", ());
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_pending_status ON pending_downloads(status)", ());
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_pending_user_id ON pending_downloads(user_id)", ());
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_payments_date ON payments(timestamp)", ());
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_invoices_date ON invoices(timestamp)", ());
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_blocks_date ON blocks(blocked_at)", ());
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_funnel_event ON funnel_events(event, created_at)", ());
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_funnel_user ON funnel_events(user_telegram_id)", ());
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_postback_ymid ON monetag_postbacks(ymid, created_at)", ());
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_mini_app_event ON mini_app_events(event, created_at)", ());
    let _ = conn.execute("CREATE INDEX IF NOT EXISTS idx_mini_app_ymid ON mini_app_events(ymid, created_at)", ());

    conn.execute(
        "INSERT OR IGNORE INTO settings (key, value) VALUES ('subscription_required', 'true')",
        (),
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO settings (key, value) VALUES ('ads_enabled', 'true')",
        (),
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO settings (key, value) VALUES ('admin_ads_enabled', 'false')",
        (),
    )?;
    Ok(())
}



#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::env;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_database_initialization() {
        // Create a temporary database for testing
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        
        unsafe {
            env::set_var("DATABASE_PATH", db_path.to_str().unwrap());
        }
        
        // Initialize the database
        let result = init_database();
        assert!(result.is_ok());
        
        // Verify that the tables were created
        let conn = Connection::open(&db_path).unwrap();
        let table_count: i32 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table'", 
            [], 
            |row| row.get(0)
        ).unwrap();
        
        // There should be at least 5 tables: users, downloads, admins, channels, settings
        assert!(table_count >= 5);
        unsafe {
            env::remove_var("DATABASE_PATH");
        }
    }
    
    #[test]
    #[serial]
    fn test_user_activity_update() {
        // Create a temporary database for testing
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        unsafe {
            env::set_var("DATABASE_PATH", db_path.to_str().unwrap());
        }
        
        // Initialize the database
        init_database().unwrap();
        
        // Test updating user activity
        let user_id = 123456;
        let result = update_user_activity(user_id);
        assert!(result.is_ok());
        
        // Verify the user exists in the database - use the same environment variable
        let db_path_from_env = env::var("DATABASE_PATH").unwrap();
        let conn = Connection::open(&db_path_from_env).unwrap();
        let count: i32 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE telegram_id = ?1",
            [user_id],
            |row| row.get(0)
        ).unwrap();
        
        assert_eq!(count, 1);
        unsafe {
            env::remove_var("DATABASE_PATH");
        }
    }
    
    #[test]
    #[serial]
    fn test_download_logging() {
        // Create a temporary database for testing
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        unsafe {
            env::set_var("DATABASE_PATH", db_path.to_str().unwrap());
        }
        
        // Initialize the database
        init_database().unwrap();
        
        // Test logging a download
        let user_id = 123456;
        let video_url = "https://example.com/video.mp4";
        let result = log_download(user_id, video_url);
        assert!(result.is_ok());
        
        // Verify the download was logged - use the same environment variable
        let db_path_from_env = env::var("DATABASE_PATH").unwrap();
        let conn = Connection::open(&db_path_from_env).unwrap();
        let count: i32 = conn.query_row(
            "SELECT COUNT(*) FROM downloads WHERE user_telegram_id = ?1 AND video_url = ?2",
            (user_id, video_url),
            |row| row.get(0)
        ).unwrap();
        
        assert_eq!(count, 1);
        unsafe {
            env::remove_var("DATABASE_PATH");
        }
    }

    fn user_columns(conn: &Connection) -> Vec<String> {
        let mut stmt = conn.prepare("PRAGMA table_info(users)").unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    #[test]
    #[serial]
    fn test_migration_adds_missing_columns() {        // Simulate a legacy DB that predates created_at/lang/ref_code.
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        unsafe {
            env::set_var("DATABASE_PATH", db_path.to_str().unwrap());
        }

        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, telegram_id BIGINT UNIQUE NOT NULL, last_active DATETIME DEFAULT CURRENT_TIMESTAMP, quality_preference TEXT DEFAULT 'h264', premium_until DATETIME)",
                (),
            ).unwrap();
        }

        init_database().unwrap();

        let conn = Connection::open(&db_path).unwrap();
        let cols = user_columns(&conn);
        for expected in ["created_at", "lang", "ref_code", "last_active", "quality_preference", "premium_until"] {
            assert!(cols.contains(&expected.to_string()), "missing column {expected}");
        }
        // blocks table must exist too.
        let blocks: i32 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='blocks'",
            [],
            |row| row.get(0)
        ).unwrap();
        assert_eq!(blocks, 1);
        unsafe {
            env::remove_var("DATABASE_PATH");
        }
    }

    #[test]
    #[serial]
    fn test_created_at_migration_on_nonempty_legacy_table() {
        // Exact production scenario: a NON-EMPTY users table without
        // created_at. SQLite rejects ADD COLUMN with a volatile default
        // here, so the migration must use DEFAULT NULL + backfill and,
        // crucially, must not fail (a failure would abort bot startup).
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        unsafe {
            env::set_var("DATABASE_PATH", db_path.to_str().unwrap());
        }

        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, telegram_id BIGINT UNIQUE NOT NULL, last_active DATETIME DEFAULT CURRENT_TIMESTAMP, quality_preference TEXT DEFAULT 'h264', premium_until DATETIME)",
                (),
            ).unwrap();
            conn.execute(
                "INSERT INTO users (telegram_id, last_active) VALUES (1, datetime('now', '-5 days')), (2, datetime('now', '-1 hour'))",
                (),
            ).unwrap();
        }

        // Must succeed on a non-empty table.
        init_database().unwrap();

        let conn = Connection::open(&db_path).unwrap();
        let filled: i32 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE created_at IS NOT NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(filled, 2);
        // Backfilled from last_active, not from "now".
        let old: String = conn.query_row(
            "SELECT created_at FROM users WHERE telegram_id = 1", [], |r| r.get(0)).unwrap();
        let active: String = conn.query_row(
            "SELECT last_active FROM users WHERE telegram_id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(old, active);
        unsafe {
            env::remove_var("DATABASE_PATH");
        }
    }

    #[test]
    #[serial]
    fn test_created_at_backfill_only_touches_broken_rows() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        unsafe {
            env::set_var("DATABASE_PATH", db_path.to_str().unwrap());
        }

        init_database().unwrap();

        {
            let conn = Connection::open(&db_path).unwrap();
            // Healthy row: registered yesterday, active today.
            conn.execute(
                "INSERT INTO users (telegram_id, last_active, created_at) VALUES (1, datetime('now'), datetime('now', '-1 day'))",
                (),
            ).unwrap();
            // Broken row (naive ADD COLUMN fill): created_at after last_active.
            conn.execute(
                "INSERT INTO users (telegram_id, last_active, created_at) VALUES (2, datetime('now', '-10 days'), datetime('now'))",
                (),
            ).unwrap();
        }

        // Snapshot the healthy row before the second migration run.
        let healthy_before: String = Connection::open(&db_path).unwrap().query_row(
            "SELECT created_at FROM users WHERE telegram_id = 1", [], |r| r.get(0)).unwrap();

        // Re-running init must fix only the broken row (idempotent).
        init_database().unwrap();

        let conn = Connection::open(&db_path).unwrap();
        let healthy: String = conn.query_row(
            "SELECT created_at FROM users WHERE telegram_id = 1", [], |r| r.get(0)).unwrap();
        let fixed: String = conn.query_row(
            "SELECT created_at FROM users WHERE telegram_id = 2", [], |r| r.get(0)).unwrap();
        let fixed_active: String = conn.query_row(
            "SELECT last_active FROM users WHERE telegram_id = 2", [], |r| r.get(0)).unwrap();
        // Broken row repaired to its last_active ...
        assert_eq!(fixed, fixed_active);
        // ... while the healthy row is untouched.
        assert_eq!(healthy, healthy_before);
        unsafe {
            env::remove_var("DATABASE_PATH");
        }
    }

    #[test]
    #[serial]
    fn test_stale_pending_retired_silently() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        unsafe {
            env::set_var("DATABASE_PATH", db_path.to_str().unwrap());
        }

        init_database().unwrap();

        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url) VALUES ('fresh', 1, 'http://x')",
                (),
            ).unwrap();
            conn.execute(
                "INSERT INTO pending_downloads (id, user_id, video_url, created_at) VALUES ('stale', 2, 'http://y', datetime('now', '-31 minutes'))",
                (),
            ).unwrap();
        }

        init_database().unwrap();

        let conn = Connection::open(&db_path).unwrap();
        let stale_status: String = conn.query_row(
            "SELECT status FROM pending_downloads WHERE id = 'stale'", [], |r| r.get(0)).unwrap();
        let stale_notified: Option<String> = conn.query_row(
            "SELECT notified_at FROM pending_downloads WHERE id = 'stale'", [], |r| r.get(0)).unwrap();
        let fresh_status: String = conn.query_row(
            "SELECT status FROM pending_downloads WHERE id = 'fresh'", [], |r| r.get(0)).unwrap();
        assert_eq!(stale_status, "expired");
        assert!(stale_notified.is_some());
        assert_eq!(fresh_status, "pending");
        unsafe {
            env::remove_var("DATABASE_PATH");
        }
    }
}