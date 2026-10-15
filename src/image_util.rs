use std::borrow::Cow;
use std::io::{BufRead, Seek};
use std::path::Path;

use anyhow::Result;
use image::metadata::Orientation;
use image::{GenericImageView, ImageDecoder};

pub trait ReduceSize {
    /// 高さが `max_height` を超える画像を、縦横比を保って `max_height` まで縮小する（拡大はしない）
    fn reduce_size(self, max_height: u32) -> Self;
}

impl ReduceSize for image::DynamicImage {
    fn reduce_size(self, max_height: u32) -> Self {
        use image::imageops::FilterType::*;
        // 高さがちょうど上限と同じ画像は、同じサイズへの無駄なリサンプリングになるので除く
        match self.dimensions() {
            (_, h) if h > max_height => self.resize(u32::MAX, max_height, Lanczos3),
            _ => self,
        }
    }
}

/// アニメーション画像（アニメーション GIF / APNG / アニメーション WebP）かどうかを判定する
///
/// 静止画の WebP に変換すると先頭フレーム以外が失われるため、変換対象外にするのに使う。
/// 判定できない（壊れているなど）場合は `false` を返し、通常のデコードの判定に任せる。
pub fn is_animated(path: &Path, format: Option<image::ImageFormat>) -> bool {
    use image::codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder};
    use image::ImageFormat;

    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let reader = std::io::BufReader::new(file);
    match format {
        // GIF はヘッダに目印がないため、2 フレーム目があるかで判定する
        Some(ImageFormat::Gif) => GifDecoder::new(reader).is_ok_and(has_multiple_frames),
        // APNG でもフレームが 1 枚だけなら実質は静止画なので、実際に 2 フレームあるかを確かめる
        // （フレームを読むのは APNG の場合だけなので、通常の PNG では追加の処理は走らない）
        Some(ImageFormat::Png) => PngDecoder::new(reader).is_ok_and(|decoder| {
            decoder.is_apng().unwrap_or(false) && decoder.apng().is_ok_and(has_multiple_frames)
        }),
        Some(ImageFormat::WebP) => {
            WebPDecoder::new(reader).is_ok_and(|decoder| decoder.has_animation())
        }
        _ => false,
    }
}

/// 正常に読めるフレームが 2 枚以上あるかどうか
///
/// 読み取りエラーはフレームとして数えない（末尾が壊れた静止画を誤判定しないため）。
fn has_multiple_frames<'a>(decoder: impl image::AnimationDecoder<'a>) -> bool {
    let frames = decoder.into_frames().take(2);
    frames.filter(|frame| frame.is_ok()).count() > 1
}

/// 画像をデコードし、EXIF の向き（Orientation）を適用する
///
/// WebP には元の EXIF を引き継がないため、向きを画素に反映しておかないと
/// 回転情報付きの写真（スキャン画像など）が横倒しのまま出力される。
pub fn decode_with_orientation<R: BufRead + Seek>(
    reader: image::ImageReader<R>,
) -> image::ImageResult<image::DynamicImage> {
    let mut decoder = reader.into_decoder()?;
    // 向きの情報が壊れているだけなら、向きを変えずにそのまま使う
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    // ImageReader::decode と同じく、デコード前に必要なメモリ量を見積もって巨大な画像を弾く
    // （ImageReader の上限はデコーダに引き継がれるが、この事前チェックは既定の上限で行う）
    image::Limits::default().reserve(decoder.total_bytes())?;
    let mut image = image::DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image)
}

pub trait ToWebPMemory {
    fn to_webp(&self, max_size: usize) -> Result<webp::WebPMemory>;
}

