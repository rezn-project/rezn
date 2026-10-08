use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::{Method, StatusCode},
    Json,
};
use serde_json::{json, Value};

use crate::{
    reconcile::{CONFIG, DEPLOYMENT, MANAGED, OWNER, POD},
    routes::apply::{apply_handler, ApplyPayload},
    store::{self, STATE_KEY},
    test_support::{app, pod, signed, HttpRezn, MockOrqos, RecordedRequest},
};

#[derive(Default)]
struct Engine {
    containers: Vec<Value>,
    sequence: usize,
    fail_list: bool,
    fail_remove: bool,
    fail_create: bool,
    failed_start: bool,
}
impl Engine {
    fn respond(&mut self, request: &RecordedRequest) -> (StatusCode, String) {
        match (request.method.as_str(), request.uri.path()) {
            ("GET", "/docker/containers") => {
                if self.fail_list {
                    (StatusCode::SERVICE_UNAVAILABLE, "Docker unavailable".into())
                } else {
                    (StatusCode::OK, json!(self.containers).to_string())
                }
            }
            ("POST", "/docker/containers") => {
                if self.fail_create {
                    return (StatusCode::NOT_FOUND, "missing pre-pulled image".into());
                }
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                self.sequence += 1;
                let id = format!("id{}", self.sequence);
                let ports: Vec<_> = body["ports"].as_array().unwrap().iter().map(|p| json!({"IP":"0.0.0.0", "PrivatePort":p["container"], "PublicPort":32000+self.sequence, "Type":"tcp"})).collect();
                self.containers.push(json!({"Id":id, "Names":[body["name"]], "Image":body["image"], "Labels":body["labels"], "State":if self.failed_start { "created" } else { "running" }, "Ports":ports}));
                if self.failed_start {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "start failed after create".into(),
                    )
                } else {
                    (
                        StatusCode::OK,
                        json!({"id":id,"ports":{"80/tcp":0}}).to_string(),
                    )
                }
            }
            ("POST", path) if path.ends_with("/remove") => {
                if self.fail_remove {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "remove failed".into());
                }
                let id = path.split('/').nth(3).unwrap();
                self.containers.retain(|c| c["Id"] != id);
                (StatusCode::NO_CONTENT, String::new())
            }
            _ => (StatusCode::NOT_FOUND, "unknown route".into()),
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
async fn apply(app: &Arc<crate::AppState>, name: &str, program: Vec<Value>) {
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
async fn ids(app: &Arc<crate::AppState>) -> Vec<String> {
    let status = app.controller.status(&app.db).await.unwrap();
    assert!(status.converged, "{:?}", status.errors);
    status
        .workloads
        .iter()
        .flat_map(|w| w.containers.iter().map(|c| c.id.clone()))
        .collect()
}

#[tokio::test]
async fn validation_is_atomic_and_signature_uses_the_entire_submitted_program() {
    let (backend, _) = backend().await;
    let app = app(backend.client.clone(), None);
    let http = HttpRezn::start(app.clone()).await;
    let client = reqwest::Client::new();
    apply(&app, "prod", vec![pod("web", "nginx:alpine", 1, &[80])]).await;
    let original = app.db.get(STATE_KEY).unwrap().unwrap();
    let programs = vec![
        vec![pod("", "nginx:alpine", 1, &[])],
        vec![pod("../web", "nginx:alpine", 1, &[])],
        vec![pod("web", "", 1, &[])],
        vec![pod("web", "UPPER/repo", 1, &[])],
        vec![pod("web", "nginx:", 1, &[])],
        vec![pod("web", "nginx@sha256:bad", 1, &[])],
        vec![pod("web", "https://nginx", 1, &[])],
        vec![pod("web", "nginx:alpine", 1, &[0])],
        vec![pod("web", "nginx:alpine", 1, &[80, 80])],
        vec![
            json!({"kind":"pod","name":"web","fields":{"image":"nginx", "replicas":-1,"ports":[]}}),
        ],
        vec![
            json!({"kind":"pod","name":"web","fields":{"image":"nginx", "replicas":1,"ports":[65536]}}),
        ],
        vec![pod("web", "nginx", 0, &[]), pod("web", "nginx", 0, &[])],
        vec![
            json!({"kind":"service","name":"web","fields":{"image":"nginx", "replicas":1,"ports":[]}}),
        ],
        vec![
            json!({"kind":"pod","name":"web","options":[],"fields":{"image":"nginx", "replicas":1,"ports":[]}}),
        ],
        vec![
            json!({"kind":"pod","name":"web","fields":{"image":"nginx", "replicas":1,"ports":[],"env":{}}}),
        ],
        vec![
            json!({"kind":"pod","name":"web","fields":{"image":"nginx", "replicas":1,"ports":[],"secure":false}}),
        ],
        vec![
            json!({"kind":"pod","name":"web","fields":{"image":"nginx", "replicas":1,"ports":[],"future":null}}),
        ],
        vec![json!({"kind":"pod","name":"web","fields":null})],
    ];
    for program in programs {
        let response = client
            .post(format!("{}/apply", http.url))
            .json(&json!({"name":"prod","instruction_wrapper":signed(program.clone())}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{program:?}");
        assert!(!response.text().await.unwrap().is_empty());
        assert_eq!(app.db.get(STATE_KEY).unwrap().unwrap(), original);
    }
    let mut tampered = signed(vec![pod("web", "nginx", 1, &[])]);
    tampered.program[0]["unknown_signed_field"] = json!(true);
    let response = client
        .post(format!("{}/apply", http.url))
        .json(&json!({"name":"prod","instruction_wrapper":tampered}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("invalid program signature"));
    for name in ["", "unsafe/name", "bad:label"] {
        let response = client
            .post(format!("{}/apply", http.url))
            .json(&json!({"name":name,"instruction_wrapper":signed(vec![])}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    for altered in [
        json!({"program":[],"signature":{"algorithm":"rsa","pub":"","sig":""}}),
        json!({"program":[],"signature":{"algorithm":"ed25519","pub":"???","sig":""}}),
        json!({"program":[],"signature":{"algorithm":"ed25519","pub":"","sig":""}}),
    ] {
        let response = client
            .post(format!("{}/apply", http.url))
            .json(&json!({"name":"prod","instruction_wrapper":altered}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let mut unsupported_envelope = json!({"name":"prod","instruction_wrapper":signed(vec![])});
    unsupported_envelope["instruction_wrapper"]["future"] = json!(1);
    assert_eq!(
        client
            .post(format!("{}/apply", http.url))
            .json(&unsupported_envelope)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(app.db.get(STATE_KEY).unwrap().unwrap(), original);
    assert!(backend.requests().is_empty());
    let stored = store::load(&app.db).unwrap();
    assert_eq!(stored.revision, 1);
    assert_eq!(
        stored.deployments["prod"].meta.instructions,
        vec![("pod".into(), "web".into())]
    );
    assert_eq!(
        stored.deployments["prod"].meta.sig_id,
        stored.deployments["prod"].envelope.signature.sig
    );
}

#[tokio::test]
async fn lifecycle_reapply_scale_update_remove_and_empty_program() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    apply(
        &app,
        "prod",
        vec![
            pod("web", "nginx:alpine", 1, &[80]),
            pod("side", "nginx:alpine", 1, &[]),
        ],
    )
    .await;
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    let original = ids(&app).await;
    apply(
        &app,
        "prod",
        vec![
            pod("web", "nginx:alpine", 1, &[80]),
            pod("side", "nginx:alpine", 1, &[]),
        ],
    )
    .await;
    assert!(!app.controller.status(&app.db).await.unwrap().converged);
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    assert_eq!(ids(&app).await, original);
    for replicas in [2, 1, 0] {
        apply(
            &app,
            "prod",
            vec![
                pod("web", "nginx:alpine", replicas, &[80]),
                pod("side", "nginx:alpine", 1, &[]),
            ],
        )
        .await;
        app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
        assert_eq!(ids(&app).await.len(), replicas + 1);
    }
    apply(&app, "prod", vec![pod("web", "nginx:alpine", 1, &[80])]).await;
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    let old = ids(&app).await;
    apply(&app, "prod", vec![pod("web", "httpd:alpine", 1, &[80])]).await;
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    let changed = ids(&app).await;
    assert_ne!(old, changed);
    apply(&app, "prod", vec![pod("web", "httpd:alpine", 1, &[80, 81])]).await;
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    assert_ne!(ids(&app).await, changed);
    let status = app.controller.status(&app.db).await.unwrap();
    assert_eq!(
        status.workloads[0].containers[0].ports[0].public_port,
        Some(32006)
    );
    apply(&app, "prod", vec![]).await;
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    assert!(ids(&app).await.is_empty());
    assert!(engine.lock().unwrap().containers.is_empty());
    assert!(backend
        .requests()
        .iter()
        .filter(|r| r.method == Method::GET)
        .all(|r| r.query().get("all").map(String::as_str) == Some("true")));
    assert!(backend
        .requests()
        .iter()
        .filter(|r| r.uri.path().ends_with("/remove"))
        .all(|r| r.uri.path().starts_with("/docker/containers/id")));
}

#[tokio::test]
async fn ownership_isolation_and_stopped_removed_pods_are_cleaned_up() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    apply(&app, "prod", vec![pod("web", "nginx:alpine", 1, &[])]).await;
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    let mut foreign = engine.lock().unwrap().containers[0].clone();
    foreign["Id"] = json!("foreign");
    foreign["Names"] = json!(["/prod-web-prefix"]);
    foreign["Labels"][OWNER] = json!("another-runtime");
    let mut unproven = foreign.clone();
    unproven["Id"] = json!("unproven");
    unproven["Labels"][OWNER] = json!(store::load(&app.db).unwrap().owner);
    unproven["Labels"].as_object_mut().unwrap().remove(MANAGED);
    {
        let mut e = engine.lock().unwrap();
        e.containers[0]["State"] = json!("exited");
        e.containers.extend([foreign.clone(), unproven.clone()]);
    }
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    assert_eq!(ids(&app).await.len(), 1);
    apply(&app, "prod", vec![]).await;
    {
        let mut e = engine.lock().unwrap();
        for c in &mut e.containers {
            if c["Id"] != "foreign" && c["Id"] != "unproven" {
                c["State"] = json!("created");
            }
        }
    }
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    assert_eq!(engine.lock().unwrap().containers, vec![foreign, unproven]);
}

#[tokio::test]
async fn failed_list_prevents_all_mutations_and_next_pass_can_recover() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    apply(&app, "prod", vec![pod("web", "nginx:alpine", 1, &[])]).await;
    engine.lock().unwrap().fail_list = true;
    assert!(app.controller.reconcile(&app.db, &app.orqos).await.is_err());
    let unknown = app.controller.status(&app.db).await.unwrap();
    assert_eq!(unknown.observation, "unknown");
    assert_eq!(unknown.workloads[0].running_replicas, None);
    assert!(!unknown.converged);
    engine.lock().unwrap().fail_list = false;
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    let fresh = app.controller.status(&app.db).await.unwrap();
    for replicas in [0, 2] {
        apply(
            &app,
            "prod",
            vec![
                pod("web", "nginx:alpine", replicas, &[]),
                pod("later", "nginx", 1, &[]),
            ],
        )
        .await;
        engine.lock().unwrap().fail_list = true;
        let before = backend.requests().len();
        let error = app
            .controller
            .reconcile(&app.db, &app.orqos)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("503"));
        assert!(backend.requests()[before..]
            .iter()
            .all(|r| r.method == Method::GET));
        let stale = app.controller.status(&app.db).await.unwrap();
        assert_eq!(stale.observation, "stale");
        assert_eq!(stale.last_observation, fresh.last_observation);
        assert!(!stale.converged);
        // A newly desired pod was never observed under this revision; its zero comes
        // from the previous complete owner-wide listing and is explicitly stale.
        assert_eq!(
            stale
                .workloads
                .iter()
                .find(|w| w.pod == "web")
                .unwrap()
                .running_replicas,
            Some(1)
        );
    }
    engine.lock().unwrap().fail_list = false;
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    assert_eq!(ids(&app).await.len(), 3);
}

#[tokio::test]
async fn mutation_failures_propagate_and_failed_start_debris_is_recovered() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    apply(&app, "prod", vec![pod("web", "nginx:alpine", 1, &[])]).await;
    engine.lock().unwrap().fail_create = true;
    assert!(app.controller.reconcile(&app.db, &app.orqos).await.is_err());
    assert!(!app.controller.status(&app.db).await.unwrap().converged);
    {
        let mut e = engine.lock().unwrap();
        e.fail_create = false;
        e.failed_start = true;
    }
    assert!(app.controller.reconcile(&app.db, &app.orqos).await.is_err());
    assert_eq!(engine.lock().unwrap().containers.len(), 1);
    engine.lock().unwrap().failed_start = false;
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    assert_eq!(ids(&app).await.len(), 1);
    assert_eq!(engine.lock().unwrap().containers.len(), 1);
    apply(&app, "prod", vec![]).await;
    engine.lock().unwrap().fail_remove = true;
    let error = app
        .controller
        .reconcile(&app.db, &app.orqos)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("remove failed"));
    assert!(!app.controller.status(&app.db).await.unwrap().converged);
    engine.lock().unwrap().fail_remove = false;
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    assert!(ids(&app).await.is_empty());
}

#[tokio::test]
async fn corrupt_missing_legacy_and_semantically_invalid_state_never_become_deletion_plans() {
    let (backend, _) = backend().await;
    let app = app(backend.client.clone(), None);
    let http = HttpRezn::start(app.clone()).await;
    apply(&app, "prod", vec![pod("web", "nginx", 1, &[])]).await;
    let original = app.db.get(STATE_KEY).unwrap().unwrap();
    let mut invalid: Value = serde_json::from_slice(&original).unwrap();
    invalid["deployments"]["prod"]["envelope"] = json!(signed(vec![pod("web", "nginx", 1, &[0])]));
    let mut wrong_format: Value = serde_json::from_slice(&original).unwrap();
    wrong_format["format"] = json!(2);
    let mut missing_deployments: Value = serde_json::from_slice(&original).unwrap();
    missing_deployments["deployments"] = json!({});
    for bytes in [
        b"not JSON".to_vec(),
        serde_json::to_vec(&invalid).unwrap(),
        serde_json::to_vec(&wrong_format).unwrap(),
        serde_json::to_vec(&missing_deployments).unwrap(),
    ] {
        app.db.insert(STATE_KEY, bytes).unwrap();
        assert!(app.controller.reconcile(&app.db, &app.orqos).await.is_err());
        for path in ["/state", "/state/raw", "/status"] {
            assert_eq!(
                reqwest::get(format!("{}{path}", http.url))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::INTERNAL_SERVER_ERROR
            );
        }
        let before = app.db.get(STATE_KEY).unwrap();
        assert_eq!(
            apply_handler(
                State(app.clone()),
                Json(ApplyPayload {
                    name: "prod".into(),
                    instruction_wrapper: signed(vec![])
                })
            )
            .await
            .err()
            .unwrap()
            .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(app.db.get(STATE_KEY).unwrap(), before);
    }
    app.db.remove(STATE_KEY).unwrap();
    assert!(app.controller.reconcile(&app.db, &app.orqos).await.is_err());
    assert!(backend.requests().is_empty());
    let legacy = sled::Config::new().temporary(true).open().unwrap();
    legacy.insert("desired", b"{}").unwrap();
    assert!(store::initialize(&legacy)
        .unwrap_err()
        .to_string()
        .contains("no migration"));
}

#[tokio::test]
async fn restart_reloads_ownership_and_ports_without_adoption_or_duplicates() {
    let (backend, engine) = backend().await;
    let path = std::env::temp_dir().join(format!("rezn-test-{}", store::random_id().unwrap()));
    let first = app(backend.client.clone(), Some(sled::open(&path).unwrap()));
    apply(&first, "prod", vec![pod("web", "nginx:alpine", 1, &[80])]).await;
    first
        .controller
        .reconcile(&first.db, &first.orqos)
        .await
        .unwrap();
    let before = ids(&first).await;
    let owner = store::load(&first.db).unwrap().owner;
    first.db.flush().unwrap();
    drop(first);
    let second = app(backend.client.clone(), Some(sled::open(&path).unwrap()));
    assert_eq!(store::load(&second.db).unwrap().owner, owner);
    assert_eq!(
        second
            .controller
            .status(&second.db)
            .await
            .unwrap()
            .observation,
        "unknown"
    );
    second
        .controller
        .reconcile(&second.db, &second.orqos)
        .await
        .unwrap();
    assert_eq!(ids(&second).await, before);
    assert_eq!(
        second
            .controller
            .status(&second.db)
            .await
            .unwrap()
            .workloads[0]
            .containers[0]
            .ports[0]
            .public_port,
        Some(32001)
    );
    assert_eq!(engine.lock().unwrap().containers.len(), 1);
    drop(second);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn older_observation_cannot_report_convergence_of_newer_intent_and_passes_are_serial() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let e = entered.clone();
    let r = release.clone();
    let backend = MockOrqos::start_async(move |_| {
        let e = e.clone();
        let r = r.clone();
        async move {
            e.notify_one();
            wait_for(&r).await;
            (StatusCode::OK, "[]".into())
        }
    })
    .await;
    let app = app(backend.client.clone(), None);
    apply(&app, "prod", vec![]).await;
    let first_app = app.clone();
    let first = tokio::spawn(async move {
        first_app
            .controller
            .reconcile(&first_app.db, &first_app.orqos)
            .await
    });
    wait_for(&entered).await;
    let second_app = app.clone();
    let second = tokio::spawn(async move {
        second_app
            .controller
            .reconcile(&second_app.db, &second_app.orqos)
            .await
    });
    apply(&app, "prod", vec![pod("web", "nginx", 0, &[])]).await;
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
async fn malformed_ownership_proof_prevents_all_mutations() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    apply(&app, "prod", vec![pod("web", "nginx", 1, &[])]).await;
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    engine.lock().unwrap().containers[0]["Labels"][CONFIG] = json!("invalid");
    apply(&app, "prod", vec![]).await;
    let before = backend.requests().len();
    assert!(app.controller.reconcile(&app.db, &app.orqos).await.is_err());
    assert_eq!(backend.requests().len(), before + 1);
    assert!(engine.lock().unwrap().containers[0]["Labels"]
        .get(DEPLOYMENT)
        .is_some());
    assert!(engine.lock().unwrap().containers[0]["Labels"]
        .get(POD)
        .is_some());
}

#[test]
fn supported_example_is_valid_and_canonically_equivalent_input_verifies() {
    let wrapper: common::types::InstructionWrapper =
        serde_json::from_str(include_str!("../../examples/test.ir.json")).unwrap();
    crate::intent::verify(&wrapper).unwrap();
    crate::intent::validate_program(&wrapper.program).unwrap();
    // Construct another wire serialization of the actual signed fixture.
    let reordered = format!(
        r#"{{"signature":{},"program":[{{"name":"web","kind":"pod","fields":{{"replicas":1,"ports":[80],"image":"nginx:alpine"}}}}]}}"#,
        serde_json::to_string(&wrapper.signature).unwrap()
    );
    let reordered = serde_json::from_str(&reordered).unwrap();
    crate::intent::verify(&reordered).unwrap();
}

#[test]
fn docker_reference_subset_and_configuration_identity() {
    use crate::intent::{configuration, validate_program};
    for image in [
        "nginx",
        "my_registry/repo",
        "example.com:5000/team/api:V1.2",
        "localhost:5000/repo",
        "a__b/c--d:v1",
        "repo@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ] {
        validate_program(&[pod("web", image, 0, &[])]).unwrap();
    }
    for image in [
        "repo..bad",
        "repo___bad",
        "repo:tag:tag",
        "registry:0/repo",
        "registry:99999/repo",
        "repo/",
        "/repo",
        "repo@sha256:ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789",
        "repo: bad",
        "repo\n",
        "é",
    ] {
        assert!(
            validate_program(&[pod("web", image, 0, &[])]).is_err(),
            "{image}"
        );
    }
    let first = common::types::PodFields {
        image: "nginx".into(),
        replicas: 1,
        ports: vec![80, 81],
    };
    let second = common::types::PodFields {
        image: "nginx".into(),
        replicas: 2,
        ports: vec![81, 80],
    };
    assert_eq!(configuration(&first), configuration(&second));
}

#[tokio::test]
async fn failed_follow_up_listing_keeps_a_stale_snapshot_and_retry_does_not_duplicate() {
    let engine = Arc::new(Mutex::new(Engine::default()));
    let captured = engine.clone();
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_requests = requests.clone();
    let backend = MockOrqos::start(move |request| {
        if request.method == Method::GET
            && captured_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1
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
    apply(&app, "prod", vec![pod("web", "nginx", 1, &[80])]).await;
    assert!(app.controller.reconcile(&app.db, &app.orqos).await.is_err());
    let stale = app.controller.status(&app.db).await.unwrap();
    assert_eq!(stale.observation, "stale");
    assert_eq!(stale.workloads[0].running_replicas, Some(0));
    assert!(!stale.converged);
    let created = engine.lock().unwrap().containers[0]["Id"]
        .as_str()
        .unwrap()
        .to_string();
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    assert_eq!(ids(&app).await, vec![created]);
    assert_eq!(engine.lock().unwrap().containers.len(), 1);
}

#[tokio::test]
async fn apply_wakes_the_worker_without_waiting_for_the_periodic_interval() {
    let (backend, _) = backend().await;
    let app = app(backend.client.clone(), None);
    let worker = tokio::spawn(crate::reconcile::run(
        app.clone(),
        std::time::Duration::from_secs(3600),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if app.controller.status(&app.db).await.unwrap().converged {
                break;
            }
            tokio::task::yield_now().await;
        }
        apply(&app, "prod", vec![pod("web", "nginx", 1, &[])]).await;
        loop {
            if app.controller.status(&app.db).await.unwrap().converged {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(ids(&app).await.len(), 1);
    worker.abort();
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
            if request.method == Method::POST && request.uri.path() == "/docker/containers" {
                e.notify_one();
                wait_for(&r).await;
            }
            captured.lock().unwrap().respond(&request)
        }
    })
    .await;
    let app = app(backend.client.clone(), None);
    apply(&app, "prod", vec![pod("web", "nginx", 1, &[])]).await;
    let pass_app = app.clone();
    let pass = tokio::spawn(async move {
        pass_app
            .controller
            .reconcile(&pass_app.db, &pass_app.orqos)
            .await
    });
    wait_for(&entered).await;
    apply(&app, "prod", vec![]).await;
    release.notify_one();
    pass.await.unwrap().unwrap();
    let pending = app.controller.status(&app.db).await.unwrap();
    assert_eq!(pending.observation, "pending");
    assert!(!pending.converged);
    assert_ne!(pending.observed_revision, Some(pending.desired_revision));
    assert_eq!(pending.workloads[0].desired_replicas, 0);
    assert_eq!(pending.workloads[0].running_replicas, Some(1));
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    assert!(ids(&app).await.is_empty());
}

#[tokio::test]
async fn openapi_and_http_status_describe_stored_acceptance_and_observation_fields() {
    let (backend, _) = backend().await;
    let app = app(backend.client.clone(), None);
    let http = HttpRezn::start(app.clone()).await;
    let client = reqwest::Client::new();
    let doc: Value = client
        .get(format!("{}/api/openapi.json", http.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(doc["paths"]["/apply"]["post"]["responses"]
        .get("202")
        .is_some());
    assert!(doc["paths"]["/apply"]["post"]["responses"]
        .get("400")
        .is_some());
    assert!(doc["paths"]["/state"]["get"]["responses"]
        .get("500")
        .is_some());
    assert!(doc["paths"].get("/status").is_some());
    for field in [
        "desired_revision",
        "observed_revision",
        "last_observation",
        "errors",
        "workloads",
    ] {
        assert!(doc["components"]["schemas"]["Status"]["properties"]
            .get(field)
            .is_some());
    }
    let unknown: Value = client
        .get(format!("{}/status", http.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(unknown["observation"], "unknown");
    assert!(unknown["last_observation"].is_null());
    assert_eq!(unknown["converged"], false);
    apply(&app, "prod", vec![pod("web", "nginx", 1, &[80])]).await;
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    let status: Value = client
        .get(format!("{}/status", http.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["converged"], true);
    assert_eq!(
        status["workloads"][0]["containers"][0]["Ports"][0]["PublicPort"],
        32001
    );
}

#[tokio::test]
async fn incomplete_or_invalid_observation_never_causes_replacement() {
    let (backend, engine) = backend().await;
    let app = app(backend.client.clone(), None);
    apply(&app, "prod", vec![pod("web", "nginx", 1, &[80])]).await;
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    let original = engine.lock().unwrap().containers[0].clone();
    for missing_state in [true, false] {
        let mut malformed = original.clone();
        if missing_state {
            malformed.as_object_mut().unwrap().remove("State");
        } else {
            malformed["Ports"][0]["PublicPort"] = json!(0);
        }
        engine.lock().unwrap().containers = vec![malformed];
        let before = backend.requests().len();
        assert!(app.controller.reconcile(&app.db, &app.orqos).await.is_err());
        assert_eq!(backend.requests().len(), before + 1);
        let status = app.controller.status(&app.db).await.unwrap();
        assert_eq!(status.observation, "stale");
        assert_eq!(status.workloads[0].running_replicas, Some(1));
        assert!(!status.converged);
    }
    engine.lock().unwrap().containers = vec![original];
    app.controller.reconcile(&app.db, &app.orqos).await.unwrap();
    assert_eq!(ids(&app).await, vec!["id1"]);
}
