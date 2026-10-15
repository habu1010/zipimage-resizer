use std::path::{Path, PathBuf};

/// `dir` 以下の通常ファイルを再帰的に列挙する
///
/// 走査中のエラー（読み取り権限がないディレクトリなど）は、そのファイルを
/// 黙って取りこぼさないよう `Err` として返す。
pub fn scan_regular_files(dir: &Path) -> impl Iterator<Item = walkdir::Result<PathBuf>> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(e) if e.file_type().is_file() => Some(Ok(e.into_path())),
            Ok(_) => None,
            Err(e) => Some(Err(e)),
        })
}

/// walkdir のエラーを、絶対パスの代わりに `base_dir` からの相対パスで示す（理由は `display_in` 参照）
///
/// ディレクトリを読む途中で起きた io エラーには walkdir がパスを付けないので、
/// その場合は失敗した位置を示せない（表示にも絶対パスは含まれないので、そのまま返す）。
pub fn walk_error_in(e: walkdir::Error, base_dir: &Path) -> anyhow::Error {
    let Some(path) = e.path() else {
        return anyhow::Error::new(e);
    };
    let path = display_in(path, base_dir);
    match e.io_error() {
        Some(io) => anyhow::anyhow!("failed to read {path}: {io}"),
        // io エラーを持たないのはシンボリックリンクのループ（リンクをたどる走査でだけ起きる）
        None => anyhow::anyhow!("filesystem loop found at {path}"),
    }
}

/// 親ディレクトリを返す（`a.zip` のような相対パスのファイル名だけの場合は `.`）
pub fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// エラー表示用に、`path` を `base_dir` からの相対パスにする（`base_dir` の外のパスはそのまま）
///
/// 主に展開先の一時ディレクトリに使う。一時ディレクトリの絶対パスは報告する時点で消えていて、
/// zip 内のどのファイルかも分からないため。
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
            .collect::<walkdir::Result<_>>()
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
        let err = super::scan_regular_files(dir.path())
            .find_map(Result::err)
            .unwrap();
        let err = super::walk_error_in(err, dir.path());
        // 一時ディレクトリの絶対パスではなく、相対パスで示す
        assert!(
            format!("{err:#}").starts_with("failed to read sub: "),
            "{err:#}"
        );
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
