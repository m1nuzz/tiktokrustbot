use reqwest::multipart::{Form, Part};
use tokio::fs::File;
use teloxide::types::ChatId;
use crate::utils::progress_bar::ProgressBar;
use crate::utils::progress_reader::ProgressReader;
use tokio_util::io::ReaderStream;
use tokio::process::Command;
use std::path::Path;
use std::path::PathBuf;
use anyhow;
use log;

async fn ensure_faststart_video(ffmpeg_path: &PathBuf, file_path: &Path) -> Result<std::path::PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    // Create a temporary file for the faststart-optimized video
    let temp_dir = std::env::temp_dir();
    let file_name = file_path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("temp.mp4");
    let temp_path = temp_dir.join(format!("faststart_{}", file_name));

    let output = Command::new(ffmpeg_path)
        .arg("-i")
        .arg(file_path)
        .arg("-c")
        .arg("copy")
        .arg("-movflags")
        .arg("+faststart")
        .arg(&temp_path)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::error!("ffmpeg faststart remux failed: {}", stderr);
        return Err(anyhow::anyhow!("ffmpeg faststart remux failed: {}", stderr).into());
    }

    Ok(temp_path)
}

async fn get_video_metadata(ffprobe_path: &str, file_path: &Path) -> Result<crate::mtproto_uploader::video_metadata::Stream, Box<dyn std::error::Error + Send + Sync>> {
    // Reuse the existing function from mtproto_uploader
    crate::mtproto_uploader::metadata::get_video_metadata(ffprobe_path, file_path).await.map_err(|e| e.into())
}

pub async fn send_video_with_progress_botapi(
    bot_token: &str,
    chat_id: ChatId,
    file_path: &std::path::Path,
    caption: Option<&str>,
    progress_bar: &mut ProgressBar,
) -> anyhow::Result<()> {
    // Get paths for ffmpeg and ffprobe (using the same approach as in main.rs)
    let libraries_dir = std::env::current_dir()? // Consider making this configurable or user-specific
        .join("lib");
    let ffmpeg_dir = libraries_dir.join("ffmpeg");
    let ffmpeg_path = ffmpeg_dir.join(if cfg!(target_os = "windows") { "ffmpeg.exe" } else { "ffmpeg" });
    let ffprobe_path = ffmpeg_dir.join(if cfg!(target_os = "windows") { "ffprobe.exe" } else { "ffprobe" });
    let ffprobe_path_str = ffprobe_path.to_string_lossy();

    // First, remux with faststart
    let (video_path, needs_cleanup) = if file_path.extension().map_or(false, |ext| ext == "mp4") {
        match ensure_faststart_video(&ffmpeg_path, file_path).await {
            Ok(temp_path) => (temp_path, true), // Use processed video and mark for cleanup
            Err(e) => {
                log::warn!("Failed to remux video with faststart for Bot API, proceeding with original: {:?}", e);
                (file_path.to_path_buf(), false) // Use original file and no cleanup needed
            }
        }
    } else {
        (file_path.to_path_buf(), false) // Use original file and no cleanup needed
    };

    // Get video metadata
    let meta = get_video_metadata(&ffprobe_path_str, &video_path).await.map_err(|e| {
        log::warn!("Failed to get video metadata, proceeding without: {:?}", e);
        e
    }).unwrap_or_else(|_| crate::mtproto_uploader::video_metadata::Stream {
        width: 0,
        height: 0,
        duration: 0.0,
    });

    // Generate thumbnail
    let thumbnail_path = video_path.with_extension("jpg");
    let thumbnail_result = crate::mtproto_uploader::thumbnail::generate_thumbnail(&ffmpeg_path, &video_path, &thumbnail_path).await;
    
    let file = File::open(&video_path).await?;
    let len = file.metadata().await?.len();

    // 80..=100% - actual Bot API upload
    let pb_clone = progress_bar.clone();
    let reader = ProgressReader::new(file, len, move |uploaded, total| {
        let overall = 80.0 + (uploaded as f64 / total as f64) * 20.0;
        // Without await inside callback: move to task
        let mut pb2 = pb_clone.clone();
        let text = format!("📤 Uploading... {:.1}/{:.1} MB",
            uploaded as f64 / 1_048_576.0,
            total as f64 / 1_048_576.0);
        tokio::spawn(async move {
            let _ = pb2.update(overall.min(100.0) as u8, Some(&text)).await;
        });
    });

    let stream_reader = ReaderStream::new(reader);

    let part = Part::stream_with_length(reqwest::Body::wrap_stream(stream_reader), len)
        .file_name(video_path.file_name().unwrap().to_string_lossy().to_string())
        .mime_str("video/mp4")?;

    let mut form = Form::new()
        .text("chat_id", chat_id.0.to_string())
        .part("video", part)
        .text("supports_streaming", "true");

    // Add width and height if available
    if meta.width > 0 {
        form = form.text("width", meta.width.to_string());
    }
    if meta.height > 0 {
        form = form.text("height", meta.height.to_string());
    }
    if meta.duration > 0.0 {
        form = form.text("duration", meta.duration.floor().to_string());
    }

    // Add thumbnail if successfully generated
    if thumbnail_result.is_ok() {
        if let Ok(thumb_part) = Part::file(&thumbnail_path).await.map(|p| p.mime_str("image/jpeg").unwrap()) {
            form = form.part("thumbnail", thumb_part);
        }
    }

    let form = if let Some(c) = caption {
        form.text("caption", c.to_string())
    } else { form };

    let url = format!("https://api.telegram.org/bot{}/sendVideo", bot_token);
    let client = reqwest::Client::new();
    let resp = client.post(&url).multipart(form).send().await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("Bot API sendVideo failed: {}", resp.status()));
    }

    // Success: hide progress bar immediately
    progress_bar.delete().await?;
    
    // Clean up temporary files
    if needs_cleanup {
        tokio::fs::remove_file(&video_path).await?;
    }
    
    if thumbnail_result.is_ok() {
        tokio::fs::remove_file(&thumbnail_path).await?;
    }
    
    Ok(())
}

