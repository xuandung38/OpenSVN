//! Identifier and numeric newtypes shared by all IPC types.

use serde::{Deserialize, Serialize};
use specta::Type;

/// Working-copy id (UUID v7 string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(transparent)]
pub struct WcId(pub String);

/// Operation id (UUID v7 string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(transparent)]
pub struct OpId(pub String);

/// Repository revision. Wire value is a JSON number (u64, safe below 2^53); the TS type is
/// declared as `number` via a `u32` override because specta rejects u64/f64 without nulls.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Type,
)]
#[serde(transparent)]
pub struct Rev(#[specta(type = u32)] pub u64);

/// Byte size; same wire/TS treatment as [`Rev`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Type,
)]
#[serde(transparent)]
pub struct Size(#[specta(type = u32)] pub u64);
