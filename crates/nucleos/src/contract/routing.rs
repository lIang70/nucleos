//! A route is a chain plus a retry count; the ledger is what walking it cost.

use super::{Degraded, ErrorCode, Usage, fold_billing};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
            self.total.add_meter(&attempt.usage);
            // The price is folded here, not in `add_meter`: a total cost is what a ledger is for,
            // and `add_meter` deliberately knows nothing about prices.
            fold_billing(&mut self.total.billing, &attempt.usage.billing);
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
                Some(total) => {
                    total.add_meter(&attempt.usage);
                    fold_billing(&mut total.billing, &attempt.usage.billing);
                }
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
