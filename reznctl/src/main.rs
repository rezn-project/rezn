use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine};
use common::types::{DesiredMap, Molecule, MoleculeMeta};
use ed25519_dalek::{Signature, Verifier, VerifyingKey as PublicKey};
use serde_json::Value;
use sled;
use std::{collections::BTreeMap, env, fs};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 {
        eprintln!("Usage: reznctl-apply <name> <signed-ir.json> <sled-db-path>");
        std::process::exit(1);
    }

    let name = &args[1];
    let json_path = &args[2];
    let sled_path = &args[3];
    let raw = fs::read_to_string(json_path).context("reading IR file")?;
    let json: Value = serde_json::from_str(&raw).context("parsing JSON")?;

    let sig_obj = json
        .get("signature")
        .context("missing 'signature' object")?;
    let pubkey_b64 = sig_obj
        .get("pub")
        .context("missing 'pub' field")?
        .as_str()
        .context("'pub' not a string")?;
    let sig_b64 = sig_obj
        .get("sig")
        .context("missing 'sig' field")?
        .as_str()
        .context("'sig' not a string")?;
    let algorithm = sig_obj
        .get("algorithm")
        .context("missing 'algorithm' field")?
        .as_str()
        .context("'algorithm' not a string")?;

    if algorithm != "ed25519" {
        anyhow::bail!("unsupported signature algorithm: {}", algorithm);
    }

    let pubkey_bytes = general_purpose::STANDARD
        .decode(pubkey_b64)
        .context("decoding public key")?;
    let sig_bytes = general_purpose::STANDARD
        .decode(sig_b64)
        .context("decoding signature")?;

    let pubkey_array: &[u8; 32] = pubkey_bytes
        .as_slice()
        .try_into()
        .context("public key is not 32 bytes")?;
    let public_key = PublicKey::from_bytes(pubkey_array).context("invalid public key")?;
    let sig_array: &[u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .context("signature is not 64 bytes")?;
    let signature =
        Signature::try_from(sig_array).map_err(|e| anyhow::anyhow!("invalid signature: {}", e))?;

    let program = json.get("program").context("missing 'program' field")?;
    let program_raw = serde_json::to_vec(program).context("serializing 'program'")?;

    public_key
        .verify(&program_raw, &signature)
        .context("signature verification failed")?;

    let db = sled::open(sled_path).context("opening sled db")?;
    let ts_key = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let full_key = format!("conf/{}", ts_key);

    let program_array = program.as_array().context("'program' must be an array")?;

    let mut items: DesiredMap = if let Some(bytes) = db.get("desired")? {
        match serde_json::from_slice(&bytes) {
            Ok(map) => map,
            Err(e) => {
                eprintln!(
                    "Error: failed to deserialize 'desired' as a molecule map.\n\
                This likely means the DB contains an old format (e.g., a flat list of atoms).\n\
                Details: {}\n\
                Tip: wipe ./rezn-data or migrate the format manually.",
                    e
                );
                std::process::exit(1);
            }
        }
    } else {
        BTreeMap::new()
    };

    if items.contains_key(name) {
        eprintln!("Warning: overwriting existing entry for '{}'", name);
    }

    items.insert(
        name.to_string(),
        program_array
            .iter()
            .map(|item| {
                serde_json::from_value(item.clone()).context("parsing item in 'program' array")
            })
            .collect::<Result<Vec<Molecule>>>()?,
    );

    let updated = serde_json::to_vec(&items).context("serializing updated desired state")?;
    db.insert("desired", updated)?;

    // ---

    let now = chrono::Utc::now();

    let atoms = program_array
        .iter()
        .map(|item| {
            let obj = item.as_object().context("expected object in 'program'")?;
            let kind = obj
                .get("kind")
                .and_then(|k| k.as_str())
                .context("missing 'kind'")?;
            let name = obj
                .get("name")
                .and_then(|n| n.as_str())
                .context("missing 'name'")?;
            Ok((kind.to_string(), name.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;

    let meta: MoleculeMeta = MoleculeMeta {
        sig_id: sig_b64.to_string(),
        applied_at: now,
        atoms,
    };

    let meta_key = format!("molecule/{}", name);
    let meta_value = serde_json::to_vec(&meta)?;
    db.insert(meta_key, meta_value)?;

    // ---

    db.flush()?;

    println!("✔ applied IR as {}", full_key);
    Ok(())
}
