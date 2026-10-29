//! Throwaway measurement: what does the wire cost per event?
//!
//! Run with `cargo run --release --example wire_cost -p nucleosd`.
//!
//! A measurement, not a test: it asserts nothing, and its numbers depend on the machine. It
//! exists so that "is the JSON wire expensive?" is answered with a number instead of an opinion.

use nucleos::contract as k;
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
            usage: None,
        },
        meta: BTreeMap::new(),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    use nucleos::adapter::Store;
    const N: u32 = 200_000;

    let small = text_delta();
    let start = Instant::now();
    let mut bytes = 0usize;
    for _ in 0..N {
        bytes += serde_json::to_vec(&small)?.len();
    }
    let elapsed = start.elapsed();
    println!(
        "serialise a text delta:      {:>7.1} ns/op  ({} bytes)  [{N} ops in {elapsed:?}]",
        elapsed.as_nanos() as f64 / f64::from(N),
        bytes / N as usize
    );

    let big = tool_result();
    let start = Instant::now();
    let mut bytes = 0usize;
    for _ in 0..N {
        bytes += serde_json::to_vec(&big)?.len();
    }
    let elapsed = start.elapsed();
    println!(
        "serialise a 256-byte result: {:>7.1} ns/op  ({} bytes)  [{N} ops in {elapsed:?}]",
        elapsed.as_nanos() as f64 / f64::from(N),
        bytes / N as usize
    );

    // A driver appends one event at a time today. That is the number to keep an eye on, because
    // a durable store turns it into one transaction per event.
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
            "store append, one at a time: {:>7.1} ns/op  [{N} ops in {elapsed:?}]",
            elapsed.as_nanos() as f64 / f64::from(N)
        );
        Ok::<(), nucleos::Error>(())
    })?;
    Ok(())
}
