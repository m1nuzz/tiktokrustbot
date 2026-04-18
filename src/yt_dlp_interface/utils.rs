use std::path::PathBuf;

pub fn is_executable_present(path: &PathBuf) -> bool {
    path.exists() && is_executable(path)
}

pub fn is_executable(path: &PathBuf) -> bool {
    #[cfg(windows)]
    {
        path.extension().map_or(false, |ext| ext == "exe")
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).map_or(false, |metadata| {
            let permissions = metadata.permissions();
            permissions.mode() & 0o111 != 0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::TempDir;

    #[test]
    fn test_is_executable_present() {
        let temp_dir = TempDir::new().unwrap();
        
        // Test with a file that doesn't exist
        let non_existent = temp_dir.path().join("non_existent.exe");
        assert!(!is_executable_present(&non_existent));
        
        // Create a file
        let test_file = temp_dir.path().join("test.exe");
        {
            File::create(&test_file).unwrap();
        }
        
        // For Windows, any file with .exe extension is considered executable
        #[cfg(windows)]
        {
            assert!(is_executable_present(&test_file));
        }
        
        // For Unix systems, we need to test with a properly permissioned file
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&test_file).unwrap().permissions();
            perms.set_mode(0o755);  // Make executable
            std::fs::set_permissions(&test_file, perms).unwrap();
            assert!(is_executable_present(&test_file));
        }
    }
}