impl ToWebPMemory for image::DynamicImage {
    fn to_webp(&self, max_size: usize) -> Result<webp::WebPMemory> {
        use image::DynamicImage::*;

        // WebP エンコーダが対応している RGB8 / RGBA8 に変換する
        // （透過情報を持つ形式は、透過が失われないよう RGBA8 にする）
        let image = match self {
            ImageRgb8(_) | ImageRgba8(_) => Cow::Borrowed(self),
            _ if self.color().has_alpha() => Cow::Owned(ImageRgba8(self.to_rgba8())),
            _ => Cow::Owned(ImageRgb8(self.to_rgb8())),
        };

        let encoder = webp::Encoder::from_image(&image)
            .map_err(|e| anyhow::anyhow!("Failed to create WebP encoder: {e}"))?;

        // 品質を高い順に試し、`max_size` 以下に収まった最初の結果を採用する
        const QUALITIES: [f32; 9] = [95.0, 85.0, 75.0, 65.0, 55.0, 45.0, 35.0, 25.0, 15.0];
        const FALLBACK_QUALITY: f32 = 10.0;
        let webp = QUALITIES
            .into_iter()
            .map(|quality| encoder.encode(quality))
            .find(|webp| webp.len() <= max_size)
            .unwrap_or_else(|| encoder.encode(FALLBACK_QUALITY));

        Ok(webp)
    }
}

#[cfg(test)]
mod tests {
    use super::ToWebPMemory;
    use image::{DynamicImage, GenericImageView};

    #[test]
    fn test_reduce_size() {
        use super::ReduceSize;

        // 上限より高い画像だけを、縦横比を保って縮小する
        let image = DynamicImage::new_rgb8(200, 400).reduce_size(100);
        assert_eq!(image.dimensions(), (50, 100));
        // 上限以下の画像はそのまま
        let image = DynamicImage::new_rgb8(200, 100).reduce_size(100);
        assert_eq!(image.dimensions(), (200, 100));
    }

    /// EXIF の向き（Orientation）を埋め込んだ JPEG を作る
    fn jpeg_with_orientation(width: u32, height: u32, orientation: u16) -> Vec<u8> {
        let mut jpeg = std::io::Cursor::new(Vec::new());
        DynamicImage::new_rgb8(width, height)
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let jpeg = jpeg.into_inner();

        // APP1 (Exif): TIFF ヘッダ（リトルエンディアン）+ Orientation タグ 1 つだけの IFD
        let mut exif = b"Exif\0\0II*\0".to_vec();
        exif.extend(8u32.to_le_bytes()); // IFD の位置
        exif.extend(1u16.to_le_bytes()); // エントリ数
        exif.extend(0x0112u16.to_le_bytes()); // Orientation タグ
        exif.extend(3u16.to_le_bytes()); // SHORT 型
        exif.extend(1u32.to_le_bytes()); // 個数
        exif.extend(orientation.to_le_bytes());
        exif.extend([0, 0]); // パディング
        exif.extend(0u32.to_le_bytes()); // 次の IFD なし

        // SOI（先頭 2 バイト）の直後に APP1 セグメントを挿入する
        let mut result = jpeg[..2].to_vec();
        result.extend([0xFF, 0xE1]);
        result.extend((exif.len() as u16 + 2).to_be_bytes());
        result.extend(exif);
        result.extend(&jpeg[2..]);
        result
    }

    #[test]
    fn test_decode_with_orientation() {
        let decode = |jpeg: Vec<u8>| {
            let reader = image::ImageReader::new(std::io::Cursor::new(jpeg))
                .with_guessed_format()
                .unwrap();
            super::decode_with_orientation(reader).unwrap()
        };

        // Orientation 6（時計回りに 90 度回転して表示）は縦横が入れ替わる
        assert_eq!(
            decode(jpeg_with_orientation(40, 20, 6)).dimensions(),
            (20, 40)
        );
        // Orientation 1（そのまま）は変わらない
        assert_eq!(
            decode(jpeg_with_orientation(40, 20, 1)).dimensions(),
            (40, 20)
        );
    }

    /// `dir` 内の `name` をアニメーションと判定するか（形式は拡張子から決める）
    fn is_animated(dir: &tempfile::TempDir, name: &str) -> bool {
        let path = dir.path().join(name);
        let format = image::ImageFormat::from_path(&path).ok();
        super::is_animated(&path, format)
    }

