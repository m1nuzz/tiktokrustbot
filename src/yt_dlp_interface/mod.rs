pub mod fetcher;
pub mod utils;
pub mod urls;
pub mod downloader;
pub mod ensure;

pub use fetcher::YoutubeFetcher;
pub use utils::is_executable_present;
pub use ensure::ensure_binaries;

// The download_file function is used by the auto_update module
// We'll keep it available and suppress the unused warning when appropriate
#[allow(unused_imports)]
pub use downloader::download_file;

#[cfg(target_os = "windows")]
pub use downloader::extract_ffmpeg_windows;

#[cfg(target_os = "macos")]
pub use downloader::extract_ffmpeg_macos;

#[cfg(all(unix, not(target_os = "macos")))]
pub use downloader::extract_ffmpeg_unix;
