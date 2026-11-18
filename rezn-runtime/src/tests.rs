use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::{Method, StatusCode},
    Json,
};
use serde_json::{json, Value};

use common::ir_v2::{self, Kind};

use crate::{
    orqos_client::PodStatus,
    reconcile::{plan, Action, Owned},
    routes::apply::{apply_handler, ApplyPayload},
    store::{self, STATE_KEY},
    test_support::{app, signed, HttpRezn, MockOrqos, RecordedRequest, TOKEN},
    workload::{pod_id, translate, Workload, OWNER},
};

/// In-memory Orqos workload API: pods keyed by ID, reported the way Orqos reports them.
#[derive(Default)]
struct Engine {
    pods: BTreeMap<String, Value>,
    /// Owned volumes by name: their spec.
    volumes: BTreeMap<String, Value>,
    /// Owned secrets by name: their spec (`data` and `labels`).
    secrets: BTreeMap<String, Value>,
    fail_list: bool,
    fail_put: Option<&'static str>,
    /// State of newly applied pods; `running` when unset.
    start_state: Option<&'static str>,
    /// Newly applied pods run but are not ready (their readiness probes fail).
    start_unready: bool,
    /// The host's CPU millis and memory; 4 CPUs and 8 GiB when unset.
    capacity: Option<(u64, u64)>,
    fail_capacity: bool,
}

