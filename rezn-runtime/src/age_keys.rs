use age::x25519;
use anyhow::{anyhow, Context, Result};
use once_cell::sync::OnceCell;
use secrecy::ExposeSecret;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
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
        .unwrap_or_else(|_| PathBuf::from("identity.txt"));

    if id_path.exists() {
        // ----- Load ---------------------------------------------------
        let raw = fs::read_to_string(&id_path)
            .with_context(|| format!("reading {}", id_path.display()))?;

        raw.lines()
            .find(|l| l.trim_start().starts_with("AGE-SECRET-KEY-1"))
            .ok_or_else(|| anyhow!("no age identity in file"))?
            .trim()
            .parse::<x25519::Identity>()
            .map_err(|e| anyhow!("parsing age identity failed: {e}"))
    } else {
        // ----- Generate ----------------------------------------------
        if let Some(dir) = id_path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }

        let id = x25519::Identity::generate();

        // write private key (0600)
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&id_path)?
            .write_all(id.to_string().expose_secret().as_bytes())?;

        // write public key (0644)
        let pub_path = PathBuf::from("default.txt");
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

        Ok(id) // ← this branch returns Result<Identity, Error>
    }
}
