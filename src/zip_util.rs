use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use indicatif::ProgressBar;
use ripunzip::{UnzipEngine, UnzipOptions, UnzipProgressReporter};

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

/// `src_dir` 以下のファイルとディレクトリを `zw` に追加する
///
/// 1 回の走査で全エントリを集め、件数を進捗表示に使ってから追加する。
/// エントリの順序は、各ディレクトリで「ファイル（名前順）→ サブディレクトリ（名前順）」の
/// 深さ優先にする（ディレクトリの直後にその中身が続く）。
fn add_dir_to_zip<W>(
    zw: &mut zip::ZipWriter<W>,
    src_dir: &Path,
    pb: &ProgressBar,
) -> anyhow::Result<()>
where
    W: std::io::Write + std::io::Seek,
{
    // 走査エラーを無視すると、そのファイルが黙って zip から抜け落ちるのでエラーにする
    let entries = walkdir::WalkDir::new(src_dir)
        .min_depth(1)
        .sort_by(|a, b| {
            (a.file_type().is_dir(), a.file_name()).cmp(&(b.file_type().is_dir(), b.file_name()))
        })
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;

    let file_count = entries.iter().filter(|e| e.file_type().is_file()).count();
    pb.set_position(0);
    pb.set_length(file_count as u64);

    for entry in &entries {
        let path = entry.path();
        let filepath = relative_path(path, src_dir);
        if entry.file_type().is_dir() {
            zw.add_directory(filepath, zip::write::SimpleFileOptions::default())?;
        } else if entry.file_type().is_file() {
            pb.inc(1);
            let mut file =
                std::fs::File::open(path).with_context(|| format!("failed to read {filepath}"))?;
            zw.start_file(filepath, get_options(path))?;
            std::io::copy(&mut file, zw)?;
        }
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
) -> anyhow::Result<()> {
    // ZipWriter は小さな書き込み（ヘッダやコピー時の 8KB 単位の書き込み）を大量に行うため、
    // バッファしてシステムコールの回数を減らす
    const WRITE_BUF_SIZE: usize = 1024 * 1024; // 1MB（変換後の WebP 1 枚がほぼ 1 回の書き込みに収まる）
    let mut zw = zip::ZipWriter::new(BufWriter::with_capacity(WRITE_BUF_SIZE, dst_file));

    add_dir_to_zip(&mut zw, src_dir.as_ref(), &pb)?;

    // drop 任せにすると書き込みエラーが無視されるので、明示的に flush する
    zw.finish()?.flush()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use indicatif::ProgressBar;

    #[test]
    fn test_zip_with_progress() {
        // 作成した zip を読み戻し、エントリの順序・中身・圧縮方式を確認する
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.webp"), b"image").unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), b"text").unwrap();

        let dst = tempfile::tempfile().unwrap();
        super::zip_with_progress(dir.path(), &dst, ProgressBar::hidden()).unwrap();

        let mut archive = zip::ZipArchive::new(&dst).unwrap();
        let names: Vec<_> = archive.file_names().map(str::to_owned).collect();
        assert_eq!(names, ["a.webp", "sub/", "sub/b.txt"]);
        for (name, content, method) in [
            ("a.webp", "image", zip::CompressionMethod::Stored),
            ("sub/b.txt", "text", zip::CompressionMethod::Deflated),
        ] {
            let entry = archive.by_name(name).unwrap();
            assert_eq!(entry.compression(), method, "{name}");
            assert_eq!(std::io::read_to_string(entry).unwrap(), content);
        }
    }

    // 読めないディレクトリがあれば、中のファイルを抜かした zip を作らずにエラーにする
    #[test]
    #[cfg(unix)]
    fn test_zip_with_progress_reports_unreadable_dir() {
        let dir = tempfile::tempdir().unwrap();
        let Some(_restore) = crate::test_util::unreadable_subdir(dir.path()) else {
            return;
        };

        let dst = tempfile::tempfile().unwrap();
        let result = super::zip_with_progress(dir.path(), &dst, ProgressBar::hidden());
        assert!(result.is_err());
    }

    #[test]
    fn test_zip_with_progress_entry_order() {
        // 各ディレクトリで「ファイル（名前順）→ サブディレクトリ（名前順）」の深さ優先になる
        let dir = tempfile::tempdir().unwrap();
        crate::test_util::touch_files(
            dir.path(),
            &[
                "z.png",
                "b.webp",
                "a/x.txt",
                "a/b/y.txt",
                "c/z.txt",
                "a/0.txt",
            ],
        );

        let dst = tempfile::tempfile().unwrap();
        super::zip_with_progress(dir.path(), &dst, ProgressBar::hidden()).unwrap();

        let archive = zip::ZipArchive::new(&dst).unwrap();
        let names: Vec<_> = archive.file_names().collect();
        assert_eq!(
            names,
            [
                "b.webp",
                "z.png",
                "a/",
                "a/0.txt",
                "a/x.txt",
                "a/b/",
                "a/b/y.txt",
                "c/",
                "c/z.txt"
            ]
        );
    }
}