impl Engine {
    fn respond(&mut self, request: &RecordedRequest) -> (StatusCode, String) {
        if request.authorization.as_deref() != Some(&format!("Bearer {TOKEN}")[..]) {
            return (StatusCode::UNAUTHORIZED, "missing token".into());
        }
        let path = request.uri.path();
        if let Some(rest) = path.strip_prefix("/workload/volumes") {
            return self.volume(
                request.method.as_str(),
                rest.trim_start_matches('/'),
                &request.body,
            );
        }
        if path == "/workload/capacity" {
            if self.fail_capacity {
                return (StatusCode::SERVICE_UNAVAILABLE, "no capacity".into());
            }
            let (cpu, memory) = self.capacity.unwrap_or((4000, 8 << 30));
            return (
                StatusCode::OK,
                json!({"cpu_millis": cpu, "memory_bytes": memory}).to_string(),
            );
        }
        if let Some(rest) = path.strip_prefix("/workload/secrets") {
            return self.secret(
                request.method.as_str(),
                rest.trim_start_matches('/'),
                &request.body,
            );
        }
        let id = path.strip_prefix("/workload/pods/").unwrap_or_default();
        match (request.method.as_str(), path) {
            ("GET", "/workload/pods") if self.fail_list => (
                StatusCode::SERVICE_UNAVAILABLE,
                "systemd unavailable".into(),
            ),
            ("GET", "/workload/pods") => (
                StatusCode::OK,
                json!(self.pods.values().collect::<Vec<_>>()).to_string(),
            ),
            ("PUT", _) if !id.is_empty() => {
                if let Some(body) = self.fail_put {
                    return (StatusCode::CONFLICT, body.into());
                }
                let spec: Value = serde_json::from_slice(&request.body).unwrap();
                for volume in spec["volumes"].as_array().into_iter().flatten() {
                    if let Some(name) = volume["volume"].as_str() {
                        if !self.volumes.contains_key(name) {
                            return (
                                StatusCode::UNPROCESSABLE_ENTITY,
                                format!(r#"{{"error":"unknown volume {name}"}}"#),
                            );
                        }
                    }
                }
                for name in Self::secret_refs(&spec) {
                    if !self.secrets.contains_key(&name) {
                        return (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            format!(r#"{{"error":"unknown secret {name}"}}"#),
                        );
                    }
                }
                let containers: Vec<Value> = spec["containers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| json!({"name": c["name"], "kind": "container", "unit": format!("orqos-{id}-c-{}.service", c["name"].as_str().unwrap()),
                                    "active_state": "active", "sub_state": "running", "result": "success", "restarts": 0,
                                    "started": true, "ready": !self.start_unready, "probe_restarts": 0, "last_probe_failure": null}))
                    .collect();
                let state = self.start_state.unwrap_or("running");
                let pod = json!({
                    "id": id, "desired": "running", "state": state, "ready": state == "running" && !self.start_unready,
                    "pod": {"unit": format!("orqos-{id}-pod.service"), "active_state": "active", "sub_state": "running", "result": "success", "restarts": 0},
                    "containers": containers, "spec": spec, "error": null, "updated_at": "2026-11-18T00:00:00Z"
                });
                self.pods.insert(id.into(), pod.clone());
                (StatusCode::OK, pod.to_string())
            }
            ("DELETE", _) if self.pods.remove(id).is_some() => (StatusCode::OK, "{}".into()),
            ("DELETE", _) => (StatusCode::NOT_FOUND, r#"{"error":"no such pod"}"#.into()),
            _ => (StatusCode::NOT_FOUND, "unknown route".into()),
        }
    }

    /// Pods whose specs mount an owned volume.
    fn mounting(&self, name: &str) -> Vec<String> {
        self.pods
            .iter()
            .filter(|(_, pod)| {
                pod["spec"]["volumes"]
                    .as_array()
                    .is_some_and(|v| v.iter().any(|v| v["volume"] == name))
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn volume(&mut self, method: &str, name: &str, body: &[u8]) -> (StatusCode, String) {
        match (method, name) {
            ("GET", "") if self.fail_list => (StatusCode::SERVICE_UNAVAILABLE, "systemd unavailable".into()),
            ("GET", "") => (
                StatusCode::OK,
                json!(self
                    .volumes
                    .iter()
                    .map(|(name, spec)| json!({"name": name, "exists": true, "spec": spec, "pods": self.mounting(name)}))
                    .collect::<Vec<_>>())
                .to_string(),
            ),
            ("PUT", _) if !name.is_empty() => {
                let spec: Value = serde_json::from_slice(body).unwrap();
                self.volumes.insert(name.into(), spec);
                (StatusCode::OK, "{}".into())
            }
            ("DELETE", _) if !self.mounting(name).is_empty() => {
                (StatusCode::CONFLICT, r#"{"error":"mounted"}"#.into())
            }
            ("DELETE", _) if self.volumes.remove(name).is_some() => (StatusCode::OK, "{}".into()),
            ("DELETE", _) => (StatusCode::NOT_FOUND, r#"{"error":"no such volume"}"#.into()),
            _ => (StatusCode::NOT_FOUND, "unknown route".into()),
        }
    }

    /// Orqos secrets a pod spec uses.
    fn secret_refs(spec: &Value) -> Vec<String> {
        let files = spec["volumes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v["secret"]["name"].as_str());
        let env = spec["containers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["secret_env"].as_object())
            .flat_map(|e| e.values().filter_map(|r| r["secret"].as_str()));
        files.chain(env).map(String::from).collect()
    }

    fn secret(&mut self, method: &str, name: &str, body: &[u8]) -> (StatusCode, String) {
        let using = |engine: &Self| -> Vec<String> {
            engine
                .pods
                .iter()
                .filter(|(_, pod)| Self::secret_refs(&pod["spec"]).iter().any(|s| s == name))
                .map(|(id, _)| id.clone())
                .collect()
        };
        match (method, name) {
            ("GET", "") if self.fail_list => (StatusCode::SERVICE_UNAVAILABLE, "systemd unavailable".into()),
            ("GET", "") => (
                StatusCode::OK,
                json!(self
                    .secrets
                    .iter()
                    .map(|(n, spec)| {
                        let pods: Vec<String> = self
                            .pods
                            .iter()
                            .filter(|(_, pod)| Self::secret_refs(&pod["spec"]).contains(n))
                            .map(|(id, _)| id.clone())
                            .collect();
                        json!({"name": n, "keys": spec["data"].as_object().unwrap().keys().collect::<Vec<_>>(),
                               "labels": spec["labels"], "pods": pods})
                    })
                    .collect::<Vec<_>>())
                .to_string(),
            ),
            ("PUT", _) if !name.is_empty() => {
                let spec: Value = serde_json::from_slice(body).unwrap();
                if let Some(existing) = self.secrets.get(name) {
                    if existing["data"] != spec["data"] {
                        return (StatusCode::CONFLICT, r#"{"error":"immutable"}"#.into());
                    }
                }
                self.secrets.insert(name.into(), spec);
                (StatusCode::OK, "{}".into())
            }
            ("DELETE", _) if !using(self).is_empty() => {
                (StatusCode::CONFLICT, r#"{"error":"in use"}"#.into())
            }
            ("DELETE", _) if self.secrets.remove(name).is_some() => (StatusCode::OK, "{}".into()),
            ("DELETE", _) => (StatusCode::NOT_FOUND, r#"{"error":"no such secret"}"#.into()),
            _ => (StatusCode::NOT_FOUND, "unknown route".into()),
        }
    }

    fn set_state(&mut self, id: &str, state: &str) {
        let pod = self.pods.get_mut(id).unwrap();
        pod["state"] = json!(state);
        pod["ready"] = json!(state == "running");
    }

    fn set_ready(&mut self, id: &str, ready: bool) {
        let pod = self.pods.get_mut(id).unwrap();
        pod["ready"] = json!(ready);
        for c in pod["containers"].as_array_mut().unwrap() {
            c["ready"] = json!(ready);
        }
    }
}

async fn wait_for(notify: &tokio::sync::Notify) {
    tokio::time::timeout(std::time::Duration::from_secs(10), notify.notified())
        .await
        .expect("test synchronization timed out");
}

async fn backend() -> (MockOrqos, Arc<Mutex<Engine>>) {
    let engine = Arc::new(Mutex::new(Engine::default()));
    let captured = engine.clone();
    let server = MockOrqos::start(move |request| captured.lock().unwrap().respond(request)).await;
    (server, engine)
}

fn program(deployment: &str, resources: Value) -> Value {
    json!({"ir": "rezn.dev/v2", "deployment": deployment, "namespace": "apps", "resources": resources})
}

fn replica_set(name: &str, replicas: u32, level: &str) -> Value {
    json!({"kind": "replica_set", "name": name, "fields": {"replicas": replicas, "pod": {"containers": [
        {"name": "app", "image": "docker.io/library/alpine:3.22", "command": ["sleep", "infinity"],
         "env": [{"name": "LEVEL", "value": level}]}]}}})
}

fn ordered_set(name: &str, replicas: u32, management: &str) -> Value {
    json!({"kind": "ordered_replica_set", "name": name, "fields": {"replicas": replicas, "service": name,
        "pod_management": management, "pod": {"containers": [{"name": "db", "image": "docker.io/library/postgres:17"}]}}})
}

fn example_v2(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../examples/ir-v2")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

async fn apply(app: &Arc<crate::AppState>, name: &str, program: Value) {
    let response = apply_handler(
        State(app.clone()),
        Json(ApplyPayload {
            name: name.into(),
            instruction_wrapper: signed(program),
        }),
    )
    .await
    .unwrap();
    assert_eq!(response.0, StatusCode::ACCEPTED);
}

fn owner(app: &Arc<crate::AppState>) -> String {
    store::load(&app.db).unwrap().owner
}

fn id(app: &Arc<crate::AppState>, deployment: &str, kind: &str, resource: &str, n: u32) -> String {
    pod_id(&owner(app), deployment, kind, resource, n)
}

/// PUT and DELETE requests after the first `skip` requests, as (method, pod ID).
fn mutations(backend: &MockOrqos, skip: usize) -> Vec<(Method, String)> {
    backend.requests()[skip..]
        .iter()
        .filter(|r| r.method != Method::GET)
        .map(|r| {
            (
                r.method.clone(),
                r.uri.path().trim_start_matches("/workload/pods/").into(),
            )
        })
        .collect()
}

fn put(id: String) -> (Method, String) {
    (Method::PUT, id)
}

fn delete(id: String) -> (Method, String) {
    (Method::DELETE, id)
}

async fn reconcile(app: &Arc<crate::AppState>) -> anyhow::Result<()> {
    app.controller
        .reconcile(&app.db, &app.secret_store, &app.orqos)
        .await
}

async fn converged(app: &Arc<crate::AppState>) -> crate::reconcile::Status {
    let status = app
        .controller
        .status(&app.db, &app.secret_store)
        .await
        .unwrap();
    assert!(
        status.converged,
        "{:?} {:?}",
        status.errors, status.observation
    );
    status
}

#[tokio::test]
async fn apply_accepts_only_valid_signed_ir_v2_and_rejections_change_nothing() {
    let (backend, _) = backend().await;
    let app = app(backend.client.clone(), None);
    let http = HttpRezn::start(app.clone()).await;
    let client = reqwest::Client::new();
    let original = app.db.get(STATE_KEY).unwrap().unwrap();
    let minimal = example_v2("minimal.json");
    let post = |body: Value| {
        let request = client.post(format!("{}/apply", http.url)).json(&body);
        async move {
            let response = request.send().await.unwrap();
            (response.status(), response.text().await.unwrap())
        }
    };

    let v1 = json!([{"kind": "pod", "name": "web", "fields": {"image": "nginx", "replicas": 1, "ports": []}}]);
    let mut upper = minimal.clone();
    upper["deployment"] = json!("Hello");
    let mut dangling = minimal.clone();
    dangling["resources"][1]["fields"]["target"] = json!("nothing");
    let mut tampered = signed(minimal.clone());
    tampered.program["resources"][0]["fields"]["replicas"] = json!(3);
    for (name, wrapper, expected) in [
        (
            "web",
            json!(signed(v1)),
            "IR v1 instruction arrays are no longer executed",
        ),
        ("other", json!(signed(minimal.clone())), "does not match"),
        ("Hello", json!(signed(upper)), "lowercase"),
        (
            "hello",
            json!(signed(dangling)),
            "/resources/1/fields/target",
        ),
        ("hello", json!(tampered), "invalid program signature"),
        (
            "hello",
            json!(signed(json!("text"))),
            "IR v2 program object",
        ),
        (
            "hello",
            json!({"program": minimal, "signature": {"algorithm": "rsa", "pub": "", "sig": ""}}),
            "unsupported signature algorithm",
        ),
    ] {
        let (status, body) = post(json!({"name": name, "instruction_wrapper": wrapper})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains(expected), "{expected}: {body}");
    }
    let mut unknown = json!({"name": "hello", "instruction_wrapper": signed(minimal.clone())});
    unknown["instruction_wrapper"]["future"] = json!(1);
    assert_eq!(post(unknown).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(app.db.get(STATE_KEY).unwrap().unwrap(), original);
    assert!(backend.requests().is_empty());

    let (status, body) =
        post(json!({"name": "hello", "instruction_wrapper": signed(minimal.clone())})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({"stored": true, "revision": 1})
    );
    let stored = store::load(&app.db).unwrap();
    assert_eq!(stored.format, store::FORMAT_V2);
    assert_eq!(
        stored.deployments["hello"].meta.instructions,
        vec![
            ("replica_set".into(), "web".into()),
            ("service".into(), "web".into())
        ]
    );
    let desired: Value = reqwest::get(format!("{}/state", http.url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(desired, json!({"hello": minimal}));
}

#[tokio::test]
async fn lifecycle_create_reapply_scale_rolling_replace_and_remove() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    let web = |n| id(&app, "shop", "replica_set", "web", n);

    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 2, "a")])),
    )
    .await;
    reconcile(&app).await.unwrap();
    assert_eq!(mutations(&backend, 0), vec![put(web(0)), put(web(1))]);
    let status = converged(&app).await;
    let row = &status.workloads[0];
    assert_eq!(
        (
            row.kind.as_str(),
            row.desired_replicas,
            row.running_replicas
        ),
        ("replica_set", Some(2), Some(2))
    );
    assert_eq!(
        row.pods
            .iter()
            .map(|p| (p.hostname.as_str(), p.current))
            .collect::<Vec<_>>(),
        vec![("web-0", true), ("web-1", true)]
    );
    let sent = engine.lock().unwrap().pods[&web(1)]["spec"].clone();
    assert_eq!(sent["labels"][OWNER], json!(owner(&app)));
    assert_eq!(sent["labels"]["dev.rezn.ordinal"], "1");
    assert_eq!(sent["containers"][0]["env"], json!({"LEVEL": "a"}));

    // A new revision with the same program changes nothing.
    let before = backend.requests().len();
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 2, "a")])),
    )
    .await;
    reconcile(&app).await.unwrap();
    assert!(mutations(&backend, before).is_empty());
    converged(&app).await;

    let before = backend.requests().len();
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 3, "a")])),
    )
    .await;
    reconcile(&app).await.unwrap();
    assert_eq!(mutations(&backend, before), vec![put(web(2))]);

    let before = backend.requests().len();
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 1, "a")])),
    )
    .await;
    reconcile(&app).await.unwrap();
    assert_eq!(
        mutations(&backend, before),
        vec![delete(web(1)), delete(web(2))]
    );
    converged(&app).await;

    // A changed spec replaces one running replica per pass, highest ordinal first.
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 2, "a")])),
    )
    .await;
    reconcile(&app).await.unwrap();
    let before = backend.requests().len();
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 2, "b")])),
    )
    .await;
    let error = reconcile(&app).await.unwrap_err();
    assert!(error.to_string().contains("not converged"), "{error:#}");
    assert_eq!(mutations(&backend, before), vec![put(web(1))]);
    let status = app
        .controller
        .status(&app.db, &app.secret_store)
        .await
        .unwrap();
    assert_eq!(
        status.workloads[0]
            .pods
            .iter()
            .map(|p| p.current)
            .collect::<Vec<_>>(),
        vec![false, true]
    );
    let before = backend.requests().len();
    reconcile(&app).await.unwrap();
    assert_eq!(mutations(&backend, before), vec![put(web(0))]);
    converged(&app).await;
    assert_eq!(
        engine.lock().unwrap().pods[&web(0)]["spec"]["containers"][0]["env"],
        json!({"LEVEL": "b"})
    );

    // An empty program removes the deployment's pods; other deployments stay.
    apply(
        &app,
        "other",
        program("other", json!([replica_set("api", 1, "a")])),
    )
    .await;
    reconcile(&app).await.unwrap();
    let before = backend.requests().len();
    apply(&app, "shop", program("shop", json!([]))).await;
    reconcile(&app).await.unwrap();
    assert_eq!(
        mutations(&backend, before),
        vec![delete(web(0)), delete(web(1))]
    );
    let status = converged(&app).await;
    assert_eq!(status.workloads.len(), 1);
    assert_eq!(status.workloads[0].deployment, "other");
}

fn headless(name: &str) -> Value {
    json!({"kind": "service", "name": name, "fields": {"target": name, "headless": true,
        "ports": [{"name": "pg", "port": 5432, "target_port": 5432}]}})
}

/// The translation of an ordered replica set with its headless service, for checking the planner directly.
fn ordered_workload(replicas: u32, management: &str) -> Workload {
    let program = ir_v2::validate(&program(
        "data",
        json!([ordered_set("db", replicas, management), headless("db")]),
    ))
    .unwrap();
    let workloads = translate(&"0".repeat(64), &program, &Default::default())
        .unwrap()
        .workloads;
    assert_eq!(workloads[0].kind, Kind::OrderedReplicaSet);
    workloads.into_iter().next().unwrap()
}

fn observed(workload: &Workload, states: &[&str]) -> Vec<Owned> {
    states
        .iter()
        .zip(&workload.pods)
        .map(|(state, pod)| Owned {
            deployment: workload.deployment.clone(),
            kind: workload.kind,
            resource: workload.resource.clone(),
            ordinal: pod.ordinal,
            status: PodStatus {
                id: pod.id.clone(),
                // "unready" is a running pod whose readiness probes fail.
                state: if *state == "unready" {
                    "running"
                } else {
                    state
                }
                .to_string(),
                ready: *state == "running",
                containers: vec![],
                spec: pod.spec.clone(),
                error: None,
            },
        })
        .collect()
}

