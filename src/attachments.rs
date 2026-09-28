//! Durable image attachments.
//!
//! An image the model reads is copied into a store under the config directory
//! and referenced by a small [`ImageRef`] — id, media type, byte size, pixel
//! dimensions. The bytes never travel inside a reference: the session log keeps
//! the reference and the bytes are read back only when a request is assembled.
//! That is what keeps a conversation that read a dozen screenshots from
//! blowing past the session store's size cap.
//!
//! The copy is what makes a follow-up question still work. A reference to the
//! original path would break the moment the file was edited, moved, or deleted;
//! a copy is immutable, so the image the model saw is the image it sees again
//! on every later turn.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::error::{AgentError, Result};
use crate::image_ops;

/// The largest image accepted, before base64 inflates it by a third.
pub const MAX_IMAGE_BYTES: usize = 1_048_576;

/// How many queued bytes the composer will hold before it refuses another
/// image.
///
/// The whole queue rides in every later request, so an unbounded strip would
/// quietly eat the context window on the next send. The cap is a multiple of
/// the per-image limit rather than a round number of megabytes so the two stay
/// in step if the per-image limit ever moves.
const MAX_QUEUED_IMAGE_BYTES: usize = 8 * MAX_IMAGE_BYTES;

/// A durable reference to a stored image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRef {
    pub id: String,
    pub media_type: String,
    pub bytes: usize,
    pub width: u32,
    pub height: u32,
    /// The source file's name, for display. Never used to locate the copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ImageRef {
    /// The stored bytes as a `data:` URL, which is what the wire format wants.
    pub fn data_url(&self) -> Result<String> {
        let dir = store_dir()
            .ok_or_else(|| AgentError::internal("No config directory is available on this system"))?;
        let bytes = std::fs::read(self.path_in(&dir)).map_err(|error| {
            AgentError::from_io("Failed to read a stored image", error)
        })?;
        Ok(data_url_from(&bytes, &self.media_type))
    }

    /// Where the bytes live under `dir`.
    fn path_in(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.{}", self.id, extension(&self.media_type)))
    }
}

/// Reads a stored image's bytes back.
///
/// `data_url` is the wire form of this; the transcript wants the raw bytes so
/// it can decode a thumbnail without re-parsing base64.
pub fn load_bytes(image: &ImageRef) -> Result<Vec<u8>> {
    let dir = store_dir()
        .ok_or_else(|| AgentError::internal("No config directory is available on this system"))?;
    std::fs::read(image.path_in(&dir)).map_err(|error| {
        AgentError::from_io("Failed to read a stored image", error)
    })
}

/// Encodes bytes as the `data:` URL the OpenAI image part expects.
pub fn data_url_from(bytes: &[u8], media_type: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("data:{media_type};base64,{encoded}")
}

/// Where stored images live. Created on demand by [`save`].
pub fn store_dir() -> Option<PathBuf> {
    config::config_dir().map(|dir| dir.join("attachments"))
}

/// Copies `bytes` into the store and returns a reference to the copy.
pub fn save(
    bytes: &[u8],
    media_type: &str,
    name: Option<String>,
    width: u32,
    height: u32,
) -> Result<ImageRef> {
    let dir = store_dir()
        .ok_or_else(|| AgentError::internal("No config directory is available on this system"))?;
    save_in(&dir, bytes, media_type, name, width, height)
}

/// The body of [`save`], with the store directory passed in so a test can
/// round-trip through a temporary directory instead of the user's real one.
pub fn save_in(
    dir: &Path,
    bytes: &[u8],
    media_type: &str,
    name: Option<String>,
    width: u32,
    height: u32,
) -> Result<ImageRef> {
    std::fs::create_dir_all(dir)
        .map_err(|error| AgentError::from_io("Failed to create the attachment store", error))?;

    let image = ImageRef {
        id: uuid::Uuid::new_v4().to_string(),
        media_type: media_type.to_string(),
        bytes: bytes.len(),
        width,
        height,
        name,
    };

    std::fs::write(image.path_in(dir), bytes)
        .map_err(|error| AgentError::from_io("Failed to store an image", error))?;

    Ok(image)
}

