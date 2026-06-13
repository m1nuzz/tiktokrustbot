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
            "CREATE TABLE users (id INTEGER PRIMARY KEY, telegram_id BIGINT UNIQUE NOT NULL, last_active DATETIME DEFAULT CURRENT_TIMESTAMP, quality_preference TEXT DEFAULT 'h264', premium_until DATETIME)",
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
// Routing is instead verified through the [PAYMENT_CHAIN] logs in manual E2E tests 
// (see docs/payment-stars-test-checklist.md).
