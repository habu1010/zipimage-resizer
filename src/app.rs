use std::{
    io::{Seek, Write},
    path::{Path, PathBuf},
};

use anyhow::Result;
use clap::Parser;
use image::{imageops::FilterType, GenericImageView};
use rayon::prelude::*;

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

trait ReduceSize {
    fn reduce_size(self, height: u32) -> Self;
}

impl ReduceSize for image::DynamicImage {
    fn reduce_size(self, height: u32) -> Self {
        match self.dimensions() {
            (_, h) if h >= height => self.resize(u32::MAX, height, FilterType::Lanczos3),
            _ => self,
        }
    }
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
        resized_image.color(),
    )

    // resized_image.save(path.as_ref().with_extension("jpg"))
}

fn resize_image_file_webp<P: AsRef<Path>>(path: P, min_height: u32) -> Result<bool> {
    if image::guess_format(&std::fs::read(path.as_ref())?)? == image::ImageFormat::WebP
        && path.as_ref().metadata()?.len() < 1024 * 1024
    {
        return Ok(false);
    }

    let Ok(image) = image::open(path.as_ref()) else {
        return Ok(false);
    };

    println!("resize image: {:?}", path.as_ref());
    std::fs::remove_file(path.as_ref())?;

    // Convert to supported format by webp encoder
    let image = match image {
        image::DynamicImage::ImageRgb8(_) => image,
        image::DynamicImage::ImageRgba8(_) => image,
        _ => image::DynamicImage::ImageRgb8(image.to_rgb8()),
    };

    let reduced_image = image.reduce_size(min_height);

    let encoder = match webp::Encoder::from_image(&reduced_image) {
        Ok(encoder) => encoder,
        Err(e) => {
            anyhow::bail!("failed to create webp encoder: {e}");
        }
    };

    let webp = (15..=95)
        .rev()
        .step_by(10)
        .map(|quality| encoder.encode(quality as f32))
        .find(|webp| webp.len() < 1024 * 1024)
        .unwrap_or_else(|| encoder.encode(10.0));

    let file = std::fs::File::create(path.as_ref().with_extension("webp"))?;
    let mut writer = std::io::BufWriter::new(file);
    writer.write_all(&webp)?;
    writer.flush()?;

    Ok(true)
}

fn resize_image_zipfile<P1: AsRef<Path>, P2: AsRef<Path>>(
    src_zipfile: P1,
    dst_zipfile: P2,
    min_height: u32,
    keep_mtime: bool,
) -> Result<()> {
    let work_dir = tempfile::tempdir()?;
    zip_util::unzip(&src_zipfile, work_dir.path())?;

    let files = walkdir::WalkDir::new(work_dir.path())
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .collect::<Vec<_>>();

    let resized_count = files
        .par_iter()
        .map(|file| resize_image_file_webp(file.path(), min_height))
        .filter(|r| *r.as_ref().ok().unwrap_or(&false))
        .collect::<Result<Vec<_>>>()?
        .len();

    if resized_count == 0 {
        return Ok(());
    }

    let mut dst_tempfile = tempfile::tempfile()?;
    zip_util::zip(work_dir.path(), &dst_tempfile)?;

    // dst_tempfile -> dst_zipfile
    dst_tempfile.seek(std::io::SeekFrom::Start(0))?;
    let mut writer = std::fs::File::create(dst_zipfile.as_ref())?;
    std::io::copy(&mut dst_tempfile, &mut writer)?;

    if keep_mtime {
        let src_mtime = src_zipfile.as_ref().metadata()?.modified()?;
        writer.set_modified(src_mtime)?;
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

fn print_error(mut err: &dyn std::error::Error) {
    let _ = writeln!(std::io::stderr(), "error: {}", err);
    while let Some(source) = err.source() {
        let _ = writeln!(std::io::stderr(), "caused by: {}", source);
        err = source;
    }
}

pub fn run() {
    let args = Args::parse();

    let (convert_files, _): (Vec<_>, Vec<_>) = args
        .zipfiles
        .iter()
        .partition(|f| calc_average_size_per_file(f).unwrap_or(0) > 2 * 1024);

    for zipfile in convert_files {
        let dst_zipfile = determine_output_path(zipfile, &args.output_dir);
        println!("resizing zipfile: {:?} -> {:?}", zipfile, dst_zipfile);
        if let Err(err) =
            resize_image_zipfile(zipfile, dst_zipfile, args.min_height, args.keep_mtime)
        {
            print_error(err.as_ref());
            std::process::exit(1);
        }
    }
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
