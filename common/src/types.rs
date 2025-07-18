use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub type DesiredMap = BTreeMap<String, Vec<Instruction>>;

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct Signature {
    pub algorithm: String,
    #[serde(rename = "pub")]
    pub pubkey: String,
    pub sig: String,
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct InstructionWrapper {
    pub program: Vec<Instruction>,
    pub signature: Signature,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct InstructionMeta {
    pub sig_id: String,
    pub applied_at: DateTime<Utc>,
    pub instructions: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum EnvVar {
    Raw(String),
    FromSource { from: EnvSource, name: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EnvSource {
    Secret,
    AwsSecretsManager,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct EnvMap(pub HashMap<String, EnvVar>);

#[derive(Serialize, Deserialize, Debug, Clone, ToSchema)]
pub struct Instruction {
    pub kind: String,
    pub name: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<serde_json::Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PodFields {
    pub image: String,
    pub replicas: usize,
    pub ports: Vec<u16>,
    pub secure: Option<bool>,
    pub env: Option<EnvMap>,
}

pub struct PodSpec {
    pub mol_name: String,
    pub name: String,
    pub image: String,
    pub replicas: usize,
    pub ports: Vec<u16>,
}
