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

/// 親ディレクトリを返す（`a.zip` のような相対パスのファイル名だけの場合は `.`）
pub fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// エラー表示用に、`path` を `base_dir`（展開先の一時ディレクトリ）からの相対パスにする
///
/// 一時ディレクトリの絶対パスは報告する時点で消えていて、zip 内のどのファイルかも分からないため。
pub fn display_in(path: &Path, base_dir: &Path) -> String {
    match path.strip_prefix(base_dir) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Ok(rel) => rel.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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

    #[test]
    fn test_parent_dir() {
        assert_eq!(super::parent_dir(Path::new("/d/a.zip")), Path::new("/d"));
        // ファイル名だけの相対パスはカレントディレクトリ
        assert_eq!(super::parent_dir(Path::new("a.zip")), Path::new("."));
    }

    #[test]
    fn test_display_in() {
        let base = Path::new("/tmp/.tmpA1b2");
        // 展開先からの相対パス（zip 内のパス）で表示する
        assert_eq!(
            super::display_in(&base.join("chapter1/001.webp"), base),
            "chapter1/001.webp"
        );
        assert_eq!(super::display_in(base, base), ".");
        // 展開先の外のパスはそのまま
        assert_eq!(
            super::display_in(Path::new("/out/a.zip"), base),
            "/out/a.zip"
        );
    }
}
