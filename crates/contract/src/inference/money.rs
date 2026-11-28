use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Provider-reported billed units and exact cost.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Billing {
    /// Billed input tokens, when they differ from the metered ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Billed output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// The price, exactly as reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Money>,
}

/// `units` over `10^scale`, in `currency`.
///
/// Not a float and not a fixed micro-unit: an API that prices in 10⁻¹⁰ USD ticks
/// must survive without rounding, and a currency that is not USD must be able to
/// say so.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Money {
    /// The currency code the units are denominated in.
    pub currency: String,
    /// The integer amount.
    pub units: i64,
    /// How many decimal places `units` carries.
    pub scale: u8,
}

impl Money {
    /// US dollars with micro-unit precision.
    #[must_use]
    pub fn micro_usd(units: i64) -> Self {
        Self {
            currency: "USD".to_owned(),
            units,
            scale: 6,
        }
    }

    /// Sum two amounts, or refuse.
    ///
    /// Mismatched currency or scale is `None`, not an error to swallow:
    /// converting between them is a policy decision (exchange rate, precision)
    /// this type does not make.
    #[must_use]
    pub fn checked_add(&self, other: &Self) -> Option<Self> {
        if self.currency != other.currency || self.scale != other.scale {
            return None;
        }
        Some(Self {
            currency: self.currency.clone(),
            units: self.units.saturating_add(other.units),
            scale: self.scale,
        })
    }
}
