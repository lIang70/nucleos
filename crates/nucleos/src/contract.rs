//! The kernel's pure types.
//!
//! No I/O, no clocks, no sockets. Everything here is data that can be compared, logged,
//! stored and replayed, which is what lets a driver be tested without a runtime and an
//! event log be replayed without a network.
//!
//! The types are deliberately narrower than any provider's vocabulary. A capability that
//! has no representation here is a capability the kernel does not claim to understand.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// A session identifier.
///
/// The caller chooses it when resuming, so it is a string rather than an opaque handle.
/// That makes a retried `Start` idempotent instead of creating a second session.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub struct SessionId(String);

impl SessionId {
    /// Wrap a caller-supplied identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A tool-source generation.
///
/// Generations are immutable snapshots of the declared sources. A session is pinned to
/// one at start and may only move forward at a turn boundary, and only when the two
/// generations are compatible.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
)]
pub struct Generation(pub u64);

// ---------------------------------------------------------------------------
// The content model
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

/// Where a session is in its life.
///
/// The vocabulary is the session's, not a provider's. `AwaitingHuman` is a state rather
/// than a callback, so a confirmation that arrives three hours later resumes a session
/// instead of reviving a stack frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Accepting calls.
    Open,
    /// Finished. No further call is accepted.
    Closed,
}

impl SessionState {
    /// Whether the session accepts no further calls.
    #[must_use]
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Closed)
    }
}

/// Why a session stopped.
///
/// Four layers, all present: a provider's `finish_reason`, the step cap, the wall clock,
/// and the cost ledger. Each alone can be defeated; together they cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model said it was finished and no stop condition fired first.
    Completed,
    /// The step cap was reached.
    Steps,
    /// The token budget was spent.
    Tokens,
    /// The cost budget was spent.
    Cost,
    /// The wall-clock budget was spent.
    Wall,
    /// Policy denied a call.
    Denied,
    /// A tool or model call failed.
    Failed,
    /// Stopped on request.
    Cancelled,
}

// ---------------------------------------------------------------------------
// Accounting
// ---------------------------------------------------------------------------

/// What a session may consume.
///
/// `None` means unbounded for that dimension, which is a decision the host makes, not a
/// default the kernel assumes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Budget {
    /// Turns a session may take.
    pub max_steps: Option<u64>,
    /// Tokens across every model call, including failed attempts that streamed.
    pub max_tokens: Option<u64>,
    /// Cost in micro-USD. Integer, because money in a float is a rounding argument waiting
    /// to happen.
    pub max_micros_usd: Option<u64>,
    /// Wall clock for the whole session.
    pub max_wall_millis: Option<u64>,
}

/// What a model call consumed.
///
/// A counter the provider did not report stays `None` rather than becoming zero: a budget
/// that charges for silence is worse than one that knows it does not know.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Cost in micro-USD, when the provider reported one.
    pub micros_usd: Option<u64>,
}

impl Usage {
    /// Fold another report in.
    ///
    /// An unreported cost stays unreported. Adding an unknown as zero under-reports the
    /// bill, which is the one direction nobody forgives.
    pub fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.micros_usd = match (self.micros_usd, other.micros_usd) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            _ => None,
        };
    }

    /// Tokens billed, however the provider split them.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// An append-only record of something that happened.
///
/// `seq` is dense and gap-free, which is what makes it usable as a resume cursor without
/// a separate acknowledgement protocol. `schema_version` is written alongside the event so
/// a reader can upcast instead of guessing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Event {
    /// Monotonic within a session, starting at 0.
    pub seq: u64,
    /// The session this belongs to.
    pub session: SessionId,
    /// Unix milliseconds, from the injected clock so replay is deterministic.
    pub at_unix_millis: u64,
    /// The schema this event was written with.
    pub schema_version: u32,
    /// What happened.
    ///
    /// Flattened, like [`Part::kind`]: without it the wire says `{"kind":{"kind":"text_delta",…}}`,
    /// which is the tag twice and a shape nobody writes by hand. It also makes the SSE event name and
    /// the JSON tag the same field at the same level, so a client that reads only `data` and one that
    /// reads only the event name cannot end up with different ideas of what the frame is.
    #[serde(flatten)]
    pub kind: EventKind,
    /// Opaque extension, same rule as [`Part::meta`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
}

impl Event {
    /// The schema version written by this build.
    pub const SCHEMA_VERSION: u32 = 1;
}

