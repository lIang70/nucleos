//! The content model: what a message is made of, and the media it may carry.

use super::PartKindName;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Who produced a message.
///
/// The set is the smallest one every provider can express. A provider with a fifth role
/// maps it onto one of these in its adapter — the kernel does not learn it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Instructions.
    System,
    /// The caller.
    User,
    /// The model.
    Assistant,
    /// A tool's output.
    Tool,
}

/// One turn's worth of content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Message {
    /// Who produced it.
    pub role: Role,
    /// What it contains.
    ///
    /// Non-empty whenever the kernel writes one: an empty content is a bug in whatever
    /// produced it, not something a reader has to tolerate.
    pub content: Content,
}

/// An ordered multimodal payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Content {
    /// The parts, in order.
    pub parts: Vec<Part>,
}

impl Content {
    /// Build a content from parts.
    #[must_use]
    pub fn new(parts: impl IntoIterator<Item = Part>) -> Self {
        Self {
            parts: parts.into_iter().collect(),
        }
    }

    /// Concatenate every text part. Thinking, images and tool parts are skipped.
    ///
    /// One definition, because "the text of a content" drifting into "first text part" for
    /// one caller and "all text parts joined" for another is how two readers of the same
    /// value start disagreeing.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for part in &self.parts {
            if let PartKind::Text(text) = &part.kind {
                out.push_str(&text.text);
            }
        }
        out
    }

    /// Whether there is nothing to render.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

/// One piece of content.
///
/// The union names **modalities**, not storage strategies. Text, image, audio and video are four
/// things a client renders differently, and a capability declares which of them a model accepts or
/// produces — so they are four kinds even though three of them share one payload shape. An earlier
/// version folded audio and video into [`PartKind::File`] on the argument that the kernel only moves
/// those bytes; the cost was that a capability could not say "produces audio", because nothing
/// mapped to it, and that an audio payload could never be inline while an image could.
///
/// [`PartKind::File`] therefore means one thing only: **a reference to content that lives
/// elsewhere** — a spilled tool result, a workspace file, an artifact whose media type is not worth
/// promising. It is a pointer, and a pointer is not a modality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Part {
    /// What it is.
    #[serde(flatten)]
    pub kind: PartKind,
    /// Contextual or vendor-specific data.
    ///
    /// The kernel never reads it, never interprets it, and never rejects an unknown key.
    /// This is the only extension hatch: a schema with two of them has everyone using the
    /// other one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
}

impl Part {
    /// A part with no extension data — the common case.
    #[must_use]
    pub fn new(kind: PartKind) -> Self {
        Self {
            kind,
            meta: BTreeMap::new(),
        }
    }

    /// A text part.
    ///
    /// The constructor every adapter and every test needs: text is the common case, and spelling out
    /// `Part::new(PartKind::Text(TextPart { .. }))` at each site is noise that hides the shape.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::new(PartKind::Text(TextPart { text: text.into() }))
    }

    /// An image, from a source that already declared itself an image.
    #[must_use]
    pub fn image(source: ImageSource) -> Self {
        Self::new(PartKind::Image(ImagePart { source }))
    }

    /// A clip of audio, from a source that already declared itself audio.
    #[must_use]
    pub fn audio(source: AudioSource) -> Self {
        Self::new(PartKind::Audio(AudioPart {
            source,
            format: None,
            duration_millis: None,
        }))
    }

    /// A video, from a source that already declared itself video.
    #[must_use]
    pub fn video(source: VideoSource) -> Self {
        Self::new(PartKind::Video(VideoPart {
            source,
            duration_millis: None,
        }))
    }

    /// A reasoning part, with the provider's signature fragment when there is one.
    #[must_use]
    pub fn reasoning(text: impl Into<String>, signature: impl Into<String>) -> Self {
        Self::new(PartKind::Reasoning(ReasoningPart {
            text: text.into(),
            signature: signature.into(),
            id: String::new(),
            source: String::new(),
        }))
    }
}

