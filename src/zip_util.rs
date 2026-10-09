use std::fs::File;
use std::path::{Path, PathBuf};

use indicatif::ProgressBar;
use ripunzip::{UnzipEngine, UnzipOptions, UnzipProgressReporter};

use crate::fs_util;

struct UnzipProgressBar(ProgressBar);

impl UnzipProgressReporter for UnzipProgressBar {
    fn extraction_finished(&self, _display_name: &str) {
        self.0.inc(1);
    }
}

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
/// * `pb` - 進捗表示に使うプログレスバー
pub fn unzip_with_progress<P1: AsRef<Path>, P2: Into<PathBuf>>(
    zip_file: P1,
    output_dir: P2,
    pb: ProgressBar,
) -> anyhow::Result<()> {
    pb.set_position(0);
    pb.set_length(get_file_count(&zip_file)? as u64);

    let file = File::open(&zip_file)?;
    let engine = UnzipEngine::for_file(file)?;
    let options = UnzipOptions {
        output_directory: Some(output_dir.into()),
        password: None,
        single_threaded: true,
        filename_filter: None,
        progress_reporter: Box::new(UnzipProgressBar(pb.clone())),
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
    // 走査エラーを無視すると、そのファイルが黙って zip から抜け落ちるのでエラーにする
    let entries = walkdir::WalkDir::new(&dir)
        .max_depth(1)
        .sort_by_file_name()
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;

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
/// * `pb` - 進捗表示に使うプログレスバー
pub fn zip_with_progress<P1: AsRef<Path>>(
    src_dir: P1,
    dst_file: &File,
    pb: ProgressBar,
) -> std::io::Result<()> {
    let mut zw = zip::ZipWriter::new(dst_file);

    let file_count =
        fs_util::scan_regular_files(&src_dir).try_fold(0u64, |n, file| file.map(|_| n + 1))?;
    pb.set_position(0);
    pb.set_length(file_count);

    zip_process_dir(&mut zw, &src_dir, src_dir.as_ref(), &pb)?;

    zw.finish()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use indicatif::ProgressBar;

    // 読めないディレクトリがあれば、中のファイルを抜かした zip を作らずにエラーにする。
    // zip_with_progress は件数を数える時点でエラーを返すため、zip_process_dir 単体でも確認する

    #[test]
    #[cfg(unix)]
    fn test_zip_with_progress_reports_unreadable_dir() {
        let dir = tempfile::tempdir().unwrap();
        let Some(_restore) = crate::fs_util::tests::unreadable_subdir(dir.path()) else {
            return;
        };

        let dst = tempfile::tempfile().unwrap();
        let result = super::zip_with_progress(dir.path(), &dst, ProgressBar::hidden());
        assert!(result.is_err());
    }

    #[test]
    #[cfg(unix)]
    fn test_zip_process_dir_reports_unreadable_dir() {
        let dir = tempfile::tempdir().unwrap();
        let Some(_restore) = crate::fs_util::tests::unreadable_subdir(dir.path()) else {
            return;
        };

        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let result =
            super::zip_process_dir(&mut zw, dir.path(), dir.path(), &ProgressBar::hidden());
        assert!(result.is_err());
    }
}
