//! Images for models that can see: loaded once, shrunk, and carried in the
//! session as JPEG so a replay never depends on the original file.
//!
//! Only the request builder decides whether an image is actually sent (see
//! `agent::request_messages`): a text-only model gets a one-line note instead,
//! and old images are dropped so one photo does not ride every request.

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};

/// Pixels kept after shrinking, about 2000 tokens. A budget on area, not on
/// the longest side: a 1206×2622 phone screenshot capped at 1024 tall came out
/// 471 wide, and the model could not make out a thumbnail inside it.
pub const MAX_PIXELS: u64 = 2048 * PATCH as u64 * PATCH as u64;

/// Side of the square the vision encoder spends one token on. Measured on the
/// 27B through vLLM: 471×1024 cost 480 image tokens, 1206×2622 cost 3116.
const PATCH: u32 = 32;

/// Refuse anything bigger than this before decoding it.
const MAX_FILE_BYTES: u64 = 40 * 1024 * 1024;

/// Extensions treated as images. HEIC is what an iPhone saves; it is turned
/// into JPEG with `sips` on macOS, since no pure-Rust decoder handles it.
const EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "heic", "heif",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Image {
    /// The name the user or tool gave it, for notes that stand in for it.
    pub name: String,
    pub mime: String,
    /// Base64 of the shrunk image.
    pub data: String,
    pub width: u32,
    pub height: u32,
}

pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

impl Image {
    /// Load `path`, shrink it to fit `MAX_PIXELS`, and re-encode as JPEG.
    pub fn load(path: &Path, name: &str) -> Result<Image> {
        let len = std::fs::metadata(path)
            .with_context(|| format!("cannot read {}", path.display()))?
            .len();
        if len > MAX_FILE_BYTES {
            bail!(
                "{} is {} MB; images over 40 MB are not sent",
                name,
                len / (1024 * 1024)
            );
        }
        let bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let decoded = match image::load_from_memory(&bytes) {
            Ok(img) => img,
            Err(e) => match convert_with_sips(path) {
                Some(jpeg) => image::load_from_memory(&jpeg)
                    .with_context(|| format!("cannot decode {name}"))?,
                None => bail!("cannot decode {name}: {e}"),
            },
        };
        Ok(Self::from_image(decoded, name))
    }

    fn from_image(img: image::DynamicImage, name: &str) -> Image {
        let pixels = img.width() as u64 * img.height() as u64;
        let img = if pixels > MAX_PIXELS {
            let scale = (MAX_PIXELS as f64 / pixels as f64).sqrt();
            img.thumbnail(
                (img.width() as f64 * scale) as u32,
                (img.height() as f64 * scale) as u32,
            )
        } else {
            img
        };
        // JPEG has no alpha: flatten onto white, which is what a transparent
        // screenshot or icon looks like on a page.
        let rgba = img.to_rgba8();
        let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
        for (x, y, p) in rgba.enumerate_pixels() {
            let a = p[3] as u32;
            let mix = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
            rgb.put_pixel(x, y, image::Rgb([mix(p[0]), mix(p[1]), mix(p[2])]));
        }
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut out), 85)
            .encode_image(&rgb)
            .expect("encoding an in-memory RGB image cannot fail");
        Image {
            name: name.to_string(),
            mime: "image/jpeg".into(),
            data: base64::engine::general_purpose::STANDARD.encode(&out),
            width: rgb.width(),
            height: rgb.height(),
        }
    }

    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.data)
    }

    /// Rough prompt cost: one token per patch. Other models are in the same
    /// range. Good enough for deciding when to compact, which is all the
    /// estimate is for.
    pub fn est_tokens(&self) -> usize {
        (self.width.div_ceil(PATCH) * self.height.div_ceil(PATCH)) as usize + 8
    }

    /// What stands in for the image when it is not sent.
    pub fn placeholder(&self, why: &str) -> String {
        format!(
            "[image {} ({}×{}) {why}]",
            self.name, self.width, self.height
        )
    }
}

/// HEIC and anything else the decoders refuse: let macOS convert it.
fn convert_with_sips(path: &Path) -> Option<Vec<u8>> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let out = std::env::temp_dir().join(format!("worksmith-{}-{nanos}.jpg", std::process::id()));
    let status = std::process::Command::new("sips")
        .args(["-s", "format", "jpeg"])
        .arg(path)
        .arg("--out")
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let bytes = match status {
        Ok(s) if s.success() => std::fs::read(&out).ok(),
        _ => None,
    };
    let _ = std::fs::remove_file(&out);
    bytes
}

