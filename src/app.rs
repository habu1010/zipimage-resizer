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
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::archive::{self, ArchiveFormat};
use crate::convert::{resize_image_files, ResizeOutcome};
use crate::fs_util::{self, parent_dir};
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

/// 処理するアーカイブ（zip / RAR）とその出力先（zip）の組
///
/// 形式と出力先は `plan_jobs` で検証済みのものを一度だけ決め、後段はそれをそのまま使う。
#[derive(Debug, PartialEq)]
struct ArchiveJob {
    src_file: PathBuf,
    format: ArchiveFormat,
    dst_file: PathBuf,
}

struct ProcessingContext {
    work_dir: tempfile::TempDir,
    job: ArchiveJob,
}

/// 変換に失敗した画像を、アーカイブごとに 1 件のエラーとしてまとめた文面にする
///
/// ディスクフルなどで大量に失敗しても表示が溢れないよう、個別のファイルは先頭の数件だけ示す。
/// パスは `ResizeOutcome::failed` のアーカイブ内のパスをそのまま示す。
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

/// 出力先のパスを決める
///
/// 出力は常に zip なので、RAR 用の拡張子（`.rar` / `.cbr`）は中身によらず `.zip` にする。
/// zip 用の拡張子（`.zip` / `.cbz`）は、中身が zip ならそのまま残し、RAR なら `.zip` にする。
/// RAR の入力のそれ以外の拡張子（`Vol.1` の `.1` など）は名前の一部かもしれないので、
/// 残して `.zip` を足す（zip の入力は従来どおりそのまま）。
fn determine_output_path(path: &Path, format: ArchiveFormat, output_dir: Option<&Path>) -> PathBuf {
    let dst = match output_dir {
        Some(output_dir) => output_dir.join(path.file_name().unwrap()),
        None => append_suffix_to_filename(path, "_resized"),
    };
    let has_extension = |exts: &[&str]| {
        dst.extension()
            .is_some_and(|ext| exts.iter().any(|e| ext.eq_ignore_ascii_case(e)))
    };
    let is_rar = format == ArchiveFormat::Rar;
    if has_extension(&["rar", "cbr"]) || (is_rar && has_extension(&["zip", "cbz"])) {
        dst.with_extension("zip")
    } else if is_rar {
        let mut dst = dst.into_os_string();
        dst.push(".zip");
        dst.into()
    } else {
        dst
    }
}

