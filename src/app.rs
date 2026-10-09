use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    ffi::{OsStr, OsString},
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

/// `src` を WebP に変換し、同じディレクトリの一時ファイルに書き出す
///
/// 書き出しに成功してから `src` を削除し、一時ファイルのパスを返す。
/// 変換対象外のファイルは `None` を返す。
/// 最終的なファイル名は、全ファイルの変換後に `rename_to_final_names` で決める。
fn resize_image_file_webp(src: &Path, min_height: u32) -> Result<Option<PathBuf>> {
    use image::ImageFormat::*;

    const MAX_WEBP_SIZE: usize = 1024 * 1024; // 1MB

    let reader = image::ImageReader::open(src)?.with_guessed_format()?;
    if reader.format() == Some(WebP) && src.metadata()?.len() < MAX_WEBP_SIZE as u64 {
        return Ok(None);
    }

    let Ok(image) = reader.decode() else {
        return Ok(None);
    };

    let webp = image.reduce_size(min_height).to_webp(MAX_WEBP_SIZE)?;

    // 既存のファイルと衝突しない一意な名前で作成する（失敗時は自動で削除される）
    let mut tmp = tempfile::Builder::new()
        .suffix(".webp.tmp")
        .tempfile_in(parent_dir(src))?;
    tmp.write_all(webp.as_ref())?;
    let tmp = tmp.into_temp_path().keep()?;

    std::fs::remove_file(src)?;

    Ok(Some(tmp))
}

fn parent_dir(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new("."))
}

/// 衝突判定用のキー（大文字小文字を区別しないファイルシステムも考慮する）
fn name_key(name: &OsStr) -> String {
    name.to_string_lossy().to_lowercase()
}

/// 衝突がない場合の変換後のファイル名（`001.png` → `001.webp`）
fn natural_webp_name(src: &Path) -> OsString {
    src.with_extension("webp")
        .file_name()
        .unwrap()
        .to_os_string()
}

/// 衝突する場合の変換後のファイル名の候補を優先順に返す
///
/// `001.png` → `001.png.webp`, `001.png.1.webp`, `001.png.2.webp`, …
fn fallback_webp_names(src: &Path) -> impl Iterator<Item = OsString> {
    let name = src.file_name().unwrap().to_os_string();

    (0..).map(move |n| {
        let mut candidate = name.clone();
        if n > 0 {
            candidate.push(format!(".{n}"));
        }
        candidate.push(".webp");
        candidate
    })
}

/// 一時ファイルに書き出した変換結果を、既存のファイルと衝突しない最終的な名前へ rename する
///
/// 単純に拡張子を `.webp` に変えると、`001.jpg` と `001.png` が同じ `001.webp` に
/// なったり、変換対象外の既存の `001.webp` を上書きしたりして画像が失われる。
/// 衝突する場合は元の拡張子を残した名前（`001.png.webp`）にする。
///
/// 変換されたファイル同士が衝突する場合は、全員を元の拡張子を残した名前にする。
/// 片方だけを `001.webp` にすると、`001.png.webp` < `001.webp` の順に並んで
/// ページ順が逆転するため。
///
/// 実際に変換されたファイルだけを対象に、ディレクトリ内に実在する名前
/// （ディレクトリも含む）と照合して決める。パス順に割り当てるので、
/// 結果は並列処理の実行順に左右されない。
fn rename_to_final_names(mut converted: Vec<(PathBuf, PathBuf)>) -> Result<()> {
    converted.sort();

    // 変換後の名前同士の衝突を数える
    let mut natural_counts: HashMap<(&Path, String), usize> = HashMap::new();
    for (src, _) in &converted {
        let key = (parent_dir(src), name_key(&natural_webp_name(src)));
        *natural_counts.entry(key).or_default() += 1;
    }

    let pending: HashSet<&Path> = converted.iter().map(|(_, tmp)| tmp.as_path()).collect();
    // ディレクトリごとの使用済みの名前
    let mut taken: HashMap<&Path, HashSet<String>> = HashMap::new();

    for (src, tmp) in &converted {
        let dir = parent_dir(src);
        let names = match taken.entry(dir) {
            Entry::Occupied(e) => e.into_mut(),
            // 未 rename の一時ファイルは使用済みとして扱わない
            Entry::Vacant(e) => e.insert(existing_names(dir, &pending)?),
        };

        let natural = natural_webp_name(src);
        let key = (dir, name_key(&natural));
        let name = if natural_counts[&key] == 1 && !names.contains(&key.1) {
            natural
        } else {
            fallback_webp_names(src)
                .find(|candidate| !names.contains(&name_key(candidate)))
                .unwrap()
        };
        names.insert(name_key(&name));
        std::fs::rename(tmp, dir.join(name))?;
    }

    Ok(())
}

