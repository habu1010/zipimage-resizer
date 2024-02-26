use std::fs::File;
use std::path::Path;

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
pub fn unzip<P1: AsRef<Path>, P2: AsRef<Path>>(
    zip_file: P1,
    output_dir: P2,
) -> zip::result::ZipResult<()> {
    let file = File::open(zip_file)?;
    let mut archive = zip::ZipArchive::new(file)?;

    archive.extract(output_dir)
}

fn get_options(path: &Path) -> zip::write::FileOptions {
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();

    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "webp" => {
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored)
        }
        _ => zip::write::FileOptions::default(),
    }
}

fn relative_path<'a>(path: &'a Path, base_dir: &Path) -> std::borrow::Cow<'a, str> {
    path.strip_prefix(base_dir).unwrap().to_string_lossy()
}

fn zip_process_dir<W, P>(zw: &mut zip::ZipWriter<W>, dir: P, base_dir: &Path) -> std::io::Result<()>
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

        println!("adding {:?} as {:?} ...", path, filepath);

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
        zw.add_directory(filepath, zip::write::FileOptions::default())?;
        zip_process_dir(zw, path, base_dir)?;
    }

    Ok(())
}

/// 指定したディレクトリをzipファイルに圧縮する
/// # Arguments
/// * `src_dir` - 圧縮するディレクトリ
/// * `dst_file` - 圧縮先のzipファイル
pub fn zip<P1: AsRef<Path>>(src_dir: P1, dst_file: &File) -> std::io::Result<()> {
    let mut zw = zip::ZipWriter::new(dst_file);

    zip_process_dir(&mut zw, src_dir.as_ref(), src_dir.as_ref())?;

    zw.finish()?;

    Ok(())
}
