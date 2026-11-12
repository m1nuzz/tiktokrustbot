use anyhow::anyhow;
use std::path::Path;
use tokio::process::Command;

use crate::mtproto_uploader::file_uploader::{
    upload_file_in_parts_with_reconnect, upload_small_file_with_reconnect,
};
use crate::mtproto_uploader::message_sender::send_media_with_retry;
use crate::mtproto_uploader::metadata::get_video_metadata;
use crate::mtproto_uploader::thumbnail::generate_thumbnail;
use crate::mtproto_uploader::uploader::MTProtoUploader;
use crate::utils::progress_bar::ProgressBar;
use crate::utils::temp_file::TempFileGuard;

impl MTProtoUploader {
    async fn ensure_faststart_video(
        &self,
        file_path: &Path,
    ) -> Result<std::path::PathBuf, Box<dyn std::error::Error + Send + Sync>> {
        // Create a temporary file for the faststart-optimized video
        let temp_dir = std::env::temp_dir();
        let file_name = file_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("temp.mp4");
        let temp_path = temp_dir.join(format!("faststart_{}", file_name));

        let output = Command::new(&self.ffmpeg_path)
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
            return Err(anyhow!("ffmpeg faststart remux failed: {}", stderr).into());
        }

        Ok(temp_path)
    }

    pub async fn upload_video(
        &self,
        chat_id: i64,
        username: Option<String>,
        file_path: &Path,
        caption: &str,
        progress_bar: &mut ProgressBar,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Fast path: Bot API (HTTPS multipart) for files within the bot
        // upload limit. Bypasses MTProto entirely, so it is immune to
        // flagged/stalled MTProto sessions (2026-11-09 incident: uploads
        // crawled at KB/s while HTTP API ran at ~9 MB/s). Falls back to
        // the MTProto path on any HTTP error; larger files keep using it.
        const BOT_API_MAX_BYTES: u64 = 45 * 1024 * 1024; // 5 MB margin under the 50 MB API cap
        let file_size = std::fs::metadata(file_path).map(|m| m.len()).unwrap_or(0);
        if file_size > 0 && file_size <= BOT_API_MAX_BYTES {
            log::info!(
                "File is {} bytes (<= {}): using Bot API (HTTPS) upload path",
                file_size,
                BOT_API_MAX_BYTES
            );
            match self
                .upload_video_via_bot_api(chat_id, file_path, caption, progress_bar)
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) => {
                    log::warn!(
                        "Bot API upload failed, falling back to MTProto uploader: {:?}",
                        e
                    );
                }
            }
        }

        // Create temporary faststart file with guard
        let (video_path, _video_guard) = if file_path.extension().map_or(false, |ext| ext == "mp4")
        {
            match self.ensure_faststart_video(file_path).await {
                Ok(temp_path) => {
                    let guard = TempFileGuard::new(temp_path.clone());
                    (temp_path, Some(guard))
                }
                Err(e) => {
                    log::warn!(
                        "Failed to remux video with faststart, proceeding with original: {:?}",
                        e
                    );
                    (file_path.to_path_buf(), None)
                }
            }
        } else {
            (file_path.to_path_buf(), None)
        };

        // Upload the main video file using reconnect mechanism
        let (file_id, file_parts) =
            upload_file_in_parts_with_reconnect(self, &video_path, progress_bar, "video")
                .await
                .map_err(|e| {
                    log::error!("Failed to upload video file {:?}: {:?}", file_path, e);
                    e
                })?;

        // Get video metadata
        let video_metadata =
            get_video_metadata(self.ffprobe_path.to_string_lossy().as_ref(), &video_path)
                .await
                .map_err(|e| {
                    log::error!("Failed to get video metadata for {:?}: {:?}", file_path, e);
                    e
                })?;

        // Generate and upload thumbnail
        let thumbnail_path = file_path.with_extension("jpg");
        generate_thumbnail(&self.ffmpeg_path, file_path, &thumbnail_path)
            .await
            .map_err(|e| {
                log::error!("Failed to generate thumbnail for {:?}: {:?}", file_path, e);
                e
            })?;

        let _thumbnail_guard = TempFileGuard::new(thumbnail_path.clone());

        // Upload the thumbnail using the reconnect mechanism
        let (thumbnail_file_id, thumbnail_parts) =
            upload_small_file_with_reconnect(self, &thumbnail_path)
                .await
                .map_err(|e| {
                    log::error!(
                        "Failed to upload thumbnail file {:?}: {:?}",
                        thumbnail_path,
                        e
                    );
                    e
                })?;

        // Send the media with retry logic
        send_media_with_retry(
            &self.client, // Pass the Arc<Mutex<Client>> directly
            &crate::mtproto_uploader::uploader::session_key(&self.session_file),
            chat_id,
            username,
            file_id,
            file_parts,
            &video_path,
            thumbnail_file_id,
            thumbnail_parts,
            &thumbnail_path,
            video_metadata.duration,
            video_metadata.width,
            video_metadata.height,
            caption,
        )
        .await
        .map_err(|e| {
            log::error!("Failed to send media: {:?}", e);
            e
        })?;

        Ok(())
    }

    /// Send a video via the HTTP Bot API (sendVideo, multipart POST).
    /// Reuses the same faststart remux / metadata / thumbnail pipeline as
    /// the MTProto flow so both paths produce identical messages.
    async fn upload_video_via_bot_api(
        &self,
        chat_id: i64,
        file_path: &Path,
        caption: &str,
        progress_bar: &mut ProgressBar,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Faststart remux so playback starts instantly, same as MTProto flow
        let (video_path, _video_guard) = if file_path.extension().map_or(false, |ext| ext == "mp4")
        {
            match self.ensure_faststart_video(file_path).await {
                Ok(temp_path) => {
                    let guard = TempFileGuard::new(temp_path.clone());
                    (temp_path, Some(guard))
                }
                Err(e) => {
                    log::warn!(
                        "Failed to remux video with faststart, proceeding with original: {:?}",
                        e
                    );
                    (file_path.to_path_buf(), None)
                }
            }
        } else {
            (file_path.to_path_buf(), None)
        };

        let video_metadata =
            get_video_metadata(self.ffprobe_path.to_string_lossy().as_ref(), &video_path)
                .await
                .map_err(|e| {
                    log::error!("Failed to get video metadata for {:?}: {:?}", file_path, e);
                    e
                })?;

        let thumbnail_path = file_path.with_extension("jpg");
        generate_thumbnail(&self.ffmpeg_path, file_path, &thumbnail_path)
            .await
            .map_err(|e| {
                log::error!("Failed to generate thumbnail for {:?}: {:?}", file_path, e);
                e
            })?;
        let _thumbnail_guard = TempFileGuard::new(thumbnail_path.clone());

        let url = format!("https://api.telegram.org/bot{}/sendVideo", self.bot_token);

        let video_part = reqwest::multipart::Part::file(&video_path)
            .await?
            .file_name("video.mp4")
            .mime_str("video/mp4")?;

        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part("video", video_part)
            .text("supports_streaming", "true");

        if !caption.is_empty() {
            form = form.text("caption", caption.to_string());
        }

        if video_metadata.duration > 0.0 {
            form = form.text(
                "duration",
                format!("{}", video_metadata.duration.round() as i64),
            );
        }
        if video_metadata.width > 0 {
            form = form.text("width", format!("{}", video_metadata.width));
        }
        if video_metadata.height > 0 {
            form = form.text("height", format!("{}", video_metadata.height));
        }

        if std::path::Path::new(&thumbnail_path).exists() {
            let thumb_part = reqwest::multipart::Part::file(&thumbnail_path)
                .await?
                .file_name("thumb.jpg")
                .mime_str("image/jpeg")?;
            form = form.part("thumbnail", thumb_part);
        }

        progress_bar.update(90, Some("uploading")).await?;

        // A per-call client is fine here: uploads are a few per minute at
        // most and each is a single request.
        let http = reqwest::Client::new();
        let resp = http.post(&url).multipart(form).send().await?;
        let status = resp.status();
        let body = resp.text().await?;

        if !status.is_success() || !body.contains("\"ok\":true") {
            return Err(anyhow!(
                "Bot API sendVideo failed: http {} body {}",
                status,
                &body.chars().take(300).collect::<String>()
            )
            .into());
        }

        progress_bar.update(100, Some("done")).await?;
        log::info!(
            "Video sent via Bot API ({} bytes)",
            std::fs::metadata(&video_path).map(|m| m.len()).unwrap_or(0)
        );
        Ok(())
    }
}
