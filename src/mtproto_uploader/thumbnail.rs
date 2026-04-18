use tokio::process::Command;
use std::path::Path;
use anyhow::anyhow;

use std::path::PathBuf;

pub async fn generate_thumbnail(
    ffmpeg_path: &PathBuf,
    video_path: &Path,
    output_path: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let output = Command::new(ffmpeg_path)
        .arg("-y") // Overwrite output files without asking
        .arg("-ss") // Seek to position
        .arg("0.1") // 0.1 seconds into the video
        .arg("-i")
        .arg(video_path)
        .arg("-vframes")
        .arg("1") // Extract only one frame
        .arg("-vf")
        .arg("scale='min(320,iw)':'min(320,ih)':force_original_aspect_ratio=decrease") // Scale to max 320px while maintaining aspect ratio
        .arg("-q:v")
        .arg("3") // Quality (1-31, 1 is best)
        .arg(output_path)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::error!("ffmpeg thumbnail generation failed: {}", stderr);
        return Err(anyhow!("ffmpeg thumbnail generation failed: {}", stderr).into());
    }

    // Check file size and re-compress if necessary
    let mut thumbnail_size = std::fs::metadata(output_path)?.len();
    let mut quality = 3; // Start with quality 3

    while thumbnail_size > 200 * 1024 && quality < 31 { // Max 200KB
        quality += 2; // Increase quality (lower value means higher quality, so increase to lower quality)
        log::warn!("Thumbnail size {}KB exceeds 200KB, re-compressing with quality {}", thumbnail_size / 1024, quality);

        let output = Command::new(ffmpeg_path)
            .arg("-y")
            .arg("-i")
            .arg(video_path) // Use original video to generate new thumbnail
            .arg("-vframes")
            .arg("1")
            .arg("-vf")
            .arg("scale='min(320,iw)':'min(320,ih)':force_original_aspect_ratio=decrease")
            .arg("-q:v")
            .arg(quality.to_string())
            .arg(output_path)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("ffmpeg thumbnail re-compression failed: {}", stderr);
            return Err(anyhow!("ffmpeg thumbnail re-compression failed: {}", stderr).into());
        }
        thumbnail_size = std::fs::metadata(output_path)?.len();
    }

    if thumbnail_size > 200 * 1024 {
        log::warn!("Thumbnail size {}KB still exceeds 200KB after max compression. Proceeding anyway.", thumbnail_size / 1024);
    }

    Ok(())
}