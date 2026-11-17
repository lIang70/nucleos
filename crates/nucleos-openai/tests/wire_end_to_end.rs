//! The adapter over a real socket: what it posts, and what it does with what comes back.
//!
//! Every other test in this crate stops at a boundary the adapter does not cross — `prepare` before
//! the call, `Decoder::push` before the transport. That is how a defect survived the suite once
//! already: `prepare` compiled with the call's settings, `invoke` compiled again with the
//! deployment's, and the plan reported the options as honoured while the body did not carry them.
//! A test that never invokes cannot see that.
//!
//! The server here is a hand-rolled HTTP/1.1 responder on a real listener, not a mock library: the
//! adapter's transport is the thing under test, so replacing it with a fake would test the fake.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use nucleos::adapter::{ExtensionEntry, Model, ModelDelta, ModelRequest, decode_extensions};
use nucleos::contract::{Content, Message, Part, PartKind, Role, TextPart};
use nucleos_openai::extension::DECODER;
use nucleos_openai::{Config, OpenAi, OpenAiSettings};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// A server that records what it was sent
// ---------------------------------------------------------------------------

/// A listener serving one fixed SSE response, recording every request body.
struct Mock {
    url: String,
    /// Scheme and authority only, for a test that needs to name several endpoints on the same server.
    base: String,
    /// Bodies of the requests received, in order.
    bodies: Arc<Mutex<Vec<Value>>>,
    /// Request paths, in the same order: which endpoint the adapter chose is part of the contract.
    paths: Arc<Mutex<Vec<String>>>,
    /// How many connections were accepted, which is how a retry becomes visible.
    requests: Arc<AtomicUsize>,
}

impl Mock {
    /// Serve `frames` as an SSE body for `expected` connections.
    ///
    /// `expected` rather than "forever" so the test thread ends: a listener that outlives the test
    /// is a leaked thread that keeps a port for the rest of the run.
    fn start(frames: Vec<String>, expected: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let url = format!("{base}/v1/responses");
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::new(AtomicUsize::new(0));

        let recorded = Arc::clone(&bodies);
        let recorded_paths = Arc::clone(&paths);
        let counted = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming().take(expected) {
                let Ok(mut stream) = stream else { continue };
                counted.fetch_add(1, Ordering::SeqCst);
                if let Some((path, body)) = read_body(&mut stream) {
                    recorded_paths.lock().unwrap().push(path);
                    if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                        recorded.lock().unwrap().push(value);
                    }
                }
                respond(&mut stream, &frames);
            }
        });

        Self {
            url,
            base,
            bodies,
            paths,
            requests,
        }
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    fn paths(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }

    fn base(&self) -> &str {
        &self.base
    }
}

impl Mock {
    /// Serve raw bytes in `chunks`, chunked-encoded: for an endpoint that answers with audio rather
    /// than with frames, and where the point is that many chunks become one clip.
    fn start_raw(chunks: Vec<Vec<u8>>, content_type: &str, expected: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let url = format!("{base}/v1/responses");
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::new(AtomicUsize::new(0));

        let recorded = Arc::clone(&bodies);
        let recorded_paths = Arc::clone(&paths);
        let counted = Arc::clone(&requests);
        let content_type = content_type.to_owned();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(expected) {
                let Ok(mut stream) = stream else { continue };
                counted.fetch_add(1, Ordering::SeqCst);
                let Some((path, request)) = read_body(&mut stream) else {
                    continue;
                };
                recorded_paths.lock().unwrap().push(path);
                if let Ok(value) = serde_json::from_slice::<Value>(&request) {
                    recorded.lock().unwrap().push(value);
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(head.as_bytes());
                for chunk in &chunks {
                    let _ = write!(stream, "{:x}\r\n", chunk.len());
                    let _ = stream.write_all(chunk);
                    let _ = stream.write_all(b"\r\n");
                    let _ = stream.flush();
                }
                let _ = stream.write_all(b"0\r\n\r\n");
                let _ = stream.flush();
            }
        });

