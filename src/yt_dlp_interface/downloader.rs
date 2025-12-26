use std::path::PathBuf;
use tokio::fs;
use tokio::io;
use zip::ZipArchive;
use anyhow::Result;

#[cfg(target_os = "macos")]
use sevenz_rust2::decompress_file as decompress_7z;

pub async fn download_file(url: &str, path: &PathBuf) -> Result<()> {
    log::info!("Downloading from {} to {:?}", url, path);
    
    let client = reqwest::Client::new();
    let mut response = client.get(url).send().await.map_err(|e| {
        log::error!("Failed to send GET request to {}: {:?}", url, e);
        anyhow::anyhow!("Failed to send GET request to {}: {:?}", url, e)
    })?;
    
    if !response.status().is_success() {
        log::error!("Download failed for {}: HTTP status {}", url, response.status());
        return Err(anyhow::anyhow!("Download failed for {}: HTTP status {}", url, response.status()));
    }

    let mut file = fs::File::create(path).await.map_err(|e| {
        log::error!("Failed to create file {:?}: {:?}", path, e);
        anyhow::anyhow!("Failed to create file {:?}: {:?}", path, e)
    })?;
    
    // Read the response body in chunks and write to the file
    while let Some(chunk) = response.chunk().await.map_err(|e| {
        log::error!("Failed to read chunk from response for {}: {:?}", url, e);
        anyhow::anyhow!("Failed to read chunk from response for {}: {:?}", url, e)
    })? {
        io::copy(&mut chunk.as_ref(), &mut file).await.map_err(|e| {
            log::error!("Failed to write chunk to file {:?}: {:?}", path, e);
            anyhow::anyhow!("Failed to write chunk to file {:?}: {:?}", path, e)
        })?;
    }
    
    log::info!("Download completed successfully to {:?}", path);
    Ok(())
}

pub async fn extract_ffmpeg_windows(zip_path: &PathBuf, extract_to: &PathBuf) -> Result<()> {
    let zip_path = zip_path.clone();
    let extract_to = extract_to.clone();

    tokio::task::spawn_blocking(move || -> Result<()> {
        let file = std::fs::File::open(&zip_path)?;
        let mut archive = ZipArchive::new(file)?;

        let mut ffmpeg_extracted = false;
        let mut ffprobe_extracted = false;

        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            let file_name = PathBuf::from(file.name());

            if file_name.ends_with("ffmpeg.exe") {
                let outpath = extract_to.join("ffmpeg.exe");
                let mut outfile = std::fs::File::create(&outpath)?;
                std::io::copy(&mut file, &mut outfile)?;
                log::info!("Extracted ffmpeg.exe to {:?}", outpath);
                ffmpeg_extracted = true;
            } else if file_name.ends_with("ffprobe.exe") {
                let outpath = extract_to.join("ffprobe.exe");
                let mut outfile = std::fs::File::create(&outpath)?;
                std::io::copy(&mut file, &mut outfile)?;
                log::info!("Extracted ffprobe.exe to {:?}", outpath);
                ffprobe_extracted = true;
            }

            if ffmpeg_extracted && ffprobe_extracted {
                break;
            }
        }
        Ok(())
    }).await??;

    Ok(())
}

