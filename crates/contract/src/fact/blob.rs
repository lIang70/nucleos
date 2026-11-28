use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// The address of one piece of content: the SHA-256 of its bytes.
///
/// Content-addressed, so the same message written on ten turns is one row, a resumed session
/// shares its prefix instead of copying it, and a fact can prove what was sent by naming a digest
/// rather than carrying the payload again.
///
/// The algorithm is part of the contract on purpose. A deployment that chose SHA-512 would produce
/// addresses that no other reader could verify against the same bytes.
///
/// **On the wire it is 64 lowercase hex characters**, not 32 integers: a digest is an opaque token,
/// and an array of numbers is neither comparable, greppable, nor shorter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlobId([u8; 32]);

impl BlobId {
    /// Address these bytes.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self(hasher.finalize().into())
    }

    /// Address this JSON value.
    ///
    /// Serialising here rather than hashing a `to_string` at each call site is what keeps two
    /// writers from disagreeing about key order: `serde_json` writes a `Value`'s object keys in
    /// sorted order, so equal values produce equal bytes.
    ///
    /// # Errors
    ///
    /// A value that cannot be serialised has no address.
    pub fn of_json<T: Serialize>(value: &T) -> Result<Self> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| Error::internal(format!("content cannot be addressed: {error}")))?;
        Ok(Self::of(&bytes))
    }

    /// The raw digest.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex.
    #[must_use]
    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
            out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
        }
        out
    }

    /// Parse 64 hex characters, either case.
    ///
    /// # Errors
    ///
    /// The wrong length, or a character that is not a hex digit.
    pub fn from_hex(text: &str) -> Result<Self> {
        if text.len() != 64 {
            return Err(Error::invalid(format!(
                "a blob id is 64 hex characters, got {}",
                text.len()
            )));
        }
        let mut bytes = [0u8; 32];
        for (index, pair) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let high = hex_value(pair[0])?;
            let low = hex_value(pair[1])?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

fn hex_value(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Error::invalid(format!(
            "a blob id is hex, got {:?}",
            char::from(byte)
        ))),
    }
}

impl Serialize for BlobId {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for BlobId {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_hex(&text).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for BlobId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "BlobId".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        String::json_schema(generator)
    }
}

impl std::fmt::Display for BlobId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}