        Self {
            url,
            base,
            bodies,
            paths,
            requests,
        }
    }
}

/// Read the request line, headers, then exactly `Content-Length` bytes.
fn read_body(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut length = 0usize;
    let mut path = String::new();
    let mut first = true;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if first {
            // `POST /v1/images/generations HTTP/1.1`
            path = line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned();
            first = false;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = rest.trim().parse().ok()?;
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some((path, body))
}

/// Answer 200 with the frames as a stream, then close. `Connection: close` is what ends the body: a
/// chunked encoding would be more faithful to a real endpoint and more code for the same bytes.
fn respond(stream: &mut TcpStream, frames: &[String]) {
    let mut body = String::new();
    for frame in frames {
        body.push_str("data: ");
        body.push_str(frame);
        body.push_str("\n\n");
    }
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// One SSE frame carrying a JSON payload.
fn frame(value: Value) -> String {
    value.to_string()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![Message {
            role: Role::User,
            content: Content::new([Part::new(PartKind::Text(TextPart {
                text: "hi".to_owned(),
            }))]),
        }]
        .into(),
        ..ModelRequest::default()
    }
}

fn adapter(url: &str, settings: OpenAiSettings) -> OpenAi {
    OpenAi::new(
        Config {
            url: url.to_owned(),
            api_key: Some("test-key".to_owned()),
            model: "gpt-test".to_owned(),
            timeout: Some(std::time::Duration::from_secs(5)),
            images_url: None,
            speech_url: None,
        },
        settings,
    )
    .unwrap()
}

fn generation(fields: Value) -> nucleos::adapter::ExtensionSet {
    let mut registry = std::collections::BTreeMap::new();
    registry.insert(
        "openai/generation".to_owned(),
        DECODER as nucleos::adapter::ExtensionDecoder,
    );
    decode_extensions(
        &[ExtensionEntry {
            provider: "openai".to_owned(),
            id: "generation".to_owned(),
            fields,
        }],
        &registry,
        "inference.extensions",
    )
    .unwrap()
}

/// A minimal well-formed answer: one text fragment and a stop.
fn text_answer() -> Vec<String> {
    vec![
        frame(serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1",
            "output_index": 0,
            "content_index": 0,
            "delta": "ok",
            "sequence_number": 1
        })),
        frame(serde_json::json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "status": "completed", "output": []},
            "sequence_number": 2
        })),
    ]
}

/// Read a stream to its end, returning the deltas and the first error.
async fn drain(mut stream: nucleos::adapter::ModelStream) -> (Vec<ModelDelta>, Option<String>) {
    use tokio_stream::StreamExt;
    let mut deltas = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(delta) => deltas.push(delta),
            Err(error) => {
                return (
                    deltas,
                    Some(format!("{:?}: {}", error.code(), error.reason())),
                );
            }
        }
    }
    (deltas, None)
}

// ---------------------------------------------------------------------------
// What the adapter posts
// ---------------------------------------------------------------------------

/// The body that leaves the process is the body that was checked.
///
/// This is the assertion the D1 test could not make: it compared `prepare`'s artifact against
/// `prepare`'s plan, which two functions can agree on while `invoke` sends something else.
#[tokio::test]
async fn the_posted_body_carries_the_per_call_options_the_plan_claimed() {
    let mock = Mock::start(text_answer(), 1);
    let adapter = adapter(&mock.url, OpenAiSettings::default());

    let mut options = request();
    options.extensions = generation(serde_json::json!({
        "prompt_cache_key": "call-key",
        "safety_identifier": "call-user",
        "include": ["reasoning_encrypted_content"],
    }));
    let prepared = adapter.prepare(options).unwrap();
    let (deltas, error) = drain(adapter.invoke(prepared).await.unwrap()).await;
    assert!(error.is_none(), "{error:?}");
    assert!(matches!(deltas.last(), Some(ModelDelta::Finished { .. })));

    let body = &mock.bodies()[0];
    assert_eq!(
        body.get("prompt_cache_key").and_then(Value::as_str),
        Some("call-key"),
        "the per-call cache key did not reach the wire: {body}"
    );
    assert_eq!(
        body.get("safety_identifier").and_then(Value::as_str),
        Some("call-user")
    );
    assert!(
        body.get("include")
            .and_then(Value::as_array)
            .is_some_and(|list| list
                .iter()
                .any(|v| v.as_str() == Some("reasoning.encrypted_content"))),
        "the per-call include list did not reach the wire: {body}"
    );
    assert_eq!(mock.requests(), 1);
}

