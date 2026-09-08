//! The image policy. A picture goes back to the model as MCP image content,
//! never as the original bytes: Claude Code counts the base64 against
//! `MAX_MCP_OUTPUT_TOKENS` and nothing downscales it client-side, so what
//! leaves here has to be small enough to be worth a turn.
//!
//! Decode JPEG, PNG, GIF (first frame) and WebP; fit the long side to the
//! profile's limit, never enlarging; then encode. JPEG at quality 80, at
//! quality 60 if that is still over the byte cap, and once more at half the
//! dimensions if it still is. PNG is kept only when the picture actually has
//! transparency and the PNG fits, because a photograph as PNG is an order of
//! magnitude over the cap. HEIC and SVG are not decoded at all; they go out
//! as a download link and the error says so.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, RgbImage};

use super::limits::{
    IMAGE_LONG_SIDE, IMAGE_LONG_SIDE_CLAUDE_CODE, IMAGE_MAX_BYTES, IMAGE_MAX_BYTES_CLAUDE_CODE,
};
use super::scope::ClientProfile;

const JPEG_QUALITY: u8 = 80;
/// What the second attempt drops to when the first is over the cap.
const JPEG_QUALITY_LOW: u8 = 60;
/// Downscaling is by a good filter; the retry at half size uses a cheap one,
/// because at that point the picture is already a thumbnail.
const RESIZE_FILTER: FilterType = FilterType::Lanczos3;
const RETRY_FILTER: FilterType = FilterType::Triangle;

/// The size a client's images are cut to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// The longest side, in pixels.
    pub long_side: u32,
    /// The largest encoded size, in bytes.
    pub max_bytes: usize,
}

impl Profile {
    /// Open WebUI, OpenCode and anything unknown.
    pub const DEFAULT: Profile = Profile {
        long_side: IMAGE_LONG_SIDE,
        max_bytes: IMAGE_MAX_BYTES,
    };
    /// Claude Code, whose tool results are budgeted in tokens.
    pub const CLAUDE_CODE: Profile = Profile {
        long_side: IMAGE_LONG_SIDE_CLAUDE_CODE,
        max_bytes: IMAGE_MAX_BYTES_CLAUDE_CODE,
    };

    pub fn for_client(client: ClientProfile) -> Profile {
        match client {
            ClientProfile::ClaudeCode => Profile::CLAUDE_CODE,
            ClientProfile::Generic | ClientProfile::OpenWebUi | ClientProfile::OpenCode => {
                Profile::DEFAULT
            }
        }
    }
}

/// What the MCP image content is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ImageError {
    /// The format is one this server does not decode. The tool says to use a
    /// download link instead.
    #[error(
        "{0} is link only: this server does not decode it, fetch the file with a download link"
    )]
    LinkOnly(&'static str),
    #[error("the image could not be decoded: {0}")]
    Decode(String),
    #[error("the image could not be re-encoded: {0}")]
    Encode(String),
}

/// Decode, downscale and re-encode, or say why not.
pub fn prepare(bytes: &[u8], profile: Profile) -> Result<Prepared, ImageError> {
    if let Some(kind) = link_only(bytes) {
        return Err(ImageError::LinkOnly(kind));
    }
    let format = image::guess_format(bytes).map_err(|e| ImageError::Decode(e.to_string()))?;
    if !matches!(
        format,
        ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::Gif | ImageFormat::WebP
    ) {
        return Err(ImageError::LinkOnly(label(format)));
    }
    // GIF decodes to its first frame, which is what a mail attachment of an
    // animation is worth looking at.
    let decoded = image::load_from_memory_with_format(bytes, format)
        .map_err(|e| ImageError::Decode(e.to_string()))?;
    let fitted = fit(decoded, profile.long_side);

    // Transparency is the only reason to spend the bytes on PNG, and only
    // when the PNG is small enough to be worth it.
    if has_transparency(&fitted) {
        let bytes = encode_png(&fitted)?;
        if bytes.len() <= profile.max_bytes {
            return Ok(Prepared {
                mime: "image/png",
                width: fitted.width(),
                height: fitted.height(),
                bytes,
            });
        }
    }

    let mut candidate = fitted;
    let mut best: Option<Prepared> = None;
    // The first attempt at the fitted size, one retry at half of it.
    for attempt in 0..2 {
        let flat = flatten(&candidate);
        let prepared = encode_jpeg(&flat, profile.max_bytes)?;
        let fits = prepared.bytes.len() <= profile.max_bytes;
        if best
            .as_ref()
            .is_none_or(|b| prepared.bytes.len() < b.bytes.len())
        {
            best = Some(prepared);
        }
        if fits || attempt == 1 {
            break;
        }
        candidate = candidate.resize(
            (candidate.width() / 2).max(1),
            (candidate.height() / 2).max(1),
            RETRY_FILTER,
        );
    }
    // Best effort: a picture slightly over the cap is worth more to the model
    // than an error, and the halving has already taken three quarters off.
    Ok(best.expect("the loop always encodes once"))
}

