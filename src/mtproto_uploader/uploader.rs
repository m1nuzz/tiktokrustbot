use grammers_client::client::InitParams;
use grammers_client::{Client, Config};
use grammers_session::Session;
use std::collections::VecDeque;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::mtproto_uploader::constants::SESSION_FILE;

// Add import for tl functions
use grammers_tl_types as tl;

#[derive(Clone)]
pub struct MTProtoUploader {
    pub client: Arc<Mutex<Client>>,
    pub ffprobe_path: PathBuf,
    pub ffmpeg_path: PathBuf,
    pub bot_token: String,
    pub session_file: PathBuf,
}

impl MTProtoUploader {
    pub async fn new(
        bot_token: &str,
        ffprobe_path: PathBuf,
        ffmpeg_path: PathBuf,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
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
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let api_id: i32 = env::var("TELEGRAM_API_ID")?.parse()?;
        let api_hash = env::var("TELEGRAM_API_HASH")?;

        let is_test_mode = env::var("TEST_MODE")
            .unwrap_or_else(|_| "false".to_string())
            .to_lowercase()
            == "true";

        if is_test_mode && session_file.exists() {
            log::warn!(
                "⚠️  TEST MODE detected. Using session file: {}",
                session_file.display()
            );
        }

        let session = Session::load_file_or_create(session_file.clone())?;

        // Configure initialization parameters
        let params = InitParams {
            device_model: "Desktop".to_string(),
            system_version: "Windows 10".to_string(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            system_lang_code: "en".to_string(),
            lang_code: "en".to_string(),
            catch_up: false,
            // Connect to Test DC2 (Amsterdam) if TEST_MODE is enabled
            server_addr: if is_test_mode {
                Some("149.154.167.40:443".parse().unwrap())
            } else {
                None
            },
            flood_sleep_threshold: 60,
            update_queue_limit: Some(1000),
            ..Default::default()
        };

        let client = Client::connect(Config {
            session,
            api_id,
            api_hash: api_hash.clone(),
            params,
        })
        .await?;

        if !client.is_authorized().await? {
            client.bot_sign_in(bot_token).await?;
        }
        client.session().save_to_file(session_file.clone())?;

        // Wrap the client in Arc<Mutex<>> for reconnection capability
        let client = Arc::new(Mutex::new(client));

        // Run keep-alive ping in a separate task. Frequent pings keep NAT /
        // middleboxes from killing the idle TCP connection (which used to
        // cause a full reconnect storm). A full reconnect replaces the live
        // client and kills in-flight uploads, so it only happens after
        // several consecutive ping failures, not on a single blip.
        let client_keepalive = client.clone();
        let keepalive_token = bot_token.to_string();
        let keepalive_session = session_file.clone();
        tokio::spawn(async move {
            // Frequent enough to keep idle connections alive through NAT.
            let mut interval = tokio::time::interval(Duration::from_secs(120));
            let mut consecutive_failures: u32 = 0;
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
                interval.tick().await;

                // Try to execute ping
                let result = {
                    let client_guard = client_keepalive.lock().await;
                    client_guard
                        .invoke(&tl::functions::updates::GetState {})
                        .await
                };

                match result {
                    Ok(_) => {
                        consecutive_failures = 0;
                        log::debug!("Keep-alive ping successful")
                    }
                    Err(e) => {
                        // Fast re-check instead of waiting a full 120 s tick
                        // per strike: a dead connection is confirmed in ~10 s
                        // while still requiring 3 consecutive failures before
                        // any reconnect happens.
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        consecutive_failures += 1;
                        if consecutive_failures < 3 {
                            log::warn!(
                                "Keep-alive ping failed ({}/3): {:?}, retrying before reconnecting...",
                                consecutive_failures,
                                e
                            );
                            continue;
                        }
                        consecutive_failures = 0;
                        log::error!(
                            "Keep-alive ping failed 3 times in a row: {:?}, reconnecting...",
                            e
                        );

                        // Reconnection attempt. Note: replacing the client
                        // drops the old one (grammers disconnects on drop),
                        // so in-flight uploads fail over via with_reconnect_retry.
                        if let Err(reconnect_err) = MTProtoUploader::reconnect_client(
                            &client_keepalive,
                            &keepalive_token,
                            &keepalive_session,
                        )
                        .await
                        {
                            log::error!("Reconnection failed: {:?}", reconnect_err);
                        } else {
                            log::info!("Client reconnected successfully");

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
            ffprobe_path,
            ffmpeg_path,
            bot_token: bot_token.to_string(),
            session_file,
        })
    }

    async fn reconnect_client(
        client: &Arc<Mutex<Client>>,
        bot_token: &str,
        session_file: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let api_id: i32 = env::var("TELEGRAM_API_ID")?.parse()?;
        let api_hash = env::var("TELEGRAM_API_HASH")?;

        let is_test_mode = env::var("TEST_MODE")
            .unwrap_or_else(|_| "false".to_string())
            .to_lowercase()
            == "true";

        let session = Session::load_file_or_create(session_file)?;

        let params = InitParams {
            device_model: "Desktop".to_string(),
            system_version: "Windows 10".to_string(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            system_lang_code: "en".to_string(),
            lang_code: "en".to_string(),
            catch_up: false,
            server_addr: if is_test_mode {
                Some("149.154.167.40:443".parse().unwrap())
            } else {
                None
            },
            flood_sleep_threshold: 60,
            update_queue_limit: Some(1000),
            ..Default::default()
        };

        let new_client = Client::connect(Config {
            session,
            api_id,
            api_hash,
            params,
        })
        .await?;

        // Check authorization and reconnect as a bot if necessary
        if !new_client.is_authorized().await? {
            new_client.bot_sign_in(bot_token).await?;
        }
        new_client.session().save_to_file(session_file)?;

        // Replace the old client with a new one
        {
            let mut client_guard = client.lock().await;
            *client_guard = new_client;
        }

        Ok(())
    }

    /// The session's auth key is suspected flagged server-side: every
    /// reconnect re-uses the same key and the fresh connection dies again
    /// within seconds (keep-alive pings reset). Archive the session file
    /// and reconnect from scratch so Telegram issues a brand-new auth key.
    /// Rate limiting is handled by the caller (at most one heal per 10 min).
    async fn recreate_session(
        client: &Arc<Mutex<Client>>,
        bot_token: &str,
        session_file: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Ok(meta) = std::fs::metadata(session_file) {
            if meta.len() > 0 {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let backup = session_file.with_extension(format!("session.bad-{}", stamp));
                match std::fs::rename(session_file, &backup) {
                    Ok(()) => log::warn!("Archived flagged session to {}", backup.display()),
                    Err(e) => {
                        log::error!(
                            "Could not archive session {}: {:?}; removing it instead",
                            session_file.display(),
                            e
                        );
                        let _ = std::fs::remove_file(session_file);
                    }
                }
            }
        }
        // load_file_or_create now mints a fresh session; bot_sign_in runs
        // inside reconnect_client when the new session is unauthorized.
        Self::reconnect_client(client, bot_token, session_file).await
    }

    pub async fn with_reconnect_retry<T, F, Fut>(
        &self,
        operation: F,
    ) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, Box<dyn std::error::Error + Send + Sync>>>,
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

                    if let Err(reconnect_err) =
                        Self::reconnect_client(&self.client, &self.bot_token, &self.session_file)
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
