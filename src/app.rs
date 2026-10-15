use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread::JoinHandle,
};

use anyhow::{Context, Result};
use clap::Parser;
use indicatif::ProgressBar;

use crate::archive::{self, ArchiveFormat};
use crate::convert::{resize_image_files, ResizeOutcome};
use crate::fs_util::parent_dir;
use crate::plan::{plan_jobs, ArchiveJob};
use crate::reporter::Reporter;
use crate::zip_util;

/// 処理対象にするアーカイブの、1 ファイルあたりの平均サイズの下限
///
/// 平均がこれ以下のアーカイブは、既に十分小さいとみなして処理しない。
const MIN_AVERAGE_FILE_SIZE: u64 = 500 * 1024; // 500KB

/// 展開済みのアーカイブ（一時ディレクトリ）を受け渡すチャネル 1 本あたりに溜めておける数
///
/// 上限がないと、下流の段が遅いときに展開済みの一時ディレクトリが際限なく溜まり、
/// ディスクを圧迫する。上限はチャネルごとなので、同時に存在する一時ディレクトリは
/// 2 本のチャネル（展開 → リサイズ、リサイズ → zip 作成）に溜まる分と、各段で処理中の
/// 分を合わせて、最大で `2 * MAX_PENDING_WORK_DIRS + 3` 個になる。
const MAX_PENDING_WORK_DIRS: usize = 3;

// 各引数の説明は --help にそのまま表示されるユーザー向けの文言なので、英語で書き、
// 説明を二重に持たないよう日本語の doc コメントは付けない。
#[derive(Parser, Debug)]
#[clap(
    about = "Shrink the images in zip/RAR archives by re-encoding them to WebP \
                (the output is always a zip)"
)]
struct Args {
    #[clap(
        short = 'd',
        long,
        help = "Output directory. If omitted, each output is written next to its input \
                with \"_resized\" added before the extension (e.g. a.zip -> a_resized.zip). \
                Outputs are always zip, so RAR archives and .rar/.cbr files are written \
                with a .zip extension (e.g. a.rar -> a_resized.zip, Vol.1 -> Vol_resized.1.zip). \
                If this is an input's directory, an input whose output would replace \
                another existing file there (e.g. a.zip for a.rar) is skipped"
    )]
    output_dir: Option<PathBuf>,

    #[clap(
        long,
        default_value_t = 1800,
        // 0 を許すと全ページが高さ 1px に潰れ、原本も消えるので弾く
        value_parser = clap::value_parser!(u32).range(1..),
        help = "Maximum image height; taller images are scaled down to this height"
    )]
    max_height: u32,

    #[clap(
        short,
        long,
        help = "Set the modification time of each output to that of its input"
    )]
    keep_mtime: bool,

    #[clap(help = "Zip or RAR files to process")]
    archives: Vec<PathBuf>,
}

struct ProcessingContext {
    work_dir: tempfile::TempDir,
    job: ArchiveJob,
}

/// 変換に失敗した画像を、アーカイブごとに 1 件のエラーとしてまとめた文面にする
///
/// ディスクフルなどで大量に失敗しても表示が溢れないよう、個別のファイルは先頭の数件だけ示す。
/// パスは `ResizeOutcome::failed` のアーカイブ内のパスをそのまま示す。
fn format_conversion_failures(src_file: &Path, outcome: &ResizeOutcome) -> String {
    use std::fmt::Write as _;

    const MAX_LISTED: usize = 5;

    let failed = &outcome.failed;
    let total = outcome.converted + failed.len();
    let mut msg = format!(
        "Failed to convert {} of {total} image(s) in {}",
        failed.len(),
        src_file.display()
    );
    if outcome.converted > 0 {
        msg.push_str(" (written with the originals kept)");
    }
    msg.push(':');
    for (file, e) in failed.iter().take(MAX_LISTED) {
        let _ = write!(msg, "\n  {}: {e:#}", file.display());
    }
    if failed.len() > MAX_LISTED {
        let _ = write!(msg, "\n  ... and {} more", failed.len() - MAX_LISTED);
    }
    msg
}

fn calc_average_size_per_file(path: &Path, format: ArchiveFormat) -> Result<u64> {
    let file_size = path.metadata()?.len();
    let file_count = archive::get_file_count(path, format)? as u64;
    // 空のアーカイブは処理対象外とする（ゼロ除算の回避）
    if file_count == 0 {
        return Ok(0);
    }
    Ok(file_size / file_count)
}