#[test]
fn planner_orders_sequential_sets_and_rolls_replacements() {
    let none = BTreeMap::new();
    let three = ordered_workload(3, "ordered");
    let id = |n: usize| three.pods[n].id.clone();
    let create = |n: usize| Action::Apply(id(n), three.pods[n].spec.clone());
    let w = std::slice::from_ref(&three);
    assert_eq!(plan(w, &none, &[]), vec![create(0)]);
    assert!(plan(w, &none, &observed(&three, &["starting"])).is_empty());
    assert_eq!(
        plan(w, &none, &observed(&three, &["running"])),
        vec![create(1)]
    );
    assert!(plan(
        w,
        &none,
        &observed(&three, &["running", "running", "running"])
    )
    .is_empty());

    // Scale-down removes the highest ordinal, once the others run.
    let one = ordered_workload(1, "ordered");
    let w = std::slice::from_ref(&one);
    assert_eq!(
        plan(
            w,
            &none,
            &observed(&three, &["running", "running", "running"])
        ),
        vec![Action::Remove(id(2))]
    );
    assert!(plan(
        w,
        &none,
        &observed(&three, &["running", "starting", "running"])
    )
    .is_empty());
    assert_eq!(
        plan(w, &none, &observed(&three, &["running", "running"])),
        vec![Action::Remove(id(1))]
    );

    // Parallel management creates and removes together.
    let parallel = ordered_workload(3, "parallel");
    assert_eq!(plan(std::slice::from_ref(&parallel), &none, &[]).len(), 3);

    // A changed spec: non-running pods are replaced at once, a running one only while the rest run.
    let mut changed = three.clone();
    for pod in &mut changed.pods {
        pod.spec.grace_seconds = 5;
    }
    let replace = |n: usize| Action::Apply(id(n), changed.pods[n].spec.clone());
    let w = std::slice::from_ref(&changed);
    assert_eq!(
        plan(
            w,
            &none,
            &observed(&three, &["running", "running", "running"])
        ),
        vec![replace(2)]
    );
    assert_eq!(
        plan(
            w,
            &none,
            &observed(&three, &["running", "failed", "running"])
        ),
        vec![replace(1)]
    );

    // A stopped pod with the current spec is applied again; pods of unexecuted deployments are left alone.
    let w = std::slice::from_ref(&three);
    assert_eq!(
        plan(
            w,
            &none,
            &observed(&three, &["running", "stopped", "running"])
        ),
        vec![create(1)]
    );
    let unexecuted = BTreeMap::from([("data".to_string(), vec!["reason".to_string()])]);
    assert!(plan(&[], &unexecuted, &observed(&three, &["running", "failed"])).is_empty());
    assert_eq!(
        plan(&[], &none, &observed(&three, &["running"])),
        vec![Action::Remove(id(0))]
    );
}

#[tokio::test]
async fn unexecuted_deployments_report_reasons_and_keep_their_pods() {
    let (backend, _) = backend().await;
    let app = app(backend.client.clone(), None);
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 1, "a")])),
    )
    .await;
    reconcile(&app).await.unwrap();

    let before = backend.requests().len();
    let mut limited = replica_set("hello", 1, "a");
    limited["fields"]["pod"]["containers"][0]["pull"] = json!("always");
    apply(&app, "hello", program("hello", json!([limited]))).await;
    let service = json!({"kind": "service", "name": "db", "fields": {"target": "db", "headless": true,
        "ports": [{"name": "pg", "port": 5433, "target_port": 5432}]}});
    apply(
        &app,
        "data",
        program("data", json!([ordered_set("db", 1, "ordered"), service])),
    )
    .await;
    reconcile(&app).await.unwrap();
    assert!(mutations(&backend, before).is_empty());
    let status = app
        .controller
        .status(&app.db, &app.secret_store)
        .await
        .unwrap();
    assert!(!status.converged);
    assert!(status.errors.is_empty());
    assert_eq!(status.unexecuted.len(), 2);
    assert_eq!(status.unexecuted[0].deployment, "data");
    assert_eq!(
        status.unexecuted[0].reasons,
        vec!["/resources/1/fields/ports/0: services whose port differs from the target port are not executed yet (rezn#123)"]
    );
    assert_eq!(status.unexecuted[1].deployment, "hello");
    assert_eq!(
        status.unexecuted[1].reasons,
        vec![
            "/resources/0/fields/pod/containers/0/pull: pull policies other than if_not_present are not executed yet (orqos#23)",
        ]
    );

    // A running deployment whose new program cannot run keeps its pods from the earlier revision.
    let mut limited = replica_set("web", 3, "b");
    limited["fields"]["pod"]["containers"][0]["pull"] = json!("never");
    let before = backend.requests().len();
    apply(&app, "shop", program("shop", json!([limited]))).await;
    reconcile(&app).await.unwrap();
    assert!(mutations(&backend, before).is_empty());
    let status = app
        .controller
        .status(&app.db, &app.secret_store)
        .await
        .unwrap();
    let shop = status
        .workloads
        .iter()
        .find(|w| w.deployment == "shop")
        .unwrap();
    assert_eq!((shop.desired_replicas, shop.pods.len()), (None, 1));
    assert!(!shop.pods[0].current);
    assert_eq!(status.unexecuted.len(), 3);

    let http = HttpRezn::start(app.clone()).await;
    let body: Value = reqwest::get(format!("{}/status", http.url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["unexecuted"][2]["deployment"], "shop");
    assert!(body["unexecuted"][2]["reasons"][0]
        .as_str()
        .unwrap()
        .contains("/pull: pull policies"));
}

#[tokio::test]
async fn services_become_aliases_and_adding_one_rolls_the_target() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    let web = |n| id(&app, "shop", "replica_set", "web", n);
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 2, "a")])),
    )
    .await;
    reconcile(&app).await.unwrap();
    let network = crate::workload::network_name(&owner(&app), "apps");
    let sent = engine.lock().unwrap().pods[&web(0)]["spec"].clone();
    assert_eq!(sent["network"], json!({"name": network, "aliases": []}));

    let service = json!({"kind": "service", "name": "web", "fields": {"target": "web",
        "ports": [{"name": "http", "port": 80, "target_port": 80}]}});
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 2, "a"), service])),
    )
    .await;
    // One running replica at a time, highest first.
    let before = backend.requests().len();
    reconcile(&app).await.unwrap_err();
    assert_eq!(mutations(&backend, before), vec![put(web(1))]);
    let before = backend.requests().len();
    reconcile(&app).await.unwrap();
    assert_eq!(mutations(&backend, before), vec![put(web(0))]);
    let status = converged(&app).await;
    for pod in &status.workloads[0].pods {
        assert_eq!(pod.aliases, ["web", "web.apps", "web.apps.svc"]);
    }
    assert_eq!(
        engine.lock().unwrap().pods[&web(1)]["spec"]["network"]["name"],
        json!(network)
    );
}

#[tokio::test]
async fn ordered_sets_with_headless_services_start_one_replica_at_a_time() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    let db = |n| id(&app, "data", "ordered_replica_set", "db", n);
    engine.lock().unwrap().start_state = Some("starting");
    apply(
        &app,
        "data",
        program(
            "data",
            json!([ordered_set("db", 2, "ordered"), headless("db")]),
        ),
    )
    .await;
    let _ = reconcile(&app).await;
    assert_eq!(mutations(&backend, 0), vec![put(db(0))]);
    let before = backend.requests().len();
    let _ = reconcile(&app).await;
    assert!(
        mutations(&backend, before).is_empty(),
        "replica 1 waits for replica 0"
    );
    engine.lock().unwrap().set_state(&db(0), "running");
    let before = backend.requests().len();
    let _ = reconcile(&app).await;
    assert_eq!(mutations(&backend, before), vec![put(db(1))]);
    engine.lock().unwrap().set_state(&db(1), "running");
    reconcile(&app).await.unwrap();
    let status = converged(&app).await;
    assert_eq!(
        status.workloads[0].pods[1].aliases,
        [
            "db",
            "db-1.db",
            "db-1.db.apps",
            "db-1.db.apps.svc",
            "db.apps",
            "db.apps.svc"
        ]
    );
}

fn stateful(replicas: u32, on_removal: bool) -> Value {
    program(
        "data",
        json!([
            {"kind": "volume_claim", "name": "shared", "fields": {"bytes": 1048576}},
            {"kind": "ordered_replica_set", "name": "db", "fields": {"replicas": replicas, "service": "db",
                "pod_management": "parallel",
                "volume_claims": [{"name": "data", "bytes": 1048576}],
                "retain": {"on_scale_down": false, "on_removal": on_removal},
                "pod": {"user": 1000, "fs_group": 2000,
                    "volumes": [{"name": "data", "claim": "data"}, {"name": "shared", "claim": "shared"}],
                    "containers": [{"name": "db", "image": "docker.io/library/postgres:17",
                        "mounts": [{"volume": "data", "path": "/data"}, {"volume": "shared", "path": "/shared"}]}]}}},
            headless("db")
        ]),
    )
}

/// Every PUT and DELETE after the first `skip` requests, as (method, path).
fn changes(backend: &MockOrqos, skip: usize) -> Vec<(Method, String)> {
    backend.requests()[skip..]
        .iter()
        .filter(|r| r.method != Method::GET)
        .map(|r| (r.method.clone(), r.uri.path().to_string()))
        .collect()
}

