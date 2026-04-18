use std::path::PathBuf;
use std::collections::HashMap;
use tokio::{fs, time::{interval, Duration}};
use anyhow::Result;
use log::{info, warn, error};
use feed_rs::parser;
use crate::auto_update::version_manager::VersionManager;
use crate::yt_dlp_interface::downloader::download_file;

#[derive(Debug, Clone)]
pub struct BinaryConfig {
    pub rss_url: String,
    pub binary_path: PathBuf,
    pub download_url_template: String, // GitHub URL template
}

pub struct AutoUpdater {
    binaries: HashMap<String, BinaryConfig>,
    version_manager: VersionManager,
    check_interval: Duration,
}

impl AutoUpdater {
    pub fn new(libraries_dir: PathBuf, check_interval_minutes: u64) -> Self {
        let mut binaries = HashMap::new();

        // Конфигурация для yt-dlp
        binaries.insert("yt-dlp".to_string(), BinaryConfig {
            rss_url: "https://github.com/yt-dlp/yt-dlp/releases.atom".to_string(),
            binary_path: libraries_dir.join(if cfg!(target_os = "windows") { "yt-dlp.exe" } else { "yt-dlp" }),
            download_url_template: if cfg!(target_os = "windows") {
                "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe".to_string()
            } else if cfg!(target_os = "linux") {
                "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux".to_string()
            } else {
                "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos".to_string()
            },
        });

        // Конфигурация для FFmpeg
        let ffmpeg_dir = libraries_dir.join("ffmpeg");
        binaries.insert("ffmpeg".to_string(), BinaryConfig {
            rss_url: "https://github.com/BtbN/FFmpeg-Builds/releases.atom".to_string(),
            binary_path: ffmpeg_dir.join(if cfg!(target_os = "windows") { "ffmpeg.exe" } else { "ffmpeg" }),
            download_url_template: if cfg!(target_os = "windows") {
                "https://github.com/BtbN/FFmpeg-Builds/releases/latest/download/ffmpeg-master-latest-win64-gpl.zip".to_string()
            } else {
                "https://johnvansickle.com/ffmpeg/builds/ffmpeg-git-amd64-static.tar.xz".to_string()
            },
        });

        Self {
            binaries,
            version_manager: VersionManager::new(libraries_dir.join(".versions")),
            check_interval: Duration::from_secs(check_interval_minutes * 60),
        }
    }

    // Get the latest release ID from the RSS feed
    async fn get_latest_release_id_from_rss(&self, rss_url: &str) -> Result<String> {
        let response = reqwest::get(rss_url).await?;
        let content = response.text().await?;
        let feed = parser::parse(content.as_bytes())?;

        if let Some(entry) = feed.entries.first() {
            // The entry ID is a stable, unique identifier for the release (e.g., a URL)
            Ok(entry.id.clone())
        } else {
            Err(anyhow::anyhow!("No entries found in RSS feed"))
        }
    }

