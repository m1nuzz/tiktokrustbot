use axum::{
    extract::{State, Query},
    routing::{get, post},
    Json, Router,
    response::Html,
};
use axum::http::{header, HeaderMap, HeaderValue};
use tower_http::cors::CorsLayer;
use std::sync::Arc;
use std::time::Duration;
use crate::database::{ClaimVia, DatabasePool, SessionYmid};
use crate::yt_dlp_interface::YoutubeFetcher;
use crate::mtproto_uploader::MTProtoUploader;
use crate::utils::task_manager::TaskManager;
use serde::{Deserialize, Serialize};
use serde_json::json;
use teloxide::prelude::*;

/// Mini-app HTML embedded at compile time — no need to deploy the folder separately
const MINI_APP_HTML: &str = include_str!("../mini-app/index.html");

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<DatabasePool>,
    pub bot: Bot,
    pub fetcher: Arc<YoutubeFetcher>,
    pub mtproto_uploader: Arc<MTProtoUploader>,
    pub task_manager: Arc<tokio::sync::Mutex<TaskManager>>,
    pub upload_semaphore: Arc<tokio::sync::Semaphore>,
}

#[derive(Deserialize, Debug)]
pub struct PostbackQuery {
    pub ymid: String,
    // Accept both "value" (per Monetag docs) and "reward_event_type" for backwards compatibility
    #[serde(alias = "value", alias = "reward_event_type")]
    pub reward_event_type: String,
    #[serde(default)]
    pub event_type: Option<String>,
    #[serde(default)]
    pub estimated_price: Option<f64>,
    // Placement attribution: which SDK call produced this event
    // ('sdk_interstitial', 'popup_retry_N', 'valued_bonus_popup', ...).
    // Optional so zones whose SSP template lacks the macros keep working.
    // Aliases cover the SSP template naming variants.
    #[serde(default, alias = "requestVar", alias = "source")]
    pub request_var: Option<String>,
    #[serde(default, alias = "subZoneId", alias = "sub")]
    pub sub_zone_id: Option<String>,
    // Optional shared secret (MONETAG_POSTBACK_SECRET env must match when set)
    #[serde(default)]
    pub secret: Option<String>,
}

/// Only the ymid is read from a claim body. serde ignores unknown fields, so
/// a body sent by an already-open older client still deserializes: whatever
/// attestation it carries is dropped and is not part of the gate any more.
#[derive(Deserialize)]
pub struct ClaimRequest {
    pub ymid: String,
}

#[derive(Deserialize)]
pub struct CheckStatusQuery {
    pub ymid: String,
}

#[derive(Deserialize)]
pub struct AdsStatusQuery {
    pub ymid: Option<String>,
}

#[derive(Serialize)]
pub struct CheckStatusResponse {
    pub status: String,
    /// Server-side lease deadline for this session, so the client learns the
    /// deadline from the server instead of running a timer of its own.
    /// NULL for rows created before the lease column existed.
    pub lease_expires_at: Option<String>,
}

/// Lease heartbeat. The mini-app calls this while the webapp is open to push
/// the row's lease forward; it is the only thing that keeps a session
/// claimable beyond the initial window.
///
/// CLIENT HOOK (the client-side task must wire exactly this, in
/// `mini-app/index.html`): POST `{"ymid": "<ymid>"}` to `/api/session-ping`
/// every 60 seconds from load until the webapp is closed or the response says
/// `live:false`. Start it as soon as the page loads, and stop it from the
/// `visibilitychange`/`pagehide` handlers. Do NOT release or shrink the lease
/// on close - just stop pinging: the server keeps the row alive for whatever
/// remained of its lease (up to the ceiling), which is what lets a valued
/// postback that Monetag sends while the user is on the advertiser's page
/// still deliver.
#[derive(Deserialize)]
pub struct SessionPingRequest {
    pub ymid: String,
}

#[derive(Serialize)]
pub struct SessionPingResponse {
    pub status: String,
    pub lease_expires_at: Option<String>,
    /// False for an unknown or terminal row: the client must stop pinging.
    pub live: bool,
}

#[derive(Deserialize)]
pub struct AdImpressionQuery {
    pub ymid: String,
}

/// One ad event = one ymid. Monetag's docs require a unique ymid per event and
/// leave frequency capping to the publisher, and live traffic showed one ymid
/// accumulating valued impression+click pairs for eight hours (duplicate
/// impressions are priced at zero).
///
/// The mini-app calls this once at load, before any ad call, and then uses the
/// returned ymid for every show_*() and every API call. The entry ymid is the one
/// the bot button put in the URL.
#[derive(Deserialize)]
pub struct NewSessionRequest {
    pub ymid: String,
}

#[derive(Serialize)]
pub struct NewSessionResponse {
    /// False means "keep the ymid you already have": rotation must never block
    /// the funnel, so every failure path answers this instead of an error the
    /// client would have to handle as fatal.
    pub success: bool,
    pub ymid: String,
    pub reused: bool,
}

fn new_session_response(session: SessionYmid) -> NewSessionResponse {
    NewSessionResponse {
        success: true,
        ymid: session.ymid,
        reused: session.reused,
    }
}

fn new_session_refusal(ymid: &str) -> NewSessionResponse {
    NewSessionResponse {
        success: false,
        ymid: ymid.to_string(),
        reused: false,
    }
}