/// The payload of an [`Event`].
///
/// Events carry the content model rather than copies of its fields — [`ToolCall`] here is
/// the same type a [`ToolCallPart`] holds — so a live stream and a stored history cannot
/// disagree about the shape of an answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    /// A session state transition, with the reason when it is terminal.
    StateChanged {
        /// The state entered.
        state: SessionState,
        /// Set when the state is terminal. Omitted otherwise: `null` is not used on this wire.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop: Option<StopReason>,
    },
    /// A fragment of assistant text.
    /// A fragment appended to the part at `part_index`.
    ///
    /// The merge rule is a property of the *event*, never inferred from the payload: text and
    /// reasoning arrive as fragments and are appended, and a media part that streams arrives as a
    /// sequence of chunks whose bytes are appended. A part that arrives whole uses
    /// [`EventKind::PartReplace`] instead.
    ///
    /// The distinction is not cosmetic. Synthesized audio is a sequence of chunks at one index;
    /// under a single "delta" event whose rule was inferred from the payload, every chunk would have
    /// replaced the last and only the final fragment would have survived.
    PartAppend {
        /// Which part of the message it belongs to.
        ///
        /// Without it a client cannot tell a second text block from a continuation of the first, so
        /// text → tool call → more text is unrepresentable in the stream.
        part_index: u32,
        /// The fragment.
        part: Part,
    },
    /// A whole part at `part_index`, replacing whatever was there.
    ///
    /// What a generated image or a file reference arrives as, and what a preview becomes when the
    /// final version is ready.
    PartReplace {
        /// Which part of the message it belongs to.
        part_index: u32,
        /// The part.
        part: Part,
    },
    /// A fragment of a reasoning trace.
    ///
    /// `signature` usually arrives on its own, in a delta with empty `text`: a provider
    /// signs the finished block rather than each fragment.

    /// A tool call began. Its arguments follow as [`EventKind::ToolCallArgs`].
    ToolCallStarted {
        /// Position in this turn's tool-call list.
        ///
        /// The tool-call ordinal, which is what [`EventKind::ToolCallArgs`] and the result are
        /// correlated by. It is not the position of the call in the message.
        index: u32,
        /// Which part of the assistant message this call occupies.
        ///
        /// The same index space as [`EventKind::PartAppend`] and [`EventKind::PartReplace`]. A tool
        /// call is a part at its own index, not a separate sequence: without it the assembled message
        /// and the event stream describe the same turn in two different orders.
        part_index: u32,
        /// The correlation id the result will be filed under.
        tool_call_id: String,
        /// The tool name.
        name: String,
        /// Wall-clock deadline for this call.
        deadline_unix_millis: u64,
        /// When set, the tool reports progress under this token.
        progress_token: String,
    },
    /// A fragment of one call's arguments.
    ///
    /// Arguments stream as text because that is what providers send and because a partial
    /// object cannot be handed to a tool. The kernel assembles and validates before anything
    /// runs; malformed arguments become a result the model can read.
    ToolCallArgs {
        /// Which call this fragment belongs to.
        index: u32,
        /// The fragment.
        fragment: String,
    },
    /// A tool call resolved.
    ToolFinished {
        /// How long it took.
        duration_millis: u64,
        /// The result — the same value that enters history.
        result: ToolResult,
        /// What the tool reported it spent, when it reported anything.
        ///
        /// Absent is not zero: a tool that says nothing has not said it was free.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// One model call's consumption, debited whether or not the answer was usable.
    Usage {
        /// What it consumed.
        usage: Usage,
        /// Why the call happened, so a summary's spend can be told from an answer's.
        purpose: Purpose,
        /// The model that served it, after routing.
        model: String,
        /// Which attempt in the ledger it belongs to.
        attempt_index: u32,
    },
    /// Tools became visible to the model.
    ///
    /// Recorded because a replay has to know which tools the model could see at turn 42 — the same
    /// reason `session_configured` exists, and the same failure if it did not.
    ToolsExposed {
        /// The names that were added.
        names: Vec<String>,
    },
    /// The session's mutable configuration changed.
    ///
    /// Recorded because a replay has to know which system prompt was in force at turn 42; without
    /// it, the log would rebuild a conversation that never happened.
    SessionConfigured {
        /// What changed, field by field.
        ///
        /// Boxed because the event union must stay small enough to move around cheaply: a config
        /// patch carries strings, and one large variant would make every event that large.
        patch: Box<ConfigPatch>,
    },
    /// A field the provider could not honour.
    ///
    /// Reported, never swallowed: a `tool_choice = Required` that quietly became `Auto` is a
    /// session behaving differently than it was configured to.
    CapabilityDegraded(Degraded),
    /// One attempt ended badly. Recorded because a retry does not erase the attempt that failed,
    /// or the money it spent.
    AttemptFailed {
        /// What happened.
        attempt: Attempt,
    },
    /// A policy verdict of `AskHuman` was reached.
    AwaitingHuman {
        /// What the host should ask.
        question: String,
        /// Which parked question this is. A tool call's id, or the session's for a command.
        pending_id: String,
        /// Typed answers, so a client can render "once" and "always" differently and the
        /// verdict can say which was chosen.
        options: Vec<PermissionOption>,
        /// What the kernel does if nobody answers. A refusal, not a silent approval.
        default_if_unanswered: PermissionKind,
    },
    /// A budget dimension ran out.
    BudgetExhausted {
        /// Which one.
        reason: StopReason,
    },
    /// Credit was granted to a session.
    ///
    /// An event rather than a silent mutation: a replay that did not see this would think the session
    /// ran out of money when it did not, and a session that was revived would look like one that never
    /// stopped.
    Credited {
        /// How much was granted. `None` means the limit was removed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        amount: Option<u64>,
        /// The balance after the grant. `None` means unlimited.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remaining: Option<u64>,
    },
    /// Something failed.
    Failed {
        /// A stable code for programs.
        code: ErrorCode,
        /// For humans.
        reason: String,
    },
    /// The session moved to a newer tool-source generation.
    ///
    /// Recorded rather than inferred, so a replay reproduces which tool set each turn ran
    /// against.
    GenerationMigrated {
        /// Where it came from.
        from: Generation,
        /// Where it went.
        to: Generation,
    },
    /// A turn began, and everything after this event belongs to it.
    ///
    /// Written before the first model call, and only once the turn is certain to run: budget spent,
    /// route resolved, policy satisfied. That makes it the line a follower watches — before it, a
    /// failure is the caller's to handle and belongs in a status code; after it, the log is the record
    /// of what happened, and a stream that stopped silently is distinguishable from one that stalled.
    TurnStarted {
        /// The model the route resolved to first.
        model: String,
        /// The generation this turn runs under.
        generation: u64,
        /// Which step of the session this is, 1-based.
        ///
        /// Carried because the step counter is a budget: a turn that fails after it began still spent
        /// one, and a reload that could not see it would hand the step back — making `max_steps`
        /// enforceable per process rather than per session.
        #[serde(default)]
        step: u64,
    },
    /// The turn ended without producing a message and without a failed attempt to point at.
    ///
    /// Not every failure is an attempt's: a policy that refuses the model model-level, a prompt that
    /// cannot fit, a store that refused the write on the way in. Without this event those turns leave
    /// the log with a `turn_started` and nothing else — a state indistinguishable from a turn that is
    /// still running, which is precisely what a caller cannot act on.
    TurnFailed {
        /// Why, in the kernel's own vocabulary.
        code: crate::contract::ErrorCode,
        /// What it said.
        reason: String,
    },
    /// The assembled message a model call produced.
    ///
    /// Deltas are a rendering protocol; this is the record. Some content can only be
    /// assembled — a reasoning signature arrives as its own delta — and a client that joins
    /// late should render without replaying every fragment.
    MessageCompleted {
        /// The message, as it enters history.
        message: Message,
        /// Which step of the turn produced it.
        step_index: u32,
        /// The provider's response identifier, when the wire carries one (OpenAI `response.id`).
        ///
        /// Empty when the provider exposes none. It is how a caller correlates an answer with the
        /// provider's own logs without inspecting the request or the payload.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        /// The provider's request identifier, when the wire carries one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// A command started.
    ExecStarted {
        /// The process id, when the platform gave one.
        pid: u32,
        /// What was run, verbatim, so an audit can see it.
        argv: Vec<String>,
        /// The policy it ran under, after any granted widening.
        policy: SandboxPolicy,
        /// The mechanism that carried the policy out, when one did.
        ///
        /// Absent means the kernel applied nothing: `danger_full_access`, an external sandbox, or an
        /// approved escalation. Recorded because "was it confined" is the first question asked after
        /// a command does something surprising, and the policy alone does not answer it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mechanism: Option<String>,
        /// The profile the mechanism was given, verbatim.
        ///
        /// A refusal from a sandbox is "Operation not permitted" and nothing else. Without this the
        /// log cannot say which rule was missing, and the only way to answer is to rebuild the
        /// profile from a policy that may since have changed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    /// A chunk of a command's output.
    ExecOutput {
        /// Which stream.
        stream: ExecStream,
        /// The text of this chunk.
        chunk: String,
    },
    /// Bytes written to a running process's stdin.
    ///
    /// Recorded because it is the most audit-worthy thing in the system: spawning `/bin/sh` is
    /// safe, writing `rm -rf /` into it is not.
    ExecStdin {
        /// What was written.
        chunk: String,
    },
    /// A command ended.
    ExecExited {
        /// How it ended.
        output: ExecOutput,
    },
    /// Progress from a long-running call. A call whose only feedback is a deadline is
    /// indistinguishable from a hang.
    Progress {
        /// The token from [`EventKind::ToolCallStarted`].
        token: String,
        /// Units completed.
        done: u64,
        /// Units expected. Zero means unknown, not zero.
        total: u64,
        /// For humans.
        message: String,
    },
    /// An event this build does not understand.
    ///
    /// Written by a newer daemon and read by an older one: carried through loading rather than
    /// refused, because a store is shared by whichever builds happen to run against it and "cannot
    /// parse one row" is not a state a durable log may have. `wire.md` says the same thing about the
    /// wire — an unknown union member is carried, not failed — and this is the half that applies to
    /// the record.
    ///
    /// The original kind name is **not** preserved: serde's `other` arm keeps the event, not the tag.
    /// That is deliberate. Rewriting an event is something this kernel never does, so the only copy of
    /// the original body is the stored one, and the correct behaviour for a build that does not
    /// understand an event is to leave it exactly as it found it.
    #[serde(other)]
    Unknown,
}

impl EventKind {
    /// The routable type string, e.g. `nucleos.v1.tool_finished`.
    ///
    /// The wire carries this alongside the `oneof` on purpose: a proxy filters on it
    /// without compiling the schema. Deriving it here rather than storing it means the two
    /// can never disagree.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::StateChanged { .. } => "nucleos.v1.state_changed",
            Self::PartAppend { .. } => "nucleos.v1.part_append",
            Self::PartReplace { .. } => "nucleos.v1.part_replace",
            Self::ToolCallStarted { .. } => "nucleos.v1.tool_call_started",
            Self::ToolFinished { .. } => "nucleos.v1.tool_finished",
            Self::ToolCallArgs { .. } => "nucleos.v1.tool_call_args",
            Self::Usage { .. } => "nucleos.v1.usage",
            Self::ExecStarted { .. } => "nucleos.v1.exec_started",
            Self::ExecOutput { .. } => "nucleos.v1.exec_output",
            Self::ExecStdin { .. } => "nucleos.v1.exec_stdin",
            Self::ExecExited { .. } => "nucleos.v1.exec_exited",
            Self::ToolsExposed { .. } => "nucleos.v1.tools_exposed",
            Self::SessionConfigured { .. } => "nucleos.v1.session_configured",
            Self::CapabilityDegraded(_) => "nucleos.v1.capability_degraded",
            Self::AttemptFailed { .. } => "nucleos.v1.attempt_failed",
            Self::AwaitingHuman { .. } => "nucleos.v1.awaiting_human",
            Self::BudgetExhausted { .. } => "nucleos.v1.budget_exhausted",
            Self::Credited { .. } => "nucleos.v1.credited",
            Self::Failed { .. } => "nucleos.v1.failed",
            Self::GenerationMigrated { .. } => "nucleos.v1.generation_migrated",
            Self::TurnStarted { .. } => "nucleos.v1.turn_started",
            Self::TurnFailed { .. } => "nucleos.v1.turn_failed",
            Self::Unknown => "nucleos.v1.unknown",
            Self::MessageCompleted { .. } => "nucleos.v1.message_completed",
            Self::Progress { .. } => "nucleos.v1.progress",
        }
    }
}

/// What a human may answer when a call is gated.
///
/// The kernel *reports* the choice; it never decides that "always" applies. That is policy,
/// and it belongs to the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionOption {
    /// Caller-chosen, returned verbatim in the verdict.
    pub id: String,
    /// What it means.
    pub kind: PermissionKind,
    /// For humans.
    pub label: String,
}

/// The kinds of answer a permission request offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    /// Allow this call.
    AllowOnce,
    /// Allow this call and, if the host agrees, this tool for the session.
    AllowAlways,
    /// Refuse this call.
    Reject,
}