    async fn update_binary(&self, binary_name: &str, config: &BinaryConfig, new_release_id: &str) -> Result<()> {
        info!("Updating {} to new release: {}", binary_name, new_release_id);

        // Form the download URL based on the binary type
        let download_url = if binary_name == "yt-dlp" {
            // For yt-dlp, use the /latest/download/ path which works without version substitution
            config.download_url_template.clone()
        } else {
            // For FFmpeg, parse the RSS feed to find the correct download URL
            // First, get the latest release info from RSS
            let response = reqwest::get(&config.rss_url).await?;
            let content = response.text().await?;
            let feed = parser::parse(content.as_bytes())?;
            
            if let Some(entry) = feed.entries.first() {
                // For FFmpeg, we need to find the correct asset link from the release
                // GitHub RSS feeds contain links to assets in the content or links sections
                // Try to extract the correct download link for the platform
                
                if binary_name == "ffmpeg" {
                    // Extract the correct asset URL for the platform
                    // The link might be in the content or in the links array
                    if cfg!(target_os = "windows") {
                        // For Windows, we need to extract the correct asset URL
                        // GitHub releases might have multiple assets, so we need to find the correct one
                        // Check the entry's links for direct asset download links
                        let mut found_asset_url = None;
                        
                        for link in &entry.links {
                            if link.href.contains("github.com/BtbN/FFmpeg-Builds/releases/download/") && 
                               link.href.contains("win64-gpl") && 
                               link.href.ends_with(".zip") {
                                // Found a Windows GPL zip asset
                                found_asset_url = Some(link.href.clone());
                                break;
                            }
                        }
                        
                        if let Some(asset_url) = found_asset_url {
                            asset_url
                        } else {
                            // Fallback if no direct link found in RSS - try with common pattern
                            // The latest naming seems to follow the pattern like ffmpeg-n7.1-latest-win64-gpl-7.1.zip
                            // Since we can't know the exact version from RSS, try with latest
                            "https://github.com/BtbN/FFmpeg-Builds/releases/latest/download/ffmpeg-n7.1-latest-win64-gpl-7.1.zip".to_string()
                        }
                    } else if cfg!(target_os = "linux") {
                        // For Linux, use the johnvansickle.com static build
                        config.download_url_template.clone()
                    } else {
                        // For macOS, use template
                        config.download_url_template.replace("{}", &new_release_id)
                    }
                } else {
                    // For other binaries, use template with version replacement
                    config.download_url_template.replace("{}", &new_release_id)
                }
            } else {
                // Fallback to template if no RSS entries
                config.download_url_template.replace("{}", &new_release_id)
            }
        };

        if binary_name == "ffmpeg" {
            // FFmpeg requires special handling depending on platform
            if cfg!(target_os = "windows") {
                // For Windows, download the zip file and extract it
                let temp_archive_path = config.binary_path.with_extension("zip");
                download_file(&download_url, &temp_archive_path).await?;
                
                // Extract ffmpeg.exe and ffprobe.exe from the zip file
                let _ffmpeg_dir = config.binary_path.parent().unwrap();
                
                #[cfg(target_os = "windows")]
                {
                    let ffmpeg_dir_pathbuf = config.binary_path.parent().unwrap().to_path_buf();
                    crate::yt_dlp_interface::extract_ffmpeg_windows(&temp_archive_path, &ffmpeg_dir_pathbuf).await?;
                }

                // Clean up the temp archive file
                fs::remove_file(temp_archive_path).await.ok();
            } else if cfg!(target_os = "macos") {
                // For macOS, download the 7z archive and extract it
                let temp_archive_path = config.binary_path.with_extension("7z");
                download_file(&download_url, &temp_archive_path).await?;
                
                #[cfg(target_os = "macos")]
                {
                    let ffmpeg_dir_pathbuf = config.binary_path.parent().unwrap().to_path_buf();
                    crate::yt_dlp_interface::extract_ffmpeg_macos(&temp_archive_path, &ffmpeg_dir_pathbuf).await?;
                }

                // Clean up the temp archive file
                fs::remove_file(temp_archive_path).await.ok();
            } else if cfg!(target_os = "linux") {
                // For Linux, download the tar.xz archive and extract it
                // Using the johnvansickle.com static builds which are already extracted
                let temp_archive_path = config.binary_path.with_extension("tar.xz");
                download_file(&download_url, &temp_archive_path).await?;
                
                #[cfg(all(unix, not(target_os = "macos")))]
                {
                    let ffmpeg_dir_pathbuf = config.binary_path.parent().unwrap().to_path_buf();
                    crate::yt_dlp_interface::extract_ffmpeg_unix(&temp_archive_path, &ffmpeg_dir_pathbuf).await?;
                }

                // Clean up the temp archive file
                fs::remove_file(temp_archive_path).await.ok();
            }
        } else {
            // For yt-dlp, just download the executable
            download_file(&download_url, &config.binary_path).await?;

            // Устанавливаем права выполнения (Unix)
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&config.binary_path).await?.permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&config.binary_path, perms).await?;
            }
        }

        // Сохраняем новую версию
        self.version_manager.save_version(binary_name, new_release_id).await?;
        info!("Successfully updated {} to new release: {}", binary_name, new_release_id);
        Ok(())
    }

    async fn check_single_binary(&self, binary_name: &str, config: &BinaryConfig) -> Result<()> {
        // Get the currently stored release ID
        let current_id = self.version_manager.get_stored_version(binary_name).await.unwrap_or_default();

        // Get the latest release ID from the RSS feed
        match self.get_latest_release_id_from_rss(&config.rss_url).await {
            Ok(latest_id) => {
                if latest_id != current_id && !latest_id.is_empty() {
                    info!("New release found for {}: ID {}",
                        binary_name, latest_id);

                    // Update the binary
                    if let Err(e) = self.update_binary(binary_name, config, &latest_id).await {
                        error!("Failed to update {}: {}", binary_name, e);
                    }
                } else {
                    info!("{} is up to date (Release ID: {})", binary_name, current_id);
                }
            }
            Err(e) => {
                warn!("Failed to check updates for {}: {}", binary_name, e);
            }
        }
        Ok(())
    }

    pub async fn check_for_updates(&self) -> Result<()> {
        info!("Checking for binary updates...");
        for (binary_name, config) in &self.binaries {
            self.check_single_binary(binary_name, config).await?;
        }
        Ok(())
    }

    pub async fn start_periodic_checks(&self) -> Result<()> {
        info!("Starting periodic update checks every {} minutes",
            self.check_interval.as_secs() / 60);
        let mut interval = interval(self.check_interval);
        
        loop {
            interval.tick().await;
            if let Err(e) = self.check_for_updates().await {
                error!("Update check failed: {}", e);
            }
        }
    }
}