/// The kind of a [`Part`], and its payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PartKind {
    /// Plain text.
    Text(TextPart),
    /// An image, inline or referenced.
    Image(ImagePart),
    /// Synthesized speech, inline or referenced, with its format when the provider says.
    Audio(AudioPart),
    /// A video, inline or referenced.
    Video(VideoPart),
    /// A reference to content that is not carried here.
    File(FilePart),
    /// Structured output.
    Data(DataPart),
    /// The model asking for a tool.
    ToolCall(ToolCallPart),
    /// A tool's answer, folded back into history.
    ToolResult(ToolResultPart),
    /// A provider reasoning trace.
    Reasoning(ReasoningPart),
}

/// Plain text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TextPart {
    /// The text.
    pub text: String,
}

/// An image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ImagePart {
    /// Where the bytes are. An [`ImageSource`] refuses a type outside `image/`.
    pub source: ImageSource,
}

/// A clip of speech.
///
/// The format and the duration belong to the clip rather than to its source: two clips with the same
/// media type can differ in sample rate, and a player that assumed otherwise would be wrong half the
/// time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AudioPart {
    /// Where the bytes are. An [`AudioSource`] refuses a type outside `audio/`.
    pub source: AudioSource,
    /// How the bytes are encoded, when the provider said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<AudioFormat>,
    /// How long it plays, when the provider said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_millis: Option<u64>,
}

/// A video.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VideoPart {
    /// Where the bytes are. A [`VideoSource`] refuses a type outside `video/`.
    pub source: VideoSource,
    /// How long it plays, when the provider said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_millis: Option<u64>,
}

/// How audio is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AudioEncoding {
    /// 16-bit signed PCM.
    Pcm16,
    /// 24-bit signed PCM.
    Pcm24,
    /// 32-bit float PCM.
    Float32,
    /// MPEG layer III.
    Mp3,
    /// Opus.
    Opus,
    /// AAC.
    Aac,
    /// FLAC.
    Flac,
}

impl AudioEncoding {
    /// The media type this encoding is carried as.
    #[must_use]
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Pcm16 | Self::Pcm24 | Self::Float32 => "audio/pcm",
            Self::Mp3 => "audio/mpeg",
            Self::Opus => "audio/opus",
            Self::Aac => "audio/aac",
            Self::Flac => "audio/flac",
        }
    }

    /// Whether this is raw samples rather than a container.
    #[must_use]
    pub fn is_raw(self) -> bool {
        matches!(self, Self::Pcm16 | Self::Pcm24 | Self::Float32)
    }
}

/// The shape of a clip's samples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AudioFormat {
    /// How the samples are encoded.
    pub encoding: AudioEncoding,
    /// Samples per second. Required for a raw encoding, meaningless for a container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate_hz: Option<u32>,
    /// Channel count. Required for a raw encoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
}

impl AudioFormat {
    /// Whether the format can be honoured: raw samples need a rate and a channel count.
    ///
    /// # Errors
    ///
    /// A raw encoding without either.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.encoding.is_raw() && (self.sample_rate_hz.is_none() || self.channels.is_none()) {
            return Err(format!(
                "raw audio needs a sample rate and a channel count, not {:?}",
                self.encoding
            ));
        }
        Ok(())
    }
}

/// Bytes, or a reference to them, plus the type that makes them renderable.
///
/// `url` exists because inlining a large image into an event log is a storage incident: the
/// log is append-only and replayed, so every byte is paid for on every replay. `data` exists
/// because a caller with no reachable store must still be able to send one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MediaSource {
    /// The media type, e.g. `image/png`. Required: bytes without a type are not renderable.
    pub media_type: String,
    /// Where the bytes come from.
    pub from: MediaFrom,
}