#[tokio::test]
async fn volumes_come_before_pods_and_follow_their_retention() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    let owner = owner(&app);
    let db = |n| {
        format!(
            "/workload/pods/{}",
            id(&app, "data", "ordered_replica_set", "db", n)
        )
    };
    let data = |n| {
        format!(
            "/workload/volumes/{}",
            crate::workload::volume_id(&owner, "data", Some("db"), "data", Some(n))
        )
    };
    let shared = format!(
        "/workload/volumes/{}",
        crate::workload::volume_id(&owner, "data", None, "shared", None)
    );

    apply(&app, "data", stateful(2, true)).await;
    reconcile(&app).await.unwrap();
    assert_eq!(
        changes(&backend, 0),
        [
            (Method::PUT, shared.clone()),
            (Method::PUT, data(0)),
            (Method::PUT, data(1)),
            (Method::PUT, db(0)),
            (Method::PUT, db(1)),
        ]
    );
    let status = converged(&app).await;
    assert_eq!(status.volumes.len(), 3);
    assert!(status
        .volumes
        .iter()
        .all(|v| v.desired && v.exists && v.retain));
    assert_eq!(
        engine.lock().unwrap().volumes[data(1).trim_start_matches("/workload/volumes/")]["group"],
        2000
    );

    // Scale down: the pod goes first; its volume (on_scale_down: false) in the same pass, once nothing mounts it.
    apply(&app, "data", stateful(1, true)).await;
    let before = backend.requests().len();
    reconcile(&app).await.unwrap();
    assert_eq!(
        changes(&backend, before),
        [(Method::DELETE, db(1)), (Method::DELETE, data(1))]
    );
    converged(&app).await;

    // Scale up creates the volume again, before the pod.
    apply(&app, "data", stateful(2, true)).await;
    let before = backend.requests().len();
    reconcile(&app).await.unwrap();
    assert_eq!(
        changes(&backend, before),
        [(Method::PUT, data(1)), (Method::PUT, db(1))]
    );

    // Removing the deployment keeps retained volumes (the claim always; replicas with on_removal: true).
    apply(&app, "data", program("data", json!([]))).await;
    reconcile(&app).await.unwrap();
    let before = backend.requests().len();
    reconcile(&app).await.unwrap();
    assert!(changes(&backend, before).is_empty());
    let status = converged(&app).await;
    assert_eq!(status.volumes.len(), 3);
    assert!(status
        .volumes
        .iter()
        .all(|v| !v.desired && v.retain && v.mounted_by.is_empty()));

    // Re-applying reuses them; with on_removal: false, removal then deletes the per-replica volumes only.
    apply(&app, "data", stateful(2, false)).await;
    let before = backend.requests().len();
    reconcile(&app).await.unwrap();
    let puts = changes(&backend, before);
    assert_eq!(
        puts.iter()
            .filter(|(_, p)| p.starts_with("/workload/pods/"))
            .count(),
        2
    );
    apply(&app, "data", program("data", json!([]))).await;
    let before = backend.requests().len();
    reconcile(&app).await.unwrap();
    let mut deleted: Vec<String> = changes(&backend, before)
        .into_iter()
        .map(|(method, path)| {
            assert_eq!(method, Method::DELETE);
            path
        })
        .collect();
    deleted.sort();
    // One pass removes the pods and then the volumes they released.
    let mut expected = vec![db(0), db(1), data(0), data(1)];
    expected.sort();
    assert_eq!(deleted, expected);
    let status = converged(&app).await;
    assert_eq!(status.volumes.len(), 1);
    assert_eq!(status.volumes[0].claim, "shared");
}

#[tokio::test]
async fn colliding_names_in_a_shared_namespace_are_refused_or_left_unexecuted() {
    let (backend, _) = backend().await;
    let app = app(backend.client.clone(), None);
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 1, "a")])),
    )
    .await;
    reconcile(&app).await.unwrap();
    let revision = store::load(&app.db).unwrap().revision;

    let refused = apply_handler(
        State(app.clone()),
        Json(ApplyPayload {
            name: "other".into(),
            instruction_wrapper: signed(program("other", json!([replica_set("web", 1, "b")]))),
        }),
    )
    .await
    .err()
    .expect("a colliding program is refused");
    assert_eq!(refused.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.1,
        "/resources/0/name: \"web\" is also a workload name in deployment \"shop\" of namespace \"apps\""
    );
    assert_eq!(store::load(&app.db).unwrap().revision, revision);

    // The same names in another namespace are accepted. State written before the check, with both in one
    // namespace, leaves both deployments unexecuted and their pods in place.
    let mut elsewhere = program("other", json!([replica_set("web", 1, "b")]));
    elsewhere["namespace"] = json!("elsewhere");
    apply(&app, "other", elsewhere).await;
    let mut state: Value =
        serde_json::from_slice(&app.db.get(STATE_KEY).unwrap().unwrap()).unwrap();
    let colliding = signed(program("other", json!([replica_set("web", 1, "b")])));
    state["deployments"]["other"]["meta"]["sig_id"] = json!(colliding.signature.sig);
    state["deployments"]["other"]["envelope"] = json!(colliding);
    app.db
        .insert(STATE_KEY, serde_json::to_vec(&state).unwrap())
        .unwrap();
    let before = backend.requests().len();
    let _ = reconcile(&app).await;
    assert!(mutations(&backend, before).is_empty());
    let status = app
        .controller
        .status(&app.db, &app.secret_store)
        .await
        .unwrap();
    let unexecuted: Vec<(&str, &str)> = status
        .unexecuted
        .iter()
        .map(|u| (u.deployment.as_str(), u.reasons[0].as_str()))
        .collect();
    assert_eq!(
        unexecuted,
        [
            ("other", "/resources/0/name: \"web\" is also a workload name in deployment \"shop\" of namespace \"apps\""),
            ("shop", "/resources/0/name: \"web\" is also a workload name in deployment \"other\" of namespace \"apps\""),
        ]
    );
    assert_eq!(status.workloads[0].pods.len(), 1);
}

#[tokio::test]
async fn restart_recovery_and_stopped_or_failed_pods() {
    let (backend, engine) = backend().await;
    let first = app(backend.client.clone(), None);
    apply(
        &first,
        "shop",
        program("shop", json!([replica_set("web", 2, "a")])),
    )
    .await;
    reconcile(&first).await.unwrap();

    // A restarted Rezn (new controller, same database) adopts nothing new and changes nothing.
    let app = app(backend.client.clone(), Some(first.db.as_ref().clone()));
    assert_eq!(
        app.controller
            .status(&app.db, &app.secret_store)
            .await
            .unwrap()
            .observation,
        "unknown"
    );
    let before = backend.requests().len();
    reconcile(&app).await.unwrap();
    assert!(mutations(&backend, before).is_empty());
    converged(&app).await;

    // A pod that is stopped (for example after a host reboot) is applied again, which starts it.
    let web0 = id(&app, "shop", "replica_set", "web", 0);
    let web1 = id(&app, "shop", "replica_set", "web", 1);
    engine.lock().unwrap().set_state(&web0, "stopped");
    let before = backend.requests().len();
    reconcile(&app).await.unwrap();
    assert_eq!(mutations(&backend, before), vec![put(web0)]);

    // Failing and restarting pods are systemd's to restart: reported, not replaced.
    engine.lock().unwrap().set_state(&web1, "failed");
    let before = backend.requests().len();
    assert!(reconcile(&app).await.is_err());
    assert!(mutations(&backend, before).is_empty());
    let status = app
        .controller
        .status(&app.db, &app.secret_store)
        .await
        .unwrap();
    assert_eq!(status.workloads[0].running_replicas, Some(1));
    assert_eq!(status.workloads[0].pods[1].state, "failed");
    assert!(!status.converged);
}

#[tokio::test]
async fn foreign_pods_are_untouched_and_unproven_ownership_blocks_every_change() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    let foreign = |id: &str, labels: Value| {
        json!({"id": id, "state": "running", "ready": true, "containers": [], "error": null,
               "spec": {"hostname": "x", "restart": "always", "grace_seconds": 30, "labels": labels,
                        "containers": [{"name": "c", "image": "alpine"}]}})
    };
    engine.lock().unwrap().pods.insert(
        "other-owner".into(),
        foreign(
            "other-owner",
            json!({OWNER: "f".repeat(64), "dev.rezn.deployment": "shop"}),
        ),
    );
    engine
        .lock()
        .unwrap()
        .pods
        .insert("plain".into(), foreign("plain", json!({})));
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 1, "a")])),
    )
    .await;
    reconcile(&app).await.unwrap();
    converged(&app).await;
    apply(&app, "shop", program("shop", json!([]))).await;
    reconcile(&app).await.unwrap();
    assert!(engine.lock().unwrap().pods.contains_key("other-owner"));
    assert!(engine.lock().unwrap().pods.contains_key("plain"));

    let ours = owner(&app);
    let labels = json!({OWNER: ours, "dev.rezn.deployment": "shop", "dev.rezn.namespace": "apps",
                        "dev.rezn.kind": "replica_set", "dev.rezn.resource": "web", "dev.rezn.ordinal": "0"});
    let mut missing = labels.clone();
    missing.as_object_mut().unwrap().remove("dev.rezn.resource");
    let mut padded = labels.clone();
    padded["dev.rezn.ordinal"] = json!("00");
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 2, "a")])),
    )
    .await;
    for (pod_id, labels, expected) in [
        ("rz-forged", labels.clone(), "does not match its labels"),
        (
            "rz-missing",
            missing,
            "lacks ownership label dev.rezn.resource",
        ),
        ("rz-padded", padded, "malformed ordinal"),
    ] {
        engine
            .lock()
            .unwrap()
            .pods
            .insert(pod_id.into(), foreign(pod_id, labels));
        let before = backend.requests().len();
        let error = format!("{:#}", reconcile(&app).await.unwrap_err());
        assert!(error.contains(expected), "{expected}: {error}");
        assert!(mutations(&backend, before).is_empty());
        assert_eq!(
            app.controller
                .status(&app.db, &app.secret_store)
                .await
                .unwrap()
                .observation,
            "stale"
        );
        engine.lock().unwrap().pods.remove(pod_id);
    }
    reconcile(&app).await.unwrap();
    converged(&app).await;
}