/// The media type an extension declares, if it is one `read_image` accepts.
///
/// Takes the extension alone (no dot) so the caller can tell "no extension"
/// from "an extension we do not know" — the first falls back to sniffing, the
/// second is an error.
pub fn media_type_for_extension(extension: &str) -> Option<&'static str> {
    match extension.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

/// The file extension for a stored media type.
pub fn extension(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

/// The media type declared by a supported image's file signature.
///
/// Used when the path carries no usable extension, and to catch a file whose
/// extension disagrees with its content.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    if bytes.starts_with(&PNG_SIGNATURE) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// One candidate image run through admission, ready for the store.
///
/// `media_type`/`bytes` are what to keep; `width`/`height` may both have been
/// changed by the resample. `None` means the image is not storable: over the
/// byte cap in a format this binary cannot resample.
#[derive(Debug)]
pub struct Prepared {
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Runs one candidate image through admission, in place, without storing it.
///
/// This is the whole admission rule, shared by both entrances: `read_image`
/// calls it for a file the model asked about, and the composer's intake —
/// [`store_from_paths`] and [`store_from_bytes`] — calls it for a pasted or
/// dropped image. An image already within the pixel budget is kept
/// byte-for-byte; an over-budget PNG or JPEG is decoded, downscaled, and
/// re-encoded, with the budget halved and retried a bounded number of times —
/// a lossless re-encode of a photograph can still land over the byte cap.
pub fn prepare(
    media_type: &str,
    bytes: &[u8],
    width: u32,
    height: u32,
) -> Result<Prepared> {
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(AgentError::invalid_params(format!(
            "the image is {} bytes, over the {MAX_IMAGE_BYTES} byte attachment limit",
            bytes.len()
        )));
    }

    let known = width > 0 && height > 0;
    let over_budget = known && width as u64 * height as u64 > image_ops::MAX_PIXELS;
    if !over_budget {
        return Ok(Prepared {
            media_type: static_media_type(media_type),
            bytes: bytes.to_vec(),
            width,
            height,
        });
    }

    if !image_ops::can_decode(media_type) {
        return Err(AgentError::invalid_params(format!(
            "the image is {width}x{height} px, over the {} pixel limit, and {media_type} cannot \
             be downscaled here",
            image_ops::MAX_PIXELS
        )));
    }

    let raster = match media_type {
        "image/png" => image_ops::decode_png(bytes),
        "image/jpeg" => image_ops::decode_jpeg(bytes),
        other => Err(format!("{other} cannot be decoded here")),
    }
    .map_err(|error| AgentError::invalid_params(format!("the image cannot be decoded: {error}")))?;

    let mut budget = image_ops::MAX_PIXELS;
    for _ in 0..4 {
        let (width, height) = image_ops::fit_within(raster.width, raster.height, budget);
        let scaled = image_ops::downsample(&raster, width, height);
        let encoded = image_ops::encode_png(&scaled).map_err(AgentError::internal)?;
        if encoded.len() <= MAX_IMAGE_BYTES {
            return Ok(Prepared {
                media_type: "image/png",
                bytes: encoded,
                width,
                height,
            });
        }
        budget /= 2;
    }

    Err(AgentError::invalid_params(
        "the image could not be downscaled below the size limit",
    ))
}

/// Stores an image [`prepare`] already admitted. Split from [`prepare`] so the
/// store directory is only touched once a candidate has cleared the budgets —
/// a refusal must not leave a half-written attachment behind.
pub fn save_prepared(prepared: Prepared, name: Option<String>) -> Result<ImageRef> {
    save(
        &prepared.bytes,
        prepared.media_type,
        name,
        prepared.width,
        prepared.height,
    )
}

/// Reads one dropped or picked path into an admitted candidate, without
/// storing it.
///
/// The resolution mirrors `read_image`'s: a path with no extension lets the
/// content decide, a path whose extension names something other than a
/// supported image is a mistake worth reporting by name, and an extension that
/// disagrees with the content is refused rather than silently trusted. Nothing
/// reaches the store until [`prepare`] has cleared the budgets.
fn prepare_from_path(path: &Path) -> Result<(Prepared, Option<String>)> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let declared = if extension.is_empty() {
        None
    } else {
        match media_type_for_extension(extension) {
            Some(media_type) => Some(media_type),
            None => {
                return Err(AgentError::invalid_params(format!(
                    "`{}` has a `.{extension}` extension, which is not a supported image \
                     format; PNG/JPEG/WebP/GIF are accepted",
                    path.display()
                )))
            }
        }
    };

    let bytes = std::fs::read(path).map_err(|error| {
        AgentError::from_io(&format!("Failed to read `{}`", path.display()), error)
    })?;

    let media_type = match (declared, sniff(&bytes)) {
        (Some(declared), Some(sniffed)) if declared != sniffed => {
            return Err(AgentError::invalid_params(format!(
                "`{}` has a `.{extension}` extension ({declared}) but its content is \
                 {sniffed}; rename it to match its format",
                path.display()
            )))
        }
        (Some(declared), _) => declared,
        (None, Some(sniffed)) => sniffed,
        (None, None) => {
            return Err(AgentError::invalid_params(format!(
                "`{}` is not a supported image; PNG/JPEG/WebP/GIF are accepted",
                path.display()
            )))
        }
    };

    let (width, height) = image_ops::dimensions(media_type, &bytes).unwrap_or((0, 0));
    let prepared = prepare(media_type, &bytes, width, height)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::to_string);
    Ok((prepared, name))
}

