use std::path::PathBuf;
use tokio::fs;
use anyhow::Result;

pub struct VersionManager {
    storage_dir: PathBuf,
}

impl VersionManager {
    pub fn new(storage_dir: PathBuf) -> Self {
        Self { storage_dir }
    }

    pub async fn get_stored_version(&self, binary_name: &str) -> Result<String> {
        let version_file = self.storage_dir.join(format!("{}.version", binary_name));
        if version_file.exists() {
            Ok(fs::read_to_string(&version_file).await?.trim().to_string())
        } else {
            Ok(String::new())
        }
    }

    pub async fn save_version(&self, binary_name: &str, version: &str) -> Result<()> {
        fs::create_dir_all(&self.storage_dir).await?;
        let version_file = self.storage_dir.join(format!("{}.version", binary_name));
        fs::write(&version_file, version).await?;
        Ok(())
    }
}