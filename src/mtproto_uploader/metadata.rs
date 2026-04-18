use serde_json;
use tokio::process::Command;
use std::path::Path;
use anyhow::anyhow;

use crate::mtproto_uploader::video_metadata::{FFProbeOutput, Stream};

pub async fn get_video_metadata(ffprobe_path: &str, file_path: &Path) -> Result<Stream, Box<dyn std::error::Error + Send + Sync>> {
    let output = Command::new(ffprobe_path)
        .arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-show_entries")
        .arg("stream=width,height,duration:format=duration")
        .arg("-of")
        .arg("json")
        .arg(file_path)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::error!("ffprobe failed: {}", stderr);
        return Err(anyhow!("ffprobe failed: {}", stderr).into());
    }

    let ff: FFProbeOutput = serde_json::from_slice(&output.stdout)?;
    let mut s = ff.streams.into_iter().next().ok_or_else(|| anyhow::anyhow!("No video stream"))?;

    if s.duration <= 0.0 {
        if let Some(fmt) = ff.format {
            s.duration = fmt.duration;
        }
    }
    Ok(s)
}