/// A per-call option beats the value derived from the session, not the other way round.
///
/// Nothing tested this: one test covered the derived default, another covered the extension with no
/// correlation present, so an implementation that applied the derived value *after* the extension
/// would have passed both while silently discarding what the caller asked for.
#[tokio::test]
async fn a_per_call_option_beats_the_value_derived_from_the_session() {
    let mock = Mock::start(text_answer(), 1);
    let adapter = adapter(&mock.url, OpenAiSettings::default());

    let mut options = request();
    options.correlation = Some(Arc::new(nucleos::adapter::Correlation {
        session: "s1".to_owned(),
        turn: 2,
        tenant: "acme".to_owned(),
        subject: "alice".to_owned(),
    }));
    options.extensions = generation(serde_json::json!({
        "prompt_cache_key": "call-key",
        "safety_identifier": "call-user",
    }));
    let prepared = adapter.prepare(options).unwrap();
    let (_, error) = drain(adapter.invoke(prepared).await.unwrap()).await;
    assert!(error.is_none(), "{error:?}");

    let body = &mock.bodies()[0];
    assert_eq!(
        body.get("prompt_cache_key").and_then(Value::as_str),
        Some("call-key"),
        "a derived value overwrote the caller's: {body}"
    );
    assert_eq!(
        body.get("safety_identifier").and_then(Value::as_str),
        Some("call-user")
    );
}

// ---------------------------------------------------------------------------
// What the adapter does with the answer
// ---------------------------------------------------------------------------

/// A tool call at a non-zero output index keeps that index as its position in the message.
///
/// The decoder fixture uses index 0, so an adapter that hard-coded zero would satisfy it. This drives
/// the whole path and asserts the number that ordering depends on.
#[tokio::test]
async fn a_tool_call_at_a_nonzero_index_keeps_its_position() {
    let mock = Mock::start(
        vec![
            frame(serde_json::json!({
                "type": "response.output_text.delta",
                "item_id": "msg_1",
                "output_index": 0,
                "content_index": 0,
                "delta": "before",
                "sequence_number": 1
            })),
            frame(serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {
                    "id": "fc_1",
                    "type": "function_call",
                    "status": "in_progress",
                    "arguments": "",
                    "call_id": "call_1",
                    "name": "read_file"
                },
                "sequence_number": 2
            })),
            frame(serde_json::json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "status": "completed", "output": []},
                "sequence_number": 3
            })),
        ],
        1,
    );
    let adapter = adapter(&mock.url, OpenAiSettings::default());
    let prepared = adapter.prepare(request()).unwrap();
    let (deltas, error) = drain(adapter.invoke(prepared).await.unwrap()).await;
    assert!(error.is_none(), "{error:?}");

    let started = deltas
        .iter()
        .find_map(|delta| match delta {
            ModelDelta::ToolCallStarted {
                index,
                part_index,
                name,
                ..
            } => Some((*index, *part_index, name.clone())),
            _ => None,
        })
        .expect("the call was announced");
    assert_eq!(started.0, 1, "the tool-call ordinal is the output index");
    assert_eq!(
        started.1, 1,
        "the call's part position must be its output index, not a constant"
    );
}