pub async fn send_audio_with_progress_botapi(
    bot_token: &str,
    chat_id: ChatId,
    file_path: &std::path::Path,
    caption: Option<&str>,
    progress_bar: &mut ProgressBar,
) -> anyhow::Result<()> {
    use reqwest::multipart::{Form, Part};
    use tokio_util::io::ReaderStream;
    use crate::utils::progress_reader::ProgressReader;
    use tokio::fs::File;

    let file = File::open(file_path).await?;
    let len = file.metadata().await?.len();

    let pb_clone = progress_bar.clone();
    // Keep track of the last update time to implement throttling
    use std::sync::Arc;
    use tokio::sync::Mutex;
    let last_update_time = Arc::new(Mutex::new(std::time::Instant::now()));
    let last_update_time_clone = last_update_time.clone();
    
    let reader = ProgressReader::new(file, len, move |uploaded, total| {
        let overall = 80.0 + (uploaded as f64 / total as f64) * 20.0;
        let mut pb2 = pb_clone.clone();
        let text = format!("📤 Uploading... {:.1}/{:.1} MB",
            uploaded as f64 / 1_048_576.0,
            total as f64 / 1_048_576.0);
        let last_update_time = last_update_time_clone.clone();
        
        tokio::spawn(async move {
            // Implement throttling: minimum 1.5 seconds between updates
            let min_update_interval = std::time::Duration::from_millis(1500);
            let now = std::time::Instant::now();
            
            let should_update = {
                let mut last_time = last_update_time.lock().await;
                if now.duration_since(*last_time) >= min_update_interval {
                    *last_time = now;
                    true
                } else {
                    false
                }
            };
            
            if should_update || overall >= 100.0 {
                let _ = pb2.update(overall.min(100.0) as u8, Some(&text)).await;
            }
        });
    });

    let stream_reader = ReaderStream::new(reader);

    let ext = file_path.extension().and_then(|s| s.to_str()).unwrap_or_default().to_lowercase();
    let mime = match ext.as_str() {
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" => "audio/ogg",
        _ => "audio/mpeg",
    };

    let part = Part::stream_with_length(reqwest::Body::wrap_stream(stream_reader), len)
        .file_name(file_path.file_name().unwrap().to_string_lossy().to_string())
        .mime_str(mime)?;

    let mut form = Form::new()
        .text("chat_id", chat_id.0.to_string())
        .part("audio", part);

    if let Some(c) = caption {
        form = form.text("caption", c.to_string());
    }

    let url = format!("https://api.telegram.org/bot{}/sendAudio", bot_token);
    let client = reqwest::Client::new();
    let resp = client.post(&url).multipart(form).send().await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("Bot API sendAudio failed: {}", resp.status()));
    }

    progress_bar.delete().await?;
    Ok(())
}

