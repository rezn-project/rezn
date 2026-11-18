//! Operator API for programs' secrets in Rezn's secret store: values of `secret` resources with a `store` or
//! `seed_once` source, by namespace. Values go in and never come back out; listings show keys and versions.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use common::ir_v2::{self, Program, Spec};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    routes::common::{app_error, AppError},
    secret::Versions,
    store, workload, AppState,
};

/// At most 1 MiB of values per secret, like a Kubernetes Secret (and an Orqos secret).
const MAX_BYTES: usize = 1 << 20;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutProgramSecret {
    /// Values by key (UTF-8 strings).
    pub data: BTreeMap<String, String>,
}

#[derive(Serialize, ToSchema)]
pub struct KeyVersion {
    pub key: String,
    /// Changes exactly when the value changes; reveals nothing about it.
    pub version: String,
}

#[derive(Serialize, ToSchema)]
pub struct ProgramSecretView {
    pub name: String,
    pub keys: Vec<KeyVersion>,
    /// Deployments of this namespace whose programs declare the secret.
    pub declared_by: Vec<String>,
    /// The declared source (`store`, `seed_once`, `random`, `ca` or `certificate`), if declared.
    pub source: Option<String>,
    /// Declared keys without a stored value.
    pub missing: Vec<String>,
}

fn source_kind(secret: &ir_v2::Secret) -> &'static str {
    let s = &secret.source;
    if s.store.is_some() {
        "store"
    } else if s.seed_once.is_some() {
        "seed_once"
    } else if s.random.is_some() {
        "random"
    } else if s.ca.is_some() {
        "ca"
    } else {
        "certificate"
    }
}

/// Every stored program's declaration of `name` in `namespace`: (deployment, program, secret).
fn declarations<'a>(
    programs: &'a BTreeMap<String, Program>,
    namespace: &str,
    name: Option<&str>,
) -> Vec<(&'a str, &'a Program, &'a str, &'a ir_v2::Secret)> {
    let mut found = Vec::new();
    for (deployment, program) in programs.iter().filter(|(_, p)| p.namespace == namespace) {
        for resource in &program.resources {
            if let Spec::Secret(secret) = &resource.spec {
                if name.is_none_or(|n| n == resource.name) {
                    found.push((deployment.as_str(), program, resource.name.as_str(), secret));
                }
            }
        }
    }
    found
}

fn check_names(namespace: &str, name: &str) -> Result<(), AppError> {
    if ir_v2::is_name(namespace, 63) && ir_v2::is_name(name, 63) {
        Ok(())
    } else {
        Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "namespace and secret names are DNS labels of at most 63 characters".into(),
        ))
    }
}

/// Kubernetes' key grammar, as IR v2 secret keys: `[-._a-zA-Z0-9]{1,253}`, not `.` or `..`.
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 253
        && key != "."
        && key != ".."
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
}

fn view(
    name: &str,
    versions: Option<&Versions>,
    declared: &[(&str, &Program, &str, &ir_v2::Secret)],
) -> ProgramSecretView {
    let keys = versions
        .into_iter()
        .flatten()
        .map(|(key, version)| KeyVersion {
            key: key.clone(),
            version: version.clone(),
        })
        .collect();
    let declared_keys: BTreeSet<&str> = declared
        .iter()
        .flat_map(|(_, _, _, s)| s.keys.iter().map(String::as_str))
        .collect();
    ProgramSecretView {
        name: name.into(),
        keys,
        declared_by: declared.iter().map(|(d, ..)| d.to_string()).collect(),
        source: declared
            .first()
            .map(|(_, _, _, s)| source_kind(s).to_string()),
        missing: declared_keys
            .into_iter()
            .filter(|k| !versions.is_some_and(|v| v.contains_key(*k)))
            .map(String::from)
            .collect(),
    }
}

