use grammers_client::Client;
use grammers_client::sender::{ConnectionParams, SenderPool, UpdatesConfiguration};
use grammers_session::Session;
use std::collections::VecDeque;
use std::env;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::mtproto_uploader::session::DbSession;
use crate::mtproto_uploader::constants::SESSION_FILE;

// Add import for tl functions
use grammers_tl_types as tl;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A connected client plus the background task driving its network I/O.
struct BuiltClient {
    client: Client,
    runner: JoinHandle<()>,
}

#[derive(Clone)]
pub struct MTProtoUploader {
    pub client: Arc<Mutex<Client>>,
    /// grammers 0.12 drives I/O through a background `SenderPoolRunner`.
    /// Keep its handle so a full reconnect tears the old pool down instead of
    /// leaving a second pool attached to the same session database.
    pub runner: Arc<Mutex<Option<JoinHandle<()>>>>,
    pub ffprobe_path: PathBuf,
    pub ffmpeg_path: PathBuf,
    pub bot_token: String,
    pub session_file: PathBuf,
}

fn is_test_mode() -> bool {
    env::var("TEST_MODE")
        .unwrap_or_else(|_| "false".to_string())
        .eq_ignore_ascii_case("true")
}

/// Session storage key for a bot, derived from the historic session file name
/// (`lib/telegram-<bot id>.session`). One key per bot so six uploaders never
/// share auth keys.
fn session_key(session_file: &Path) -> String {
    session_file
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "telegram".to_string())
}

async fn build_client(
    bot_token: &str,
    session_file: &Path,
    api_id: i32,
    api_hash: &str,
) -> Result<BuiltClient, BoxError> {
    let session = Arc::new(DbSession::open(session_key(session_file))?);

    // 0.12 removed InitParams::server_addr. Override the home DC address in
    // the session instead; the pool reads DcOption when it opens a connection.
    if is_test_mode() {
        let dc_id = session.home_dc_id()?;
        if let Some(mut option) = session.dc_option(dc_id)? {
            option.ipv4 = "149.154.167.40:443".parse()?;
            session.set_dc_option(&option).await?;
            log::warn!("TEST_MODE: routing MTProto DC {} to {}", dc_id, option.ipv4);
        }
    }

    let params = ConnectionParams {
        device_model: "Desktop".to_string(),
        system_version: "Windows 10".to_string(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        system_lang_code: "en".to_string(),
        lang_code: "en".to_string(),
        updates_channel_capacity: NonZeroUsize::new(1000).unwrap(),
        ..Default::default()
    };

    let SenderPool { runner, handle, updates, .. } =
        SenderPool::with_configuration(Arc::clone(&session), api_id, params);

    // The runner drives network I/O in the background. Unlike 0.7 this keeps
    // grammers' own 60 s PingDelayDisconnect flowing while the client is idle,
    // so Telegram no longer closes the socket at ~75 s (the "read 0 bytes"
    // storm we saw on every one of the six clients).
    let runner = tokio::spawn(runner.run());
    let client = Client::new(handle);

    if !client.is_authorized().await? {
        client.bot_sign_in(bot_token, api_hash).await?;
    }

    // grammers caches a peer only when the application consumes the updates
    // stream (build_peer_map runs inside UpdateStream::next). Dropping the
    // receiver - as this code did before - meant the cache never filled, and
    // every MTProto sendMedia failed with "Cannot resolve peer ... as bot
    // without username; dialogs are forbidden for bots" once a restart wiped
    // the in-memory cache (15 failed large deliveries on 2026-11-11). Drain
    // the stream for the lifetime of this client: each update caches its
    // peers into the session (memory + mtproto_peers), and the update state
    // syncs periodically so a restart resumes cleanly.
    let drain_client = client.clone();
    tokio::spawn(async move {
        match drain_client
            .stream_updates(updates, UpdatesConfiguration::default())
            .await
        {
            Ok(mut stream) => {
                let mut since_sync: u64 = 0;
                while let Ok(_update) = stream.next().await {
                    since_sync += 1;
                    if since_sync % 200 == 0 {
                        if let Err(e) = stream.sync_update_state().await {
                            log::warn!("MTProto update-state sync failed: {e:?}");
                        }
                    }
                }
                log::warn!("MTProto updates stream ended");
            }
            Err(e) => log::error!("MTProto updates stream unavailable: {e:?}"),
        }
    });

    Ok(BuiltClient { client, runner })
}

/// Drop the persisted session state of a bot whose auth key is suspected
/// flagged server-side. The next `build_client` starts from a fresh key and
/// `bot_sign_in` mints a brand-new one.
fn purge_session_storage(session_file: &Path) {
    let key = session_key(session_file);
    match DbSession::purge(&key) {
        Ok(()) => log::warn!("Purged MTProto session state for {}", key),
        Err(e) => log::error!("Could not purge MTProto session state for {}: {:?}", key, e),
    }
    // Leftovers from the grammers 0.7 / libsql era are dead weight now.
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", session_file.to_string_lossy(), suffix));
        if sidecar.exists() {
            let _ = std::fs::remove_file(&sidecar);
        }
    }
}

