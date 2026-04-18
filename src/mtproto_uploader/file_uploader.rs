use grammers_client::Client;
use grammers_tl_types as tl;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use anyhow;
use rand;

use crate::utils::progress_bar::ProgressBar;
use crate::mtproto_uploader::uploader::MTProtoUploader;

pub async fn upload_file_in_parts_with_reconnect(
    mtproto_uploader: &MTProtoUploader,
    file_path: &Path,
    progress_bar: &mut ProgressBar,
    file_type: &str, // "video" or "thumbnail" to customize progress calculation
) -> Result<(i64, i32), Box<dyn std::error::Error + Send + Sync>> {  // Return (file_id, parts_count)
    let file_path = file_path.to_path_buf();
    let file_type = file_type.to_string();
    let progress_bar_clone = progress_bar.clone();
    
    mtproto_uploader.with_reconnect_retry(|| {
        let mtproto_uploader = mtproto_uploader.clone();
        let file_path = file_path.clone();
        let file_type = file_type.clone();
        let mut progress_bar = progress_bar_clone.clone();
        
        Box::pin(async move {
            // Get access to the client
            let client_guard = mtproto_uploader.client.lock().await;
            let result = upload_file_in_parts(&*client_guard, &file_path, &mut progress_bar, &file_type).await;
            drop(client_guard); // Release the lock early
            result
        })
    }).await
}

pub async fn upload_file_in_parts(
    client: &Client,
    file_path: &Path,
    progress_bar: &mut ProgressBar,
    file_type: &str, // "video" or "thumbnail" to customize progress calculation
) -> Result<(i64, i32), Box<dyn std::error::Error + Send + Sync>> {  // Return (file_id, parts_count)
    let file = File::open(file_path)?;
    let mut reader = BufReader::new(file);
    let file_size = file_path.metadata()?.len() as usize;
    
    // Use different part sizes for different file types
    let part_size: usize = if file_type == "thumbnail" {
        128 * 1024  // 128 KB for thumbnails
    } else {
        512 * 1024  // 512 KB for videos
    };
    
    let total_parts = (file_size + part_size - 1) / part_size;

    let file_id: i64 = rand::random();

    // Uploading file in parts
    for part in 0..total_parts {
        let mut buf = vec![0; part_size];
        let bytes_read = reader.read(&mut buf)?;
        buf.truncate(bytes_read);

        let request = tl::functions::upload::SaveBigFilePart {
            file_id,
            file_part: part as i32,
            file_total_parts: total_parts as i32,
            bytes: buf,
        };
        
        let result = client.invoke(&request).await;
        match result {
            Ok(success) => {
                if !success {
                    return Err(anyhow::anyhow!("saveBigFilePart {} returned false", part).into());
                }
            }
            Err(e) => {
                let err_msg = e.to_string();
                if err_msg.contains("ConnectionReset") || err_msg.contains("read 0 bytes") {
                    log::error!("Connection lost during upload at part {}/{}, connection requires reset", part, total_parts);
                    return Err(anyhow::anyhow!(
                        "saveBigFilePart {} failed due to connection loss: {:?}",
                        part,
                        e
                    ).into());
                } else {
                    return Err(anyhow::anyhow!("saveBigFilePart {} failed: {:?}", part, e).into());
                }
            }
        }

        // Calculate progress differently based on file type
        let uploaded = part + 1;
        let overall = if file_type == "video" {
            // For video: 80..=99 range
            80 + ((uploaded as f64 / total_parts as f64) * 19.0).floor() as u8
        } else {
            // For thumbnail: different range if needed, or just update progress generally
            ((uploaded as f64 / total_parts as f64) * 79.0).floor() as u8  // 0..=79 range
        };
        
        // showing "real" upload
        let info = format!("📤 Uploading {}... {}/{} parts", file_type, uploaded, total_parts);
        let _ = progress_bar.update(overall.min(99), Some(&info)).await;
    }

    Ok((file_id, total_parts as i32))
}

// Function specifically for uploading small files (like thumbnails) that don't require multipart upload
pub async fn upload_small_file_with_reconnect(
    mtproto_uploader: &MTProtoUploader,
    file_path: &Path,
) -> Result<(i64, i32), Box<dyn std::error::Error + Send + Sync>> {  // Return (file_id, parts_count)
    let file_path = file_path.to_path_buf();
    
    mtproto_uploader.with_reconnect_retry(|| {
        let mtproto_uploader = mtproto_uploader.clone();
        let file_path = file_path.clone();
        
        Box::pin(async move {
            // Get access to the client
            let client_guard = mtproto_uploader.client.lock().await;
            let result = upload_small_file(&*client_guard, &file_path).await;
            drop(client_guard); // Release the lock early
            result
        })
    }).await
}

pub async fn upload_small_file(
    client: &Client,
    file_path: &Path,
) -> Result<(i64, i32), Box<dyn std::error::Error + Send + Sync>> {  // Return (file_id, parts_count)
    let mut file = File::open(file_path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;

    // For small files like thumbnails, Telegram recommends using just upload::SaveFilePart
    // Check if the file size is small enough for single-part upload (under 512KB)
    if bytes.len() <= 512 * 1024 {
        let file_id: i64 = rand::random();
        
        let request = tl::functions::upload::SaveFilePart {
            file_id,
            file_part: 0,
            bytes,
        };
        
        client.invoke(&request).await.map_err(|e| anyhow::anyhow!("saveFilePart failed: {:?}", e))?;
        
        Ok((file_id, 1)) // Return file_id and 1 part
    } else {
        // If file is larger than 512KB, fall back to multipart upload using reconnection mechanism
        let (file_id, parts_count) = upload_file_in_parts(
            client, 
            file_path, 
            &mut crate::utils::progress_bar::ProgressBar::new_silent(), 
            "thumbnail"
        ).await?;
        Ok((file_id, parts_count))
    }
}