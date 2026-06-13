use std::path::PathBuf;
use std::fs;
use log;

/// A simple RAII guard that deletes a file when dropped.
/// Useful for ensuring temporary files are cleaned up even if a task is cancelled or errors out.
pub struct TempFileGuard {
    path: Option<PathBuf>,
}

impl TempFileGuard {
    /// Create a new guard for the given path.
    pub fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Explicitly delete the file now.
    pub fn cleanup(&mut self) {
        if let Some(path) = self.path.take() {
            if path.exists() {
                match fs::remove_file(&path) {
                    Ok(_) => log::debug!("Successfully cleaned up temporary file: {:?}", path),
                    Err(e) => log::warn!("Failed to clean up temporary file {:?}: {}", path, e),
                }
            }
        }
    }

    /// Forget about the file, so it won't be deleted when this guard is dropped.
    pub fn forget(&mut self) {
        self.path = None;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        // We clean up even during panics to avoid leaking disk space.
        self.cleanup();
    }
}