#[cfg(target_os = "macos")]
pub async fn extract_ffmpeg_macos(archive_path: &PathBuf, extract_to: &PathBuf) -> Result<()> {
    use tokio::fs;
    fs::create_dir_all(extract_to).await?;

    let archive_path = archive_path.clone();
    let extract_to = extract_to.clone();

    tokio::task::spawn_blocking(move || -> Result<()> {
        let archive_result = decompress_7z(archive_path.as_path(), extract_to.as_path());
        if archive_result.is_err() {
            return Err(anyhow::anyhow!("Failed to extract 7z archive: {:?}", archive_result.err()));
        }

        let mut ffmpeg_found = false;
        let mut ffprobe_found = false;
        
        let mut entries_to_check = vec![extract_to.clone()];
        let ffmpeg_output_path = extract_to.join("ffmpeg");
        let ffprobe_output_path = extract_to.join("ffprobe");

        while let Some(dir_to_check) = entries_to_check.pop() {
            if let Ok(entries) = std::fs::read_dir(&dir_to_check) {
                for entry in entries {
                    if let Ok(entry) = entry {
                        let entry_path = entry.path();
                        
                        if entry_path.is_dir() {
                            entries_to_check.push(entry_path);
                        } else if entry_path.is_file() {
                            let file_name = entry.file_name();
                            
                            if file_name.to_string_lossy().to_lowercase().contains("ffmpeg") 
                                && !file_name.to_string_lossy().to_lowercase().contains("ffprobe") {
                                
                                if !ffmpeg_output_path.exists() || ffmpeg_output_path != entry_path {
                                    std::fs::copy(&entry_path, &ffmpeg_output_path)?;
                                }
                                
                                use std::os::unix::fs::PermissionsExt;
                                let mut perms = std::fs::metadata(&ffmpeg_output_path)?.permissions();
                                perms.set_mode(0o755);
                                std::fs::set_permissions(&ffmpeg_output_path, perms)?;

                                log::info!("Extracted ffmpeg to {:?}", ffmpeg_output_path);
                                ffmpeg_found = true;
                            } else if file_name.to_string_lossy().to_lowercase().contains("ffprobe") {
                                if !ffprobe_output_path.exists() || ffprobe_output_path != entry_path {
                                    std::fs::copy(&entry_path, &ffprobe_output_path)?;
                                }
                                
                                use std::os::unix::fs::PermissionsExt;
                                let mut perms = std::fs::metadata(&ffprobe_output_path)?.permissions();
                                perms.set_mode(0o755);
                                std::fs::set_permissions(&ffprobe_output_path, perms)?;
                                
                                log::info!("Extracted ffprobe to {:?}", ffprobe_output_path);
                                ffprobe_found = true;
                            }
                            
                            if ffmpeg_found && ffprobe_found {
                                break;
                            }
                        }
                    }
                }
            }
        }
        
        if !ffmpeg_found {
            return Err(anyhow::anyhow!("ffmpeg binary not found in 7z archive"));
        }
        if !ffprobe_found {
            return Err(anyhow::anyhow!("ffprobe binary not found in 7z archive"));
        }

        Ok(())
    }).await??;
    
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub async fn extract_ffmpeg_unix(archive_path: &PathBuf, extract_to: &PathBuf) -> Result<()> {
    use tokio::fs;
    fs::create_dir_all(extract_to).await?;

    let archive_path = archive_path.clone();
    let extract_to = extract_to.clone();

    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::fs::File;
        use tar::Archive;
        use xz2::read::XzDecoder;

        let file = File::open(&archive_path)?;
        let decompressed = XzDecoder::new(file);
        let mut archive = Archive::new(decompressed);
        
        let mut ffmpeg_extracted = false;
        let mut ffprobe_extracted = false;
        
        for entry in archive.entries()? {
            let mut entry = entry?;
            let entry_path = entry.path()?;
            
            if entry_path.file_name().map_or(false, |name| name == "ffmpeg") {
                let output_path = extract_to.join("ffmpeg");
                let mut outfile = std::fs::File::create(&output_path)?;
                std::io::copy(&mut entry, &mut outfile)?;
                
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&output_path)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&output_path, perms)?;
                
                log::info!("Extracted ffmpeg to {:?}", output_path);
                ffmpeg_extracted = true;
            } else if entry_path.file_name().map_or(false, |name| name == "ffprobe") {
                let output_path = extract_to.join("ffprobe");
                let mut outfile = std::fs::File::create(&output_path)?;
                std::io::copy(&mut entry, &mut outfile)?;
                
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&output_path)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&output_path, perms)?;
                
                log::info!("Extracted ffprobe to {:?}", output_path);
                ffprobe_extracted = true;
            }
            
            if ffmpeg_extracted && ffprobe_extracted {
                break;
            }
        }
        
        if !ffmpeg_extracted {
            return Err(anyhow::anyhow!("ffmpeg binary not found in archive"));
        }
        if !ffprobe_extracted {
            return Err(anyhow::anyhow!("ffprobe binary not found in archive"));
        }
        
        Ok(())
    }).await??;

    Ok(())
}