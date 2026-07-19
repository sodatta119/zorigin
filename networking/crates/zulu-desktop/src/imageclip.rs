//! Small clipboard-image sync.
//!
//! A clip is normally text, but Zulu also syncs small images. An image travels
//! as a `data:image/png;base64,...` URL - just a (longer) string - so it rides
//! the exact same `/clip` + SSE path as text, and the web receiver can render it
//! with a plain `<img>`. Images are downscaled and size-capped so one screenshot
//! can't blow past the server's clip-size limit or bloat the history ring.
//!
//! Encoding is deterministic (same pixels -> same PNG bytes -> same data URL),
//! which is what lets the sync engine's content guard treat an image clip like
//! any other string and still break the echo loop.

use std::borrow::Cow;
use std::io::Cursor;

use arboard::{Clipboard, ImageData};
use image::{DynamicImage, ImageFormat, RgbaImage};

/// Longest side an image is scaled down to before sending.
const MAX_DIM: u32 = 1600;
/// Largest PNG we'll send (keeps the base64 body under the server's 1 MiB clip
/// cap and the history ring reasonable). Bigger images are skipped, not sent
/// blurry - stated plainly rather than silently degrading.
const MAX_PNG_BYTES: usize = 700_000;

const PREFIX: &str = "data:image/png;base64,";

/// True if `s` is an image clip this module produced.
pub fn is_image(s: &str) -> bool {
    s.starts_with("data:image/")
}

/// Read the OS clipboard image (if any) as a size-capped PNG data URL. Returns
/// `None` when there's no image, or it's too large after downscaling.
pub fn read_image_data_url(clip: &mut Clipboard) -> Option<String> {
    let img = clip.get_image().ok()?;
    rgba_to_data_url(img.width as u32, img.height as u32, &img.bytes)
}

/// Decode an image data URL and put it on the OS clipboard. Returns whether it
/// was applied.
pub fn apply_image_data_url(clip: &mut Clipboard, url: &str) -> bool {
    match data_url_to_rgba(url) {
        Some((w, h, bytes)) => clip
            .set_image(ImageData { width: w as usize, height: h as usize, bytes: Cow::Owned(bytes) })
            .is_ok(),
        None => false,
    }
}

// ---- pure conversions (unit-tested) ----

/// Encode RGBA8 pixels to a downscaled, size-capped PNG data URL.
fn rgba_to_data_url(w: u32, h: u32, rgba: &[u8]) -> Option<String> {
    let img = RgbaImage::from_raw(w, h, rgba.to_vec())?;
    let mut dynimg = DynamicImage::ImageRgba8(img);
    if w.max(h) > MAX_DIM {
        // Preserves aspect ratio; fits within MAX_DIM x MAX_DIM.
        dynimg = dynimg.resize(MAX_DIM, MAX_DIM, image::imageops::FilterType::Triangle);
    }
    let mut png = Vec::new();
    dynimg.write_to(&mut Cursor::new(&mut png), ImageFormat::Png).ok()?;
    if png.len() > MAX_PNG_BYTES {
        return None;
    }
    Some(format!("{PREFIX}{}", b64_encode(&png)))
}

/// Decode a PNG data URL back to `(width, height, rgba8)`.
fn data_url_to_rgba(url: &str) -> Option<(u32, u32, Vec<u8>)> {
    let b64 = url.split("base64,").nth(1)?;
    let bytes = b64_decode(b64)?;
    let rgba = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Some((w, h, rgba.into_raw()))
}

// ---- base64 (dependency-free) ----

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b'=').collect();
    let mut out = Vec::with_capacity(clean.len() / 4 * 3);
    for chunk in clean.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_arbitrary_bytes() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar", &[0u8, 255, 1, 254, 127]] {
            assert_eq!(b64_decode(&b64_encode(data)).unwrap(), data, "round trip {data:?}");
        }
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64_encode(b"Ma"), "TWE=");
    }

    #[test]
    fn image_round_trips_pixel_exact() {
        // A tiny 2x2 RGBA image survives encode -> data URL -> decode unchanged.
        let px = vec![
            255, 0, 0, 255, // red
            0, 255, 0, 255, // green
            0, 0, 255, 255, // blue
            255, 255, 0, 255, // yellow
        ];
        let url = rgba_to_data_url(2, 2, &px).expect("encode");
        assert!(is_image(&url) && url.starts_with(PREFIX));
        let (w, h, back) = data_url_to_rgba(&url).expect("decode");
        assert_eq!((w, h), (2, 2));
        assert_eq!(back, px, "pixels preserved");
    }

    #[test]
    fn encoding_is_deterministic() {
        // Same pixels twice must yield the identical data URL - this is what lets
        // the sync engine's string content-guard dedup images and avoid an echo
        // loop.
        let px = vec![10, 20, 30, 255, 40, 50, 60, 255];
        let a = rgba_to_data_url(2, 1, &px).unwrap();
        let b = rgba_to_data_url(2, 1, &px).unwrap();
        assert_eq!(a, b);
    }
}