fn spawn_size_check_thread(
    jobs: Vec<ArchiveJob>,
    reporter: &Reporter,
) -> (JoinHandle<Result<()>>, Receiver<ArchiveJob>) {
    let pb = reporter.add_stage_bar("Size checking", jobs.len() as u64);

    let reporter = reporter.clone();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let total = jobs.len();
        let mut to_convert = 0;
        for job in jobs {
            let src_file = &job.src_file;
            pb.set_message(src_file.to_string_lossy().into_owned());
            match calc_average_size_per_file(src_file, job.format) {
                Ok(size_per_file) if size_per_file > MIN_AVERAGE_FILE_SIZE => {
                    to_convert += 1;
                    tx.send(job)?;
                }
                Err(e) => {
                    reporter.report_failure("calculating size for", src_file, e);
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

/// アーカイブを一時ディレクトリへ展開する
fn extract_archive(job: &ArchiveJob, pb: &ProgressBar) -> Result<tempfile::TempDir> {
    let work_dir = tempfile::tempdir().context("failed to create a temporary directory")?;
    archive::extract_with_progress(&job.src_file, work_dir.path(), job.format, pb.clone())?;
    Ok(work_dir)
}

fn spawn_extraction_thread(
    size_check_rx: Receiver<ArchiveJob>,
    reporter: &Reporter,
) -> (JoinHandle<Result<()>>, Receiver<ProcessingContext>) {
    let pb = reporter.add_stage_bar("Extracting", 0);

    let reporter = reporter.clone();
    let (tx, rx) = mpsc::sync_channel(MAX_PENDING_WORK_DIRS);
    let handle = std::thread::spawn(move || {
        for job in size_check_rx {
            pb.set_message(job.src_file.to_string_lossy().into_owned());
            // 1 ファイルの失敗で全体を止めず、次のファイルへ進む
            let work_dir = match extract_archive(&job, &pb) {
                Ok(work_dir) => work_dir,
                Err(e) => {
                    reporter.report_failure("extracting", &job.src_file, e);
                    continue;
                }
            };
            tx.send(ProcessingContext { work_dir, job })?;
        }
        pb.finish_with_message("Extracting completed");
        Ok(())
    });

    (handle, rx)
}

fn spawn_image_resizing_thread(
    max_height: u32,
    extraction_rx: Receiver<ProcessingContext>,
    reporter: &Reporter,
) -> (JoinHandle<Result<()>>, Receiver<ProcessingContext>) {
    let pb = reporter.add_stage_bar("Resizing images", 0);

    let reporter = reporter.clone();
    let (tx, rx) = mpsc::sync_channel(MAX_PENDING_WORK_DIRS);
    let handle = std::thread::spawn(move || {
        for ctx in extraction_rx {
            pb.set_message(ctx.job.src_file.to_string_lossy().into_owned());
            match resize_image_files(ctx.work_dir.path(), max_height, pb.clone()) {
                Ok(outcome) => {
                    if !outcome.failed.is_empty() {
                        let msg = format_conversion_failures(&ctx.job.src_file, &outcome);
                        reporter.report_error(msg);
                    }
                    // 1 枚も変換されなかったアーカイブは出力しない
                    if outcome.converted > 0 {
                        tx.send(ctx)?;
                    }
                }
                Err(e) => {
                    reporter.report_failure("resizing", &ctx.job.src_file, e);
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
/// 出力先と同じディレクトリの一時ファイルに zip を書き出し、完成してから rename で置き換える。
/// 途中で失敗・中断しても、出力先に書きかけの zip が現れたり、既存のファイルが壊れたりしない。
/// 失敗時の一時ファイルは drop 時に自動で削除される。ただし Ctrl-C などで強制終了した場合は
/// drop されずに残るため、何の残骸か分かるよう出力先の名前を含めておく
/// （`a_resized.zip` → `.a_resized.zip.XXXXXX.tmp`）。
fn create_resized_zip(ctx: &ProcessingContext, keep_mtime: bool, pb: &ProgressBar) -> Result<()> {
    let dst_file = &ctx.job.dst_file;
    let mut prefix = OsString::from(".");
    prefix.push(dst_file.file_name().unwrap_or_default());
    prefix.push(".");

    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix).suffix(".tmp");
    // 一時ファイルは既定で所有者のみ読み書き可（0600）になるため、
    // 通常のファイル作成と同じく umask に従ったパーミッションにする
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let dst_dir = parent_dir(dst_file);
    let dst_tempfile = builder
        .tempfile_in(dst_dir)
        .with_context(|| format!("failed to create a temporary file in {}", dst_dir.display()))?;

    zip_util::zip_with_progress(ctx.work_dir.path(), dst_tempfile.as_file(), pb.clone())
        .context("failed to write the zip")?;

    if keep_mtime {
        let src_file = &ctx.job.src_file;
        let src_mtime = src_file
            .metadata()
            .and_then(|metadata| metadata.modified())
            .with_context(|| {
                format!(
                    "failed to read the modification time of {}",
                    src_file.display()
                )
            })?;
        dst_tempfile
            .as_file()
            .set_modified(src_mtime)
            .context("failed to set the modification time")?;
    }

    // データをディスクに書き出してから置き換える。そうしないと、電源断などの際に
    // rename だけが反映され、既存の正しい出力が空や壊れたファイルに置き換わることがある
    dst_tempfile
        .as_file()
        .sync_all()
        .context("failed to flush the zip to disk")?;
    // PersistError は一時ファイルを保持しているので、io::Error だけを取り出して
    // その場で一時ファイルを drop（削除）する
    dst_tempfile
        .persist(dst_file)
        .map_err(|e| e.error)
        .context("failed to move the completed zip into place")?;

    Ok(())
}

fn spawn_zip_creation_thread(
    keep_mtime: bool,
    image_resizing_rx: Receiver<ProcessingContext>,
    reporter: &Reporter,
) -> JoinHandle<Result<()>> {
    let pb = reporter.add_stage_bar("Creating zip", 0);

    let reporter = reporter.clone();
    std::thread::spawn(move || {
        for ctx in image_resizing_rx {
            pb.set_message(ctx.job.dst_file.to_string_lossy().into_owned());
            if let Err(e) = create_resized_zip(&ctx, keep_mtime, &pb) {
                reporter.report_failure("creating", &ctx.job.dst_file, e);
            }
        }
        pb.finish_with_message("Creating zip completed");
        Ok(())
    })
}

pub fn run() -> Result<()> {
    run_with(Args::parse())
}

/// 解析済みの引数で処理全体を実行する（テストから CLI と同じ流れで呼べるよう分けている）
fn run_with(args: Args) -> Result<()> {
    let Args {
        output_dir,
        max_height,
        keep_mtime,
        archives,
    } = args;

    let reporter = Reporter::new();
    let jobs = plan_jobs(archives, output_dir.as_deref(), &reporter)?;

    let (size_check_handle, size_check_rx) = spawn_size_check_thread(jobs, &reporter);
    let (extraction_handle, extraction_rx) = spawn_extraction_thread(size_check_rx, &reporter);
    let (image_resizing_handle, image_resizing_rx) =
        spawn_image_resizing_thread(max_height, extraction_rx, &reporter);
    let zip_creation_handle = spawn_zip_creation_thread(keep_mtime, image_resizing_rx, &reporter);

    // 途中で return すると残りのスレッドが強制終了され、
    // 書きかけの zip が残るため、まず全スレッドの終了を待つ
    let results = [
        size_check_handle.join(),
        extraction_handle.join(),
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
        anyhow::bail!("{error_count} file(s) had errors");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use clap::Parser;

    use crate::test_util::{copy_rar_fixture, list_names, touch_files};

    #[test]
    fn test_args_rejects_zero_max_height() {
        let parse = |height: &str| super::Args::try_parse_from(["prog", "--max-height", height]);
        assert!(parse("0").is_err());
        assert_eq!(parse("1").unwrap().max_height, 1);
    }

    #[test]
    fn test_format_conversion_failures() {
        // zip 内のパスで、先頭の 5 件だけを示す
        let failed = (1..=7)
            .map(|i| {
                let file = PathBuf::from(format!("{i:03}.png"));
                (file, anyhow::anyhow!("broken"))
            })
            .collect();
        let outcome = super::ResizeOutcome {
            converted: 3,
            failed,
        };

        assert_eq!(
            super::format_conversion_failures(Path::new("book.zip"), &outcome),
            "Failed to convert 7 of 10 image(s) in book.zip (written with the originals kept):\n  \
             001.png: broken\n  002.png: broken\n  003.png: broken\n  004.png: broken\n  \
             005.png: broken\n  ... and 2 more"
        );
    }

    /// `dst_file` へ出力する、ファイルを 1 つ含む展開済みディレクトリのコンテキストを作る
    fn processing_context(src_file: PathBuf, dst_file: PathBuf) -> super::ProcessingContext {
        let work_dir = tempfile::tempdir().unwrap();
        std::fs::write(work_dir.path().join("001.webp"), b"image").unwrap();
        super::ProcessingContext {
            work_dir,
            job: super::ArchiveJob {
                src_file,
                format: super::ArchiveFormat::Zip,
                dst_file,
            },
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_create_resized_zip() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let src = touch_files(dir.path(), &["a.zip"]).remove(0);
        let old_mtime =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        std::fs::File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(old_mtime)
            .unwrap();
        let dst = dir.path().join("a_resized.zip");

        let ctx = processing_context(src, dst.clone());
        super::create_resized_zip(&ctx, true, &indicatif::ProgressBar::hidden()).unwrap();

        // 出力された zip の中身と mtime
        let archive = zip::ZipArchive::new(std::fs::File::open(&dst).unwrap()).unwrap();
        assert_eq!(archive.file_names().collect::<Vec<_>>(), ["001.webp"]);
        assert_eq!(dst.metadata().unwrap().modified().unwrap(), old_mtime);

        // パーミッションは通常のファイル作成と同じ（一時ファイルの 0600 のままにならない）
        let reference = touch_files(dir.path(), &["reference"]).remove(0);
        let mode = |path: &Path| path.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dst), mode(&reference));

        // 一時ファイルが残っていない
        assert_eq!(
            list_names(dir.path()),
            ["a.zip", "a_resized.zip", "reference"]
        );
    }

    #[test]
    fn test_create_resized_zip_failure_keeps_existing_output() {
        // 途中で失敗しても（ここでは keep_mtime 用の元ファイルが無い）、
        // 既存の出力先を壊さず、一時ファイルも残さない
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("a_resized.zip");
        std::fs::write(&dst, b"old").unwrap();

        let ctx = processing_context(dir.path().join("missing.zip"), dst.clone());
        assert!(super::create_resized_zip(&ctx, true, &indicatif::ProgressBar::hidden()).is_err());

        assert_eq!(std::fs::read(&dst).unwrap(), b"old");
        assert_eq!(list_names(dir.path()), ["a_resized.zip"]);
    }

    #[test]
    fn test_create_resized_zip_persist_failure() {
        // 置き換え（rename）に失敗しても（ここでは出力先がディレクトリ）、一時ファイルを残さない
        let dir = tempfile::tempdir().unwrap();
        let src = touch_files(dir.path(), &["a.zip"]).remove(0);
        let dst = dir.path().join("a_resized.zip");
        std::fs::create_dir(&dst).unwrap();

        let ctx = processing_context(src, dst.clone());
        let err =
            super::create_resized_zip(&ctx, false, &indicatif::ProgressBar::hidden()).unwrap_err();
        // どの段階で、どのファイルについて失敗したかがエラーに含まれる
        let msg = format!("{err:#}");
        assert!(
            msg.starts_with("failed to move the completed zip into place: "),
            "{msg}"
        );

        // err はスコープの終わりまで生きているが、それでも一時ファイルは残っていない
        // （エラーが一時ファイルを抱えたまま持ち回されない）
        assert!(dst.is_dir());
        assert_eq!(list_names(dir.path()), ["a.zip", "a_resized.zip"]);
    }

    // ---- CLI と同じ流れで、小さな zip を実際に処理する統合テスト ----

    /// 引数を解析して処理全体を実行する
    fn run_cli(args: &[&str]) -> anyhow::Result<()> {
        let args = std::iter::once("zipimage-resizer").chain(args.iter().copied());
        super::run_with(super::Args::try_parse_from(args).unwrap())
    }

    /// 圧縮が効かない（サイズの大きい）ノイズ画像を作る
    fn noise_image(width: u32, height: u32) -> image::RgbImage {
        let mut state = 0x2545_f491_u32;
        image::RgbImage::from_fn(width, height, |_, _| {
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            };
            image::Rgb([next(), next(), next()])
        })
    }

    /// `entries`（zip 内のパスと中身）を格納した zip を作る
    fn write_zip(path: &Path, entries: &[(&str, Vec<u8>)]) {
        use std::io::Write;

        let mut zw = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            zw.start_file(*name, options).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
    }

    /// 画像をエンコードしたバイト列にする
    fn encode(image: &image::RgbImage, format: image::ImageFormat) -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, format).unwrap();
        bytes.into_inner()
    }

    /// 処理対象になる（1 エントリあたりの平均が 500KB を超える）zip を作る
    fn write_large_zip(path: &Path) {
        use image::ImageFormat::{Jpeg, Png};

        let image = noise_image(700, 700);
        let png = encode(&image, Png);
        write_zip(
            path,
            &[
                ("001.png", png.clone()),
                ("001.jpg", encode(&image, Jpeg)),
                ("sub/002.png", png),
                ("readme.txt", b"not an image".to_vec()),
            ],
        );
    }

    #[test]
    fn test_run_converts_zip() {
        use image::GenericImageView;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("book.zip");
        write_large_zip(&src);
        let original = std::fs::read(&src).unwrap();

        run_cli(&[src.to_str().unwrap(), "--max-height", "100"]).unwrap();

        // 元の zip は変更されず、_resized を付けた名前で出力される
        assert_eq!(std::fs::read(&src).unwrap(), original);
        assert_eq!(list_names(dir.path()), ["book.zip", "book_resized.zip"]);

        // 同名の画像は両方残り（ページ順も保たれ）、画像でないファイルはそのまま
        let file = std::fs::File::open(dir.path().join("book_resized.zip")).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<_> = archive.file_names().map(str::to_owned).collect();
        assert_eq!(
            names,
            [
                "001.jpg.webp",
                "001.png.webp",
                "readme.txt",
                "sub/",
                "sub/002.webp"
            ]
        );
        for name in ["001.jpg.webp", "001.png.webp", "sub/002.webp"] {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut archive.by_name(name).unwrap(), &mut bytes).unwrap();
            let image = image::load_from_memory(&bytes).unwrap();
            assert_eq!(image.dimensions(), (100, 100), "{name}");
        }
    }

    #[test]
    fn test_run_skips_small_zip() {
        // 1 エントリあたりの平均が小さい zip は処理せず、出力も作らない
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("small.zip");
        write_zip(
            &src,
            &[(
                "001.png",
                encode(&noise_image(8, 8), image::ImageFormat::Png),
            )],
        );

        run_cli(&[src.to_str().unwrap()]).unwrap();

        assert_eq!(list_names(dir.path()), ["small.zip"]);
    }

    #[test]
    fn test_pipeline_converts_rar() {
        // 処理対象になる（平均 500KB 超の）RAR はフィクスチャとして大きすぎるので、
        // サイズチェックの後段（展開 → リサイズ → zip 作成）に直接流す
        let dir = tempfile::tempdir().unwrap();
        let src = copy_rar_fixture(dir.path(), "book.rar");
        let reporter = super::Reporter::new();
        let jobs = super::plan_jobs(vec![src], None, &reporter).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        for job in jobs {
            tx.send(job).unwrap();
        }
        drop(tx);
        let (extraction, rx) = super::spawn_extraction_thread(rx, &reporter);
        let (resizing, rx) = super::spawn_image_resizing_thread(1800, rx, &reporter);
        let creation = super::spawn_zip_creation_thread(false, rx, &reporter);
        for handle in [extraction, resizing, creation] {
            handle.join().unwrap().unwrap();
        }
        assert_eq!(reporter.error_count(), 0);

        assert_eq!(list_names(dir.path()), ["book.rar", "book_resized.zip"]);
        let file = std::fs::File::open(dir.path().join("book_resized.zip")).unwrap();
        let archive = zip::ZipArchive::new(file).unwrap();
        assert_eq!(
            archive.file_names().collect::<Vec<_>>(),
            ["001.webp", "日本語.txt", "sub/", "sub/002.txt"]
        );
    }

    #[test]
    fn test_run_refuses_to_overwrite_input() {
        // -d に入力元と同じディレクトリを指定しても、原本を壊さずにエラーで終わる
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("book.zip");
        write_large_zip(&src);
        let original = std::fs::read(&src).unwrap();

        let result = run_cli(&[src.to_str().unwrap(), "-d", dir.path().to_str().unwrap()]);

        assert!(result.is_err());
        assert_eq!(std::fs::read(&src).unwrap(), original);
        assert_eq!(list_names(dir.path()), ["book.zip"]);
    }
}