/// A stable error category.
///
/// Codes are enums rather than strings because the first caller who compares a string
/// freezes it forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request was malformed.
    Invalid,
    /// The thing named does not exist.
    NotFound,
    /// Policy refused.
    Denied,
    /// A budget was exhausted.
    Budget,
    /// A deadline passed.
    Timeout,
    /// The request is valid but the thing it refers to moved.
    ///
    /// Distinct from `Invalid`, which blames the request. A caller whose configuration document was
    /// written against revision 3 and is being applied to revision 5 sent something correct; the
    /// answer is "re-read and try again", which is a different instruction.
    Conflict,
    /// The caller cancelled it.
    ///
    /// Distinct from `Timeout` because nothing went wrong: an operator or an application decided to
    /// stop, and the difference matters when reading a log the next morning.
    Cancelled,
    /// A dependency was not reachable.
    Unavailable,
    /// The request asks for something this model does not do: an output kind it does not produce, a
    /// content kind it does not accept, or a tool it does not call.
    ///
    /// Distinct from `Invalid`, which is a malformed request: the request is well-formed and the model
    /// is simply not the one to serve it, so the instruction is to route elsewhere rather than to fix
    /// the request.
    UnsupportedFeature,
    /// The route names a model this kernel does not have.
    UnknownModel,
    /// The adapter neither honoured nor explained a field the caller set, or claims a field the
    /// request never set.
    ///
    /// An adapter bug rather than a caller's mistake, and distinct from `Internal` because the caller
    /// can do something about it: report the adapter, not the request.
    AdapterContractViolation,
    /// The answer stopped arriving before the provider said it was finished.
    ///
    /// Its own code because the alternative is a lie: the turn would be recorded as a complete answer
    /// with a `Stop` nobody received, and the reader has no way to tell a short answer from a cut
    /// connection. Whatever arrived first stays in the log — the money was spent, and a client has
    /// already rendered it — but the turn failed.
    ProviderTruncated,
    /// The provider's payload could not be decoded into the contract's vocabulary.
    ///
    /// Distinct from `Unavailable`: this is not a dependency being down but an answer that arrived
    /// and could not be read, so retrying the same request re-sends it into the same decoder.
    InvalidProviderResponse,
    /// The call completed and its payload cannot be used: an answer with no content at all.
    ///
    /// A successful empty turn is worse than a refused one: the caller sees a turn that cost money
    /// and produced nothing, and cannot tell it from a model that had nothing to say.
    ProviderResponseUnusable,
    /// A bug.
    Internal,
}

impl ErrorCode {
    /// Whether retrying the same request could succeed.
    ///
    /// A statement about the request, not about the world: `Budget` is retryable only
    /// after the caller grants more credit.
    ///
    /// `ProviderTruncated` and `ProviderResponseUnusable` are here because the *request* is fine and
    /// a second attempt could produce a complete answer. Whether the turn is actually retried is a
    /// different question, and a stricter one: an attempt that produced content is never retried, so
    /// a stream that was cut after streaming text fails the turn rather than becoming two partial
    /// answers.
    ///
    /// `InvalidProviderResponse` is not retryable: the payload is already unreadable, and a retry
    /// hands the same wire format to the same decoder.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Unavailable
                | Self::Timeout
                | Self::ProviderTruncated
                | Self::ProviderResponseUnusable
        )
    }
}

// ---------------------------------------------------------------------------
// Kernel-facing types
// ---------------------------------------------------------------------------

/// Who is asking.
///
/// Travels as data. The kernel never authenticates, never verifies a signature, and never
/// derives a tenant from a request field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Principal {
    /// The tenant this caller belongs to.
    pub tenant: String,
    /// The subject within it.
    pub subject: String,
}

impl Default for Principal {
    /// Nobody. Used as the "unknown owner" value for a snapshot written before the field existed,
    /// never as an authenticated identity — a caller with an empty tenant never matches a real one.
    fn default() -> Self {
        Self {
            tenant: String::new(),
            subject: String::new(),
        }
    }
}

/// A tool, declared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolSpec {
    /// The name the model calls.
    pub name: String,
    /// What it does, in the model's words.
    pub description: String,
    /// A JSON Schema object, kept as text so the kernel never interprets it.
    pub input_schema_json: String,
    /// How the tool is reached, derived by whoever connected it.
    ///
    /// Part of the generation's compatibility check: a tool that keeps its name and schema but now
    /// runs in-process where it used to run in a sandboxed child is a silent downgrade every other
    /// check passes. `None` means the caller did not say, which is not the same as "any" — a
    /// generation that knows must not migrate onto one that does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<Isolation>,
}

/// How a tool is reached, as a fact about the connection rather than a claim about the tool.
///
/// Derived by the daemon from how it reached the source — a spawned child, an HTTP endpoint, an
/// in-process closure — because that is a fact the daemon owns. A source asserting its own isolation
/// would be an unchecked claim dressed as a verified one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// A spawned child process, which the daemon supervises.
    Process,
    /// A remote HTTP endpoint. The process boundary is somebody else's, and so is the isolation.
    Http,
    /// In the kernel's own process: a built-in, or a test double.
    InProcess,
}

/// What a model can do.
///
/// Checked at `Start`, never discovered by trial: a model that silently ignores tools, or
/// silently truncates a prompt, changes the answer without saying so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
/// What a model can do, declared once and enforced before every call.
///
/// The rule for what belongs here, and what does not:
///
/// - **An absence that makes the call pointless is a field here**, and the kernel refuses the request
///   before any I/O: an output modality it does not produce, an input kind it does not accept, a
///   force it will not honour, a schema it cannot be held to.
/// - **An absence that only lowers quality is adapter settings, reported as [`Degraded`]**: a
///   reasoning effort with no channel, a hint the endpoint does not know. The call proceeds and the
///   degradation is recorded, because a call that is slightly worse is still the call that was asked
///   for.
///
/// A field here with no reader is worse than no field: it reads as a guarantee the caller can rely on
/// while the kernel never checks it.
pub struct Capability {
    /// Which content kinds it accepts as input.
    ///
    /// **Empty means undeclared**, which filters nothing: an adapter that has not been taught to
    /// declare must keep working, and the compiler remains the final arbiter for everything a
    /// declaration does not cover. A declared list rejects a request carrying an undeclared kind
    /// before any I/O, which is the difference between a clear refusal and a provider's 400.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<PartKindName>,
    /// Which content kinds it can produce.
    ///
    /// Same rule: empty is undeclared. A declared list is what makes `Output` checkable before a call
    /// is sent — a model that produces text and images, asked for audio, is a mistake the kernel can
    /// see without paying for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<PartKindName>,
    /// Whether it accepts tool definitions and emits tool calls.
    pub tool_calling: bool,
    /// Whether it can be *made* to call one. A provider that accepts tools but ignores
    /// `tool_choice = required` turns a configured guarantee into a suggestion, which is why the
    /// kernel refuses the request rather than sending it.
    pub tool_choice_required: bool,
    /// Whether the answer can be held to a JSON Schema.
    ///
    /// A model that cannot be is a model whose `format: json_schema` request buys either a refusal or
    /// an unvalidated answer — the call has no purpose, so it is not sent.
    pub structured_output: bool,
    /// Total context, prompt plus completion.
    pub context_window: u32,
    /// The largest completion the provider will produce.
    pub max_output_tokens: u32,
    /// The largest inline media payload this model accepts, in bytes.
    ///
    /// `None` means undeclared, which filters nothing. A token limit says nothing about a file, so an
    /// audio clip needs its own ceiling; without one a request can be small in tokens and enormous in
    /// bytes, and the provider's refusal is discovered after the upload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_bytes: Option<u64>,
}

impl Capability {
    /// Whether this model declared nothing about the content kinds it accepts.
    #[must_use]
    pub fn accepts_undeclared_inputs(&self) -> bool {
        self.inputs.is_empty()
    }

    /// Whether this model declared nothing about the content kinds it produces.
    #[must_use]
    pub fn produces_undeclared_outputs(&self) -> bool {
        self.outputs.is_empty()
    }

    /// Check a declaration that is about to become a model a route can name.
    ///
    /// Run when a model is registered, not when a call is made. A declaration that cannot serve any
    /// call is a deployment error, and per-call discovery means discovering it in front of a user:
    /// a model with no declared window is refused by the fit check every time, and an `outputs` list
    /// naming something that is not an output is a `Output` no caller can ask for.
    ///
    /// # Errors
    ///
    /// An output kind that is not a modality, a duplicate kind, or a window of zero.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let mut seen = std::collections::BTreeSet::new();
        for kind in &self.inputs {
            if !seen.insert(*kind) {
                return Err(format!("input content kind {kind:?} is declared twice"));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for kind in &self.outputs {
            if !matches!(
                kind,
                PartKindName::Text
                    | PartKindName::Image
                    | PartKindName::Audio
                    | PartKindName::Video
            ) {
                return Err(format!(
                    "{kind:?} is not an output modality: a call can ask for text, an image, audio or \
                     video, and nothing else"
                ));
            }
            if !seen.insert(*kind) {
                return Err(format!("output content kind {kind:?} is declared twice"));
            }
        }
        if self.context_window == 0 {
            return Err(
                "a model that declares no context window cannot be held to one, so every call to it \
                 would be refused: declare the window"
                    .to_owned(),
            );
        }
        Ok(())
    }
}

/// What a call asks the model to produce.
///
/// The only modality switch in the contract: the same request type, the same response type and the
/// same `invoke` serve every output, because the answer is a [`Content`] whose parts say what
/// arrived. A modality that lands later adds one constant, not a request type — and everything
/// modality-specific that is not the modality (an image's size, a voice) is adapter configuration or
/// an extension, never a field here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Output {
    /// The next assistant turn: text, tool calls, or both.
    #[default]
    Text,
    /// Generated images.
    Image,
    /// Synthesized speech.
    Audio,
    /// Generated video.
    Video,
}

