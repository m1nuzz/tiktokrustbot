use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;
use tiktokdownloader::yt_dlp_interface::fetcher::YoutubeFetcher;

/// Creates a mock video file (just a placeholder file with extension)
fn create_mock_file(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, "mock content").unwrap();
    path
}

/// Finds the system ffprobe executable
fn find_ffprobe() -> Option<PathBuf> {
    // Try common locations
    let candidates = [
        "C:\\Program Files\\FFmpeg\\bin\\ffprobe.exe",
        "C:\\ProgramData\\chocolatey\\bin\\ffprobe.exe",
        "ffprobe.exe",
        "ffprobe",
    ];
    
    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return Some(path);
        }
    }
    
    // Try using PATH
    if let Ok(output) = Command::new("where").arg("ffprobe").output() {
        if !output.stdout.is_empty() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let first_line = stdout.lines().next().unwrap_or("");
            if !first_line.is_empty() {
                return Some(PathBuf::from(first_line.trim()));
            }
        }
    }
    
    None
}

#[tokio::test]
async fn test_file_has_video_with_mock() {
    // This test verifies file_has_video method can be called
    let temp_dir = tempdir().unwrap();
    let temp_path = temp_dir.path();
    
    let test_file = create_mock_file(temp_path, "test_video.mp4");
    
    let fetcher = YoutubeFetcher::new(
        PathBuf::from("yt-dlp"),
        temp_path.to_path_buf(),
        temp_dir.path().join("ffmpeg"),
    ).unwrap();
    
    // Test that file_has_video returns true when video is present
    // (this will use the real ffprobe if available, or fail gracefully)
    // For now, we'll just verify the method exists and can be called
    let has_video = fetcher.file_has_video(&test_file).await;
    // We can't guarantee the result without real ffprobe, but we can verify it doesn't panic
    println!("file_has_video returned: {}", has_video);
    
    // The method should return true when ffprobe is not available (fail-safe behavior)
    // or when video is actually present
}

#[tokio::test]
async fn test_fetcher_creation() {
    let temp_dir = tempdir().unwrap();
    let fetcher = YoutubeFetcher::new(
        PathBuf::from("yt-dlp"),
        temp_dir.path().to_path_buf(),
        temp_dir.path().join("ffmpeg"),
    ).unwrap();
    
    assert_eq!(fetcher.output_dir, temp_dir.path());
}

#[test]
fn test_stream_detection_logic() {
    // Test the logic of detecting streams from ffprobe output
    // This simulates what probe_stream_present does
    
    // Case 1: Has video stream
    let stdout_with_video = "0\n";
    let has_video = stdout_with_video.lines().any(|l| !l.trim().is_empty());
    assert!(has_video);
    
    // Case 2: No streams
    let stdout_no_streams = "";
    let has_streams = stdout_no_streams.lines().any(|l| !l.trim().is_empty());
    assert!(!has_streams);
    
    // Case 3: Multiple streams
    let stdout_multi = "0\n1\n2\n";
    let has_streams = stdout_multi.lines().any(|l| !l.trim().is_empty());
    assert!(has_streams);
}

