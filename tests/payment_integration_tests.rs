use tiktokdownloader::database::DatabasePool;
use tiktokdownloader::handlers::payments::{
    process_successful_payment_logic, PREMIUM_PAYLOAD, CURRENCY_XTR,
    validate_pre_checkout,
};
use tempfile::NamedTempFile;

async fn setup_test_db() -> (DatabasePool, NamedTempFile) {
    let temp_file = NamedTempFile::new().unwrap();
    let db_path = temp_file.path().to_str().unwrap().to_string();
    let pool = DatabasePool::new(db_path.clone(), 1);
    
    pool.execute_with_timeout(|conn| {
        conn.execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, telegram_id BIGINT UNIQUE NOT NULL, last_active DATETIME DEFAULT CURRENT_TIMESTAMP, created_at DATETIME DEFAULT CURRENT_TIMESTAMP, quality_preference TEXT DEFAULT 'h264', premium_until DATETIME)",
            (),
        )?;
        conn.execute(
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            (),
        )?;
        conn.execute(
            "CREATE TABLE pending_downloads (id TEXT PRIMARY KEY, user_id BIGINT NOT NULL, video_url TEXT NOT NULL, status TEXT DEFAULT 'pending', created_at DATETIME DEFAULT CURRENT_TIMESTAMP)",
            (),
        )?;
        conn.execute(
            "CREATE TABLE downloads (id INTEGER PRIMARY KEY, user_telegram_id BIGINT, video_url TEXT NOT NULL, download_date DATETIME DEFAULT CURRENT_TIMESTAMP)",
            (),
        )?;
        conn.execute(
            "CREATE TABLE payments (id INTEGER PRIMARY KEY, user_id BIGINT NOT NULL, amount INTEGER NOT NULL, payload TEXT, timestamp DATETIME DEFAULT CURRENT_TIMESTAMP)",
            (),
        )?;
        conn.execute(
            "CREATE TABLE invoices (id INTEGER PRIMARY KEY, user_id BIGINT NOT NULL, amount INTEGER NOT NULL, payload TEXT, timestamp DATETIME DEFAULT CURRENT_TIMESTAMP)",
            (),
        )?;
        Ok(())
    }).await.unwrap();
    
    (pool, temp_file)
}

// =====================================================================
// INTEGRATION TESTS (Logic + Database)
// These tests verify the entire business flow without Telegram API
// =====================================================================

#[tokio::test]
async fn test_successful_payment_logic_activates_premium() {
    let (pool, _file) = setup_test_db().await;
    let user_id = 123456789i64;

    assert!(!pool.is_user_premium(user_id).await);

    let success = process_successful_payment_logic(
        user_id,
        PREMIUM_PAYLOAD,
        CURRENCY_XTR,
        50,
        &pool,
    ).await.unwrap();

    assert!(success);
    assert!(pool.is_user_premium(user_id).await);
}

#[tokio::test]
async fn test_duplicate_successful_payment_logic_behavior() {
    let (pool, _file) = setup_test_db().await;
    let user_id = 987654321i64;

    process_successful_payment_logic(user_id, PREMIUM_PAYLOAD, CURRENCY_XTR, 50, &pool).await.unwrap();
    let premium_users = pool.get_premium_users().await.unwrap();
    let first_expiry = premium_users[0].1.clone();

    process_successful_payment_logic(user_id, PREMIUM_PAYLOAD, CURRENCY_XTR, 50, &pool).await.unwrap();
    let premium_users = pool.get_premium_users().await.unwrap();
    let second_expiry = premium_users[0].1.clone();

    assert!(second_expiry > first_expiry, "Expiry date should increase (accumulate) on duplicate payment");
}

#[tokio::test]
async fn test_invalid_payload_rejection() {
    let (pool, _file) = setup_test_db().await;
    let user_id = 111222333i64;

    let success = process_successful_payment_logic(
        user_id,
        "malicious_payload",
        CURRENCY_XTR,
        50,
        &pool,
    ).await.unwrap();

    assert!(!success, "Logic should reject invalid payload");
    assert!(!pool.is_user_premium(user_id).await, "DB should not be updated on invalid payload");
}

