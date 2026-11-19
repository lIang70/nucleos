//! The `openai.generation` extension: options only this provider understands, carried per call.
//!
//! A prefix-cache key, a privacy-preserving end-user identifier, and the `include` list are OpenAI
//! facts, not canonical request fields. Keeping them here means `GenerateParams` does not grow an
//! OpenAI section, and another adapter can treat the whole set as inert.

use crate::wire::Include;
use nucleos::adapter::{Extension, ExtensionDecoder};
use nucleos::error::{Error, Result};

/// Options for one `openai.generation` call.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
// A deployment writes these by hand, and a typo that decodes silently is exactly the "configured but
// not in effect" state the registry exists to prevent.
#[serde(deny_unknown_fields)]
pub struct GenerationOptions {
    /// The provider's prefix-cache key. Overrides the deployment default when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// A privacy-preserving end-user identifier. Overrides the deployment default when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety_identifier: Option<String>,
    /// Extra data to ask the provider for. Replaces the deployment's list rather than appending to
    /// it, so a caller cannot produce a duplicated entry by overriding one field.
    #[serde(default)]
    pub include: Vec<Include>,
}

impl GenerationOptions {
    /// Which knobs are actually set. An empty set is a configuration mistake, not a no-op.
    fn fields(&self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.prompt_cache_key.is_some() {
            fields.push("prompt_cache_key");
        }
        if self.safety_identifier.is_some() {
            fields.push("safety_identifier");
        }
        if !self.include.is_empty() {
            fields.push("include");
        }
        fields
    }
}

impl Extension for GenerationOptions {
    fn provider(&self) -> &str {
        "openai"
    }

    fn id(&self) -> &str {
        "generation"
    }

    fn active_fields(&self) -> Vec<&'static str> {
        self.fields()
    }

    fn validate(&self) -> Result<()> {
        if self.prompt_cache_key.as_deref() == Some("") {
            return Err(Error::invalid("prompt_cache_key must not be empty"));
        }
        if self.safety_identifier.as_deref() == Some("") {
            return Err(Error::invalid("safety_identifier must not be empty"));
        }
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn Extension> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Decode one `openai.generation` entry from its fields object.
pub const DECODER: ExtensionDecoder = |fields| {
    let options: GenerationOptions = serde_json::from_value(fields.clone())
        .map_err(|error| Error::invalid(format!("openai.generation: {error}")))?;
    Ok(Box::new(options))
};

/// The identity of the image options extension.
pub const IMAGE_EXTENSION_ID: &str = "image";

/// Options for one image generation call.
///
/// A separate extension from [`GenerationOptions`] because the two option sets do not intersect: one
/// struct holding both would put text sampling next to image sizing, and every text call would carry
/// a field it cannot use.
///
/// Everything here is optional and nothing defaults: an absent field means "let the provider decide",
/// which is different from naming the provider's default and keeping it in step by hand.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageOptions {
    /// How many images to produce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// Output dimensions, for example `1024x1024`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// `low`, `medium`, `high` or `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    /// `vivid` or `natural`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// The encoded format: `png`, `jpeg` or `webp`. It also decides the part's media type, which is
    /// why a payload cannot arrive labelled with a type the caller did not ask for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_format: Option<String>,
    /// `b64_json` or `url`. A URL response is a reference part, not bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<String>,
    /// `transparent`, `opaque` or `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    /// `low` or `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moderation: Option<String>,
    /// Compression level for the lossy formats, 0 to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_compression: Option<u8>,
    /// How many interim previews to stream before the finished image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_images: Option<u32>,
}

impl ImageOptions {
    fn fields(&self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.count.is_some() {
            fields.push("count");
        }
        if self.size.is_some() {
            fields.push("size");
        }
        if self.quality.is_some() {
            fields.push("quality");
        }
        if self.style.is_some() {
            fields.push("style");
        }
        if self.output_format.is_some() {
            fields.push("output_format");
        }
        if self.response_format.is_some() {
            fields.push("response_format");
        }
        if self.background.is_some() {
            fields.push("background");
        }
        if self.moderation.is_some() {
            fields.push("moderation");
        }
        if self.output_compression.is_some() {
            fields.push("output_compression");
        }
        if self.partial_images.is_some() {
            fields.push("partial_images");
        }
        fields
    }
}

