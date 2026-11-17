//! MCP content blocks, as the kernel's content model.
//!
//! The two vocabularies overlap but are not the same: MCP has an `audio` block where we have none,
//! and a resource that may be text or a blob. The mapping is where those differences are decided —
//! once, instead of in every caller.
//!
//! A block this build does not understand becomes a **visible marker**, not a silent drop: a model
//! that is told "there was something here" can ask for it; one that is told nothing will reason
//! about a file it never saw.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use nucleos::contract::{
    AudioPart, Content, DataPart, FilePart, ImagePart, Part, PartKind, TextPart,
};
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};

/// Turn a `tools/call` result into content, and say whether it was a failure.
#[must_use]
pub fn decode(result: &CallToolResult) -> (Content, bool) {
    let mut parts: Vec<Part> = result.content.iter().map(decode_block).collect();
    // Structured output is content too: a caller that asked for a schema gets it as data rather
    // than as prose only one provider can parse.
    if let Some(structured) = &result.structured_content {
        parts.push(Part::new(PartKind::Data(DataPart {
            media_type: "application/json".to_owned(),
            json: structured.to_string(),
        })));
    }
    if parts.is_empty() {
        parts.push(text("the tool returned no content"));
    }
    (Content::new(parts), result.is_error.unwrap_or(false))
}

fn decode_block(block: &ContentBlock) -> Part {
    match block {
        ContentBlock::Text(text) => Part::new(PartKind::Text(TextPart {
            text: text.text.clone(),
        })),
        ContentBlock::Image(image) => Part::new(PartKind::Image(ImagePart {
            // rmcp has already decoded the transport; what it hands over is the payload's base64 as
            // the protocol carries it, so it is decoded here, once. A block whose type is not
            // `image/*` becomes a file reference rather than a part that lies about its kind.
            source: match nucleos::contract::ImageSource::data(
                STANDARD.decode(&image.data).unwrap_or_default(),
                image.mime_type.clone(),
            ) {
                Ok(source) => source,
                Err(reason) => return text(format!("[image block refused: {reason}]")),
            },
        })),
        // Audio is its own modality, not a file: a client plays it where it would render an image,
        // and a capability that says "accepts audio" has to mean something a part can answer.
        ContentBlock::Audio(audio) => Part::new(PartKind::Audio(AudioPart {
            source: match nucleos::contract::AudioSource::data(
                STANDARD.decode(&audio.data).unwrap_or_default(),
                audio.mime_type.clone(),
            ) {
                Ok(source) => source,
                Err(reason) => return text(format!("[audio block refused: {reason}]")),
            },
            format: None,
            duration_millis: None,
        })),
        ContentBlock::Resource(resource) => decode_resource(&resource.resource),
        // A link is a reference the model can ask for by name; resolving it is a `read_resource`
        // call, which this crate does not make — a tool that wants the bytes should send them.
        ContentBlock::ResourceLink(link) => Part::new(PartKind::File(FilePart {
            uri: link.uri.clone(),
            media_type: link
                .mime_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_owned()),
            name: link.name.clone(),
        })),
        // The protocol adds block types; a client that fails on an unknown one breaks on every
        // future feature, and one that drops it silently lies about what the tool returned.
        other => text(format!(
            "[unsupported content of type {other:?}: the kernel carried it and cannot render it]"
        )),
    }
}

fn decode_resource(resource: &ResourceContents) -> Part {
    match resource {
        // A resource that carries text is text: a pointer to something we already have is a worse
        // answer than the thing itself.
        ResourceContents::TextResourceContents { text, .. } => {
            Part::new(PartKind::Text(TextPart { text: text.clone() }))
        }
        ResourceContents::BlobResourceContents { uri, mime_type, .. } => {
            Part::new(PartKind::File(FilePart {
                uri: uri.clone(),
                media_type: mime_type
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".to_owned()),
                name: uri.clone(),
            }))
        }
        // Same rule as a content block: a resource kind from a newer protocol is carried, named,
        // and not interpreted.
        other => text(format!(
            "[unsupported resource of type {}: the kernel carried it and cannot render it]",
            resource_kind(other)
        )),
    }
}

fn resource_kind(resource: &ResourceContents) -> &'static str {
    match resource {
        ResourceContents::TextResourceContents { .. } => "text",
        ResourceContents::BlobResourceContents { .. } => "blob",
        _ => "unknown",
    }
}

fn text(body: impl Into<String>) -> Part {
    Part::new(PartKind::Text(TextPart { text: body.into() }))
}