/// A frame the decoder cannot read fails the call, and is not retried by the adapter.
#[tokio::test]
async fn a_malformed_frame_fails_the_call_without_a_second_request() {
    let mock = Mock::start(
        vec![
            frame(serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": "half",
                "sequence_number": 1
            })),
            // Truncated JSON: what a proxy cutting a frame leaves behind.
            "{\"type\":\"response.output_text.delta\",\"delta\":".to_owned(),
        ],
        1,
    );
    let adapter = adapter(&mock.url, OpenAiSettings::default());
    let prepared = adapter.prepare(request()).unwrap();
    let (deltas, error) = drain(adapter.invoke(prepared).await.unwrap()).await;

    // What had arrived first is still delivered; the turn's failure is the error.
    assert!(deltas.iter().any(|delta| matches!(
        delta,
        ModelDelta::PartAppend { part, .. }
            if matches!(&part.kind, PartKind::Text(text) if text.text == "half")
    )));
    let error = error.expect("a malformed frame must surface as an error");
    assert!(
        error.contains("InvalidProviderResponse"),
        "a malformed frame was not classified as an unreadable answer: {error}"
    );
    assert_eq!(
        mock.requests(),
        1,
        "the adapter retried a payload the decoder had already rejected"
    );
}

// ---------------------------------------------------------------------------
// The image surface, over a socket
// ---------------------------------------------------------------------------

/// An image request goes to the image endpoint, with a prompt, and its preview replaces its final
/// image at one part index.
#[tokio::test]
async fn an_image_call_uses_the_image_endpoint_and_reports_one_part() {
    let mock = Mock::start(
        vec![
            // Captured from the image generation stream: a preview, then the finished image.
            frame(serde_json::json!({
                "type": "image_generation.partial_image",
                "b64_json": "cHJldmlldw==",
                "partial_image_index": 0,
                "size": "1024x1024",
                "quality": "low",
                "background": "auto",
                "output_format": "png"
            })),
            frame(serde_json::json!({
                "type": "image_generation.completed",
                "b64_json": "ZmluYWw=",
                "created_at": 0,
                "size": "1024x1024",
                "quality": "low",
                "background": "auto",
                "output_format": "png",
                "usage": {
                    "input_tokens": 2,
                    "output_tokens": 3,
                    "total_tokens": 5,
                    "input_tokens_details": {"text_tokens": 2, "image_tokens": 0}
                }
            })),
        ],
        1,
    );

    let mut settings = OpenAiSettings::default();
    settings.capability.outputs = vec![nucleos::contract::PartKindName::Image];
    let config = Config {
        // Deliberately unreachable: if the dispatch went to the responses endpoint the call would
        // fail here and the mock would never see a request.
        url: "http://127.0.0.1:1/v1/responses".to_owned(),
        api_key: Some("test-key".to_owned()),
        model: "gpt-image-1".to_owned(),
        timeout: Some(std::time::Duration::from_secs(5)),
        images_url: Some(format!("{}/v1/images/generations", mock.base())),
        speech_url: None,
    };
    let adapter = OpenAi::new(config, settings).unwrap();

    let mut request = request();
    request.output = nucleos::contract::Output::Image;
    request.extensions =
        nucleos::adapter::ExtensionSet::new(vec![Box::new(nucleos_openai::ImageOptions {
            output_format: Some("png".to_owned()),
            partial_images: Some(1),
            ..Default::default()
        })]);

    let prepared = adapter.prepare(request).unwrap();
    let (deltas, error) = drain(adapter.invoke(prepared).await.unwrap()).await;
    assert!(error.is_none(), "{error:?}");

    assert_eq!(
        mock.paths(),
        vec!["/v1/images/generations".to_owned()],
        "the call did not go to the image endpoint"
    );
    let body = &mock.bodies()[0];
    assert_eq!(
        body["prompt"], "hi",
        "the prompt did not reach the wire: {body}"
    );
    assert_eq!(body["partial_images"], 1);

    // Both frames are the same part: a preview, then what it became.
    let parts: Vec<&ModelDelta> = deltas
        .iter()
        .filter(|delta| matches!(delta, ModelDelta::PartReplace { part_index: 0, .. }))
        .collect();
    assert_eq!(
        parts.len(),
        2,
        "a preview and a final image are two frames: {deltas:?}"
    );
    assert!(matches!(&deltas.last(), Some(ModelDelta::Finished { .. })));
}