/// A source that refuses a media type outside one family.
///
/// The three modalities carry the same bytes over the same two carriers, so they share [`MediaSource`]
/// — but a part whose kind says "audio" and whose source says `image/png` is a part that lies, and the
/// type is the only place that can refuse it before a player tries to decode it.
macro_rules! media_source {
    ($name:ident, $prefix:literal, $what:literal) => {
        #[doc = concat!("A ", $what, " source: `", $prefix, "*` only.")]
        ///
        /// Built through [`Self::url`] or [`Self::data`], which check the family; a
        /// [`MediaSource`] converts through [`TryFrom`] for the same reason.
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(try_from = "MediaSource", into = "MediaSource")]
        pub struct $name {
            media_type: String,
            from: MediaFrom,
        }

        impl $name {
            /// A reference to bytes somewhere else.
            ///
            /// # Errors
            ///
            /// A media type outside the family.
            pub fn url(
                url: impl Into<String>,
                media_type: impl Into<String>,
            ) -> std::result::Result<Self, String> {
                Self::try_from(MediaSource {
                    media_type: media_type.into(),
                    from: MediaFrom::Url { url: url.into() },
                })
            }

            /// Bytes carried inline.
            ///
            /// # Errors
            ///
            /// A media type outside the family.
            pub fn data(
                data: impl Into<Vec<u8>>,
                media_type: impl Into<String>,
            ) -> std::result::Result<Self, String> {
                Self::try_from(MediaSource {
                    media_type: media_type.into(),
                    from: MediaFrom::Data { data: data.into() },
                })
            }

            /// The media type, e.g. the `image/png` behind an image part.
            #[must_use]
            pub fn media_type(&self) -> &str {
                &self.media_type
            }

            /// Where the bytes come from.
            #[must_use]
            pub fn from(&self) -> &MediaFrom {
                &self.from
            }

            /// The bytes, when they were carried inline.
            #[must_use]
            pub fn bytes(&self) -> Option<&[u8]> {
                match &self.from {
                    MediaFrom::Data { data } => Some(data),
                    MediaFrom::Url { .. } => None,
                }
            }

            /// Append another source's bytes to this one, when both are inline and the same type.
            ///
            /// What a streamed clip needs: many chunks at one index become one part. A URL source, a
            /// different media type, or a mismatch between a URL and inline bytes is not an append,
            /// and the caller replaces instead.
            pub fn append(&mut self, other: &Self) -> bool {
                if self.media_type != other.media_type {
                    return false;
                }
                match (&mut self.from, &other.from) {
                    (MediaFrom::Data { data }, MediaFrom::Data { data: incoming }) => {
                        data.extend_from_slice(incoming);
                        true
                    }
                    _ => false,
                }
            }
        }

        impl TryFrom<MediaSource> for $name {
            type Error = String;

            fn try_from(source: MediaSource) -> std::result::Result<Self, Self::Error> {
                if !source.media_type.starts_with($prefix) {
                    return Err(format!(
                        concat!(
                            "a ",
                            $what,
                            " part needs a media type under `",
                            $prefix,
                            "*`, not {:?}"
                        ),
                        source.media_type
                    ));
                }
                Ok(Self {
                    media_type: source.media_type,
                    from: source.from,
                })
            }
        }

        impl From<$name> for MediaSource {
            fn from(source: $name) -> Self {
                Self {
                    media_type: source.media_type,
                    from: source.from,
                }
            }
        }
    };
}

media_source!(ImageSource, "image/", "image");
media_source!(AudioSource, "audio/", "audio");
media_source!(VideoSource, "video/", "video");

/// Where media bytes live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum MediaFrom {
    /// A URL to fetch.
    Url {
        /// The URL.
        url: String,
    },
    /// Bytes carried inline.
    Data {
        /// The bytes.
        data: Vec<u8>,
    },
}

/// A reference to content that is not carried here: an artifact, a workspace file, or a
/// truncated tool result's spill.
///
/// This is also how truncation is expressed. A result too large to store is spilled; the
/// text keeps a marker and this part keeps the pointer. That is a policy outcome, not an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FilePart {
    /// Where to find it.
    pub uri: String,
    /// What it is.
    pub media_type: String,
    /// A human-facing name.
    pub name: String,
}

