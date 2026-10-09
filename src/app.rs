use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver},
        Arc,
    },
    thread::JoinHandle,
};

use anyhow::{Context, Result};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::convert::{resize_image_files, ResizeOutcome};
use crate::fs_util::{self, parent_dir};
use crate::zip_util;

/// 処理対象にする zip の、1 ファイルあたりの平均サイズの下限
///
/// 平均がこれ以下の zip は、既に十分小さいとみなして処理しない。
const MIN_AVERAGE_FILE_SIZE: u64 = 500 * 1024; // 500KB

/// 展開済み zip（一時ディレクトリ）を受け渡すチャネル 1 本あたりに溜めておける数
///
/// 上限がないと、下流の段が遅いときに展開済みの一時ディレクトリが際限なく溜まり、
/// ディスクを圧迫する。上限はチャネルごとなので、同時に存在する一時ディレクトリは
/// 2 本のチャネル（展開 → リサイズ、リサイズ → zip 作成）に溜まる分と、各段で処理中の
/// 分を合わせて、最大で `2 * MAX_PENDING_WORK_DIRS + 3` 個になる。
const MAX_PENDING_WORK_DIRS: usize = 3;

// 各引数の説明は --help にそのまま表示されるユーザー向けの文言なので、英語で書き、
// 説明を二重に持たないよう日本語の doc コメントは付けない。
#[derive(Parser, Debug)]
#[clap(about = "Shrink the images in zip archives by re-encoding them to WebP")]
struct Args {
    #[clap(
        short = 'd',
        long,
        help = "Output directory. If omitted, each output is written next to its input \
                with \"_resized\" added before the extension (e.g. a.zip -> a_resized.zip)"
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

    #[clap(help = "Zip files to process")]
    zipfiles: Vec<PathBuf>,
}

/// 処理する zip とその出力先の組
///
/// 出力先は `plan_jobs` で検証済みのものを一度だけ決め、後段はそれをそのまま使う。
#[derive(Debug, PartialEq)]
struct ZipJob {
    src_file: PathBuf,
    dst_file: PathBuf,
}

struct ProcessingContext {
    work_dir: tempfile::TempDir,
    job: ZipJob,
}