impl Output {
    /// The content kind this output produces, for a capability check.
    #[must_use]
    pub fn part_kind(self) -> PartKindName {
        match self {
            Self::Text => PartKindName::Text,
            Self::Image => PartKindName::Image,
            Self::Audio => PartKindName::Audio,
            Self::Video => PartKindName::Video,
        }
    }
}

/// The content kinds, without their payloads.
///
/// [`PartKind`] carries data, which makes it unusable as a set member for a capability check; this is
/// the same vocabulary with nothing in it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PartKindName {
    /// Text.
    Text,
    /// An image.
    Image,
    /// A file reference.
    File,
    /// Structured data.
    Data,
    /// A tool call.
    ToolCall,
    /// A tool result.
    ToolResult,
    /// A reasoning trace.
    Reasoning,
    /// Synthesized audio.
    Audio,
    /// Generated video.
    Video,
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

/// How strongly a tool's execution is separated from the kernel.
///
/// A call waiting on a human.
///
/// Part of the snapshot because the wait outlives any process: a session parked on a
/// confirmation must be resumable tomorrow, and the call it is holding has to come back with
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PendingCall {
    /// What this question is about, and how it is identified in a log.
    pub id: String,
    /// What has not run yet.
    pub action: PendingAction,
    /// What may be answered.
    pub options: Vec<PermissionOption>,
    /// What was asked.
    pub question: String,
}

/// What a parked question is about.
///
/// Approval is not a tool concept: a policy can ask a human about a tool call *or* about running a
/// command outside its sandbox, and both are answered the same way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingAction {
    /// A tool call the policy asked about.
    Tool {
        /// The call.
        call: ToolCall,
    },
    /// A command the policy asked about — an escalation, or a program nobody has allowed yet.
    Exec {
        /// What would run, exactly.
        spec: ProcessSpec,
    },
}

/// A session's state, materialised at a point in its log.
///
/// `seq` is the event this snapshot already includes, so recovery is `load(snapshot)` then
/// `events_since(snapshot.seq + 1)`. Two numbers that must agree, written down once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    /// The session it belongs to.
    pub session: SessionId,
    /// Who owns it.
    ///
    /// Recorded because the store keys sessions by id alone: without this, a second tenant could
    /// load the first tenant's session by guessing its name. `load` refuses a mismatch, and an
    /// identifier that already belongs to somebody else cannot be opened.
    #[serde(default)]
    pub principal: Principal,
    /// The last event folded in.
    pub seq: u64,
    /// How many times this session has been checkpointed.
    ///
    /// The store refuses a write that is not strictly newer, which is what makes a **stale handle**
    /// detectable: two requests can load the same session, and without this the second one to finish
    /// overwrites the first one's work with a materialisation of a log it read before that work
    /// existed. The version is the only thing in the snapshot that says "I have seen less than you".
    #[serde(default)]
    pub version: u64,
    /// The tool-source generation the session is pinned to.
    pub generation: Generation,
    /// Where the session was.
    pub state: SessionState,
    /// The conversation as of `seq`.
    pub messages: Vec<Message>,
    /// What it has spent.
    pub usage: Usage,
    /// How many inference calls it has made.
    pub inference_calls: u64,
    /// The limits this session was opened with.
    ///
    /// In the checkpoint for the same reason the credit is: over the ABI every request loads the
    /// session, and a budget that is re-supplied per request is a budget the caller can forget — or
    /// quietly widen — by sending a different one. What a session was opened with is part of what the
    /// session *is*.
    #[serde(default)]
    pub budget: Budget,
    /// Wall-clock milliseconds the session has already burned.
    ///
    /// Separate from the start time because the two answer different questions. A session that waited
    /// an hour for a human verdict has not spent an hour of model time, and a `max_wall_millis` that
    /// charged it would fire on every session that used parked calls. Accumulated at each checkpoint,
    /// so a reload continues the clock instead of restarting it — which is what made the wall budget
    /// impossible to hit over the ABI.
    #[serde(default)]
    pub spent_wall_millis: u64,
    /// Every attempt this session has made, and the bill they add up to.
    ///
    /// In the checkpoint because it is the record, not a statistic: a resumed session that reported
    /// nothing spent would under-report exactly the sessions that were interrupted, which are the ones
    /// somebody is reconciling.
    #[serde(default)]
    pub ledger: Ledger,
    /// Tokens granted and not yet spent. `None` means no limit.
    ///
    /// In the checkpoint because every syscall may be served by a different process: a budget
    /// that resets on reload is not a budget.
    pub credit_tokens: Option<u64>,
    /// Calls waiting on a human. A batch can have more than one.
    #[serde(default)]
    pub pending: Vec<PendingCall>,
    /// The route this session was started with.
    ///
    /// In the checkpoint because a resume that had to be told its own route again is a resume
    /// that can silently behave differently — and the caller cannot know what to send.
    pub routing: Routing,
    /// Standing instructions.
    pub system: Option<String>,
    /// Tool names this session may use.
    pub tools: Vec<String>,
    /// How answers are shaped.
    pub params: GenerateParams,
    /// Where the session's commands run, and the default writable root.
    pub workspace: Option<String>,
    /// What its commands may touch.
    pub sandbox: SandboxPolicy,
    /// The adapter hints in force.
    #[serde(default)]
    pub hints: Hints,
    /// The exposure policy in force.
    #[serde(default)]
    pub tool_policy: ToolPolicy,
    /// Tools discovered or used, and the turn each was last reached.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub discovered: BTreeMap<String, u64>,
    /// Turns taken, which the pool expires against.
    #[serde(default)]
    pub round: u64,
    /// Why the session stopped, once it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopReason>,
    /// The application's own state.
    ///
    /// Opaque: the kernel stores it, updates it atomically and restores it, and never reads a
    /// key. Every write is also an event, so a replay reproduces the value the application saw.
    #[serde(default)]
    pub data: BTreeMap<String, serde_json::Value>,
    /// What this session is for.
    #[serde(default)]
    pub kind: SessionKind,
    /// The process, when this is a process session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessState>,
}

// ---------------------------------------------------------------------------
// Route and ledger
// ---------------------------------------------------------------------------

/// Why a model call is happening.
///
/// The kernel makes calls of its own — a compaction summary is one — so the purpose has to be
/// first-class: a budget whose summarisation spend is indistinguishable from answer spend is a
/// budget nobody can explain after the fact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// The answer the caller is waiting for.
    #[default]
    Answer,
    /// The kernel's own bookkeeping.
    Summary,
    /// Another try at a turn that produced something unusable.
    Repair,
}

/// The route a call takes, in order.
///
/// A chain rather than a name because the kernel owns retries and fallback: a runtime that
/// gives up on the first 503 has the availability of its least reliable provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Routing {
    /// Candidate models, first that answers wins.
    pub chain: Vec<String>,
    /// Extra attempts against the same target before moving to the next.
    pub retries: u32,
}

/// Where in a call an attempt happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttemptPhase {
    /// Compiling and checking, before anything was sent.
    Preflight,
    /// The provider call itself.
    Execute,
    /// Refused before it happened.
    CircuitOpen,
}

/// What caused an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttemptTrigger {
    /// The first attempt for the chosen target.
    Selection,
    /// Another try at the same target.
    Retry,
    /// A hop to the next target.
    Fallback,
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// It answered.
    Ok,
    /// It did not. Present even when a partial answer was streamed, because that one still
    /// cost money.
    Failed {
        /// A stable category.
        code: ErrorCode,
        /// For humans.
        reason: String,
    },
}

/// One knob a request can set.
///
/// A vocabulary rather than a free string: [`Degraded`] names the knob it could not honour, and two
/// spellings of one knob turn "how often is `stop` dropped" into a question nobody can answer. It is
/// also what makes an adapter's silence detectable — a request reports the fields it set, an adapter
/// reports the fields it will honour, and the kernel refuses to call one that did not account for the
/// difference.
///
/// The set is small on purpose: one name per knob that appears on [`crate::adapter::ModelRequest`],
/// not one name per provider parameter. A knob only one provider understands is an
/// [`Field::Extension`], qualified with the provider and extension that own it.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
pub enum Field {
    /// The standing instructions.
    System,
    /// The conversation.
    Messages,
    /// The tool declarations.
    Tools,
    /// How to choose among them.
    ToolChoice,
    /// What the call asks the model to produce.
    Output,
    /// One sampling knob from [`GenerateParams`].
    Param(Param),
    /// One adapter hint from [`Hints`].
    Hint(Hint),
    /// A provider-specific option, named by the extension that owns it.
    Extension {
        /// The provider the option belongs to.
        provider: String,
        /// The extension within that provider.
        id: String,
        /// The option within that extension.
        option: String,
    },
}

/// A sampling knob, named the way it is spelled on the wire: `param.temperature`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Param {
    /// The completion ceiling.
    MaxOutputTokens,
    /// Sampling temperature.
    Temperature,
    /// Nucleus sampling.
    TopP,
    /// Stop sequences.
    Stop,
    /// Reasoning effort.
    ReasoningEffort,
    /// Whether one turn may call several tools.
    ParallelToolCalls,
    /// The shape the answer must have.
    Format,
}

