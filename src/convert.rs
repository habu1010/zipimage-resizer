//! 展開済みの画像を WebP に変換する

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use indicatif::{ParallelProgressIterator, ProgressBar};
use rayon::prelude::*;

use crate::fs_util::{parent_dir, scan_regular_files, walk_error_in};
use crate::image_util::{self, ReduceSize, ToWebPMemory};
use crate::webp_names::rename_to_final_names;

/// 変換後の WebP の目標サイズ
///
/// エンコード時はこのサイズ以下に収まるまで品質を下げる。また、既にこのサイズ未満の
/// WebP は変換しない。
const MAX_WEBP_SIZE: usize = 1024 * 1024; // 1MB

/// `src` を WebP に変換し、同じディレクトリの一時ファイルに書き出す
///
/// 書き出しに成功してから `src` を削除し、一時ファイルのパスを返す。
/// 変換対象外のファイルは `None` を返す。
/// 最終的なファイル名は、全ファイルの変換後に `rename_to_final_names` で決める。
fn resize_image_file_webp(src: &Path, max_height: u32) -> Result<Option<PathBuf>> {
    use image::ImageFormat::*;

    let reader = image::ImageReader::open(src)?.with_guessed_format()?;
    if reader.format() == Some(WebP) && src.metadata()?.len() < MAX_WEBP_SIZE as u64 {
        return Ok(None);
    }
    // 静止画の WebP にすると先頭フレーム以外が失われるので、そのまま残す
    if image_util::is_animated(src, reader.format()) {
        return Ok(None);
    }

    let image = match image_util::decode_with_orientation(reader) {
        Ok(image) => image,
        // 画像でない（形式を判定できない）ファイルや未対応の形式は、変換対象外として黙って残す
        Err(image::ImageError::Unsupported(_)) => return Ok(None),
        // 画像と判定できたのにデコードできないのは壊れた画像なので、エラーとして報告する
        Err(e) => return Err(anyhow::Error::new(e).context("failed to decode")),
    };

    let webp = image.reduce_size(max_height).to_webp(MAX_WEBP_SIZE)?;

    // 既存のファイルと衝突しない一意な名前で作成する（失敗時は自動で削除される）
    let mut tmp = tempfile::Builder::new()
        .suffix(".webp.tmp")
        .tempfile_in(parent_dir(src))
        .context("failed to create a temporary file")?;
    tmp.write_all(webp.as_ref())
        .context("failed to write the converted image")?;
    // エラー（PathPersistError）は一時ファイルを保持しているので、io::Error だけを取り出して
    // 一時ファイルをその場で削除する（失敗の一覧に残ると zip に紛れ込みうる）
    let tmp = tmp
        .into_temp_path()
        .keep()
        .map_err(|e| e.error)
        .context("failed to keep the converted image")?;

    // 元画像を消せなかった場合は、変換結果を消して元画像だけを残す
    // （失敗しても zip の処理を続けるため、変換結果の残骸を zip に紛れ込ませない）
    if let Err(e) = std::fs::remove_file(src) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::new(e).context("failed to remove the original image"));
    }

    Ok(Some(tmp))
}

/// `resize_image_files` の結果
pub struct ResizeOutcome {
    /// 変換できた画像の枚数
    pub converted: usize,
    /// 変換に失敗した画像の zip 内のパス（展開先からの相対パス）とその理由（パス順）。
    /// これらの画像は元のまま残っている
    pub failed: Vec<(PathBuf, anyhow::Error)>,
}

