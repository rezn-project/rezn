use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub type DesiredMap = BTreeMap<String, Vec<Molecule>>;

#[derive(Serialize, Deserialize, Debug)]
pub struct MoleculeMeta {
    pub sig_id: String,
    pub applied_at: DateTime<Utc>,
    pub atoms: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Molecule {
    pub kind: String,
    pub name: String,
    pub fields: Option<serde_json::Value>,
    pub options: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PodFields {
    pub image: String,
    pub replicas: usize,
    pub ports: Vec<u16>,
    pub secure: Option<bool>,
}

pub struct PodSpec {
    pub name: String,
    pub image: String,
    pub replicas: usize,
    pub ports: Vec<u16>,
}