/// An adapter hint, named the way it is spelled on the wire: `hint.verbosity`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Hint {
    /// How much prose an answer should carry.
    Verbosity,
    /// How much reasoning to summarise.
    ReasoningSummary,
    /// The cost/latency trade.
    ServiceTier,
    /// A ceiling on tool calls in one response.
    MaxToolCalls,
    /// What to do when the conversation does not fit.
    Truncation,
    /// Labels the provider echoes back.
    Metadata,
}

impl Field {
    /// The wire spelling: `tools`, `param.stop`, `extension.openai.generation.effort`.
    #[must_use]
    pub fn as_str(&self) -> String {
        match self {
            Self::System => "system".to_owned(),
            Self::Messages => "messages".to_owned(),
            Self::Tools => "tools".to_owned(),
            Self::ToolChoice => "tool_choice".to_owned(),
            Self::Output => "output".to_owned(),
            Self::Param(param) => format!("param.{}", param.name()),
            Self::Hint(hint) => format!("hint.{}", hint.name()),
            Self::Extension {
                provider,
                id,
                option,
            } => format!("extension.{provider}.{id}.{option}"),
        }
    }
}

impl Param {
    /// The name as it appears in a qualified field.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::MaxOutputTokens => "max_output_tokens",
            Self::Temperature => "temperature",
            Self::TopP => "top_p",
            Self::Stop => "stop",
            Self::ReasoningEffort => "reasoning_effort",
            Self::ParallelToolCalls => "parallel_tool_calls",
            Self::Format => "format",
        }
    }
}

impl Hint {
    /// The name as it appears in a qualified field.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Verbosity => "verbosity",
            Self::ReasoningSummary => "reasoning_summary",
            Self::ServiceTier => "service_tier",
            Self::MaxToolCalls => "max_tool_calls",
            Self::Truncation => "truncation",
            Self::Metadata => "metadata",
        }
    }
}

impl std::fmt::Display for Field {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl TryFrom<String> for Field {
    type Error = String;

    fn try_from(text: String) -> std::result::Result<Self, Self::Error> {
        let fail = || format!("unknown field {text:?}");
        if let Some(rest) = text.strip_prefix("param.") {
            return match rest {
                "max_output_tokens" => Ok(Self::Param(Param::MaxOutputTokens)),
                "temperature" => Ok(Self::Param(Param::Temperature)),
                "top_p" => Ok(Self::Param(Param::TopP)),
                "stop" => Ok(Self::Param(Param::Stop)),
                "reasoning_effort" => Ok(Self::Param(Param::ReasoningEffort)),
                "parallel_tool_calls" => Ok(Self::Param(Param::ParallelToolCalls)),
                "format" => Ok(Self::Param(Param::Format)),
                _ => Err(fail()),
            };
        }
        if let Some(rest) = text.strip_prefix("hint.") {
            return match rest {
                "verbosity" => Ok(Self::Hint(Hint::Verbosity)),
                "reasoning_summary" => Ok(Self::Hint(Hint::ReasoningSummary)),
                "service_tier" => Ok(Self::Hint(Hint::ServiceTier)),
                "max_tool_calls" => Ok(Self::Hint(Hint::MaxToolCalls)),
                "truncation" => Ok(Self::Hint(Hint::Truncation)),
                "metadata" => Ok(Self::Hint(Hint::Metadata)),
                _ => Err(fail()),
            };
        }
        if let Some(rest) = text.strip_prefix("extension.") {
            let mut parts = rest.splitn(3, '.');
            let (Some(provider), Some(id), Some(option)) =
                (parts.next(), parts.next(), parts.next())
            else {
                return Err(fail());
            };
            return Ok(Self::Extension {
                provider: provider.to_owned(),
                id: id.to_owned(),
                option: option.to_owned(),
            });
        }
        match text.as_str() {
            "system" => Ok(Self::System),
            "messages" => Ok(Self::Messages),
            "tools" => Ok(Self::Tools),
            "tool_choice" => Ok(Self::ToolChoice),
            "output" => Ok(Self::Output),
            _ => Err(fail()),
        }
    }
}

impl From<Field> for String {
    fn from(field: Field) -> Self {
        field.as_str()
    }
}

/// A field the provider could not honour.
///
/// Recorded rather than swallowed: a `tool_choice = Required` that quietly became `Auto` is a
/// session behaving differently than it was configured to, and the difference is
/// undiscoverable afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Degraded {
    /// The request field, e.g. [`Field::ToolChoice`].
    pub field: Field,
    /// Why, in the provider's terms.
    pub reason: String,
    /// A stable, driver-defined machine label for the adjustment — `implicit_off`, `quantized`,
    /// `not_supported`.
    ///
    /// A caller aggregates on the code rather than parsing the sentence, which is how a counter can
    /// answer "how often is a knob dropped here" without learning the provider's phrasing. Absent is
    /// allowed: a provider that cannot label it still says what happened, but cannot be counted on
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl Degraded {
    /// A degradation without an explanation is a second silent failure.
    ///
    /// # Errors
    ///
    /// An empty or whitespace-only reason.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.reason.trim().is_empty() {
            return Err(format!(
                "a degradation of `{}` has no explanation",
                self.field.as_str()
            ));
        }
        Ok(())
    }
}

/// One attempt, recorded whether or not it produced anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Attempt {
    /// Position in the ledger.
    pub index: u32,
    /// The target. Empty for a preflight refusal, which never had one.
    pub model: String,
    /// Where it happened.
    pub phase: AttemptPhase,
    /// What caused it.
    pub trigger: AttemptTrigger,
    /// How it ended.
    pub outcome: AttemptOutcome,
    /// What it consumed, if anything.
    pub usage: Usage,
    /// How long it took.
    pub duration_millis: u64,
    /// Fields this attempt could not honour.
    pub degraded: Vec<Degraded>,
}

/// Every attempt a call made, and the sum.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Ledger {
    /// In order.
    pub attempts: Vec<Attempt>,
    /// The sum across every attempt that produced tokens.
    pub total: Usage,
}

impl Ledger {
    /// Record an attempt and fold its usage into the total.
    ///
    /// Folding happens here rather than at the call site so that a failed attempt that still
    /// streamed cannot be quietly left out of the bill.
    ///
    /// The first attempt *defines* the accumulator instead of being added to it: a fresh total
    /// is zeros-and-unknown, and "nothing recorded yet" is not the same as "a report was
    /// missing". Adding the first report to an empty total would otherwise make every cost
    /// unknown — which is what an empty total looks like, not what the attempt said.
    pub fn record(&mut self, attempt: Attempt) {
        if self.attempts.is_empty() {
            self.total = attempt.usage.clone();
        } else {
            self.total.add(&attempt.usage);
        }
        self.attempts.push(attempt);
    }

    /// Fold one attempt in by index, replacing what a streamed usage report built.
    ///
    /// Replay sees an attempt twice: once as the `usage` reports it streamed, once as the
    /// `attempt_failed` that ended it. Appending both would bill the tokens twice and leave two
    /// entries for one attempt, which is the opposite of what the ledger is for. The later report is
    /// the better one — it knows how the attempt ended — so it replaces the entry rather than adding.
    pub fn adopt(&mut self, attempt: Attempt) {
        match self
            .attempts
            .iter_mut()
            .find(|existing| existing.index == attempt.index)
        {
            Some(existing) => *existing = attempt,
            None => self.attempts.push(attempt),
        }
        self.attempts.sort_by_key(|attempt| attempt.index);
        self.recompute();
    }

    /// The total, derived from the attempts rather than accumulated beside them.
    ///
    /// Two accumulators that must agree is one accumulator too many: replay can insert an attempt that
    /// was never added to the total, and the bill would then disagree with its own entries.
    pub fn recompute(&mut self) {
        let mut total: Option<Usage> = None;
        for attempt in &self.attempts {
            match &mut total {
                // The first attempt defines the accumulator rather than being added to one, so an
                // unreported counter stays unreported instead of being added to a zero that was
                // never a report.
                None => total = Some(attempt.usage.clone()),
                Some(total) => total.add(&attempt.usage),
            }
        }
        self.total = total.unwrap_or_default();
    }

    /// How many attempts were made.
    #[must_use]
    pub fn len(&self) -> usize {
        self.attempts.len()
    }

    /// Whether nothing was attempted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.attempts.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Generation parameters
// ---------------------------------------------------------------------------

/// How the model should choose among the tools it was given.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// It decides.
    #[default]
    Auto,
    /// It must call one.
    Required,
    /// It must not call one.
    None,
    /// It must call this one, by name.
    ///
    /// The name is the whole point: structured extraction asks for one function and reads its
    /// arguments, and "call something" is not the same instruction. The kernel refuses a name the
    /// request does not offer, because a forced call to a tool the model cannot see fails at the
    /// provider and costs a round trip to find out.
    Function {
        /// The tool it must call.
        name: String,
    },
}

/// How hard a reasoning model should think.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// Barely.
    Minimal,
    /// A little.
    Low,
    /// Normal.
    Medium,
    /// As much as it costs.
    High,
}

/// The shape the answer must have.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TextFormat {
    /// Prose.
    #[default]
    Text,
    /// A JSON object, schema unchecked.
    JsonObject,
    /// A JSON object that must match a schema.
    JsonSchema {
        /// The name the provider files it under.
        name: String,
        /// The schema, serialized.
        schema_json: String,
    },
}

