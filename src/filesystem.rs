use anyhow::{Context, Result};
use glob::glob;

use std::fs;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};

pub fn find_dir_by_pattern(base_dir: &PathBuf, dir_pattern: &str) -> Option<PathBuf> {
    let pattern = format!(
        "{}{}{}",
        base_dir.to_string_lossy(),
        MAIN_SEPARATOR,
        dir_pattern
    );
    let dirs: Vec<_> = glob(&pattern)
        .expect("Failed to read glob pattern")
        .filter_map(Result::ok)
        .filter(|path| path.is_dir())
        .collect();

    match dirs.len() {
        1 => Some(dirs[0].clone()),
        0 => {
            println!(
                "No directory matching '{}' found in {:?}",
                dir_pattern, base_dir
            );
            None
        }
        _ => {
            println!(
                "Multiple directories matching '{}' found in {:?}",
                dir_pattern, base_dir
            );
            None
        }
    }
}


pub fn find_files(dir: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    find_files_recursive(dir, extension, &mut files)?;
    Ok(files)
}

fn find_files_recursive(dir: &Path, extension: &str, files: &mut Vec<PathBuf>) -> Result<()> {
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                find_files_recursive(&path, extension, files)?;
            } else if path.is_file()
                && path.extension().and_then(|s| s.to_str())
                    == Some(extension.trim_start_matches('.'))
            {
                files.push(path);
            }
        }
    }
    Ok(())
}

pub fn move_files(paths: Vec<PathBuf>, dir: &Path, verbose: bool) -> Result<()> {
    // Move files to 'unmatched' directory
    for path in paths {
        let dest = dir.join(path.file_name().context("Failed to get file destination name")?);
        if verbose {
            println!("{} -> {}", path.display(), dest.display());
        }
        fs::rename(&path, &dest)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::fs;

    #[test]
    fn test_find_dir_by_pattern() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path().to_path_buf();

        fs::create_dir(base_path.join("test_dir_123")).unwrap();
        fs::create_dir(base_path.join("another_dir_456")).unwrap();

        let result = find_dir_by_pattern(&base_path, "test_dir_*");
        assert!(result.is_some());
        assert_eq!(result.unwrap().file_name().unwrap(), "test_dir_123");

        let no_match = find_dir_by_pattern(&base_path, "nonexistent_*");
        assert!(no_match.is_none());

        fs::create_dir(base_path.join("CAMERA_RGB")).unwrap();
        let result = find_dir_by_pattern(&base_path, "C*_RGB");
        assert!(result.is_some());
        assert_eq!(result.unwrap().file_name().unwrap(), "CAMERA_RGB");

        fs::create_dir(base_path.join("Camera_NIR")).unwrap();
        let result = find_dir_by_pattern(&base_path, "C*_NIR");
        assert!(result.is_some());
        assert_eq!(result.unwrap().file_name().unwrap(), "Camera_NIR");
    }

    #[test]
    fn test_find_files() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path();

        fs::write(base_path.join("test1.txt"), "content").unwrap();
        fs::write(base_path.join("test2.txt"), "content").unwrap();
        fs::write(base_path.join("test3.doc"), "content").unwrap();

        let txt_files = find_files(base_path, "txt").unwrap();
        assert_eq!(txt_files.len(), 2);

        let doc_files = find_files(base_path, "doc").unwrap();
        assert_eq!(doc_files.len(), 1);
    }

    #[test]
    fn test_move_files() {
        let temp_dir = TempDir::new().unwrap();
        let source_dir = temp_dir.path().join("source");
        let dest_dir = temp_dir.path().join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();

        let paths = vec![
            source_dir.join("file1.txt"),
            source_dir.join("file2.txt"),
        ];

        // Create test files
        for path in &paths {
            fs::write(path, "content").unwrap();
        }

        move_files(paths, &dest_dir, false).unwrap();

        assert!(!source_dir.join("file1.txt").exists());
        assert!(!source_dir.join("file2.txt").exists());
        assert!(dest_dir.join("file1.txt").exists());
        assert!(dest_dir.join("file2.txt").exists());
    }

    #[test]
    fn test_find_files_recursive() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path();
        let sub_dir = base_path.join("subdir");
        fs::create_dir_all(&sub_dir).unwrap();

        fs::write(base_path.join("test1.txt"), "content").unwrap();
        fs::write(base_path.join("test2.doc"), "content").unwrap();
        fs::write(sub_dir.join("test3.txt"), "content").unwrap();

        let mut files = Vec::new();
        find_files_recursive(base_path, "txt", &mut files).unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f.file_name().unwrap() == "test1.txt"));
        assert!(files.iter().any(|f| f.file_name().unwrap() == "test3.txt"));
    }
}