#[tokio::test]
async fn backend_failures_propagate_and_the_next_pass_recovers() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 2, "a")])),
    )
    .await;

    engine.lock().unwrap().fail_list = true;
    let error = format!("{:#}", reconcile(&app).await.unwrap_err());
    assert!(
        error.contains("required workload observation failed") && error.contains("HTTP 503"),
        "{error}"
    );
    assert!(mutations(&backend, 0).is_empty());
    let status = app
        .controller
        .status(&app.db, &app.secret_store)
        .await
        .unwrap();
    assert!(!status.converged && status.workloads[0].running_replicas.is_none());

    // Every pod is attempted, and each failure keeps Orqos's status and message.
    engine.lock().unwrap().fail_list = false;
    engine.lock().unwrap().fail_put = Some(r#"{"error":"units exist without ownership"}"#);
    let before = backend.requests().len();
    let error = format!("{:#}", reconcile(&app).await.unwrap_err());
    assert_eq!(mutations(&backend, before).len(), 2);
    assert!(
        error.contains("HTTP 409") && error.contains("units exist without ownership"),
        "{error}"
    );
    let status = app
        .controller
        .status(&app.db, &app.secret_store)
        .await
        .unwrap();
    assert_eq!(status.errors, vec![error]);
    assert_eq!(status.observation, "stale");

    engine.lock().unwrap().fail_put = None;
    reconcile(&app).await.unwrap();
    assert!(converged(&app).await.errors.is_empty());
}

#[tokio::test]
async fn failed_follow_up_observation_keeps_a_stale_snapshot_and_retry_does_not_duplicate() {
    let engine = Arc::new(Mutex::new(Engine::default()));
    let captured = engine.clone();
    let lists = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = lists.clone();
    let backend = MockOrqos::start(move |request| {
        if request.method == Method::GET
            && request.uri.path() == "/workload/pods"
            && counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1
        {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "follow-up listing failed".into(),
            )
        } else {
            captured.lock().unwrap().respond(request)
        }
    })
    .await;
    let app = app(backend.client.clone(), None);
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 1, "a")])),
    )
    .await;
    let error = format!("{:#}", reconcile(&app).await.unwrap_err());
    assert!(error.contains("follow-up observation"), "{error}");
    let stale = app
        .controller
        .status(&app.db, &app.secret_store)
        .await
        .unwrap();
    assert_eq!(stale.observation, "stale");
    assert_eq!(stale.workloads[0].running_replicas, Some(0));
    let before = backend.requests().len();
    reconcile(&app).await.unwrap();
    assert!(mutations(&backend, before).is_empty());
    assert_eq!(engine.lock().unwrap().pods.len(), 1);
    converged(&app).await;
}

#[tokio::test]
async fn older_observation_cannot_report_convergence_of_newer_intent_and_passes_are_serial() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (e, r) = (entered.clone(), release.clone());
    let backend = MockOrqos::start_async(move |request| {
        let (e, r) = (e.clone(), r.clone());
        // Pod listings block until released; volume listings answer at once.
        let blocks = request.uri.path() == "/workload/pods";
        let capacity = request.uri.path() == "/workload/capacity";
        async move {
            if blocks {
                e.notify_one();
                wait_for(&r).await;
            }
            if capacity {
                return (
                    StatusCode::OK,
                    r#"{"cpu_millis": 4000, "memory_bytes": 8589934592}"#.into(),
                );
            }
            (StatusCode::OK, "[]".into())
        }
    })
    .await;
    let app = app(backend.client.clone(), None);
    apply(&app, "shop", program("shop", json!([]))).await;
    let first_app = app.clone();
    let first = tokio::spawn(async move { reconcile(&first_app).await });
    wait_for(&entered).await;
    let second_app = app.clone();
    let second = tokio::spawn(async move { reconcile(&second_app).await });
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 0, "a")])),
    )
    .await;
    release.notify_one();
    assert!(first.await.unwrap().is_err());
    let pending = app
        .controller
        .status(&app.db, &app.secret_store)
        .await
        .unwrap();
    assert!(!pending.converged);
    assert_ne!(pending.observed_revision, Some(pending.desired_revision));
    wait_for(&entered).await;
    let pod_listings = |b: &MockOrqos| {
        b.requests()
            .iter()
            .filter(|r| r.uri.path() == "/workload/pods")
            .count()
    };
    assert_eq!(pod_listings(&backend), 2);
    release.notify_one();
    second.await.unwrap().unwrap();
    assert!(
        app.controller
            .status(&app.db, &app.secret_store)
            .await
            .unwrap()
            .converged
    );
}

#[tokio::test]
async fn successful_older_pass_during_apply_cannot_claim_newer_convergence() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let engine = Arc::new(Mutex::new(Engine::default()));
    let (e, r, captured) = (entered.clone(), release.clone(), engine.clone());
    let backend = MockOrqos::start_async(move |request| {
        let (e, r, captured) = (e.clone(), r.clone(), captured.clone());
        async move {
            if request.method == Method::PUT {
                e.notify_one();
                wait_for(&r).await;
            }
            captured.lock().unwrap().respond(&request)
        }
    })
    .await;
    let app = app(backend.client.clone(), None);
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 1, "a")])),
    )
    .await;
    let pass_app = app.clone();
    let pass = tokio::spawn(async move { reconcile(&pass_app).await });
    wait_for(&entered).await;
    apply(&app, "shop", program("shop", json!([]))).await;
    release.notify_one();
    pass.await.unwrap().unwrap();
    let pending = app
        .controller
        .status(&app.db, &app.secret_store)
        .await
        .unwrap();
    assert_eq!(pending.observation, "pending");
    assert!(!pending.converged);
    assert_eq!(pending.workloads[0].desired_replicas, None);
    assert_eq!(pending.workloads[0].running_replicas, Some(1));
    reconcile(&app).await.unwrap();
    assert!(converged(&app).await.workloads.is_empty());
}

#[tokio::test]
async fn apply_wakes_the_worker_without_waiting_for_the_periodic_interval() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    let worker = tokio::spawn(crate::reconcile::run(
        app.clone(),
        std::time::Duration::from_secs(3600),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !app
            .controller
            .status(&app.db, &app.secret_store)
            .await
            .unwrap()
            .converged
        {
            tokio::task::yield_now().await;
        }
        apply(
            &app,
            "shop",
            program("shop", json!([replica_set("web", 1, "a")])),
        )
        .await;
        while !app
            .controller
            .status(&app.db, &app.secret_store)
            .await
            .unwrap()
            .converged
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(engine.lock().unwrap().pods.len(), 1);
    worker.abort();
}

#[tokio::test]
async fn corrupt_missing_legacy_and_ir_v1_state_never_become_deletion_plans() {
    let (backend, _) = backend().await;
    let app = app(backend.client.clone(), None);
    let http = HttpRezn::start(app.clone()).await;
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 1, "a")])),
    )
    .await;
    let original: Value = serde_json::from_slice(&app.db.get(STATE_KEY).unwrap().unwrap()).unwrap();
    let mut tampered = original.clone();
    tampered["deployments"]["shop"]["envelope"]["program"]["resources"][0]["fields"]["replicas"] =
        json!(5);
    let mut wrong_format = original.clone();
    wrong_format["format"] = json!(3);
    let mut missing_deployments = original.clone();
    missing_deployments["deployments"] = json!({});
    let mut as_format_1 = original.clone();
    as_format_1["format"] = json!(1);
    let v1 = signed(
        json!([{"kind": "pod", "name": "web", "fields": {"image": "nginx", "replicas": 1, "ports": []}}]),
    );
    let mut ir_v1 = original.clone();
    ir_v1["deployments"]["shop"]["envelope"] = json!(v1);
    ir_v1["deployments"]["shop"]["meta"]["sig_id"] = json!(v1.signature.sig);
    ir_v1["deployments"]["shop"]["meta"]["instructions"] = json!([["pod", "web"]]);
    for (bytes, expected) in [
        (b"not JSON".to_vec(), "corrupt controller state JSON"),
        (
            serde_json::to_vec(&tampered).unwrap(),
            "invalid program signature",
        ),
        (
            serde_json::to_vec(&wrong_format).unwrap(),
            "invalid controller format",
        ),
        (
            serde_json::to_vec(&missing_deployments).unwrap(),
            "inconsistent controller revision",
        ),
        (
            serde_json::to_vec(&as_format_1).unwrap(),
            "format 1 controller state contains an IR v2 program",
        ),
        (
            serde_json::to_vec(&ir_v1).unwrap(),
            "holds an IR v1 program",
        ),
    ] {
        app.db.insert(STATE_KEY, bytes.clone()).unwrap();
        let error = format!("{:#}", reconcile(&app).await.unwrap_err());
        assert!(error.contains(expected), "{expected}: {error}");
        assert!(store::StoredState::decode(&bytes).is_err());
        for path in ["/state", "/state/raw", "/status"] {
            assert_eq!(
                reqwest::get(format!("{}{path}", http.url))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::INTERNAL_SERVER_ERROR
            );
        }
        let (status, _) = apply_handler(
            State(app.clone()),
            Json(ApplyPayload {
                name: "shop".into(),
                instruction_wrapper: signed(program("shop", json!([]))),
            }),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(app.db.get(STATE_KEY).unwrap().unwrap(), bytes);
    }
    app.db.remove(STATE_KEY).unwrap();
    assert!(reconcile(&app).await.is_err());
    assert!(backend
        .requests()
        .iter()
        .all(|r| r.uri.path() == "/workload/capacity"));
    let legacy = sled::Config::new().temporary(true).open().unwrap();
    legacy.insert("desired", b"{}").unwrap();
    assert!(store::initialize(&legacy)
        .unwrap_err()
        .to_string()
        .contains("no migration"));
}

