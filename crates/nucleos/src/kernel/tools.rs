//! Tool calls, and the gate in front of them.
//!
//! Policy is asked about a whole batch before any of it runs, and a verdict that has already been
//! answered is not asked again — `invoke_approved` is the one door that skips the gate.

use super::{Kernel, session};
use crate::adapter::Tool;
use crate::adapter::ToolOutput;
use crate::contract::{Content, Message, Part, PartKind};
use crate::contract::{EventKind, ToolResultPart};
use crate::error::{Error, Result};
use std::sync::Arc;

impl Kernel {
    /// Ask policy about one call.
    pub async fn check(
        &self,
        session: &session::Session,
        call: &crate::contract::ToolCall,
    ) -> crate::adapter::Verdict {
        self.policy()
            .check(crate::adapter::PolicyRequest {
                principal: &session.principal,
                session: Some(&session.id),
                subject: crate::adapter::Gated::Tool { call },
            })
            .await
    }
    /// Run one tool call.
    ///
    /// Policy is consulted here as well as in [`Kernel::check`]: a kernel that executed an ungated
    /// tool because a caller forgot to ask first would not be a kernel.
    pub async fn invoke_tool(
        &self,
        session: &mut session::Session,
        call: &crate::contract::ToolCall,
    ) -> Result<ToolOutput> {
        let mut outputs = self
            .invoke_tools(session, std::slice::from_ref(call))
            .await?;
        outputs
            .pop()
            .ok_or_else(|| Error::internal("a single call produced no result"))
    }
    /// Run a batch of calls.
    ///
    /// Policy is asked about every call before any runs, so a batch that needs a human parks with
    /// nothing executed. Then the allowed ones run concurrently and their results are folded back
    /// in order — state writes serial, tool execution parallel, which are different axes.
    pub async fn invoke_tools(
        &self,
        session: &mut session::Session,
        calls: &[crate::contract::ToolCall],
    ) -> Result<Vec<ToolOutput>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }

        let mut verdicts = Vec::with_capacity(calls.len());
        for call in calls {
            match self.check(session, call).await {
                crate::adapter::Verdict::AskHuman { question, options } => {
                    self.park(
                        session,
                        crate::contract::PendingCall {
                            call: call.clone(),
                            options,
                            question,
                        },
                    )
                    .await?;
                    return Err(Error::unavailable(
                        "policy asked a human; the batch is parked with nothing executed",
                    ));
                }
                crate::adapter::Verdict::Allow => verdicts.push(None),
                crate::adapter::Verdict::Deny { reason, .. } => verdicts.push(Some(reason)),
            }
        }
        self.execute_calls(session, calls, verdicts).await
    }
    /// Run calls that are already allowed.
    ///
    /// A human's verdict *is* the policy answer; asking again would ask the same question
    /// forever. This is the only door that skips the gate, and it is not reachable from the wire.
    pub async fn invoke_approved(
        &self,
        session: &mut session::Session,
        calls: &[crate::contract::ToolCall],
    ) -> Result<Vec<ToolOutput>> {
        let verdicts = vec![None; calls.len()];
        self.execute_calls(session, calls, verdicts).await
    }

    async fn execute_calls(
        &self,
        session: &mut session::Session,
        calls: &[crate::contract::ToolCall],
        verdicts: Vec<Option<String>>,
    ) -> Result<Vec<ToolOutput>> {
        let deadline = self.deadline(session);
        let session_id = session.id.clone();
        let started = self.now();
        let futures = calls.iter().zip(verdicts).map(|(call, denial)| {
            let tool = self.tool(&call.name);
            let args = call.arguments_json.clone();
            let call_id = call.id.clone();
            let session_id = session_id.clone();
            async move {
                let ctx = crate::adapter::ToolCtx {
                    session: &session_id,
                    call_id: &call_id,
                    deadline_unix_millis: deadline,
                };
                match denial {
                    None => match tool {
                        // A model that produced broken JSON gets told so, and the turn
                        // continues. Executing it would mean guessing at the arguments.
                        Some(tool) => match serde_json::from_str::<serde_json::Value>(&args) {
                            Ok(_) => tool.invoke(&args, &ctx).await,
                            Err(error) => ToolOutput::failed(format!(
                                "arguments were not valid JSON: {error}"
                            )),
                        },
                        None => ToolOutput::failed(format!("no tool named {:?}", ctx.call_id)),
                    },
                    Some(reason) => ToolOutput::failed(reason),
                }
            }
        });
        let outputs = futures_util::future::join_all(futures).await;
        let duration = self.now().saturating_sub(started);

        for (call, output) in calls.iter().zip(outputs.iter()) {
            self.record_call(session, call, output.clone(), duration)
                .await?;
        }
        self.write_checkpoint(session).await?;
        Ok(outputs)
    }
    /// Record a refusal as the call's result.
    ///
    /// A refusal is content, not a failure of the session: the model reads why and decides what
    /// to do next.
    pub async fn record_refusal(
        &self,
        session: &mut session::Session,
        call: &crate::contract::ToolCall,
        reason: impl Into<String>,
    ) -> Result<()> {
        self.record_call(session, call, ToolOutput::failed(reason), 0)
            .await
    }

    /// Record a call's outcome: the event, the spend, and the message that enters history.
    async fn record_call(
        &self,
        session: &mut session::Session,
        call: &crate::contract::ToolCall,
        output: ToolOutput,
        duration_millis: u64,
    ) -> Result<()> {
        // A tool that reported a cost is charged for it; one that said nothing is not assumed
        // free. The ledger's rule is the same one model usage follows.
        if let Some(usage) = &output.usage {
            session.usage.add(usage);
            session.credit_tokens = session.credit_tokens.saturating_sub(usage.total_tokens());
        }
        let result = crate::contract::ToolResult {
            call_id: call.id.clone(),
            content: output.content,
            is_error: output.is_error,
        };
        self.emit(
            session,
            EventKind::ToolFinished {
                duration_millis,
                result: result.clone(),
                usage: output.usage,
            },
        )
        .await?;
        session.messages.push(Message {
            role: crate::contract::Role::Tool,
            content: Content::new([Part::new(PartKind::ToolResult(ToolResultPart { result }))]),
        });
        Ok(())
    }
    /// One tool by name.
    #[must_use]
    pub fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// The declarations this session may use, in name order so a prompt is reproducible.
    #[must_use]
    pub fn tool_specs(&self, session: &session::Session) -> Vec<crate::contract::ToolSpec> {
        self.tools
            .iter()
            .filter(|(name, _)| session.tools.is_empty() || session.tools.contains(name))
            .map(|(_, tool)| tool.spec())
            .collect()
    }
}