    #[test]
    fn test_is_animated_gif() {
        use crate::test_util::{write_animated_gif, write_image};

        // 2 フレーム以上ならアニメーション
        let dir = tempfile::tempdir().unwrap();
        write_animated_gif(&dir.path().join("anim.gif"));
        write_image(&dir.path().join("still.gif"));
        assert!(is_animated(&dir, "anim.gif"));
        assert!(!is_animated(&dir, "still.gif"));

        // 末尾が壊れた静止画 GIF（2 フレーム目の読み取りがエラーになる）は静止画扱い
        let mut broken = std::fs::read(dir.path().join("still.gif")).unwrap();
        assert_eq!(broken.pop(), Some(0x3B)); // 終端（Trailer）を取り除く
        broken.extend([0x2C, 0x00]); // 途中で切れたイメージディスクリプタ
        std::fs::write(dir.path().join("broken.gif"), broken).unwrap();
        assert!(!is_animated(&dir, "broken.gif"));
    }

    #[test]
    fn test_is_animated_webp() {
        use crate::test_util::{write_animated_webp, write_image};

        let dir = tempfile::tempdir().unwrap();
        write_animated_webp(&dir.path().join("anim.webp"));
        write_image(&dir.path().join("still.webp"));
        assert!(is_animated(&dir, "anim.webp"));
        assert!(!is_animated(&dir, "still.webp"));
    }

    #[test]
    fn test_is_animated_png() {
        // APNG はテスト用に作れないので、静止画の PNG のみ確認する
        let dir = tempfile::tempdir().unwrap();
        crate::test_util::write_image(&dir.path().join("still.png"));
        assert!(!is_animated(&dir, "still.png"));
    }

    /// WebP に変換してからデコードし直す
    fn roundtrip(image: DynamicImage) -> DynamicImage {
        let webp = image.to_webp(1024 * 1024).unwrap();
        image::load_from_memory(&webp).unwrap()
    }

    /// 非可逆圧縮による誤差を許容して、各チャンネルの値が近いことを確認する
    fn assert_pixel_near(actual: image::Rgba<u8>, expected: [u8; 4]) {
        const TOLERANCE: u8 = 16;
        let near = actual
            .0
            .iter()
            .zip(expected)
            .all(|(a, e)| a.abs_diff(e) <= TOLERANCE);
        assert!(near, "{actual:?} is not near {expected:?}");
    }

    #[test]
    fn test_to_webp_keeps_alpha() {
        // 透過付きグレースケール（LumaA8）や 16bit RGBA も透過が保たれ、色も変わらないこと
        let mut luma_a = image::GrayAlphaImage::from_pixel(8, 8, image::LumaA([128, 255]));
        luma_a.put_pixel(0, 0, image::LumaA([128, 0]));
        let mut rgba16 =
            image::ImageBuffer::from_pixel(8, 8, image::Rgba([u16::MAX, 0, 0, u16::MAX]));
        rgba16.put_pixel(0, 0, image::Rgba([u16::MAX, 0, 0, 0]));

        for (image, opaque) in [
            (DynamicImage::ImageLumaA8(luma_a), [128, 128, 128, 255]),
            (DynamicImage::ImageRgba16(rgba16), [255, 0, 0, 255]),
        ] {
            let decoded = roundtrip(image);
            assert!(decoded.color().has_alpha());
            // 透明なピクセルは透明のまま（色は保証されないのでアルファだけ見る）
            assert!(decoded.get_pixel(0, 0)[3] < 16);
            assert_pixel_near(decoded.get_pixel(1, 1), opaque);
        }
    }

    #[test]
    fn test_to_webp_without_alpha() {
        // 透過情報を持たない形式は RGB のまま（不要なアルファチャンネルを付けない）
        let luma = image::GrayImage::from_pixel(8, 8, image::Luma([128]));
        let decoded = roundtrip(DynamicImage::ImageLuma8(luma));
        assert!(!decoded.color().has_alpha());
    }
}
