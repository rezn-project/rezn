use axum::extract::State;
use axum::Json;
use base64::engine::general_purpose;
use base64::Engine;
use common::types::{DesiredMap, InstructionMeta, InstructionWrapper};
use serde::Deserialize;
use serde_json_canonicalizer::to_vec;
use sled::transaction::{ConflictableTransactionError, TransactionError};
use std::sync::Arc;
use utoipa::ToSchema;

use anyhow::Result;
use ed25519_dalek::{Signature as Ed25519Signature, Verifier, VerifyingKey as PublicKey};

use crate::{
    routes::common::{app_error, AppError},
    AppState,
};

#[derive(Debug, Deserialize, ToSchema)]
pub struct ApplyPayload {
    name: String,
    instruction_wrapper: InstructionWrapper,
}

#[utoipa::path(
    post,
    path = "/apply",
    request_body(
        content = ApplyPayload,
        description = "Payload to create a container",
        content_type = "application/json",
    ),
    responses(
        (status = 200, body = Object)
    ),
    tag = "Apply",
)]
pub async fn apply_handler(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<ApplyPayload>,
) -> Result<Json<bool>, AppError> {
    tracing::debug!("Applying payload");

    let name = payload.name;
    let instruction_wrapper = payload.instruction_wrapper;

    let sig = &instruction_wrapper.signature;
    let pubkey_b64 = &sig.pubkey;
    let sig_b64 = &sig.sig;
    let algorithm = &sig.algorithm;
    let program = &instruction_wrapper.program;
    let program_raw = to_vec(program).map_err(app_error)?;

    if algorithm != "ed25519" {
        return Err(app_error(format!(
            "unsupported signature algorithm: {}",
            algorithm
        )));
    }

    let pubkey_bytes = general_purpose::STANDARD
        .decode(pubkey_b64)
        .map_err(app_error)?;
    let sig_bytes = general_purpose::STANDARD
        .decode(sig_b64)
        .map_err(app_error)?;

    let pubkey_array: &[u8; 32] = pubkey_bytes.as_slice().try_into().map_err(app_error)?;
    let public_key = PublicKey::from_bytes(pubkey_array).map_err(app_error)?;
    let sig_array: &[u8; 64] = sig_bytes.as_slice().try_into().map_err(app_error)?;
    let signature = Ed25519Signature::try_from(sig_array).map_err(app_error)?;

    public_key
        .verify(&program_raw, &signature)
        .map_err(app_error)?;

    app.db
        .transaction(|tree| {
            // ---- load current state (may be absent) ----
            let mut desired: DesiredMap = tree
                .get("desired")?
                .map(|v| {
                    serde_json::from_slice(&v)
                        .map_err(|e| ConflictableTransactionError::Abort(app_error(e)))
                    // 🔑 convert error
                })
                .transpose()?
                .unwrap_or_default();

            // ---- mutate ----
            desired.insert(name.clone(), program.to_vec());

            // ---- store back ----
            let bytes = serde_json::to_vec(&desired)
                .map_err(|e| ConflictableTransactionError::Abort(app_error(e)))?;

            tree.insert("desired", bytes)?; // sled ops already return CTE

            Ok(())
        })
        .map_err(|e| match e {
            TransactionError::Abort(app_e) => app_e,
            TransactionError::Storage(io_e) => app_error(io_e),
        })?;

    // ----

    let now = chrono::Utc::now();

    let atoms = program
        .iter()
        .map(|item| {
            let kind = item.kind.as_str();
            let name = item.name.as_str();
            Ok((kind.to_string(), name.to_string()))
        })
        .collect::<Result<Vec<_>>>()
        .map_err(app_error)?;

    let meta: InstructionMeta = InstructionMeta {
        sig_id: sig_b64.to_string(),
        applied_at: now,
        atoms,
    };

    let meta_key = format!("instruction/{}", name);
    let meta_value = serde_json::to_vec(&meta)
        .map_err(|e| app_error(format!("Failed to serialize meta: {e}")))?;
    app.db.insert(meta_key, meta_value).map_err(app_error)?;

    // ---

    app.db.flush().map_err(app_error)?;

    Ok(Json(true))
}
