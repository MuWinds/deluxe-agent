//! Attachments: path references for the user, durable copies for `read_image`.
//!
//! A file the user attaches is kept as an [`Attachment`] — a path plus a name
//! and size for display. Nothing is copied: the user turn carries the path and
//! the model reads the file with `read_file` (text) or `read_image` (images),
//! so any format works without the host understanding it.
//!
//! An image the model reads through `read_image` is a different thing: the tool
//! copies it into a store under the config directory and returns an
//! [`ImageRef`] — id, media type, byte size, pixel dimensions — whose bytes are
//! inlined when the request is assembled. The copy is what makes a follow-up
//! question still work: a reference to the original path would break the moment
//! the file was edited, moved, or deleted, while a copy is immutable, so the
//! image the model saw is the image it sees again on every later turn.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::error::{AgentError, Result};
use crate::image_ops;

/// The largest image accepted, before base64 inflates it by a third.
pub const MAX_IMAGE_BYTES: usize = 1_048_576;

/// A path reference to a file the user attached to a prompt.
///
/// The bytes never travel: the user turn carries the path and the model reads
/// it with `read_file` (text) or `read_image` (images).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    /// The absolute path.
    ///
    /// Stored as a `String` rather than a `PathBuf`: `PathBuf` serialises via
    /// `Path::to_str` and errors on a non-UTF-8 path, and a failed session load
    /// discards the whole store. A non-UTF-8 path is refused in [`from_paths`]
    /// instead.
    pub path: String,
    /// The file's name, for display.
    pub name: String,
    /// The file's size in bytes, for display.
    pub bytes: u64,
}

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
        let dir = store_dir().ok_or_else(|| {
            AgentError::internal("No config directory is available on this system")
        })?;
        let bytes = std::fs::read(self.path_in(&dir))
            .map_err(|error| AgentError::from_io("Failed to read a stored image", error))?;
        Ok(data_url_from(&bytes, &self.media_type))
    }

    /// Where the bytes live under `dir`.
    fn path_in(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.{}", self.id, extension(&self.media_type)))
    }
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
/// calls it for a file the model asked about, and [`store_blob`] calls it for a
/// pasted bitmap. An image already within the pixel budget is kept
/// byte-for-byte; an over-budget PNG or JPEG is decoded, downscaled, and
/// re-encoded, with the budget halved and retried a bounded number of times —
/// a lossless re-encode of a photograph can still land over the byte cap.
pub fn prepare(media_type: &str, bytes: &[u8], width: u32, height: u32) -> Result<Prepared> {
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

/// Builds one path reference per file, all-or-nothing.
///
/// A directory, a missing path, or a path that is not valid UTF-8 is refused
/// with a message that names it — a bad entry leaves the composer exactly as it
/// was rather than attaching half a batch.
pub fn from_paths(paths: &[PathBuf]) -> Result<Vec<Attachment>> {
    let mut attachments = Vec::with_capacity(paths.len());
    for path in paths {
        let metadata = std::fs::metadata(path).map_err(|error| {
            AgentError::from_io(&format!("Failed to read `{}`", path.display()), error)
        })?;
        if metadata.is_dir() {
            return Err(AgentError::invalid_params(format!(
                "`{}` is a directory; attach a file",
                path.display()
            )));
        }
        let path = path
            .to_str()
            .ok_or_else(|| {
                AgentError::invalid_params(format!(
                    "`{}` is not valid UTF-8 and cannot be attached",
                    path.display()
                ))
            })?
            .to_string();
        let name = Path::new(&path)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(path.as_str())
            .to_string();
        attachments.push(Attachment {
            path,
            name,
            bytes: metadata.len(),
        });
    }
    Ok(attachments)
}

/// Stores one clipboard bitmap that has no path of its own, returning a
/// reference to the copy under the store.
///
/// A screenshot or a "copy image" hands over pixels, which the caller has
/// already re-encoded as a PNG, so there is no extension to check and nothing
/// to name. The bytes still run through [`prepare`], so an oversized image is
/// downscaled here rather than refused later by `read_image`.
pub fn store_blob(bytes: &[u8], name: Option<String>) -> Result<Attachment> {
    let media_type = sniff(bytes).ok_or_else(|| {
        AgentError::invalid_params("the pasted image is not a supported PNG/JPEG/WebP/GIF")
    })?;
    let (width, height) = image_ops::dimensions(media_type, bytes).unwrap_or((0, 0));
    let prepared = prepare(media_type, bytes, width, height)?;

    let dir = store_dir()
        .ok_or_else(|| AgentError::internal("No config directory is available on this system"))?;
    std::fs::create_dir_all(&dir)
        .map_err(|error| AgentError::from_io("Failed to create the attachment store", error))?;

    let file_name = format!(
        "{}.{}",
        uuid::Uuid::new_v4(),
        extension(prepared.media_type)
    );
    std::fs::write(dir.join(&file_name), &prepared.bytes)
        .map_err(|error| AgentError::from_io("Failed to store a pasted image", error))?;

    let path = dir
        .join(&file_name)
        .to_str()
        .ok_or_else(|| AgentError::internal("the attachment path is not valid UTF-8"))?
        .to_string();
    Ok(Attachment {
        path,
        name: name.unwrap_or(file_name),
        bytes: prepared.bytes.len() as u64,
    })
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
        assert_eq!(
            sniff(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]),
            Some("image/png")
        );
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
        let image = save_in(dir.path(), &bytes, "image/png", None, 1, 1).expect("the image stores");

        assert_eq!(image.bytes, 8);
        assert_eq!(image.media_type, "image/png");
        assert!(image.path_in(dir.path()).exists());
        assert_eq!(
            data_url_from(&bytes, "image/png"),
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn from_paths_builds_a_reference_for_each_file() {
        let dir = tempfile::tempdir().expect("a temp directory is available");
        let first = dir.path().join("notes.txt");
        let second = dir.path().join("shot.png");
        std::fs::write(&first, b"hello").expect("the file writes");
        std::fs::write(&second, b"png").expect("the file writes");

        let attachments =
            from_paths(&[first.clone(), second.clone()]).expect("both files are attached");

        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].name, "notes.txt");
        assert_eq!(attachments[0].bytes, 5);
        assert_eq!(attachments[0].path, first.to_str().unwrap());
        assert_eq!(attachments[1].name, "shot.png");
    }

    #[test]
    fn from_paths_refuses_a_directory() {
        let dir = tempfile::tempdir().expect("a temp directory is available");
        let error = from_paths(&[dir.path().to_path_buf()]).unwrap_err();
        assert!(error.to_string().contains("directory"), "{error}");
    }

    #[test]
    fn from_paths_refuses_a_missing_path() {
        let dir = tempfile::tempdir().expect("a temp directory is available");
        let missing = dir.path().join("gone.txt");
        assert!(from_paths(&[missing]).is_err());
    }

    #[test]
    fn store_blob_writes_the_png_under_the_store() {
        let config_dir = tempfile::tempdir().expect("a temp directory is available");
        std::env::set_var(crate::config::CONFIG_DIR_ENV, config_dir.path());
        let bytes = png_of(8, 8);

        let attachment = store_blob(&bytes, None).expect("the pasted image stores");

        assert!(std::path::Path::new(&attachment.path).exists());
        assert_eq!(attachment.bytes, bytes.len() as u64);
    }

    #[test]
    fn a_blob_that_is_not_an_image_is_refused_before_anything_is_stored() {
        // The paste path's guard: a clipboard that holds neither files nor a
        // bitmap must fail with a clear message, not write a garbage attachment.
        let error = store_blob(b"not an image", None).unwrap_err();
        assert!(error.to_string().contains("supported"), "{error}");
    }
}
