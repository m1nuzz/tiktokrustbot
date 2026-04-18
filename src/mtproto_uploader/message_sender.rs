use grammers_client::{Client, InvocationError};
use grammers_tl_types as tl;
use std::{path::Path, sync::Arc};
use anyhow;
use tokio::sync::Mutex;

use crate::peers::resolve_peer;

pub async fn send_media_with_retry(
    client: &Arc<Mutex<Client>>,
    chat_id: i64,
    username: Option<String>,
    file_id: i64,
    file_parts: i32,
    file_path: &Path,
    thumb_id: i64,
    thumb_parts: i32,
    thumbnail_path: &Path,
    duration: f64,
    width: u32,
    height: u32,
    caption: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Get input peer
    let input_peer = resolve_peer(client, chat_id, username.as_deref()).await.map_err(|e| {
        log::error!("Failed to resolve peer for chat_id {}: {:?}", chat_id, e);
        e
    })?;

    // Create input file
    let input_file = tl::enums::InputFile::Big(tl::types::InputFileBig {
        id: file_id,
        parts: file_parts,
        name: file_path
            .file_name()
            .and_then(|os_str| os_str.to_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                log::error!("Failed to extract file name from path: {:?}", file_path);
                anyhow::anyhow!("Failed to extract file name from path")
            })?,
    });

    // Create video attributes
    let video_attr = tl::enums::DocumentAttribute::Video(tl::types::DocumentAttributeVideo {
        round_message: false,
        supports_streaming: true,
        nosound: false,
        duration,
        w: width as i32,
        h: height as i32,
        preload_prefix_size: None,
        video_start_ts: None,
        video_codec: None,
    });

    // Create input thumbnail - use InputFile::File for single-part files, InputFile::Big for multi-part
    let input_thumb = if thumb_parts == 1 {
        tl::enums::InputFile::File(tl::types::InputFile {
            id: thumb_id,
            parts: 1,
            name: thumbnail_path
                .file_name()
                .and_then(|os_str| os_str.to_str())
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    log::error!("Failed to extract thumbnail file name from path: {:?}", thumbnail_path);
                    anyhow::anyhow!("Failed to extract thumbnail file name from path")
                })?,
            md5_checksum: String::new(), // Empty for proper files
        })
    } else {
        tl::enums::InputFile::Big(tl::types::InputFileBig {
            id: thumb_id,
            parts: thumb_parts,
            name: thumbnail_path
                .file_name()
                .and_then(|os_str| os_str.to_str())
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    log::error!("Failed to extract thumbnail file name from path: {:?}", thumbnail_path);
                    anyhow::anyhow!("Failed to extract thumbnail file name from path")
                })?,
        })
    };

    // Create media object
    let media = tl::enums::InputMedia::UploadedDocument(tl::types::InputMediaUploadedDocument {
        nosound_video: false,
        spoiler: false,
        file: input_file,
        thumb: Some(input_thumb), // Pass the uploaded thumbnail
        mime_type: "video/mp4".to_string(),
        force_file: false,
        attributes: vec![video_attr],
        stickers: None,
        ttl_seconds: None,
        video_cover: None,
        video_timestamp: None,
    });

    // Sending message with retry logic
    let mut attempts = 0;
    loop {
        attempts += 1;
        let random_id: i64 = rand::random();
        match {
            let actual_client = client.lock().await;
            actual_client.invoke(&tl::functions::messages::SendMedia {
                silent: false,
                background: false,
                clear_draft: false,
                noforwards: false,
                update_stickersets_order: false,
                allow_paid_floodskip: false,
                allow_paid_stars: None,
                schedule_repeat_period: None,
                effect: None,
                suggested_post: None,
                peer: input_peer.clone(), // Clone input_peer for retries
                reply_to: None,
                media: media.clone(), // Clone media for retries
                message: caption.to_string(),
                random_id,
                reply_markup: None,
                entities: Some(Vec::new()),
                schedule_date: None,
                send_as: None,
                invert_media: false,
                quick_reply_shortcut: None,
            }).await
        } {
            Ok(_) => break,
            Err(InvocationError::Rpc(e)) if e.name.starts_with("FLOOD_WAIT_") => {
                let secs = e.code as u64;
                eprintln!("FLOOD_WAIT_X: Waiting for {} seconds", secs);
                tokio::time::sleep(std::time::Duration::from_secs(secs.min(30))).await;
            },
            Err(e) if attempts < 3 => {
                eprintln!("Attempt {} failed: {:?}. Retrying...", attempts, e);
                tokio::time::sleep(std::time::Duration::from_millis(500 * attempts as u64)).await;
            },
            Err(e) => {
                match e {
                    InvocationError::Rpc(ref rpc_err) => {
                        log::error!("sendMedia failed after {} attempts with RPC Error: code={}, name={}, value={:?}", attempts, rpc_err.code, rpc_err.name, rpc_err.value);
                    },
                    _ => {
                        log::error!("sendMedia failed after {} attempts with InvocationError: {:?}", attempts, e);
                    }
                }
                return Err(anyhow::anyhow!("sendMedia failed after {} attempts: {:?}", attempts, e).into());
            }
        }
    }

    Ok(())
}