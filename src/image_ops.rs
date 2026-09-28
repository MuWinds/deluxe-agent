//! Decode → resample → re-encode, without an image-processing library.
//!
//! `read_image` has to shrink an oversized image before it reaches the
//! provider, and a provider only accepts a real encoded image — so shrinking
//! means decode, resample, and re-encode. The resample and the encode are
//! written here; the decoders are the two that are already compiled into this
//! binary: `png` for PNG and `zune-jpeg` for JPEG. That is the whole reason
//! this module handles exactly those two formats — GIF and WebP have no
//! decoder in the dependency tree, so `read_image` passes them through
//! untouched when they fit and refuses them when they do not.
//!
//! Nothing here touches the filesystem: the module is a pure transform over
//! bytes, which is what makes it testable without fixtures on disk.

use std::io::Cursor;

/// The largest image this module will decode, in pixels.
///
/// Enforced from the header *before* a decode, so a decompression bomb — a
/// small file that declares a gigapixel canvas — is refused on its declared
/// size instead of after the allocation it would cause.
pub const MAX_PIXELS: u64 = 640_000;

/// A decoded, 8-bit RGBA raster.
pub struct Raster {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, row-major, straight (non-premultiplied) RGBA.
    pub rgba: Vec<u8>,
}

/// Whether this module can decode the format, and therefore resize it.
///
/// A format it cannot decode is still readable: `read_image` sends it as-is
/// when it fits the budget.
pub fn can_decode(media_type: &str) -> bool {
    matches!(media_type, "image/png" | "image/jpeg")
}

/// Reads the pixel dimensions from a supported image's header.
///
/// Header-only on purpose — see [`MAX_PIXELS`]. Returns `None` for a format it
/// does not recognise or a header that is truncated.
pub fn dimensions(media_type: &str, bytes: &[u8]) -> Option<(u32, u32)> {
    match media_type {
        "image/png" => png_dimensions(bytes),
        "image/jpeg" => jpeg_dimensions(bytes),
        "image/gif" => gif_dimensions(bytes),
        "image/webp" => webp_dimensions(bytes),
        _ => None,
    }
}

/// The largest size no larger than `width` x `height` that fits `max_pixels`,
/// preserving the aspect ratio.
pub fn fit_within(width: u32, height: u32, max_pixels: u64) -> (u32, u32) {
    let pixels = width as u64 * height as u64;
    if pixels == 0 || pixels <= max_pixels {
        return (width.max(1), height.max(1));
    }
    let scale = (max_pixels as f64 / pixels as f64).sqrt();
    let width = ((width as f64 * scale).floor() as u32).max(1);
    let height = ((height as f64 * scale).floor() as u32).max(1);
    (width, height)
}

/// Decodes a PNG into RGBA8.
pub fn decode_png(bytes: &[u8]) -> Result<Raster, String> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    // Expand palette to RGB, grayscale to 8-bit, and tRNS to an alpha channel,
    // so every colour type lands on the same 8-bit path below.
    decoder.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
    );

    let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
    let buffer_size = reader
        .output_buffer_size()
        .ok_or_else(|| "the PNG frame is too large to decode".to_string())?;
    let mut buffer = vec![0u8; buffer_size];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| error.to_string())?;

    let rgba = expand_to_rgba(&buffer[..info.buffer_size()], info.color_type)?;
    Ok(Raster {
        width: info.width,
        height: info.height,
        rgba,
    })
}

/// Decodes a baseline or progressive JPEG into RGBA8.
pub fn decode_jpeg(bytes: &[u8]) -> Result<Raster, String> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;

    // `ZCursor`, not a bare `&[u8]`: the decoder's reader trait wants `Seek`.
    let mut decoder = zune_jpeg::JpegDecoder::new(ZCursor::new(bytes));
    let pixels = decoder.decode().map_err(|error| error.to_string())?;
    let info = decoder
        .info()
        .ok_or_else(|| "the JPEG declares no size".to_string())?;
    let space = decoder
        .output_colorspace()
        .ok_or_else(|| "the JPEG declares no colour space".to_string())?;

    let rgba = match space {
        ColorSpace::RGB => to_rgba_from_rgb(&pixels),
        ColorSpace::RGBA => pixels,
        ColorSpace::Luma => to_rgba_from_luma(&pixels, false),
        ColorSpace::LumaA => to_rgba_from_luma(&pixels, true),
        // CMYK/YCCK JPEGs are a print format and vanishingly rare here. Refusing
        // them with a clear message beats a silent colour-space bug.
        other => return Err(format!("unsupported JPEG colour space {other:?}")),
    };

    Ok(Raster {
        width: info.width as u32,
        height: info.height as u32,
        rgba,
    })
}

