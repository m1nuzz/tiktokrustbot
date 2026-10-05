use axum::{
    extract::{State, Query},
    routing::{get, post},
    Json, Router,
    response::Html,
};
use axum::http::{header, HeaderMap, HeaderValue};
use tower_http::cors::CorsLayer;
use std::sync::Arc;
use crate::database::{ClaimVia, DatabasePool};
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
}

#[derive(Deserialize)]
pub struct AdImpressionQuery {
    pub ymid: String,
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
        .fallback(serve_mini_app)
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    log::info!("Starting web server on {}", addr);
    
    let listener = tokio::net::TcpListener::bind(addr).await.expect("Failed to bind web server port");
    axum::serve(listener, app).await.expect("Failed to start axum server");
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
    let html = html.replace(
        "/*STRINGS_INJECT*/",
        &format!("const STRINGS = {};\n        const PAGE_LANG = {};", serde_json::Value::Object(dict), serde_json::Value::String(lang.clone())),
    );
    let html = html.replace("<html lang=\"en\">", &format!("<html lang=\"{}\">", crate::i18n::resolve_lang(Some(&lang))));
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
    // so the HTTP handler must answer 200 OK immediately. The whole valued
    // sequence (verify, readiness wait, claim, spawn) runs in this task, and
    // every outcome is logged — a silent task is as bad as the dropped
    // handler future this replaces.
    tokio::spawn(async move {
        match task_state.db.mark_as_verified(&task_ymid).await {
            Ok(()) => log::info!("Download {} marked as VERIFIED (valued)", task_ymid),
            Err(e) => log::error!(
                "Failed to mark download as verified for ymid {}: {}",
                task_ymid,
                e
            ),
        }

        // Readiness wait loop: the strict gate (valued impression, 15s watch,
        // click or 90s) may still be ahead of this postback, and duplicate
        // postbacks are Monetag retries, so poll until the gate passes.
        const POLL_SECS: u64 = 5;
        const DEADLINE_SECS: u64 = 150;
        let tries = DEADLINE_SECS / POLL_SECS;
        for _ in 0..tries {
            match task_state.db.claim_if_ready(&task_ymid).await {
                Ok((user_id, url, via)) => {
                    // Instant auto-delivery: the user may already browse
                    // another app/site. claim_if_ready is atomic single-use:
                    // if the user already claimed via mini-app, this returns
                    // Err and we skip (no doubles).
                    log::info!(
                        "Auto-delivering download for user {} (valued postback)",
                        user_id
                    );
                    spawn_download_job(task_state, user_id, url, task_ymid, via).await;
                    return;
                }
                Err(e) => {
                    log::info!("Auto-delivery not ready for ymid {}: {}", task_ymid, e);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(POLL_SECS)).await;
        }
        log::warn!(
            "Valued delivery window elapsed for ymid {} without gate pass",
            task_ymid
        );
    });

    axum::http::StatusCode::OK
}

async fn check_ad_status(
    State(state): State<AppState>,
    Query(query): Query<CheckStatusQuery>,
) -> Json<CheckStatusResponse> {
    let db = state.db.clone();
    let ymid = query.ymid.clone();

    match db.get_pending_download_status(&ymid).await {
        Ok(Some(status)) => Json(CheckStatusResponse { status }),
        Ok(None) => Json(CheckStatusResponse { status: "not_found".to_string() }),
        Err(e) => {
            log::error!("Error checking status for ymid {}: {}", ymid, e);
            Json(CheckStatusResponse { status: "error".to_string() })
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

    // 1. Get user_id for this ymid
    let user_id = match db.get_user_id_by_ymid(&ymid).await {
        Ok(id) => id,
        Err(e) => {
            log::error!("Claim failed: Ymid {} not found: {}", ymid, e);
            return Json(json!({ "success": false, "error": "Invalid request ID" }));
        }
    };

    // 2. Check if user is admin
    let admins: Vec<i64> = std::env::var("ADMIN_IDS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    
    let is_admin = admins.contains(&user_id);

    // 3. Attempt to claim: admin bypass, else the strict gate (valued
    // impression plus 15s watch plus click-or-90s). No timer backstop:
    // without a valued postback the download stays locked.
    let claim_result = if is_admin {
        log::info!("Admin detected (user {}), using bypass claim for ymid {}", user_id, ymid);
        db.claim_any_download(&ymid)
            .await
            .map(|(u, url)| (u, url, ClaimVia::Admin))
    } else {
        db.claim_if_ready(&ymid).await
    };

    match claim_result {
        Ok((user_id, url, via)) => {
            log::info!("Claim success ({:?})! Triggering download for user {}: {}", via, user_id, url);
            spawn_download_job(state, user_id, url, ymid, via).await;
            Json(json!({ "success": true }))
        },
        Err(e) => {
            // Idempotent success: a concurrent auto-delivery (valued postback)
            // may have completed this row first, so the video is on its way.
            if let Ok(Some(status)) = db.get_pending_download_status(&ymid).await {
                if status == "completed" {
                    log::info!(
                        "Claim for ymid {} already completed, returning success",
                        ymid
                    );
                    return Json(json!({ "success": true }));
                }
            }
            log::error!("Claim failed for ymid {}: {}", ymid, e);
            Json(json!({ 
                "success": false, 
                "error": "Ad verification not received yet. Please finish watching the ad or wait a few seconds." 
            }))
        }
    }
}