impl Extension for ImageOptions {
    fn provider(&self) -> &str {
        "openai"
    }

    fn id(&self) -> &str {
        IMAGE_EXTENSION_ID
    }

    fn active_fields(&self) -> Vec<&'static str> {
        self.fields()
    }

    fn validate(&self) -> Result<()> {
        if let Some(format) = &self.output_format
            && !matches!(format.as_str(), "png" | "jpeg" | "webp")
        {
            return Err(Error::invalid(format!(
                "openai.image: output_format must be png, jpeg or webp, not {format:?}"
            )));
        }
        if let Some(format) = &self.response_format
            && !matches!(format.as_str(), "b64_json" | "url")
        {
            return Err(Error::invalid(format!(
                "openai.image: response_format must be b64_json or url, not {format:?}"
            )));
        }
        if let Some(level) = self.output_compression {
            if level > 100 {
                return Err(Error::invalid(
                    "openai.image: output_compression is a percentage, so at most 100",
                ));
            }
            // PNG is lossless: the provider rejects a compression level for it, so a deployment that
            // set one has a configuration that would fail at the call rather than here.
            if self
                .output_format
                .as_deref()
                .is_none_or(|format| format == "png")
            {
                return Err(Error::invalid(
                    "openai.image: output_compression applies to jpeg and webp only",
                ));
            }
        }
        if self.count == Some(0) {
            return Err(Error::invalid("openai.image: count must be at least one"));
        }
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn Extension> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Decode one `openai.image` entry from its fields object.
pub const IMAGE_DECODER: ExtensionDecoder = |fields| {
    let options: ImageOptions = serde_json::from_value(fields.clone())
        .map_err(|error| Error::invalid(format!("openai.image: {error}")))?;
    Ok(Box::new(options))
};

/// The identity of the speech options extension.
pub const SPEECH_EXTENSION_ID: &str = "speech";

/// Options for one speech synthesis call.
///
/// Voice, format and speed share nothing with the text or image option sets, so they get their own
/// identity rather than a corner of someone else's struct.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechOptions {
    /// The provider's voice name, for example `alloy` or `nova`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// The encoded format: `mp3`, `opus`, `aac`, `flac`, `wav` or `pcm`. It also decides the part's
    /// media type, which is what makes a chunked answer mergeable: chunks of different types are not
    /// an append.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Speaking rate, from 0.25 to 4.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
    /// Delivery guidance, on the models that accept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl SpeechOptions {
    fn fields(&self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.voice.is_some() {
            fields.push("voice");
        }
        if self.format.is_some() {
            fields.push("format");
        }
        if self.speed.is_some() {
            fields.push("speed");
        }
        if self.instructions.is_some() {
            fields.push("instructions");
        }
        fields
    }
}

impl Extension for SpeechOptions {
    fn provider(&self) -> &str {
        "openai"
    }

    fn id(&self) -> &str {
        SPEECH_EXTENSION_ID
    }

    fn active_fields(&self) -> Vec<&'static str> {
        self.fields()
    }

    fn validate(&self) -> Result<()> {
        if let Some(format) = &self.format
            && !matches!(
                format.as_str(),
                "mp3" | "opus" | "aac" | "flac" | "wav" | "pcm"
            )
        {
            return Err(Error::invalid(format!(
                "openai.speech: format must be mp3, opus, aac, flac, wav or pcm, not {format:?}"
            )));
        }
        if let Some(speed) = self.speed {
            // The provider's own range. A value outside it is a 400 at the call, and a configuration
            // error is cheaper to discover here.
            if !(0.25..=4.0).contains(&speed) {
                return Err(Error::invalid(
                    "openai.speech: speed must be between 0.25 and 4.0",
                ));
            }
        }
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn Extension> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Decode one `openai.speech` entry from its fields object.
pub const SPEECH_DECODER: ExtensionDecoder = |fields| {
    let options: SpeechOptions = serde_json::from_value(fields.clone())
        .map_err(|error| Error::invalid(format!("openai.speech: {error}")))?;
    Ok(Box::new(options))
};