/// Downscales to exactly `width` x `height` with a box (area-average) filter.
///
/// Area averaging is the right filter for a pure downscale: every source pixel
/// in the destination pixel's footprint contributes, so detail is averaged
/// rather than sampled away. Nearest-neighbour would alias; bilinear would
/// skip source rows when the ratio is large.
pub fn downsample(source: &Raster, width: u32, height: u32) -> Raster {
    let (source_width, source_height) = (source.width as u64, source.height as u64);
    let (target_width, target_height) = (width.max(1) as u64, height.max(1) as u64);
    let mut rgba = vec![0u8; (target_width * target_height * 4) as usize];

    for y in 0..target_height {
        let y0 = y * source_height / target_height;
        let y1 = ((y + 1) * source_height / target_height).max(y0 + 1);
        for x in 0..target_width {
            let x0 = x * source_width / target_width;
            let x1 = ((x + 1) * source_width / target_width).max(x0 + 1);

            let mut sum = [0u32; 4];
            let mut count = 0u32;
            for sy in y0..y1.min(source_height) {
                for sx in x0..x1.min(source_width) {
                    let index = ((sy * source_width + sx) * 4) as usize;
                    sum[0] += source.rgba[index] as u32;
                    sum[1] += source.rgba[index + 1] as u32;
                    sum[2] += source.rgba[index + 2] as u32;
                    sum[3] += source.rgba[index + 3] as u32;
                    count += 1;
                }
            }

            let count = count.max(1);
            let index = ((y * target_width + x) * 4) as usize;
            rgba[index] = (sum[0] / count) as u8;
            rgba[index + 1] = (sum[1] / count) as u8;
            rgba[index + 2] = (sum[2] / count) as u8;
            rgba[index + 3] = (sum[3] / count) as u8;
        }
    }

    Raster {
        width: target_width as u32,
        height: target_height as u32,
        rgba,
    }
}

/// Encodes an RGBA8 raster as a PNG.
///
/// PNG rather than JPEG because it is lossless and, more to the point, because
/// this binary already carries a PNG *encoder*; it carries no JPEG encoder.
/// The output is checked against the byte budget by the caller, which shrinks
/// again if a lossless re-encode of a photographic image came out too large.
pub fn encode_png(raster: &Raster) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, raster.width, raster.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
        writer
            .write_image_data(&raster.rgba)
            .map_err(|error| error.to_string())?;
        writer.finish().map_err(|error| error.to_string())?;
    }
    Ok(out)
}