/// 変換に失敗した画像を、zip ごとに 1 件のエラーとしてまとめた文面にする
///
/// ディスクフルなどで大量に失敗しても表示が溢れないよう、個別のファイルは先頭の数件だけ示す。
/// パスは `ResizeOutcome::failed` の zip 内のパスをそのまま示す。
fn format_conversion_failures(ctx: &ProcessingContext, outcome: &ResizeOutcome) -> String {
    use std::fmt::Write as _;

    const MAX_LISTED: usize = 5;

    let failed = &outcome.failed;
    let total = outcome.converted + failed.len();
    let mut msg = format!(
        "Failed to convert {} of {total} image(s) in {}",
        failed.len(),
        ctx.job.src_file.display()
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

fn append_suffix_to_filename<P: AsRef<Path>>(path: P, suffix: &str) -> PathBuf {
    let path = path.as_ref();
    let mut stem = path.file_stem().unwrap().to_os_string();
    let ext = path.extension().unwrap_or(std::ffi::OsStr::new(""));

    stem.push(suffix);
    path.with_file_name(stem).with_extension(ext)
}

fn determine_output_path(path: &Path, output_dir: Option<&Path>) -> PathBuf {
    match output_dir {
        Some(output_dir) => output_dir.join(path.file_name().unwrap()),
        None => append_suffix_to_filename(path, "_resized"),
    }
}

/// 同じファイルかどうかを比較できるよう、パスを正規化する
///
/// 存在しないパス（これから作る出力先など）は、親ディレクトリだけを正規化する。
fn normalize_path(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize().or_else(|_| {
        let parent = parent_dir(path).canonicalize()?;
        Ok(parent.join(path.file_name().unwrap_or_default()))
    })
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

    /// パイプラインの段ごとの進捗表示を追加する（`label` は表示の先頭に付く段の名前）
    fn add_stage_bar(&self, label: &str, len: u64) -> ProgressBar {
        let style = ProgressStyle::with_template(
            "{prefix} [{bar:20.cyan/blue}] {pos}/{len} ETA:{eta} {msg}",
        )
        .unwrap()
        .progress_chars("#>-");
        let pb = ProgressBar::new(len)
            .with_style(style)
            .with_prefix(label.to_string());
        self.mp.add(pb)
    }

    /// エラーを stderr に出力して件数を数える
    ///
    /// プログレスバーの描画を一時停止しないと表示が崩れるため、`suspend` 経由で出力する。
    fn report_error(&self, msg: String) {
        self.error_count.fetch_add(1, Ordering::Relaxed);
        self.mp.suspend(|| eprintln!("{msg}"));
    }

    /// `path` についての処理（`action`）の失敗を、原因まで含めて報告する
    fn report_failure(&self, action: &str, path: &Path, e: impl std::fmt::Display) {
        self.report_error(format!("Error {action} {}: {e:#}", path.display()));
    }

    fn error_count(&self) -> usize {
        self.error_count.load(Ordering::Relaxed)
    }
}

/// 各入力の出力先を決め、原本や他の出力を上書きするものを除いた処理対象を返す
///
/// 例えば `-d` に入力元と同じディレクトリを指定すると出力先が入力ファイル自身になり、
/// 原本が壊れる。展開・変換を始める前に、正規化したパスで以下を検証する。
///
/// - 出力先が入力ファイル（自身でも他の入力でも）と重なる入力は処理しない
/// - 出力先が重なる場合は、先に指定された入力だけを処理する
/// - 同じファイルが重複して指定された場合は、1 つにまとめる
///
/// 処理しない入力は `Reporter` で報告し、残りの処理は続ける。
fn plan_jobs(
    zipfiles: Vec<PathBuf>,
    output_dir: Option<&Path>,
    reporter: &Reporter,
) -> Vec<ZipJob> {
    let mut inputs = Vec::new();
    let mut input_paths = HashSet::new();
    for src in zipfiles {
        let canonical = match src.canonicalize() {
            Ok(canonical) => canonical,
            Err(e) => {
                reporter.report_failure("reading", &src, e);
                continue;
            }
        };
        if !canonical.is_file() {
            reporter.report_error(format!("Skipping {}: not a file", src.display()));
            continue;
        }
        if input_paths.insert(canonical) {
            inputs.push(src);
        }
    }

    let mut outputs: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut jobs = Vec::new();
    for src in inputs {
        let dst = determine_output_path(&src, output_dir);
        let normalized = match normalize_path(&dst) {
            Ok(normalized) => normalized,
            Err(e) => {
                reporter.report_failure("resolving", &dst, e);
                continue;
            }
        };

        if input_paths.contains(&normalized) {
            reporter.report_error(format!(
                "Skipping {}: output {} would overwrite an input file",
                src.display(),
                dst.display()
            ));
            continue;
        }
        match outputs.entry(normalized) {
            Entry::Occupied(other) => reporter.report_error(format!(
                "Skipping {}: output {} is also the output of {}",
                src.display(),
                dst.display(),
                other.get().display()
            )),
            Entry::Vacant(entry) => {
                entry.insert(src.clone());
                jobs.push(ZipJob {
                    src_file: src,
                    dst_file: dst,
                });
            }
        }
    }

    jobs
}

fn spawn_size_check_thread(
    jobs: Vec<ZipJob>,
    reporter: &Reporter,
) -> (JoinHandle<Result<()>>, Receiver<ZipJob>) {
    let pb = reporter.add_stage_bar("Size checking", jobs.len() as u64);

    let reporter = reporter.clone();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let total = jobs.len();
        let mut to_convert = 0;
        for job in jobs {
            let zipfile = &job.src_file;
            pb.set_message(zipfile.to_string_lossy().into_owned());
            match calc_average_size_per_file(zipfile) {
                Ok(size_per_file) if size_per_file > MIN_AVERAGE_FILE_SIZE => {
                    to_convert += 1;
                    tx.send(job)?;
                }
                Err(e) => {
                    reporter.report_failure("calculating size for", zipfile, e);
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

/// zip を一時ディレクトリへ展開する
fn extract_zip(src_file: &Path, pb: &ProgressBar) -> Result<tempfile::TempDir> {
    let work_dir = tempfile::tempdir().context("failed to create a temporary directory")?;
    zip_util::unzip_with_progress(src_file, work_dir.path(), pb.clone())?;
    Ok(work_dir)
}

fn spawn_zip_extraction_thread(
    size_check_rx: Receiver<ZipJob>,
    reporter: &Reporter,
) -> (JoinHandle<Result<()>>, Receiver<ProcessingContext>) {
    let pb = reporter.add_stage_bar("Extracting", 0);

    let reporter = reporter.clone();
    let (tx, rx) = mpsc::sync_channel(MAX_PENDING_WORK_DIRS);
    let handle = std::thread::spawn(move || {
        for job in size_check_rx {
            pb.set_message(job.src_file.to_string_lossy().into_owned());
            // 1 ファイルの失敗で全体を止めず、次のファイルへ進む
            let work_dir = match extract_zip(&job.src_file, &pb) {
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
    zip_extraction_rx: Receiver<ProcessingContext>,
    reporter: &Reporter,
) -> (JoinHandle<Result<()>>, Receiver<ProcessingContext>) {
    let pb = reporter.add_stage_bar("Resizing images", 0);

    let reporter = reporter.clone();
    let (tx, rx) = mpsc::sync_channel(MAX_PENDING_WORK_DIRS);
    let handle = std::thread::spawn(move || {
        for ctx in zip_extraction_rx {
            pb.set_message(ctx.job.src_file.to_string_lossy().into_owned());
            let files: Vec<_> = match fs_util::scan_regular_files(ctx.work_dir.path()).collect() {
                Ok(files) => files,
                Err(e) => {
                    reporter.report_failure("scanning", &ctx.job.src_file, e);
                    continue;
                }
            };
            match resize_image_files(&files, ctx.work_dir.path(), max_height, pb.clone()) {
                Ok(outcome) => {
                    if !outcome.failed.is_empty() {
                        reporter.report_error(format_conversion_failures(&ctx, &outcome));
                    }
                    // 1 枚も変換されなかった zip は出力しない
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
    let Args {
        output_dir,
        max_height,
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
    let jobs = plan_jobs(zipfiles, output_dir.as_deref(), &reporter);

    let (size_check_handle, size_check_rx) = spawn_size_check_thread(jobs, &reporter);
    let (zip_extraction_handle, zip_extraction_rx) =
        spawn_zip_extraction_thread(size_check_rx, &reporter);
    let (image_resizing_handle, image_resizing_rx) =
        spawn_image_resizing_thread(max_height, zip_extraction_rx, &reporter);
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
        anyhow::bail!("{error_count} file(s) had errors");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::test_util::{list_names, touch_files};

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
    fn test_args_rejects_zero_max_height() {
        use clap::Parser;

        let parse = |height: &str| super::Args::try_parse_from(["prog", "--max-height", height]);
        assert!(parse("0").is_err());
        assert_eq!(parse("1").unwrap().max_height, 1);
    }

    #[test]
    fn test_determine_output_path() {
        let src = Path::new("/path1/path2/test.zip");

        // 出力先ディレクトリ指定時はファイル名だけを引き継ぐ
        let path = super::determine_output_path(src, Some(Path::new("/out")));
        assert_eq!(path, Path::new("/out/test.zip"));

        // 未指定時は元ファイル名に _resized を付ける
        let path = super::determine_output_path(src, None);
        assert_eq!(path, Path::new("/path1/path2/test_resized.zip"));
    }

    #[test]
    fn test_format_conversion_failures() {
        // zip 内のパスで、先頭の 5 件だけを示す
        let ctx = processing_context(PathBuf::from("book.zip"), PathBuf::from("out.zip"));
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
            super::format_conversion_failures(&ctx, &outcome),
            "Failed to convert 7 of 10 image(s) in book.zip (written with the originals kept):\n  \
             001.png: broken\n  002.png: broken\n  003.png: broken\n  004.png: broken\n  \
             005.png: broken\n  ... and 2 more"
        );
    }

    /// `plan_jobs` を実行し、処理対象の入力とエラー件数を返す
    fn plan(zipfiles: Vec<PathBuf>, output_dir: Option<PathBuf>) -> (Vec<PathBuf>, usize) {
        let reporter = super::Reporter::new();
        let jobs = super::plan_jobs(zipfiles, output_dir.as_deref(), &reporter);
        let srcs = jobs.into_iter().map(|job| job.src_file).collect();
        (srcs, reporter.error_count())
    }

    #[test]
    fn test_plan_jobs_ok() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let zipfiles = touch_files(dir.path(), &["a.zip", "b.zip"]);

        let reporter = super::Reporter::new();
        let jobs = super::plan_jobs(zipfiles.clone(), Some(&out), &reporter);
        assert_eq!(reporter.error_count(), 0);
        assert_eq!(
            jobs,
            [
                super::ZipJob {
                    src_file: zipfiles[0].clone(),
                    dst_file: out.join("a.zip"),
                },
                super::ZipJob {
                    src_file: zipfiles[1].clone(),
                    dst_file: out.join("b.zip"),
                },
            ]
        );
    }

    #[test]
    fn test_plan_jobs_output_dir_is_input_dir() {
        // -d に入力元と同じディレクトリを指定すると、原本を上書きしてしまう
        let dir = tempfile::tempdir().unwrap();
        let zipfiles = touch_files(dir.path(), &["a.zip"]);

        let (srcs, errors) = plan(zipfiles, Some(dir.path().join(".")));
        assert!(srcs.is_empty());
        assert_eq!(errors, 1);
    }

    #[test]
    fn test_plan_jobs_output_is_another_input() {
        // 再実行時に前回の出力が glob に含まれた場合など。a.zip の出力先 a_resized.zip が
        // 別の入力と重なるので a.zip だけを除き、残りは処理を続ける
        let dir = tempfile::tempdir().unwrap();
        let zipfiles = touch_files(dir.path(), &["a.zip", "a_resized.zip", "b.zip"]);

        let (srcs, errors) = plan(zipfiles.clone(), None);
        assert_eq!(srcs, &zipfiles[1..]);
        assert_eq!(errors, 1);
    }

    #[test]
    fn test_plan_jobs_same_output() {
        // -d 指定時、別ディレクトリの同名 zip は同じ出力先になるので、先に指定した方だけを処理する
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let zipfiles = touch_files(dir.path(), &["x/a.zip", "y/a.zip"]);

        let (srcs, errors) = plan(zipfiles.clone(), Some(out));
        assert_eq!(srcs, &zipfiles[..1]);
        assert_eq!(errors, 1);
    }

    #[test]
    fn test_plan_jobs_duplicate_input() {
        // 同じファイルを別の書き方で重複して指定しても、エラーにせず 1 つにまとめる
        let dir = tempfile::tempdir().unwrap();
        let zipfiles = touch_files(dir.path(), &["a.zip"]);
        let duplicate = dir.path().join(".").join("a.zip");

        let (srcs, errors) = plan(vec![zipfiles[0].clone(), duplicate], None);
        assert_eq!(srcs, zipfiles);
        assert_eq!(errors, 0);
    }

    #[test]
    fn test_plan_jobs_invalid_input() {
        // 存在しない入力やディレクトリは報告して除く（panic しない）
        let dir = tempfile::tempdir().unwrap();
        let zipfiles = vec![
            dir.path().join("missing.zip"),
            dir.path().join("."),
            PathBuf::from("/"),
        ];

        let (srcs, errors) = plan(zipfiles, None);
        assert!(srcs.is_empty());
        assert_eq!(errors, 3);
    }

    /// `dst_file` へ出力する、ファイルを 1 つ含む展開済みディレクトリのコンテキストを作る
    fn processing_context(src_file: PathBuf, dst_file: PathBuf) -> super::ProcessingContext {
        let work_dir = tempfile::tempdir().unwrap();
        std::fs::write(work_dir.path().join("001.webp"), b"image").unwrap();
        super::ProcessingContext {
            work_dir,
            job: super::ZipJob { src_file, dst_file },
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
}
