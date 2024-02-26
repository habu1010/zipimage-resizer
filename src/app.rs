use std::{
    io::Seek,
    path::{Path, PathBuf},
};

use anyhow::Result;
use clap::Parser;
use indicatif::{ParallelProgressIterator, ProgressBar, ProgressStyle};
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

    std::fs::remove_file(path)?;

    let webp = image.reduce_size(min_height).to_webp(MAX_WEBP_SIZE)?;
    std::fs::write(path.with_extension("webp"), webp.as_ref())?;

    Ok(true)
}

fn resize_image_files<P>(files: &[P], min_height: u32) -> Result<usize>
where
    P: AsRef<Path> + Sync,
{
    let pb = ProgressBar::new(files.len() as u64)
        .with_message("Resizing images...")
        .with_style(
            ProgressStyle::with_template("{msg:20} [{bar:40.cyan/blue}] {pos}/{len} ETA:{eta}")
                .unwrap()
                .progress_chars("#>-"),
        );

    let results = files
        .par_iter()
        .progress_with(pb.clone())
        .map(|file| resize_image_file_webp(file, min_height))
        .collect::<Result<Vec<_>>>()?;

    pb.finish_with_message("Resizing completed");

    Ok(results.into_iter().filter(|r| *r).count())
}

fn resize_image_zipfile<P1: AsRef<Path>, P2: AsRef<Path>>(
    src_zipfile: P1,
    dst_zipfile: P2,
    min_height: u32,
    keep_mtime: bool,
) -> Result<()> {
    let work_dir = tempfile::tempdir()?;
    zip_util::unzip(&src_zipfile, work_dir.path())?;

    let files: Vec<_> = fs_util::scan_regular_files(work_dir.path()).collect();
    if resize_image_files(&files, min_height)? == 0 {
        return Ok(());
    }

    let mut dst_tempfile = tempfile::tempfile()?;
    zip_util::zip(work_dir.path(), &dst_tempfile)?;

    // dst_tempfile -> dst_zipfile
    dst_tempfile.seek(std::io::SeekFrom::Start(0))?;
    let dst_zipfile = std::fs::File::create(dst_zipfile.as_ref())?;
    let mut dst_zipfile_writer = std::io::BufWriter::new(&dst_zipfile);
    std::io::copy(&mut dst_tempfile, &mut dst_zipfile_writer)?;

    if keep_mtime {
        let src_mtime = src_zipfile.as_ref().metadata()?.modified()?;
        dst_zipfile.set_modified(src_mtime)?;
    }

    Ok(())
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
    Ok(file_size / file_count)
}

pub fn run() -> Result<()> {
    let args = Args::parse();

    let (convert_files, _): (Vec<_>, Vec<_>) = args
        .zipfiles
        .par_iter()
        .partition(|f| calc_average_size_per_file(f).unwrap_or(0) > 500 * 1024);

    println!(
        "Files to convert: {}/{}",
        convert_files.len(),
        args.zipfiles.len()
    );

    for (seq, zipfile) in convert_files.iter().enumerate().map(|(i, f)| (i + 1, f)) {
        let dst_zipfile = determine_output_path(zipfile, &args.output_dir);
        println!(
            "resizing zipfile ({seq}/{}): {zipfile:?} -> {dst_zipfile:?}",
            convert_files.len()
        );
        resize_image_zipfile(zipfile, dst_zipfile, args.min_height, args.keep_mtime)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_append_suffix_to_filename() {
        let path = std::path::Path::new("/path1/path2/test.txt");
        let path = super::append_suffix_to_filename(path, "suffix");
        assert_eq!(path, std::path::Path::new("/path1/path2/test_suffix.txt"));
    }
}
