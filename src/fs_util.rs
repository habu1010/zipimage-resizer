use std::path::{Path, PathBuf};

pub fn scan_regular_files<P: AsRef<Path>>(path: P) -> impl Iterator<Item = PathBuf> {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
}
