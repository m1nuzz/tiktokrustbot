use anyhow::Result;
use regex::Regex;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::utils::progress_bar::ProgressBar;
use crate::utils::temp_file::TempFileGuard;

#[derive(Clone)]
pub struct YoutubeFetcher {
    pub yt_dlp_path: PathBuf,
    pub output_dir: PathBuf,
    pub ffmpeg_dir: PathBuf,
}

impl YoutubeFetcher {
    /// Returns the platform-specific ffprobe binary path inside the ffmpeg directory.
    fn ffprobe_path(&self) -> PathBuf {
        self.ffmpeg_dir.join(if cfg!(target_os = "windows") {
            "ffprobe.exe"
        } else {
            "ffprobe"
        })
    }

    /// Returns the platform-specific ffmpeg binary path inside the ffmpeg directory.
    fn ffmpeg_path(&self) -> PathBuf {
        self.ffmpeg_dir.join(if cfg!(target_os = "windows") {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        })
    }
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
        log::info!("Starting download for URL: {} (quality: {})", url, quality);

        // Audio mode: nothing to verify, just download directly using the full 0-80% range.
        if quality == "audio" {
            return self
                .run_yt_dlp(&url, filename_stem, quality, fingerprint.clone(), progress_bar, 0, 80)
                .await;
        }

        // Video modes (h264 / h265 / best): download using 0-60% of the progress bar.
        // This reserves 60-80% for a potential audio-fallback step if the chosen
        // format turns out to be video-only (known yt-dlp/TikTok HEVC bug, #16950).
        let primary_path = self
            .run_yt_dlp(&url, filename_stem, quality, fingerprint.clone(), progress_bar, 0, 60)
            .await?;

