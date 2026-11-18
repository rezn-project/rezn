use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};
use common::{
    ir_v2::Spec,
    types::{InstructionMeta, InstructionWrapper},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    admission, certificates,
    intent::{instructions, validate_envelope},
    routes::common::{app_error, AppError},
    secret,
    store::{self, Deployment},
    workload, AppState,
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
    request_body(content = ApplyPayload, description = "Replace one deployment with a signed program: an IR v2 program object, validated, stored and then executed through the Orqos workload backend (programs needing features it lacks are listed under /status unexecuted). IR v1 instruction arrays are refused. The signature covers the canonical submitted program, including its deployment and namespace.", content_type = "application/json"),
    responses(
        (status = 202, description = "Intent stored durably; inspect /status for convergence", body = Accepted),
        (status = 400, description = "Invalid signature or program, resource names colliding with another deployment of the same namespace, or referenced store or seed_once secret keys missing from the secret store (or holding control characters where they become environment variables), or requests that, with the other deployments', exceed the host's allocatable CPU or memory", body = String),
        (status = 422, description = "Malformed request or unknown envelope fields", body = String),
        (status = 503, description = "The host's capacity could not be read from Orqos, so requests cannot be checked", body = String),
        (status = 500, description = "Unreadable/corrupt persisted state or storage failure", body = String)
    ), tag = "Apply"
)]
pub async fn apply_handler(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<ApplyPayload>,
) -> Result<(StatusCode, Json<Accepted>), AppError> {
    let invalid = |e: anyhow::Error| (StatusCode::BAD_REQUEST, format!("{e:#}"));
    let program =
        validate_envelope(&payload.name, &payload.instruction_wrapper).map_err(invalid)?;
    let meta = InstructionMeta {
        sig_id: payload.instruction_wrapper.signature.sig.clone(),
        applied_at: chrono::Utc::now(),
        instructions: instructions(&program),
    };
    let _intent = app.controller.intent.lock().await;
    let mut state = store::load(&app.db).map_err(app_error)?;
    let collisions = workload::collisions(
        &payload.name,
        &program,
        &state.desired().map_err(app_error)?,
    );
    if !collisions.is_empty() {
        return Err((StatusCode::BAD_REQUEST, collisions.join("\n")));
    }
    // Local checks first: Orqos is asked nothing for a request that fails on its own.
    let next_revision = state
        .revision
        .checked_add(1)
        .ok_or_else(|| app_error("revision exhausted"))?;
    // Requests are admission checks against the host: a revision that does not fit is refused, not started.
    let capacity = app.orqos.capacity().await.map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("cannot check the revision against the host's capacity: {e:#}"),
        )
    })?;
    let allocatable = admission::allocatable(capacity, admission::reserve());
    admission::check(
        &payload.name,
        &program,
        &state.desired().map_err(app_error)?,
        allocatable,
    )
    .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    // A revision whose referenced secret keys are missing fails before any workload changes.
    let namespace = &program.namespace;
    let stored = app
        .secret_store
        .program_versions(namespace)
        .map_err(app_error)?;
    let mut blocking: Vec<String> = workload::missing_keys(&program, &stored)
        .iter()
        .filter(|m| !m.generated)
        .map(|m| m.reason(namespace))
        .collect();
    for (secret, key) in workload::env_keys(&program) {
        let value = app
            .secret_store
            .get_program(namespace, secret, key)
            .map_err(app_error)?;
        if value.is_some_and(|(v, _)| {
            String::from_utf8(v).map_or(true, |v| v.chars().any(char::is_control))
        }) {
            blocking.push(format!(
                "key {key:?} of secret {secret:?} becomes an environment variable but is not UTF-8 without control characters"
            ));
        }
    }
    if !blocking.is_empty() {
        return Err((StatusCode::BAD_REQUEST, blocking.join("\n")));
    }
    // Random keys are generated once and kept across revisions.
    for resource in &program.resources {
        let Spec::Secret(s) = &resource.spec else {
            continue;
        };
        for (key, random) in s.source.random.iter().flatten() {
            if !stored
                .get(&resource.name)
                .is_some_and(|have| have.contains_key(key))
            {
                let value =
                    secret::random_value(random.length, random.alphabet).map_err(app_error)?;
                app.secret_store
                    .put_program_key_if_absent(namespace, &resource.name, key, value.as_bytes())
                    .map_err(app_error)?;
            }
        }
    }
    // Certificate authorities are generated once; certificates are issued when missing or changed.
    for change in certificates::maintain(&app.secret_store, [&program], chrono::Utc::now())
        .map_err(app_error)?
    {
        tracing::info!("{change}");
    }
    state.revision = next_revision;
    // Forward-only migration: an empty format 1 database becomes format 2 in this same write.
    state.format = store::FORMAT_V2;
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
