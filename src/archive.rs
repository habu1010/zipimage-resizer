//! 入力アーカイブの形式（zip / RAR）の判定と、形式ごとの処理への振り分け

use std::io::Read;
use std::path::Path;

use indicatif::ProgressBar;

use crate::rar_util::{self, RAR5_SIGNATURE};
use crate::zip_util;

/// 入力アーカイブの形式
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ArchiveFormat {
    Zip,
    Rar,
}

/// RAR 1.5 以降の先頭のシグネチャ（RAR 1.5〜4.x は続けて `\x00`、RAR5 は `\x01\x00`）
const RAR_SIGNATURE: &[u8] = RAR5_SIGNATURE.split_at(6).0;
/// RAR 1.4 の先頭のシグネチャ
const RAR14_SIGNATURE: &[u8] = b"RE~^";

impl ArchiveFormat {
    /// ファイルの先頭のシグネチャで形式を判定する
    ///
    /// 拡張子ではなく中身で判定する（中身が zip の `.cbr` などもあるため）。
    /// RAR でなければ zip として扱い、zip でもなければ後段の zip の読み込みでエラーになる。
    pub fn detect(path: &Path) -> std::io::Result<Self> {
        let mut head = Vec::with_capacity(RAR_SIGNATURE.len());
        std::fs::File::open(path)?
            .take(RAR_SIGNATURE.len() as u64)
            .read_to_end(&mut head)?;
        Ok(
            if head == RAR_SIGNATURE || head.starts_with(RAR14_SIGNATURE) {
                Self::Rar
            } else {
                Self::Zip
            },
        )
    }
}

/// アーカイブに含まれるエントリ数を取得する
pub fn get_file_count(path: &Path, format: ArchiveFormat) -> anyhow::Result<usize> {
    match format {
        ArchiveFormat::Zip => Ok(zip_util::get_file_count(path)?),
        ArchiveFormat::Rar => rar_util::get_file_count(path),
    }
}

/// アーカイブを `output_dir` に展開する
pub fn extract_with_progress(
    path: &Path,
    output_dir: &Path,
    format: ArchiveFormat,
    pb: ProgressBar,
) -> anyhow::Result<()> {
    match format {
        ArchiveFormat::Zip => zip_util::unzip_with_progress(path, output_dir, pb),
        ArchiveFormat::Rar => rar_util::unrar_with_progress(path, output_dir, pb),
    }
}

#[cfg(test)]
mod tests {
    use super::ArchiveFormat;

    #[test]
    fn test_detect() {
        let dir = tempfile::tempdir().unwrap();
        for (name, content, expected) in [
            (
                "rar5.cbr",
                &b"Rar!\x1a\x07\x01\x00rest"[..],
                ArchiveFormat::Rar,
            ),
            ("rar4.rar", b"Rar!\x1a\x07\x00rest", ArchiveFormat::Rar),
            ("rar14.rar", b"RE~^rest", ArchiveFormat::Rar),
            // 拡張子が .cbr でも中身が zip なら zip
            ("zip.cbr", b"PK\x03\x04rest", ArchiveFormat::Zip),
            ("short.rar", b"Rar", ArchiveFormat::Zip),
            ("empty.rar", b"", ArchiveFormat::Zip),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, content).unwrap();
            assert_eq!(ArchiveFormat::detect(&path).unwrap(), expected, "{name}");
        }
        assert!(ArchiveFormat::detect(&dir.path().join("missing.rar")).is_err());
    }
}
