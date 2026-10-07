//! `read_image`.
//!
//! Reads a PNG/JPEG/WebP/GIF and hands the picture itself to the model, the way
//! the DeepSeek Harness does. The tool is only registered for a model that
//! declares image input (see [`ToolRegistry::with_image_input`]), so unlike the
//! harness it does not also have to re-check the route at execution time.
//!
//! An image within the byte and pixel budgets is stored as it was read. One
//! that fits the bytes but not the pixels is decoded, downscaled, and
//! re-encoded as PNG first — see [`crate::image_ops`]. GIF and WebP have no
//! decoder in this binary, so an oversized one is refused with advice to shrink
//! it rather than sent as something the provider will reject.

use std::time::Instant;

use serde_json::{json, Value};

use super::settings::ToolSettings;
use super::{ContentBlock, ObjectSchema, Tool, ToolDescriptor, ToolOutput};
use crate::attachments;
use crate::error::{AgentError, Result};
use crate::image_ops;

pub struct ReadImage;

fn schema(properties: Value, required: &[&str]) -> ObjectSchema {
    ObjectSchema {
        schema_type: "object".into(),
        properties: serde_json::from_value(properties)
            .expect("schema properties must be an object"),
        required: required.iter().map(|name| (*name).to_string()).collect(),
    }
}

#[async_trait::async_trait]
impl Tool for ReadImage {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "read_image".into(),
            summary: "Read a PNG/JPEG/WebP/GIF image and return the picture itself".into(),
            description: "Reads an image file and returns the image so you can look at it. \
                          PNG, JPEG, WebP and GIF are accepted; the format is detected from the \
                          content, so a path without an extension works too. An image that is too \
                          large is downscaled before it is sent. Use this to inspect a screenshot, \
                          a diagram, or a photo — do not install an image library or shell out to a \
                          converter just to look at one."
                .into(),
            guidelines: vec![
                "Use `read_image` to look at an image; do not install image libraries or write \
                 thumbnails merely to inspect one."
                    .into(),
            ],
            host_validates_arguments: true,
            mutating: false,
            input_schema: schema(
                json!({
                    "path": {
                        "type": "string",
                        "description": "Path to the image; relative paths resolve against the \
                                        working directory",
                    },
                }),
                &["path"],
            ),
        }
    }

    async fn execute(&self, arguments: Value, settings: &ToolSettings) -> Result<ToolOutput> {
        let started = Instant::now();
        let raw_path = super::required_str(&arguments, "path")?;
        let path = settings.resolve(&raw_path)?;

        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_string();
        // A path with no extension is fine — the content decides. A path with an
        // extension that names something other than a supported image is a
        // mistake worth reporting by name, rather than silently sniffing.
        let declared = if extension.is_empty() {
            None
        } else {
            match attachments::media_type_for_extension(&extension) {
                Some(media_type) => Some(media_type),
                None => {
                    return Err(AgentError::invalid_params(format!(
                        "`{}` has a `.{extension}` extension, which is not a supported image \
                         format; read_image accepts PNG/JPEG/WebP/GIF",
                        path.display()
                    )))
                }
            }
        };

        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|error| AgentError::from_io("Failed to stat image", error))?;
        if metadata.is_dir() {
            return Err(AgentError::invalid_params(format!(
                "`{}` is a directory",
                path.display()
            )));
        }

        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| AgentError::from_io("Failed to read image", error))?;
        if bytes.len() > attachments::MAX_IMAGE_BYTES {
            return Ok(ToolOutput::error(format!(
                "`{}` is {} bytes, over the {} byte image limit; downscale it and read the \
                 smaller copy",
                path.display(),
                bytes.len(),
                attachments::MAX_IMAGE_BYTES
            )));
        }

        let sniffed = attachments::sniff(&bytes);
        let media_type = match (declared, sniffed) {
            // The extension and the content disagree. Trust the bytes, but say
            // so: the model is about to be told the media type, and a mismatch
            // usually means the file was renamed.
            (Some(declared), Some(sniffed)) if declared != sniffed => {
                return Ok(ToolOutput::error(format!(
                    "`{}` has a `.{extension}` extension ({declared}) but its content is \
                     {sniffed}; rename it to match its format, or convert it",
                    path.display()
                )))
            }
            (Some(declared), _) => declared,
            (None, Some(sniffed)) => sniffed,
            (None, None) => {
                return Ok(ToolOutput::error(format!(
                    "`{}` is not a supported image; read_image accepts PNG/JPEG/WebP/GIF",
                    path.display()
                )))
            }
        };

        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .map(str::to_string);
        let display = path.display().to_string();
        let original_bytes = bytes.len();
        let media_type = media_type.to_string();
        // Decoding, resampling and re-encoding are CPU-bound, and storing the
        // copy is blocking IO. Both belong on a blocking worker rather than
        // stalling one of the runtime's async threads.
        let image = tokio::task::spawn_blocking(move || {
            let (width, height) = image_ops::dimensions(&media_type, &bytes).unwrap_or((0, 0));
            let prepared =
                attachments::prepare(&media_type, &bytes, width, height).map_err(|error| {
                    // A refusal to downscale is the model's to fix, so it is a
                    // tool error rather than a failed run.
                    AgentError::invalid_params(format!("cannot read `{display}`: {error}"))
                })?;
            attachments::save_prepared(prepared, name)
        })
        .await
        .map_err(|error| AgentError::internal(format!("read_image worker failed: {error}")))??;

        Ok(ToolOutput {
            content: vec![
                ContentBlock::text(format!(
                    "<path>{}</path>\n<type>image</type>\n<content>\n{} image, {}x{} px, {} \
                     bytes\n</content>",
                    path.display(),
                    image.media_type,
                    image.width,
                    image.height,
                    image.bytes
                )),
                ContentBlock::Image { image },
            ],
            is_error: false,
            truncated: false,
            original_bytes: Some(original_bytes),
            duration_ms: Some(started.elapsed().as_millis() as u64),
            hunks: Vec::new(),
        })
    }
}

// The admission/normalisation this module once owned lives in
// `crate::attachments::prepare` now, so the composer's paste path and this tool
// share one budget-and-resample rule instead of two.
