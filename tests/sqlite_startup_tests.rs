#![cfg(target_os = "linux")]
//! Boot-order regression for the grammers 0.12 upgrade.
//!
//! The production crash on 2026-11-10 was a runtime conflict between two
//! SQLite bindings in one process: `init_database()` (rusqlite) initialized
//! sqlite3 first, then `SqliteSession::open()` (libsql) tried to call
//! `sqlite3_config(SQLITE_CONFIG_SERIALIZED)` and panicked with SQLITE_MISUSE.
//! `cargo test --all-targets` never runs `main()`, so no existing test ever
//! exercised both bindings in the same process.
//!
//! This file is intentionally the only test binary that touches both. Keep it
//! standalone: the ordering only holds in a process where SQLite is still
//! uninitialized when `prime_session_storage()` runs.

use grammers_session::storages::SqliteSession;
use tiktokdownloader::database::init_database;
use tiktokdownloader::mtproto_uploader::prime_session_storage;

#[tokio::test]
async fn app_database_then_mtproto_session_in_one_process() {
    // Mirrors the first statement of main() followed by the first
    // MTProtoUploader construction. Without this call the test reproduces the
    // production panic.
    prime_session_storage()
        .await
        .expect("prime grammers session storage");

    let dir = tempfile::tempdir().expect("temp dir");

    unsafe {
        std::env::set_var("DATABASE_PATH", dir.path().join("tiktok_downloader.db"));
    }
    init_database().expect("app database initialization");

    let session_path = dir.path().join("telegram-test.session");
    let session = SqliteSession::open(&session_path)
        .await
        .expect("grammers 0.12 SQLite session must open after init_database()");

    use grammers_session::Session;
    assert!(session.home_dc_id().is_ok());
    assert!(session_path.exists(), "session file should be created");
}
