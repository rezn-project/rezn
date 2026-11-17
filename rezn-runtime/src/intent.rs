use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
pub use common::image::validate_image;
use common::ir_v2;
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

/// A verified, validated program of either IR version.
pub enum Validated {
    V1(Vec<Instruction>),
    V2(ir_v2::Program),
}

impl Validated {
    /// (kind, name) of every instruction or resource, as recorded in `InstructionMeta`.
    pub fn instructions(&self) -> Vec<(String, String)> {
        match self {
            Validated::V1(instructions) => instructions
                .iter()
                .map(|i| (i.kind.clone(), i.name.clone()))
                .collect(),
            Validated::V2(program) => program
                .resources
                .iter()
                .map(|r| (r.kind().as_str().to_string(), r.name.clone()))
                .collect(),
        }
    }
}

/// Checks the deployment name, the signature and the program. An array is IR v1 (unchanged rules); an object
/// is IR v2, whose `deployment` must equal `name`.
pub fn validate_envelope(name: &str, wrapper: &InstructionWrapper) -> Result<Validated> {
    match &wrapper.program {
        serde_json::Value::Array(program) => {
            validate_name(name)?;
            verify(wrapper)?;
            Ok(Validated::V1(validate_program(program)?))
        }
        serde_json::Value::Object(_) => {
            ensure!(
                ir_v2::is_name(name, 63),
                "IR v2 deployment names are 1–63 lowercase letters, digits or '-', beginning and ending with a letter or digit"
            );
            verify(wrapper)?;
            let program = ir_v2::validate(&wrapper.program)
                .map_err(|issues| anyhow!("invalid IR v2 program: {}", ir_v2::describe(&issues)))?;
            ensure!(
                program.deployment == name,
                "program deployment {:?} does not match the deployment name {name:?}",
                program.deployment
            );
            Ok(Validated::V2(program))
        }
        _ => bail!("program must be an array (IR v1) or an object (IR v2)"),
    }
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
