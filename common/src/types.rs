use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub type DesiredMap = BTreeMap<String, Vec<Instruction>>;

#[derive(Clone, Serialize, Deserialize, Debug, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Signature {
    pub algorithm: String,
    #[serde(rename = "pub")]
    pub pubkey: String,
    pub sig: String,
}

/// Keep submitted program values intact until canonical signature verification.
#[derive(Clone, Serialize, Deserialize, Debug, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InstructionWrapper {
    pub program: Vec<serde_json::Value>,
    pub signature: Signature,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct InstructionMeta {
    pub sig_id: String,
    pub applied_at: DateTime<Utc>,
    pub instructions: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize, Debug, Clone, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Instruction {
    pub kind: String,
    pub name: String,
    pub fields: PodFields,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PodFields {
    pub image: String,
    pub replicas: usize,
    pub ports: Vec<u16>,
}