async fn new_session(
    State(state): State<AppState>,
    Json(payload): Json<NewSessionRequest>,
) -> Json<NewSessionResponse> {
    match state.db.resolve_session_ymid(&payload.ymid).await {
        Ok(Some(session)) => {
            log::info!(
                "Session rotation for entry ymid {}: {} ({})",
                payload.ymid,
                session.ymid,
                if session.reused { "reused" } else { "minted" }
            );
            Json(new_session_response(session))
        }
        Ok(None) => {
            log::warn!("Session rotation for unknown entry ymid {}", payload.ymid);
            Json(new_session_refusal(&payload.ymid))
        }
        Err(e) => {
            log::error!("Session rotation failed for {}: {}", payload.ymid, e);
            Json(new_session_refusal(&payload.ymid))
        }
    }
}

/// Unchanged wording on purpose: the mini-app renders its own localised text,
/// so this string is only the API/log field.
const CLAIM_PENDING_ERROR: &str =
    "Ad verification not received yet. Please finish watching the ad or wait a few seconds.";

const CLAIM_INVALID_REQUEST_ERROR: &str = "Invalid request ID";

/// A refused claim, as a pure function of the row's state.
///
/// The client must tell three refusals apart: "not yet, keep waiting", "this
/// session is gone" and "this ymid does not exist". Only the first is worth
/// waiting on, and one generic answer left an expired session looking temporary
/// - the screen promised a retry that could never work.
fn claim_refusal(status: &str, error: &str) -> serde_json::Value {
    json!({
        "success": false,
        "status": status,
        "error": error,
    })
}

/// Row state a refused claim reports, read once after the bounded wait.
async fn claim_refusal_response(db: &Arc<DatabasePool>, ymid: &str) -> serde_json::Value {
    let status = match db.get_pending_download_status(ymid).await {
        Ok(Some(status)) => status,
        Ok(None) => "not_found".to_string(),
        Err(e) => {
            log::error!("claim refusal status read failed for {}: {}", ymid, e);
            "unknown".to_string()
        }
    };
    claim_refusal(&status, CLAIM_PENDING_ERROR)
}

/// Client-side funnel beacon. Fire-and-forget: the mini-app must never wait
/// for telemetry, so the response is empty and failures are logged only.
#[derive(Deserialize)]
pub struct MiniAppEventRequest {
    pub ymid: String,
    pub event: String,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub sdk_host: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
}

async fn log_mini_app_event(
    State(state): State<AppState>,
    // Raw body, not axum's `Json`: `navigator.sendBeacon` posts `text/plain`
    // by default and `Json` rejects anything but `application/json` (415) —
    // that is how every beacon got silently eaten.
    body: String,
) -> axum::http::StatusCode {
    let payload: MiniAppEventRequest = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(_) => return axum::http::StatusCode::BAD_REQUEST,
    };
    if let Err(e) = state
        .db
        .log_mini_app_event(
            &payload.ymid,
            &payload.event,
            payload.platform.as_deref(),
            payload.sdk_host.as_deref(),
            payload.user_agent.as_deref(),
        )
        .await
    {
        log::error!("Failed to journal mini-app event {}: {}", payload.event, e);
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR;
    }
    axum::http::StatusCode::OK
}

#[derive(Serialize)]
pub struct AdImpressionResponse {
    /// True when Monetag journaled at least one ad view for this ymid.
    pub impression: bool,
}

pub async fn start_web_server(state: AppState, port: u16) {
    let app = Router::new()
        .route("/api/ads-status", get(get_ads_status))
        .route("/api/monetag-postback", get(monetag_postback))
        .route("/api/check-status", get(check_ad_status))
        .route("/api/ad-impression", get(get_ad_impression))
        .route("/api/mini-app-event", post(log_mini_app_event))
        .route("/api/claim-video", post(claim_video))
        .route("/api/session-ping", post(session_ping))
        .route("/api/new-session", post(new_session))
        .fallback(serve_mini_app)
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    log::info!("Starting web server on {}", addr);
    
    let listener = tokio::net::TcpListener::bind(addr).await.expect("Failed to bind web server port");
    axum::serve(listener, app).await.expect("Failed to start axum server");
}

/// Substitutes the per-locale STRINGS dict into the mini-app template.
/// The template line reads `const STRINGS = /*STRINGS_INJECT*/null`, so the
/// marker INCLUSIVE of the trailing `null` is replaced by the bare dict —
/// anything else (a second `const`, a leftover `null`) is a syntax error
/// that kills the entire inline script. Shipped broken once; the unit test
/// below guards the exact template shape.
fn inject_mini_app_strings(html: &str, lang: &str, dict: &serde_json::Value) -> String {
    let html = html.replace("/*STRINGS_INJECT*/null", &dict.to_string());
    html.replace(
        "<html lang=\"en\">",
        &format!(
            "<html lang=\"{}\">",
            crate::i18n::resolve_lang(Some(lang))
        ),
    )
}

/// Substitutes the bot username into the mini-app template, the same way the
/// STRINGS dict is injected.
///
/// The username lives nowhere in the codebase and must never be hardcoded in the
/// client: it is only rendered into a `t.me/<username>` deep link, so the value
/// is whitelisted down to the characters a Telegram username can contain (a
/// leading `@` is dropped, everything else that is not `[A-Za-z0-9_]` is
/// removed). Unset or unusable leaves the literal `null` in place, and the
/// client then simply does not render the premium button.
///
/// A JSON string literal is emitted rather than raw interpolation, so no value
/// can terminate the declaration or inject script.
fn inject_bot_username(html: &str, username: Option<&str>) -> String {
    let literal = username
        .map(|name| name.trim().trim_start_matches('@'))
        .map(|name| name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<String>())
        .filter(|name| !name.is_empty())
        .map(|name| serde_json::Value::String(name).to_string())
        .unwrap_or_else(|| "null".to_string());
    html.replace("/*BOT_USERNAME_INJECT*/null", &literal)
}

