//! A tool that answers with whatever it was constructed with.

use async_trait::async_trait;
use nucleos::adapter::{Tool, ToolCtx, ToolOutput};
use nucleos::contract::ToolSpec;

/// A tool that answers with a fixed value.
#[derive(Debug, Clone)]
pub struct FakeTool {
    spec: ToolSpec,
    output: ToolOutput,
}

impl FakeTool {
    /// Answer with text.
    #[must_use]
    pub fn new(name: impl Into<String>, answer: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            spec: ToolSpec {
                description: format!("the {name} tool"),
                name,
                input_schema_json: "{\"type\":\"object\"}".to_owned(),
                isolation: Some(nucleos::contract::Isolation::InProcess),
            },
            output: ToolOutput::text(answer),
        }
    }

    /// Answer with a failure, which is content with `is_error` set.
    #[must_use]
    pub fn failing(mut self, answer: impl Into<String>) -> Self {
        self.output = ToolOutput::failed(answer);
        self
    }

    /// Report a cost for every call.
    #[must_use]
    pub fn reporting_usage(mut self, usage: nucleos::contract::Usage) -> Self {
        self.output = self.output.reporting_usage(usage);
        self
    }

    /// Replace the whole answer.
    #[must_use]
    pub fn answering(mut self, output: ToolOutput) -> Self {
        self.output = output;
        self
    }
}

#[async_trait]
impl Tool for FakeTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn invoke(&self, _args_json: &str, _cx: &ToolCtx<'_>) -> ToolOutput {
        self.output.clone()
    }
}
