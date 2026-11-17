use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};
use common::types::{InstructionMeta, InstructionWrapper};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    intent::{validate_envelope, Validated},
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
    request_body(content = ApplyPayload, description = "Replace one deployment with a signed program: an IR v1 instruction array (executed) or an IR v2 program object (validated and stored; not yet executed, see /status unexecuted). The signature covers the canonical submitted program; an IR v1 deployment name is unsigned, an IR v2 program signs its deployment and namespace.", content_type = "application/json"),
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
    let validated =
        validate_envelope(&payload.name, &payload.instruction_wrapper).map_err(invalid)?;
    let meta = InstructionMeta {
        sig_id: payload.instruction_wrapper.signature.sig.clone(),
        applied_at: chrono::Utc::now(),
        instructions: validated.instructions(),
    };
    let _intent = app.controller.intent.lock().await;
    let mut state = store::load(&app.db).map_err(app_error)?;
    state.revision = state
        .revision
        .checked_add(1)
        .ok_or_else(|| app_error("revision exhausted"))?;
    if matches!(validated, Validated::V2(_)) {
        // Forward-only migration: the first IR v2 program moves the database to format 2 in this same write.
        state.format = state.format.max(store::FORMAT_V2);
    }
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
