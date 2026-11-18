use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use common::ir_v2;
use common::types::InstructionWrapper;
use ed25519_dalek::{Signature, VerifyingKey};

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

/// (kind, name) of every resource, as recorded in `InstructionMeta`.
pub fn instructions(program: &ir_v2::Program) -> Vec<(String, String)> {
    program
        .resources
        .iter()
        .map(|r| (r.kind().as_str().to_string(), r.name.clone()))
        .collect()
}

/// Checks the deployment name, the signature and the IR v2 program, whose `deployment` must equal `name`. IR v1
/// instruction arrays are refused: Rezn no longer executes them.
pub fn validate_envelope(name: &str, wrapper: &InstructionWrapper) -> Result<ir_v2::Program> {
    match &wrapper.program {
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
            Ok(program)
        }
        serde_json::Value::Array(_) => bail!(
            "IR v1 instruction arrays are no longer executed; submit an IR v2 program object (docs/IR-v2.md)"
        ),
        _ => bail!("program must be an IR v2 program object"),
    }
}
