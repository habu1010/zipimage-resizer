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

        // Convert to supported format by webp encoder
        let image = match self {
            ImageRgb8(_) | ImageRgba8(_) => Cow::Borrowed(self),
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
