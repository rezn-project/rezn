//! Translates IR v2 programs into Orqos workload pods.
//!
//! Orqos's workload backend runs pods of plain containers (image, command, args, literal environment, restart
//! policy, grace period) on owned networks with DNS aliases. A program runs only if every resource maps exactly
//! onto that; otherwise the whole deployment is left unexecuted and every feature that blocks it is reported with
//! the issue that adds it. Nothing is silently dropped.
//!
//! Every pod joins its namespace's network. A service becomes aliases on its target's pods: `<svc>`,
//! `<svc>.<ns>` and `<svc>.<ns>.svc`, plus `<pod hostname>.<svc>` (and the qualified forms) when headless.
use std::collections::{BTreeMap, BTreeSet};

use common::ir_v2::{self, Kind, PodManagement, PortRef, Program, Pull, Spec};
use sha2::{Digest, Sha256};

use crate::orqos_client::{ContainerSpec, Network, PodSpec, Restart};

pub const OWNER: &str = "dev.rezn.owner";
pub const DEPLOYMENT: &str = "dev.rezn.deployment";
pub const NAMESPACE: &str = "dev.rezn.namespace";
pub const KIND: &str = "dev.rezn.kind";
pub const RESOURCE: &str = "dev.rezn.resource";
pub const ORDINAL: &str = "dev.rezn.ordinal";