/// What the caller wants from one call.
///
/// Every field is carried, not decided: the kernel has no opinion about sampling. They live
/// here rather than as a bag of extras because a bag is a second extension hatch, and these are
/// the parameters every provider has.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GenerateParams {
    /// The completion ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Nucleus sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Sequences that end the completion.
    ///
    /// Not every provider has this — the OpenAI Responses API rejects it outright — so an adapter
    /// that cannot send it reports a [`Degraded`] rather than dropping it in silence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// Reasoning effort, for models that expose one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// How to choose among tools.
    #[serde(default)]
    pub tool_choice: ToolChoice,
    /// Whether one turn may call several tools at once.
    ///
    /// In the contract rather than in an adapter because the *kernel* acts on it: `invoke_tools`
    /// fans a batch out concurrently. A provider that forbids it and a kernel that does it anyway
    /// would disagree about what happened in a turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// The shape the answer must have.
    #[serde(default)]
    pub format: TextFormat,
}

impl GenerateParams {
    /// Field-by-field merge: `over` wins where it sets something, and the rest is inherited.
    ///
    /// A per-call override is a patch, not a replacement: a caller changing only the temperature
    /// should not also have to repeat the stop sequences, the format and the tool choice. Defaults
    /// mean "inherit", which is why `Auto` and `Text` are not treated as overrides.
    #[must_use]
    pub fn merged(&self, over: &Self) -> Self {
        Self {
            max_output_tokens: over.max_output_tokens.or(self.max_output_tokens),
            temperature: over.temperature.or(self.temperature),
            top_p: over.top_p.or(self.top_p),
            stop: if over.stop.is_empty() {
                self.stop.clone()
            } else {
                over.stop.clone()
            },
            reasoning_effort: over.reasoning_effort.or(self.reasoning_effort),
            tool_choice: if over.tool_choice == ToolChoice::Auto {
                self.tool_choice.clone()
            } else {
                over.tool_choice.clone()
            },
            parallel_tool_calls: over.parallel_tool_calls.or(self.parallel_tool_calls),
            format: if over.format == TextFormat::Text {
                self.format.clone()
            } else {
                over.format.clone()
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Sandboxing
// ---------------------------------------------------------------------------

/// What a session's commands may touch.
///
/// Set when the session opens and carried in its checkpoint. It is **not** a field of a request:
/// a caller that can name its own policy can name `DangerFullAccess`, and a permission slip the
/// untrusted side writes is not a permission model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SandboxPolicy {
    /// No restriction. Use with caution, and only where the code is yours.
    DangerFullAccess,
    /// Read everything, write nothing.
    ReadOnly {
        /// Whether outbound network is allowed.
        #[serde(default)]
        network_access: bool,
    },
    /// Write inside the session's workspace and nothing else.
    WorkspaceWrite {
        /// Extra roots outside the workspace that may be written.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        extra_writable_roots: Vec<String>,
        /// Whether outbound network is allowed.
        #[serde(default)]
        network_access: bool,
    },
    /// The process is already outside our reach; only the network setting is ours to honour.
    ExternalSandbox {
        /// Whether outbound network is allowed.
        #[serde(default)]
        network_access: bool,
    },
}

/// The default is the useful one, not the loosest: write inside the workspace, no network.
impl Default for SandboxPolicy {
    fn default() -> Self {
        Self::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: false,
        }
    }
}

impl SandboxPolicy {
    /// The name a policy rule matches, and the name an error message uses.
    ///
    /// A string rather than a `Debug` rendering: a rule reads `{"sandbox": "workspace_write"}`, and a
    /// rename in the enum has to be a decision about the wire, not a side effect of refactoring.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::DangerFullAccess => "danger_full_access",
            Self::ReadOnly { .. } => "read_only",
            Self::WorkspaceWrite { .. } => "workspace_write",
            Self::ExternalSandbox { .. } => "external_sandbox",
        }
    }

    /// Every root this policy would let a command write to.
    ///
    /// Both the session's workspace and the extra roots a caller may add: bounding one and not the
    /// other would leave the widening reachable through the field nobody bounded.
    #[must_use]
    pub fn writable_roots<'a>(&'a self, workspace: Option<&'a str>) -> Vec<&'a str> {
        let mut roots = Vec::new();
        match self {
            Self::DangerFullAccess | Self::ReadOnly { .. } => {}
            Self::WorkspaceWrite {
                extra_writable_roots,
                ..
            } => {
                if let Some(workspace) = workspace {
                    roots.push(workspace);
                }
                roots.extend(extra_writable_roots.iter().map(String::as_str));
            }
            // Outside the kernel's reach: the policy is somebody else's, so nothing here bounds it.
            Self::ExternalSandbox { .. } => {}
        }
        roots
    }

    /// The narrowest policy: read only, no network.
    #[must_use]
    pub fn read_only() -> Self {
        Self::ReadOnly {
            network_access: false,
        }
    }

    /// Whether outbound network is permitted under this policy.
    #[must_use]
    pub fn network_access(&self) -> bool {
        match self {
            Self::DangerFullAccess => true,
            Self::ReadOnly { network_access }
            | Self::WorkspaceWrite { network_access, .. }
            | Self::ExternalSandbox { network_access } => *network_access,
        }
    }

    /// Whether moving from this policy to `next` only ever takes room away.
    ///
    /// The same rule tools follow — narrowing is a configuration change, widening is not — applied
    /// to the one thing a session is allowed to change about its own confinement. A session that
    /// wants *more* access does not reconfigure itself; it asks for an escalation on one command,
    /// which the gate answers.
    ///
    /// The order is by what a process may touch, not by name: `read_only` is narrower than
    /// `workspace_write` because it grants no writes at all, and `danger_full_access` is wider than
    /// both. `external_sandbox` is not a rung on that ladder — it means the kernel is not enforcing
    /// anything — so entering it from a policy the kernel *does* enforce is a widening, and leaving
    /// it is not.
    #[must_use]
    pub fn narrows_to(&self, next: &Self) -> bool {
        match (self, next) {
            // Widening, in any of its forms.
            (_, Self::DangerFullAccess) => false,
            (Self::ReadOnly { .. } | Self::WorkspaceWrite { .. }, Self::ExternalSandbox { .. }) => {
                false
            }

            // From full access, or from outside our reach, anything the kernel enforces is
            // narrower.
            (Self::DangerFullAccess, _) | (Self::ExternalSandbox { .. }, _) => true,

            // Within one rung: fewer roots, and no network that was not already allowed.
            (
                Self::ReadOnly {
                    network_access: was,
                },
                Self::ReadOnly { network_access },
            ) => !network_access || *was,
            (Self::ReadOnly { .. }, _) => false,

            // A read-only policy grants no writes, so it is narrower from any writing policy.
            (
                Self::WorkspaceWrite {
                    network_access: was,
                    ..
                },
                Self::ReadOnly { network_access },
            ) => !network_access || *was,
            (
                Self::WorkspaceWrite {
                    extra_writable_roots: was,
                    network_access: was_net,
                },
                Self::WorkspaceWrite {
                    extra_writable_roots,
                    network_access,
                },
            ) => {
                // Compared as written: a root that only differs by a symlink is a different grant,
                // and treating it as equal would let a session swap one directory for another.
                (!network_access || *was_net)
                    && extra_writable_roots.iter().all(|root| was.contains(root))
            }
        }
    }
}

/// Extra room asked for by one command.
///
/// Only ever widens: a caller cannot use it to narrow the session's policy, because narrowing by
/// request would let one command quietly change what a later command is allowed to assume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdditionalPermissions {
    /// Roots outside the workspace this command may write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_roots: Vec<String>,
    /// Whether this command may reach the network.
    #[serde(default)]
    pub network_access: bool,
}

/// What a single `exec` asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SandboxPermissions {
    /// Run under the session's policy.
    #[default]
    UseDefault,
    /// Run outside the sandbox. The gate turns this into a human question, not a formality.
    RequireEscalated,
    /// Stay in the sandbox, with more room for this command only.
    WithAdditionalPermissions {
        /// What to add. Never what to remove.
        additional: AdditionalPermissions,
    },
}

/// Which stream a chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// A piece of captured output, and whether it was cut.
///
/// Truncation is carried rather than applied silently: a model that cannot see that its output
/// was shortened will reason about a file it only half read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OutputChunk {
    /// The text kept.
    pub text: String,
    /// The line after which the rest was dropped, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_after_lines: Option<u32>,
}

/// How one command ended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExecOutput {
    /// The exit status, or `-1` when the process was killed by a signal.
    pub exit_code: i32,
    /// Standard output.
    pub stdout: OutputChunk,
    /// Standard error.
    pub stderr: OutputChunk,
    /// How long it ran.
    pub duration_millis: u64,
    /// Whether it was killed for exceeding its deadline.
    pub timed_out: bool,
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// What a session is for.
///
/// A process session is not a second kind of object: it is the same lifecycle — single writer,
/// event log, budget, sandbox, checkpoint — with a different syscall face. In Unix a shell
/// session is a process, and the same reasoning applies here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// Runs a model loop.
    #[default]
    Agent,
    /// Runs one command, and stays open while it does.
    Process,
}