/// `-d` に入力元と同じディレクトリを指定したときに、拡張子を変えた出力先（`a.rar` → `a.zip`）が
/// 既に存在するかどうか
///
/// 入力や既存の出力がシンボリックリンクの場合も考え、リンク自体の置き場所で比べる
/// （入力はリンク先のディレクトリも入力元とみなす）。
fn output_exists_in_input_dir(src: &Path, canonical_src: &Path, dst: &Path) -> bool {
    let Ok(dst_dir) = parent_dir(dst).canonicalize() else {
        return false;
    };
    let in_input_dir = Some(dst_dir.as_path()) == canonical_src.parent()
        || parent_dir(src)
            .canonicalize()
            .is_ok_and(|src_dir| src_dir == dst_dir);
    in_input_dir && dst.symlink_metadata().is_ok()
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

fn calc_average_size_per_file(path: &Path, format: ArchiveFormat) -> Result<u64> {
    let file_size = path.metadata()?.len();
    let file_count = archive::get_file_count(path, format)? as u64;
    // 空のアーカイブは処理対象外とする（ゼロ除算の回避）
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
        // テストでは進捗表示を描画しない（cargo test の出力の途中にバーが描かれるため）
        let mp = if cfg!(test) {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        } else {
            MultiProgress::new()
        };
        Self {
            mp,
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
/// - `-d` に入力元と同じディレクトリを指定し、拡張子を変えた出力先（`a.rar` → `a.zip`）が
///   既に存在する場合は処理しない（入力に含まれていなくても、同じ本の zip 版の原本かもしれないため）
///
/// 処理しない入力は `Reporter` で報告し、残りの処理は続ける。
fn plan_jobs(
    archives: Vec<PathBuf>,
    output_dir: Option<&Path>,
    reporter: &Reporter,
) -> Vec<ArchiveJob> {
    let mut inputs = Vec::new();
    let mut input_paths = HashSet::new();
    for src in archives {
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
        if !input_paths.insert(canonical.clone()) {
            continue;
        }
        match ArchiveFormat::detect(&canonical) {
            Ok(format) => inputs.push((src, canonical, format)),
            Err(e) => reporter.report_failure("reading", &src, e),
        }
    }

    let mut outputs: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut jobs = Vec::new();
    for (src, canonical, format) in inputs {
        let dst = determine_output_path(&src, format, output_dir);
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
        // 出力先が入力自身の場合は上で除いているので、ここで見つかるのは拡張子を .zip にした
        // 出力先（入力がシンボリックリンクなら、リンク先と名前が違う場合も含む）
        if output_dir.is_some() && output_exists_in_input_dir(&src, &canonical, &dst) {
            reporter.report_error(format!(
                "Skipping {}: output {} already exists in the input directory",
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
                jobs.push(ArchiveJob {
                    src_file: src,
                    format,
                    dst_file: dst,
                });
            }
        }
    }

    jobs
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
    let jobs = plan_jobs(archives, output_dir.as_deref(), &reporter);

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
        let parse = |height: &str| super::Args::try_parse_from(["prog", "--max-height", height]);
        assert!(parse("0").is_err());
        assert_eq!(parse("1").unwrap().max_height, 1);
    }

    #[test]
    fn test_determine_output_path() {
        use super::ArchiveFormat::{Rar, Zip};

        let src = Path::new("/path1/path2/test.cbz");

        // 出力先ディレクトリ指定時はファイル名だけを引き継ぐ
        let path = super::determine_output_path(src, Zip, Some(Path::new("/out")));
        assert_eq!(path, Path::new("/out/test.cbz"));

        // 未指定時は元ファイル名に _resized を付ける
        let path = super::determine_output_path(src, Zip, None);
        assert_eq!(path, Path::new("/path1/path2/test_resized.cbz"));

        // RAR の出力は zip なので、拡張子を .zip にする
        for src in [
            "/path1/path2/test.rar",
            "/path1/path2/test.cbr",
            "/path1/path2/test.RAR",
        ] {
            let src = Path::new(src);
            let path = super::determine_output_path(src, Rar, Some(Path::new("/out")));
            assert_eq!(path, Path::new("/out/test.zip"));
            let path = super::determine_output_path(src, Rar, None);
            assert_eq!(path, Path::new("/path1/path2/test_resized.zip"));
        }

        // .rar / .cbr 以外の拡張子は名前の一部として残す（Vol.1 と Vol.2 が同じ出力にならない）
        let src = Path::new("/path1/path2/Vol.1");
        let path = super::determine_output_path(src, Rar, Some(Path::new("/out")));
        assert_eq!(path, Path::new("/out/Vol.1.zip"));
        let path = super::determine_output_path(src, Rar, None);
        assert_eq!(path, Path::new("/path1/path2/Vol_resized.1.zip"));

        // 中身が RAR で拡張子が .zip / .cbz の場合も、.zip を重ねない
        let path = super::determine_output_path(Path::new("/p/test.cbz"), Rar, None);
        assert_eq!(path, Path::new("/p/test_resized.zip"));

        // 中身が zip で拡張子が .cbr の場合も、出力は zip なので拡張子を .zip にする
        let path = super::determine_output_path(Path::new("/p/test.cbr"), Zip, None);
        assert_eq!(path, Path::new("/p/test_resized.zip"));
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
                super::ArchiveJob {
                    src_file: zipfiles[0].clone(),
                    format: super::ArchiveFormat::Zip,
                    dst_file: out.join("a.zip"),
                },
                super::ArchiveJob {
                    src_file: zipfiles[1].clone(),
                    format: super::ArchiveFormat::Zip,
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

    /// `testdata/basic.rar` を `dir` に `dest_name` という名前でコピーし、そのパスを返す
    fn copy_rar_fixture(dir: &Path, dest_name: &str) -> PathBuf {
        let path = dir.join(dest_name);
        std::fs::copy(crate::test_util::fixture("basic.rar"), &path).unwrap();
        path
    }

    #[test]
    fn test_plan_jobs_rar() {
        // 形式は中身で判定し、RAR の出力先は拡張子を .zip にする
        let dir = tempfile::tempdir().unwrap();
        let src = copy_rar_fixture(dir.path(), "a.cbr");

        let reporter = super::Reporter::new();
        let jobs = super::plan_jobs(vec![src.clone()], None, &reporter);
        assert_eq!(reporter.error_count(), 0);
        assert_eq!(
            jobs,
            [super::ArchiveJob {
                src_file: src,
                format: super::ArchiveFormat::Rar,
                dst_file: dir.path().join("a_resized.zip"),
            }]
        );
    }

    #[test]
    fn test_plan_jobs_rar_output_exists_in_input_dir() {
        // -d に入力元と同じディレクトリを指定すると、a.rar の出力先は隣の a.zip になる。
        // 入力に含まれていなくても同じ本の zip 版の原本かもしれないので、上書きしない
        let dir = tempfile::tempdir().unwrap();
        let src = copy_rar_fixture(dir.path(), "a.rar");
        let other = copy_rar_fixture(dir.path(), "b.rar");
        touch_files(dir.path(), &["a.zip"]);

        let (srcs, errors) = plan(
            vec![src.clone(), other.clone()],
            Some(dir.path().to_path_buf()),
        );
        assert_eq!(srcs, [other]);
        assert_eq!(errors, 1);

        // 中身が zip の c.cbr も出力先が c.zip になるので、同様に上書きしない
        let cbr = touch_files(dir.path(), &["c.cbr", "c.zip"]).remove(0);
        let (srcs, errors) = plan(vec![cbr], Some(dir.path().to_path_buf()));
        assert!(srcs.is_empty());
        assert_eq!(errors, 1);

        // 別のディレクトリへの出力では、既存の出力は従来どおり上書きする
        let out = dir.path().join("out");
        touch_files(&out, &["a.zip"]);
        let (srcs, errors) = plan(vec![src], Some(out));
        assert_eq!(srcs.len(), 1);
        assert_eq!(errors, 0);
    }

    #[test]
    #[cfg(unix)]
    fn test_plan_jobs_rar_output_exists_next_to_symlink() {
        // 入力がシンボリックリンクでも、リンクのあるディレクトリの a.zip を上書きしない
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let target = copy_rar_fixture(&real, "a.rar");
        let books = dir.path().join("books");
        touch_files(&books, &["a.zip"]);
        let link = books.join("a.rar");
        std::os::unix::fs::symlink(target, &link).unwrap();

        let (srcs, errors) = plan(vec![link], Some(books));
        assert!(srcs.is_empty());
        assert_eq!(errors, 1);
    }

    #[test]
    #[cfg(unix)]
    fn test_plan_jobs_rar_symlink_named_zip() {
        // 中身が RAR の real/a.rar を books/a.zip というリンクで渡し、-d にリンク先の
        // ディレクトリを指定しても、real/a.zip を上書きしない
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let target = copy_rar_fixture(&real, "a.rar");
        touch_files(&real, &["a.zip"]);
        let books = dir.path().join("books");
        std::fs::create_dir(&books).unwrap();
        let link = books.join("a.zip");
        std::os::unix::fs::symlink(target, &link).unwrap();

        let (srcs, errors) = plan(vec![link], Some(real));
        assert!(srcs.is_empty());
        assert_eq!(errors, 1);
    }

    #[test]
    #[cfg(unix)]
    fn test_plan_jobs_rar_existing_output_is_symlink() {
        // 入力元にある a.zip が別の場所へのシンボリックリンクでも、上書きしない
        let dir = tempfile::tempdir().unwrap();
        let target = touch_files(dir.path(), &["library/a.zip"]).remove(0);
        let books = dir.path().join("books");
        std::fs::create_dir(&books).unwrap();
        let src = copy_rar_fixture(&books, "a.rar");
        std::os::unix::fs::symlink(target, books.join("a.zip")).unwrap();

        let (srcs, errors) = plan(vec![src], Some(books));
        assert!(srcs.is_empty());
        assert_eq!(errors, 1);
    }

    #[test]
    fn test_plan_jobs_zip_and_rar_same_output() {
        // a.zip と a.rar はどちらも出力先が a_resized.zip になるので、先に指定した方だけを処理する
        let dir = tempfile::tempdir().unwrap();
        let zip = touch_files(dir.path(), &["a.zip"]).remove(0);
        let rar = copy_rar_fixture(dir.path(), "a.rar");

        let (srcs, errors) = plan(vec![zip.clone(), rar], None);
        assert_eq!(srcs, [zip]);
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
        let jobs = super::plan_jobs(vec![src], None, &reporter);

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