/// One replica set of a deployment, expanded into its pods by ordinal.
#[derive(Clone, Debug, PartialEq)]
pub struct Workload {
    pub deployment: String,
    pub kind: Kind,
    pub resource: String,
    /// Ordered replica sets with `ordered` pod management start, stop and scale one replica at a time.
    pub sequential: bool,
    pub pods: Vec<DesiredPod>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DesiredPod {
    pub id: String,
    pub ordinal: u32,
    pub spec: PodSpec,
}

/// The Orqos pod ID of one replica: stable for its identity, distinct across owners, at most 48 characters.
pub fn pod_id(owner: &str, deployment: &str, kind: &str, resource: &str, ordinal: u32) -> String {
    let identity = serde_json::json!([owner, deployment, kind, resource, ordinal]);
    let digest = Sha256::digest(identity.to_string().as_bytes());
    format!("rz-{}", &hex::encode(digest)[..24])
}

/// The Orqos network of one namespace: shared by every deployment of this owner in that namespace, distinct across
/// owners, at most 27 characters.
pub fn network_name(owner: &str, namespace: &str) -> String {
    let identity = serde_json::json!([owner, "namespace", namespace]);
    let digest = Sha256::digest(identity.to_string().as_bytes());
    format!("rz-{}", &hex::encode(digest)[..24])
}

/// The DNS names of one pod: those of every service targeting its resource, sorted.
fn aliases(services: &[(&str, bool)], namespace: &str, hostname: &str) -> Vec<String> {
    let mut names = BTreeSet::new();
    for (service, headless) in services {
        for name in [
            service.to_string(),
            format!("{service}.{namespace}"),
            format!("{service}.{namespace}.svc"),
        ] {
            if *headless {
                names.insert(format!("{hostname}.{name}"));
            }
            names.insert(name);
        }
    }
    names.into_iter().collect()
}

/// `<resource>-<ordinal>`, with the resource name shortened when the result would exceed a DNS label.
pub fn hostname(resource: &str, ordinal: u32) -> String {
    let suffix = format!("-{ordinal}");
    let keep = resource.len().min(63 - suffix.len());
    format!("{}{suffix}", resource[..keep].trim_end_matches('-'))
}

/// The pods of every replica set, or every reason the program cannot run on the workload backend yet.
pub fn translate(owner: &str, program: &Program) -> Result<Vec<Workload>, Vec<String>> {
    let reasons = unsupported(program);
    if !reasons.is_empty() {
        return Err(reasons);
    }
    // Services by target workload: (service name, headless).
    let mut services: BTreeMap<&str, Vec<(&str, bool)>> = BTreeMap::new();
    for resource in &program.resources {
        if let Spec::Service(s) = &resource.spec {
            services
                .entry(s.target.as_str())
                .or_default()
                .push((resource.name.as_str(), s.headless.unwrap_or(false)));
        }
    }
    let network = network_name(owner, &program.namespace);
    let mut workloads = Vec::new();
    for resource in &program.resources {
        let (replicas, sequential, pod) = match &resource.spec {
            Spec::ReplicaSet(s) => (s.replicas, false, &s.pod),
            Spec::OrderedReplicaSet(s) => (
                s.replicas,
                s.pod_management.unwrap_or(PodManagement::Ordered) == PodManagement::Ordered,
                &s.pod,
            ),
            _ => continue,
        };
        let kind = resource.kind();
        let containers: Vec<ContainerSpec> = pod
            .containers
            .iter()
            .map(|c| ContainerSpec {
                name: c.name.clone(),
                image: c.image.clone(),
                command: c.command.clone(),
                args: c.args.clone(),
                env: c
                    .env
                    .iter()
                    .filter_map(|e| Some((e.name.clone(), e.value.clone()?)))
                    .collect(),
            })
            .collect();
        let restart = match pod.restart() {
            ir_v2::Restart::Always => Restart::Always,
            ir_v2::Restart::OnFailure => Restart::OnFailure,
            ir_v2::Restart::Never => Restart::Never,
        };
        let targeting = services
            .get(resource.name.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let pods = (0..replicas)
            .map(|ordinal| DesiredPod {
                id: pod_id(
                    owner,
                    &program.deployment,
                    kind.as_str(),
                    &resource.name,
                    ordinal,
                ),
                ordinal,
                spec: PodSpec {
                    network: Some(Network {
                        name: network.clone(),
                        aliases: aliases(
                            targeting,
                            &program.namespace,
                            &hostname(&resource.name, ordinal),
                        ),
                    }),
                    hostname: hostname(&resource.name, ordinal),
                    containers: containers.clone(),
                    restart,
                    grace_seconds: pod.grace_seconds.unwrap_or(30),
                    labels: BTreeMap::from([
                        (OWNER.into(), owner.into()),
                        (DEPLOYMENT.into(), program.deployment.clone()),
                        (NAMESPACE.into(), program.namespace.clone()),
                        (KIND.into(), kind.as_str().into()),
                        (RESOURCE.into(), resource.name.clone()),
                        (ORDINAL.into(), ordinal.to_string()),
                    ]),
                },
            })
            .collect();
        workloads.push(Workload {
            deployment: program.deployment.clone(),
            kind,
            resource: resource.name.clone(),
            sequential,
            pods,
        });
    }
    Ok(workloads)
}

/// IR v2's name classes: workload kinds share one, every other kind is its own.
fn name_class(kind: Kind) -> &'static str {
    if kind.is_workload() {
        "workload"
    } else {
        kind.as_str()
    }
}

/// Names of `program` that collide with a resource of the same class in another deployment of the same namespace
/// (IR v2: deployments may share a namespace only if their resource names do not collide), as `pointer: reason`.
pub fn collisions<'a>(
    deployment: &str,
    program: &Program,
    others: impl IntoIterator<Item = (&'a String, &'a Program)>,
) -> Vec<String> {
    let mut reasons = Vec::new();
    for (other_name, other) in others {
        if other_name == deployment || other.namespace != program.namespace {
            continue;
        }
        for (i, resource) in program.resources.iter().enumerate() {
            let class = name_class(resource.kind());
            if other
                .resources
                .iter()
                .any(|r| r.name == resource.name && name_class(r.kind()) == class)
            {
                reasons.push(format!(
                    "/resources/{i}/name: {:?} is also a {class} name in deployment {other_name:?} of namespace {:?}",
                    resource.name, program.namespace
                ));
            }
        }
    }
    reasons
}

