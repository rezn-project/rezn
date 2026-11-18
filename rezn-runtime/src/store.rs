use std::{collections::BTreeMap, fs::File, io::Read};

use anyhow::{ensure, Context, Result};
use common::{
    ir_v2::Program,
    types::{InstructionMeta, InstructionWrapper},
};
use serde::{Deserialize, Serialize};
use sled::Db;

use crate::intent::{instructions, validate_envelope};

pub const STATE_KEY: &str = "controller/v1";
/// Format 1 held IR v1 programs, which Rezn no longer executes: a format 1 database loads only while it holds no
/// deployment, and the next apply rewrites it as format 2. Format 2 holds IR v2 programs; binaries that only know
/// format 1 refuse it instead of misreading it.
pub const FORMAT_V1: u32 = 1;
pub const FORMAT_V2: u32 = 2;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub envelope: InstructionWrapper,
    pub meta: InstructionMeta,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredState {
    pub format: u32,
    pub owner: String,
    pub revision: u64,
    pub deployments: BTreeMap<String, Deployment>,
}

pub fn random_id() -> Result<String> {
    let mut bytes = [0; 32];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex::encode(bytes))
}

pub fn is_identity(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl StoredState {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let state: Self = serde_json::from_slice(bytes).context("corrupt controller state JSON")?;
        state.desired()?;
        Ok(state)
    }

    /// Every deployment's validated IR v2 program.
    pub fn desired(&self) -> Result<BTreeMap<String, Program>> {
        ensure!(
            matches!(self.format, FORMAT_V1 | FORMAT_V2) && is_identity(&self.owner),
            "invalid controller format/ownership identity"
        );
        ensure!(
            (self.revision == 0) == self.deployments.is_empty(),
            "inconsistent controller revision/deployment records"
        );
        let mut desired = BTreeMap::new();
        for (name, deployment) in &self.deployments {
            ensure!(
                !deployment.envelope.program.is_array(),
                "deployment {name:?} holds an IR v1 program, which this Rezn no longer executes; start with a new state database (its Docker containers are left untouched)"
            );
            ensure!(
                self.format == FORMAT_V2,
                "format {} controller state contains an IR v2 program",
                self.format
            );
            let program = validate_envelope(name, &deployment.envelope)
                .with_context(|| format!("invalid persisted deployment {name:?}"))?;
            ensure!(
                deployment.meta.sig_id == deployment.envelope.signature.sig
                    && deployment.meta.instructions == instructions(&program),
                "inconsistent persisted instruction metadata"
            );
            desired.insert(name.clone(), program);
        }
        Ok(desired)
    }

    /// Each deployment's accepted IR v2 program object, as signed.
    pub fn programs(&self) -> Result<BTreeMap<String, serde_json::Value>> {
        Ok(self
            .desired()?
            .into_keys()
            .map(|name| {
                let program = self.deployments[&name].envelope.program.clone();
                (name, program)
            })
            .collect())
    }
}

pub fn load(db: &Db) -> Result<StoredState> {
    let bytes = db
        .get(STATE_KEY)?
        .context("controller state missing; refusing an empty deletion plan")?;
    StoredState::decode(&bytes)
}

pub fn initialize(db: &Db) -> Result<()> {
    if db.contains_key(STATE_KEY)? {
        load(db)?;
        return Ok(());
    }
    ensure!(db.is_empty(), "legacy/unrecognized state database: use a new isolated database; no migration or adoption is performed");
    let state = StoredState {
        format: FORMAT_V2,
        owner: random_id()?,
        revision: 0,
        deployments: BTreeMap::new(),
    };
    db.insert(STATE_KEY, serde_json::to_vec(&state)?)?;
    db.flush()?;
    Ok(())
}