#[tokio::test]
async fn an_empty_format_1_database_is_upgraded_by_the_first_apply() {
    let (backend, _) = backend().await;
    let db = sled::Config::new().temporary(true).open().unwrap();
    db.insert(
        STATE_KEY,
        serde_json::to_vec(
            &json!({"format": 1, "owner": "a".repeat(64), "revision": 0, "deployments": {}}),
        )
        .unwrap(),
    )
    .unwrap();
    let app = app(backend.client.clone(), Some(db));
    assert_eq!(store::load(&app.db).unwrap().format, store::FORMAT_V1);
    apply(&app, "shop", program("shop", json!([]))).await;
    let state = store::load(&app.db).unwrap();
    assert_eq!(
        (state.format, state.owner.as_str()),
        (store::FORMAT_V2, &*"a".repeat(64))
    );
    let fresh = sled::Config::new().temporary(true).open().unwrap();
    store::initialize(&fresh).unwrap();
    assert_eq!(store::load(&fresh).unwrap().format, store::FORMAT_V2);
}

#[tokio::test]
async fn openapi_and_http_status_describe_acceptance_observation_and_pods() {
    let (backend, _) = backend().await;
    let app = app(backend.client.clone(), None);
    let http = HttpRezn::start(app.clone()).await;
    let client = reqwest::Client::new();
    let get = |path: &str| {
        let request = client.get(format!("{}{path}", http.url));
        async move { request.send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let doc = get("/api/openapi.json").await;
    for (path, method, code) in [
        ("/apply", "post", "202"),
        ("/apply", "post", "400"),
        ("/state", "get", "500"),
    ] {
        assert!(
            doc["paths"][path][method]["responses"].get(code).is_some(),
            "{path} {code}"
        );
    }
    assert!(doc["paths"].get("/stats").is_none());
    let schemas = &doc["components"]["schemas"];
    for field in [
        "desired_revision",
        "observed_revision",
        "last_observation",
        "errors",
        "unexecuted",
        "workloads",
    ] {
        assert!(
            schemas["Status"]["properties"].get(field).is_some(),
            "{field}"
        );
    }
    for field in [
        "id",
        "ordinal",
        "hostname",
        "state",
        "current",
        "containers",
        "error",
    ] {
        assert!(
            schemas["PodView"]["properties"].get(field).is_some(),
            "{field}"
        );
    }

    let unknown = get("/status").await;
    assert_eq!(
        (
            unknown["observation"].as_str(),
            unknown["converged"].as_bool()
        ),
        (Some("unknown"), Some(false))
    );
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 1, "a")])),
    )
    .await;
    reconcile(&app).await.unwrap();
    let status = get("/status").await;
    assert_eq!(status["converged"], true);
    assert_eq!(status["observation"], "fresh");
    let pod = &status["workloads"][0]["pods"][0];
    assert_eq!(pod["hostname"], "web-0");
    assert_eq!(pod["state"], "running");
    assert_eq!(pod["containers"][0]["name"], "app");
    assert_eq!(pod["containers"][0]["restarts"], 0);
}

#[test]
fn shared_jcs_and_ed25519_fixture_matches_the_dsl_verifier() {
    use common::types::InstructionWrapper;
    let raw = include_str!("../tests/fixtures/signatures/signed-program.json");
    let mut wrapper: InstructionWrapper = serde_json::from_str(raw).unwrap();
    let canonical = serde_json_canonicalizer::to_vec(&wrapper.program).unwrap();
    assert_eq!(
        canonical,
        include_bytes!("../tests/fixtures/signatures/program.canonical.json")
    );
    crate::intent::verify(&wrapper).unwrap();

    // Both verifiers accept a different nested object ordering on the wire.
    let reordered = format!(
        r#"{{"signature":{},"program":[{{"name":"web","kind":"pod","fields":{{"replicas":1,"ports":[80,81],"image":"nginx:alpine"}}}},{{"name":"side","kind":"pod","fields":{{"replicas":0,"ports":[80],"image":"httpd:alpine"}}}}]}}"#,
        serde_json::to_string(&wrapper.signature).unwrap()
    );
    let reordered: InstructionWrapper = serde_json::from_str(&reordered).unwrap();
    crate::intent::verify(&reordered).unwrap();
    let mut equivalent_number = wrapper.clone();
    equivalent_number.program[0]["fields"]["replicas"] = json!(1.0);
    crate::intent::verify(&equivalent_number).unwrap();

    for field in ["image", "replicas", "ports", "secure"] {
        let mut tampered = wrapper.clone();
        tampered.program[0]["fields"][field] = match field {
            "image" => json!("httpd:alpine"),
            "replicas" => json!(2),
            "ports" => json!([81, 80]),
            _ => json!(false),
        };
        assert!(
            crate::intent::verify(&tampered).is_err(),
            "tampered {field}"
        );
    }
    wrapper.program.as_array_mut().unwrap().reverse();
    assert!(crate::intent::verify(&wrapper).is_err());
}

/// A program with a config, a stored secret, a random secret, a workload using all three and one using none.
fn configured(settings: &str) -> Value {
    program(
        "conf",
        json!([
            {"kind": "config", "name": "settings", "fields": {"data": {"app.json": settings, "ca.crt": "CA"}}},
            {"kind": "secret", "name": "db", "fields": {"type": "opaque", "keys": ["password"], "source": {"store": {}}}},
            {"kind": "secret", "name": "app", "fields": {"type": "opaque", "keys": ["token"],
                "source": {"random": {"token": {"length": 24, "alphabet": "hex"}}}}},
            {"kind": "replica_set", "name": "api", "fields": {"replicas": 2, "pod": {
                "fs_group": 2000,
                "volumes": [
                    {"name": "settings", "config": {"name": "settings", "items": [{"key": "app.json", "path": "conf/app.json"}]}},
                    {"name": "creds", "secret": {"name": "db", "mode": 288}}
                ],
                "containers": [{"name": "api", "image": "docker.io/library/alpine:3.22", "command": ["sleep", "infinity"],
                    "env": [{"name": "DB_PASSWORD", "secret": {"name": "db", "key": "password"}},
                            {"name": "TOKEN", "secret": {"name": "app", "key": "token"}}],
                    "mounts": [{"volume": "settings", "path": "/etc/api"}, {"volume": "creds", "path": "/run/creds"}]}]}}},
            replica_set("other", 1, "info")
        ]),
    )
}

