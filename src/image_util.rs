use std::borrow::Cow;

use anyhow::Result;
use image::GenericImageView;

pub trait ReduceSize {
    fn reduce_size(self, height: u32) -> Self;
}

impl ReduceSize for image::DynamicImage {
    fn reduce_size(self, height: u32) -> Self {
        use image::imageops::FilterType::*;
        match self.dimensions() {
            (_, h) if h >= height => self.resize(u32::MAX, height, Lanczos3),
            _ => self,
        }
    }
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

        let webp = (15i16..=95)
            .rev()
            .step_by(10)
            .map(f32::from)
            .map(|quality| encoder.encode(quality))
            .find(|webp| webp.len() <= max_size)
            .unwrap_or_else(|| encoder.encode(10.0));

        Ok(webp)
    }
}

#[cfg(test)]
mod tests {
    use super::ToWebPMemory;
    use image::{DynamicImage, GenericImageView};

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
