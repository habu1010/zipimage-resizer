use std::path::{Path, PathBuf};

/// `path` 以下の通常ファイルを再帰的に列挙する
///
/// 走査中のエラー（読み取り権限がないディレクトリなど）は、そのファイルを
/// 黙って取りこぼさないよう `Err` として返す。
pub fn scan_regular_files<P: AsRef<Path>>(
    path: P,
) -> impl Iterator<Item = std::io::Result<PathBuf>> {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(e) if e.file_type().is_file() => Some(Ok(e.into_path())),
            Ok(_) => None,
            Err(e) => Some(Err(e.into())),
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_scan_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.txt"), b"").unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), b"").unwrap();

        let mut files: Vec<_> = super::scan_regular_files(dir.path())
            .collect::<std::io::Result<_>>()
            .unwrap();
        files.sort();
        assert_eq!(
            files,
            [dir.path().join("a.txt"), dir.path().join("sub/b.txt")]
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_scan_regular_files_reports_error() {
        // 読めないディレクトリがあれば、黙って飛ばさずにエラーを返す
        let dir = tempfile::tempdir().unwrap();
        let Some(_restore) = crate::test_util::unreadable_subdir(dir.path()) else {
            return;
        };
        assert!(super::scan_regular_files(dir.path()).any(|r| r.is_err()));
    }
}
