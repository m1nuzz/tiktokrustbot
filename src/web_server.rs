use axum::{
    extract::{State, Query},
    routing::{get, post},
    Json, Router,
    response::Html,
};
use tower_http::cors::CorsLayer;
use std::sync::Arc;
use crate::database::DatabasePool;
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
}

#[derive(Deserialize)]
pub struct ClaimRequest {
    pub ymid: String,
}

#[derive(Deserialize)]
pub struct CheckStatusQuery {
    pub ymid: String,
}

#[derive(Serialize)]
pub struct CheckStatusResponse {
    pub status: String,
}

pub async fn start_web_server(state: AppState, port: u16) {
    let app = Router::new()
        .route("/api/ads-status", get(get_ads_status))
        .route("/api/monetag-postback", get(monetag_postback))
        .route("/api/check-status", get(check_ad_status))
        .route("/api/claim-video", post(claim_video))
        .fallback(serve_mini_app)
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    log::info!("Starting web server on {}", addr);
    
    let listener = tokio::net::TcpListener::bind(addr).await.expect("Failed to bind web server port");
    axum::serve(listener, app).await.expect("Failed to start axum server");
}

async fn serve_mini_app() -> impl axum::response::IntoResponse {
    // Get SmartLink from environment variable with fallback
    let smartlink = std::env::var("MONETAG_SMARTLINK")
        .unwrap_or_else(|_| "https://omg10.com/4/11148490".to_string());
    
    // Inject SmartLink URL into HTML
    let html = MINI_APP_HTML.replace("// SMARTLINK_INJECT", &format!("const AD_CONFIG_SMARTLINK = \"{}\";", smartlink));
    Html(html)
}

async fn get_ads_status(State(state): State<AppState>) -> Json<serde_json::Value> {
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

async fn monetag_postback(
    State(state): State<AppState>,
    Query(query): Query<PostbackQuery>,
) -> impl axum::response::IntoResponse {
    let event_type = query.reward_event_type.to_lowercase();
    log::info!("Received Monetag postback: ymid={}, type={}", query.ymid, event_type);

    // ✅ ИСПРАВЛЕНО: Принимаем ТОЛЬКО valued (оплачиваемые) события
    if event_type == "valued" {
        let db = state.db.clone();
        let ymid = query.ymid.clone();
        
        // Just mark as verified, do NOT trigger download yet
        if let Err(e) = db.mark_as_verified(&ymid).await {
            log::error!("Failed to mark download as verified for ymid {}: {}", ymid, e);
        } else {
            log::info!("✅ VALUED impression confirmed for ymid: {} (MONEY EARNED!)", ymid);
        }
    } else if event_type == "non_valued" {
        // ⚠️ Monetag НЕ заплатит за этот трафик
        log::warn!(
            "⚠️ NON-VALUED event for ymid: {} (type: {}) - User did NOT generate revenue!",
            query.ymid,
            query.reward_event_type
        );
        // НЕ помечаем как verified — пользователь не получит видео
        // и сможет попробовать ещё раз
    } else {
        log::warn!("Received unknown event_type='{}' for ymid: {}", event_type, query.ymid);
    }

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

    // 3. Attempt to claim
    let claim_result = if is_admin {
        log::info!("Admin detected (user {}), using bypass claim for ymid {}", user_id, ymid);
        db.claim_any_download(&ymid).await
    } else {
        db.claim_verified_download(&ymid).await
    };

    match claim_result {
        Ok((user_id, url)) => {
            log::info!("Claim success! Triggering download for user {}: {}", user_id, url);
            
            // Process in background
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
                    ChatId(user_id)
                ).await {
                    log::error!("Error processing claimed download: {}", e);
                }
            });

            Json(json!({ "success": true }))
        },
        Err(e) => {
            log::error!("Claim failed for ymid {}: {}", ymid, e);
            Json(json!({ 
                "success": false, 
                "error": "Ad verification not received yet. Please finish watching the ad or wait a few seconds." 
            }))
        }
    }
}