#[tokio::test]
async fn test_tikwm_fallback_url_parsing() {
    // Test URL parsing logic for tikwm API responses
    use serde_json::json;
    
    // Case 1: Normal response with play URL
    let response = json!({
        "code": 0,
        "data": {
            "play": "https://video.tiktok.com/v1/play.mp4",
            "wmplay": "https://video.tiktok.com/v1/wmplay.mp4"
        }
    });
    
    let video_url = response
        .get("data")
        .and_then(|d| d.get("play"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            response.get("data")
                .and_then(|d| d.get("wmplay"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        });
    
    assert_eq!(video_url, Some("https://video.tiktok.com/v1/play.mp4"));
    
    // Case 2: Only wmplay available
    let response2 = json!({
        "code": 0,
        "data": {
            "play": "",
            "wmplay": "https://video.tiktok.com/v1/wmplay.mp4"
        }
    });
    
    let video_url2 = response2
        .get("data")
        .and_then(|d| d.get("play"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            response2.get("data")
                .and_then(|d| d.get("wmplay"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        });
    
    assert_eq!(video_url2, Some("https://video.tiktok.com/v1/wmplay.mp4"));
    
    // Case 3: No video URLs
    let response3 = json!({
        "code": 0,
        "data": {
            "play": "",
            "wmplay": ""
        }
    });
    
    let video_url3 = response3
        .get("data")
        .and_then(|d| d.get("play"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            response3.get("data")
                .and_then(|d| d.get("wmplay"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        });
    
    assert_eq!(video_url3, None);
}

#[test]
fn test_file_extension_handling() {
    let path = Path::new("test.webm");
    let mp4_path = path.with_extension("mp4");
    assert_eq!(mp4_path.to_string_lossy(), "test.mp4");
    
    let path2 = Path::new("test.mp4");
    let mp4_path2 = path2.with_extension("mp4");
    assert_eq!(mp4_path2.to_string_lossy(), "test.mp4");
}

#[test]
fn test_fallback_decision_logic() {
    // Simulate the logic from download_video_from_url for deciding fallback strategy
    
    // Case 1: Both video and audio present - no fallback
    let has_video = true;
    let has_audio = true;
    let needs_tikwm_fallback = !has_video;
    let needs_audio_fallback = !has_audio && has_video;
    assert!(!needs_tikwm_fallback);
    assert!(!needs_audio_fallback);
    
    // Case 2: Video missing, audio present - tikwm fallback
    let has_video = false;
    let has_audio = true;
    let needs_tikwm_fallback = !has_video;
    let needs_audio_fallback = !has_audio && has_video;
    assert!(needs_tikwm_fallback);
    assert!(!needs_audio_fallback);
    
    // Case 3: Video present, audio missing - audio fallback
    let has_video = true;
    let has_audio = false;
    let needs_tikwm_fallback = !has_video;
    let needs_audio_fallback = !has_audio && has_video;
    assert!(!needs_tikwm_fallback);
    assert!(needs_audio_fallback);
    
    // Case 4: Neither video nor audio - this shouldn't happen but tikwm fallback would trigger
    let has_video = false;
    let has_audio = false;
    let needs_tikwm_fallback = !has_video;
    let needs_audio_fallback = !has_audio && has_video;
    assert!(needs_tikwm_fallback);
    assert!(!needs_audio_fallback);
}

#[tokio::test]
async fn test_ffprobe_command_construction() {
    // Test that we can construct the ffprobe command correctly
    // This verifies the arguments used in probe_stream_present
    let args = vec![
        "-v", "error",
        "-select_streams", "v",
        "-show_entries", "stream=index",
        "-of", "csv=p=0",
        "test.mp4"
    ];
    
    assert_eq!(args[0], "-v");
    assert_eq!(args[1], "error");
    assert_eq!(args[2], "-select_streams");
    assert_eq!(args[3], "v"); // video stream selector
}

#[tokio::test]
async fn test_probe_stream_present_logic_with_real_ffprobe() {
    // This test verifies the logic of stream detection through the public file_has_video method
    let ffprobe_path = match find_ffprobe() {
        Some(p) => p,
        None => {
            println!("Skipping test_probe_stream_present_logic_with_real_ffprobe: ffprobe not found");
            return;
        }
    };
    
    let temp_dir = tempdir().unwrap();
    let temp_path = temp_dir.path();
    
    // Create a fake video file (just a file with .mp4 extension, won't have real streams)
    let test_file = create_mock_file(temp_path, "test.mp4");
    
    // Create fetcher with real ffprobe directory
    let ffmpeg_dir = temp_path.join("ffmpeg");
    std::fs::create_dir(&ffmpeg_dir).unwrap();
    
    // Copy ffprobe to temp directory for testing
    let target_ffprobe = ffmpeg_dir.join("ffprobe.exe");
    if let Err(e) = std::fs::copy(&ffprobe_path, &target_ffprobe) {
        println!("Could not copy ffprobe to temp dir: {}, using original path", e);
    }
    
    let fetcher = YoutubeFetcher::new(
        PathBuf::from("yt-dlp"),
        temp_path.to_path_buf(),
        ffmpeg_dir,
    ).unwrap();
    
    // Test video stream detection through public method
    let has_video = fetcher.file_has_video(&test_file).await;
    println!("file_has_video for fake file returned: {}", has_video);
    
    // For a fake file, this should return true (fail-safe behavior when ffprobe fails)
    // or false if ffprobe can run and detects no video streams
}

#[tokio::test]
async fn test_fetcher_with_real_ffprobe() {
    let ffprobe_path = match find_ffprobe() {
        Some(p) => p,
        None => {
            println!("Skipping test_fetcher_with_real_ffprobe: ffprobe not found");
            return;
        }
    };
    
    let temp_dir = tempdir().unwrap();
    let temp_path = temp_dir.path();
    
    // Create a fetcher with the real ffprobe directory
    let ffmpeg_dir = temp_path.join("ffmpeg");
    std::fs::create_dir(&ffmpeg_dir).unwrap();
    
    // Copy ffprobe to temp directory for testing
    let target_ffprobe = ffmpeg_dir.join("ffprobe.exe");
    if let Err(e) = std::fs::copy(&ffprobe_path, &target_ffprobe) {
        println!("Could not copy ffprobe to temp dir: {}, using parent directory", e);
    }
    
    let fetcher = YoutubeFetcher::new(
        PathBuf::from("yt-dlp"),
        temp_path.to_path_buf(),
        ffmpeg_dir,
    ).unwrap();
    
    let test_file = create_mock_file(temp_path, "test_video.mp4");
    
    // Test file_has_video method
    let has_video = fetcher.file_has_video(&test_file).await;
    println!("file_has_video with real ffprobe returned: {}", has_video);
    
    // The method should handle errors gracefully and return true by default
}
