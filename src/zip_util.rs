use std::fs::File;
use std::path::{Path, PathBuf};

use indicatif::{ProgressBar, ProgressStyle};
use ripunzip::{NullProgressReporter, UnzipEngine, UnzipOptions};

use crate::fs_util;

/// zipファイルに含まれるファイル数を取得する
/// # Arguments
/// * `zip_file` - ファイル数を取得するzipファイル
/// # Returns
/// ファイル数
/// # Errors
/// zipファイルが存在しない場合、またはzipファイルが壊れている場合にエラーを返す
pub fn get_file_count<P: AsRef<Path>>(zip_file: P) -> zip::result::ZipResult<usize> {
    let file = File::open(zip_file)?;
    let archive = zip::ZipArchive::new(file)?;

    Ok(archive.len())
}

/// zipファイルを解凍する
///
/// # Arguments
///
/// * `zip_file` - 解凍するzipファイル
/// * `output_dir` - 解凍先のディレクトリ
pub fn unzip<P1: AsRef<Path>, P2: Into<PathBuf>>(
    zip_file: P1,
    output_dir: P2,
) -> anyhow::Result<()> {
    let file = File::open(zip_file)?;
    let engine = UnzipEngine::for_file(file)?;
    let options = UnzipOptions {
        output_directory: Some(output_dir.into()),
        password: None,
        single_threaded: false,
        filename_filter: None,
        progress_reporter: Box::new(NullProgressReporter),
    };
    engine.unzip(options)
}

fn get_options(path: &Path) -> zip::write::SimpleFileOptions {
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();

    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "webp" => zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored),
        _ => zip::write::SimpleFileOptions::default(),
    }
}

fn relative_path<'a>(path: &'a Path, base_dir: &Path) -> std::borrow::Cow<'a, str> {
    path.strip_prefix(base_dir).unwrap().to_string_lossy()
}

fn zip_process_dir<W, P>(
    zw: &mut zip::ZipWriter<W>,
    dir: P,
    base_dir: &Path,
    pb: &ProgressBar,
) -> std::io::Result<()>
where
    W: std::io::Write + std::io::Seek,
    P: AsRef<Path>,
{
    let entries = walkdir::WalkDir::new(&dir)
        .max_depth(1)
        .sort_by_file_name()
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    for file in entries.iter().filter(|e| e.file_type().is_file()) {
        let path = file.path();
        let filepath = relative_path(path, base_dir);

        pb.inc(1);

        let options = get_options(path);

        let mut file = std::fs::File::open(path)?;
        zw.start_file(filepath, options)?;
        std::io::copy(&mut file, zw)?;
    }

    for dir_entry in entries
        .iter()
        .filter(|e| e.file_type().is_dir() && e.path() != dir.as_ref())
    {
        let path = dir_entry.path();
        let filepath = relative_path(path, base_dir);
        zw.add_directory(filepath, zip::write::SimpleFileOptions::default())?;
        zip_process_dir(zw, path, base_dir, pb)?;
    }

    Ok(())
}

/// 指定したディレクトリをzipファイルに圧縮する
/// # Arguments
/// * `src_dir` - 圧縮するディレクトリ
/// * `dst_file` - 圧縮先のzipファイル
pub fn zip<P1: AsRef<Path>>(src_dir: P1, dst_file: &File) -> std::io::Result<()> {
    let mut zw = zip::ZipWriter::new(dst_file);

    let file_count = fs_util::scan_regular_files(&src_dir).count();
    let pb = ProgressBar::new(file_count as u64)
        .with_message("Zipping files...")
        .with_style(
            ProgressStyle::with_template("{msg:20} [{bar:40.cyan/blue}] {pos}/{len} ETA:{eta}")
                .unwrap()
                .progress_chars("#>-"),
        );

    zip_process_dir(&mut zw, &src_dir, src_dir.as_ref(), &pb)?;

    zw.finish()?;

    pb.finish_with_message("Zipping completed");

    Ok(())
}
