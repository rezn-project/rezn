//! IR v2 validator conformance with examples/ir-v2 (valid, schema-invalid and semantically invalid programs).
use common::ir_v2::{validate, Kind};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn load(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn programs(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "json") && p.file_name().unwrap() != "expected.json"
        })
        .collect();
    files.sort();
    files
}

#[test]
fn valid_examples_validate() {
    let files = programs(&root().join("examples/ir-v2"));
    assert!(files.len() >= 6);
    for file in files {
        if let Err(issues) = validate(&load(&file)) {
            panic!("{}: {issues:#?}", file.display());
        }
    }
}

fn check_invalid(dir: &str) {
    let dir = root().join(dir);
    let expected = load(&dir.join("expected.json"));
    let files = programs(&dir);
    assert_eq!(
        files.len(),
        expected.as_object().unwrap().len(),
        "{}",
        dir.display()
    );
    for file in files {
        let name = file.file_name().unwrap().to_str().unwrap();
        let at = expected[name].as_str().unwrap();
        let issues = validate(&load(&file)).expect_err(name);
        assert!(
            issues
                .iter()
                .any(|i| i.pointer == at || i.pointer.starts_with(&format!("{at}/"))),
            "{name}: expected an issue at {at:?}, got {issues:#?}"
        );
    }
}

#[test]
fn schema_invalid_examples_are_rejected() {
    check_invalid("examples/ir-v2/invalid");
}

#[test]
fn semantically_invalid_examples_are_rejected() {
    check_invalid("examples/ir-v2/invalid-semantic");
}

fn minimal() -> Value {
    load(&root().join("examples/ir-v2/minimal.json"))
}

#[test]
fn reference_program_decodes_every_kind() {
    let program = validate(&load(&root().join("examples/ir-v2/reference.json"))).unwrap();
    let kinds: std::collections::BTreeSet<_> = program.resources.iter().map(|r| r.kind()).collect();
    for kind in [
        Kind::ReplicaSet,
        Kind::OrderedReplicaSet,
        Kind::Job,
        Kind::ScheduledJob,
        Kind::Service,
    ] {
        assert!(kinds.contains(&kind), "{kind:?}");
    }
    assert!(
        kinds.contains(&Kind::NetworkPolicy)
            && kinds.contains(&Kind::Ingress)
            && kinds.contains(&Kind::Secret)
    );
}

#[test]
fn quantities_must_be_unsigned_integers() {
    for bad in [json!(1.0), json!(-1), json!("1")] {
        let mut program = minimal();
        program["resources"][0]["fields"]["replicas"] = bad.clone();
        let issues = validate(&program).expect_err("non-integer replicas");
        assert_eq!(issues[0].pointer, "/resources/0/fields/replicas", "{bad}");
    }
}

#[test]
fn ordered_and_scheduled_names_leave_room_for_suffixes() {
    let mut program = load(&root().join("examples/ir-v2/stateful.json"));
    let long = "c".repeat(53);
    for resource in program["resources"].as_array_mut().unwrap() {
        if resource["kind"] == "ordered_replica_set" {
            resource["name"] = json!(long);
        }
    }
    let issues = validate(&program).unwrap_err();
    assert!(
        issues.iter().any(|i| i.pointer == "/resources/1/name"),
        "{issues:#?}"
    );
}

#[test]
fn every_issue_is_reported() {
    let mut program = minimal();
    program["namespace"] = json!("Bad");
    program["resources"][0]["fields"]["pod"]["containers"][0]["image"] = json!("UPPER/case");
    program["resources"][1]["fields"]["target"] = json!("nothing");
    let pointers: Vec<_> = validate(&program)
        .unwrap_err()
        .into_iter()
        .map(|i| i.pointer)
        .collect();
    for at in [
        "/namespace",
        "/resources/0/fields/pod/containers/0/image",
        "/resources/1/fields/target",
    ] {
        assert!(
            pointers.iter().any(|p| p == at),
            "{at} missing from {pointers:?}"
        );
    }
}

#[test]
fn a_v1_array_is_not_a_v2_program() {
    let issues = validate(&json!([])).unwrap_err();
    assert_eq!(issues[0].pointer, "");
}
