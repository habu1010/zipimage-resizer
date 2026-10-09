use std::{
    io::{Seek, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver},
        Arc,
    },
    thread::JoinHandle,
};

use anyhow::Result;
use clap::Parser;
use indicatif::{MultiProgress, ParallelProgressIterator, ProgressBar, ProgressStyle};
use rayon::prelude::*;

use crate::fs_util;
use crate::image_util::{ReduceSize, ToWebPMemory};
use crate::zip_util;

#[derive(Parser, Debug)]
struct Args {
    /// 出力先ディレクトリ
    #[clap(short = 'd', long)]
    output_dir: Option<PathBuf>,

    /// 最小の高さ
    #[clap(long, default_value = "1800")]
    min_height: u32,

    /// 最終更新日時を保持する
    #[clap(short, long)]
    keep_mtime: bool,

    zipfiles: Vec<PathBuf>,
}

struct ProcessingContext {
    work_dir: tempfile::TempDir,
    src_file: PathBuf,
    dst_file: PathBuf,
}

#[allow(dead_code)]
fn resize_image_file_jpg<P: AsRef<Path>>(path: P, min_height: u32) -> image::ImageResult<()> {
    let Ok(image) = image::open(path.as_ref()) else {
        return Ok(());
    };

    println!("resize image: {:?}", path.as_ref());
    std::fs::remove_file(path.as_ref())?;
    let resized_image = image.reduce_size(min_height);

    let writer =
        std::io::BufWriter::new(std::fs::File::create(path.as_ref().with_extension("jpg"))?);
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(writer, 95);

    // エンコード（ファイルへの書き込み）
    enc.encode(
        resized_image.as_bytes(),
        resized_image.width(),
        resized_image.height(),
        resized_image.color().into(),
    )

    // resized_image.save(path.as_ref().with_extension("jpg"))
}

fn resize_image_file_webp<P: AsRef<Path>>(path: P, min_height: u32) -> Result<bool> {
    use image::ImageFormat::*;

    const MAX_WEBP_SIZE: usize = 1024 * 1024; // 1MB
    let path = path.as_ref();

    let reader = image::ImageReader::open(path)?.with_guessed_format()?;
    if reader.format() == Some(WebP) && path.metadata()?.len() < MAX_WEBP_SIZE as u64 {
        return Ok(false);
    }

    let Ok(image) = reader.decode() else {
        return Ok(false);
    };

    let webp = image.reduce_size(min_height).to_webp(MAX_WEBP_SIZE)?;

    // 書き込みに失敗しても元画像が残るよう、一時ファイルに書き出してから置き換える
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .suffix(".webp.tmp")
        .tempfile_in(dir)?;
    tmp.write_all(webp.as_ref())?;

    let dst = path.with_extension("webp");
    tmp.persist(&dst)?;
    // 元が WebP の場合は置き換え済みなので削除しない
    if dst != path {
        std::fs::remove_file(path)?;
    }

    Ok(true)
}

fn resize_image_files<P>(files: &[P], min_height: u32, pb: ProgressBar) -> Result<usize>
where
    P: AsRef<Path> + Sync,
{
    pb.set_position(0);
    pb.set_length(files.len() as u64);

    let results = files
        .par_iter()
        .progress_with(pb)
        .map(|file| resize_image_file_webp(file, min_height))
        .collect::<Result<Vec<_>>>()?;

    Ok(results.into_iter().filter(|r| *r).count())
}

fn append_suffix_to_filename<P: AsRef<Path>>(path: P, suffix: &str) -> PathBuf {
    let path = path.as_ref();
    let mut stem = path.file_stem().unwrap().to_os_string();
    let ext = path.extension().unwrap_or(std::ffi::OsStr::new(""));

    stem.push(suffix);
    path.with_file_name(stem).with_extension(ext)
}

fn determine_output_path(path: &Path, output_dir: &Option<PathBuf>) -> PathBuf {
    match output_dir {
        Some(output_dir) => output_dir.join(path.file_name().unwrap()),
        None => append_suffix_to_filename(path, "_resized"),
    }
}

fn calc_average_size_per_file<P: AsRef<Path>>(path: P) -> Result<u64> {
    let file_size = path.as_ref().metadata()?.len();
    let file_count = zip_util::get_file_count(path)? as u64;
    // 空の zip は処理対象外とする（ゼロ除算の回避）
    if file_count == 0 {
        return Ok(0);
    }
    Ok(file_size / file_count)
}

/// 進捗表示とエラー報告をまとめて扱う
///
/// 各段のスレッドへクローンして渡し、エラー件数を全段で共有する。
#[derive(Clone)]
struct Reporter {
    mp: MultiProgress,
    error_count: Arc<AtomicUsize>,
}

