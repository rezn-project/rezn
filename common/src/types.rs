use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Clone, Serialize, Deserialize, Debug, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Signature {
    pub algorithm: String,
    #[serde(rename = "pub")]
    pub pubkey: String,
    pub sig: String,
}

/// Keep submitted program values intact until canonical signature verification.
/// `program` is an IR v2 program object (see `crate::ir_v2`); arrays are the retired IR v1.
#[derive(Clone, Serialize, Deserialize, Debug, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InstructionWrapper {
    pub program: serde_json::Value,
    pub signature: Signature,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct InstructionMeta {
    pub sig_id: String,
    pub applied_at: DateTime<Utc>,
    pub instructions: Vec<(String, String)>,
}