/// Structured output. JSON text rather than bytes, because JSON is UTF-8 by definition and
/// text is what every toolchain can read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DataPart {
    /// The media type, usually `application/json`.
    pub media_type: String,
    /// The JSON, as text.
    pub json: String,
}

/// The model asking for a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallPart {
    /// The call.
    pub call: ToolCall,
}

/// A tool's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResultPart {
    /// The result.
    pub result: ToolResult,
}

/// One tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCall {
    /// The correlation id.
    pub id: String,
    /// The tool name.
    pub name: String,
    /// A JSON object, kept as text so that a replay is byte-faithful.
    pub arguments_json: String,
    /// Opaque extension, same rule as [`Part::meta`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
}

/// A turn the runtime did not survive, as far as it got.
///
/// Not a message: a message is something the model finished writing. This is the fragment left in the
/// event stream when a process died mid-generation — kept because it was paid for and because it is
/// often the most useful thing available, reported so a caller does not have to reassemble deltas by
/// hand to find out what was said.
///
/// It cannot be resumed. No provider continues a generation from the middle: the stream was one HTTP
/// response, and it ended when the process did. What a caller can do is continue the *conversation* —
/// send this text back as context for a new turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PartialTurn {
    /// The text the assistant had produced, in order.
    #[serde(default)]
    pub text: String,
    /// How many steps into the turn it got.
    #[serde(default)]
    pub step_index: u32,
    /// A tool call whose arguments were still arriving, if that is where it stopped.
    ///
    /// The name is known and the arguments may be truncated, which is why this is reported as data
    /// rather than executed: half a JSON object is not a call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unfinished_call: Option<UnfinishedCall>,
}

/// A call that started and never resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UnfinishedCall {
    /// The correlation id it would have been filed under.
    pub tool_call_id: String,
    /// The tool the model was calling.
    pub name: String,
    /// The arguments as far as they arrived. Possibly incomplete, and possibly not valid JSON.
    #[serde(default)]
    pub arguments_json: String,
}

/// How a tool call ended.
///
/// **Failure is a flag, not a branch.** A failed call still carries content, because the
/// error text is something the model must be able to read and reason about; putting the
/// failure outside the content model would leave the model with a message it cannot see.
/// The cost is honest: `is_error` is a runtime flag a client can forget to check, where a
/// `oneof` would have been enforced by the type system.
///
/// One rule constrains the recursion `Content → Part → ToolResult → Content` otherwise
/// allows: a tool result's content may contain any kind **except** [`PartKind::ToolCall`] or
/// [`PartKind::ToolResult`]. Nesting is one level deep, and a validator enforces it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResult {
    /// The call this answers.
    pub call_id: String,
    /// What came back — on success and on failure alike.
    pub content: Content,
    /// Whether the call failed.
    pub is_error: bool,
}

/// One provider reasoning trace.
///
/// `signature` is why this is not a plain string: some providers sign a reasoning block and
/// reject the next turn when the signature is not returned verbatim. Dropping it turns a
/// multi-turn session into a failure on turn two. `id` and `source` exist for the same
/// reason — they are opaque to the kernel and must round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReasoningPart {
    /// The reasoning text. May be empty when the provider withholds it.
    pub text: String,
    /// The provider's signature, returned verbatim.
    pub signature: String,
    /// The provider's item id, when it uses one.
    pub id: String,
    /// The provider's verification scope.
    pub source: String,
}

impl PartKind {
    /// This part's kind, without the payload.
    #[must_use]
    pub fn name(&self) -> PartKindName {
        match self {
            Self::Text(_) => PartKindName::Text,
            Self::Image(_) => PartKindName::Image,
            Self::Audio(_) => PartKindName::Audio,
            Self::Video(_) => PartKindName::Video,
            Self::File(_) => PartKindName::File,
            Self::Data(_) => PartKindName::Data,
            Self::ToolCall(_) => PartKindName::ToolCall,
            Self::ToolResult(_) => PartKindName::ToolResult,
            Self::Reasoning(_) => PartKindName::Reasoning,
        }
    }
}
