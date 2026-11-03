//! Image sniffing: which of PNG, JPEG, WebP and GIF a file's first bytes are, and the size its
//! header declares, without decoding any pixel.
//!
//! The type comes from the magic bytes alone and a client's own type is never read. SVG is none
//! of the four, so it is never served as an image. An image whose header declares more than
//! [`MAX_SIDE`] pixels on a side or [`MAX_PIXELS`] in all is refused.

/// Most bytes of a file the header parse reads.
pub const PREFIX_BYTES: usize = 1 << 20;

/// Most pixels along either side of an image.
const MAX_SIDE: u32 = 8000;

/// Most pixels in an image, 40 megapixels.
const MAX_PIXELS: u64 = 40_000_000;

/// Returns the `Content-Type` of the image `head` starts, or [`None`] when it is no PNG, JPEG, WebP
/// or GIF, its header is cut off or invalid, or its size is over the caps.
#[must_use]
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    let (content_type, width, height) = if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        let (width, height) = png_size(head)?;
        ("image/png", width, height)
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        let (width, height) = jpeg_size(head)?;
        ("image/jpeg", width, height)
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        let (width, height) = gif_size(head)?;
        ("image/gif", width, height)
    } else if head.starts_with(b"RIFF") && head.get(8..12)? == b"WEBP" {
        let (width, height) = webp_size(head)?;
        ("image/webp", width, height)
    } else {
        return None;
    };
    let fits = width > 0
        && height > 0
        && width <= MAX_SIDE
        && height <= MAX_SIDE
        && u64::from(width) * u64::from(height) <= MAX_PIXELS;
    fits.then_some(content_type)
}

/// Returns whether `mime` is one of the four types [`sniff`] returns.
#[must_use]
pub fn is_image_type(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    )
}

/// Reads the big-endian `u32` at `at`.
fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Reads the big-endian `u16` at `at`.
fn be16(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_be_bytes(
        bytes.get(at..at + 2)?.try_into().ok()?,
    )))
}

/// Reads the little-endian `u16` at `at`.
fn le16(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_le_bytes(
        bytes.get(at..at + 2)?.try_into().ok()?,
    )))
}

/// Reads the little-endian 24-bit number at `at`.
fn le24(bytes: &[u8], at: usize) -> Option<u32> {
    let [a, b, c] = *bytes.get(at..at + 3)? else {
        return None;
    };
    Some(u32::from(a) | (u32::from(b) << 8) | (u32::from(c) << 16))
}

/// Returns the size in the IHDR chunk, which must come first.
fn png_size(head: &[u8]) -> Option<(u32, u32)> {
    if head.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((be32(head, 16)?, be32(head, 20)?))
}

/// Returns the size in the GIF logical screen descriptor.
fn gif_size(head: &[u8]) -> Option<(u32, u32)> {
    Some((le16(head, 6)?, le16(head, 8)?))
}

/// Returns the size of the first VP8, VP8L or VP8X chunk of a WebP file.
fn webp_size(head: &[u8]) -> Option<(u32, u32)> {
    match head.get(12..16)? {
        b"VP8 " => {
            if head.get(23..26)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            Some((le16(head, 26)? & 0x3FFF, le16(head, 28)? & 0x3FFF))
        }
        b"VP8L" => {
            if *head.get(20)? != 0x2F {
                return None;
            }
            let bits = u32::from_le_bytes(head.get(21..25)?.try_into().ok()?);
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        b"VP8X" => Some((le24(head, 24)? + 1, le24(head, 27)? + 1)),
        _ => None,
    }
}

/// Returns the size in the first start-of-frame segment of a JPEG, walking its segments.
fn jpeg_size(head: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2;
    loop {
        while *head.get(at)? != 0xFF {
            at += 1;
        }
        while *head.get(at)? == 0xFF {
            at += 1;
        }
        let marker = *head.get(at)?;
        at += 1;
        match marker {
            0x00 | 0x01 | 0xD0..=0xD8 => {}
            0xD9 | 0xDA => return None,
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                return Some((be16(head, at + 5)?, be16(head, at + 3)?));
            }
            _ => {
                let len = be16(head, at)? as usize;
                if len < 2 {
                    return None;
                }
                at += len;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        bytes.extend([8, 6, 0, 0, 0]);
        bytes
    }

    fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xC0, 0, 11, 8];
        bytes.extend(height.to_be_bytes());
        bytes.extend(width.to_be_bytes());
        bytes.extend([1, 1, 0x11, 0]);
        bytes
    }

    fn gif(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend(width.to_le_bytes());
        bytes.extend(height.to_le_bytes());
        bytes
    }

    fn webp_lossy(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"RIFF\0\0\0\0WEBPVP8 \0\0\0\0\0\0\0\x9D\x01\x2A".to_vec();
        bytes.extend(width.to_le_bytes());
        bytes.extend(height.to_le_bytes());
        bytes
    }

    fn webp_lossless(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"RIFF\0\0\0\0WEBPVP8L\0\0\0\0\x2F".to_vec();
        bytes.extend(((width - 1) | ((height - 1) << 14)).to_le_bytes());
        bytes
    }

    fn webp_extended(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"RIFF\0\0\0\0WEBPVP8X\0\0\0\0\0\0\0\0".to_vec();
        bytes.extend(&(width - 1).to_le_bytes()[..3]);
        bytes.extend(&(height - 1).to_le_bytes()[..3]);
        bytes
    }

    #[test]
    fn sniff_should_read_the_type_and_size_of_each_format() {
        assert_eq!(sniff(&png(30, 20)), Some("image/png"));
        assert_eq!(sniff(&jpeg(30, 20)), Some("image/jpeg"));
        assert_eq!(sniff(&gif(30, 20)), Some("image/gif"));
        assert_eq!(sniff(&webp_lossy(30, 20)), Some("image/webp"));
        assert_eq!(sniff(&webp_lossless(30, 20)), Some("image/webp"));
        assert_eq!(sniff(&webp_extended(30, 20)), Some("image/webp"));
    }

    #[test]
    fn sniff_should_refuse_svg_text_and_cut_off_headers() {
        assert_eq!(sniff(b"<svg xmlns='http://www.w3.org/2000/svg'/>"), None);
        assert_eq!(sniff(b"<?xml version='1.0'?><svg/>"), None);
        assert_eq!(sniff(b"plain text"), None);
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(&png(30, 20)[..18]), None);
        assert_eq!(sniff(&jpeg(30, 20)[..14]), None);
        assert_eq!(sniff(b"RIFF\0\0\0\0WAVEfmt "), None);
    }

    #[test]
    fn sniff_should_refuse_an_image_over_the_caps() {
        assert!(sniff(&png(8000, 5000)).is_some());
        assert_eq!(sniff(&png(8001, 10)), None);
        assert_eq!(sniff(&png(10, 8001)), None);
        assert_eq!(sniff(&png(7000, 6000)), None);
        assert_eq!(sniff(&png(0, 10)), None);
        assert_eq!(sniff(&jpeg(9000, 10)), None);
    }

    #[test]
    fn sniff_should_walk_jpeg_segments_to_the_frame_header() {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE1, 0, 6, 1, 2, 3, 4];
        bytes.extend(&jpeg(40, 50)[2..]);
        assert_eq!(sniff(&bytes), Some("image/jpeg"));
        let scan_first = [0xFF, 0xD8, 0xFF, 0xDA, 0, 2, 0xFF, 0xC0, 0, 11];
        assert_eq!(sniff(&scan_first), None);
    }
}
