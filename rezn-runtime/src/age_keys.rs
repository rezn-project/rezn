// src/age_keys.rs
use age::{x25519, Identity};
use anyhow::{Context, Result};
use once_cell::sync::OnceCell;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

static IDENTITY: OnceCell<x25519::Identity> = OnceCell::new();

/// Public entry-point: grab the singleton private key.
pub fn get_identity() -> Result<&'static x25519::Identity> {
    IDENTITY.get_or_try_init(load_or_generate_identity)
}

/// Try to load an existing key from disk; otherwise generate + persist it.
fn load_or_generate_identity() -> Result<x25519::Identity> {
    // Let admins override the path if they really need to.
    let id_path = std::env::var("REZN_AGE_IDENTITY")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/rezn/keys/identity.txt"));

    if id_path.exists() {
        // ----- Load -----
        let raw = fs::read_to_string(&id_path)
            .with_context(|| format!("reading identity from {}", id_path.display()))?;

        raw.parse::<x25519::Identity>()
            .context("parsing age identity failed")
    } else {
        // ----- Generate -----
        if let Some(dir) = id_path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }

        let id = x25519::Identity::generate();

        // Persist private key (0600)
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&id_path)?
            .write_all(id.to_string().as_bytes())?;

        // Persist public key so CI/CD can encrypt to it (0644)
        let pub_path = PathBuf::from("/etc/rezn/recipients/default.txt");
        if let Some(dir) = pub_path.parent() {
            fs::create_dir_all(dir)?;
        }
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o644)
            .open(&pub_path)?
            .write_all(id.to_public().to_string().as_bytes())?;

        Ok(id)
    }
}
