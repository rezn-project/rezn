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
    fail_list: bool,
    fail_put: Option<&'static str>,
    /// State of newly applied pods; `running` when unset.
    start_state: Option<&'static str>,
}

impl Engine {
    fn respond(&mut self, request: &RecordedRequest) -> (StatusCode, String) {
        if request.authorization.as_deref() != Some(&format!("Bearer {TOKEN}")[..]) {
            return (StatusCode::UNAUTHORIZED, "missing token".into());
        }
        let path = request.uri.path();
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
                let containers: Vec<Value> = spec["containers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| json!({"name": c["name"], "unit": format!("orqos-{id}-c-{}.service", c["name"].as_str().unwrap()),
                                    "active_state": "active", "sub_state": "running", "result": "success", "restarts": 0}))
                    .collect();
                let pod = json!({
                    "id": id, "desired": "running", "state": self.start_state.unwrap_or("running"),
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

    fn set_state(&mut self, id: &str, state: &str) {
        self.pods.get_mut(id).unwrap()["state"] = json!(state);
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
    app.controller.reconcile(&app.db, &app.orqos).await
}

async fn converged(app: &Arc<crate::AppState>) -> crate::reconcile::Status {
    let status = app.controller.status(&app.db).await.unwrap();
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
    let status = app.controller.status(&app.db).await.unwrap();
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

/// Ordered replica sets need a headless service, which is not executed yet (rezn#58); their sequencing is checked
/// on the planner directly with the workload translation of the program minus its service.
fn ordered_workload(replicas: u32, management: &str) -> Workload {
    let service = json!({"kind": "service", "name": "db", "fields": {"target": "db", "headless": true,
        "ports": [{"name": "pg", "port": 5432, "target_port": 5432}]}});
    let mut program = ir_v2::validate(&program(
        "data",
        json!([ordered_set("db", replicas, management), service]),
    ))
    .unwrap();
    program
        .resources
        .retain(|r| r.kind() == Kind::OrderedReplicaSet);
    translate(&"0".repeat(64), &program).unwrap().remove(0)
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
                state: state.to_string(),
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
    apply(&app, "hello", example_v2("minimal.json")).await;
    let service = json!({"kind": "service", "name": "db", "fields": {"target": "db", "headless": true,
        "ports": [{"name": "pg", "port": 5432, "target_port": 5432}]}});
    apply(
        &app,
        "data",
        program("data", json!([ordered_set("db", 1, "ordered"), service])),
    )
    .await;
    reconcile(&app).await.unwrap();
    assert!(mutations(&backend, before).is_empty());
    let status = app.controller.status(&app.db).await.unwrap();
    assert!(!status.converged);
    assert!(status.errors.is_empty());
    assert_eq!(status.unexecuted.len(), 2);
    assert_eq!(status.unexecuted[0].deployment, "data");
    assert_eq!(
        status.unexecuted[0].reasons,
        vec!["/resources/1: services are not executed yet (rezn#58)"]
    );
    assert_eq!(status.unexecuted[1].deployment, "hello");
    assert_eq!(
        status.unexecuted[1].reasons,
        vec![
            "/resources/0/fields/pod/containers/0/readiness: probes are not executed yet (rezn#71, orqos#26)",
            "/resources/1: services are not executed yet (rezn#58)",
        ]
    );

    // A running deployment whose new program cannot run keeps its pods from the earlier revision.
    let mut probed = replica_set("web", 3, "b");
    probed["fields"]["pod"]["containers"][0]["liveness"] = json!({"exec": ["true"]});
    let before = backend.requests().len();
    apply(&app, "shop", program("shop", json!([probed]))).await;
    reconcile(&app).await.unwrap();
    assert!(mutations(&backend, before).is_empty());
    let status = app.controller.status(&app.db).await.unwrap();
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
        .contains("/liveness: probes"));
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
        app.controller.status(&app.db).await.unwrap().observation,
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
    let status = app.controller.status(&app.db).await.unwrap();
    assert_eq!(status.workloads[0].running_replicas, Some(1));
    assert_eq!(status.workloads[0].pods[1].state, "failed");
    assert!(!status.converged);
}

#[tokio::test]
async fn foreign_pods_are_untouched_and_unproven_ownership_blocks_every_change() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    let foreign = |id: &str, labels: Value| {
        json!({"id": id, "state": "running", "containers": [], "error": null,
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
            app.controller.status(&app.db).await.unwrap().observation,
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
    let status = app.controller.status(&app.db).await.unwrap();
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
    let status = app.controller.status(&app.db).await.unwrap();
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
    let stale = app.controller.status(&app.db).await.unwrap();
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
    let backend = MockOrqos::start_async(move |_| {
        let (e, r) = (e.clone(), r.clone());
        async move {
            e.notify_one();
            wait_for(&r).await;
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
    let pending = app.controller.status(&app.db).await.unwrap();
    assert!(!pending.converged);
    assert_ne!(pending.observed_revision, Some(pending.desired_revision));
    wait_for(&entered).await;
    assert_eq!(backend.requests().len(), 2);
    release.notify_one();
    second.await.unwrap().unwrap();
    assert!(app.controller.status(&app.db).await.unwrap().converged);
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
    let pending = app.controller.status(&app.db).await.unwrap();
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
        while !app.controller.status(&app.db).await.unwrap().converged {
            tokio::task::yield_now().await;
        }
        apply(
            &app,
            "shop",
            program("shop", json!([replica_set("web", 1, "a")])),
        )
        .await;
        while !app.controller.status(&app.db).await.unwrap().converged {
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
    assert!(backend.requests().is_empty());
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
