use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use common::types::{Instruction, InstructionWrapper, PodFields};
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 63
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "name must be 1–63 ASCII letters/digits, '-' or '_', beginning with a letter/digit"
    );
    Ok(())
}

pub fn verify(wrapper: &InstructionWrapper) -> Result<()> {
    let sig = &wrapper.signature;
    ensure!(
        sig.algorithm == "ed25519",
        "unsupported signature algorithm: {}",
        sig.algorithm
    );
    let key: [u8; 32] = STANDARD
        .decode(&sig.pubkey)
        .context("invalid public key base64")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("public key must be 32 bytes"))?;
    let signature: [u8; 64] = STANDARD
        .decode(&sig.sig)
        .context("invalid signature base64")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("signature must be 64 bytes"))?;
    let canonical = serde_json_canonicalizer::to_vec(&wrapper.program)?;
    VerifyingKey::from_bytes(&key)?
        .verify_strict(&canonical, &Signature::from_bytes(&signature))
        .context("invalid program signature")
}

pub fn validate_program(program: &[serde_json::Value]) -> Result<Vec<Instruction>> {
    let mut names = BTreeSet::new();
    program.iter().enumerate().map(|(index, raw)| {
        if let Some(kind) = raw.get("kind").and_then(serde_json::Value::as_str) {
            ensure!(kind == "pod", "instruction {index}: unsupported kind '{kind}'; only pod is executable");
        }
        let instruction: Instruction = serde_json::from_value(raw.clone())
            .with_context(|| format!("instruction {index}: only kind, name and fields (image, replicas, ports) are supported"))?;
        validate_name(&instruction.name).with_context(|| format!("instruction {index}"))?;
        ensure!(names.insert(instruction.name.clone()), "duplicate pod name '{}'", instruction.name);
        validate_image(&instruction.fields.image).with_context(|| format!("pod '{}'", instruction.name))?;
        let ports = &instruction.fields.ports;
        ensure!(ports.iter().all(|p| *p != 0), "pod '{}': TCP ports must be 1–65535", instruction.name);
        ensure!(ports.iter().collect::<BTreeSet<_>>().len() == ports.len(), "pod '{}': duplicate TCP port", instruction.name);
        Ok(instruction)
    }).collect()
}

// Deliberately bounded Docker reference grammar; availability is a backend concern.
fn validate_image(image: &str) -> Result<()> {
    ensure!(
        !image.is_empty() && image.len() <= 255,
        "image must be a nonempty Docker reference up to 255 bytes"
    );
    let (reference, digest) = match image.split_once('@') {
        Some((r, d)) => (r, Some(d)),
        None => (image, None),
    };
    if let Some(digest) = digest {
        let hash = digest
            .strip_prefix("sha256:")
            .context("image digest must be sha256")?;
        ensure!(
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "image sha256 digest must have 64 hex digits"
        );
    }
    let slash = reference.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (path, tag) = match reference[slash..].split_once(':') {
        Some((_, tag)) => (&reference[..reference.len() - tag.len() - 1], Some(tag)),
        None => (reference, None),
    };
    if let Some(tag) = tag {
        ensure!(
            !tag.is_empty()
                && tag.len() <= 128
                && (tag.as_bytes()[0].is_ascii_alphanumeric() || tag.starts_with('_'))
                && tag
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
            "invalid image tag"
        );
    }
    let components: Vec<_> = path.split('/').collect();
    for (i, component) in components.iter().enumerate() {
        if i == 0
            && components.len() > 1
            && (component.contains('.') || component.contains(':') || *component == "localhost")
        {
            let (host, port) = component
                .split_once(':')
                .map(|(h, p)| (h, Some(p)))
                .unwrap_or((component, None));
            ensure!(
                !host.is_empty()
                    && host.split('.').all(|part| !part.is_empty()
                        && part.as_bytes()[0].is_ascii_alphanumeric()
                        && part.as_bytes()[part.len() - 1].is_ascii_alphanumeric()
                        && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')),
                "invalid image registry"
            );
            if let Some(port) = port {
                ensure!(
                    port.parse::<u16>().is_ok_and(|p| p > 0),
                    "invalid image registry port"
                );
            }
            continue;
        }
        let bytes = component.as_bytes();
        ensure!(
            !bytes.is_empty()
                && bytes[0].is_ascii_lowercase_or_digit()
                && bytes[bytes.len() - 1].is_ascii_lowercase_or_digit(),
            "invalid image repository component"
        );
        let mut pos = 0;
        while pos < bytes.len() {
            if bytes[pos].is_ascii_lowercase_or_digit() {
                pos += 1;
                continue;
            }
            match bytes[pos] {
                b'.' => pos += 1,
                b'_' => {
                    pos += 1;
                    if bytes.get(pos) == Some(&b'_') {
                        pos += 1;
                    }
                }
                b'-' => {
                    while bytes.get(pos) == Some(&b'-') {
                        pos += 1;
                    }
                }
                _ => bail!("invalid image repository character"),
            }
            ensure!(
                bytes
                    .get(pos)
                    .is_some_and(|b| b.is_ascii_lowercase_or_digit()),
                "invalid image repository separator"
            );
        }
    }
    Ok(())
}

trait LowercaseOrDigit {
    fn is_ascii_lowercase_or_digit(&self) -> bool;
}
impl LowercaseOrDigit for u8 {
    fn is_ascii_lowercase_or_digit(&self) -> bool {
        self.is_ascii_lowercase() || self.is_ascii_digit()
    }
}

pub fn configuration(fields: &PodFields) -> String {
    let mut ports = fields.ports.clone();
    ports.sort_unstable();
    hex::encode(Sha256::digest(
        serde_json_canonicalizer::to_vec(
            &serde_json::json!({"image": fields.image, "ports": ports}),
        )
        .expect("serializable configuration"),
    ))
}