async fn serve_mini_app(
    State(state): State<AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> impl axum::response::IntoResponse {
    // Get SmartLink from environment variable with fallback
    let smartlink = std::env::var("MONETAG_SMARTLINK")
        .unwrap_or_else(|_| "https://omg10.com/4/11148490".to_string());

    // Inject SmartLink URL into HTML
    let html = MINI_APP_HTML.replace("// SMARTLINK_INJECT", &format!("const AD_CONFIG_SMARTLINK = \"{}\";", smartlink));
    // Localise the mini-app the same way the bot localises replies: an
    // explicit ?lang= (appended by the bot button) wins, else the stored
    // /language override for the ymid's owner, else English. The resolved
    // dict is injected below; the client never guesses the language itself.
    let lang = match query.get("lang").map(|s| s.as_str()) {
        Some(code) if !code.is_empty() => crate::i18n::resolve_lang(Some(code)).to_string(),
        _ => match query.get("ymid") {
            Some(ymid) if !ymid.is_empty() => match state.db.get_user_id_by_ymid(ymid).await {
                Ok(user_id) => state.db.get_effective_lang(user_id, None).await,
                Err(_) => "en".to_string(),
            },
            _ => "en".to_string(),
        },
    };
    let mut dict = serde_json::Map::new();
    for (name, key) in crate::i18n::MINI_APP_STRINGS {
        dict.insert(
            name.to_string(),
            serde_json::Value::String(crate::i18n::t(*key, Some(&lang)).to_string()),
        );
    }
    let html = inject_mini_app_strings(&html, &lang, &serde_json::Value::Object(dict));
    // The bot username, injected the same way as the STRINGS dict so the
    // ad-blocked screen can offer premium without the username ever appearing in
    // the client source. Unset in the environment means no premium button.
    let bot_username = std::env::var("BOT_USERNAME").ok();
    let html = inject_bot_username(&html, bot_username.as_deref());
    // Never cache the document: Telegram WebViews keep serving a stale copy
    // otherwise, and users get stuck on old funnel screens (e.g. a blue,
    // always-visible Continue from a build before btn-success/watch-gate).
    // Every open already carries a unique ?ymid=, this only stops the client
    // from reusing a previous document for it.
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store, no-cache, must-revalidate, max-age=0"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    (headers, Html(html))
}

async fn get_ads_status(
    State(state): State<AppState>,
    Query(query): Query<AdsStatusQuery>,
) -> Json<serde_json::Value> {
    // Per-user decision when the mini-app passes its ymid (same rules as the
    // bot handler, so an admin testing with Admin Ads ON sees the real ad
    // flow even while ads are globally OFF). Unknown/missing ymid falls back
    // to the global switch (backwards compatible).
    if let Some(ymid) = query.ymid.as_deref().filter(|s| !s.is_empty()) {
        match state.db.get_user_id_by_ymid(ymid).await {
            Ok(user_id) => {
                let is_user_admin = crate::handlers::admin::is_admin_id(user_id);
                let enabled =
                    crate::handlers::link::ads_enabled_for(&state.db, user_id, is_user_admin).await;
                return Json(json!({ "enabled": enabled }));
            }
            Err(e) => {
                log::warn!("ads-status with unknown ymid {}: {}, using global", ymid, e);
            }
        }
    }

    let module_enabled = std::env::var("MONETAG_MODULE_ENABLED")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(true);

    if !module_enabled {
        return Json(json!({ "enabled": false }));
    }

    let enabled = match state.db.get_setting("ads_enabled").await {
        Ok(val) => val == "true",
        Err(e) => {
            log::error!("Error fetching ads_enabled setting: {}", e);
            true
        }
    };

    Json(json!({ "enabled": enabled }))
}

/// Shared download spawner for manual claims and valued auto-delivery.
/// Logs the delivery reason so the funnel can split paid vs timer giveaways.
async fn spawn_download_job(
    state: AppState,
    user_id: i64,
    url: String,
    ymid: String,
    via: ClaimVia,
) {
    let reason = match via {
        ClaimVia::Verified => "delivered_valued",
        ClaimVia::Timer => "delivered_timer",
        ClaimVia::Admin => "delivered_admin",
    };
    state.db.log_funnel_event(user_id, reason).await;
    // A download really begins here, so record it: this is what lets the
    // expiry sweeper tell a failed download from a session that never earned
    // its ad, and therefore which of the two messages the user gets.
    match state.db.mark_job_started(&ymid).await {
        Ok(0) => log::warn!("Job start recorded for unknown ymid {}", ymid),
        Ok(_) => {}
        Err(e) => log::error!("Failed to record job start for {}: {}", ymid, e),
    }
    // Resolve locale from the stored /language override (no Telegram
    // User object in this flow, so no device language available).
    let lang = state.db.get_effective_lang(user_id, None).await;
    tokio::spawn(async move {
        if let Err(e) = crate::handlers::link::process_video_request(
            state.bot,
            user_id,
            url,
            state.fetcher,
            state.mtproto_uploader,
            state.db,
            state.task_manager,
            state.upload_semaphore,
            None,
            ChatId(user_id),
            Some(lang),
            Some(ymid)
        ).await {
            log::error!("Error processing claimed download: {}", e);
        }
    });
}

/// Bounded retry for the postback ordering race only: the valued postback
/// that triggered this task is journaled before the task starts, so the gate
/// normally passes on the first attempt.
const AUTO_DELIVERY_ATTEMPTS: u32 = 3;
const AUTO_DELIVERY_INTERVAL: Duration = Duration::from_secs(2);

/// One client POST races Monetag's postback: the ad settles before the backend
/// confirmation is journaled, so the first attempt can lose by a second or
/// two. Three attempts 1.5s apart cover that window; `success:false` is only
/// answered once the wait is exhausted.
const CLAIM_WAIT_ATTEMPTS: u32 = 3;
const CLAIM_WAIT_INTERVAL: Duration = Duration::from_millis(1500);

/// Retry a single-use claim within a bounded window. Split out from the
/// handler so the wait itself is testable without axum wiring.
async fn claim_with_bounded_wait<F, Fut>(
    attempts: u32,
    interval: Duration,
    mut claim_once: F,
) -> Result<(i64, String, ClaimVia), anyhow::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(i64, String, ClaimVia), anyhow::Error>>,
{
    let mut last: Result<(i64, String, ClaimVia), anyhow::Error> =
        Err(anyhow::anyhow!("claim never attempted"));
    for attempt in 0..attempts {
        match claim_once().await {
            Ok(won) => return Ok(won),
            Err(e) => last = Err(e),
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(interval).await;
        }
    }
    last
}

/// What a valued postback actually did to the row. The first two can still
/// deliver; the last two never can.
#[derive(Debug, PartialEq, Eq)]
enum VerifyOutcome {
    Verified,
    AlreadyVerified,
    Terminal(String),
    Unknown,
}

/// `mark_as_verified_with_logging` reports affected rows, so 0 rows only means
/// "this postback changed nothing" - the row itself says why.
fn classify_verify(rows: usize, status: Option<&str>) -> VerifyOutcome {
    if rows > 0 {
        return VerifyOutcome::Verified;
    }
    match status {
        Some("verified") => VerifyOutcome::AlreadyVerified,
        Some(other) => VerifyOutcome::Terminal(other.to_string()),
        None => VerifyOutcome::Unknown,
    }
}

/// Result of processing one valued postback. `Delivered` is the only variant
/// that may spawn a download job.
#[derive(Debug, PartialEq, Eq)]
enum ValuedPostbackResult {
    Delivered {
        user_id: i64,
        url: String,
        via: ClaimVia,
    },
    NotDeliverable(String),
    NotValuedYet,
    DbError(String),
}

/// One valued postback end to end: verify the row truthfully, then take the
/// single-use claim. Monetag retries any postback that does not answer 200, so
/// this runs many times per ymid and the atomic claim - not luck - is what
/// keeps delivery at exactly one. Every outcome is logged: a silent task is as
/// bad as the dropped handler future it replaced.
async fn run_valued_delivery(db: &Arc<DatabasePool>, ymid: &str) -> ValuedPostbackResult {
    let rows = match db.mark_as_verified_with_logging(ymid).await {
        Ok(rows) => rows,
        Err(e) => {
            log::error!(
                "Valued postback for {}: mark-as-verified failed: {}",
                ymid,
                e
            );
            return ValuedPostbackResult::DbError(e.to_string());
        }
    };
    let status = match db.get_pending_download_status(ymid).await {
        Ok(status) => status,
        Err(e) => {
            log::error!("Valued postback for {}: status read failed: {}", ymid, e);
            return ValuedPostbackResult::DbError(e.to_string());
        }
    };

    match classify_verify(rows, status.as_deref()) {
        VerifyOutcome::Verified => log::info!("Valued postback: row {} marked verified", ymid),
        VerifyOutcome::AlreadyVerified => {
            log::info!("Valued postback for row {}: already verified", ymid)
        }
        VerifyOutcome::Terminal(status) => {
            log::info!(
                "Valued postback for row {} ignored: terminal row ({}), no delivery",
                ymid,
                status
            );
            return ValuedPostbackResult::NotDeliverable(status);
        }
        VerifyOutcome::Unknown => {
            log::warn!("Valued postback for unknown ymid {}, no delivery", ymid);
            return ValuedPostbackResult::NotDeliverable("unknown ymid".to_string());
        }
    }

    // The postback that triggered this task is already journaled, so the gate
    // normally passes on the first attempt. The retries exist only for
    // ordering: a valued click can be journaled before its valued impression.
    let claim_db = db.clone();
    let claim_ymid = ymid.to_string();
    match claim_with_bounded_wait(AUTO_DELIVERY_ATTEMPTS, AUTO_DELIVERY_INTERVAL, move || {
        let claim_db = claim_db.clone();
        let claim_ymid = claim_ymid.clone();
        async move { claim_db.claim_if_valued(&claim_ymid).await }
    })
    .await
    {
        Ok((user_id, url, via)) => ValuedPostbackResult::Delivered { user_id, url, via },
        Err(e) => {
            log::info!(
                "Valued postback for row {}: no claim after {} attempts: {}",
                ymid,
                AUTO_DELIVERY_ATTEMPTS,
                e
            );
            ValuedPostbackResult::NotValuedYet
        }
    }
}

async fn monetag_postback(
    State(state): State<AppState>,
    Query(query): Query<PostbackQuery>,
) -> impl axum::response::IntoResponse {
    let reward = query.reward_event_type.to_lowercase();
    log::info!(
        "Received Monetag postback: ymid={}, type={}, event={:?}, price={:?}",
        query.ymid, reward, query.event_type, query.estimated_price
    );

    // Optional shared secret: when MONETAG_POSTBACK_SECRET is set, requests
    // without a matching ?secret= are rejected (nobody can self-verify ymids
    // with curl). Unset = accept all (backwards compatible with SSP configs
    // that don't send the secret yet).
    if let Ok(secret_env) = std::env::var("MONETAG_POSTBACK_SECRET") {
        if !secret_env.is_empty() && query.secret.as_deref() != Some(secret_env.as_str()) {
            log::warn!("Postback with bad/missing secret for ymid {}", query.ymid);
            return axum::http::StatusCode::FORBIDDEN;
        }
    }

    // Journal EVERYTHING (even unknown ymids and non_valued): the Ads counters
    // and mismatch alerts are computed from this table.
    if let Err(e) = state
        .db
        .log_postback(
            &query.ymid,
            query.event_type.as_deref(),
            &reward,
            query.estimated_price,
            query.request_var.as_deref(),
            query.sub_zone_id.as_deref(),
        )
        .await
    {
        log::error!("Failed to journal postback for ymid {}: {}", query.ymid, e);
    }

    // Gate: only valued unlocks. Non-valued is counted above, nothing more.
    if reward != "valued" {
        log::info!("Non-valued postback for ymid {}, no unlock", query.ymid);
        return axum::http::StatusCode::OK;
    }

    let task_state = state.clone();
    let task_ymid = query.ymid.clone();
    // Detached valued sequence: Monetag times out slow postback responses,
    // so the HTTP handler must answer 200 OK immediately.
    tokio::spawn(async move {
        match run_valued_delivery(&task_state.db, &task_ymid).await {
            ValuedPostbackResult::Delivered { user_id, url, via } => {
                // Instant auto-delivery: the user may already be browsing the
                // advertiser's page, so the chat must not wait for a return.
                log::info!(
                    "Auto-delivering download for user {} (valued postback)",
                    user_id
                );
                spawn_download_job(task_state, user_id, url, task_ymid, via).await;
            }
            refused => log::debug!(
                "Valued postback for ymid {} delivered nothing: {:?}",
                task_ymid, refused
            ),
        }
    });

    axum::http::StatusCode::OK
}

/// Built as a pure function so both the extended and the dead-end branch are
/// testable without a Bot or an HTTP server.
fn ping_response(status: &str, lease_expires_at: Option<String>, live: bool) -> SessionPingResponse {
    SessionPingResponse {
        status: status.to_string(),
        lease_expires_at,
        live,
    }
}

async fn session_ping(
    State(state): State<AppState>,
    Json(payload): Json<SessionPingRequest>,
) -> Json<SessionPingResponse> {
    let db = state.db.clone();
    let ymid = payload.ymid;

    match db
        .refresh_session_lease(&ymid, crate::database::SESSION_LEASE_SECS)
        .await
    {
        Ok(Some((status, lease_expires_at))) => {
            Json(ping_response(&status, Some(lease_expires_at), true))
        }
        // Unknown ymid, or a row that is already terminal: a heartbeat must
        // never reanimate it, so tell the client to stop pinging.
        Ok(None) => match db.get_pending_download_status(&ymid).await {
            Ok(Some(status)) => Json(ping_response(&status, None, false)),
            Ok(None) => Json(ping_response("not_found", None, false)),
            Err(e) => {
                log::error!("session-ping status read failed for {}: {}", ymid, e);
                Json(ping_response("error", None, false))
            }
        },
        Err(e) => {
            log::error!("session-ping lease refresh failed for {}: {}", ymid, e);
            Json(ping_response("error", None, false))
        }
    }
}

fn check_status_response(
    state: Option<(String, Option<String>)>,
) -> CheckStatusResponse {
    match state {
        Some((status, lease_expires_at)) => CheckStatusResponse {
            status,
            lease_expires_at,
        },
        None => CheckStatusResponse {
            status: "not_found".to_string(),
            lease_expires_at: None,
        },
    }
}

async fn check_ad_status(
    State(state): State<AppState>,
    Query(query): Query<CheckStatusQuery>,
) -> Json<CheckStatusResponse> {
    let db = state.db.clone();
    let ymid = query.ymid.clone();

    match db.get_session_state(&ymid).await {
        Ok(snapshot) => Json(check_status_response(snapshot)),
        Err(e) => {
            log::error!("Error checking status for ymid {}: {}", ymid, e);
            Json(check_status_response(Some((
                "error".to_string(),
                None,
            ))))
        }
    }
}

/// Server-side oracle for the ad-block funnel. The SDK registers on blocked
/// iOS devices too, so a client-side probe cannot separate "no fill" from
/// "ad never presented": both look like a rejected/never-settling show() call.
/// What separates them is whether Monetag ever recorded a view for this ymid.
/// An unreachable/errored DB reports `false` so the client stays conservative
/// (adblock screen, no free video) instead of handing out the download.
async fn get_ad_impression(
    State(state): State<AppState>,
    Query(query): Query<AdImpressionQuery>,
) -> Json<AdImpressionResponse> {
    let db = state.db.clone();
    let ymid = query.ymid.clone();

    match db.has_ad_impression(&ymid).await {
        Ok(true) => Json(AdImpressionResponse { impression: true }),
        Ok(false) => Json(AdImpressionResponse { impression: false }),
        Err(e) => {
            log::warn!("ad-impression check failed for ymid {}: {}", ymid, e);
            Json(AdImpressionResponse { impression: false })
        }
    }
}

async fn claim_video(
    State(state): State<AppState>,
    Json(payload): Json<ClaimRequest>,
) -> Json<serde_json::Value> {
    log::info!("Received claim request for ymid: {}", payload.ymid);

    let db = state.db.clone();
    let ymid = payload.ymid.clone();

    // 1. Reject unknown ymids early (the shared gate below would fail
    // them anyway, but with a less specific error).
    if let Err(e) = db.get_user_id_by_ymid(&ymid).await {
        log::error!("Claim failed: Ymid {} not found: {}", ymid, e);
        return Json(claim_refusal("not_found", CLAIM_INVALID_REQUEST_ERROR));
    }

    // 2. Already delivered (by the valued-postback auto-delivery or by an
    // earlier POST): idempotent success, and no reason to wait for a postback.
    if let Ok(Some(status)) = db.get_pending_download_status(&ymid).await {
        if status == "completed" {
            log::info!(
                "Claim for ymid {} already completed, returning success",
                ymid
            );
            return Json(json!({ "success": true }));
        }
    }

    // 3. One gate for everyone, admins included: a valued impression is the
    // reward. No admin bypass (admin testing must reproduce the user
    // experience), no timer backstop and no client attestation is read. The
    // bounded wait lets this one request win the race against postback lag.
    let claim_db = db.clone();
    let claim_ymid = ymid.clone();
    let claim_result =
        claim_with_bounded_wait(CLAIM_WAIT_ATTEMPTS, CLAIM_WAIT_INTERVAL, move || {
            let claim_db = claim_db.clone();
            let claim_ymid = claim_ymid.clone();
            async move { claim_db.claim_if_valued(&claim_ymid).await }
        })
        .await;

    match claim_result {
        Ok((user_id, url, via)) => {
            log::info!("Claim success ({:?})! Triggering download for user {}: {}", via, user_id, url);
            spawn_download_job(state, user_id, url, ymid, via).await;
            Json(json!({ "success": true }))
        },
        Err(e) => {
            // The valued postback's own auto-delivery may have completed this
            // row while we were waiting, so the video is on its way.
            if let Ok(Some(status)) = db.get_pending_download_status(&ymid).await {
                if status == "completed" {
                    log::info!(
                        "Claim for ymid {} completed by auto-delivery while waiting, returning success",
                        ymid
                    );
                    return Json(json!({ "success": true }));
                }
            }
            log::error!(
                "Claim failed for ymid {} after {} attempts: {}",
                ymid,
                CLAIM_WAIT_ATTEMPTS,
                e
            );
            Json(claim_refusal_response(&db, &ymid).await)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client must learn its deadline from the server: both the heartbeat
    /// answer and the status check carry `lease_expires_at`, so no client timer
    /// invents its own.
    #[test]
    fn ping_and_check_status_payloads_carry_the_server_lease() {
        let deadline = "2026-10-07 12:00:00".to_string();

        let live =
            serde_json::to_string(&ping_response("verified", Some(deadline.clone()), true)).unwrap();
        assert!(live.contains("\"lease_expires_at\":\"2026-10-07 12:00:00\""));
        assert!(live.contains("\"live\":true"));

        let status_payload = serde_json::to_string(&check_status_response(Some((
            "pending".to_string(),
            Some(deadline.clone()),
        ))))
        .unwrap();
        assert!(status_payload.contains("\"lease_expires_at\":\"2026-10-07 12:00:00\""));
        assert!(status_payload.contains("\"status\":\"pending\""));
    }

    /// A heartbeat for a delivered row must be told the session is over: no
    /// lease is handed back (so the client stops its timer) and `live` is false.
    #[test]
    fn ping_response_for_a_terminal_row_carries_no_lease() {
        let payload = serde_json::to_string(&ping_response("completed", None, false)).unwrap();
        assert!(payload.contains("\"status\":\"completed\""));
        assert!(payload.contains("\"live\":false"));
        assert!(payload.contains("\"lease_expires_at\":null"));

        let unknown = serde_json::to_string(&check_status_response(None)).unwrap();
        assert!(unknown.contains("\"status\":\"not_found\""));
        assert!(unknown.contains("\"lease_expires_at\":null"));
    }

    /// Regression test: the STRINGS substitution must never emit a second
    /// `const STRINGS` declaration — that exact bug shipped once and killed
    /// the whole inline script (served page failed to parse, mini-app dead).
    #[test]
    fn strings_injection_emits_single_declaration() {
        let template = "<html lang=\"en\"><script>const STRINGS = /*STRINGS_INJECT*/null;</script>";
        let mut dict = serde_json::Map::new();
        dict.insert(
            "MiniNoAdsTitle".to_string(),
            serde_json::Value::String("x".to_string()),
        );
        let out = inject_mini_app_strings(
            template,
            "ru",
            &serde_json::Value::Object(dict),
        );
        assert_eq!(out.matches("const STRINGS").count(), 1);
        assert!(!out.contains("STRINGS_INJECT"));
        assert!(!out.contains("}null"), "leftover null after the dict");
        assert!(out.contains("<html lang=\"ru\">"));
        assert!(out.contains("\"MiniNoAdsTitle\":\"x\""));
    }

    /// The username injection must never break the inline script: a second
    /// `const BOT_USERNAME`, a leftover marker or a stray `null` all killed the
    /// whole page once already.
    #[test]
    fn bot_username_injection_emits_single_declaration() {
        let template = "<script>const BOT_USERNAME = /*BOT_USERNAME_INJECT*/null;</script>";

        let injected = inject_bot_username(template, Some("tikyoubot"));
        assert_eq!(injected.matches("const BOT_USERNAME").count(), 1);
        assert!(injected.contains("const BOT_USERNAME = \"tikyoubot\";"));
        assert!(!injected.contains("BOT_USERNAME_INJECT"));

        // Unset: the declaration survives with a null value, which is what makes
        // the client skip the button instead of rendering a broken deep link.
        let unset = inject_bot_username(template, None);
        assert_eq!(unset.matches("const BOT_USERNAME").count(), 1);
        assert!(unset.contains("const BOT_USERNAME = null;"));
    }

    /// The value is interpolated into served script, so it is whitelisted and
    /// emitted as a JSON literal: no quote, angle bracket or statement can get
    /// through, whatever the environment holds.
    #[test]
    fn bot_username_is_whitelisted_before_it_reaches_the_client() {
        let template = "<script>const BOT_USERNAME = /*BOT_USERNAME_INJECT*/null;</script>";

        assert!(inject_bot_username(template, Some("@Some_Bot1"))
            .contains("\"Some_Bot1\""));
        // Anything outside [A-Za-z0-9_] is dropped, including an attempt to end
        // the declaration and start new script.
        let hostile = inject_bot_username(template, Some("bot\";alert(1);//"));
        assert!(
            hostile.contains("const BOT_USERNAME = \"botalert1\";"),
            "hostile value must be reduced to plain characters, got {}",
            hostile
        );
        assert!(!hostile.contains("alert(1);//\""));

        // Whitespace-only and empty are treated as unset.
        assert!(inject_bot_username(template, Some("   ")).contains("null;"));
        assert!(inject_bot_username(template, Some("")).contains("null;"));
    }

    /// serde drops unknown body fields instead of rejecting them, so a body
    /// from an already-open older client still reaches the gate.
    #[test]
    fn claim_request_ignores_unknown_body_fields() {        let parsed: ClaimRequest =
            serde_json::from_str(r#"{"ymid":"legacy-ymid","legacy_attestation":true}"#).unwrap();
        assert_eq!(parsed.ymid, "legacy-ymid");
    }

    /// The refusal the client branches on: "not yet" must stay distinguishable
    /// from a session that can never deliver, or the mini-app keeps waiting on
    /// a dead row instead of telling the user to start a new one.
    #[test]
    fn claim_refusal_names_the_row_state_it_refused() {
        for (status, expected) in [
            ("pending", "pending"),
            ("verified", "verified"),
            ("expired", "expired"),
            ("failed", "failed"),
            ("not_found", "not_found"),
        ] {
            let payload = serde_json::to_string(&claim_refusal(status, CLAIM_PENDING_ERROR)).unwrap();
            assert!(payload.contains("\"success\":false"));
            assert!(
                payload.contains(&format!("\"status\":\"{}\"", expected)),
                "refusal must carry status {}, got {}",
                expected,
                payload
            );
        }

        // The pre-existing answer wording is untouched, and a ymid that does not
        // exist keeps its own message.
        assert!(serde_json::to_string(&claim_refusal("pending", CLAIM_PENDING_ERROR))
            .unwrap()
            .contains(CLAIM_PENDING_ERROR));
        assert!(serde_json::to_string(&claim_refusal("not_found", CLAIM_INVALID_REQUEST_ERROR))
            .unwrap()
            .contains(CLAIM_INVALID_REQUEST_ERROR));
    }

    /// Same contract against a real row: the status the client acts on is read
    /// from the database, not guessed.
    #[tokio::test]
    async fn claim_refusal_reports_the_real_row_state() {
        let (pool, _file) = crate::database::setup_test_db().await;
        let db = Arc::new(pool);
        for (ymid, status) in [
            ("still-open", "pending"),
            ("earned", "verified"),
            ("gone", "expired"),
        ] {
            db.execute_with_timeout(move |conn| {
                conn.execute(
                    "INSERT INTO pending_downloads (id, user_id, video_url, status) VALUES (?1, 7, 'http://v', ?2)",
                    rusqlite::params![ymid, status],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        }

        for (ymid, expected) in [
            ("still-open", "pending"),
            ("earned", "verified"),
            ("gone", "expired"),
        ] {
            let payload = claim_refusal_response(&db, ymid).await;
            assert_eq!(payload["status"], expected);
            assert_eq!(payload["success"], false);
        }

        let unknown = claim_refusal_response(&db, "never-existed").await;
        assert_eq!(unknown["status"], "not_found");
    }

    /// Rotation must never block the funnel: a refusal answers 200 with
    /// `success:false` and hands the client back the ymid it already had, so the
    /// ad can still run under it.
    #[test]
    fn new_session_refusal_hands_the_entry_ymid_back() {
        let refused = new_session_refusal("entry-ymid");
        assert!(!refused.success);
        assert_eq!(refused.ymid, "entry-ymid");
        assert!(!refused.reused);
    }

    /// The success answer is what the client adopts: the new ymid plus whether an
    /// existing row was reused, which is the observable difference between the
    /// cheap path and a mint.
    #[test]
    fn new_session_success_carries_the_ymid_and_the_reuse_flag() {
        let minted = new_session_response(SessionYmid {
            ymid: "fresh".to_string(),
            reused: false,
        });
        let payload = serde_json::to_string(&minted).unwrap();
        assert!(payload.contains("\"success\":true"));
        assert!(payload.contains("\"ymid\":\"fresh\""));
        assert!(payload.contains("\"reused\":false"));

        let reused = new_session_response(SessionYmid {
            ymid: "entry".to_string(),
            reused: true,
        });
        assert!(serde_json::to_string(&reused)
            .unwrap()
            .contains("\"reused\":true"));
    }

    #[test]
    fn classify_verify_separates_moved_already_verified_and_terminal() {        assert_eq!(
            classify_verify(1, Some("verified")),
            VerifyOutcome::Verified
        );
        assert_eq!(
            classify_verify(0, Some("verified")),
            VerifyOutcome::AlreadyVerified
        );
        assert_eq!(
            classify_verify(0, Some("completed")),
            VerifyOutcome::Terminal("completed".to_string())
        );
        assert_eq!(
            classify_verify(0, Some("expired")),
            VerifyOutcome::Terminal("expired".to_string())
        );
        assert_eq!(classify_verify(0, None), VerifyOutcome::Unknown);
    }

    /// The race the wait exists for: the valued postback lands while the
    /// handler is still retrying, so the same request delivers.
    #[tokio::test]
    async fn claim_wait_delivers_value_that_arrives_mid_wait() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();

        let won = claim_with_bounded_wait(3, Duration::ZERO, move || {
            let counter = counter.clone();
            async move {
                if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    Err(anyhow::anyhow!("no valued impression for now"))
                } else {
                    Ok((7, "http://v".to_string(), ClaimVia::Verified))
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(won.2, ClaimVia::Verified);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// The wait is bounded: a request that never gets its postback gives up
    /// after exactly `attempts` tries instead of holding the connection.
    #[tokio::test]
    async fn claim_wait_gives_up_after_the_bounded_attempts() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();

        let result = claim_with_bounded_wait(3, Duration::ZERO, move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err::<(i64, String, ClaimVia), anyhow::Error>(anyhow::anyhow!(
                    "no valued impression"
                ))
            }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn valued_postback_delivers_a_pending_row() {
        let (pool, _file) = crate::database::setup_test_db().await;
        let db = Arc::new(pool);
        crate::database::setup_gate_row(&db, "valued-pending", "pending", 5, &[]).await;

        let outcome = run_valued_delivery(&db, "valued-pending").await;
        assert_eq!(
            outcome,
            ValuedPostbackResult::Delivered {
                user_id: 7,
                url: "http://v".to_string(),
                via: ClaimVia::Verified,
            }
        );
        assert_eq!(
            db.get_pending_download_status("valued-pending")
                .await
                .unwrap(),
            Some("completed".to_string())
        );
    }

    /// Replay probe: Monetag retries any postback that does not answer 200, so
    /// one ymid can carry several valued impressions. The atomic claim must
    /// keep that at exactly one delivery.
    #[tokio::test]
    async fn three_valued_postbacks_deliver_exactly_once() {
        let (pool, _file) = crate::database::setup_test_db().await;
        let db = Arc::new(pool);
        crate::database::setup_gate_row(&db, "retried", "pending", 5, &[]).await;

        let mut deliveries = 0;
        for _ in 0..3 {
            db.log_postback("retried", Some("impression"), "valued", None, None, None)
                .await
                .unwrap();
            if let ValuedPostbackResult::Delivered { .. } =
                run_valued_delivery(&db, "retried").await
            {
                deliveries += 1;
            }
        }

        assert_eq!(deliveries, 1, "three valued postbacks, one delivery");
        assert_eq!(
            db.get_pending_download_status("retried").await.unwrap(),
            Some("completed".to_string())
        );
    }

    /// Stale-state probe: a valued postback for a row that can no longer be
    /// delivered must never spawn a job, whatever the postback claims.
    #[tokio::test]
    async fn valued_postback_never_delivers_for_terminal_or_unknown_ymid() {
        let (pool, _file) = crate::database::setup_test_db().await;
        let db = Arc::new(pool);
        crate::database::setup_gate_row(&db, "already-done", "completed", 5, &[]).await;
        crate::database::setup_gate_row(&db, "too-late", "expired", 5, &[]).await;

        for ymid in ["already-done", "too-late", "never-existed"] {
            db.log_postback(ymid, Some("impression"), "valued", None, None, None)
                .await
                .unwrap();
            let outcome = run_valued_delivery(&db, ymid).await;
            assert!(
                matches!(&outcome, ValuedPostbackResult::NotDeliverable(_)),
                "ymid {} must not deliver, got {:?}",
                ymid,
                outcome
            );
        }

        assert_eq!(
            db.get_pending_download_status("already-done")
                .await
                .unwrap(),
            Some("completed".to_string())
        );
        assert_eq!(
            db.get_pending_download_status("too-late").await.unwrap(),
            Some("expired".to_string())
        );
    }
}