fn expand_to_rgba(samples: &[u8], color_type: png::ColorType) -> Result<Vec<u8>, String> {
    Ok(match color_type {
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(samples.len() * 4);
            for &grey in samples {
                out.extend_from_slice(&[grey, grey, grey, 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(samples.len() / 2 * 4);
            for pixel in samples.chunks_exact(2) {
                out.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
            out
        }
        png::ColorType::Rgb => to_rgba_from_rgb(samples),
        png::ColorType::Rgba => samples.to_vec(),
        png::ColorType::Indexed => {
            return Err("an indexed PNG was not expanded by the decoder".into())
        }
    })
}

fn to_rgba_from_rgb(samples: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() / 3 * 4);
    for pixel in samples.chunks_exact(3) {
        out.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
    }
    out
}

fn to_rgba_from_luma(samples: &[u8], has_alpha: bool) -> Vec<u8> {
    let stride = if has_alpha { 2 } else { 1 };
    let mut out = Vec::with_capacity(samples.len() / stride * 4);
    for pixel in samples.chunks_exact(stride) {
        let alpha = if has_alpha { pixel[1] } else { 255 };
        out.extend_from_slice(&[pixel[0], pixel[0], pixel[0], alpha]);
    }
    out
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || bytes[..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((width, height))
}

fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 10 || (&bytes[..6] != b"GIF87a" && &bytes[..6] != b"GIF89a") {
        return None;
    }
    let width = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;
    let height = u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32;
    Some((width, height))
}

/// Walks JPEG segments to the start-of-frame marker, which carries the size.
///
/// A scan of the marker list rather than a full decode, so a huge JPEG can be
/// sized (and refused) without being decoded.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }

    let mut index = 2usize;
    while index + 3 < bytes.len() {
        if bytes[index] != 0xFF {
            index += 1;
            continue;
        }
        let marker = bytes[index + 1];
        // A run of 0xFF is padding before the real marker.
        if marker == 0xFF {
            index += 1;
            continue;
        }
        // Standalone markers (RSTn, SOI, EOI, TEM) carry no length field.
        if (0xD0..=0xD9).contains(&marker) {
            index += 2;
            continue;
        }

        let length = u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]) as usize;
        if length < 2 {
            return None;
        }
        // SOF0..SOF15, minus DHT (C4), JPG (C8) and DAC (CC), which sit in the
        // same numeric range but are not frame headers.
        if (0xC0..=0xCF).contains(&marker)
            && marker != 0xC4
            && marker != 0xC8
            && marker != 0xCC
        {
            if index + 9 > bytes.len() {
                return None;
            }
            let height = u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]) as u32;
            let width = u16::from_be_bytes([bytes[index + 7], bytes[index + 8]]) as u32;
            return Some((width, height));
        }
        index += 2 + length;
    }
    None
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 30 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }

    match &bytes[12..16] {
        // Extended format: 24-bit canvas size, stored minus one.
        b"VP8X" => {
            let width = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]) + 1;
            let height = u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]) + 1;
            Some((width, height))
        }
        // Lossy: 14-bit size after the 0x9D 0x01 0x2A start code.
        b"VP8 " => {
            if bytes[23] != 0x9D || bytes[24] != 0x01 || bytes[25] != 0x2A {
                return None;
            }
            let width = (u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3FFF) as u32;
            let height = (u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3FFF) as u32;
            Some((width, height))
        }
        // Lossless: a signature byte, then two 14-bit sizes minus one.
        b"VP8L" => {
            if bytes[20] != 0x2F {
                return None;
            }
            let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = PNG_SIGNATURE.to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    #[test]
    fn reads_png_dimensions_from_the_header() {
        assert_eq!(png_dimensions(&png_header(800, 600)), Some((800, 600)));
    }

    #[test]
    fn reads_gif_dimensions_from_the_header() {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&320u16.to_le_bytes());
        bytes.extend_from_slice(&240u16.to_le_bytes());
        assert_eq!(gif_dimensions(&bytes), Some((320, 240)));
    }

    #[test]
    fn reads_jpeg_dimensions_from_the_start_of_frame() {
        // SOI, a fill segment (APP0), then SOF0 carrying the size.
        let mut bytes = vec![0xFF, 0xD8];
        bytes.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]);
        bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        bytes.extend_from_slice(&300u16.to_be_bytes());
        bytes.extend_from_slice(&400u16.to_be_bytes());
        assert_eq!(jpeg_dimensions(&bytes), Some((400, 300)));
    }

    #[test]
    fn reads_webp_dimensions_from_the_extended_header() {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(b"VP8X");
        bytes.extend_from_slice(&[0; 8]);
        // Canvas size is stored minus one.
        bytes.extend_from_slice(&[0x3F, 0x02, 0x00]); // 576 - 1
        bytes.extend_from_slice(&[0x1F, 0x01, 0x00]); // 288 - 1
        assert_eq!(webp_dimensions(&bytes), Some((576, 288)));
    }

    #[test]
    fn an_unknown_format_has_no_dimensions() {
        assert_eq!(dimensions("image/bmp", &[0; 64]), None);
    }

    #[test]
    fn fit_within_preserves_the_aspect_ratio() {
        // 2000x1000 is 2M pixels; a 500k budget is a quarter, so both sides halve.
        assert_eq!(fit_within(2000, 1000, 500_000), (1000, 500));
    }

    #[test]
    fn fit_within_leaves_a_small_image_alone() {
        assert_eq!(fit_within(100, 100, 640_000), (100, 100));
    }

    #[test]
    fn a_raster_survives_an_encode_decode_round_trip() {
        let raster = Raster {
            width: 2,
            height: 2,
            rgba: vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 10, 20, 30, 40,
            ],
        };
        let encoded = encode_png(&raster).expect("the raster encodes");
        let decoded = decode_png(&encoded).expect("the PNG decodes");
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(decoded.rgba, raster.rgba);
    }

    #[test]
    fn downsampling_averages_each_footprint() {
        // Four pixels: two black, two white. Halving to 1x1 must average to mid-grey.
        let raster = Raster {
            width: 2,
            height: 2,
            rgba: vec![0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255],
        };
        let scaled = downsample(&raster, 1, 1);
        assert_eq!((scaled.width, scaled.height), (1, 1));
        assert_eq!(scaled.rgba, vec![127, 127, 127, 255]);
    }

    #[test]
    fn a_truncated_header_is_not_mistaken_for_a_size() {
        assert_eq!(png_dimensions(&PNG_SIGNATURE), None);
        assert_eq!(jpeg_dimensions(&[0xFF, 0xD8]), None);
    }
}