/// `base_dir`（アーカイブの展開先）以下の画像を WebP に変換する
///
/// 個々の画像の変換に失敗しても、その画像は元のまま残して `failed` に記録し、
/// 残りの画像の変換を続ける（1 枚のために zip 全体を諦めない）。
///
/// 走査に失敗した場合は、何も変更せずに `Err` を返す。最終名への rename に失敗した場合の
/// `Err` では、`base_dir` の中身は途中の状態（元画像を消した後の一時ファイルが残っている
/// など）なので、出力に使ってはならない。
pub fn resize_image_files(
    base_dir: &Path,
    max_height: u32,
    pb: ProgressBar,
) -> Result<ResizeOutcome> {
    let files: Vec<PathBuf> = scan_regular_files(base_dir)
        .collect::<walkdir::Result<_>>()
        .map_err(|e| walk_error_in(e, base_dir))
        .context("failed to scan the extracted files")?;

    pb.set_position(0);
    pb.set_length(files.len() as u64);

    let results: Vec<_> = files
        .par_iter()
        .progress_with(pb)
        .map(|src| (src, resize_image_file_webp(src, max_height)))
        .collect();

    let mut converted = Vec::new();
    let mut failed = Vec::new();
    for (src, result) in results {
        match result {
            Ok(Some(tmp)) => converted.push((src.clone(), tmp)),
            Ok(None) => {}
            // 一時ディレクトリの絶対パスは報告時には消えているので、zip 内のパスにしておく
            Err(e) => failed.push((src.strip_prefix(base_dir).unwrap_or(src).to_path_buf(), e)),
        }
    }
    failed.sort_by(|(a, _), (b, _)| a.cmp(b));

    let outcome = ResizeOutcome {
        converted: converted.len(),
        failed,
    };
    rename_to_final_names(converted, base_dir)?;

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::test_util::{list_names, write_animated_gif, write_image};

    /// `files` を作成して変換し、変換件数と変換後のエントリ名を返す
    ///
    /// 末尾が `/` ならディレクトリ、`.db` なら画像でないファイル、`anim` で始まれば
    /// アニメーション GIF、それ以外は静止画として作成する
    fn resize_files_in_tempdir(files: &[&str]) -> (usize, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        for file in files {
            let path = dir.path().join(file);
            if file.ends_with('/') {
                std::fs::create_dir(&path).unwrap();
            } else if file.ends_with(".db") {
                std::fs::write(&path, b"not an image").unwrap();
            } else if file.starts_with("anim") {
                write_animated_gif(&path);
            } else {
                write_image(&path);
            }
        }

        let outcome = resize_dir(dir.path());
        assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
        (outcome.converted, list_names(dir.path()))
    }

    /// `dir` 内の全ファイルを変換する
    fn resize_dir(dir: &Path) -> super::ResizeOutcome {
        super::resize_image_files(dir, 1800, indicatif::ProgressBar::hidden()).unwrap()
    }

    #[test]
    fn test_resize_image_files_keeps_same_stem_images() {
        // 両方の画像が別名の WebP として残り、ページ順も保たれること
        let (resized, names) = resize_files_in_tempdir(&["001.jpg", "001.png"]);
        assert_eq!(resized, 2);
        assert_eq!(names, ["001.jpg.webp", "001.png.webp"]);
    }

    #[test]
    fn test_resize_image_files_does_not_overwrite_skipped_webp() {
        // 変換対象外の既存 WebP は上書きしない（大文字小文字違いも衝突とみなす）
        let (resized, names) =
            resize_files_in_tempdir(&["001.webp", "001.png", "002.WEBP", "002.png"]);
        assert_eq!(resized, 2);
        assert_eq!(
            names,
            ["001.png.webp", "001.webp", "002.WEBP", "002.png.webp"]
        );
    }

    #[test]
    fn test_resize_image_files_keeps_animated_gif() {
        // アニメーション GIF は変換せずにそのまま残し、静止画の GIF は変換する
        let (resized, names) = resize_files_in_tempdir(&["anim.gif", "still.gif"]);
        assert_eq!(resized, 1);
        assert_eq!(names, ["anim.gif", "still.webp"]);
    }

    #[test]
    #[cfg(unix)]
    fn test_resize_image_files_continues_on_error() {
        // 読めない画像があっても、その画像は元のまま残して他の画像の変換を続ける
        let dir = tempfile::tempdir().unwrap();
        write_image(&dir.path().join("001.png"));
        write_image(&dir.path().join("002.png"));
        let Some(_restore) = crate::test_util::make_unreadable(&dir.path().join("002.png")) else {
            return;
        };

        let outcome = resize_dir(dir.path());
        assert_eq!(outcome.converted, 1);
        let failed: Vec<_> = outcome.failed.iter().map(|(file, _)| file).collect();
        assert_eq!(failed, [Path::new("002.png")]);
        assert_eq!(list_names(dir.path()), ["001.webp", "002.png"]);
    }

    #[test]
    fn test_resize_image_files_reports_corrupt_image() {
        // 画像と判定できるのにデコードできない（壊れた）画像は報告し、画像でないファイルは黙って残す
        let dir = tempfile::tempdir().unwrap();
        write_image(&dir.path().join("001.png"));
        let png = std::fs::read(dir.path().join("001.png")).unwrap();
        std::fs::write(dir.path().join("002.png"), &png[..png.len() / 2]).unwrap();
        std::fs::write(dir.path().join("readme.txt"), b"not an image").unwrap();

        let outcome = resize_dir(dir.path());
        assert_eq!(outcome.converted, 1);
        let failed: Vec<_> = outcome.failed.iter().map(|(file, _)| file).collect();
        assert_eq!(failed, [Path::new("002.png")]);
        assert_eq!(
            list_names(dir.path()),
            ["001.webp", "002.png", "readme.txt"]
        );
    }

    #[test]
    fn test_resize_image_files_ignores_non_image_names() {
        // 画像でないファイルは変換されないので、名前の割り当てにも影響しない
        let (resized, names) = resize_files_in_tempdir(&["001.db", "001.jpg"]);
        assert_eq!(resized, 1);
        assert_eq!(names, ["001.db", "001.webp"]);
    }

    #[test]
    fn test_resize_image_files_avoids_directory_name() {
        // 同名のディレクトリ（001.webp/）とも衝突させない
        let (resized, names) = resize_files_in_tempdir(&["001.webp/", "001.png"]);
        assert_eq!(resized, 1);
        assert_eq!(names, ["001.png.webp", "001.webp"]);
    }

    #[test]
    #[cfg(unix)]
    fn test_resize_image_files_reports_scan_error() {
        // 読めないディレクトリがあれば、その中の画像を黙って変換し損ねずにエラーにする。
        // 走査の失敗は変換を始める前に起きるので、読める画像も変換しない
        let dir = tempfile::tempdir().unwrap();
        write_image(&dir.path().join("001.png"));
        let Some(_restore) = crate::test_util::unreadable_subdir(dir.path()) else {
            return;
        };
        let result = super::resize_image_files(dir.path(), 1800, indicatif::ProgressBar::hidden());
        let Err(err) = result else {
            panic!("expected an error");
        };
        assert!(
            format!("{err:#}")
                .starts_with("failed to scan the extracted files: failed to read sub: "),
            "{err:#}"
        );
        assert_eq!(list_names(dir.path()), ["001.png", "sub"]);
    }
}