impl Reporter {
    fn new() -> Self {
        Self {
            mp: MultiProgress::new(),
            error_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn add_progress_bar(&self, pb: ProgressBar) -> ProgressBar {
        self.mp.add(pb)
    }

    /// エラーを stderr に出力して件数を数える
    ///
    /// プログレスバーの描画を一時停止しないと表示が崩れるため、`suspend` 経由で出力する。
    fn report_error(&self, msg: String) {
        self.error_count.fetch_add(1, Ordering::Relaxed);
        self.mp.suspend(|| eprintln!("{msg}"));
    }

    fn error_count(&self) -> usize {
        self.error_count.load(Ordering::Relaxed)
    }
}

fn spawn_size_check_thread(
    zipfiles: Vec<PathBuf>,
    reporter: &Reporter,
) -> (JoinHandle<Result<()>>, Receiver<PathBuf>) {
    let pb = reporter.add_progress_bar(
        ProgressBar::new(zipfiles.len() as u64).with_style(
            ProgressStyle::with_template(
                "Size checking [{bar:20.cyan/blue}] {pos}/{len} ETA:{eta} {msg}",
            )
            .unwrap()
            .progress_chars("#>-"),
        ),
    );

    let reporter = reporter.clone();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let total = zipfiles.len();
        let mut to_convert = 0;
        for zipfile in zipfiles {
            pb.set_message(zipfile.to_string_lossy().into_owned());
            match calc_average_size_per_file(&zipfile) {
                Ok(size_per_file) if size_per_file > 500 * 1024 => {
                    to_convert += 1;
                    tx.send(zipfile)?;
                }
                Err(e) => {
                    reporter.report_error(format!("Error calculating size for {zipfile:?}: {e}"));
                }
                _ => {}
            }
            pb.inc(1);
        }
        pb.finish_with_message(format!(
            "Size checking completed: {to_convert}/{total} files to convert"
        ));
        Ok(())
    });

    (handle, rx)
}

/// zip を一時ディレクトリへ展開し、後続の段へ渡すコンテキストを作る
fn extract_zip(
    src_file: &Path,
    output_dir: &Option<PathBuf>,
    pb: &ProgressBar,
) -> Result<ProcessingContext> {
    let dst_file = determine_output_path(src_file, output_dir);
    let work_dir = tempfile::tempdir()?;
    zip_util::unzip_with_progress(src_file, work_dir.path(), pb.clone())?;

    Ok(ProcessingContext {
        work_dir,
        src_file: src_file.to_path_buf(),
        dst_file,
    })
}

fn spawn_zip_extraction_thread(
    output_dir: Option<PathBuf>,
    size_check_rx: Receiver<PathBuf>,
    reporter: &Reporter,
) -> (JoinHandle<Result<()>>, Receiver<ProcessingContext>) {
    let pb = reporter.add_progress_bar(
        ProgressBar::new(0).with_style(
            ProgressStyle::with_template(
                "Extracting [{bar:20.cyan/blue}] {pos}/{len} ETA:{eta} {msg}",
            )
            .unwrap()
            .progress_chars("#>-"),
        ),
    );

    let reporter = reporter.clone();
    let (tx, rx) = mpsc::sync_channel(3);
    let handle = std::thread::spawn(move || {
        for src_file in size_check_rx {
            pb.set_message(src_file.to_string_lossy().into_owned());
            // 1 ファイルの失敗で全体を止めず、次のファイルへ進む
            let ctx = match extract_zip(&src_file, &output_dir, &pb) {
                Ok(ctx) => ctx,
                Err(e) => {
                    reporter.report_error(format!("Error extracting {src_file:?}: {e}"));
                    continue;
                }
            };
            tx.send(ctx)?;
        }
        pb.finish_with_message("Extracting completed");
        Ok(())
    });

    (handle, rx)
}

fn spawn_image_resizing_thread(
    min_height: u32,
    zip_extraction_rx: Receiver<ProcessingContext>,
    reporter: &Reporter,
) -> (JoinHandle<Result<()>>, Receiver<ProcessingContext>) {
    let pb = reporter.add_progress_bar(
        ProgressBar::new(0).with_style(
            ProgressStyle::with_template(
                "Resizing images [{bar:20.cyan/blue}] {pos}/{len} ETA:{eta} {msg}",
            )
            .unwrap()
            .progress_chars("#>-"),
        ),
    );

    let reporter = reporter.clone();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        for ctx in zip_extraction_rx {
            pb.set_message(ctx.src_file.to_string_lossy().into_owned());
            let files: Vec<_> = fs_util::scan_regular_files(ctx.work_dir.path()).collect();
            match resize_image_files(&files, min_height, pb.clone()) {
                // 1 枚も変換されなかった zip は出力しない
                Ok(resized) if resized > 0 => tx.send(ctx)?,
                Ok(_) => {}
                Err(e) => {
                    reporter.report_error(format!("Error resizing {:?}: {e}", ctx.src_file));
                }
            }
        }
        pb.finish_with_message("Resizing completed");
        Ok(())
    });

    (handle, rx)
}