/// Images named with `@path` in a message, resolved against `cwd`. Text files
/// named the same way are inlined by the front ends; images ride along here.
/// Unreadable images come back as errors to show the user, not silent drops.
pub fn mentioned(text: &str, cwd: &Path) -> (Vec<Image>, Vec<String>) {
    let mut images = Vec::new();
    let mut errors = Vec::new();
    for token in text.split_whitespace() {
        let Some(name) = token.strip_prefix('@') else {
            continue;
        };
        let name = name.trim_end_matches([',', '.', ';', ':', ')', '?', '!']);
        // `@~/Desktop/shot.png` is how a person names a file outside the
        // project; without expanding it the image was silently not attached.
        let path = match (name.strip_prefix("~/"), dirs::home_dir()) {
            (Some(rest), Some(home)) => home.join(rest),
            _ if Path::new(name).is_absolute() => Path::new(name).to_path_buf(),
            _ => cwd.join(name),
        };
        if !is_image_path(&path) || !path.is_file() {
            continue;
        }
        if images.iter().any(|i: &Image| i.name == name) {
            continue;
        }
        match Image::load(&path, name) {
            Ok(img) => images.push(img),
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    (images, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, alpha: u8) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, _| {
            if x < w / 2 {
                image::Rgba([255, 0, 0, alpha])
            } else {
                image::Rgba([0, 0, 255, alpha])
            }
        });
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn a_big_photo_is_shrunk_to_fit_and_sent_as_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("closet.png");
        std::fs::write(&p, png(3000, 2000, 255)).unwrap();
        let img = Image::load(&p, "closet.png").unwrap();
        assert_eq!((img.width, img.height), (1773, 1182));
        assert_eq!(img.mime, "image/jpeg");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&img.data)
            .unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "a JPEG");
        assert!(img.data_url().starts_with("data:image/jpeg;base64,"));
        assert!(
            (1900..2200).contains(&img.est_tokens()),
            "{}",
            img.est_tokens()
        );
    }

    #[test]
    fn a_tall_phone_screenshot_stays_wide_enough_to_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("confirm.png");
        std::fs::write(&p, png(1206, 2622, 255)).unwrap();
        let img = Image::load(&p, "confirm.png").unwrap();
        assert!(img.width > 950, "{}×{}", img.width, img.height);
        assert!(img.width as u64 * img.height as u64 <= MAX_PIXELS);
    }

    #[test]
    fn a_small_image_keeps_its_size_and_transparency_turns_white() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("icon.png");
        std::fs::write(&p, png(64, 32, 0)).unwrap();
        let img = Image::load(&p, "icon.png").unwrap();
        assert_eq!((img.width, img.height), (64, 32));
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&img.data)
            .unwrap();
        let px = image::load_from_memory(&bytes).unwrap().to_rgb8();
        assert!(
            px.get_pixel(5, 5).0.iter().all(|&c| c > 240),
            "{:?}",
            px.get_pixel(5, 5)
        );
    }

    #[test]
    fn mentions_attach_images_and_report_broken_ones() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shirt.png"), png(10, 10, 255)).unwrap();
        std::fs::write(dir.path().join("broken.jpg"), b"not a jpeg").unwrap();
        std::fs::write(dir.path().join("notes.md"), "text").unwrap();
        let (imgs, errs) = mentioned(
            "match @shirt.png, @shirt.png and @notes.md with @broken.jpg or @missing.png",
            dir.path(),
        );
        assert_eq!(imgs.len(), 1, "once, despite two mentions");
        assert_eq!(imgs[0].name, "shirt.png");
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("broken.jpg"), "{errs:?}");
    }

    #[test]
    fn a_home_relative_mention_is_found() {
        let home = dirs::home_dir().unwrap();
        let dir = tempfile::tempdir_in(&home).unwrap();
        std::fs::write(dir.path().join("doll.png"), png(8, 8, 255)).unwrap();
        let rel = dir.path().strip_prefix(&home).unwrap().join("doll.png");
        let (imgs, errs) = mentioned(&format!("view @~/{}", rel.display()), Path::new("/"));
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(imgs.len(), 1);
    }

    #[test]
    fn only_image_extensions_count() {
        assert!(is_image_path(Path::new("a/IMG_0001.HEIC")));
        assert!(is_image_path(Path::new("shot.webp")));
        assert!(!is_image_path(Path::new("main.rs")));
        assert!(!is_image_path(Path::new("Makefile")));
    }
}
