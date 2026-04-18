use std::path::Path;
use tokio::fs;
use tokio::process::Command;
use anyhow::anyhow;

use crate::utils::progress_bar::ProgressBar;
use crate::mtproto_uploader::uploader::MTProtoUploader;
use crate::mtproto_uploader::thumbnail::generate_thumbnail;
use crate::mtproto_uploader::metadata::get_video_metadata;
use crate::mtproto_uploader::file_uploader::{upload_file_in_parts_with_reconnect, upload_small_file_with_reconnect};
use crate::mtproto_uploader::message_sender::send_media_with_retry;

impl MTProtoUploader {
    async fn ensure_faststart_video(&self, file_path: &Path) -> Result<std::path::PathBuf, Box<dyn std::error::Error + Send + Sync>> {
        // Create a temporary file for the faststart-optimized video
        let temp_dir = std::env::temp_dir();
        let file_name = file_path.file_name()
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
        // RAII guard for automatic deletion of temporary faststart file
        struct TempVideoGuard {
            path: Option<std::path::PathBuf>,
        }

        impl TempVideoGuard {
            fn new(path: std::path::PathBuf) -> Self {
                Self { path: Some(path) }
            }
        }

        impl Drop for TempVideoGuard {
            fn drop(&mut self) {
                if let Some(path) = &self.path {
                    if std::thread::panicking() {
                        log::warn!("Skipping faststart cleanup during panic: {}", path.display());
                        return;
                    }
                    // Use blocking operation for guaranteed cleanup
                    match std::fs::remove_file(path) {
                        Ok(_) => log::debug!("Cleaned up temporary faststart file: {}", path.display()),
                        Err(e) => log::warn!("Failed to cleanup faststart file {}: {}", path.display(), e),
                    }
                }
            }
        }

        // Create temporary faststart file with guard
        let (video_path, temp_guard) = if file_path.extension().map_or(false, |ext| ext == "mp4") {
            match self.ensure_faststart_video(file_path).await {
                Ok(temp_path) => {
                    let guard = TempVideoGuard::new(temp_path.clone());
                    // Guard will automatically clean up the temp file when function exits
                    // regardless of how it exits (panic or normal return)
                    (temp_path, Some(guard))
                },
                Err(e) => {
                    log::warn!("Failed to remux video with faststart, proceeding with original: {:?}", e);
                    (file_path.to_path_buf(), None) // Use original file
                }
            }
        } else {
            (file_path.to_path_buf(), None) // Use original file
        };

        // Upload the main video file using reconnect mechanism
        let (file_id, file_parts) = upload_file_in_parts_with_reconnect(self, &video_path, progress_bar, "video").await.map_err(|e| {
            log::error!("Failed to upload video file {:?}: {:?}", file_path, e);
            e
        })?;

        // Get video metadata
        let video_metadata = get_video_metadata(self.ffprobe_path.to_string_lossy().as_ref(), &video_path).await.map_err(|e| {
            log::error!("Failed to get video metadata for {:?}: {:?}", file_path, e);
            e
        })?;

        // Generate and upload thumbnail
        let thumbnail_path = file_path.with_extension("jpg");
        generate_thumbnail(&self.ffmpeg_path, file_path, &thumbnail_path).await.map_err(|e| {
            log::error!("Failed to generate thumbnail for {:?}: {:?}", file_path, e);
            e
        })?;

        // Upload the thumbnail using the reconnect mechanism
        let (thumbnail_file_id, thumbnail_parts) = upload_small_file_with_reconnect(self, &thumbnail_path).await.map_err(|e| {
            log::error!("Failed to upload thumbnail file {:?}: {:?}", thumbnail_path, e);
            e
        })?;

        // Send the media with retry logic
        send_media_with_retry(
            &self.client, // Pass the Arc<Mutex<Client>> directly
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
        ).await.map_err(|e| {
            log::error!("Failed to send media: {:?}", e);
            e
        })?;

        // Keep temp_guard in scope so it doesn't get dropped early
        // Guard automatically cleans up the temporary faststart file at function exit
        let _ = temp_guard; // Use the temp_guard to keep it in scope without warning

        // Clean up the thumbnail file
        fs::remove_file(&thumbnail_path).await.map_err(|e| {
            log::warn!("Failed to remove thumbnail file {:?}: {:?}", thumbnail_path, e);
            e
        })?;

        Ok(())
    }
}