/// リサイズ済みのディレクトリから出力先の zip を作成する
///
/// 一旦 tempfile に zip を書き出し、完成してから出力先へコピーする。
fn create_resized_zip(ctx: &ProcessingContext, keep_mtime: bool, pb: &ProgressBar) -> Result<()> {
    let mut dst_tempfile = tempfile::tempfile()?;
    zip_util::zip_with_progress(ctx.work_dir.path(), &dst_tempfile, pb.clone())?;

    // dst_tempfile -> dst_file
    dst_tempfile.rewind()?;
    let mut dst_zipfile = std::fs::File::create(&ctx.dst_file)?;
    std::io::copy(&mut dst_tempfile, &mut dst_zipfile)?;

    if keep_mtime {
        let src_mtime = ctx.src_file.metadata()?.modified()?;
        dst_zipfile.set_modified(src_mtime)?;
    }

    Ok(())
}

fn spawn_zip_creation_thread(
    keep_mtime: bool,
    image_resizing_rx: Receiver<ProcessingContext>,
    reporter: &Reporter,
) -> JoinHandle<Result<()>> {
    let pb = reporter.add_progress_bar(
        ProgressBar::new(0).with_style(
            ProgressStyle::with_template(
                "Creating zip [{bar:20.cyan/blue}] {pos}/{len} ETA:{eta} {msg}",
            )
            .unwrap()
            .progress_chars("#>-"),
        ),
    );

    let reporter = reporter.clone();
    std::thread::spawn(move || {
        for ctx in image_resizing_rx {
            pb.set_message(ctx.dst_file.to_string_lossy().into_owned());
            if let Err(e) = create_resized_zip(&ctx, keep_mtime, &pb) {
                // コピー途中で失敗した場合、壊れた zip を残さない
                let _ = std::fs::remove_file(&ctx.dst_file);
                reporter.report_error(format!("Error creating {:?}: {e}", ctx.dst_file));
            }
        }
        pb.finish_with_message("Creating zip completed");
        Ok(())
    })
}

pub fn run() -> Result<()> {
    let Args {
        output_dir,
        min_height,
        keep_mtime,
        zipfiles,
    } = Args::parse();

    // 展開・変換を始める前に出力先を検証する
    // （最終段まで進んでから失敗すると、それまでの処理が無駄になるため）
    if let Some(output_dir) = &output_dir {
        if !output_dir.exists() {
            anyhow::bail!("output directory not found: {}", output_dir.display());
        }
        if !output_dir.is_dir() {
            anyhow::bail!("output path is not a directory: {}", output_dir.display());
        }
    }

    let reporter = Reporter::new();

    let (size_check_handle, size_check_rx) = spawn_size_check_thread(zipfiles, &reporter);
    let (zip_extraction_handle, zip_extraction_rx) =
        spawn_zip_extraction_thread(output_dir, size_check_rx, &reporter);
    let (image_resizing_handle, image_resizing_rx) =
        spawn_image_resizing_thread(min_height, zip_extraction_rx, &reporter);
    let zip_creation_handle = spawn_zip_creation_thread(keep_mtime, image_resizing_rx, &reporter);

    // 途中で return すると残りのスレッドが強制終了され、
    // 書きかけの zip が残るため、まず全スレッドの終了を待つ
    let results = [
        size_check_handle.join(),
        zip_extraction_handle.join(),
        image_resizing_handle.join(),
        zip_creation_handle.join(),
    ];

    // 下流のエラーが根本原因で、上流のエラーはチャネル切断による波及。
    // そのため下流から順にエラーを探す
    for result in results.into_iter().rev() {
        result.unwrap()?;
    }

    // 個々のファイルの失敗は処理を止めないが、終了コードには反映する
    let error_count = reporter.error_count();
    if error_count > 0 {
        anyhow::bail!("{error_count} file(s) failed");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn test_append_suffix_to_filename() {
        // サフィックスはそのまま付与される（区切り文字は呼び出し側が含める）
        let path = super::append_suffix_to_filename(Path::new("/path1/path2/test.txt"), "_suffix");
        assert_eq!(path, Path::new("/path1/path2/test_suffix.txt"));
    }

    #[test]
    fn test_append_suffix_to_filename_without_extension() {
        let path = super::append_suffix_to_filename(Path::new("/path1/path2/test"), "_suffix");
        assert_eq!(path, Path::new("/path1/path2/test_suffix"));
    }

    #[test]
    fn test_determine_output_path() {
        let src = Path::new("/path1/path2/test.zip");

        // 出力先ディレクトリ指定時はファイル名だけを引き継ぐ
        let path = super::determine_output_path(src, &Some(Path::new("/out").to_path_buf()));
        assert_eq!(path, Path::new("/out/test.zip"));

        // 未指定時は元ファイル名に _resized を付ける
        let path = super::determine_output_path(src, &None);
        assert_eq!(path, Path::new("/path1/path2/test_resized.zip"));
    }
}
