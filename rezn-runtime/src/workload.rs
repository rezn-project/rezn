//! Translates IR v2 programs into Orqos workload pods.
//!
//! Orqos's workload backend runs pods of plain containers (image, command, args, literal environment, restart
//! policy, grace period) on owned networks with DNS aliases. A program runs only if every resource maps exactly
//! onto that; otherwise the whole deployment is left unexecuted and every feature that blocks it is reported with
//! the issue that adds it. Nothing is silently dropped.
//!
//! Every pod joins its namespace's network. A service becomes aliases on its target's pods: `<svc>`,
//! `<svc>.<ns>` and `<svc>.<ns>.svc`, plus `<pod hostname>.<svc>` (and the qualified forms) when headless.
//!
//! Each `config` and `secret` a pod uses becomes an immutable Orqos secret whose ID covers its content (the
//! config's data, or the secret's stored key versions): a changed value is a new object, so exactly the pods that
//! use it get a new spec and are replaced, and nothing else is.
use std::collections::{BTreeMap, BTreeSet};

use common::ir_v2::{self, Kind, PodManagement, PortRef, Program, Pull, Spec};
use sha2::{Digest, Sha256};

use crate::orqos_client::{
    ContainerSpec, Medium, MountSpec, Network, PodSpec, PodVolume, Restart, Scratch, SecretFiles,
    SecretItem, SecretKeyRef, VolumeSpec,
};
use crate::secret::Versions;

pub const OWNER: &str = "dev.rezn.owner";
pub const DEPLOYMENT: &str = "dev.rezn.deployment";
pub const NAMESPACE: &str = "dev.rezn.namespace";
pub const KIND: &str = "dev.rezn.kind";
pub const RESOURCE: &str = "dev.rezn.resource";
pub const ORDINAL: &str = "dev.rezn.ordinal";
pub const CLAIM: &str = "dev.rezn.claim";
pub const WORKLOAD: &str = "dev.rezn.workload";
/// `true` or `false`: whether the volume is kept once its workload or deployment is gone. Kept in Orqos's volume
/// record, so the decision survives the program that made it.
pub const RETAIN: &str = "dev.rezn.retain";
/// The digest an Orqos secret's ID covers: of a config's data, or of a secret's stored key versions.
pub const CONTENT: &str = "dev.rezn.content";

/// Stored key versions of a namespace's program secrets, by secret name.
pub type Stored = BTreeMap<String, Versions>;

/// Highest user or group ID a pod can use: Orqos gives each pod a user namespace of 65536 IDs.
const MAX_ID: u32 = 65535;

/// A deployment's pods, volumes and the Orqos secrets its pods use.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Translation {
    pub workloads: Vec<Workload>,
    pub volumes: Vec<DesiredVolume>,
    pub objects: Vec<DesiredObject>,
}

/// An Orqos secret holding a `config`'s data or a `secret`'s stored keys.
#[derive(Clone, Debug, PartialEq)]
pub struct DesiredObject {
    pub id: String,
    pub labels: BTreeMap<String, String>,
    pub source: ObjectSource,
}

/// Where an Orqos secret's values come from. Secret values are read from the store only when the object is
/// created, and must still have these versions.
#[derive(Clone, Debug, PartialEq)]
pub enum ObjectSource {
    Config(BTreeMap<String, String>),
    Secret {
        namespace: String,
        name: String,
        versions: Versions,
    },
}

/// An owned volume: a `volume_claim`'s, or one replica's from an ordered set's `volume_claims`.
#[derive(Clone, Debug, PartialEq)]
pub struct DesiredVolume {
    pub id: String,
    pub spec: VolumeSpec,
}

