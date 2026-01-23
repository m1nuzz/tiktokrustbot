use anyhow::Result;
use regex::Regex;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::utils::progress_bar::ProgressBar;

#[derive(Clone)]
pub struct YoutubeFetcher {
    pub yt_dlp_path: PathBuf,
    pub output_dir: PathBuf,
    pub ffmpeg_dir: PathBuf,
}

impl YoutubeFetcher {
    pub fn new(yt_dlp_path: PathBuf, output_dir: PathBuf, ffmpeg_dir: PathBuf) -> Result<Self> {
        Ok(YoutubeFetcher {
            yt_dlp_path,
            output_dir,
            ffmpeg_dir,
        })
    }

    pub async fn download_video_from_url(
        &self,
        url: String,
        filename_stem: &str,
        quality: &str,
        fingerprint: Option<String>,
        progress_bar: &mut ProgressBar,
    ) -> Result<std::path::PathBuf> {
        log::info!("Starting download for URL: {}", url);
        let start_time = std::time::Instant::now();

        let output_template = if quality == "audio" {
            self.output_dir.join(format!("{}.%(ext)s", filename_stem))
        } else {
            self.output_dir.join(format!("{}.mp4", filename_stem))
        };

        let mut cmd = Command::new(&self.yt_dlp_path);
        cmd.arg("--extractor-args")
            .arg("tiktok:skip=feed")
            .arg("--output")
            .arg(&output_template)
            .arg("--no-part")
            .arg("--no-mtime")
            .arg("--ffmpeg-location")
            .arg(&self.ffmpeg_dir)
            .arg("--progress")
            .arg("--newline")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(fp) = fingerprint {
            log::info!("🔐 Applying TLS fingerprint: {}", fp);
            // Use the combined format --impersonate=VALUE
            cmd.arg(format!("--impersonate={}", fp));
        }

        // Add quality arguments
        if quality == "h264" {
            cmd.arg("-f")
                .arg("bestvideo[vcodec^=avc]+bestaudio/best[vcodec^=avc]/best");
        } else if quality == "audio" {
            cmd.arg("-x").arg("--audio-format").arg("best");
        } else {
            cmd.arg("-f").arg("bestvideo+bestaudio/best");
        }

        cmd.arg(&url);

        // Add command logging for diagnostics
        log::info!("🔍 Full yt-dlp command: {:?}", cmd);

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("stdout not captured");
        let stderr = child.stderr.take().expect("stderr not captured");

        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        let mut last_percentage = 0.0f64;
        let mut last_update_time = std::time::Instant::now();
        const MIN_UPDATE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500); // Minimum 500ms between updates

        loop {
            tokio::select! {
                line = stdout_reader.next_line() => {
                    match line {
                        Ok(Some(line)) => {
                            log::trace!("yt-dlp stdout: {}", line);
                            if let Some((percentage, total_size)) = parse_progress_line(&line) {
                                if percentage > last_percentage {
                                    let now = std::time::Instant::now();
                                    if now.duration_since(last_update_time) >= MIN_UPDATE_INTERVAL {
                                        last_percentage = percentage;
                                        last_update_time = now;
                                        // KEY CHANGE: scale 0-100% yt-dlp to 0-80% of overall progress
                                        let overall_percentage = (percentage * 0.8) as u8; // 0-80%
                                        let info = format!("⬇️ Downloading: {:.1}% ({:.1} MB)", percentage, total_size as f64 / 1_048_576.0);
                                        progress_bar.update(overall_percentage, Some(&info)).await?;
                                    }
                                }
                            }
                        },
                        Ok(None) => break,
                        Err(_) => break,
                    }
                },
                line = stderr_reader.next_line() => {
                    match line {
                        Ok(Some(line)) => {
                            log::trace!("yt-dlp stderr: {}", line);
                            if let Some((percentage, total_size)) = parse_progress_line(&line) {
                                if percentage > last_percentage {
                                    let now = std::time::Instant::now();
                                    if now.duration_since(last_update_time) >= MIN_UPDATE_INTERVAL {
                                        last_percentage = percentage;
                                        last_update_time = now;
                                        let current_size = (total_size as f64 * (percentage / 100.0)) as u64;
                                        let overall_percentage = ((current_size as f64 / total_size as f64 * 80.0).min(80.0).max(0.0)) as u8;
                                        let info = format!("⬇️ Downloading: {:.1}% ({:.1} MB)", percentage, total_size as f64 / 1_048_576.0);
                                        progress_bar.update(overall_percentage, Some(&info)).await?;
                                    }
                                }
                            }
                        },
                        Ok(None) => {},
                        Err(_) => {},
                    }
                }
            }
        }

        let output = child.wait_with_output().await?;
        let elapsed = start_time.elapsed();

        log::debug!(
            "yt-dlp process finished with status: {:?}, stdout len: {}, stderr len: {}",
            output.status,
            output.stdout.len(),
            output.stderr.len()
        );

