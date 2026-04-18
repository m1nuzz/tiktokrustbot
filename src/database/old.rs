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

pub fn init_database() -> Result<()> {
    let db_path = get_database_path();
    let conn = Connection::open(db_path)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, telegram_id BIGINT UNIQUE NOT NULL, last_active DATETIME DEFAULT CURRENT_TIMESTAMP, quality_preference TEXT DEFAULT 'h264')",
        (),
    )?;
    // Add the quality_preference column to the users table if it doesn't exist, ignoring the error if it does.
    let _ = conn.execute("ALTER TABLE users ADD COLUMN quality_preference TEXT DEFAULT 'h264'", ());

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
        "INSERT OR IGNORE INTO settings (key, value) VALUES ('subscription_required', 'true')",
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
}