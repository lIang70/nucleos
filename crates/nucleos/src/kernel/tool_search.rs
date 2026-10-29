//! Tool discovery: one built-in tool that ranks the catalog and asks the kernel to widen exposure.
//!
//! A prompt cannot carry every tool's schema — each one costs 100 to 300 tokens, and a model that
//! is choosing between sixty tools chooses worse than one choosing between eight. So the catalog
//! is searchable, and what the model finds becomes visible from the next turn on.
//!
//! The tool does not widen anything itself: it has no handle on the session. It *asks*, in
//! [`ToolOutput::expose`], and the kernel answers against the session's ceiling and records a
//! `tools_exposed` event. Exposure is not permission — execution still goes through the gate.

use super::bm25::{self, Index};
use crate::adapter::{Tool, ToolCtx, ToolOutput};
use crate::contract::{Content, Part, PartKind, TextPart, ToolSpec};
use async_trait::async_trait;
use std::collections::BTreeMap;

/// The name the model calls. Fixed, because the prompt's description of discovery refers to it.
pub(crate) const NAME: &str = "tool_search";

/// How many hits a call gets when it does not say.
const DEFAULT_LIMIT: usize = 8;

/// A searchable view of the tool catalog.
pub(crate) struct Search {
    index: Index,
    /// One-line descriptions, so a hit is readable without the full schema.
    summaries: BTreeMap<String, String>,
}

impl Search {
    /// Build the catalog from the registered specifications.
    ///
    /// `tool_search` itself is not in it: a search tool that returns itself spends a result on
    /// something the model already has.
    pub(crate) fn new(specs: &[ToolSpec]) -> Self {
        let indexed: Vec<ToolSpec> = specs
            .iter()
            .filter(|spec| spec.name != NAME)
            .cloned()
            .collect();
        let summaries = indexed
            .iter()
            .map(|spec| (spec.name.clone(), summary(&spec.description)))
            .collect();
        Self {
            index: Index::new(
                indexed
                    .iter()
                    .map(|spec| bm25::Doc {
                        id: spec.name.clone(),
                        name: spec.name.clone(),
                        text: spec.description.clone(),
                    })
                    .collect(),
            ),
            summaries,
        }
    }

}

#[async_trait]
impl Tool for Search {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME.to_owned(),
            description: "Search the tool catalog for tools that can do something, and make the \
                          best matches visible to you from the next turn on. Use this when you \
                          need a capability you cannot see."
                .to_owned(),
            input_schema_json: r#"{
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "What you want to do, in words."},
                    "limit": {"type": "integer", "minimum": 1, "description": "How many matches."}
                },
                "required": ["query"]
            }"#
            .to_owned(),
        }
    }

    async fn invoke(&self, args_json: &str, _cx: &ToolCtx<'_>) -> ToolOutput {
        let (query, limit) = match parse(args_json) {
            Ok(parsed) => parsed,
            Err(message) => return ToolOutput::failed(message),
        };

        // Catalog terms, not raw ones: a bare "a" would drag in every definition.
        let terms = bm25::catalog_terms(&query);
        if terms.is_empty() {
            return ToolOutput::failed("query had no searchable words");
        }

        let hits = self.index.search(&terms.join(" "), limit);
        if hits.is_empty() {
            return ToolOutput::ok(Content::new([Part::new(PartKind::Text(TextPart {
                text: format!("No tool matches {query:?}."),
            }))]));
        }

        let mut text = String::new();
        let mut names = Vec::with_capacity(hits.len());
        for hit in &hits {
            let Some(summary) = self.summaries.get(&hit.id) else {
                continue;
            };
            text.push_str("- ");
            text.push_str(&hit.id);
            text.push_str(": ");
            text.push_str(summary);
            text.push('\n');
            names.push(hit.id.clone());
        }

        ToolOutput::ok(Content::new([Part::new(PartKind::Text(TextPart {
            text: text.trim_end().to_owned(),
        }))]))
        .exposing(names)
    }
}