/// Admits and stores a batch of dropped or picked files.
///
/// `queued_bytes` is what the composer already holds. The batch is checked as
/// a whole against [`MAX_QUEUED_IMAGE_BYTES`] before anything is written, so a
/// refusal leaves the strip exactly as it was — the all-or-nothing shape the
/// composer's intake promises.
pub fn store_from_paths(paths: &[PathBuf], queued_bytes: usize) -> Result<Vec<ImageRef>> {
    let mut total = queued_bytes;
    let mut admitted = Vec::with_capacity(paths.len());
    for path in paths {
        let (prepared, name) = prepare_from_path(path)?;
        total = total.saturating_add(prepared.bytes.len());
        if total > MAX_QUEUED_IMAGE_BYTES {
            return Err(AgentError::invalid_params(format!(
                "the queued images would pass the {} byte limit; send or remove some first",
                MAX_QUEUED_IMAGE_BYTES
            )));
        }
        admitted.push((prepared, name));
    }

    admitted
        .into_iter()
        .map(|(prepared, name)| save_prepared(prepared, name))
        .collect()
}

/// Admits and stores one image that arrived as bytes rather than as a path.
///
/// This is the clipboard's bitmap case: a screenshot or a "copy image" hands
/// over pixels, which the caller has already re-encoded as a PNG, so there is
/// no extension to check and nothing to name.
pub fn store_from_bytes(bytes: &[u8], name: Option<String>) -> Result<Vec<ImageRef>> {
    let media_type = sniff(bytes).ok_or_else(|| {
        AgentError::invalid_params("the pasted image is not a supported PNG/JPEG/WebP/GIF")
    })?;
    let (width, height) = image_ops::dimensions(media_type, bytes).unwrap_or((0, 0));
    let prepared = prepare(media_type, bytes, width, height)?;
    Ok(vec![save_prepared(prepared, name)?])
}