impl MTProtoUploader {
    pub async fn new(
        bot_token: &str,
        ffprobe_path: PathBuf,
        ffmpeg_path: PathBuf,
    ) -> Result<Self, BoxError> {
        Self::new_with_session(
            bot_token,
            PathBuf::from(SESSION_FILE),
            ffprobe_path,
            ffmpeg_path,
        )
        .await
    }

    /// Per-bot constructor: each bot keeps its own session file so two
    /// uploaders never fight over one file (auth keys and saved state are
    /// per session). `new` above is the single-bot case with the historic name.
    pub async fn new_with_session(
        bot_token: &str,
        session_file: PathBuf,
        ffprobe_path: PathBuf,
        ffmpeg_path: PathBuf,
    ) -> Result<Self, BoxError> {
        let api_id: i32 = env::var("TELEGRAM_API_ID")?.parse()?;
        let api_hash = env::var("TELEGRAM_API_HASH")?;

        if is_test_mode() && session_file.exists() {
            log::warn!(
                "⚠️  TEST MODE detected. Using session file: {}",
                session_file.display()
            );
        }

        let built = build_client(bot_token, &session_file, api_id, &api_hash).await?;
        let client = Arc::new(Mutex::new(built.client));
        let runner = Arc::new(Mutex::new(Some(built.runner)));

        // Health probe with jitter and exponential backoff. grammers 0.12
        // already keeps the socket alive with its own ping, so this loop is a
        // watchdog only: staggered phase, quiet logs, backoff while the link
        // stays unhealthy, and session self-heal on repeated reconnects.
        // All uploaders used to start together and tick on the exact same
        // 120 s boundary, which made every client reconnect and log in the
        // same second.
        let client_keepalive = client.clone();
        let runner_keepalive = runner.clone();
        let keepalive_token = bot_token.to_string();
        let keepalive_session = session_file.clone();
        tokio::spawn(async move {
            const PING_BASE: Duration = Duration::from_secs(120);
            const JITTER_MS: u64 = 25_000;
            const MAX_BACKOFF: Duration = Duration::from_secs(300);

            // Random start phase: the six clients no longer share a tick.
            tokio::time::sleep(Duration::from_millis(rand::random::<u64>() % 60_000)).await;

            let mut consecutive_failures: u32 = 0;
            let mut backoff_level: u32 = 0;
            // Self-heal bookkeeping: on a healthy link a reconnect is rare.
            // A burst of reconnects where each fresh connection dies again
            // means the session's auth key is flagged server-side (observed
            // 2026-11-09: Telegram started resetting known bot MTProto
            // sessions; reconnecting with the same key loops forever).
            // Archiving the session file and reconnecting forces a
            // brand-new auth key and heals the uploader.
            let mut recent_reconnects: VecDeque<Instant> = VecDeque::new();
            let mut last_heal: Option<Instant> = None;
            loop {
                let jitter = Duration::from_millis(rand::random::<u64>() % JITTER_MS);
                // Extra backoff is zero on a healthy link, so a normal probe
                // still happens every ~120-145 s. While the link is dead it
                // grows 30s -> 60s -> 120s -> 240s (capped at 300s).
                let backoff = if backoff_level == 0 {
                    Duration::ZERO
                } else {
                    std::cmp::min(
                        Duration::from_secs(
                            30u64.saturating_mul(1u64 << (backoff_level - 1).min(3)),
                        ),
                        MAX_BACKOFF,
                    )
                };
                tokio::time::sleep(PING_BASE + jitter + backoff).await;

                // Try to execute the probe
                let result = {
                    let client_guard = client_keepalive.lock().await;
                    client_guard
                        .invoke(&tl::functions::updates::GetState {})
                        .await
                };

                match result {
                    Ok(_) => {
                        if backoff_level > 0 {
                            log::info!(
                                "MTProto link recovered (was at backoff level {})",
                                backoff_level
                            );
                        }
                        consecutive_failures = 0;
                        backoff_level = 0;
                        log::debug!("MTProto health probe successful")
                    }
                    Err(e) => {
                        // Retry quickly at first, then double the delay while
                        // the link stays dead (5s -> 10s -> 20s -> ... 300s).
                        let retry_delay = std::cmp::min(
                            Duration::from_secs(5u64.saturating_mul(1u64 << backoff_level.min(6))),
                            MAX_BACKOFF,
                        );
                        tokio::time::sleep(retry_delay).await;
                        consecutive_failures += 1;
                        if consecutive_failures < 3 {
                            log::debug!(
                                "MTProto health probe failed ({}/3): {:?}, retrying in {:?}",
                                consecutive_failures,
                                e,
                                retry_delay
                            );
                            continue;
                        }
                        consecutive_failures = 0;
                        let next_level = (backoff_level + 1).min(6);
                        let escalated = next_level != backoff_level;
                        // Only log on escalation: a permanently dead link
                        // otherwise produced hundreds of identical lines/hour.
                        if escalated {
                            log::warn!(
                                "MTProto link unhealthy (backoff level {} -> {}): {:?}",
                                backoff_level,
                                next_level,
                                e
                            );
                        } else {
                            log::debug!("Reconnecting MTProto client: {:?}", e);
                        }
                        backoff_level = next_level;

                        // Rebuild the whole pool. Replacing the client drops
                        // the old one, and in-flight uploads fail over via
                        // with_reconnect_retry.
                        if let Err(reconnect_err) = MTProtoUploader::reconnect_client(
                            &client_keepalive,
                            &runner_keepalive,
                            &keepalive_token,
                            &keepalive_session,
                        )
                        .await
                        {
                            if escalated {
                                log::warn!("Reconnection failed: {:?}", reconnect_err);
                            } else {
                                log::debug!("Reconnection failed: {:?}", reconnect_err);
                            }
                        } else {
                            log::debug!("Client reconnected successfully");

                            // Self-heal: 3+ reconnects within 10 minutes
                            // (throttled to one heal per 10 minutes) means
                            // the auth key itself is being rejected; archive
                            // the session and re-auth with a fresh key.
                            let now = Instant::now();
                            recent_reconnects.push_back(now);
                            while let Some(front) = recent_reconnects.front() {
                                if now.duration_since(*front) <= Duration::from_secs(600) {
                                    break;
                                }
                                recent_reconnects.pop_front();
                            }
                            let throttle_ok = match last_heal {
                                Some(t) => now.duration_since(t) >= Duration::from_secs(600),
                                None => true,
                            };
                            if recent_reconnects.len() >= 3 && throttle_ok {
                                let burst = recent_reconnects.len();
                                recent_reconnects.clear();
                                last_heal = Some(now);
                                log::warn!(
                                    "🚑 {} reconnects in 10 minutes: MTProto session looks \
                                     flagged server-side; recreating session (fresh auth key)",
                                    burst
                                );
                                if let Err(heal_err) = MTProtoUploader::recreate_session(
                                    &client_keepalive,
                                    &runner_keepalive,
                                    &keepalive_token,
                                    &keepalive_session,
                                )
                                .await
                                {
                                    log::error!("Session recreation failed: {:?}", heal_err);
                                } else {
                                    log::info!("✅ Session recreated with a fresh auth key");
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            client,
            runner,
            ffprobe_path,
            ffmpeg_path,
            bot_token: bot_token.to_string(),
            session_file,
        })
    }

    async fn reconnect_client(
        client: &Arc<Mutex<Client>>,
        runner: &Arc<Mutex<Option<JoinHandle<()>>>>,
        bot_token: &str,
        session_file: &Path,
    ) -> Result<(), BoxError> {
        let api_id: i32 = env::var("TELEGRAM_API_ID")?.parse()?;
        let api_hash = env::var("TELEGRAM_API_HASH")?;

        let built = build_client(bot_token, session_file, api_id, &api_hash).await?;

        // Tear down the old pool before swapping clients: both pools would
        // otherwise hold a handle to the same session database.
        let mut runner_guard = runner.lock().await;
        if let Some(old) = runner_guard.take() {
            old.abort();
        }
        {
            let mut client_guard = client.lock().await;
            *client_guard = built.client;
        }
        *runner_guard = Some(built.runner);

        Ok(())
    }

    /// The session's auth key is suspected flagged server-side: every
    /// reconnect re-uses the same key and the fresh connection dies again
    /// within seconds. Archive the session and reconnect from scratch so
    /// Telegram issues a brand-new auth key.
    /// Rate limiting is handled by the caller (at most one heal per 10 min).
    async fn recreate_session(
        client: &Arc<Mutex<Client>>,
        runner: &Arc<Mutex<Option<JoinHandle<()>>>>,
        bot_token: &str,
        session_file: &Path,
    ) -> Result<(), BoxError> {
        // Stop the old pool first so it releases the database file.
        if let Some(old) = runner.lock().await.take() {
            old.abort();
        }
        purge_session_storage(session_file);
        // DbSession::purge removed the stored auth key; bot_sign_in runs inside
        // reconnect_client when the new session is unauthorized.
        Self::reconnect_client(client, runner, bot_token, session_file).await
    }

    pub async fn with_reconnect_retry<T, F, Fut>(&self, operation: F) -> Result<T, BoxError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, BoxError>>,
    {
        let max_retries = 3;
        for attempt in 0..max_retries {
            let result = operation().await;

            match result {
                Ok(value) => return Ok(value),
                Err(e)
                    if e.to_string().contains("read 0 bytes")
                        || e.to_string().contains("ConnectionReset")
                        || e.to_string().contains("Connection lost") =>
                {
                    log::warn!(
                        "Connection lost, reconnecting... (attempt {}/{})",
                        attempt + 1,
                        max_retries
                    );

                    if let Err(reconnect_err) = Self::reconnect_client(
                        &self.client,
                        &self.runner,
                        &self.bot_token,
                        &self.session_file,
                    )
                    .await
                    {
                        log::error!("Reconnection failed: {:?}", reconnect_err);
                        if attempt == max_retries - 1 {
                            return Err(e);
                        }
                    } else {
                        log::info!("Client reconnected successfully");
                        if attempt < max_retries - 1 {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }

        // If we have reached this point, then this is an error that is not related to the connection
        // or all reconnection attempts were unsuccessful
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Operation failed after retries",
        )))
    }
}
