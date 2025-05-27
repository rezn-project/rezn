use anyhow::{Context, Result};
use sled::{Db, IVec};

pub trait Store {
    fn read(&self, key: &str) -> Result<Vec<u8>>;
}

pub struct SledStore {
    db: Db,
}

impl SledStore {
    pub fn new(path: &str) -> Result<Self> {
        let db = sled::open(path).context("opening sled store")?;
        Ok(Self { db })
    }
}

impl Store for SledStore {
    fn read(&self, key: &str) -> Result<Vec<u8>> {
        let val: Option<IVec> = self.db.get(key).context("reading key")?;
        val.map(|v| v.to_vec()).context("key not found")
    }
}
