use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine};
use ed25519_dalek::{Signature, Verifier, VerifyingKey as PublicKey};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sled;
use std::{env, fs};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: reznctl-apply <signed-ir.json> <sled-db-path>");
        std::process::exit(1);
    }

    let json_path = &args[1];
    let sled_path = &args[2];
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

    let hash = Sha256::digest(&program_raw);
    println!("✔ hash: {}", hex::encode(hash));

    public_key
        .verify(&program_raw, &signature)
        .context("signature verification failed")?;

    let db = sled::open(sled_path).context("opening sled db")?;
    let ts_key = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let full_key = format!("conf/{}", ts_key);

    db.insert(full_key.as_bytes(), raw.as_bytes())
        .context("inserting conf")?;
    db.insert("conf/latest", ts_key.as_bytes())
        .context("setting latest conf")?;

    db.flush()?;

    println!("✔ applied IR as {}", full_key);
    Ok(())
}