/// Fit inside a square of `long_side`, keeping the aspect ratio. A picture
/// that is already small enough is left exactly as it is: upscaling adds
/// bytes and no detail.
fn fit(img: DynamicImage, long_side: u32) -> DynamicImage {
    if img.width().max(img.height()) <= long_side {
        return img;
    }
    img.resize(long_side, long_side, RESIZE_FILTER)
}

fn has_transparency(img: &DynamicImage) -> bool {
    img.color().has_alpha() && img.to_rgba8().pixels().any(|p| p[3] < 255)
}

/// JPEG has no alpha, so a transparent picture that has to become one is
/// composited onto white rather than having its alpha dropped, which would
/// show whatever colour happened to be under it.
fn flatten(img: &DynamicImage) -> RgbImage {
    if !img.color().has_alpha() {
        return img.to_rgb8();
    }
    let rgba = img.to_rgba8();
    let mut out = RgbImage::new(rgba.width(), rgba.height());
    for (x, y, pixel) in rgba.enumerate_pixels() {
        let alpha = u32::from(pixel[3]);
        let flat = out.get_pixel_mut(x, y);
        for channel in 0..3 {
            let over = u32::from(pixel[channel]) * alpha + 255 * (255 - alpha);
            flat[channel] = (over / 255) as u8;
        }
    }
    out
}

fn encode_jpeg(img: &RgbImage, max_bytes: usize) -> Result<Prepared, ImageError> {
    let mut bytes = encode_jpeg_at(img, JPEG_QUALITY)?;
    if bytes.len() > max_bytes {
        let lower = encode_jpeg_at(img, JPEG_QUALITY_LOW)?;
        if lower.len() < bytes.len() {
            bytes = lower;
        }
    }
    Ok(Prepared {
        mime: "image/jpeg",
        width: img.width(),
        height: img.height(),
        bytes,
    })
}

fn encode_jpeg_at(img: &RgbImage, quality: u8) -> Result<Vec<u8>, ImageError> {
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, quality)
        .encode_image(img)
        .map_err(|e| ImageError::Encode(e.to_string()))?;
    Ok(bytes)
}

fn encode_png(img: &DynamicImage) -> Result<Vec<u8>, ImageError> {
    let mut bytes = Vec::new();
    img.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .map_err(|e| ImageError::Encode(e.to_string()))?;
    Ok(bytes)
}

/// ISO base media brands that mean HEIF, and the two ways an SVG starts.
/// Neither is decoded here, and neither is worth a dependency: phones send
/// HEIC and Drive holds SVG, and both have a perfectly good download link.
const HEIF_BRANDS: [&[u8]; 10] = [
    b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1", b"msf1",
];

fn link_only(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && HEIF_BRANDS.contains(&&bytes[8..12]) {
        return Some("HEIC");
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    let trimmed = head.trim_start();
    if trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && head.contains("<svg")) {
        return Some("SVG");
    }
    None
}

