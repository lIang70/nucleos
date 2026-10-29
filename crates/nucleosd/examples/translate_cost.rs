//! Throwaway measurement: what does the translation layer cost per event?
//!
//! Run with `cargo run --release --example translate_cost -p nucleosd`.
//!
//! A measurement, not a test: it asserts nothing, and its numbers depend on the machine. It
//! exists so that "is the translation layer expensive?" is answered with a number instead of an
//! opinion.

use nucleos::contract as k;
use nucleosd::translate;
use std::collections::BTreeMap;
use std::time::Instant;

fn text_delta() -> k::Event {
    k::Event {
        seq: 1234,
        session: k::SessionId::new("session-01HQ0Z9V3K4M5N6P7Q8R9S0T1U"),
        at_unix_millis: 1_700_000_000_123,
        schema_version: 1,
        kind: k::EventKind::TextDelta {
            part_index: 0,
            text: "a fragment of an answer, about forty bytes".to_owned(),
        },
        meta: BTreeMap::new(),
    }
}

fn tool_result() -> k::Event {
    let text = "x".repeat(256);
    k::Event {
        seq: 1235,
        session: k::SessionId::new("session-01HQ0Z9V3K4M5N6P7Q8R9S0T1U"),
        at_unix_millis: 1_700_000_000_456,
        schema_version: 1,
        kind: k::EventKind::ToolFinished {
            duration_millis: 12,
            result: k::ToolResult {
                call_id: "call-1".to_owned(),
                content: k::Content::new([k::Part::new(k::PartKind::Text(k::TextPart { text }))]),
                is_error: false,
            },
        },
        meta: BTreeMap::new(),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    const N: u32 = 500_000;

    let small = text_delta();
    let start = Instant::now();
    let mut sink = 0usize;
    for _ in 0..N {
        let wire = translate::event(&small);
        sink += wire.seq as usize;
    }
    let elapsed = start.elapsed();
    println!(
        "translate small event: {:>7.1} ns/op  ({N} ops in {:?})  [sink {sink}]",
        elapsed.as_nanos() as f64 / f64::from(N),
        elapsed
    );

    let big = tool_result();
    let start = Instant::now();
    for _ in 0..N {
        let wire = translate::event(&big);
        sink += wire.seq as usize;
    }
    let elapsed = start.elapsed();
    println!(
        "translate 256-byte tool result: {:>7.1} ns/op  ({N} ops in {:?})  [sink {sink}]",
        elapsed.as_nanos() as f64 / f64::from(N),
        elapsed
    );

    // The engine appends one event at a time today. That is the number worth comparing against
    // translation, because it is the one that would hurt with a durable store.
    use nucleos::adapter::Store;
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    let store = nucleos::adapter::MemoryStore::new();
    let session = k::SessionId::new("s1");
    runtime.block_on(async {
        let start = Instant::now();
        for i in 0..N {
            let mut event = small.clone();
            event.seq = u64::from(i);
            event.session = session.clone();
            store.append(&session, &[event]).await?;
        }
        let elapsed = start.elapsed();
        println!(
            "store append, one event at a time: {:>7.1} ns/op  ({N} ops in {:?})",
            elapsed.as_nanos() as f64 / f64::from(N),
            elapsed
        );
        Ok::<(), nucleos::Error>(())
    })?;

    let wire = translate::event(&small);
    let start = Instant::now();
    let mut bytes = 0usize;
    for _ in 0..N {
        let encoded = prost::Message::encode_to_vec(&wire);
        bytes += encoded.len();
    }
    let elapsed = start.elapsed();
    println!(
        "prost encode of that event: {:>7.1} ns/op  ({N} ops in {:?})  [{} bytes each]",
        elapsed.as_nanos() as f64 / f64::from(N),
        elapsed,
        bytes / N as usize
    );
    Ok(())
}
