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
pub mod tests {
    use std::path::Path;

    /// `dir` に読み取り権限のないサブディレクトリを作る
    ///
    /// root で実行しているなどで権限を無視して読めてしまう場合は `None` を返す
    /// （呼び出し側はテストをスキップする）。戻り値を drop すると権限を戻す。
    #[cfg(unix)]
    pub fn unreadable_subdir(dir: &Path) -> Option<impl Drop> {
        use std::os::unix::fs::PermissionsExt;

        struct Restore(std::path::PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                // TempDir が削除できるよう権限を戻す
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
        }

        let sub = dir.join("sub");
        std::fs::create_dir(&sub).unwrap();
        // 途中で panic しても権限が戻るよう、権限を変える前にガードを作る
        let restore = Restore(sub.clone());
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::read_dir(&sub).is_err().then_some(restore)
    }

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
        let Some(_restore) = unreadable_subdir(dir.path()) else {
            return;
        };
        assert!(super::scan_regular_files(dir.path()).any(|r| r.is_err()));
    }
}
