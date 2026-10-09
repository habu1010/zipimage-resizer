//! 複数のモジュールのテストで共有するヘルパー

use std::path::Path;

/// `dir` に読み取り権限のないサブディレクトリを作る
///
/// root で実行しているなどで権限を無視して読めてしまう場合は `None` を返す
/// （呼び出し側はテストをスキップする）。戻り値を drop すると権限を戻す。
#[cfg(unix)]
pub fn unreadable_subdir(dir: &Path) -> Option<impl Drop> {
    use std::os::unix::fs::PermissionsExt;

    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            // TempDir が削除できるよう権限を戻す
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    let sub = dir.join("sub");
    std::fs::create_dir(&sub).unwrap();
    // 途中で panic しても権限が戻るよう、権限を変える前にガードを作る
    let restore = Restore(sub.clone());
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::read_dir(&sub).is_err().then_some(restore)
}

/// 小さい静止画を書き出す（形式は拡張子で決まる。WebP は変換対象外になるサイズ）
pub fn write_image(path: &Path) {
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

/// アニメーションの各フレーム（8x8 の赤と青）
fn animation_frames() -> [image::RgbaImage; 2] {
    [[255, 0, 0, 255], [0, 0, 255, 255]]
        .map(|color| image::RgbaImage::from_pixel(8, 8, image::Rgba(color)))
}

/// 2 フレームのアニメーション GIF を書き出す
pub fn write_animated_gif(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = image::codecs::gif::GifEncoder::new(file);
    encoder
        .encode_frames(animation_frames().map(image::Frame::new))
        .unwrap();
}

/// 2 フレームのアニメーション WebP を書き出す
pub fn write_animated_webp(path: &Path) {
    let config = webp::WebPConfig::new().unwrap();
    let mut encoder = webp::AnimEncoder::new(8, 8, &config);
    let frames = animation_frames().map(image::DynamicImage::ImageRgba8);
    for (i, frame) in frames.iter().enumerate() {
        encoder.add_frame(webp::AnimFrame::from_image(frame, i as i32 * 100).unwrap());
    }
    std::fs::write(path, &*encoder.encode()).unwrap();
}