/// `dir` 直下にある名前（ディレクトリも含む）の衝突判定用キーを集める（`exclude` は除く）
fn existing_names(dir: &Path, exclude: &HashSet<&Path>) -> Result<HashSet<String>> {
    let mut names = HashSet::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if !exclude.contains(path.as_path()) {
            names.insert(name_key(path.file_name().unwrap()));
        }
    }
    Ok(names)
}

fn resize_image_files(files: &[PathBuf], min_height: u32, pb: ProgressBar) -> Result<usize> {
    pb.set_position(0);
    pb.set_length(files.len() as u64);

    let converted: Vec<_> = files
        .par_iter()
        .progress_with(pb)
        .map(|src| Ok(resize_image_file_webp(src, min_height)?.map(|tmp| (src.clone(), tmp))))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

    let count = converted.len();
    rename_to_final_names(converted)?;

    Ok(count)
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
    use std::path::{Path, PathBuf};

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

    #[test]
    fn test_fallback_webp_names() {
        let candidates: Vec<_> = super::fallback_webp_names(Path::new("/d/001.png"))
            .take(3)
            .collect();
        assert_eq!(
            candidates,
            ["001.png.webp", "001.png.1.webp", "001.png.2.webp"]
        );
    }

    /// 小さい画像を書き出す（形式は拡張子で決まる。WebP は変換対象外になるサイズ）
    fn write_image(path: &Path) {
        use crate::image_util::ToWebPMemory;

        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([255, 0, 0]));
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("webp"))
        {
            let webp = image::DynamicImage::ImageRgb8(img).to_webp(1024).unwrap();
            std::fs::write(path, webp.as_ref()).unwrap();
        } else {
            img.save(path).unwrap();
        }
    }

    /// ディレクトリ直下のエントリ名をソートして返す
    fn list_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// `files` を作成して変換し、変換件数と変換後のエントリ名を返す
    ///
    /// 末尾が `/` ならディレクトリ、`.db` なら画像でないファイル、それ以外は画像として作成する
    fn resize_files_in_tempdir(files: &[&str]) -> (usize, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        for file in files {
            let path = dir.path().join(file);
            if file.ends_with('/') {
                std::fs::create_dir(&path).unwrap();
            } else if file.ends_with(".db") {
                std::fs::write(&path, b"not an image").unwrap();
            } else {
                write_image(&path);
            }
        }

        let files: Vec<PathBuf> = crate::fs_util::scan_regular_files(dir.path()).collect();
        let resized =
            super::resize_image_files(&files, 1800, indicatif::ProgressBar::hidden()).unwrap();
        (resized, list_names(dir.path()))
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
    fn test_rename_to_final_names_numbering() {
        // 元の拡張子を残した名前まで使用済みなら連番を付ける
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("001.png.webp"), b"existing").unwrap();
        let tmp_jpg = dir.path().join("a.webp.tmp");
        let tmp_png = dir.path().join("b.webp.tmp");
        std::fs::write(&tmp_jpg, b"jpg").unwrap();
        std::fs::write(&tmp_png, b"png").unwrap();

        super::rename_to_final_names(vec![
            (dir.path().join("001.png"), tmp_png),
            (dir.path().join("001.jpg"), tmp_jpg),
        ])
        .unwrap();

        assert_eq!(
            list_names(dir.path()),
            ["001.jpg.webp", "001.png.1.webp", "001.png.webp"]
        );
        assert_eq!(
            std::fs::read(dir.path().join("001.png.1.webp")).unwrap(),
            b"png"
        );
        assert_eq!(
            std::fs::read(dir.path().join("001.png.webp")).unwrap(),
            b"existing"
        );
    }
}
