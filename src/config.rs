use std::path::PathBuf;
use anyhow::Result;

pub fn find_dotenv() -> Result<Option<PathBuf>> {
    // 1. Check directory where the executable is located
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(exe_dir) = current_exe.parent() {
            let exe_dir_dotenv = exe_dir.join(".env");
            if exe_dir_dotenv.exists() {
                return Ok(Some(exe_dir_dotenv));
            }
        }
    }

    // 2. Check current working directory (for cargo run compatibility)
    let current_dir = std::env::current_dir()?;
    let current_dotenv = current_dir.join(".env");
    if current_dotenv.exists() {
        return Ok(Some(current_dotenv));
    }

    Ok(None)
}

pub fn load_environment() -> Result<()> {
    match find_dotenv()? {
        Some(path) => {
            dotenv::from_path(&path)?;
            log::info!("Loaded environment variables from {:?}", path);
        },
        None => {
            log::warn!("No .env file found. Using system environment variables.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_dotenv_none_when_not_exists() {
        // This test just ensures that the function doesn't crash when .env doesn't exist
        // The actual path returned depends on the test environment
        let result = find_dotenv();
        // We just test that it doesn't panic
        assert!(result.is_ok());
    }
}