        if output.status.success() {
            // After download completion, show 80%
            progress_bar
                .update(80, Some("⬇️ Download completed"))
                .await?;

            let parent = self.output_dir.clone();
            let stem = PathBuf::from(filename_stem);

            // Log the contents of the output directory to debug what files were actually created
            log::debug!("Looking for files in: {:?}", parent);
            if let Ok(entries) = tokio::fs::read_dir(&parent).await {
                let mut entry = entries;
                while let Ok(Some(file)) = entry.next_entry().await {
                    if let Ok(file_type) = file.file_type().await {
                        if file_type.is_file() {
                            if let Some(filename) = file.file_name().to_str() {
                                if filename.starts_with(stem.to_string_lossy().as_ref()) {
                                    let path = parent.join(filename);
                                    log::info!("Found unexpected file for download: {:?}", path);
                                    return Ok(path);
                                }
                            }
                        }
                    }
                }
            }

            for ext in [
                ".mp4", ".mov", ".webm", ".mkv", ".flv", ".m4a", ".mp3", ".ogg", ".aac",
            ] {
                let alt_path = parent.join(format!("{}{}", stem.to_string_lossy(), ext));
                if alt_path.exists() {
                    log::info!(
                        "Download completed successfully in {:.2?} for: {} with file: {:?}",
                        elapsed,
                        url,
                        alt_path
                    );
                    return Ok(alt_path);
                }
            }

            // If we can't find with expected extensions, try to find any file that starts with the stem
            if let Ok(entries) = tokio::fs::read_dir(&parent).await {
                let mut entry = entries;
                while let Ok(Some(file)) = entry.next_entry().await {
                    if let Ok(file_type) = file.file_type().await {
                        if file_type.is_file() {
                            if let Some(filename) = file.file_name().to_str() {
                                if filename.starts_with(stem.to_string_lossy().as_ref()) {
                                    let path = parent.join(filename);
                                    log::info!("Found unexpected file for download: {:?}", path);
                                    return Ok(path);
                                }
                            }
                        }
                    }
                }
            }

            log::error!(
                "Downloaded file not found after successful yt-dlp execution for: {}",
                url
            );
            Err(anyhow::anyhow!("Downloaded file not found"))
        } else {
            let stderr_output = String::from_utf8_lossy(&output.stderr);
            let stdout_output = String::from_utf8_lossy(&output.stdout);

            log::error!(
                "yt-dlp failed with status {:?} for URL: {}",
                output.status,
                url
            );
            log::error!("yt-dlp stderr: {}", stderr_output);
            log::error!("yt-dlp stdout: {}", stdout_output);

            // Log the command that was executed for debugging
            log::debug!("yt-dlp command for quality '{}': url: {}", quality, url);

            // Return more informative error
            Err(anyhow::anyhow!("yt-dlp failed: {}", stderr_output.trim()))
        }
    }
}

fn parse_progress_line(line: &str) -> Option<(f64, u64)> {
    let clean_line = remove_ansi_codes(line);
    let patterns = [
        r"\[download\]\s+(\d+\.?\d*)%\s+of\s+(\d+\.?\d*[KMGT]?i?B)",
        r"\[download\]\s+(\d+\.?\d*)%\s+of\s+~(\d+\.?\d*[KMGT]?i?B)",
        r"(\d+\.?\d*)%",
    ];

    for pattern in patterns {
        if let Ok(re) = Regex::new(pattern) {
            if let Some(caps) = re.captures(&clean_line) {
                if let Ok(percentage) = caps[1].parse::<f64>() {
                    let total_size = if caps.len() > 2 {
                        parse_size_string(&caps[2])
                    } else {
                        10_485_760
                    };
                    return Some((percentage, total_size));
                }
            }
        }
    }
    None
}

fn remove_ansi_codes(text: &str) -> String {
    let re = Regex::new(r"\x1B\[[0-?]*[ -/]*[@-~]").unwrap();
    re.replace_all(text, "").to_string()
}

fn parse_size_string(s: &str) -> u64 {
    let s_clean = s.trim().to_lowercase();
    let (number_str, multiplier) = if s_clean.ends_with("mib") {
        (s_clean.trim_end_matches("mib"), 1_024 * 1_024) // Mebibyte (1024^2)
    } else if s_clean.ends_with("mb") {
        (s_clean.trim_end_matches("mb"), 1_000 * 1_000) // Megabyte (1000^2)
    } else if s_clean.ends_with("gib") {
        (s_clean.trim_end_matches("gib"), 1_024 * 1_024 * 1_024) // Gibibyte (1024^3)
    } else if s_clean.ends_with("gb") {
        (s_clean.trim_end_matches("gb"), 1_000 * 1_000 * 1_000) // Gigabyte (1000^3)
    } else {
        (s_clean.trim_end_matches("b"), 1) // For plain bytes
    };
    number_str.parse::<f64>().unwrap_or(1.0) as u64 * multiplier
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_size_string_mb() {
        // MB is 1000^2, not 1024^2 (MiB)
        assert_eq!(parse_size_string("10.0MB"), 10_000_000); // 10.0 * 1000^2
        assert_eq!(parse_size_string("5.0MB"), 5_000_000); // 5.0 * 1000^2
    }

    #[test]
    fn test_parse_size_string_gb() {
        assert_eq!(parse_size_string("1.0GB"), 1_000_000_000); // 1.0 * 1000^3
    }

    #[test]
    fn test_remove_ansi_codes() {
        let input = "\x1B[31mRed text\x1B[0m";
        let result = remove_ansi_codes(input);
        assert_eq!(result, "Red text");
    }

    #[test]
    fn test_parse_progress_line() {
        let line = "[download]  50.0% of 10.00MiB";
        let result = parse_progress_line(line);
        assert!(result.is_some());
        let (percentage, total_size) = result.unwrap();
        assert_eq!(percentage, 50.0);
        assert_eq!(total_size, 10_485_760); // 10 MiB
    }
}