fn parse(args_json: &str) -> Result<(String, usize), String> {
    let value: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|error| format!("arguments were not valid JSON: {error}"))?;
    let query = value
        .get("query")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "arguments need a string \"query\"".to_owned())?
        .to_owned();
    let limit = match value.get("limit").and_then(serde_json::Value::as_u64) {
        // A limit of zero is a caller error, not a request for nothing.
        Some(0) | None => DEFAULT_LIMIT,
        Some(limit) => usize::try_from(limit).unwrap_or(DEFAULT_LIMIT),
    };
    Ok((query, limit))
}

/// The first sentence, so a hit reads as a line rather than a paragraph.
fn summary(description: &str) -> String {
    let first = description
        .split_inclusive(['.', '\n'])
        .next()
        .unwrap_or(description);
    let trimmed = first.trim();
    if trimmed.is_empty() {
        "(no description)".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, description: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: description.to_owned(),
            input_schema_json: "{}".to_owned(),
        }
    }

    async fn search(catalog: &Search, args: &str) -> ToolOutput {
        let session = crate::contract::SessionId::new("s");
        let cx = ToolCtx {
            session: &session,
            call_id: "call-1",
            deadline_unix_millis: u64::MAX,
        };
        catalog.invoke(args, &cx).await
    }

    fn text(output: &ToolOutput) -> String {
        output
            .content
            .parts
            .iter()
            .filter_map(|part| match &part.kind {
                PartKind::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_query_returns_readable_hits_and_asks_for_them() {
        let catalog = Search::new(&[
            spec("read_file", "Read a file. Fails if it is missing."),
            spec("send_email", "Send an email."),
        ]);
        let output = search(&catalog, r#"{"query":"read a file"}"#).await;

        assert!(!output.is_error);
        assert_eq!(output.expose, ["read_file"]);
        let text = text(&output);
        assert!(text.contains("read_file"), "{text}");
        // The summary is one line, not the whole description: the schema is what is expensive.
        assert!(!text.contains("Fails if it is missing"), "{text}");
    }

    #[tokio::test]
    async fn the_search_tool_is_not_in_its_own_catalog() {
        let catalog = Search::new(&[spec("read_file", "Read a file."), spec(NAME, "Search.")]);
        let output = search(&catalog, r#"{"query":"search the catalog"}"#).await;

        // It spends a result on something the model already has, every time it is used.
        assert!(output.expose.is_empty(), "{output:?}");
    }

    #[tokio::test]
    async fn a_query_with_nothing_to_match_is_a_failure_the_model_can_read() {
        let catalog = Search::new(&[spec("read_file", "Read a file.")]);
        let output = search(&catalog, r#"{"query":"a"}"#).await;

        // "a" is dropped as a single character of ASCII, so the query is empty rather than
        // matching every tool.
        assert!(output.is_error);
        assert!(text(&output).contains("no searchable words"), "{output:?}");
    }

    #[tokio::test]
    async fn a_query_that_matches_nothing_returns_content_not_an_error() {
        let catalog = Search::new(&[spec("read_file", "Read a file.")]);
        let output = search(&catalog, r#"{"query":"quantum"}"#).await;

        assert!(!output.is_error, "not finding a tool is not a failed call");
        assert!(output.expose.is_empty());
        assert!(text(&output).contains("No tool matches"));
    }

    #[tokio::test]
    async fn a_missing_query_is_a_failure() {
        let catalog = Search::new(&[spec("read_file", "Read a file.")]);
        assert!(search(&catalog, "{}").await.is_error);
        assert!(search(&catalog, "not json").await.is_error);
    }

    #[tokio::test]
    async fn the_limit_bounds_the_hits() {
        let catalog = Search::new(&[
            spec("read_file", "Read a file."),
            spec("write_file", "Write a file."),
            spec("list_files", "List files."),
        ]);
        let output = search(&catalog, r#"{"query":"file","limit":2}"#).await;
        assert_eq!(output.expose.len(), 2);
    }
}
