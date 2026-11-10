//! Boot regression for the grammers 0.12 upgrade.
//!
//! The production crash on 2026-11-10 was caused by grammers' default
//! `sqlite-storage` session backend pulling `libsql` into a binary that also
//! links rusqlite's bundled SQLite: duplicate `sqlite3_*` symbols at link
//! time, and a `SQLITE_MISUSE` panic when libsql called `sqlite3_config` after
//! rusqlite had initialized SQLite. `cargo test --all-targets` never runs
//! `main()`, so no existing test exercised the two bindings together.
//!
//! The fix removes libsql from the dependency graph entirely and stores MTProto
//! session state in the application database. These tests prove that the
//! application database and the session storage coexist in one process and
//! round-trip across a reopen.

use grammers_session::types::{
    ChannelState, DcOption, UpdateState, UpdatesState,
};
use grammers_session::Session;
use serial_test::serial;
use tiktokdownloader::database::init_database;
use tiktokdownloader::mtproto_uploader::session::DbSession;

fn test_dc_option() -> DcOption {
    DcOption {
        id: 2,
        ipv4: "149.154.167.40:443".parse().unwrap(),
        ipv6: "[2001:67c:4e8:f004::9]:443".parse().unwrap(),
        auth_key: Some([7u8; 256]),
    }
}

#[tokio::test]
#[serial]
async fn app_database_and_session_storage_coexist_in_one_process() {
    let dir = tempfile::tempdir().expect("temp dir");

    unsafe {
        std::env::set_var("DATABASE_PATH", dir.path().join("tiktok_downloader.db"));
    }
    init_database().expect("app database initialization");

    let session = DbSession::open("telegram-test").expect("open session storage");
    assert_eq!(session.home_dc_id().expect("home dc"), 2);
}

#[tokio::test]
#[serial]
async fn session_state_survives_reopen() {
    let dir = tempfile::tempdir().expect("temp dir");

    unsafe {
        std::env::set_var("DATABASE_PATH", dir.path().join("tiktok_downloader.db"));
    }
    init_database().expect("app database initialization");

    let state = UpdatesState {
        pts: 11,
        qts: 22,
        date: 33,
        seq: 44,
        channels: vec![ChannelState {
            id: -100123,
            pts: 7,
        }],
    };

    {
        let session = DbSession::open("telegram-test").expect("open session storage");
        session.set_home_dc_id(4).await.expect("set home dc");
        session
            .set_dc_option(&test_dc_option())
            .await
            .expect("set dc option");
        session
            .set_update_state(UpdateState::All(state.clone()))
            .await
            .expect("set update state");
    }

    let reopened = DbSession::open("telegram-test").expect("reopen session storage");
    assert_eq!(reopened.home_dc_id().expect("home dc"), 4);
    let option = reopened.dc_option(2).expect("dc option").expect("dc 2 present");
    assert_eq!(option, test_dc_option());
    let restored = reopened.updates_state().await.expect("updates state");
    assert_eq!(restored.pts, 11);
    assert_eq!(restored.qts, 22);
    assert_eq!(restored.date, 33);
    assert_eq!(restored.seq, 44);
    assert_eq!(restored.channels.len(), 1);
    assert_eq!(restored.channels[0].id, -100123);
    assert_eq!(restored.channels[0].pts, 7);

    // Sessions are keyed per bot: another key must not see this state.
    // A fresh session still knows the five static DC options, so assert on
    // the auth key rather than on the option itself.
    let other = DbSession::open("telegram-other").expect("open second session");
    assert_eq!(other.home_dc_id().expect("home dc"), 2);
    assert!(other
        .dc_option(2)
        .expect("dc option")
        .expect("static dc 2 present")
        .auth_key
        .is_none());
}

#[tokio::test]
#[serial]
async fn purge_drops_the_auth_key_so_self_heal_reauthenticates() {
    let dir = tempfile::tempdir().expect("temp dir");

    unsafe {
        std::env::set_var("DATABASE_PATH", dir.path().join("tiktok_downloader.db"));
    }
    init_database().expect("app database initialization");

    {
        let session = DbSession::open("telegram-test").expect("open session storage");
        session
            .set_dc_option(&test_dc_option())
            .await
            .expect("set dc option");
    }

    DbSession::purge("telegram-test").expect("purge session state");

    let session = DbSession::open("telegram-test").expect("reopen session storage");
    let option = session.dc_option(2).expect("dc option").expect("dc 2 present");
    assert_eq!(option.auth_key, None);
}
