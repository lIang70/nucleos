//! The kernel's pure types.
//!
//! No I/O, no clocks, no sockets. Everything here is data that can be compared, logged,
//! stored and replayed, which is what lets a driver be tested without a runtime and an
//! event log be replayed without a network.
//!
//! The types are deliberately narrower than any provider's vocabulary. A capability that
//! has no representation here is a capability the kernel does not claim to understand.

//!
//! # Layout
//!
//! One module per question. This file grew to three thousand lines and every addition landed at
//! the bottom of it, which is a file nobody reads. The split is by domain, not by size: a reader
//! looking for "what may a command touch" opens [`sandbox`], and one asking "what did the session
//! do" opens [`event`].
//!
//! | Module | Question |
//! | --- | --- |
//! | [`id`] | what names a session and a tool set |
//! | [`content`] | what a message is made of |
//! | [`session`] | where a session can be, and why it stopped |
//! | [`event`] | what the log records |
//! | [`authority`] | who is asking, and what may be asked for |
//! | [`routing`] | which model answers, and what each attempt cost |
//! | [`params`] | what a caller may ask a model for on one call |
//! | [`hints`] | what belongs to a session rather than to a call |
//! | [`sandbox`] | what a command may touch |
//! | [`process`] | a command that is the session |
//! | [`config`] | exposure, budgets, and the patch that changes them |
//! | [`outcome`] | what one model call returned, and what it cost |
//!
//! Everything is re-exported here, so `crate::contract::Usage` keeps working: the split is a
//! reading aid, not an interface change.

pub mod authority;
pub mod config;
pub mod content;
pub mod event;
pub mod hints;
pub mod id;
pub mod outcome;
pub mod params;
pub mod process;
pub mod routing;
pub mod sandbox;
pub mod session;

pub use authority::*;
pub use config::*;
pub use content::*;
pub use event::*;
pub use hints::*;
pub use id::*;
pub use outcome::{
    Billing, CacheTtl, Details, InputTokenUsage, Metadata, Modality, Money, OutputTokenUsage,
    ReasoningAccounting, Usage, WorkKind, fold_billing,
};
pub use params::*;
pub use process::*;
pub use routing::*;
pub use sandbox::*;
pub use session::*;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Usage
    // ------------------------------------------------------------------

    #[test]
    fn add_meter_sums_tokens_and_leaves_price_alone() {
        let mut total = Usage {
            input_tokens: 100,
            output_tokens: 50,
            ..Usage::costing(10)
        };
        total.add_meter(&Usage {
            input_tokens: 20,
            output_tokens: 5,
            ..Usage::costing(3)
        });
        assert_eq!(total.input_tokens, 120);
        assert_eq!(total.output_tokens, 55);
        // A price is not a meter: it is the first report's, not a sum of the two.
        assert_eq!(total.cost_micros_usd(), Some(10));
    }

    #[test]
    fn an_unreported_cost_stays_unreported_through_the_accumulator() {
        // The rule the whole ledger depends on. It no longer lives in `add` — `add_meter` does not
        // touch `Billing` at all — but in the session's first-report-defines-the-total rule: a session
        // whose first report carried no price has `billing: None`, and adding a later report does not
        // invent one. See `Session::add_usage`.
        let mut total = Usage::default();
        total.add_meter(&Usage {
            input_tokens: 20,
            output_tokens: 5,
            ..Usage::costing(3)
        });
        assert!(total.billing.is_none());
    }

    #[test]
    fn total_tokens_is_input_plus_output() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            ..Usage::default()
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
