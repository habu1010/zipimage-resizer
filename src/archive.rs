//! 入力アーカイブの形式（zip / RAR）の判定と、形式ごとの処理への振り分け

use std::path::Path;

use indicatif::ProgressBar;

use crate::rar_util;
use crate::zip_util;

/// 入力アーカイブの形式
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ArchiveFormat {
    Zip,
    Rar,
}

impl ArchiveFormat {
    /// ファイルの先頭のシグネチャで形式を判定する
    ///
    /// 拡張子ではなく中身で判定する（中身が zip の `.cbr` などもあるため）。
    /// RAR でなければ zip として扱い、zip でもなければ後段の zip の読み込みでエラーになる。
    pub fn detect(path: &Path) -> std::io::Result<Self> {
        Ok(if rar_util::is_rar(path)? {
            Self::Rar
        } else {
            Self::Zip
        })
    }

    /// アーカイブに含まれるエントリ数（ディレクトリを含む）を取得する
    pub fn file_count(self, path: &Path) -> anyhow::Result<usize> {
        match self {
            Self::Zip => zip_util::file_count(path),
            Self::Rar => rar_util::file_count(path),
        }
    }

    /// アーカイブを `output_dir` に展開し、展開したエントリ数を `pb` に進捗として示す
    pub fn extract(self, path: &Path, output_dir: &Path, pb: &ProgressBar) -> anyhow::Result<()> {
        match self {
            Self::Zip => zip_util::extract(path, output_dir, pb),
            Self::Rar => rar_util::extract(path, output_dir, pb),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ArchiveFormat;

    #[test]
    fn test_detect() {
        let dir = tempfile::tempdir().unwrap();
        // 拡張子ではなく中身で判定する（シグネチャの細かい場合分けは rar_util::is_rar のテスト）
        for (name, content, expected) in [
            (
                "rar.cbz",
                &b"Rar!\x1a\x07\x01\x00rest"[..],
                ArchiveFormat::Rar,
            ),
            ("zip.cbr", b"PK\x03\x04rest", ArchiveFormat::Zip),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, content).unwrap();
            assert_eq!(ArchiveFormat::detect(&path).unwrap(), expected, "{name}");
        }
        assert!(ArchiveFormat::detect(&dir.path().join("missing.rar")).is_err());
    }

    #[test]
    fn test_file_count_and_extract() {
        use crate::test_util::{fixture, write_zip};

        // 形式ごとに、件数の取得と展開がそれぞれの実装に振り分けられる
        // （展開の中身は zip_util・rar_util のテストで確かめる）
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        write_zip(&zip, &[("a.txt", b"text".to_vec())]);

        for (format, path, count) in [
            (ArchiveFormat::Zip, zip, 1),
            (ArchiveFormat::Rar, fixture("basic.rar"), 4),
        ] {
            assert_eq!(format.file_count(&path).unwrap(), count, "{format:?}");
            let out = tempfile::tempdir().unwrap();
            let pb = indicatif::ProgressBar::hidden();
            format.extract(&path, out.path(), &pb).unwrap();
            assert_eq!(pb.position(), count as u64, "{format:?}");
        }
    }
}