fn label(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Avif => "AVIF",
        ImageFormat::Bmp => "BMP",
        ImageFormat::Tiff => "TIFF",
        ImageFormat::Ico => "ICO",
        _ => "this image format",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::{Frame, Rgb, Rgba, RgbaImage};

    /// A smooth, photograph-shaped picture: it compresses the way a real one
    /// does, so the byte cap means what it means in production.
    fn photo(width: u32, height: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            let r = (x * 255 / width.max(1)) as u8;
            let g = (y * 255 / height.max(1)) as u8;
            let b = ((x + y) * 255 / (width + height).max(1)) as u8;
            Rgb([r, g, b])
        })
    }

    fn jpeg(img: &RgbImage) -> Vec<u8> {
        encode_jpeg_at(img, 90).unwrap()
    }

    fn transparent_png(width: u32, height: u32) -> Vec<u8> {
        let img = RgbaImage::from_fn(width, height, |x, y| {
            if (x + y) % 2 == 0 {
                Rgba([255, 0, 0, 0])
            } else {
                Rgba([0, 0, 255, 255])
            }
        });
        encode_png(&DynamicImage::ImageRgba8(img)).unwrap()
    }

    fn animated_gif() -> Vec<u8> {
        let frame = |colour: [u8; 4]| Frame::new(RgbaImage::from_pixel(64, 48, Rgba(colour)));
        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            encoder.encode_frame(frame([220, 20, 20, 255])).unwrap();
            encoder.encode_frame(frame([20, 20, 220, 255])).unwrap();
        }
        bytes
    }

    #[test]
    fn a_large_photograph_is_fitted_to_each_profile() {
        let original = jpeg(&photo(3000, 2000));
        for (profile, long_side, cap) in [
            (Profile::DEFAULT, 1568, IMAGE_MAX_BYTES),
            (Profile::CLAUDE_CODE, 1024, IMAGE_MAX_BYTES_CLAUDE_CODE),
        ] {
            let out = prepare(&original, profile).unwrap();
            assert_eq!(out.mime, "image/jpeg");
            assert_eq!(out.width, long_side);
            // Two thirds of the long side, to the pixel the filter lands on.
            assert!((i64::from(out.height) - i64::from(long_side) * 2 / 3).abs() <= 1);
            assert!(out.bytes.len() <= cap, "{} bytes", out.bytes.len());
            assert!(out.bytes.len() < original.len());
            // It really is a JPEG, and it really decodes.
            assert_eq!(image::guess_format(&out.bytes).unwrap(), ImageFormat::Jpeg);
        }
        assert_eq!(
            Profile::for_client(ClientProfile::ClaudeCode).long_side,
            1024
        );
        assert_eq!(
            Profile::for_client(ClientProfile::OpenWebUi),
            Profile::DEFAULT
        );
    }

    #[test]
    fn a_small_picture_is_never_enlarged() {
        let tiny = jpeg(&photo(32, 24));
        for profile in [Profile::DEFAULT, Profile::CLAUDE_CODE] {
            let out = prepare(&tiny, profile).unwrap();
            assert_eq!((out.width, out.height), (32, 24));
        }
    }

    #[test]
    fn transparency_keeps_the_png() {
        let png = transparent_png(200, 150);
        let out = prepare(&png, Profile::DEFAULT).unwrap();
        assert_eq!(out.mime, "image/png");
        assert_eq!((out.width, out.height), (200, 150));
        assert!(out.bytes.len() <= IMAGE_MAX_BYTES);
        assert!(
            image::load_from_memory(&out.bytes)
                .unwrap()
                .color()
                .has_alpha()
        );
    }

    #[test]
    fn an_opaque_png_does_not_stay_one() {
        let opaque = encode_png(&DynamicImage::ImageRgb8(photo(400, 300))).unwrap();
        let out = prepare(&opaque, Profile::DEFAULT).unwrap();
        assert_eq!(out.mime, "image/jpeg");
        assert_eq!((out.width, out.height), (400, 300));
    }

    #[test]
    fn a_png_that_does_not_fit_becomes_a_jpeg() {
        // The transparent chequerboard is expensive as PNG; a tight cap makes
        // it fall through to the JPEG ladder, alpha composited onto white.
        let png = transparent_png(400, 400);
        let tight = Profile {
            long_side: 400,
            max_bytes: 1_500,
        };
        let out = prepare(&png, tight).unwrap();
        assert_eq!(out.mime, "image/jpeg");
        // The retry halved the picture when quality 60 was not enough.
        assert_eq!((out.width, out.height), (200, 200));
    }

    #[test]
    fn a_gif_comes_back_as_its_first_frame() {
        let out = prepare(&animated_gif(), Profile::DEFAULT).unwrap();
        assert_eq!(out.mime, "image/jpeg");
        assert_eq!((out.width, out.height), (64, 48));
        let pixel = image::load_from_memory(&out.bytes).unwrap().to_rgb8();
        let Rgb([r, g, b]) = *pixel.get_pixel(32, 24);
        assert!(
            r > 150 && g < 100 && b < 100,
            "first frame is red: {r},{g},{b}"
        );
    }

    #[test]
    fn heic_and_svg_are_link_only() {
        let mut heic = vec![0u8; 4];
        heic.extend_from_slice(b"ftypheic");
        heic.extend_from_slice(&[0u8; 16]);
        assert_eq!(
            prepare(&heic, Profile::DEFAULT),
            Err(ImageError::LinkOnly("HEIC"))
        );
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="9" height="9"/></svg>"#;
        assert_eq!(
            prepare(svg, Profile::DEFAULT),
            Err(ImageError::LinkOnly("SVG"))
        );
        let declared = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"></svg>"#;
        assert_eq!(
            prepare(declared, Profile::DEFAULT),
            Err(ImageError::LinkOnly("SVG"))
        );
        assert!(
            prepare(&heic, Profile::DEFAULT)
                .unwrap_err()
                .to_string()
                .contains("link only")
        );
    }

    #[test]
    fn nonsense_is_a_decode_error_and_never_a_panic() {
        assert!(matches!(
            prepare(b"", Profile::DEFAULT),
            Err(ImageError::Decode(_))
        ));
        assert!(matches!(
            prepare(b"not a picture at all", Profile::DEFAULT),
            Err(ImageError::Decode(_))
        ));
        // A JPEG header with nothing behind it.
        assert!(prepare(&[0xff, 0xd8, 0xff, 0xe0, 0, 0], Profile::DEFAULT).is_err());
    }
}
