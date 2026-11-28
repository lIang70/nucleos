use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A position in one session's log.
///
/// A distinct type rather than a `u64`, because this value is also the resume cursor and the
/// comparison a client uses to ask for what it has not seen. Mixing it with a row number, an
/// index into a vector, or a count of facts is the usual way to make a cursor silently skip an
/// entry. Positions are dense and start at zero.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(transparent)]
pub struct Seq(u64);

impl Seq {
    /// The first position of a new session.
    pub const FIRST: Self = Self(0);

    /// Wrap a raw position, for a store that is decoding its own index.
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The next position.
    ///
    /// Saturating rather than wrapping: a log that reached `u64::MAX` entries is a broken store,
    /// and handing back position zero would look like an empty session instead of the failure it
    /// is. [`Self::is_exhausted`] makes that state checkable.
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// The raw number.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    /// Whether no further position can be issued.
    #[must_use]
    pub fn is_exhausted(self) -> bool {
        self.0 == u64::MAX
    }
}

impl TryFrom<u64> for Seq {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Ok(Self(value))
    }
}

impl std::fmt::Display for Seq {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}
