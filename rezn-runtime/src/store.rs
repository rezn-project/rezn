use std::{collections::BTreeMap, fs::File, io::Read};

use anyhow::{bail, ensure, Context, Result};
use common::{
    ir_v2::Program,
    types::{DesiredMap, InstructionMeta, InstructionWrapper},
};
use serde::{Deserialize, Serialize};
use sled::Db;

use crate::intent::{validate_envelope, Validated};

pub const STATE_KEY: &str = "controller/v1";
/// Format 1 holds only IR v1 programs. Storing the first IR v2 program rewrites the state as format 2, which holds
/// either; binaries that only know format 1 then refuse the database instead of misreading it.
pub const FORMAT_V1: u32 = 1;
pub const FORMAT_V2: u32 = 2;

/// Validated desired intent, split by IR version.
#[derive(Default)]
pub struct Desired {
    pub v1: DesiredMap,
    pub v2: BTreeMap<String, Program>,
}

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

    pub fn desired(&self) -> Result<Desired> {
        ensure!(
            matches!(self.format, FORMAT_V1 | FORMAT_V2) && is_identity(&self.owner),
            "invalid controller format/ownership identity"
        );
        ensure!(
            (self.revision == 0) == self.deployments.is_empty(),
            "inconsistent controller revision/deployment records"
        );
        let mut desired = Desired::default();
        for (name, deployment) in &self.deployments {
            let validated = validate_envelope(name, &deployment.envelope)
                .with_context(|| format!("invalid persisted deployment {name:?}"))?;
            ensure!(
                deployment.meta.sig_id == deployment.envelope.signature.sig
                    && deployment.meta.instructions == validated.instructions(),
                "inconsistent persisted instruction metadata"
            );
            match validated {
                Validated::V1(instructions) => {
                    desired.v1.insert(name.clone(), instructions);
                }
                Validated::V2(program) => {
                    if self.format == FORMAT_V1 {
                        bail!("format 1 controller state contains an IR v2 program");
                    }
                    desired.v2.insert(name.clone(), program);
                }
            }
        }
        Ok(desired)
    }

    /// Each deployment's accepted program as JSON: IR v1 instruction arrays, IR v2 program objects.
    pub fn programs(&self) -> Result<BTreeMap<String, serde_json::Value>> {
        let desired = self.desired()?;
        let mut programs = BTreeMap::new();
        for (name, instructions) in desired.v1 {
            programs.insert(name, serde_json::to_value(instructions)?);
        }
        for name in desired.v2.into_keys() {
            programs.insert(
                name.clone(),
                self.deployments[&name].envelope.program.clone(),
            );
        }
        Ok(programs)
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
        format: FORMAT_V1,
        owner: random_id()?,
        revision: 0,
        deployments: BTreeMap::new(),
    };
    db.insert(STATE_KEY, serde_json::to_vec(&state)?)?;
    db.flush()?;
    Ok(())
}
