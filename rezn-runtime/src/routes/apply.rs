use axum::extract::State;
use axum::Json;
use base64::engine::general_purpose;
use base64::Engine;
use common::types::{DesiredMap, MoleculeMeta, MoleculeWrapper};
use reqwest::StatusCode;
use serde::Deserialize;
use std::{collections::BTreeMap, sync::Arc};
use utoipa::ToSchema;

use anyhow::Result;
use ed25519_dalek::{Signature as Ed25519Signature, Verifier, VerifyingKey as PublicKey};

use crate::AppState;

type AppError = (StatusCode, String);

#[derive(Debug, Deserialize, ToSchema)]
pub struct ApplyPayload {
    name: String,
    molecule_wrapper: MoleculeWrapper,
}

#[utoipa::path(
    post,
    path = "/state",
    responses(
        (status = 200, body = Object)
    ),
    tag = "State",
)]
pub async fn apply_handler(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<ApplyPayload>,
) -> Result<Json<bool>, AppError> {
    let name = payload.name;
    let molecule_wrapper = payload.molecule_wrapper;

    let sig = &molecule_wrapper.signature;
    let pubkey_b64 = &sig.pubkey;
    let sig_b64 = &sig.sig;
    let algorithm = &sig.algorithm;
    let program = &molecule_wrapper.program;
    let program_raw = serde_json::to_vec(program).map_err(app_error)?;

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

    let mut desired_state_map: DesiredMap = match app.db.get("desired").map_err(app_error)? {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            tracing::error!("Failed to deserialize 'desired'. Likely old format. Err: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid desired state format. Try wiping ./rezn-data or migrating manually."
                    .into(),
            )
        })?,
        None => BTreeMap::new(),
    };

    if desired_state_map.contains_key(&name) {
        eprintln!("Warning: overwriting existing entry for '{}'", name);
    }

    desired_state_map.insert(name.to_string(), program.to_vec());

    let updated = serde_json::to_vec(&desired_state_map).map_err(app_error)?;
    app.db.insert("desired", updated).map_err(app_error)?;

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

    let meta: MoleculeMeta = MoleculeMeta {
        sig_id: sig_b64.to_string(),
        applied_at: now,
        atoms,
    };

    let meta_key = format!("molecule/{}", name);
    let meta_value = serde_json::to_vec(&meta)
        .map_err(|e| app_error(format!("Failed to serialize meta: {e}")))?;
    app.db.insert(meta_key, meta_value).map_err(app_error)?;

    // ---

    app.db.flush().map_err(app_error)?;

    Ok(Json(true))
}

fn app_error<E: std::fmt::Display>(e: E) -> AppError {
    tracing::warn!("internal error: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
