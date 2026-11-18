//! secret.rs – “Vault-for-grownups”
//!
//! Usage:
//!     let id = age_keys::get_identity()?;
//!     let store = SecretStore::open("/etc/rezn/secrets/db", id.clone())?;
//!     store.put("rezn/prod/db_url", b"postgres://doom:doom@localhost")?;
//!     let plain = store.get("rezn/prod/db_url")?.unwrap();
//!     println!("decrypted = {}", String::from_utf8_lossy(&plain));

//!
//! Programs' secrets (IR v2 `secret` resources) live apart from the raw keys above, in their own trees: values by
//! `<namespace>/<secret>/<key>`, age-encrypted, and each value's **version** in plain text. A version is an
//! HMAC-SHA256 of the key path and value under a random key kept encrypted in the store: equal for an unchanged
//! value, so rewriting it restarts nothing, and revealing nothing about values that are easy to guess.
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::iter;
use std::path::Path;

use age::x25519;
use age::{Decryptor, Encryptor};
use anyhow::{anyhow, ensure, Context, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sled::transaction::{ConflictableTransactionError, Transactional};
use sled::{Config, Db, Tree};

/// Key and version of every stored key of one program secret.
pub type Versions = BTreeMap<String, String>;

#[derive(Clone)]
pub struct SecretStore {
    db: Db,
    id: x25519::Identity,
    values: Tree,
    versions: Tree,
    mac_key: [u8; 32],
}

impl SecretStore {
    #[cfg(test)]
    pub fn temporary() -> Self {
        Self::with_db(
            Config::new().temporary(true).open().unwrap(),
            x25519::Identity::generate(),
        )
        .unwrap()
    }

    fn with_db(db: Db, id: x25519::Identity) -> Result<Self> {
        let values = db.open_tree("program-secrets")?;
        let versions = db.open_tree("program-secret-versions")?;
        let meta = db.open_tree("program-secret-meta")?;
        let mac_key = match meta.get("version-key")? {
            Some(cipher) => decrypt(&id, &cipher)?,
            None => {
                let mut key = [0u8; 32];
                fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
                meta.insert("version-key", encrypt(&id.to_public(), &key)?)?;
                meta.flush()?;
                key.to_vec()
            }
        };
        let mac_key = mac_key
            .try_into()
            .map_err(|_| anyhow!("corrupt secret version key"))?;
        Ok(Self {
            db,
            id,
            values,
            versions,
            mac_key,
        })
    }

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

        Self::with_db(db, id)
    }

    fn path(namespace: &str, secret: &str, key: &str) -> String {
        format!("{namespace}/{secret}/{key}")
    }

    /// The version of one value.
    fn version(&self, path: &str, value: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.mac_key).expect("any key length");
        mac.update(path.as_bytes());
        mac.update(&[0]);
        mac.update(value);
        hex::encode(&mac.finalize().into_bytes()[..16])
    }

    /// Replaces every stored key of a program secret with `data`, returning the new versions.
    pub fn put_program(
        &self,
        namespace: &str,
        secret: &str,
        data: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Versions> {
        let prefix = format!("{namespace}/{secret}/");
        let old: Vec<sled::IVec> = self
            .versions
            .scan_prefix(&prefix)
            .keys()
            .collect::<sled::Result<_>>()?;
        let mut encrypted = Vec::new();
        let mut versions = Versions::new();
        for (key, value) in data {
            let path = Self::path(namespace, secret, key);
            let version = self.version(&path, value);
            encrypted.push((path, encrypt(&self.id.to_public(), value)?, version.clone()));
            versions.insert(key.clone(), version);
        }
        (&self.values, &self.versions)
            .transaction(|(values, stored)| {
                for key in &old {
                    values.remove(key)?;
                    stored.remove(key)?;
                }
                for (path, cipher, version) in &encrypted {
                    values.insert(path.as_bytes(), cipher.as_slice())?;
                    stored.insert(path.as_bytes(), version.as_bytes())?;
                }
                Ok::<_, ConflictableTransactionError<()>>(())
            })
            .map_err(|e| anyhow!("writing secret {namespace}/{secret}: {e:?}"))?;
        self.db.flush()?;
        Ok(versions)
    }

    /// Stores one key unless it already has a value; returns whether it was written.
    pub fn put_program_key_if_absent(
        &self,
        namespace: &str,
        secret: &str,
        key: &str,
        value: &[u8],
    ) -> Result<bool> {
        let path = Self::path(namespace, secret, key);
        let cipher = encrypt(&self.id.to_public(), value)?;
        let version = self.version(&path, value);
        let written = (&self.values, &self.versions)
            .transaction(|(values, stored)| {
                if stored.get(path.as_bytes())?.is_some() {
                    return Ok(false);
                }
                values.insert(path.as_bytes(), cipher.as_slice())?;
                stored.insert(path.as_bytes(), version.as_bytes())?;
                Ok::<_, ConflictableTransactionError<()>>(true)
            })
            .map_err(|e| anyhow!("writing secret {namespace}/{secret}: {e:?}"))?;
        self.db.flush()?;
        Ok(written)
    }

    /// One decrypted value of a program secret, checked against its stored version.
    pub fn get_program(
        &self,
        namespace: &str,
        secret: &str,
        key: &str,
    ) -> Result<Option<(Vec<u8>, String)>> {
        let path = Self::path(namespace, secret, key);
        let Some(cipher) = self.values.get(&path)? else {
            return Ok(None);
        };
        let value = decrypt(&self.id, &cipher)?;
        let version = self.version(&path, &value);
        ensure!(
            self.versions.get(&path)?.as_deref() == Some(version.as_bytes()),
            "secret {path} does not match its recorded version"
        );
        Ok(Some((value, version)))
    }

    /// Versions of every stored program secret of a namespace, by secret name. Nothing is decrypted.
    pub fn program_versions(&self, namespace: &str) -> Result<BTreeMap<String, Versions>> {
        let mut secrets: BTreeMap<String, Versions> = BTreeMap::new();
        for entry in self.versions.scan_prefix(format!("{namespace}/")) {
            let (path, version) = entry?;
            let path = String::from_utf8(path.to_vec())?;
            let (secret, key) = path[namespace.len() + 1..]
                .split_once('/')
                .with_context(|| format!("malformed secret path {path:?}"))?;
            secrets
                .entry(secret.into())
                .or_default()
                .insert(key.into(), String::from_utf8(version.to_vec())?);
        }
        Ok(secrets)
    }

    /// Deletes every stored key of a program secret; returns whether there were any.
    pub fn delete_program(&self, namespace: &str, secret: &str) -> Result<bool> {
        let existed = self
            .versions
            .scan_prefix(format!("{namespace}/{secret}/"))
            .next()
            .is_some();
        self.put_program(namespace, secret, &BTreeMap::new())?;
        Ok(existed)
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

/// A random value of `length` characters from `alphabet`, drawn from the OS RNG without bias.
pub fn random_value(length: u32, alphabet: common::ir_v2::Alphabet) -> Result<String> {
    const ALPHANUMERIC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    const HEX: &[u8] = b"0123456789abcdef";
    let chars = match alphabet {
        common::ir_v2::Alphabet::Alphanumeric => ALPHANUMERIC,
        common::ir_v2::Alphabet::Hex => HEX,
    };
    // Bytes at or above the largest multiple of the alphabet size would favour its first characters.
    let limit = 256 - 256 % chars.len();
    let mut source = fs::File::open("/dev/urandom")?;
    let mut value = String::with_capacity(length as usize);
    let mut buffer = [0u8; 64];
    while value.len() < length as usize {
        source.read_exact(&mut buffer)?;
        for &b in &buffer {
            if (b as usize) < limit && value.len() < length as usize {
                value.push(chars[b as usize % chars.len()] as char);
            }
        }
    }
    Ok(value)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_secrets_are_versioned_without_decrypting() {
        let store = SecretStore::temporary();
        let data = BTreeMap::from([
            ("password".to_string(), b"hunter2".to_vec()),
            ("user".to_string(), b"app".to_vec()),
        ]);
        let versions = store.put_program("shop", "db", &data).unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions["password"].len(), 32);
        assert_ne!(versions["password"], versions["user"]);
        // Rewriting the same value keeps its version; a new value changes it.
        assert_eq!(store.put_program("shop", "db", &data).unwrap(), versions);
        let (value, version) = store
            .get_program("shop", "db", "password")
            .unwrap()
            .unwrap();
        assert_eq!(
            (value.as_slice(), version.as_str()),
            (&b"hunter2"[..], versions["password"].as_str())
        );
        // The same value under another path has another version.
        let other = store
            .put_program(
                "shop",
                "other",
                &BTreeMap::from([("password".to_string(), b"hunter2".to_vec())]),
            )
            .unwrap();
        assert_ne!(other["password"], versions["password"]);
        let listed = store.program_versions("shop").unwrap();
        assert_eq!(listed["db"], versions);
        assert!(store.program_versions("sho").unwrap().is_empty());
        // Replacing drops keys that are no longer given.
        let fewer = store
            .put_program(
                "shop",
                "db",
                &BTreeMap::from([("password".to_string(), b"new".to_vec())]),
            )
            .unwrap();
        assert_ne!(fewer["password"], versions["password"]);
        assert_eq!(store.program_versions("shop").unwrap()["db"], fewer);
        assert!(store.get_program("shop", "db", "user").unwrap().is_none());
        // Raw keys of the legacy routes do not see program secrets.
        assert!(store.keys().unwrap().is_empty());
        assert!(store.delete_program("shop", "db").unwrap());
        assert!(!store.delete_program("shop", "db").unwrap());
        assert!(!store.program_versions("shop").unwrap().contains_key("db"));
    }

    #[test]
    fn random_values_use_their_alphabet() {
        use common::ir_v2::Alphabet;
        let a = random_value(4096, Alphabet::Alphanumeric).unwrap();
        assert_eq!(a.len(), 4096);
        assert!(a.bytes().all(|b| b.is_ascii_alphanumeric()));
        // Every class of the alphabet shows up in 4096 draws.
        assert!(a.bytes().any(|b| b.is_ascii_uppercase()) && a.bytes().any(|b| b.is_ascii_digit()));
        let h = random_value(16, Alphabet::Hex).unwrap();
        assert!(
            h.len() == 16
                && h.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_ne!(
            random_value(32, Alphabet::Hex).unwrap(),
            random_value(32, Alphabet::Hex).unwrap()
        );
    }

    #[test]
    fn generated_keys_are_written_once() {
        let store = SecretStore::temporary();
        assert!(store
            .put_program_key_if_absent("shop", "app", "k", b"first")
            .unwrap());
        assert!(!store
            .put_program_key_if_absent("shop", "app", "k", b"second")
            .unwrap());
        assert_eq!(
            store.get_program("shop", "app", "k").unwrap().unwrap().0,
            b"first"
        );
    }

    #[test]
    fn the_version_key_survives_reopening() {
        let dir = std::env::temp_dir().join(format!("rezn-secret-test-{}", std::process::id()));
        let id = x25519::Identity::generate();
        let first = {
            let store = SecretStore::open(&dir, id.clone()).unwrap();
            store
                .put_program(
                    "ns",
                    "s",
                    &BTreeMap::from([("k".to_string(), b"v".to_vec())]),
                )
                .unwrap()
        };
        let store = SecretStore::open(&dir, id).unwrap();
        assert_eq!(store.program_versions("ns").unwrap()["s"], first);
        assert!(store.get_program("ns", "s", "k").unwrap().is_some());
        drop(store);
        let _ = fs::remove_dir_all(&dir);
    }
}