#[utoipa::path(
    put, path = "/namespaces/{namespace}/secrets/{name}",
    params(("namespace" = String, Path), ("name" = String, Path)),
    request_body(content = PutProgramSecret, description = "Every value of the secret: replaces what is stored. A `seed_once` secret only gains keys it lacks.", content_type = "application/json"),
    responses(
        (status = 200, description = "Stored; keys and versions, never values", body = ProgramSecretView),
        (status = 400, description = "A value that a program uses as an environment variable contains control characters", body = String),
        (status = 409, description = "Rezn generates this secret, or a seed_once key already has a value", body = String),
        (status = 422, description = "Invalid names, keys or size", body = String)
    ), tag = "Secrets"
)]
pub async fn put_program_secret(
    State(app): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(payload): Json<PutProgramSecret>,
) -> Result<Json<ProgramSecretView>, AppError> {
    check_names(&namespace, &name)?;
    let invalid = |m: String| (StatusCode::UNPROCESSABLE_ENTITY, m);
    if payload.data.is_empty() {
        return Err(invalid("data: at least one key".into()));
    }
    if let Some(key) = payload.data.keys().find(|k| !valid_key(k)) {
        return Err(invalid(format!(
            "data: invalid key {key:?} (letters, digits, '-', '_' and '.', at most 253; not \".\" or \"..\")"
        )));
    }
    if payload.data.values().map(String::len).sum::<usize>() > MAX_BYTES {
        return Err(invalid(format!("data: at most {MAX_BYTES} bytes in total")));
    }
    let _intent = app.controller.intent.lock().await;
    let programs = store::load(&app.db)
        .and_then(|s| s.desired())
        .map_err(app_error)?;
    let declared = declarations(&programs, &namespace, Some(&name));
    let conflict = |m: String| (StatusCode::CONFLICT, m);
    let seed_once = match declared.first().map(|(_, _, _, s)| source_kind(s)) {
        Some(kind @ ("random" | "ca" | "certificate")) => {
            return Err(conflict(format!(
                "secret {name:?} is generated by Rezn (source {kind}) for deployment {:?}; it cannot be set",
                declared[0].0
            )))
        }
        Some("seed_once") => true,
        _ => false,
    };
    for (deployment, program, ..) in &declared {
        for (secret, key) in workload::env_keys(program) {
            let control = payload
                .data
                .get(key)
                .is_some_and(|v| v.chars().any(char::is_control));
            if secret == name && control {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("key {key:?} becomes an environment variable in deployment {deployment:?}, which cannot carry control characters"),
                ));
            }
        }
    }
    let stored = app
        .secret_store
        .program_versions(&namespace)
        .map_err(app_error)?;
    let have = stored.get(&name);
    let versions = if seed_once {
        if let Some(key) = payload
            .data
            .keys()
            .find(|k| have.is_some_and(|h| h.contains_key(*k)))
        {
            return Err(conflict(format!(
                "key {key:?} of seed_once secret {name:?} already has a value, which is kept"
            )));
        }
        for (key, value) in &payload.data {
            app.secret_store
                .put_program_key_if_absent(&namespace, &name, key, value.as_bytes())
                .map_err(app_error)?;
        }
        app.secret_store
            .program_versions(&namespace)
            .map_err(app_error)?
            .remove(&name)
            .unwrap_or_default()
    } else {
        let data = payload
            .data
            .iter()
            .map(|(k, v)| (k.clone(), v.as_bytes().to_vec()))
            .collect();
        app.secret_store
            .put_program(&namespace, &name, &data)
            .map_err(app_error)?
    };
    app.controller.trigger.notify_one();
    Ok(Json(view(&name, Some(&versions), &declared)))
}

#[utoipa::path(
    get, path = "/namespaces/{namespace}/secrets",
    params(("namespace" = String, Path)),
    responses((status = 200, description = "Stored and declared secrets of the namespace: keys, versions and what is missing; never values", body = [ProgramSecretView])),
    tag = "Secrets"
)]
pub async fn list_program_secrets(
    State(app): State<Arc<AppState>>,
    Path(namespace): Path<String>,
) -> Result<Json<Vec<ProgramSecretView>>, AppError> {
    check_names(&namespace, "x")?;
    let _intent = app.controller.intent.lock().await;
    let programs = store::load(&app.db)
        .and_then(|s| s.desired())
        .map_err(app_error)?;
    let stored = app
        .secret_store
        .program_versions(&namespace)
        .map_err(app_error)?;
    let declared = declarations(&programs, &namespace, None);
    let names: BTreeSet<&str> = stored
        .keys()
        .map(String::as_str)
        .chain(declared.iter().map(|(_, _, n, _)| *n))
        .collect();
    Ok(Json(
        names
            .into_iter()
            .map(|name| {
                let mine: Vec<_> = declared
                    .iter()
                    .filter(|(_, _, n, _)| *n == name)
                    .cloned()
                    .collect();
                view(name, stored.get(name), &mine)
            })
            .collect(),
    ))
}

#[utoipa::path(
    delete, path = "/namespaces/{namespace}/secrets/{name}",
    params(("namespace" = String, Path), ("name" = String, Path)),
    responses(
        (status = 200, description = "Every stored value of the secret is deleted", body = bool),
        (status = 404, description = "Nothing stored", body = String),
        (status = 409, description = "A stored program declares the secret", body = String)
    ), tag = "Secrets"
)]
pub async fn delete_program_secret(
    State(app): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<bool>, AppError> {
    check_names(&namespace, &name)?;
    let _intent = app.controller.intent.lock().await;
    let programs = store::load(&app.db)
        .and_then(|s| s.desired())
        .map_err(app_error)?;
    let declared = declarations(&programs, &namespace, Some(&name));
    if let Some((deployment, ..)) = declared.first() {
        return Err((
            StatusCode::CONFLICT,
            format!("secret {name:?} is declared by deployment {deployment:?}; remove it from the program first"),
        ));
    }
    if !app
        .secret_store
        .delete_program(&namespace, &name)
        .map_err(app_error)?
    {
        return Err((StatusCode::NOT_FOUND, format!("no stored secret {name:?}")));
    }
    Ok(Json(true))
}