/// One replica set of a deployment, expanded into its pods by ordinal.
#[derive(Clone, Debug, PartialEq)]
pub struct Workload {
    pub deployment: String,
    pub kind: Kind,
    pub resource: String,
    /// Ordered replica sets with `ordered` pod management start, stop and scale one replica at a time.
    pub sequential: bool,
    /// Per-replica volumes of ordinals beyond the replica count are deleted (`retain.on_scale_down: false`).
    pub delete_on_scale_down: bool,
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

/// The Orqos volume of a claim: a deployment's `volume_claim` (no workload or ordinal) or one replica's volume from
/// an ordered set's `volume_claims`. Stable, so retained volumes come back to the same claim and ordinal.
pub fn volume_id(
    owner: &str,
    deployment: &str,
    workload: Option<&str>,
    claim: &str,
    ordinal: Option<u32>,
) -> String {
    let identity = serde_json::json!([owner, "volume", deployment, workload, claim, ordinal]);
    let digest = Sha256::digest(identity.to_string().as_bytes());
    format!("rz-{}", &hex::encode(digest)[..24])
}

/// The Orqos secret of one `config` or `secret` (`kind`) at one content digest.
pub fn object_id(owner: &str, deployment: &str, kind: &str, name: &str, content: &str) -> String {
    let identity = serde_json::json!([owner, kind, deployment, name, content]);
    let digest = Sha256::digest(identity.to_string().as_bytes());
    format!("rz-{}", &hex::encode(digest)[..24])
}

/// The hex SHA-256 of the canonical JSON of a string map.
fn content_digest(map: &BTreeMap<String, String>) -> String {
    let canonical = serde_json_canonicalizer::to_vec(map).expect("string maps serialize");
    hex::encode(Sha256::digest(canonical))
}

/// Keys each `secret` of the program is referenced for, by secret name: environment variables, secret volumes
/// (their `items`, or every declared key), `image_pull_secrets` and ingress `tls`.
pub fn referenced_keys<'a>(program: &'a Program) -> BTreeMap<&'a str, BTreeSet<&'a str>> {
    let declared: BTreeMap<&str, &ir_v2::Secret> = program
        .resources
        .iter()
        .filter_map(|r| match &r.spec {
            Spec::Secret(s) => Some((r.name.as_str(), s)),
            _ => None,
        })
        .collect();
    let mut keys: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut add = |secret: &'a str, key: &'a str| {
        keys.entry(secret).or_default().insert(key);
    };
    for resource in &program.resources {
        if let Spec::Ingress(ingress) = &resource.spec {
            for tls in &ingress.tls {
                add(&tls.secret, "tls.crt");
                add(&tls.secret, "tls.key");
            }
        }
        let Some(pod) = resource.pod() else { continue };
        for secret in &pod.image_pull_secrets {
            add(secret, ".dockerconfigjson");
        }
        for volume in &pod.volumes {
            let Some(projection) = &volume.secret else {
                continue;
            };
            match &projection.items {
                Some(items) => items.iter().for_each(|i| add(&projection.name, &i.key)),
                None => declared
                    .get(projection.name.as_str())
                    .into_iter()
                    .flat_map(|s| &s.keys)
                    .for_each(|k| add(&projection.name, k)),
            }
        }
        for c in pod.init.iter().chain(&pod.sidecars).chain(&pod.containers) {
            for env in &c.env {
                if let Some(r) = &env.secret {
                    add(&r.name, &r.key);
                }
            }
        }
    }
    keys
}

/// Secret keys that become environment variables, as (secret, key).
pub fn env_keys(program: &Program) -> BTreeSet<(&str, &str)> {
    program
        .resources
        .iter()
        .filter_map(|r| r.pod())
        .flat_map(|pod| pod.init.iter().chain(&pod.sidecars).chain(&pod.containers))
        .flat_map(|c| &c.env)
        .filter_map(|e| e.secret.as_ref())
        .map(|r| (r.name.as_str(), r.key.as_str()))
        .collect()
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

/// A referenced key the secret store lacks.
#[derive(Clone, Debug, PartialEq)]
pub struct Missing<'a> {
    /// Index of the `secret` resource.
    pub index: usize,
    pub secret: &'a str,
    pub key: &'a str,
    /// Rezn generates it (a `random` source); otherwise the operator stores it.
    pub generated: bool,
}

impl Missing<'_> {
    pub fn reason(&self, namespace: &str) -> String {
        format!(
            "/resources/{}: key {:?} of secret {:?} is not in the secret store (set it with PUT /namespaces/{namespace}/secrets/{})",
            self.index, self.key, self.secret, self.secret
        )
    }
}