/// How to start a process session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessSpec {
    /// The command line. No shell by default: an argv cannot be word-split by accident.
    pub argv: Vec<String>,
    /// Where to run it. Absent means the session's workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Environment to add.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// What this command asks for on top of the session's policy.
    #[serde(default)]
    pub permissions: SandboxPermissions,
}

/// Where a process is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProcessStatus {
    /// No pid yet: the session exists, the process does not.
    #[default]
    Pending,
    /// Running, under whatever isolation the platform could give.
    Running {
        /// The process id.
        pid: u32,
    },
    /// It ended.
    Exited {
        /// How.
        output: ExecOutput,
    },
    /// The process is gone and we do not know how it ended.
    ///
    /// A live process cannot be reconstructed from a log — its memory, its file descriptors and
    /// its cursor are not in the event stream. So a session that outlives the runtime says this,
    /// rather than pretending the command is still running.
    Lost,
}

/// A process session's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessState {
    /// How it was started.
    pub spec: ProcessSpec,
    /// Where it is now.
    pub status: ProcessStatus,
}

/// Where a process is, without the payload.
///
/// A filter asks "which ended?", not "which ended with exit code 3", so the tag is its own type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessPhase {
    /// No pid yet.
    Pending,
    /// Running.
    Running,
    /// It ended.
    Exited,
    /// It is gone and we do not know how it ended.
    Lost,
}

impl ProcessStatus {
    /// The phase, without the payload.
    #[must_use]
    pub fn phase(&self) -> ProcessPhase {
        match self {
            Self::Pending => ProcessPhase::Pending,
            Self::Running { .. } => ProcessPhase::Running,
            Self::Exited { .. } => ProcessPhase::Exited,
            Self::Lost => ProcessPhase::Lost,
        }
    }
}

impl ProcessState {
    /// Whether the process has ended, one way or another.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        matches!(
            self.status,
            ProcessStatus::Exited { .. } | ProcessStatus::Lost
        )
    }
}

// ---------------------------------------------------------------------------
// Hints: the parameters that belong to a session, not to a call
// ---------------------------------------------------------------------------

/// Preferences an adapter may honour, and must report when it cannot.
///
/// Separate from [`GenerateParams`] because these change while a session runs — a conversation
/// that is filling its window wants less prose, and a review turn wants more detail — and because
/// they are *hints*: an adapter that has no channel for one reports a [`Degraded`] rather than
/// failing the call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Hints {
    /// How much prose an answer should carry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<Verbosity>,
    /// How much of the reasoning to summarise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_summary: Option<Summary>,
    /// The cost/latency trade this session prefers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    /// A ceiling on tool calls in one response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// What to do when the conversation does not fit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<Truncation>,
    /// Labels the provider echoes back, up to sixteen.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl Hints {
    /// A patch that leaves every field `None` changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Take every field this patch sets, and keep the rest.
    #[must_use]
    pub fn merged(&self, over: &Self) -> Self {
        Self {
            verbosity: over.verbosity.or(self.verbosity),
            reasoning_summary: over.reasoning_summary.or(self.reasoning_summary),
            service_tier: over.service_tier.or(self.service_tier),
            max_tool_calls: over.max_tool_calls.or(self.max_tool_calls),
            truncation: over.truncation.or(self.truncation),
            metadata: if over.metadata.is_empty() {
                self.metadata.clone()
            } else {
                over.metadata.clone()
            },
        }
    }
}

/// How much prose an answer should carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verbosity {
    /// Terse.
    Low,
    /// Normal.
    Medium,
    /// Expansive.
    High,
}

/// How much of the reasoning to summarise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Summary {
    /// The provider decides.
    Auto,
    /// Short.
    Concise,
    /// Long.
    Detailed,
}

/// The cost/latency trade a session prefers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    /// Let the provider decide.
    Auto,
    /// The default tier.
    Default,
    /// Cheaper, slower.
    Flex,
    /// More expensive, faster.
    Priority,
}

/// What to do when the conversation does not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Truncation {
    /// Drop the oldest items.
    Auto,
    /// Fail instead of dropping silently.
    Disabled,
}

/// How visible a tool is, as distinct from whether it may be used.
///
/// The ceiling (`config.tools`) is the application's to narrow and only ever narrows. Exposure is
/// what is in the prompt right now: it may grow up to the ceiling, by configuration or by a search
/// tool asking. Keeping the two apart is what makes dynamic tool loading safe.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Exposure {
    /// In the prompt every turn. Keep this set small: it is the stable baseline.
    Always,
    /// In the prompt once it has been found or used, and while it stays in the pool.
    Direct,
    /// Invisible until a search surfaces it. The default, and the reason discovery exists.
    #[default]
    Deferred,
    /// Never in the prompt, not even via search. Still callable by name — hiding a tool is not
    /// forbidding it.
    Hidden,
}

impl Exposure {
    /// Which of two visible tools to keep when the budget cannot hold both. Lower wins.
    ///
    /// `Always` outranks `Direct`, which outranks `Deferred`: a baseline that disappears because
    /// the model searched too much is worse than a found tool that expires.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Always => 0,
            Self::Direct => 1,
            Self::Deferred => 2,
            Self::Hidden => 3,
        }
    }

    /// Whether a search may surface it.
    #[must_use]
    pub fn searchable(self) -> bool {
        matches!(self, Self::Direct | Self::Deferred)
    }
}

/// How many tool definitions may reach the model per turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DefinitionBudget {
    /// How many definitions, counted before the prompt is built.
    pub max_definitions: usize,
    /// What they may weigh together, in bytes of name, description and schema.
    pub max_bytes: u64,
}

impl Default for DefinitionBudget {
    /// Thirty-two definitions or 16 KiB, whichever comes first.
    ///
    /// Both caps, because the two failures are different: forty one-line tools are cheap and
    /// confusing, four tools with enormous schemas are expensive and clear.
    fn default() -> Self {
        Self {
            max_definitions: 32,
            max_bytes: 16 * 1024,
        }
    }
}

/// What a session remembers having found.
///
/// Separate from the per-turn budget on purpose: the budget caps what a prompt shows, the pool caps
/// what the session remembers. A session that can only show eight tools may still have discovered
/// thirty, and re-searching for one it used two turns ago is the waste this prevents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DiscoveryPolicy {
    /// How many discovered tools stay remembered.
    pub max_tools: usize,
    /// What they may weigh together.
    pub max_bytes: u64,
    /// How many turns without a use before a tool is forgotten.
    pub idle_rounds: u64,
}

impl Default for DiscoveryPolicy {
    fn default() -> Self {
        Self {
            max_tools: 32,
            max_bytes: 16 * 1024,
            idle_rounds: 10,
        }
    }
}

/// The host's exposure policy for one session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolPolicy {
    /// The exposure of every tool without an entry in `exposures`. Defaults to `Deferred`.
    #[serde(default)]
    pub default: Exposure,
    /// Per-tool overrides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exposures: BTreeMap<String, Exposure>,
    /// What a prompt may carry this turn.
    #[serde(default)]
    pub budget: DefinitionBudget,
    /// What the session remembers.
    #[serde(default)]
    pub discovery: DiscoveryPolicy,
}

impl ToolPolicy {
    /// The exposure of one tool.
    #[must_use]
    pub fn exposure_of(&self, name: &str) -> Exposure {
        self.exposures.get(name).copied().unwrap_or(self.default)
    }

    /// Refuse a policy that would make discovery impossible.
    ///
    /// A hidden or deferred `tool_search` is a catalog nobody can reach: the model is told to search
    /// with a tool it cannot see. Absence is not a refusal — an unconfigured search tool is seeded
    /// to `Always` by the kernel, so the default policy discovers rather than hides.
    pub fn validate(&self, search_tool: &str) -> crate::error::Result<()> {
        if self
            .exposures
            .get(search_tool)
            .is_some_and(|exposure| *exposure != Exposure::Always)
        {
            return Err(crate::error::Error::invalid(format!(
                "{search_tool} must be exposed as always; any other exposure disables discovery"
            )));
        }
        Ok(())
    }
}

/// A partial update to a session's mutable configuration.
///
/// The wiring — kind, sandbox, workspace — is not here, because it is decided when the session
/// opens and is not the application's to change while it runs. Everything in this struct is
/// content or strategy, and every field is optional: absent means unchanged, `null` (where the
/// field is doubly optional) means cleared.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigPatch {
    /// Standing instructions. `Some(None)` clears them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<Option<String>>,
    /// Generation parameters. Replaces the whole block, because every field is already optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<GenerateParams>,
    /// Adapter hints. Merged field by field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hints: Option<Hints>,
    /// The sandbox, when the change only narrows it.
    ///
    /// Widening is refused here on purpose: more room for the whole session is an escalation, and an
    /// escalation is a gated decision about one command, not a field in a configuration call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxPolicy>,
    /// The tool allow-list. Empty means every registered tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// The exposure policy. Absent leaves it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_policy: Option<ToolPolicy>,
    /// The route this session takes. Absent means unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<Routing>,
}

// ---------------------------------------------------------------------------
// Listing processes
// ---------------------------------------------------------------------------

