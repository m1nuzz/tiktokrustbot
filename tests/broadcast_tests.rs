use std::sync::atomic::AtomicU64;
use tempfile::NamedTempFile;
use tiktokdownloader::database::DatabasePool;
use tiktokdownloader::handlers::broadcast::{
    broadcast_pace_interval_ms, broadcast_pause_remaining, broadcast_pause_set,
    BROADCAST_CANCEL, BROADCAST_CONFIRM, BROADCAST_MAX_CONCURRENT,
    BROADCAST_SCOPE_ALL, BROADCAST_SCOPE_THIS, BROADCAST_TARGET_PER_SEC,
};

async fn setup_broadcast_db() -> (DatabasePool, NamedTempFile) {
    let temp_file = NamedTempFile::new().unwrap();
    let db_path = temp_file.path().to_str().unwrap().to_string();
    let pool = DatabasePool::new(db_path.clone(), 1);

    pool.execute_with_timeout(|conn| {
        conn.execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, telegram_id BIGINT NOT NULL, bot_id TEXT NOT NULL DEFAULT 'primary', last_active DATETIME DEFAULT CURRENT_TIMESTAMP, UNIQUE(bot_id, telegram_id))",
            (),
        )?;
        // One human on two bots (recent on A, stale on B), one human only on
        // B, one human on A with NULL activity.
        conn.execute(
            "INSERT INTO users (telegram_id, bot_id, last_active) VALUES \
             (111, 'botA', '2026-10-01 10:00:00'), \
             (111, 'botB', '2026-09-01 10:00:00'), \
             (222, 'botB', '2026-10-02 10:00:00'), \
             (333, 'botA', NULL)",
            (),
        )?;
        Ok(())
    })
    .await
    .unwrap();

    (pool, temp_file)
}

#[tokio::test]
async fn test_recipients_this_bot_scoped_to_own_bot() {
    let (pool, _file) = setup_broadcast_db().await;

    let mut got = pool.get_broadcast_recipients_this_bot("botA").await.unwrap();
    got.sort();
    assert_eq!(
        got,
        vec![
            (111, "botA".to_string()),
            (333, "botA".to_string()),
        ]
    );

    let got_b = pool.get_broadcast_recipients_this_bot("botB").await.unwrap();
    assert_eq!(got_b.len(), 2);
    assert!(got_b.contains(&(111, "botB".to_string())));
    assert!(got_b.contains(&(222, "botB".to_string())));
}

#[tokio::test]
async fn test_recipients_all_dedupes_to_most_recent_bot() {
    let (pool, _file) = setup_broadcast_db().await;

    let mut got = pool.get_broadcast_recipients_all().await.unwrap();
    got.sort();
    // User 111 appears once, from botA (most recent activity). User 222 only
    // on botB. User 333 on botA despite NULL activity.
    assert_eq!(
        got,
        vec![
            (111, "botA".to_string()),
            (222, "botB".to_string()),
            (333, "botA".to_string()),
        ]
    );
}

#[test]
fn test_pace_math_pins_send_rate() {
    // 22/s -> a task start every 45ms; never divide by zero.
    assert_eq!(broadcast_pace_interval_ms(BROADCAST_TARGET_PER_SEC), 45);
    assert_eq!(broadcast_pace_interval_ms(30), 33);
    assert_eq!(broadcast_pace_interval_ms(0), 1000);
    assert!(BROADCAST_MAX_CONCURRENT >= 1);
}

#[test]
fn test_pause_flag_moves_forward_only() {
    let flag = AtomicU64::new(0);
    assert_eq!(broadcast_pause_remaining(&flag), 0);

    broadcast_pause_set(&flag, 60);
    let first = broadcast_pause_remaining(&flag);
    assert!(first >= 59 && first <= 61);

    // A shorter wait must not pull the deadline backward.
    broadcast_pause_set(&flag, 1);
    assert!(broadcast_pause_remaining(&flag) >= first - 1);
}

#[test]
fn test_callback_data_fits_telegram_limit() {
    for data in [
        BROADCAST_SCOPE_THIS,
        BROADCAST_SCOPE_ALL,
        BROADCAST_CONFIRM,
        BROADCAST_CANCEL,
    ] {
        assert!(data.len() < 64, "callback data too long: {}", data);
    }
    assert_ne!(BROADCAST_SCOPE_THIS, BROADCAST_SCOPE_ALL);
}