/// Referenced keys of stored, seeded and random secrets that the secret store lacks.
pub fn missing_keys<'a>(program: &'a Program, stored: &Stored) -> Vec<Missing<'a>> {
    let referenced = referenced_keys(program);
    let mut missing = Vec::new();
    for (index, resource) in program.resources.iter().enumerate() {
        let Spec::Secret(secret) = &resource.spec else {
            continue;
        };
        if secret.source.ca.is_some() || secret.source.certificate.is_some() {
            continue;
        }
        let have = stored.get(&resource.name);
        for key in referenced.get(resource.name.as_str()).into_iter().flatten() {
            if !have.is_some_and(|h| h.contains_key(*key)) {
                missing.push(Missing {
                    index,
                    secret: &resource.name,
                    key,
                    generated: secret.source.random.is_some(),
                });
            }
        }
    }
    missing
}

/// The pods of every replica set, or every reason the program cannot run on the workload backend yet. `stored`
/// holds the versions of the program namespace's stored secrets.
pub fn translate(
    owner: &str,
    program: &Program,
    stored: &Stored,
) -> Result<Translation, Vec<String>> {
    let mut reasons = unsupported(program);
    reasons.extend(
        missing_keys(program, stored)
            .iter()
            .map(|m| m.reason(&program.namespace)),
    );
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
    let labels = |workload: Option<&str>, claim: &str, ordinal: Option<u32>, retain: bool| {
        let mut labels = BTreeMap::from([
            (OWNER.to_string(), owner.to_string()),
            (DEPLOYMENT.into(), program.deployment.clone()),
            (NAMESPACE.into(), program.namespace.clone()),
            (CLAIM.into(), claim.into()),
            (RETAIN.into(), retain.to_string()),
        ]);
        if let (Some(workload), Some(ordinal)) = (workload, ordinal) {
            labels.insert(WORKLOAD.into(), workload.into());
            labels.insert(ORDINAL.into(), ordinal.to_string());
        }
        labels
    };
    let claim_groups = claim_groups(program);
    let mut translation = Translation::default();
    // Orqos secrets for the configs and secrets pods use, by (kind, name).
    let mut used: BTreeSet<(&str, &str)> = BTreeSet::new();
    for pod in program.resources.iter().filter_map(|r| r.pod()) {
        for v in &pod.volumes {
            if let Some(p) = &v.config {
                used.insert(("config", p.name.as_str()));
            }
            if let Some(p) = &v.secret {
                used.insert(("secret", p.name.as_str()));
            }
        }
        for env in pod.containers.iter().flat_map(|c| &c.env) {
            if let Some(r) = &env.secret {
                used.insert(("secret", r.name.as_str()));
            }
        }
    }
    let mut object_ids: BTreeMap<(&str, &str), String> = BTreeMap::new();
    for resource in &program.resources {
        let (kind, content, source) = match &resource.spec {
            Spec::Config(c) if used.contains(&("config", resource.name.as_str())) => (
                "config",
                content_digest(&c.data),
                ObjectSource::Config(c.data.clone()),
            ),
            Spec::Secret(s) if used.contains(&("secret", resource.name.as_str())) => {
                let have = stored.get(&resource.name);
                let versions: Versions = s
                    .keys
                    .iter()
                    .filter_map(|k| Some((k.clone(), have?.get(k)?.clone())))
                    .collect();
                (
                    "secret",
                    content_digest(&versions),
                    ObjectSource::Secret {
                        namespace: program.namespace.clone(),
                        name: resource.name.clone(),
                        versions,
                    },
                )
            }
            _ => continue,
        };
        let id = object_id(owner, &program.deployment, kind, &resource.name, &content);
        object_ids.insert((kind, resource.name.as_str()), id.clone());
        translation.objects.push(DesiredObject {
            id,
            labels: BTreeMap::from([
                (OWNER.to_string(), owner.to_string()),
                (DEPLOYMENT.into(), program.deployment.clone()),
                (NAMESPACE.into(), program.namespace.clone()),
                (KIND.into(), kind.into()),
                (RESOURCE.into(), resource.name.clone()),
                (CONTENT.into(), content),
            ]),
            source,
        });
    }
    for resource in &program.resources {
        if let Spec::VolumeClaim(_) = resource.spec {
            translation.volumes.push(DesiredVolume {
                id: volume_id(owner, &program.deployment, None, &resource.name, None),
                spec: VolumeSpec {
                    group: claim_groups
                        .get(resource.name.as_str())
                        .and_then(|groups| groups.iter().next().copied().flatten()),
                    // IR v2: a volume claim is kept when its deployment is removed.
                    labels: labels(None, &resource.name, None, true),
                },
            });
        }
    }
    for resource in &program.resources {
        let (replicas, sequential, pod, templates, retain) = match &resource.spec {
            Spec::ReplicaSet(s) => (s.replicas, false, &s.pod, &[][..], None),
            Spec::OrderedReplicaSet(s) => (
                s.replicas,
                s.pod_management.unwrap_or(PodManagement::Ordered) == PodManagement::Ordered,
                &s.pod,
                &s.volume_claims[..],
                s.retain.as_ref(),
            ),
            _ => continue,
        };
        let kind = resource.kind();
        let retain_on_removal = retain.and_then(|r| r.on_removal).unwrap_or(true);
        let delete_on_scale_down = !retain.and_then(|r| r.on_scale_down).unwrap_or(true);
        for ordinal in 0..replicas {
            for template in templates {
                let id = volume_id(
                    owner,
                    &program.deployment,
                    Some(&resource.name),
                    &template.name,
                    Some(ordinal),
                );
                translation.volumes.push(DesiredVolume {
                    id,
                    spec: VolumeSpec {
                        group: pod.fs_group,
                        labels: labels(
                            Some(&resource.name),
                            &template.name,
                            Some(ordinal),
                            retain_on_removal,
                        ),
                    },
                });
            }
        }
        let volumes = |ordinal: u32| -> Vec<PodVolume> {
            pod.volumes
                .iter()
                .map(|v| PodVolume {
                    name: v.name.clone(),
                    scratch: v.scratch.as_ref().map(|s| Scratch {
                        medium: match s.medium {
                            ir_v2::Medium::Disk => Medium::Disk,
                            ir_v2::Medium::Memory => Medium::Memory,
                        },
                        bytes: s.bytes,
                    }),
                    secret: v
                        .config
                        .as_ref()
                        .map(|p| ("config", p))
                        .or(v.secret.as_ref().map(|p| ("secret", p)))
                        .map(|(kind, p)| SecretFiles {
                            name: object_ids[&(kind, p.name.as_str())].clone(),
                            items: p.items.as_ref().map(|items| {
                                items
                                    .iter()
                                    .map(|i| SecretItem {
                                        key: i.key.clone(),
                                        path: i.path.clone(),
                                    })
                                    .collect()
                            }),
                            mode: p.mode.unwrap_or(0o644),
                            group: pod.fs_group,
                        }),
                    volume: v.claim.as_ref().map(|claim| {
                        if templates.iter().any(|t| &t.name == claim) {
                            volume_id(
                                owner,
                                &program.deployment,
                                Some(&resource.name),
                                claim,
                                Some(ordinal),
                            )
                        } else {
                            volume_id(owner, &program.deployment, None, claim, None)
                        }
                    }),
                })
                .collect()
        };
        let containers: Vec<ContainerSpec> = pod
            .containers
            .iter()
            .map(|c| {
                let security = c.security.as_ref();
                ContainerSpec {
                    name: c.name.clone(),
                    image: c.image.clone(),
                    command: c.command.clone(),
                    args: c.args.clone(),
                    env: c
                        .env
                        .iter()
                        .filter_map(|e| Some((e.name.clone(), e.value.clone()?)))
                        .collect(),
                    secret_env: c
                        .env
                        .iter()
                        .filter_map(|e| {
                            let r = e.secret.as_ref()?;
                            Some((
                                e.name.clone(),
                                SecretKeyRef {
                                    secret: object_ids[&("secret", r.name.as_str())].clone(),
                                    key: r.key.clone(),
                                },
                            ))
                        })
                        .collect(),
                    user: security.and_then(|s| s.user).or(pod.user),
                    group: security.and_then(|s| s.group).or(pod.group),
                    read_only_root: security.and_then(|s| s.read_only_root) == Some(true),
                    no_new_privileges: security.and_then(|s| s.allow_privilege_escalation)
                        == Some(false),
                    drop_all_capabilities: security
                        .and_then(|s| s.drop_capabilities.as_ref())
                        .is_some_and(|d| d == &["all"]),
                    mounts: c
                        .mounts
                        .iter()
                        .map(|m| MountSpec {
                            volume: m.volume.clone(),
                            path: m.path.clone(),
                            read_only: m.read_only == Some(true),
                            sub_path: m.sub_path.clone(),
                        })
                        .collect(),
                }
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
                    supplementary_groups: pod.fs_group.into_iter().collect(),
                    volumes: volumes(ordinal),
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
        translation.workloads.push(Workload {
            deployment: program.deployment.clone(),
            kind,
            resource: resource.name.clone(),
            sequential,
            delete_on_scale_down,
            pods,
        });
    }
    Ok(translation)
}

/// For each `volume_claim`, the `fs_group`s of the pods that mount it (`None` for a pod without one).
fn claim_groups(program: &Program) -> BTreeMap<&str, BTreeSet<Option<u32>>> {
    let mut groups: BTreeMap<&str, BTreeSet<Option<u32>>> = BTreeMap::new();
    for resource in &program.resources {
        let Some(pod) = resource.pod() else { continue };
        let templates: BTreeSet<&str> = match &resource.spec {
            Spec::OrderedReplicaSet(s) => s.volume_claims.iter().map(|t| t.name.as_str()).collect(),
            _ => BTreeSet::new(),
        };
        for claim in pod.volumes.iter().filter_map(|v| v.claim.as_deref()) {
            if !templates.contains(claim) {
                groups.entry(claim).or_default().insert(pod.fs_group);
            }
        }
    }
    groups
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
            Spec::OrderedReplicaSet(s) => &s.pod,
            Spec::Config(_) => continue,
            Spec::Secret(s) => {
                if s.source.ca.is_some() || s.source.certificate.is_some() {
                    add(
                        format!("{at}/fields/source"),
                        "generated certificate authorities and certificates are",
                        "rezn#54",
                    );
                }
                continue;
            }
            Spec::VolumeClaim(_) => {
                if claim_groups(program)
                    .get(resource.name.as_str())
                    .is_some_and(|groups| groups.len() > 1)
                {
                    add(
                        at,
                        "claims mounted by pods with different fs_group values are",
                        "rezn#67",
                    );
                }
                continue;
            }
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
        if pod.fs_group.is_some_and(|g| g > MAX_ID) {
            add(
                format!("{at}/fs_group"),
                "user and group IDs above 65535 are",
                "orqos#20",
            );
        }
        if !pod.image_pull_secrets.is_empty() {
            add(
                format!("{at}/image_pull_secrets"),
                "registry credentials are",
                "orqos#23",
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
            let security = c.security.as_ref();
            let user = security.and_then(|s| s.user).or(pod.user);
            let group = security.and_then(|s| s.group).or(pod.group);
            if user.is_some_and(|u| u > MAX_ID) || group.is_some_and(|g| g > MAX_ID) {
                add(at.clone(), "user and group IDs above 65535 are", "orqos#20");
            }
            if group.is_some() && user.is_none() {
                add(
                    at.clone(),
                    "a group without a user (the image's user with another group) is",
                    "orqos#20",
                );
            }
            let non_root = security
                .and_then(|s| s.run_as_non_root)
                .or(pod.run_as_non_root);
            if non_root == Some(true) && user.is_none() {
                add(
                    at.clone(),
                    "run_as_non_root without an explicit user (an image user check) is",
                    "orqos#20",
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

    /// Translation with an empty secret store.
    fn translate(owner: &str, program: &Program) -> Result<Translation, Vec<String>> {
        super::translate(owner, program, &Stored::new())
    }

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
        let workloads = translate(OWNER_ID, &program(web(2))).unwrap().workloads;
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
        assert!(translate(OWNER_ID, &program(ordered)).unwrap().workloads[1].sequential);
        assert!(translate(OWNER_ID, &program(web(0))).unwrap().workloads[0]
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
        let workloads = translate(OWNER_ID, &program(value)).unwrap().workloads;
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
        let spec = &translate(OWNER_ID, &program(lone)).unwrap().workloads[0].pods[0].spec;
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

    /// An ordered set with a per-replica claim, a shared claim, scratch, identity and hardening.
    fn stateful() -> serde_json::Value {
        json!({"ir": "rezn.dev/v2", "deployment": "shop", "namespace": "store", "resources": [
            {"kind": "volume_claim", "name": "shared", "fields": {"bytes": 1048576}},
            {"kind": "ordered_replica_set", "name": "db", "fields": {"replicas": 2, "service": "db",
                "volume_claims": [{"name": "data", "bytes": 1048576}],
                "retain": {"on_scale_down": false, "on_removal": false},
                "pod": {"user": 1000, "group": 1000, "fs_group": 2000, "fs_group_policy": "on_root_mismatch",
                    "run_as_non_root": true, "seccomp": "runtime_default",
                    "volumes": [{"name": "data", "claim": "data"}, {"name": "shared", "claim": "shared"},
                                {"name": "tmp", "scratch": {"medium": "memory", "bytes": 1024}}],
                    "containers": [{"name": "db", "image": "postgres:17",
                        "security": {"user": 1001, "read_only_root": true, "allow_privilege_escalation": false,
                                     "drop_capabilities": ["all"]},
                        "mounts": [{"volume": "data", "path": "/var/lib/db", "sub_path": "v1"},
                                   {"volume": "shared", "path": "/shared", "read_only": true},
                                   {"volume": "tmp", "path": "/tmp"}]}]}}},
            {"kind": "service", "name": "db", "fields": {"target": "db", "headless": true, "ports": []}}
        ]})
    }

    #[test]
    fn storage_and_identity_translate() {
        let t = translate(OWNER_ID, &program(stateful())).unwrap();
        let shared = volume_id(OWNER_ID, "shop", None, "shared", None);
        let data = |n| volume_id(OWNER_ID, "shop", Some("db"), "data", Some(n));
        assert_eq!(
            t.volumes.iter().map(|v| v.id.clone()).collect::<Vec<_>>(),
            [shared.clone(), data(0), data(1)]
        );
        assert_eq!(t.volumes[0].spec.group, Some(2000));
        assert_eq!(t.volumes[0].spec.labels[RETAIN], "true");
        assert!(!t.volumes[0].spec.labels.contains_key(ORDINAL));
        let replica = &t.volumes[2].spec;
        assert_eq!(replica.group, Some(2000));
        assert_eq!(
            (
                replica.labels[RETAIN].as_str(),
                replica.labels[WORKLOAD].as_str(),
                replica.labels[ORDINAL].as_str(),
                replica.labels[CLAIM].as_str()
            ),
            ("false", "db", "1", "data")
        );
        assert_ne!(data(0), volume_id(OWNER_ID, "shop", None, "data", Some(0)));

        let db = &t.workloads[0];
        assert!(db.delete_on_scale_down);
        let spec = &db.pods[1].spec;
        assert_eq!(spec.supplementary_groups, [2000]);
        assert_eq!(
            spec.volumes,
            [
                PodVolume {
                    name: "data".into(),
                    volume: Some(data(1)),
                    scratch: None,
                    secret: None
                },
                PodVolume {
                    name: "shared".into(),
                    volume: Some(shared),
                    scratch: None,
                    secret: None
                },
                PodVolume {
                    name: "tmp".into(),
                    volume: None,
                    scratch: Some(Scratch {
                        medium: Medium::Memory,
                        bytes: 1024
                    }),
                    secret: None
                },
            ]
        );
        let c = &spec.containers[0];
        assert_eq!((c.user, c.group), (Some(1001), Some(1000)));
        assert!(c.read_only_root && c.no_new_privileges && c.drop_all_capabilities);
        assert_eq!(
            c.mounts[0],
            MountSpec {
                volume: "data".into(),
                path: "/var/lib/db".into(),
                read_only: false,
                sub_path: Some("v1".into())
            }
        );
        assert!(c.mounts[1].read_only);

        // Defaults: retain everything, no hardening unless asked.
        let mut plain = stateful();
        plain["resources"][1]["fields"]
            .as_object_mut()
            .unwrap()
            .remove("retain");
        plain["resources"][1]["fields"]["pod"]["containers"][0]
            .as_object_mut()
            .unwrap()
            .remove("security");
        let t = translate(OWNER_ID, &program(plain)).unwrap();
        assert!(!t.workloads[0].delete_on_scale_down);
        assert_eq!(t.volumes[1].spec.labels[RETAIN], "true");
        let c = &t.workloads[0].pods[0].spec.containers[0];
        assert_eq!((c.user, c.group), (Some(1000), Some(1000)));
        assert!(!c.read_only_root && !c.no_new_privileges && !c.drop_all_capabilities);
    }

    #[test]
    fn identity_and_storage_that_need_more_are_reported() {
        let pod = "/resources/1/fields/pod";
        let reasons = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut v = stateful();
            edit(&mut v["resources"][1]["fields"]["pod"]);
            unsupported(&program(v))
        };
        assert_eq!(
            reasons(&|p| {
                p.as_object_mut().unwrap().remove("user");
                p["containers"][0]["security"].as_object_mut().unwrap().remove("user");
            }),
            [
                format!("{pod}/containers/0: a group without a user (the image's user with another group) is not executed yet (orqos#20)"),
                format!("{pod}/containers/0: run_as_non_root without an explicit user (an image user check) is not executed yet (orqos#20)"),
            ]
        );
        assert_eq!(
            reasons(&|p| p["containers"][0]["security"]["user"] = json!(70000)),
            [format!("{pod}/containers/0: user and group IDs above 65535 are not executed yet (orqos#20)")]
        );
        assert_eq!(
            reasons(&|p| p["fs_group"] = json!(70000)),
            [format!(
                "{pod}/fs_group: user and group IDs above 65535 are not executed yet (orqos#20)"
            )]
        );

        // A shared claim mounted by pods with different fs_groups cannot get one group.
        let mut v = stateful();
        v["resources"].as_array_mut().unwrap().push(json!(
            {"kind": "replica_set", "name": "reader", "fields": {"replicas": 1, "pod": {
                "volumes": [{"name": "shared", "claim": "shared"}],
                "containers": [{"name": "r", "image": "alpine", "mounts": [{"volume": "shared", "path": "/s"}]}]}}}));
        assert_eq!(
            unsupported(&program(v)),
            ["/resources/0: claims mounted by pods with different fs_group values are not executed yet (rezn#67)"]
        );
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
            translate(OWNER_ID, &example("runnable.json"))
                .unwrap()
                .workloads[0]
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

    /// A config, a stored secret and two workloads: `api` uses both, `web` neither.
    fn configured(settings: &str) -> serde_json::Value {
        json!({"ir": "rezn.dev/v2", "deployment": "shop", "namespace": "store", "resources": [
            {"kind": "config", "name": "settings", "fields": {"data": {"app.json": settings}}},
            {"kind": "secret", "name": "db", "fields": {"type": "opaque", "keys": ["password", "user"], "source": {"store": {}}}},
            {"kind": "replica_set", "name": "api", "fields": {"replicas": 1, "pod": {
                "fs_group": 2000,
                "volumes": [{"name": "conf", "config": {"name": "settings"}},
                            {"name": "creds", "secret": {"name": "db", "items": [{"key": "user", "path": "u/name"}], "mode": 256}}],
                "containers": [{"name": "api", "image": "registry.example.com/api:1",
                    "env": [{"name": "PW", "secret": {"name": "db", "key": "password"}}],
                    "mounts": [{"volume": "conf", "path": "/etc/api"}, {"volume": "creds", "path": "/run/creds", "sub_path": "u"}]}]}}},
            {"kind": "replica_set", "name": "web", "fields": {"replicas": 1, "pod": {
                "containers": [{"name": "web", "image": "nginx:1.27"}]}}}
        ]})
    }

    fn stored(password: &str) -> Stored {
        BTreeMap::from([(
            "db".to_string(),
            BTreeMap::from([
                ("password".to_string(), password.to_string()),
                ("user".to_string(), "v-user".to_string()),
                ("undeclared".to_string(), "v-x".to_string()),
            ]),
        )])
    }

    #[test]
    fn configs_and_secrets_become_content_addressed_objects() {
        let t = super::translate(OWNER_ID, &program(configured("{}")), &stored("v1")).unwrap();
        assert_eq!(t.objects.len(), 2);
        let (config, secret) = (&t.objects[0], &t.objects[1]);
        assert_eq!(
            config.source,
            ObjectSource::Config(BTreeMap::from([("app.json".into(), "{}".into())]))
        );
        assert_eq!(
            secret.source,
            ObjectSource::Secret {
                namespace: "store".into(),
                name: "db".into(),
                // Declared keys only.
                versions: BTreeMap::from([
                    ("password".into(), "v1".into()),
                    ("user".into(), "v-user".into())
                ]),
            }
        );
        assert_eq!(secret.labels[KIND], "secret");
        assert_eq!(secret.labels[RESOURCE], "db");
        assert_eq!(
            secret.id,
            object_id(OWNER_ID, "shop", "secret", "db", &secret.labels[CONTENT])
        );
        let api = &t.workloads[0].pods[0].spec;
        assert_eq!(
            api.volumes[0].secret,
            Some(SecretFiles {
                name: config.id.clone(),
                items: None,
                mode: 0o644,
                group: Some(2000)
            })
        );
        assert_eq!(
            api.volumes[1].secret,
            Some(SecretFiles {
                name: secret.id.clone(),
                items: Some(vec![SecretItem {
                    key: "user".into(),
                    path: "u/name".into()
                }]),
                mode: 0o400,
                group: Some(2000),
            })
        );
        assert_eq!(api.containers[0].mounts[1].sub_path.as_deref(), Some("u"));
        assert_eq!(
            api.containers[0].secret_env,
            BTreeMap::from([(
                "PW".into(),
                SecretKeyRef {
                    secret: secret.id.clone(),
                    key: "password".into()
                }
            )])
        );
        assert!(api.containers[0].env.is_empty());

        // A changed config or secret value changes exactly the objects and specs of the pods using them.
        let changed_config =
            super::translate(OWNER_ID, &program(configured("{\"v\":2}")), &stored("v1")).unwrap();
        let changed_secret =
            super::translate(OWNER_ID, &program(configured("{}")), &stored("v2")).unwrap();
        assert_ne!(changed_config.objects[0].id, config.id);
        assert_eq!(changed_config.objects[1].id, secret.id);
        assert_eq!(changed_secret.objects[0].id, config.id);
        assert_ne!(changed_secret.objects[1].id, secret.id);
        for other in [&changed_config, &changed_secret] {
            assert_ne!(other.workloads[0].pods[0].spec, *api);
            assert_eq!(other.workloads[1].pods[0].spec, t.workloads[1].pods[0].spec);
        }
        // Other owners and deployments get other objects for the same content.
        assert_ne!(
            object_id("1".repeat(64).as_str(), "shop", "secret", "db", "c"),
            object_id(OWNER_ID, "shop", "secret", "db", "c")
        );
        assert_ne!(
            object_id(OWNER_ID, "shop2", "secret", "db", "c"),
            object_id(OWNER_ID, "shop", "secret", "db", "c")
        );
    }

    #[test]
    fn missing_secret_keys_block_the_deployment_with_the_fix() {
        let only_user: Stored = BTreeMap::from([(
            "db".to_string(),
            BTreeMap::from([("user".to_string(), "v".to_string())]),
        )]);
        let reasons =
            super::translate(OWNER_ID, &program(configured("{}")), &only_user).unwrap_err();
        assert_eq!(
            reasons,
            [
                r#"/resources/1: key "password" of secret "db" is not in the secret store (set it with PUT /namespaces/store/secrets/db)"#
            ]
        );
        let p = program(configured("{}"));
        let missing = missing_keys(&p, &Stored::new());
        assert_eq!(
            missing.iter().map(|m| m.key).collect::<Vec<_>>(),
            ["password", "user"]
        );
        assert!(missing
            .iter()
            .all(|m| !m.generated && m.secret == "db" && m.index == 1));

        // Every declared key is referenced by a volume without items; image pull secrets and ingress TLS count too.
        let reference = example("reference.json");
        let referenced = referenced_keys(&reference);
        assert_eq!(
            referenced["registry"],
            BTreeSet::from([".dockerconfigjson"])
        );
        assert_eq!(
            referenced["api-tls"],
            BTreeSet::from(["tls.crt", "tls.key"])
        );
        assert_eq!(
            referenced["edge-tls"],
            BTreeSet::from(["tls.crt", "tls.key"])
        );
        assert!(env_keys(&reference).contains(&("db", "password")));
        // Certificate secrets wait for rezn#54 instead of being reported missing.
        let missing: Vec<&str> = missing_keys(&reference, &Stored::new())
            .iter()
            .map(|m| m.secret)
            .collect();
        assert!(
            !missing.contains(&"api-tls")
                && missing.contains(&"registry")
                && missing.contains(&"db")
        );
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
            "generated certificate authorities and certificates are",
            "jobs are",
            "init containers are",
            "resource requests and limits are",
            "revision phases are",
            "ingresses are",
            "network policies are",
        ] {
            assert!(reasons.contains(expected), "{expected}:\n{reasons}");
        }
        // Storage, identity, configs and secrets run now.
        for gone in [
            "mounts are",
            "volume claims are",
            "security settings",
            "IDs above",
            "without",
            "secrets are",
            "config and secret volumes",
            "secret references",
        ] {
            assert!(!reasons.contains(gone), "{gone}:\n{reasons}");
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