/// Narrows a runtime media-type string to one of the known static ones.
fn static_media_type(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" => "image/jpeg",
        "image/webp" => "image/webp",
        "image/gif" => "image/gif",
        _ => "image/png",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(width: u32, height: u32) -> Vec<u8> {
        let raster = crate::image_ops::Raster {
            width,
            height,
            rgba: vec![128u8; (width * height * 4) as usize],
        };
        crate::image_ops::encode_png(&raster).expect("the fixture encodes")
    }

    #[test]
    fn a_small_image_is_passed_through_prepare_untouched() {
        let bytes = png_of(10, 10);
        let prepared = prepare("image/png", &bytes, 10, 10).expect("a small image passes");
        assert_eq!(prepared.bytes, bytes);
        assert_eq!(prepared.media_type, "image/png");
        assert_eq!((prepared.width, prepared.height), (10, 10));
    }

    #[test]
    fn an_oversized_png_is_downscaled_and_re_encoded() {
        let (source_width, source_height) = (1200u32, 1200u32);
        let bytes = png_of(source_width, source_height);
        assert!(u64::from(source_width) * u64::from(source_height) > image_ops::MAX_PIXELS);

        let prepared = prepare("image/png", &bytes, source_width, source_height)
            .expect("an oversized PNG shrinks");

        assert_eq!(prepared.media_type, "image/png");
        assert!(prepared.width as u64 * prepared.height as u64 <= image_ops::MAX_PIXELS);
        // The re-encoded PNG is a different, smaller image than the source.
        assert_ne!(prepared.bytes, bytes);
        assert!(prepared.bytes.len() <= MAX_IMAGE_BYTES);
    }

    #[test]
    fn an_oversized_gif_is_refused() {
        let error = prepare("image/gif", &[0u8; 16], 4000, 4000).unwrap_err();
        assert!(error.to_string().contains("downscaled"), "{error}");
    }

    #[test]
    fn an_image_without_known_dimensions_is_passed_through() {
        let bytes = vec![0u8; 16];
        let prepared = prepare("image/webp", &bytes, 0, 0).expect("an unknown size passes");
        assert_eq!(prepared.bytes, bytes);
        assert_eq!((prepared.width, prepared.height), (0, 0));
    }

    #[test]
    fn an_over_byte_cap_image_is_refused_before_any_resampling() {
        let error = prepare("image/png", &vec![0u8; MAX_IMAGE_BYTES + 1], 1, 1).unwrap_err();
        assert!(error.to_string().contains("byte"), "{error}");
    }

    #[test]
    fn sniffs_each_supported_signature() {
        assert_eq!(sniff(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]), Some("image/png"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a...."), Some("image/gif"));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0; 4]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(sniff(&webp), Some("image/webp"));
        assert_eq!(sniff(b"not an image"), None);
    }

    #[test]
    fn maps_extensions_to_media_types() {
        assert_eq!(media_type_for_extension("PNG"), Some("image/png"));
        assert_eq!(media_type_for_extension("jpeg"), Some("image/jpeg"));
        assert_eq!(media_type_for_extension("bmp"), None);
        assert_eq!(media_type_for_extension(""), None);
    }

    #[test]
    fn a_stored_image_round_trips_through_a_data_url() {
        let dir = tempfile::tempdir().expect("a temp directory is available");
        let bytes = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let image = save_in(dir.path(), &bytes, "image/png", None, 1, 1)
            .expect("the image stores");

        assert_eq!(image.bytes, 8);
        assert_eq!(image.media_type, "image/png");
        assert!(image.path_in(dir.path()).exists());
        assert_eq!(
            data_url_from(&bytes, "image/png"),
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn bytes_that_are_not_an_image_are_refused_before_anything_is_stored() {
        // The paste path's guard: a clipboard that holds neither files nor a
        // bitmap must fail with a clear message, not write a garbage attachment.
        let error = store_from_bytes(b"not an image", None).unwrap_err();
        assert!(error.to_string().contains("supported"), "{error}");
    }

}