/// What a listing asks for.
///
/// A keyset cursor rather than an offset: sessions are created and retired constantly, and an
/// offset would skip or repeat rows exactly when a process ends — which is the moment a caller is
/// most likely to be paging.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessQuery {
    /// Only sessions in this phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<ProcessPhase>,
    /// How many to return. Zero asks for the default.
    #[serde(default)]
    pub limit: u32,
    /// Continue after this session id. Absent starts at the beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<SessionId>,
}

impl ProcessQuery {
    /// The page size this query asks for.
    #[must_use]
    pub fn page_size(&self) -> usize {
        match self.limit {
            0 => 50,
            limit => usize::try_from(limit.min(500)).unwrap_or(50),
        }
    }
}

/// One process, as a listing reports it.
///
/// Not the whole [`ProcessState`]: a listing is a cross-session view, and the environment a caller
/// passed to its command is a credential as often as it is a setting. What an operator needs to
/// identify a process is what it runs and where it is, so that is what a page carries — the full spec
/// stays behind `GET /v1/sessions/{id}`, which already answers only to the session's owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSummary {
    /// The session that owns it.
    pub session: SessionId,
    /// The command line.
    pub argv: Vec<String>,
    /// Where it runs, when the caller said.
    pub cwd: Option<String>,
    /// Where it is now.
    pub status: ProcessStatus,
}

/// One page of a listing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessPage {
    /// The sessions in this page, in id order.
    pub processes: Vec<ProcessSummary>,
    /// Where to continue. Absent means this was the last page.
    pub next: Option<SessionId>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Usage
    // ------------------------------------------------------------------

    #[test]
    fn usage_add_sums_tokens() {
        let mut total = Usage {
            input_tokens: 100,
            output_tokens: 50,
            micros_usd: Some(10),
        };
        total.add(&Usage {
            input_tokens: 20,
            output_tokens: 5,
            micros_usd: Some(3),
        });
        assert_eq!(total.input_tokens, 120);
        assert_eq!(total.output_tokens, 55);
        assert_eq!(total.micros_usd, Some(13));
    }

    #[test]
    fn an_unreported_cost_stays_unreported() {
        // The rule the whole ledger depends on: adding an unknown as zero is the same lie told more
        // quietly. If either side does not know the cost, the total does not know it either.
        let mut total = Usage {
            input_tokens: 100,
            output_tokens: 50,
            micros_usd: Some(10),
        };
        total.add(&Usage {
            input_tokens: 20,
            output_tokens: 5,
            micros_usd: None,
        });
        assert_eq!(total.micros_usd, None, "unknown + known = unknown");
    }

    #[test]
    fn an_unreported_cost_on_the_left_stays_unreported() {
        let mut total = Usage {
            input_tokens: 100,
            output_tokens: 50,
            micros_usd: None,
        };
        total.add(&Usage {
            input_tokens: 20,
            output_tokens: 5,
            micros_usd: Some(3),
        });
        assert_eq!(total.micros_usd, None);
    }

    #[test]
    fn total_tokens_is_input_plus_output() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            micros_usd: None,
        };
        assert_eq!(usage.total_tokens(), 150);
    }

    // ------------------------------------------------------------------
    // ErrorCode
    // ------------------------------------------------------------------

    #[test]
    fn retryable_is_the_four_transient_codes() {
        assert!(ErrorCode::Unavailable.is_retryable());
        assert!(ErrorCode::Timeout.is_retryable());
        assert!(ErrorCode::ProviderTruncated.is_retryable());
        assert!(ErrorCode::ProviderResponseUnusable.is_retryable());
    }

    #[test]
    fn a_permanent_failure_is_not_retryable() {
        assert!(!ErrorCode::Invalid.is_retryable());
        assert!(!ErrorCode::NotFound.is_retryable());
        assert!(!ErrorCode::Denied.is_retryable());
        assert!(!ErrorCode::Budget.is_retryable());
        assert!(!ErrorCode::Internal.is_retryable());
        assert!(!ErrorCode::UnknownModel.is_retryable());
        assert!(!ErrorCode::Conflict.is_retryable());
        // InvalidProviderResponse is deliberately not retryable: the payload is already unreadable,
        // and a retry hands the same wire format to the same decoder.
        assert!(!ErrorCode::InvalidProviderResponse.is_retryable());
    }

    // ------------------------------------------------------------------
    // Content and Part
    // ------------------------------------------------------------------

    #[test]
    fn content_text_concatenates_only_the_text_parts() {
        let content = Content::new([
            Part::text("hello "),
            Part::reasoning("thinking".to_owned(), String::new()),
            Part::text("world"),
        ]);
        assert_eq!(content.text(), "hello world");
    }

    #[test]
    fn an_empty_content_is_empty() {
        assert!(Content::default().is_empty());
        assert!(!Content::new([Part::text("x")]).is_empty());
    }

    // ------------------------------------------------------------------
    // MediaSource
    // ------------------------------------------------------------------

    #[test]
    fn an_image_source_refuses_a_non_image_media_type() {
        let result = ImageSource::data(vec![1, 2, 3], "text/plain");
        assert!(result.is_err());
    }

    #[test]
    fn an_audio_source_refuses_an_image_media_type() {
        let result = AudioSource::data(vec![1], "image/png");
        assert!(
            result.is_err(),
            "a part whose kind and source disagree is a part that lies"
        );
    }

    #[test]
    fn a_media_source_with_bytes_returns_them() {
        let source = ImageSource::data(vec![1, 2, 3], "image/png").unwrap();
        assert_eq!(source.bytes(), Some(&[1, 2, 3][..]));
        assert_eq!(source.media_type(), "image/png");
    }

    #[test]
    fn a_url_media_source_has_no_bytes() {
        let source = ImageSource::url("https://example.com/a.png", "image/png").unwrap();
        assert_eq!(source.bytes(), None);
    }

    #[test]
    fn appending_bytes_to_a_url_source_fails() {
        let mut target = ImageSource::url("https://example.com/a.png", "image/png").unwrap();
        let incoming = ImageSource::data(vec![1], "image/png").unwrap();
        assert!(!target.append(&incoming));
    }

    #[test]
    fn appending_different_media_types_fails() {
        let mut target = ImageSource::data(vec![1], "image/png").unwrap();
        let incoming = ImageSource::data(vec![2], "image/jpeg").unwrap();
        assert!(!target.append(&incoming));
    }

    // ------------------------------------------------------------------
    // SandboxPolicy::narrows_to
    // ------------------------------------------------------------------

    #[test]
    fn nothing_narrows_to_danger_full_access() {
        let read_only = SandboxPolicy::ReadOnly {
            network_access: false,
        };
        assert!(!read_only.narrows_to(&SandboxPolicy::DangerFullAccess));
        assert!(!SandboxPolicy::DangerFullAccess.narrows_to(&SandboxPolicy::DangerFullAccess));
    }

    #[test]
    fn workspace_write_narrows_to_read_only_without_network_widening() {
        let write = SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: vec!["/srv".to_owned()],
            network_access: false,
        };
        let read = SandboxPolicy::ReadOnly {
            network_access: false,
        };
        assert!(
            write.narrows_to(&read),
            "dropping all writes is a narrowing"
        );
    }

    #[test]
    fn workspace_write_narrowing_to_read_only_cannot_add_network() {
        let write = SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: false,
        };
        let read_with_net = SandboxPolicy::ReadOnly {
            network_access: true,
        };
        assert!(
            !write.narrows_to(&read_with_net),
            "narrowing in one axis may not widen another"
        );
    }

    #[test]
    fn read_only_does_not_narrow_to_workspace_write() {
        let read = SandboxPolicy::ReadOnly {
            network_access: false,
        };
        let write = SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: Vec::new(),
            network_access: false,
        };
        assert!(!read.narrows_to(&write), "gaining writes is a widening");
    }

    #[test]
    fn workspace_write_narrows_by_dropping_a_root() {
        let write = SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: vec!["/a".to_owned(), "/b".to_owned()],
            network_access: false,
        };
        let narrowed = SandboxPolicy::WorkspaceWrite {
            extra_writable_roots: vec!["/a".to_owned()],
            network_access: false,
        };
        assert!(write.narrows_to(&narrowed));
        assert!(!narrowed.narrows_to(&write), "adding a root is a widening");
    }

    #[test]
    fn external_sandbox_is_not_a_rung_on_the_ladder() {
        let read = SandboxPolicy::ReadOnly {
            network_access: false,
        };
        let external = SandboxPolicy::ExternalSandbox {
            network_access: false,
        };
        assert!(
            !read.narrows_to(&external),
            "entering an unenforced policy from an enforced one is a widening"
        );
        assert!(external.narrows_to(&read), "leaving it is not");
    }

    #[test]
    fn network_can_be_dropped_but_not_added() {
        let with_net = SandboxPolicy::ReadOnly {
            network_access: true,
        };
        let without_net = SandboxPolicy::ReadOnly {
            network_access: false,
        };
        assert!(with_net.narrows_to(&without_net));
        assert!(!without_net.narrows_to(&with_net));
    }

    // ------------------------------------------------------------------
    // SessionState
    // ------------------------------------------------------------------

    #[test]
    fn closed_states_are_the_terminal_ones() {
        assert!(SessionState::Closed.is_closed());
        assert!(!SessionState::Open.is_closed());
    }
}
