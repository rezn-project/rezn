use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};
use common::types::{InstructionMeta, InstructionWrapper};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    intent::{validate_name, validate_program, verify},
    routes::common::{app_error, AppError},
    store::{self, Deployment},
    AppState,
};

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyPayload {
    pub name: String,
    pub instruction_wrapper: InstructionWrapper,
}

#[derive(Serialize, ToSchema)]
pub struct Accepted {
    pub stored: bool,
    pub revision: u64,
}

#[utoipa::path(
    post, path = "/apply",
    request_body(content = ApplyPayload, description = "Replace one deployment with a signed executable pod program. The signature covers the canonical submitted program; the deployment name is unsigned.", content_type = "application/json"),
    responses(
        (status = 202, description = "Intent stored durably; inspect /status for convergence", body = Accepted),
        (status = 400, description = "Invalid signature or unsupported intent", body = String),
        (status = 422, description = "Malformed request or unknown envelope fields", body = String),
        (status = 500, description = "Unreadable/corrupt persisted state or storage failure", body = String)
    ), tag = "Apply"
)]
pub async fn apply_handler(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<ApplyPayload>,
) -> Result<(StatusCode, Json<Accepted>), AppError> {
    let invalid = |e: anyhow::Error| (StatusCode::BAD_REQUEST, format!("{e:#}"));
    validate_name(&payload.name).map_err(invalid)?;
    verify(&payload.instruction_wrapper).map_err(invalid)?;
    let instructions = validate_program(&payload.instruction_wrapper.program).map_err(invalid)?;
    let meta = InstructionMeta {
        sig_id: payload.instruction_wrapper.signature.sig.clone(),
        applied_at: chrono::Utc::now(),
        instructions: instructions
            .iter()
            .map(|i| (i.kind.clone(), i.name.clone()))
            .collect(),
    };
    let _intent = app.controller.intent.lock().await;
    let mut state = store::load(&app.db).map_err(app_error)?;
    state.revision = state
        .revision
        .checked_add(1)
        .ok_or_else(|| app_error("revision exhausted"))?;
    state.deployments.insert(
        payload.name,
        Deployment {
            envelope: payload.instruction_wrapper,
            meta,
        },
    );
    // One sled value contains intent, the original signed envelope and all metadata.
    app.db
        .insert(
            store::STATE_KEY,
            serde_json::to_vec(&state).map_err(app_error)?,
        )
        .map_err(app_error)?;
    app.db.flush().map_err(app_error)?;
    app.controller.trigger.notify_one();
    Ok((
        StatusCode::ACCEPTED,
        Json(Accepted {
            stored: true,
            revision: state.revision,
        }),
    ))
}