#[tokio::test]
async fn configs_and_secrets_flow_through_the_store_and_replace_only_their_users() {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    let http = HttpRezn::start(app.clone()).await;
    let client = reqwest::Client::new();
    let url = |path: &str| format!("{}{path}", http.url);
    let apply_raw = |program: Value| {
        let request = client
            .post(url("/apply"))
            .json(&json!({"name": "conf", "instruction_wrapper": signed(program)}));
        async move {
            let response = request.send().await.unwrap();
            (response.status(), response.text().await.unwrap())
        }
    };
    let put_secret = |name: &str, data: Value| {
        let request = client
            .put(url(&format!("/namespaces/apps/secrets/{name}")))
            .json(&json!({"data": data}));
        async move {
            let response = request.send().await.unwrap();
            (response.status(), response.text().await.unwrap())
        }
    };

    // A referenced stored key that is missing fails the revision before anything is stored.
    let (code, body) = apply_raw(configured("{}")).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains(r#"key "password" of secret "db" is not in the secret store (set it with PUT /namespaces/apps/secrets/db)"#), "{body}");
    assert!(
        app.secret_store
            .program_versions("apps")
            .unwrap()
            .is_empty(),
        "nothing generated by a refused apply"
    );
    let (code, body) = put_secret("db", json!({"password": "line\nbreak"})).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let (code, body) = apply_raw(configured("{}")).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("control characters"), "{body}");
    let (code, _) = put_secret("db", json!({"password": "pw-one"})).await;
    assert_eq!(code, StatusCode::OK);
    let (code, body) = apply_raw(configured("{}")).await;
    assert_eq!(code, StatusCode::ACCEPTED, "{body}");

    // The random key was generated once; the listing shows keys and versions, never values.
    let listed: Value = client
        .get(url("/namespaces/apps/secrets"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed[0]["name"], "app");
    assert_eq!(listed[0]["source"], "random");
    assert_eq!(listed[0]["keys"][0]["key"], "token");
    assert_eq!(listed[1]["missing"], json!([]));
    let token = app
        .secret_store
        .get_program("apps", "app", "token")
        .unwrap()
        .unwrap()
        .0;
    assert!(token.len() == 24 && token.iter().all(u8::is_ascii_hexdigit));
    assert!(!listed.to_string().contains("pw-one"));
    let (code, body) = put_secret("app", json!({"token": "mine"})).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    let gone = client
        .delete(url("/namespaces/apps/secrets/db"))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), StatusCode::CONFLICT);

    // Objects first, then pods. Their data is the config's and the store's.
    reconcile(&app).await.unwrap();
    let requests = changes(&backend, 0);
    let objects: Vec<&String> = requests
        .iter()
        .filter(|(m, p)| *m == Method::PUT && p.starts_with("/workload/secrets/"))
        .map(|(_, p)| p)
        .collect();
    assert_eq!(objects.len(), 3);
    let first_pod = requests
        .iter()
        .position(|(_, p)| p.starts_with("/workload/pods/"))
        .unwrap();
    assert!(requests[..first_pod]
        .iter()
        .all(|(_, p)| p.starts_with("/workload/secrets/")));
    let status = converged(&app).await;
    let api = |status: &crate::reconcile::Status| -> Vec<String> {
        status
            .workloads
            .iter()
            .find(|w| w.resource == "api")
            .unwrap()
            .pods
            .iter()
            .map(|p| p.id.clone())
            .collect()
    };
    let api_ids = api(&status);
    let (spec, other_spec) = {
        let engine = engine.lock().unwrap();
        (
            engine.pods[&api_ids[0]]["spec"].clone(),
            engine.pods[&id(&app, "conf", "replica_set", "other", 0)]["spec"].clone(),
        )
    };
    let config_id = spec["volumes"][0]["secret"]["name"]
        .as_str()
        .unwrap()
        .to_string();
    let db_id = spec["volumes"][1]["secret"]["name"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        spec["volumes"][0]["secret"]["items"],
        json!([{"key": "app.json", "path": "conf/app.json"}])
    );
    assert_eq!(
        (
            spec["volumes"][0]["secret"]["mode"].clone(),
            spec["volumes"][0]["secret"]["group"].clone()
        ),
        (json!(420), json!(2000))
    );
    assert_eq!(spec["volumes"][1]["secret"]["mode"], 288);
    assert_eq!(
        spec["containers"][0]["secret_env"]["DB_PASSWORD"],
        json!({"secret": db_id, "key": "password"})
    );
    {
        let engine = engine.lock().unwrap();
        let decode = |id: &str, key: &str| {
            String::from_utf8(
                STANDARD
                    .decode(engine.secrets[id]["data"][key].as_str().unwrap())
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(decode(&config_id, "app.json"), "{}");
        assert_eq!(decode(&config_id, "ca.crt"), "CA");
        assert_eq!(decode(&db_id, "password"), "pw-one");
        assert_eq!(engine.secrets[&db_id]["labels"]["dev.rezn.resource"], "db");
    }
    // Values never appear in Rezn's status or state.
    for path in ["/status", "/state"] {
        let text = client
            .get(url(path))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            !text.contains("pw-one") && !text.contains(std::str::from_utf8(&token).unwrap()),
            "{path}"
        );
    }

    // A changed config replaces only the pods that use it; status says why, and the old object goes in the same pass.
    let (code, _) = apply_raw(configured(r#"{"v":2}"#)).await;
    assert_eq!(code, StatusCode::ACCEPTED);
    let status = app
        .controller
        .status(&app.db, &app.secret_store)
        .await
        .unwrap();
    let pods = &status
        .workloads
        .iter()
        .find(|w| w.resource == "api")
        .unwrap()
        .pods;
    assert!(
        pods.iter()
            .all(|p| !p.current && p.changes == [r#"config "settings""#]),
        "{:?}",
        pods.iter().map(|p| &p.changes).collect::<Vec<_>>()
    );
    let before = backend.requests().len();
    // Replicas are replaced one per pass, so the first pass does not converge yet.
    reconcile(&app).await.unwrap_err();
    let requests = changes(&backend, before);
    let pod_puts: Vec<&String> = requests
        .iter()
        .filter(|(m, p)| *m == Method::PUT && p.starts_with("/workload/pods/"))
        .map(|(_, p)| p)
        .collect();
    assert_eq!(pod_puts.len(), 1, "one replica at a time: {requests:?}");
    assert!(requests
        .iter()
        .any(|(m, p)| *m == Method::PUT && p.starts_with("/workload/secrets/")));
    reconcile(&app).await.unwrap();
    let status = converged(&app).await;
    assert_eq!(api(&status), api_ids);
    {
        let engine = engine.lock().unwrap();
        assert_eq!(
            engine.pods[&id(&app, "conf", "replica_set", "other", 0)]["spec"],
            other_spec,
            "unrelated pods keep their spec"
        );
        assert!(
            !engine.secrets.contains_key(&config_id),
            "the old config object is removed once unused"
        );
        assert!(engine.secrets.contains_key(&db_id));
    }

    // Re-storing the same value changes nothing; a new value replaces the secret's users.
    let (code, _) = put_secret("db", json!({"password": "pw-one"})).await;
    assert_eq!(code, StatusCode::OK);
    let before = backend.requests().len();
    reconcile(&app).await.unwrap();
    assert!(changes(&backend, before).is_empty());
    let (code, _) = put_secret("db", json!({"password": "pw-two"})).await;
    assert_eq!(code, StatusCode::OK);
    let status = app
        .controller
        .status(&app.db, &app.secret_store)
        .await
        .unwrap();
    let pods = &status
        .workloads
        .iter()
        .find(|w| w.resource == "api")
        .unwrap()
        .pods;
    assert!(pods.iter().all(|p| p.changes == [r#"secret "db""#]));
    reconcile(&app).await.unwrap_err();
    reconcile(&app).await.unwrap();
    converged(&app).await;
    {
        let engine = engine.lock().unwrap();
        assert!(!engine.secrets.contains_key(&db_id));
        let new_db = engine.pods[&api_ids[1]]["spec"]["volumes"][1]["secret"]["name"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            STANDARD
                .decode(
                    engine.secrets[&new_db]["data"]["password"]
                        .as_str()
                        .unwrap()
                )
                .unwrap(),
            b"pw-two"
        );
    }

    // Removing the deployment removes its pods and then their objects in one pass; stored values stay.
    let (code, _) = apply_raw(program("conf", json!([]))).await;
    assert_eq!(code, StatusCode::ACCEPTED);
    reconcile(&app).await.unwrap();
    converged(&app).await;
    assert!(engine.lock().unwrap().secrets.is_empty());
    assert_eq!(app.secret_store.program_versions("apps").unwrap().len(), 2);
    let gone = client
        .delete(url("/namespaces/apps/secrets/db"))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), StatusCode::OK);
}

#[test]
fn object_ownership_is_proven_from_labels() {
    use crate::orqos_client::SecretStatus;
    use crate::reconcile::owned_objects;
    let owner = "a".repeat(64);
    let content = "c".repeat(64);
    let labels = |kind: &str| {
        BTreeMap::from([
            (OWNER.to_string(), owner.clone()),
            ("dev.rezn.deployment".to_string(), "shop".to_string()),
            ("dev.rezn.namespace".to_string(), "store".to_string()),
            ("dev.rezn.kind".to_string(), kind.to_string()),
            ("dev.rezn.resource".to_string(), "db".to_string()),
            ("dev.rezn.content".to_string(), content.clone()),
        ])
    };
    let status = |name: String, labels: BTreeMap<String, String>| SecretStatus {
        name,
        labels,
        pods: vec![],
    };
    let good = crate::workload::object_id(&owner, "shop", "secret", "db", &content);
    let foreign = status(
        "someone-else".into(),
        BTreeMap::from([(OWNER.to_string(), "b".repeat(64))]),
    );
    let owned = owned_objects(
        &owner,
        vec![status(good.clone(), labels("secret")), foreign],
    )
    .unwrap();
    assert_eq!(owned.len(), 1);
    assert_eq!(
        (owned[0].kind.as_str(), owned[0].resource.as_str()),
        ("secret", "db")
    );
    // A name that does not recompute from the labels, or an unknown kind, fails the observation.
    assert!(owned_objects(
        &owner,
        vec![status(
            "rz-000000000000000000000000".into(),
            labels("secret")
        )]
    )
    .is_err());
    assert!(owned_objects(&owner, vec![status(good, labels("volume"))]).is_err());
}

/// A CA, a certificate for `web` and a client trusting the CA by its certificate only.
fn tls_program(names: &[&str]) -> Value {
    program(
        "certs",
        json!([
            {"kind": "secret", "name": "ca", "fields": {"type": "tls", "keys": ["tls.crt", "tls.key"],
                "source": {"ca": {"common_name": "apps CA", "days": 3650}}}},
            {"kind": "secret", "name": "web-tls", "fields": {"type": "tls", "keys": ["tls.crt", "tls.key"],
                "source": {"certificate": {"ca": "ca", "dns_names": names, "days": 30}}}},
            {"kind": "replica_set", "name": "web", "fields": {"replicas": 1, "pod": {
                "volumes": [{"name": "tls", "secret": {"name": "web-tls", "mode": 288}}],
                "containers": [{"name": "web", "image": "docker.io/library/alpine:3.22",
                    "mounts": [{"volume": "tls", "path": "/run/tls"}]}]}}},
            {"kind": "replica_set", "name": "client", "fields": {"replicas": 1, "pod": {
                "volumes": [{"name": "ca", "secret": {"name": "ca", "items": [{"key": "tls.crt", "path": "ca.crt"}]}}],
                "containers": [{"name": "client", "image": "docker.io/library/alpine:3.22",
                    "mounts": [{"volume": "ca", "path": "/run/ca"}]}]}}}
        ]),
    )
}

#[tokio::test]
async fn generated_certificates_are_issued_reissued_renewed_and_trusted_by_their_ca_only() {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use openssl::x509::X509;
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    let http = HttpRezn::start(app.clone()).await;
    let client = reqwest::Client::new();
    let object = |pod: &str, volume: usize| -> String {
        engine.lock().unwrap().pods[pod]["spec"]["volumes"][volume]["secret"]["name"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let data = |object: &str, key: &str| -> Option<Vec<u8>> {
        let engine = engine.lock().unwrap();
        engine.secrets[object]["data"][key]
            .as_str()
            .map(|v| STANDARD.decode(v).unwrap())
    };
    let sans = |pem: &[u8]| -> Vec<String> {
        let cert = X509::from_pem(pem).unwrap();
        let mut names: Vec<String> = cert
            .subject_alt_names()
            .unwrap()
            .iter()
            .filter_map(|n| n.dnsname().map(String::from))
            .collect();
        names.sort();
        names
    };
    let web = id(&app, "certs", "replica_set", "web", 0);
    let reader = id(&app, "certs", "replica_set", "client", 0);

    // Apply generates the CA and issues the certificate; the client's object holds the CA certificate only.
    apply(&app, "certs", tls_program(&["web", "web.apps.svc"])).await;
    reconcile(&app).await.unwrap();
    converged(&app).await;
    let (ca_object, tls_object) = (object(&reader, 0), object(&web, 0));
    let ca_pem = data(&ca_object, "tls.crt").unwrap();
    assert!(
        data(&ca_object, "tls.key").is_none(),
        "the CA key never leaves Rezn"
    );
    let leaf = data(&tls_object, "tls.crt").unwrap();
    assert!(data(&tls_object, "tls.key").is_some());
    assert_eq!(sans(&leaf), ["web", "web.apps.svc"]);
    let ca = X509::from_pem(&ca_pem).unwrap();
    assert!(X509::from_pem(&leaf)
        .unwrap()
        .verify(&ca.public_key().unwrap())
        .unwrap());

    // An unchanged re-apply and pass change nothing.
    apply(&app, "certs", tls_program(&["web", "web.apps.svc"])).await;
    let before = backend.requests().len();
    reconcile(&app).await.unwrap();
    assert!(changes(&backend, before).is_empty());

    // A changed name list reissues the certificate from the same CA: only `web` is replaced.
    apply(
        &app,
        "certs",
        tls_program(&["web", "web.apps", "web.apps.svc"]),
    )
    .await;
    reconcile(&app).await.unwrap();
    converged(&app).await;
    let reissued = object(&web, 0);
    assert_ne!(reissued, tls_object);
    assert_eq!(object(&reader, 0), ca_object);
    let leaf = data(&reissued, "tls.crt").unwrap();
    assert_eq!(sans(&leaf), ["web", "web.apps", "web.apps.svc"]);
    assert!(X509::from_pem(&leaf)
        .unwrap()
        .verify(&ca.public_key().unwrap())
        .unwrap());
    assert!(!engine.lock().unwrap().secrets.contains_key(&tls_object));

    // A certificate two thirds through its lifetime is renewed by the next pass, which rolls its users.
    let store = &app.secret_store;
    let past = chrono::Utc::now() - chrono::Duration::days(25);
    let ca_key = store
        .get_program("apps", "ca", "tls.key")
        .unwrap()
        .unwrap()
        .0;
    let old = crate::certificates::issue(
        &["web", "web.apps", "web.apps.svc"].map(String::from),
        30,
        &ca_pem,
        &ca_key,
        past,
    )
    .unwrap();
    let mut meta = crate::certificates::metas(store, "apps").unwrap()["web-tls"].clone();
    (meta.not_before, meta.not_after) = (old.not_before, old.not_after);
    store
        .put_generated(
            "apps",
            "web-tls",
            &BTreeMap::from([
                ("tls.crt".to_string(), old.cert_pem),
                ("tls.key".to_string(), old.key_pem),
            ]),
            &serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
    reconcile(&app).await.unwrap();
    reconcile(&app).await.unwrap();
    converged(&app).await;
    let renewed = object(&web, 0);
    assert_ne!(renewed, reissued);
    assert_eq!(object(&reader, 0), ca_object, "the CA is unchanged");
    let fresh = crate::certificates::metas(store, "apps").unwrap()["web-tls"].clone();
    assert!(fresh.not_before > past + chrono::Duration::days(24));

    // Generated secrets cannot be set; the listing shows their validity.
    let response = client
        .put(format!("{}/namespaces/apps/secrets/ca", http.url))
        .json(&json!({"data": {"tls.crt": "x", "tls.key": "y"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let listed: Value = client
        .get(format!("{}/namespaces/apps/secrets", http.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed[0]["name"], "ca");
    assert_eq!(listed[0]["certificate"]["kind"], "ca");
    assert!(listed[0]["certificate"]["renew_after"].is_null());
    assert_eq!(listed[1]["certificate"]["kind"], "certificate");
    assert_eq!(listed[1]["certificate"]["spec_changed"], false);
    assert!(listed[1]["certificate"]["renew_after"].is_string());
    assert!(!listed.to_string().contains("BEGIN"));
}

#[test]
fn readiness_gates_rollouts_ordered_starts_and_scale_down() {
    let none = BTreeMap::new();
    let three = ordered_workload(3, "ordered");
    let id = |n: usize| three.pods[n].id.clone();
    let w = std::slice::from_ref(&three);
    // The next ordinal waits until the lower ones are ready, not just running.
    assert!(plan(w, &none, &observed(&three, &["unready"])).is_empty());
    assert_eq!(
        plan(w, &none, &observed(&three, &["running"])),
        vec![Action::Apply(id(1), three.pods[1].spec.clone())]
    );
    // Scale-down waits for the remaining replicas to be ready.
    let one = ordered_workload(1, "ordered");
    let w = std::slice::from_ref(&one);
    assert!(plan(
        w,
        &none,
        &observed(&three, &["unready", "running", "running"])
    )
    .is_empty());

    // A rollout: replica 2 has the new spec but is not ready, so replica 1 waits; readiness lets it go on.
    let mut changed = three.clone();
    for pod in &mut changed.pods {
        pod.spec.grace_seconds = 5;
    }
    let w = std::slice::from_ref(&changed);
    let mut seen = observed(&three, &["running", "running", "running"]);
    seen[2].status.spec = changed.pods[2].spec.clone();
    seen[2].status.ready = false;
    assert!(
        plan(w, &none, &seen).is_empty(),
        "an unready new replica holds the rollout"
    );
    seen[2].status.ready = true;
    assert_eq!(
        plan(w, &none, &seen),
        vec![Action::Apply(id(1), changed.pods[1].spec.clone())]
    );
    // An unready old replica serves nothing, so it is replaced at once (a broken revision cannot block its fix);
    // the ready ones still wait their turn.
    let seen = observed(&three, &["running", "unready", "running"]);
    assert_eq!(
        plan(w, &none, &seen),
        vec![Action::Apply(id(1), changed.pods[1].spec.clone())]
    );
}

#[tokio::test]
async fn unready_replicas_hold_the_rollout_and_are_reported() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 2, "a")])),
    )
    .await;
    reconcile(&app).await.unwrap();
    let status = converged(&app).await;
    assert_eq!(status.workloads[0].ready_replicas, Some(2));
    assert!(status.workloads[0].rollout.is_none());

    // New pods come up unready: the first replacement happens, then the rollout waits for it.
    engine.lock().unwrap().start_unready = true;
    apply(
        &app,
        "shop",
        program("shop", json!([replica_set("web", 2, "b")])),
    )
    .await;
    reconcile(&app).await.unwrap_err();
    let before = backend.requests().len();
    reconcile(&app).await.unwrap_err();
    assert!(
        mutations(&backend, before).is_empty(),
        "nothing else is replaced while replica 1 is unready"
    );
    let status = app
        .controller
        .status(&app.db, &app.secret_store)
        .await
        .unwrap();
    let web = &status.workloads[0];
    let one = id(&app, "shop", "replica_set", "web", 1);
    assert!(!status.converged);
    assert_eq!(
        (web.running_replicas, web.ready_replicas),
        (Some(2), Some(1))
    );
    let rollout = web
        .rollout
        .as_ref()
        .expect("the waiting rollout is reported");
    assert_eq!(rollout.waiting_for, std::slice::from_ref(&one));
    assert!(!web.pods.iter().find(|p| p.id == one).unwrap().ready);

    // Readiness returning lets the rollout finish.
    engine.lock().unwrap().start_unready = false;
    engine.lock().unwrap().set_ready(&one, true);
    reconcile(&app).await.unwrap();
    let status = converged(&app).await;
    assert_eq!(status.workloads[0].ready_replicas, Some(2));
    assert!(status.workloads[0].rollout.is_none());
}

fn requesting(name: &str, replicas: u32, cpu_millis: u64) -> Value {
    let mut set = replica_set(name, replicas, "a");
    set["fields"]["pod"]["containers"][0]["resources"] = json!({"cpu_request_millis": cpu_millis, "cpu_limit_millis": cpu_millis, "memory_limit_bytes": 67108864});
    set
}

#[tokio::test]
async fn requests_are_admitted_against_the_hosts_allocatable_capacity() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    let try_apply = |name: &'static str, resources: Value| {
        let app = app.clone();
        async move {
            apply_handler(
                State(app),
                Json(ApplyPayload {
                    name: name.into(),
                    instruction_wrapper: signed(program(name, resources)),
                }),
            )
            .await
            .map(|(code, _)| code)
        }
    };
    // 4 CPUs, minus the default 250m reserve: 3750m allocatable.
    assert_eq!(
        try_apply("a", json!([requesting("web", 2, 1000)]))
            .await
            .unwrap(),
        StatusCode::ACCEPTED
    );
    let (code, message) = try_apply("b", json!([requesting("api", 2, 1000)]))
        .await
        .unwrap_err();
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(
        message.contains("CPU: 4000m requested of 3750m allocatable (deployment \"b\" 2000m: api 2 × 1000m; other deployments 2000m)"),
        "{message}"
    );
    // The refusal changed nothing; shrinking the other deployment makes room.
    assert_eq!(
        try_apply("a", json!([requesting("web", 1, 1000)]))
            .await
            .unwrap(),
        StatusCode::ACCEPTED
    );
    assert_eq!(
        try_apply("b", json!([requesting("api", 2, 1000)]))
            .await
            .unwrap(),
        StatusCode::ACCEPTED
    );

    reconcile(&app).await.unwrap();
    let status = converged(&app).await;
    assert_eq!(status.resources.requested.cpu_millis, 3000);
    assert_eq!(
        status.resources.requested.memory_bytes,
        3 * 67108864,
        "requests default to limits"
    );
    assert_eq!(status.resources.allocatable.unwrap().cpu_millis, 3750);
    let pods = engine.lock().unwrap().pods.clone();
    let spec = &pods.values().next().unwrap()["spec"]["containers"][0]["resources"];
    assert_eq!(spec["memory_request_bytes"], 67108864);

    // Without capacity, nothing is admitted blind.
    engine.lock().unwrap().fail_capacity = true;
    let (code, message) = try_apply("c", json!([requesting("tiny", 1, 1)]))
        .await
        .unwrap_err();
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        message.contains("cannot check the revision against the host's capacity"),
        "{message}"
    );
}
