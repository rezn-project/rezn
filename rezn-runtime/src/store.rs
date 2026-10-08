use std::{collections::BTreeMap, fs::File, io::Read};

use anyhow::{ensure, Context, Result};
use common::types::{DesiredMap, InstructionMeta, InstructionWrapper};
use serde::{Deserialize, Serialize};
use sled::Db;

use crate::intent::{validate_name, validate_program, verify};

pub const STATE_KEY: &str = "controller/v1";

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

    pub fn desired(&self) -> Result<DesiredMap> {
        ensure!(
            self.format == 1 && is_identity(&self.owner),
            "invalid controller format/ownership identity"
        );
        ensure!(
            (self.revision == 0) == self.deployments.is_empty(),
            "inconsistent controller revision/deployment records"
        );
        self.deployments
            .iter()
            .map(|(name, deployment)| {
                validate_name(name).context("invalid persisted deployment name")?;
                verify(&deployment.envelope).context("invalid persisted signature")?;
                let instructions = validate_program(&deployment.envelope.program)
                    .context("invalid persisted intent")?;
                ensure!(
                    deployment.meta.sig_id == deployment.envelope.signature.sig
                        && deployment.meta.instructions
                            == instructions
                                .iter()
                                .map(|i| (i.kind.clone(), i.name.clone()))
                                .collect::<Vec<_>>(),
                    "inconsistent persisted instruction metadata"
                );
                Ok((name.clone(), instructions))
            })
            .collect()
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
        format: 1,
        owner: random_id()?,
        revision: 0,
        deployments: BTreeMap::new(),
    };
    db.insert(STATE_KEY, serde_json::to_vec(&state)?)?;
    db.flush()?;
    Ok(())
}
