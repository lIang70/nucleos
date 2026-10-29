//! MCP content blocks, as the kernel's content model.
//!
//! The two vocabularies overlap but are not the same: MCP has an `audio` block where we have none,
//! and a `resource` that may be text or a blob. The mapping is where those differences are decided
//! — once, instead of in every caller.
//!
//! A block this build does not understand becomes a **visible marker**, not a silent drop: a model
//! that is told "there was an image here" can ask for it; one that is told nothing will reason
//! about a file it never saw.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use nucleos::contract::{Content, ImagePart, MediaFrom, MediaSource, Part, PartKind, TextPart};
use serde_json::Value;

/// Turn a `tools/call` result into content, and say whether it was a failure.
#[must_use]
pub fn decode(result: &Value) -> (Content, bool) {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut parts = Vec::new();
    match result.get("content").and_then(Value::as_array) {
        Some(blocks) => {
            for block in blocks {
                parts.push(decode_block(block));
            }
        }
        None => parts.push(text("the tool returned no content")),
    }
    (Content::new(parts), is_error)
}

fn decode_block(block: &Value) -> Part {
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "text" => text(
            block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        "image" => {
            let media_type = block
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream");
            match block.get("data").and_then(Value::as_str) {
                Some(data) => Part::new(PartKind::Image(ImagePart {
                    source: MediaSource {
                        media_type: media_type.to_owned(),
                        from: MediaFrom::Data {
                            data: STANDARD.decode(data).unwrap_or_default(),
                        },
                    },
                })),
                None => text("[an image the server described but did not send]"),
            }
        }
        // No audio part in this content model: it is a file with a media type, which is what the
        // kernel means by "bytes it moves and never interprets".
        "audio" => {
            let media_type = block
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("audio/mpeg");
            let data = block
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Part::new(PartKind::File(nucleos::contract::FilePart {
                uri: format!("data:{media_type};base64,{data}"),
                media_type: media_type.to_owned(),
                name: "audio".to_owned(),
            }))
        }
        "resource" => decode_resource(block.get("resource")),
        other => text(format!(
            "[unsupported content of type {other:?}: the kernel carried it and cannot render it]"
        )),
    }
}

fn decode_resource(resource: Option<&Value>) -> Part {
    let Some(resource) = resource else {
        return text("[a resource the server described but did not send]");
    };
    let uri = resource
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let media_type = resource
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("text/plain");

    // A resource that carries text is text: a pointer to something we already have is a worse
    // answer than the thing itself.
    if let Some(text_body) = resource.get("text").and_then(Value::as_str) {
        return text(text_body);
    }
    Part::new(PartKind::File(nucleos::contract::FilePart {
        uri: uri.to_owned(),
        media_type: media_type.to_owned(),
        name: resource
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(uri)
            .to_owned(),
    }))
}

fn text(body: impl Into<String>) -> Part {
    Part::new(PartKind::Text(TextPart { text: body.into() }))
}