        // Check whether the downloaded file actually contains an audio track.
        match file_has_audio(&self.ffprobe_path(), &primary_path).await {
            Ok(true) => {
                log::info!("Downloaded file has audio track, no fallback needed: {:?}", primary_path);
                progress_bar.update(80, Some("⬇️ Download completed")).await?;
                Ok(primary_path)
            }
            Ok(false) => {
                // TikTok served a video-only stream. Recover the audio by downloading an
                // H.264 variant (which reliably has embedded audio) and muxing its audio
                // track into the original (e.g. HEVC) video.
                log::warn!(
                    "Downloaded file is missing audio, attempting H.264 audio fallback: {:?}",
                    primary_path
                );
                self.audio_fallback(&url, filename_stem, fingerprint.clone(), &primary_path, progress_bar)
                    .await
            }
            Err(e) => {
                // If ffprobe itself fails, don't risk an incorrect fallback — just deliver the
                // original file (preserves previous behavior) but warn about it.
                log::warn!(
                    "Could not probe audio for {:?} ({}); delivering file as-is",
                    primary_path,
                    e
                );
                progress_bar.update(80, Some("⬇️ Download completed")).await?;
                Ok(primary_path)
            }
        }
    }

    /// Downloads a fresh H.264 (avc) copy of the video, extracts its audio track, and
    /// muxes that audio into the existing `primary_path` (typically an HEVC stream that
    /// TikTok served without audio). On success returns the muxed file path.
    async fn audio_fallback(
        &self,
        url: &str,
        filename_stem: &str,
        fingerprint: Option<String>,
        primary_path: &Path,
        progress_bar: &mut ProgressBar,
    ) -> Result<PathBuf> {
        // Stage 1 (60..78): fetch the H.264 variant that reliably carries audio.
        progress_bar
            .update(60, Some("🔧 No audio detected — fetching audio track..."))
            .await?;

        let audio_src_stem = format!("{}_audio_src", filename_stem);
        let h264_path = self
            .run_yt_dlp(url, &audio_src_stem, "h264", fingerprint, progress_bar, 60, 78)
            .await?;

        let _h264_guard = TempFileGuard::new(h264_path.clone());

        // Make sure the H.264 source really has an audio track before muxing.
        match file_has_audio(&self.ffprobe_path(), &h264_path).await {
            Ok(true) => {}
            Ok(false) => {
                log::error!(
                    "H.264 fallback also has no audio for {}; cannot recover audio",
                    url
                );
                // Deliver the original file rather than failing the whole request.
                progress_bar.update(80, Some("⬇️ Download completed")).await?;
                return Ok(primary_path.to_path_buf());
            }
            Err(e) => {
                log::warn!("Could not probe fallback audio ({}); muxing anyway", e);
            }
        }

        // Stage 2 (78..80): mux audio from the H.264 file into the original video.
        progress_bar.update(78, Some("🔧 Merging audio track...")).await?;

        let muxed_path = self.output_dir.join(format!("{}_muxed.mp4", filename_stem));
        let mut muxed_guard = TempFileGuard::new(muxed_path.clone());

        let ffmpeg_output = Command::new(&self.ffmpeg_path())
            .arg("-i")
            .arg(primary_path)
            .arg("-i")
            .arg(&h264_path)
            .arg("-map")
            .arg("0:v:0")
            .arg("-map")
            .arg("1:a:0")
            .arg("-c")
            .arg("copy")
            .arg("-shortest")
            .arg("-movflags")
            .arg("+faststart")
            .arg("-y")
            .arg(&muxed_path)
            .output()
            .await?;

        if !ffmpeg_output.status.success() {
            let stderr = String::from_utf8_lossy(&ffmpeg_output.stderr);
            log::error!("Audio mux failed, delivering original file: {}", stderr);
            progress_bar.update(80, Some("⬇️ Download completed")).await?;
            return Ok(primary_path.to_path_buf());
        }

        // Verify the muxed output actually has an audio stream; otherwise fall back.
        match file_has_audio(&self.ffprobe_path(), &muxed_path).await {
            Ok(true) => {
                log::info!("Audio fallback succeeded: {:?}", muxed_path);
                // Replace primary_path contents with the muxed file so downstream code
                // (which operates on the returned path) gets the audio-enabled file.
                if let Err(e) = tokio::fs::rename(&muxed_path, primary_path).await {
                    // rename can fail across volumes; fall back to copy + remove.
                    tokio::fs::copy(&muxed_path, primary_path).await?;
                    muxed_guard.forget();
                    log::debug!("Copied muxed file over primary (rename failed: {})", e);
                } else {
                    muxed_guard.forget();
                }
                progress_bar.update(80, Some("⬇️ Download completed")).await?;
                Ok(primary_path.to_path_buf())
            }
            _ => {
                log::error!("Muxed file has no audio, delivering original file");
                progress_bar.update(80, Some("⬇️ Download completed")).await?;
                Ok(primary_path.to_path_buf())
            }
        }
    }

    /// Runs yt-dlp for the given quality and reports download progress into the slice
    /// of the progress bar defined by [start_pct, end_pct]. Returns the produced file path.
    async fn run_yt_dlp(
        &self,
        url: &str,
        filename_stem: &str,
        quality: &str,
        fingerprint: Option<String>,
        progress_bar: &mut ProgressBar,
        start_pct: u8,
        end_pct: u8,
    ) -> Result<PathBuf> {
        let start_time = std::time::Instant::now();
        let span = (end_pct.saturating_sub(start_pct)) as f64;

        let output_template = if quality == "audio" {
            self.output_dir.join(format!("{}.%(ext)s", filename_stem))
        } else {
            self.output_dir.join(format!("{}.mp4", filename_stem))
        };

        let mut cmd = Command::new(&self.yt_dlp_path);
        cmd.kill_on_drop(true); // Guarantee process termination on drop
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
            cmd.arg(format!("--impersonate={}", fp));
        }

        // Format selection.
        //
        // TikTok serves HD as HEVC (bytevc1) streams that yt-dlp often marks as
        // combined (acodec=aac) but are actually video-only — known issue #16950.
        // Crucially, the classic `-f bestvideo[vcodec^=avc]...` filter does NOT work
        // around this: yt-dlp still picks the HEVC stream. Only the format-sort
        // (`-S`) reliably prefers H.264 (avc). So:
        //   * h264: force avc via -S (correctly returns H.264 with audio)
        //   * h265 / best: allow HEVC; a post-download audio check + mux fallback
        //     (see audio_fallback) recovers missing audio.
        if quality == "h264" {
            cmd.arg("-S").arg("vcodec:avc,mres");
        } else if quality == "audio" {
            cmd.arg("-x").arg("--audio-format").arg("best");
        } else {
            // best / h265 / anything else: pick the best quality available.
            cmd.arg("-f").arg("bestvideo+bestaudio/best");
        }

        cmd.arg(url);

        log::info!("🔍 Full yt-dlp command: {:?}", cmd);

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("stdout not captured");
        let stderr = child.stderr.take().expect("stderr not captured");

        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        let mut last_percentage = 0.0f64;
        let mut last_update_time = std::time::Instant::now();
        const MIN_UPDATE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

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
                                        let overall = scale_to_range(percentage, start_pct, span);
                                        let info = format!("⬇️ Downloading: {:.1}% ({:.1} MB)", percentage, total_size as f64 / 1_048_576.0);
                                        progress_bar.update(overall, Some(&info)).await?;
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
                                        let overall = scale_to_range(percentage, start_pct, span);
                                        let info = format!("⬇️ Downloading: {:.1}% ({:.1} MB)", percentage, total_size as f64 / 1_048_576.0);
                                        progress_bar.update(overall, Some(&info)).await?;
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
            let parent = self.output_dir.clone();
            let stem = PathBuf::from(filename_stem);

            // Look for any file produced by yt-dlp that matches the stem.
            log::debug!("Looking for files in: {:?}", parent);
            if let Ok(entries) = tokio::fs::read_dir(&parent).await {
                let mut entry = entries;
                while let Ok(Some(file)) = entry.next_entry().await {
                    if let Ok(file_type) = file.file_type().await {
                        if file_type.is_file() {
                            if let Some(filename) = file.file_name().to_str() {
                                if filename.starts_with(&*stem.to_string_lossy()) {
                                    let path = parent.join(filename);
                                    log::info!(
                                        "Found downloaded file for {}: {:?}",
                                        filename_stem,
                                        path
                                    );
                                    log::info!(
                                        "Download completed successfully in {:.2?} for: {}",
                                        elapsed,
                                        url
                                    );
                                    return Ok(path);
                                }
                            }
                        }
                    }
                }
            }

            // Fall back to known extensions if no stem match was found.
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

            Err(anyhow::anyhow!("yt-dlp failed: {}", stderr_output.trim()))
        }
    }
}

/// Scales a 0-100 yt-dlp download percentage into the [start_pct, start_pct+span] window.
fn scale_to_range(percentage: f64, start_pct: u8, span: f64) -> u8 {
    let scaled = start_pct as f64 + (percentage / 100.0) * span;
    scaled.round().clamp(0.0, 100.0) as u8
}

/// Uses ffprobe to determine whether the given file contains at least one audio stream.
async fn file_has_audio(ffprobe_path: &Path, file_path: &Path) -> Result<bool> {
    let output = Command::new(ffprobe_path)
        .arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("a")
        .arg("-show_entries")
        .arg("stream=index")
        .arg("-of")
        .arg("csv=p=0")
        .arg(file_path)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "ffprobe failed for {:?}: {}",
            file_path,
            stderr.trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // If an audio stream exists, ffprobe prints at least one line with the stream index.
    Ok(stdout.lines().any(|l| !l.trim().is_empty()))
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