#[tokio::test]
async fn test_invalid_currency_rejection() {
    let (pool, _file) = setup_test_db().await;
    let user_id = 444555666i64;

    let success = process_successful_payment_logic(
        user_id,
        PREMIUM_PAYLOAD,
        "USD",
        50,
        &pool,
    ).await.unwrap();

    assert!(!success, "Logic should reject non-XTR currency");
}

#[tokio::test]
async fn test_validate_pre_checkout_logic() {
    // Happy path
    assert!(validate_pre_checkout(PREMIUM_PAYLOAD, CURRENCY_XTR));
    
    // Reject paths
    assert!(!validate_pre_checkout("wrong", CURRENCY_XTR));
    assert!(!validate_pre_checkout(PREMIUM_PAYLOAD, "USD"));
}

// NOTE: Handler-level routing tests (dptree::dispatch) for SuccessfulPayment and PreCheckoutQuery 
// are currently omitted because teloxide_tests (v0.2) does not yet provide 
// MockMessageSuccessfulPayment or MockPreCheckoutQuery.
//
#[tokio::test]
async fn test_rich_daily_stats_logic() {
    let (pool, _file) = setup_test_db().await;
    
    // 1. Setup "Yesterday" data
    pool.execute_with_timeout(|conn| {
        // Yesterday's users
        conn.execute("INSERT INTO users (telegram_id, last_active, created_at) VALUES (101, datetime('now', '-1 day'), datetime('now', '-1 day'))", ())?;
        conn.execute("INSERT INTO users (telegram_id, last_active, created_at) VALUES (102, datetime('now', '-1 day'), datetime('now', '-1 day'))", ())?;
        Ok(())
    }).await.unwrap();

    // 2. Setup "Today" data
    let user_today_new = 201i64;
    let user_today_returning = 101i64; // One from yesterday returns

    pool.execute_with_timeout(move |conn| {
        // Returning user
        conn.execute("UPDATE users SET last_active = datetime('now') WHERE telegram_id = ?1", [user_today_returning])?;
        // New user today
        conn.execute("INSERT INTO users (telegram_id, last_active, created_at) VALUES (?1, datetime('now'), datetime('now'))", [user_today_new])?;
        
        // Downloads today
        conn.execute("INSERT INTO downloads (user_telegram_id, video_url, download_date) VALUES (?1, 'url1', datetime('now'))", [user_today_new])?;
        conn.execute("INSERT INTO downloads (user_telegram_id, video_url, download_date) VALUES (?1, 'url2', datetime('now'))", [user_today_new])?;
        conn.execute("INSERT INTO downloads (user_telegram_id, video_url, download_date) VALUES (?1, 'url3', datetime('now'))", [user_today_returning])?;
        
        // Ad impressions today
        conn.execute("INSERT INTO pending_downloads (id, user_id, video_url, created_at) VALUES ('id1', ?1, 'url1', datetime('now'))", [user_today_new])?;
        
        // Payments today
        conn.execute("INSERT INTO payments (user_id, amount, payload, timestamp) VALUES (?1, 50, 'payload', datetime('now'))", [user_today_new])?;
        
        Ok(())
    }).await.unwrap();

    // 3. Get stats
    let stats = pool.get_rich_daily_stats().await.unwrap();

    // 4. Verify
    assert_eq!(stats.unique_users, 2); // 101 and 201
    assert_eq!(stats.unique_users_delta, 1); // 2 today - 1 yesterday (102) = 1
    assert_eq!(stats.new_users, 1); // Only 201
    assert_eq!(stats.returning_users, 1); // Only 101
    assert_eq!(stats.unique_downloaders, 2); // Both downloaded
    assert_eq!(stats.total_downloads, 3);
    assert_eq!(stats.ad_impressions, 1);
    assert_eq!(stats.payments_count, 1);
    assert_eq!(stats.revenue_xtr, 50);
}