// ---------------------------------------------------------------------------
// The speech surface, over a socket
// ---------------------------------------------------------------------------

/// Speech answers with audio bytes, and every chunk appends to one part.
///
/// This is the merge rule the contract defines for media, exercised against a real body: if the
/// chunks did not share a media type they would not be an append, and the part would carry only the
/// last fragment of the clip.
#[tokio::test]
async fn a_speech_call_streams_one_clip_from_many_chunks() {
    // Two chunks, as a chunked audio response delivers them.
    let mock = Mock::start_raw(vec![b"RIFF".to_vec(), b"payload".to_vec()], "audio/mpeg", 1);

    let mut settings = OpenAiSettings::default();
    settings.capability.outputs = vec![nucleos::contract::PartKindName::Audio];
    let adapter = OpenAi::new(
        Config {
            url: "http://127.0.0.1:1/v1/responses".to_owned(),
            api_key: Some("test-key".to_owned()),
            model: "gpt-4o-mini-tts".to_owned(),
            timeout: Some(std::time::Duration::from_secs(5)),
            images_url: None,
            speech_url: Some(format!("{}/v1/audio/speech", mock.base())),
        },
        settings,
    )
    .unwrap();

    let mut request = request();
    request.output = nucleos::contract::Output::Audio;
    request.extensions =
        nucleos::adapter::ExtensionSet::new(vec![Box::new(nucleos_openai::SpeechOptions {
            voice: Some("nova".to_owned()),
            format: Some("mp3".to_owned()),
            speed: Some(1.25),
            ..Default::default()
        })]);

    let prepared = adapter.prepare(request).unwrap();
    let (deltas, error) = drain(adapter.invoke(prepared).await.unwrap()).await;
    assert!(error.is_none(), "{error:?}");

    assert_eq!(
        mock.paths(),
        vec!["/v1/audio/speech".to_owned()],
        "the call did not go to the speech endpoint"
    );
    let body = &mock.bodies()[0];
    assert_eq!(
        body["input"], "hi",
        "the text did not reach the wire: {body}"
    );
    assert_eq!(body["voice"], "nova");
    assert_eq!(body["response_format"], "mp3");
    assert_eq!(body["speed"], 1.25);
    assert_eq!(body["stream_format"], "audio");

    // Every chunk is the same part, and the media types agree — which is what makes them an append
    // rather than a sequence of replacements. Combining them must give the whole clip back.
    let mut clip = Vec::new();
    let mut chunks = 0;
    for delta in &deltas {
        if let ModelDelta::PartAppend {
            part_index: 0,
            part,
        } = delta
            && let PartKind::Audio(audio) = &part.kind
        {
            assert_eq!(audio.source.media_type(), "audio/mpeg");
            clip.extend_from_slice(audio.source.bytes().unwrap_or_default());
            chunks += 1;
        }
    }
    assert!(chunks >= 1, "the clip produced no parts: {deltas:?}");
    assert_eq!(
        clip, b"RIFFpayload",
        "the parts do not reassemble into the clip: {deltas:?}"
    );
    assert!(matches!(&deltas.last(), Some(ModelDelta::Finished { .. })));
}

/// A speech call with no text has nothing to read.
#[tokio::test]
async fn a_speech_call_without_text_is_refused() {
    let adapter = OpenAi::new(
        Config {
            url: "http://127.0.0.1:1/v1/responses".to_owned(),
            api_key: None,
            model: "gpt-4o-mini-tts".to_owned(),
            timeout: None,
            images_url: None,
            speech_url: Some("http://127.0.0.1:1/v1/audio/speech".to_owned()),
        },
        OpenAiSettings::default(),
    )
    .unwrap();

    let mut request = request();
    request.output = nucleos::contract::Output::Audio;
    request.messages = vec![Message {
        role: Role::User,
        content: Content::new([Part::text("  ".to_owned())]),
    }]
    .into();
    let error = adapter.prepare(request).unwrap_err();
    assert_eq!(error.code(), nucleos::contract::ErrorCode::Invalid);
    assert!(error.reason().contains("needs text"), "{error}");
}