/// Every feature of the program that the workload backend cannot honour yet, as `pointer: reason (issue)`.
pub fn unsupported(program: &Program) -> Vec<String> {
    let mut reasons = Vec::new();
    let mut add = |pointer: String, what: &str, issue: &str| {
        reasons.push(format!("{pointer}: {what} not executed yet ({issue})"))
    };
    for (i, resource) in program.resources.iter().enumerate() {
        let at = format!("/resources/{i}");
        if resource.phase.is_some() {
            add(format!("{at}/phase"), "revision phases are", "rezn#43");
        }
        let pod = match &resource.spec {
            Spec::ReplicaSet(s) => &s.pod,
            Spec::OrderedReplicaSet(s) => {
                if !s.volume_claims.is_empty() {
                    add(
                        format!("{at}/fields/volume_claims"),
                        "volume claims are",
                        "rezn#67",
                    );
                }
                &s.pod
            }
            Spec::Config(_) => continue,
            Spec::Service(s) => {
                // Services are DNS names for the target's replicas: clients connect to the container's own port.
                let target_ports: BTreeMap<&str, u16> = program
                    .resources
                    .iter()
                    .find(|r| r.name == s.target && r.kind().is_workload())
                    .and_then(|r| r.pod())
                    .map(|pod| {
                        pod.containers
                            .iter()
                            .chain(&pod.sidecars)
                            .flat_map(|c| &c.ports)
                            .map(|p| (p.name.as_str(), p.port))
                            .collect()
                    })
                    .unwrap_or_default();
                for (j, port) in s.ports.iter().enumerate() {
                    let target = match &port.target_port {
                        PortRef::Number(n) => Some(*n),
                        PortRef::Name(name) => target_ports.get(name.as_str()).copied(),
                    };
                    if target != Some(port.port) {
                        add(
                            format!("{at}/fields/ports/{j}"),
                            "services whose port differs from the target port are",
                            "rezn#123",
                        );
                    }
                }
                continue;
            }
            other => {
                let (what, issue) = match other {
                    Spec::Job(_) => ("jobs are", "rezn#42"),
                    Spec::ScheduledJob(_) => ("scheduled jobs are", "rezn#46"),
                    Spec::VolumeClaim(_) => ("volume claims are", "rezn#67"),
                    Spec::Secret(_) => ("secrets are", "rezn#52, rezn#53"),
                    Spec::Ingress(_) => ("ingresses are", "rezn#60"),
                    _ => ("network policies are", "rezn#64"),
                };
                add(at, what, issue);
                continue;
            }
        };
        let at = format!("{at}/fields/pod");
        if !pod.init.is_empty() {
            add(format!("{at}/init"), "init containers are", "orqos#19");
        }
        if !pod.sidecars.is_empty() {
            add(format!("{at}/sidecars"), "sidecars are", "orqos#19");
        }
        for (field, set) in [
            ("user", pod.user.is_some()),
            ("group", pod.group.is_some()),
            ("run_as_non_root", pod.run_as_non_root.is_some()),
            ("fs_group", pod.fs_group.is_some()),
            ("fs_group_policy", pod.fs_group_policy.is_some()),
            ("seccomp", pod.seccomp.is_some()),
        ] {
            if set {
                add(
                    format!("{at}/{field}"),
                    "pod security settings are",
                    "orqos#20",
                );
            }
        }
        if !pod.image_pull_secrets.is_empty() {
            add(
                format!("{at}/image_pull_secrets"),
                "registry credentials are",
                "orqos#23",
            );
        }
        if !pod.volumes.is_empty() {
            add(
                format!("{at}/volumes"),
                "volumes are",
                "orqos#21, orqos#22, orqos#24",
            );
        }
        for (j, c) in pod.containers.iter().enumerate() {
            let at = format!("{at}/containers/{j}");
            if matches!(c.pull, Some(Pull::Always | Pull::Never)) {
                add(
                    format!("{at}/pull"),
                    "pull policies other than if_not_present are",
                    "orqos#23",
                );
            }
            for (k, env) in c.env.iter().enumerate() {
                if env.secret.is_some() {
                    add(
                        format!("{at}/env/{k}/secret"),
                        "secret references are",
                        "rezn#52",
                    );
                }
                if env.pod_ip.is_some() {
                    add(
                        format!("{at}/env/{k}/pod_ip"),
                        "pod IP references are",
                        "orqos#25",
                    );
                }
            }
            for (probe, set) in [
                ("startup", c.startup.is_some()),
                ("readiness", c.readiness.is_some()),
                ("liveness", c.liveness.is_some()),
            ] {
                if set {
                    add(format!("{at}/{probe}"), "probes are", "rezn#71, orqos#26");
                }
            }
            if c.resources.is_some() {
                add(
                    format!("{at}/resources"),
                    "resource requests and limits are",
                    "rezn#40",
                );
            }
            if c.security.is_some() {
                add(
                    format!("{at}/security"),
                    "container security settings are",
                    "orqos#20",
                );
            }
            if !c.mounts.is_empty() {
                add(
                    format!("{at}/mounts"),
                    "mounts are",
                    "orqos#21, orqos#22, orqos#24",
                );
            }
        }
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OWNER_ID: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";

    fn program(value: serde_json::Value) -> Program {
        ir_v2::validate(&value).unwrap_or_else(|issues| panic!("{}", ir_v2::describe(&issues)))
    }

    fn example(name: &str) -> Program {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../examples/ir-v2")
            .join(name);
        program(serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap())
    }

    /// A program with a config, a replica set and an ordered replica set with its headless service.
    fn web(replicas: u32) -> serde_json::Value {
        json!({"ir": "rezn.dev/v2", "deployment": "shop", "namespace": "store", "resources": [
            {"kind": "config", "name": "settings", "fields": {"data": {"a": "b"}}},
            {"kind": "replica_set", "name": "web", "fields": {"replicas": replicas, "pod": {
                "grace_seconds": 10,
                "containers": [{"name": "web", "image": "docker.io/library/nginx:1.27-alpine",
                    "pull": "if_not_present", "command": ["nginx"], "args": ["-g", "daemon off;"],
                    "env": [{"name": "LEVEL", "value": "info"}], "ports": [{"name": "http", "port": 80}]}]}}},
            {"kind": "ordered_replica_set", "name": "db", "fields": {"replicas": 2, "service": "db",
                "pod_management": "parallel", "pod": {
                "containers": [{"name": "db", "image": "postgres:17", "ports": [{"name": "pg", "port": 5432}]}]}}},
            {"kind": "service", "name": "db", "fields": {"target": "db", "headless": true,
                "ports": [{"name": "pg", "port": 5432, "target_port": "pg"}]}}
        ]})
    }

    #[test]
    fn replica_sets_become_labelled_pods() {
        let workloads = translate(OWNER_ID, &program(web(2))).unwrap();
        assert_eq!(workloads.len(), 2);
        let set = &workloads[0];
        assert_eq!(
            (set.kind, set.resource.as_str(), set.sequential),
            (Kind::ReplicaSet, "web", false)
        );
        assert_eq!(set.pods.len(), 2);
        let pod = &set.pods[1];
        assert_eq!(pod.ordinal, 1);
        assert_eq!(pod.id, pod_id(OWNER_ID, "shop", "replica_set", "web", 1));
        assert_eq!(pod.spec.hostname, "web-1");
        assert_eq!(pod.spec.restart, Restart::Always);
        assert_eq!(pod.spec.grace_seconds, 10);
        let c = &pod.spec.containers[0];
        assert_eq!(c.command, Some(vec!["nginx".to_string()]));
        assert_eq!(
            c.args,
            Some(vec!["-g".to_string(), "daemon off;".to_string()])
        );
        assert_eq!(
            c.env,
            BTreeMap::from([("LEVEL".to_string(), "info".to_string())])
        );
        assert_eq!(
            pod.spec.labels,
            BTreeMap::from([
                (OWNER.to_string(), OWNER_ID.to_string()),
                (DEPLOYMENT.to_string(), "shop".to_string()),
                (NAMESPACE.to_string(), "store".to_string()),
                (KIND.to_string(), "replica_set".to_string()),
                (RESOURCE.to_string(), "web".to_string()),
                (ORDINAL.to_string(), "1".to_string()),
            ])
        );
        let db = &workloads[1];
        assert_eq!((db.kind, db.sequential), (Kind::OrderedReplicaSet, false));
        assert_eq!(db.pods[0].spec.restart, Restart::Always);
        assert_eq!(db.pods[0].spec.grace_seconds, 30);
        assert_eq!(db.pods[0].spec.hostname, "db-0");

        let mut ordered = web(1);
        ordered["resources"][2]["fields"]
            .as_object_mut()
            .unwrap()
            .remove("pod_management");
        assert!(translate(OWNER_ID, &program(ordered)).unwrap()[1].sequential);
        assert!(translate(OWNER_ID, &program(web(0))).unwrap()[0]
            .pods
            .is_empty());
    }

    #[test]
    fn pods_join_their_namespace_network_with_service_aliases() {
        let mut value = web(2);
        value["resources"].as_array_mut().unwrap().extend([
            json!({"kind": "service", "name": "web", "fields": {"target": "web",
                    "ports": [{"name": "http", "port": 80, "target_port": "http"}]}}),
            json!({"kind": "service", "name": "front", "fields": {"target": "web",
                    "ports": [{"name": "http", "port": 80, "target_port": 80}]}}),
        ]);
        let workloads = translate(OWNER_ID, &program(value)).unwrap();
        let network = network_name(OWNER_ID, "store");
        let names = |w: usize, p: usize| {
            let spec = &workloads[w].pods[p].spec;
            let n = spec.network.as_ref().unwrap();
            assert_eq!(n.name, network);
            n.aliases.clone()
        };
        assert_eq!(
            names(0, 1),
            [
                "front",
                "front.store",
                "front.store.svc",
                "web",
                "web.store",
                "web.store.svc"
            ]
        );
        assert_eq!(names(0, 0), names(0, 1));
        assert_eq!(
            names(1, 1),
            [
                "db",
                "db-1.db",
                "db-1.db.store",
                "db-1.db.store.svc",
                "db.store",
                "db.store.svc"
            ]
        );

        // A workload no service targets still joins the namespace network, without aliases.
        let mut lone = web(1);
        lone["resources"].as_array_mut().unwrap().truncate(2);
        let spec = &translate(OWNER_ID, &program(lone)).unwrap()[0].pods[0].spec;
        assert_eq!(
            spec.network,
            Some(Network {
                name: network.clone(),
                aliases: vec![]
            })
        );
    }

    #[test]
    fn networks_are_per_owner_and_namespace() {
        let n = network_name(OWNER_ID, "store");
        assert_eq!(n, network_name(OWNER_ID, "store"));
        assert_eq!(n.len(), 27);
        assert_ne!(n, network_name(OWNER_ID, "store2"));
        assert_ne!(n, network_name(&"1".repeat(64), "store"));
        assert_ne!(n, pod_id(OWNER_ID, "store", "namespace", "", 0));
    }

    #[test]
    fn services_that_remap_ports_are_not_executed() {
        let mut value = web(1);
        value["resources"].as_array_mut().unwrap().push(json!(
            {"kind": "service", "name": "web", "fields": {"target": "web", "ports": [
                {"name": "same", "port": 80, "target_port": "http"},
                {"name": "named", "port": 8080, "target_port": "http"},
                {"name": "numbered", "port": 443, "target_port": 8443}]}}));
        assert_eq!(
            translate(OWNER_ID, &program(value)).unwrap_err(),
            vec![
                "/resources/4/fields/ports/1: services whose port differs from the target port are not executed yet (rezn#123)",
                "/resources/4/fields/ports/2: services whose port differs from the target port are not executed yet (rezn#123)",
            ]
        );
    }

    #[test]
    fn names_may_not_collide_across_deployments_of_a_namespace() {
        let shop = program(web(1));
        let mut other = web(1);
        other["deployment"] = json!("other");
        // Same names in another namespace are fine.
        let elsewhere = {
            let mut v = other.clone();
            v["namespace"] = json!("elsewhere");
            program(v)
        };
        // A job named like another deployment's ordered set shares the workload class; a service named like a
        // workload does not collide with it.
        other["resources"] = json!([
            {"kind": "replica_set", "name": "web", "fields": {"replicas": 1, "pod": {
                "containers": [{"name": "c", "image": "alpine"}]}}},
            {"kind": "service", "name": "web", "fields": {"target": "web", "ports": []}},
            {"kind": "job", "name": "db", "fields": {"pod": {"restart": "never",
                "containers": [{"name": "c", "image": "alpine"}]}}},
            {"kind": "config", "name": "settings", "fields": {"data": {}}},
            {"kind": "config", "name": "fresh", "fields": {"data": {}}}
        ]);
        let other = program(other);
        let all = BTreeMap::from([
            ("shop".to_string(), shop.clone()),
            ("other".to_string(), other.clone()),
            ("elsewhere".to_string(), elsewhere),
        ]);
        assert_eq!(
            collisions("other", &other, &all),
            vec![
                "/resources/0/name: \"web\" is also a workload name in deployment \"shop\" of namespace \"store\"",
                "/resources/2/name: \"db\" is also a workload name in deployment \"shop\" of namespace \"store\"",
                "/resources/3/name: \"settings\" is also a config name in deployment \"shop\" of namespace \"store\"",
            ]
        );
        assert_eq!(collisions("shop", &shop, &all).len(), 3);
        assert!(collisions("shop", &shop, [(&"shop".to_string(), &shop)]).is_empty());
    }

    #[test]
    fn ids_are_stable_short_and_distinct() {
        let id = pod_id(OWNER_ID, "shop", "replica_set", "web", 0);
        assert_eq!(id, pod_id(OWNER_ID, "shop", "replica_set", "web", 0));
        assert_eq!(id.len(), 27);
        assert!(id.starts_with("rz-"));
        for other in [
            pod_id(&"1".repeat(64), "shop", "replica_set", "web", 0),
            pod_id(OWNER_ID, "shop2", "replica_set", "web", 0),
            pod_id(OWNER_ID, "shop", "ordered_replica_set", "web", 0),
            pod_id(OWNER_ID, "shop", "replica_set", "web2", 0),
            pod_id(OWNER_ID, "shop", "replica_set", "web", 1),
        ] {
            assert_ne!(id, other);
        }
    }

    #[test]
    fn hostnames_fit_a_dns_label() {
        assert_eq!(hostname("web", 12), "web-12");
        let long = format!("{}-bc", "a".repeat(60));
        assert_eq!(hostname(&long, 3), format!("{}-3", "a".repeat(60)));
        assert_eq!(
            hostname(&"a".repeat(52), 99),
            format!("{}-99", "a".repeat(52))
        );
        assert_eq!(hostname(&"a".repeat(63), 100).len(), 63);
    }

    #[test]
    fn the_runnable_example_runs_and_the_others_need_more() {
        assert_eq!(
            translate(OWNER_ID, &example("runnable.json")).unwrap()[0]
                .pods
                .len(),
            2
        );
        for name in [
            "edge.json",
            "jobs.json",
            "minimal.json",
            "reference.json",
            "secrets.json",
            "stateful.json",
        ] {
            assert!(translate(OWNER_ID, &example(name)).is_err(), "{name}");
        }
    }

    #[test]
    fn unsupported_features_are_all_reported() {
        let reasons = translate(OWNER_ID, &example("minimal.json")).unwrap_err();
        assert_eq!(
            reasons,
            vec![
                "/resources/0/fields/pod/containers/0/readiness: probes are not executed yet (rezn#71, orqos#26)",
            ]
        );
        let reasons = unsupported(&example("reference.json")).join("\n");
        for expected in [
            "secrets are",
            "jobs are",
            "init containers are",
            "volumes are",
            "mounts are",
            "secret references are",
            "pod security settings are",
            "resource requests and limits are",
            "revision phases are",
            "ingresses are",
            "network policies are",
            "volume claims are",
        ] {
            assert!(reasons.contains(expected), "{expected}:\n{reasons}");
        }
        let mut always = web(1);
        always["resources"].as_array_mut().unwrap().truncate(2);
        always["resources"][1]["fields"]["pod"]["containers"][0]["pull"] = json!("always");
        always["resources"][1]["fields"]["pod"]["containers"][0]["env"] =
            json!([{"name": "IP", "pod_ip": true}]);
        assert_eq!(
            unsupported(&program(always)),
            vec![
                "/resources/1/fields/pod/containers/0/pull: pull policies other than if_not_present are not executed yet (orqos#23)",
                "/resources/1/fields/pod/containers/0/env/0/pod_ip: pod IP references are not executed yet (orqos#25)",
            ]
        );
    }
}
