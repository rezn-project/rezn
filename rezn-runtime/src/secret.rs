//! secret.rs – “Vault-for-grownups”
//!
//! Usage:
//!     let id = age_keys::get_identity()?;
//!     let store = SecretStore::open("/etc/rezn/secrets/db", id.clone())?;
//!     store.put("rezn/prod/db_url", b"postgres://doom:doom@localhost")?;
//!     let plain = store.get("rezn/prod/db_url")?.unwrap();
//!     println!("decrypted = {}", String::from_utf8_lossy(&plain));

use std::fs;
use std::io::{Read, Write};
use std::iter;
use std::path::Path;

use age::x25519;
use age::{Decryptor, Encryptor};
use anyhow::{anyhow, Context, Result};
use sled::{Config, Db};

#[derive(Clone)]
pub struct SecretStore {
    db: Db,
    id: x25519::Identity,
}

impl SecretStore {
    /// Open (or create) a store at `path`.
    pub fn open<P: AsRef<Path>>(path: P, id: x25519::Identity) -> Result<Self> {
        // make sure parent dirs exist
        if let Some(dir) = path.as_ref().parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }

        let db = Config::default()
            .path(path)
            .flush_every_ms(Some(5_000))
            .open()
            .context("opening sled DB")?;

        Ok(Self { db, id })
    }

    /// Insert/overwrite a secret (encrypted before hitting sled).
    pub fn put(&self, key: &str, plaintext: &[u8]) -> Result<()> {
        let ciphertext = encrypt(&self.id.to_public(), plaintext)?;
        self.db.insert(key, ciphertext)?.map(|_| ()); // ignore previous value
        self.db.flush()?;
        Ok(())
    }

    /// Fetch & decrypt.  Returns Ok(None) if key doesn’t exist.
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let val_opt = self.db.get(key)?;
        match val_opt {
            None => Ok(None),
            Some(ivec) => {
                let plain = decrypt(&self.id, ivec.as_ref())?;
                Ok(Some(plain))
            }
        }
    }

    pub fn delete(&self, key: &str) -> Result<bool> {
        let removed = self.db.remove(key)?;
        self.db.flush()?;
        Ok(removed.is_some())
    }

    pub fn keys(&self) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        for kv in self.db.iter() {
            let (k, _) = kv?;
            keys.push(String::from_utf8(k.to_vec())?);
        }
        Ok(keys)
    }

    /// Dump a secret to a `.age` file so it can be shipped elsewhere.
    pub fn export<P: AsRef<Path>>(&self, key: &str, output: P) -> Result<()> {
        let plain = self
            .get(key)?
            .ok_or_else(|| anyhow!("secret '{}' not found", key))?;
        let mut file = fs::File::create(output)?;
        let cipher = encrypt(&self.id.to_public(), &plain)?;
        file.write_all(&cipher)?;
        Ok(())
    }

    /// Import a `.age` file into the store under `key`.
    pub fn import<P: AsRef<Path>>(&self, key: &str, input: P) -> Result<()> {
        let cipher = fs::read(input)?;
        // Quick sanity check: can we decrypt with *our* identity?
        let _ = decrypt(&self.id, &cipher)?;
        self.db.insert(key, cipher)?;
        self.db.flush()?;
        Ok(())
    }
}

/* --------------------------------------------------------------------- */
/*                      Internals: encrypt / decrypt                     */
/* --------------------------------------------------------------------- */

fn encrypt(recipient: &age::x25519::Recipient, plain: &[u8]) -> Result<Vec<u8>> {
    let encryptor = Encryptor::with_recipients(iter::once(recipient as &dyn age::Recipient))
        .expect("recipient provided");

    let mut out = vec![];
    let mut w = encryptor.wrap_output(&mut out)?;
    w.write_all(plain)?;
    w.finish()?;
    Ok(out)
}

fn decrypt(id: &x25519::Identity, cipher: &[u8]) -> Result<Vec<u8>> {
    let decryptor = Decryptor::new(cipher).map_err(|_| anyhow!("invalid age header"))?;

    let mut r = decryptor.decrypt(iter::once(id as &dyn age::Identity))?;
    let mut out = vec![];
    r.read_to_end(&mut out)?;
    Ok(out)
}
