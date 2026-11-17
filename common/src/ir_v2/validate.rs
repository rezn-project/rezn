//! IR v2 validation: decoding with JSON-pointer errors, the value domains of spec/ir-v2.schema.json, and the
//! cross-reference rules of docs/IR-v2.md ("Validation beyond the schema"). All issues are collected.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::Ipv4Addr;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use super::model::*;
use crate::image::validate_image;

/// One validation problem, located by a JSON pointer into the program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub pointer: String,
    pub message: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pointer = if self.pointer.is_empty() {
            "/"
        } else {
            &self.pointer
        };
        write!(f, "{pointer}: {}", self.message)
    }
}

/// Joins issues into one message, for HTTP responses and errors.
pub fn describe(issues: &[Issue]) -> String {
    issues
        .iter()
        .map(Issue::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

const MAX_ID: u32 = 4_294_967_294;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProgram {
    ir: String,
    deployment: String,
    namespace: String,
    resources: Vec<RawResource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResource {
    kind: Kind,
    name: String,
    phase: Option<Phase>,
    fields: Value,
}

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn decode<T: DeserializeOwned>(value: &Value, prefix: &str) -> Result<T, Issue> {
    serde_path_to_error::deserialize(value).map_err(|error| {
        let mut pointer = prefix.to_string();
        for segment in error.path().iter() {
            match segment {
                serde_path_to_error::Segment::Seq { index } => {
                    pointer.push_str(&format!("/{index}"))
                }
                serde_path_to_error::Segment::Map { key } => {
                    pointer.push('/');
                    pointer.push_str(&escape(key));
                }
                _ => {}
            }
        }
        Issue {
            pointer,
            message: error.inner().to_string(),
        }
    })
}

/// Validates an IR v2 program. Returns the typed program, or every issue found.
pub fn validate(program: &Value) -> Result<Program, Vec<Issue>> {
    if !program.is_object() {
        return Err(vec![Issue {
            pointer: String::new(),
            message: "an IR v2 program is a JSON object".into(),
        }]);
    }
    let raw: RawProgram = decode(program, "").map_err(|issue| vec![issue])?;
    let mut issues = Vec::new();
    let mut resources = Vec::new();
    for (i, resource) in raw.resources.into_iter().enumerate() {
        let at = format!("/resources/{i}/fields");
        let spec = match resource.kind {
            Kind::ReplicaSet => decode(&resource.fields, &at).map(Spec::ReplicaSet),
            Kind::OrderedReplicaSet => decode(&resource.fields, &at).map(Spec::OrderedReplicaSet),
            Kind::Job => decode(&resource.fields, &at).map(Spec::Job),
            Kind::ScheduledJob => decode(&resource.fields, &at).map(Spec::ScheduledJob),
            Kind::Service => decode(&resource.fields, &at).map(Spec::Service),
            Kind::VolumeClaim => decode(&resource.fields, &at).map(Spec::VolumeClaim),
            Kind::Config => decode(&resource.fields, &at).map(Spec::Config),
            Kind::Secret => decode(&resource.fields, &at).map(Spec::Secret),
            Kind::Ingress => decode(&resource.fields, &at).map(Spec::Ingress),
            Kind::NetworkPolicy => decode(&resource.fields, &at).map(Spec::NetworkPolicy),
        };
        match spec {
            Ok(spec) => resources.push(Resource {
                name: resource.name,
                phase: resource.phase,
                spec,
            }),
            Err(issue) => issues.push(issue),
        }
    }
    if !issues.is_empty() {
        return Err(issues);
    }
    let program = Program {
        ir: raw.ir,
        deployment: raw.deployment,
        namespace: raw.namespace,
        resources,
    };
    let mut checker = Checker::new(&program);
    checker.run();
    if checker.issues.is_empty() {
        Ok(program)
    } else {
        Err(checker.issues)
    }
}

/// DNS-1123 label of at most `max` characters.
pub fn is_name(value: &str, max: usize) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= max
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

fn is_port_name(value: &str) -> bool {
    is_name(value, 15) && value.bytes().any(|b| b.is_ascii_lowercase())
}

fn is_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
}

fn is_env_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn is_relative_path(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('/') && value.split('/').all(|part| part != "..")
}

fn is_cron(value: &str) -> bool {
    let fields: Vec<_> = value.split(' ').collect();
    fields.len() == 5
        && fields.iter().all(|f| {
            !f.is_empty()
                && f.bytes()
                    .all(|b| b.is_ascii_digit() || b"*/,-".contains(&b))
        })
}

fn is_cidr(value: &str) -> bool {
    let Some((address, prefix)) = value.split_once('/') else {
        return false;
    };
    let (Ok(address), Ok(prefix)) = (address.parse::<Ipv4Addr>(), prefix.parse::<u8>()) else {
        return false;
    };
    if prefix > 32 || prefix.to_string().len() != value.len() - value.find('/').unwrap() - 1 {
        return false;
    }
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    u32::from(address) & !mask == 0
}

fn is_host(value: &str) -> bool {
    !value.is_empty() && !value.bytes().any(|b| b.is_ascii_whitespace())
}

fn exactly_one(present: &[bool]) -> bool {
    present.iter().filter(|p| **p).count() == 1
}

struct Checker<'a> {
    program: &'a Program,
    issues: Vec<Issue>,
    secrets: BTreeMap<&'a str, &'a Secret>,
    configs: BTreeMap<&'a str, &'a Config>,
    claims: BTreeSet<&'a str>,
    services: BTreeMap<&'a str, &'a Service>,
    workloads: BTreeMap<&'a str, &'a Resource>,
}

impl<'a> Checker<'a> {
    fn new(program: &'a Program) -> Self {
        let mut checker = Self {
            program,
            issues: Vec::new(),
            secrets: BTreeMap::new(),
            configs: BTreeMap::new(),
            claims: BTreeSet::new(),
            services: BTreeMap::new(),
            workloads: BTreeMap::new(),
        };
        for resource in &program.resources {
            let name = resource.name.as_str();
            match &resource.spec {
                Spec::Secret(s) => {
                    checker.secrets.entry(name).or_insert(s);
                }
                Spec::Config(c) => {
                    checker.configs.entry(name).or_insert(c);
                }
                Spec::VolumeClaim(_) => {
                    checker.claims.insert(name);
                }
                Spec::Service(s) => {
                    checker.services.entry(name).or_insert(s);
                }
                _ => {
                    checker.workloads.entry(name).or_insert(resource);
                }
            }
        }
        checker
    }

    fn add(&mut self, pointer: impl Into<String>, message: impl Into<String>) {
        self.issues.push(Issue {
            pointer: pointer.into(),
            message: message.into(),
        });
    }

    fn name(&mut self, pointer: impl Into<String>, value: &str, max: usize) {
        if !is_name(value, max) {
            self.add(
                pointer,
                format!("{value:?} is not a name (lowercase letters, digits and '-', at most {max} characters)"),
            );
        }
    }

    fn positive(&mut self, pointer: impl Into<String>, value: u64) {
        if value == 0 {
            self.add(pointer, "must be at least 1");
        }
    }

    fn id(&mut self, pointer: impl Into<String>, value: Option<u32>) {
        if value.is_some_and(|v| v > MAX_ID) {
            self.add(pointer, "user and group IDs are at most 4294967294");
        }
    }

    fn run(&mut self) {
        let program = self.program;
        if program.ir != IR {
            self.add("/ir", format!("expected {IR:?}"));
        }
        self.name("/deployment", &program.deployment, 63);
        self.name("/namespace", &program.namespace, 63);
        let mut workload_names = BTreeSet::new();
        let mut names: BTreeMap<Kind, BTreeSet<&str>> = BTreeMap::new();
        for (i, resource) in program.resources.iter().enumerate() {
            let at = format!("/resources/{i}");
            let kind = resource.kind();
            let max = if matches!(kind, Kind::OrderedReplicaSet | Kind::ScheduledJob) {
                52
            } else {
                63
            };
            self.name(format!("{at}/name"), &resource.name, max);
            let unique = if kind.is_workload() {
                workload_names.insert(resource.name.as_str())
            } else {
                names
                    .entry(kind)
                    .or_default()
                    .insert(resource.name.as_str())
            };
            if !unique {
                self.add(
                    format!("{at}/name"),
                    format!("duplicate {} name {:?}", kind.as_str(), resource.name),
                );
            }
            if let Some(phase) = &resource.phase {
                let stages: BTreeSet<_> = phase.on.iter().collect();
                if phase.on.is_empty() || stages.len() != phase.on.len() {
                    self.add(
                        format!("{at}/phase/on"),
                        "list install and/or upgrade once each",
                    );
                }
            }
            self.resource(&format!("{at}/fields"), resource);
        }
    }

    fn resource(&mut self, at: &str, resource: &'a Resource) {
        match &resource.spec {
            Spec::ReplicaSet(s) => {
                self.pod(&format!("{at}/pod"), &s.pod, None);
                if s.pod.restart() != Restart::Always {
                    self.add(
                        format!("{at}/pod/restart"),
                        "replica sets use restart: always",
                    );
                }
            }
            Spec::OrderedReplicaSet(s) => {
                self.ordered(at, &resource.name, s);
            }
            Spec::Job(s) => self.job(at, s),
            Spec::ScheduledJob(s) => {
                if !is_cron(&s.schedule) {
                    self.add(
                        format!("{at}/schedule"),
                        "expected a five-field cron expression",
                    );
                }
                self.job(&format!("{at}/job"), &s.job);
            }
            Spec::Service(s) => self.service(at, s),
            Spec::VolumeClaim(c) => self.positive(format!("{at}/bytes"), c.bytes),
            Spec::Config(c) => {
                for key in c.data.keys() {
                    if !is_key(key) {
                        self.add(format!("{at}/data/{}", escape(key)), "invalid key");
                    }
                }
            }
            Spec::Secret(s) => self.secret(at, s),
            Spec::Ingress(s) => self.ingress(at, s),
            Spec::NetworkPolicy(p) => self.policy(at, p),
        }
    }

    fn ordered(&mut self, at: &str, name: &str, s: &'a OrderedReplicaSet) {
        self.name(format!("{at}/service"), &s.service, 63);
        let mut templates = BTreeSet::new();
        for (j, claim) in s.volume_claims.iter().enumerate() {
            self.name(format!("{at}/volume_claims/{j}/name"), &claim.name, 63);
            self.positive(format!("{at}/volume_claims/{j}/bytes"), claim.bytes);
            if !templates.insert(claim.name.as_str()) {
                self.add(
                    format!("{at}/volume_claims/{j}/name"),
                    "duplicate volume claim",
                );
            }
        }
        self.pod(&format!("{at}/pod"), &s.pod, Some(&templates));
        if s.pod.restart() != Restart::Always {
            self.add(
                format!("{at}/pod/restart"),
                "ordered replica sets use restart: always",
            );
        }
        let headless = self
            .services
            .get(s.service.as_str())
            .is_some_and(|svc| svc.headless == Some(true) && svc.target == name);
        if !headless {
            self.add(
                format!("{at}/service"),
                format!(
                    "needs a headless service {:?} whose target is {name:?}",
                    s.service
                ),
            );
        }
    }

    fn job(&mut self, at: &str, s: &'a JobSpec) {
        self.pod(&format!("{at}/pod"), &s.pod, None);
        if s.pod.restart() == Restart::Always {
            self.add(
                format!("{at}/pod/restart"),
                "job pods use restart: never or on_failure",
            );
        }
    }

    fn service(&mut self, at: &str, s: &'a Service) {
        self.name(format!("{at}/target"), &s.target, 63);
        let target = self.workloads.get(s.target.as_str()).copied();
        let pod = match target.map(|t| (t.kind(), t.pod())) {
            Some((Kind::ReplicaSet | Kind::OrderedReplicaSet, pod)) => pod,
            _ => {
                self.add(
                    format!("{at}/target"),
                    format!("{:?} is not a replica set in this program", s.target),
                );
                None
            }
        };
        let container_ports: BTreeSet<&str> = pod
            .map(|p| {
                p.containers
                    .iter()
                    .chain(&p.sidecars)
                    .flat_map(|c| c.ports.iter().map(|port| port.name.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let mut seen = BTreeSet::new();
        for (j, port) in s.ports.iter().enumerate() {
            let here = format!("{at}/ports/{j}");
            if !is_port_name(&port.name) {
                self.add(format!("{here}/name"), "invalid port name");
            }
            if !seen.insert(port.name.as_str()) {
                self.add(format!("{here}/name"), "duplicate port name");
            }
            self.positive(format!("{here}/port"), port.port.into());
            match &port.target_port {
                PortRef::Number(n) => self.positive(format!("{here}/target_port"), (*n).into()),
                PortRef::Name(name) => {
                    if pod.is_some() && !container_ports.contains(name.as_str()) {
                        self.add(
                            format!("{here}/target_port"),
                            format!("no container port named {name:?} in the target"),
                        );
                    }
                }
            }
        }
    }

    fn secret(&mut self, at: &str, s: &'a Secret) {
        let keys: BTreeSet<&str> = s.keys.iter().map(String::as_str).collect();
        if s.keys.is_empty() || keys.len() != s.keys.len() || !s.keys.iter().all(|k| is_key(k)) {
            self.add(format!("{at}/keys"), "declare one or more unique keys");
        }
        if s.kind == SecretType::Tls && keys != BTreeSet::from(["tls.crt", "tls.key"]) {
            self.add(
                format!("{at}/keys"),
                "tls secrets declare exactly tls.crt and tls.key",
            );
        }
        let source = &s.source;
        let present = [
            source.store.is_some(),
            source.seed_once.is_some(),
            source.random.is_some(),
            source.ca.is_some(),
            source.certificate.is_some(),
        ];
        if !exactly_one(&present) {
            self.add(format!("{at}/source"), "exactly one source");
        }
        if let Some(random) = &source.random {
            if random.keys().map(String::as_str).collect::<BTreeSet<_>>() != keys {
                self.add(
                    format!("{at}/source/random"),
                    "a random source covers exactly the declared keys",
                );
            }
            for (key, r) in random {
                if !(16..=4096).contains(&r.length) {
                    self.add(
                        format!("{at}/source/random/{}/length", escape(key)),
                        "length is 16 to 4096",
                    );
                }
            }
        }
        if let Some(ca) = &source.ca {
            if ca.common_name.is_empty() {
                self.add(format!("{at}/source/ca/common_name"), "must not be empty");
            }
            self.positive(format!("{at}/source/ca/days"), ca.days.into());
        }
        if let Some(cert) = &source.certificate {
            let names: BTreeSet<_> = cert.dns_names.iter().collect();
            if cert.dns_names.is_empty()
                || names.len() != cert.dns_names.len()
                || !cert.dns_names.iter().all(|n| is_host(n))
            {
                self.add(
                    format!("{at}/source/certificate/dns_names"),
                    "one or more unique DNS names",
                );
            }
            self.positive(format!("{at}/source/certificate/days"), cert.days.into());
            let is_ca = self
                .secrets
                .get(cert.ca.as_str())
                .is_some_and(|ca| ca.source.ca.is_some());
            if !is_ca {
                self.add(
                    format!("{at}/source/certificate/ca"),
                    format!("{:?} is not a ca-sourced secret", cert.ca),
                );
            }
        }
        if (source.ca.is_some() || source.certificate.is_some()) && s.kind != SecretType::Tls {
            self.add(
                format!("{at}/type"),
                "ca and certificate sources produce tls secrets",
            );
        }
    }

    fn ingress(&mut self, at: &str, s: &'a Ingress) {
        if s.hosts.is_empty() {
            self.add(format!("{at}/hosts"), "one or more hosts");
        }
        for (j, host) in s.hosts.iter().enumerate() {
            let here = format!("{at}/hosts/{j}");
            if !is_host(&host.host) {
                self.add(format!("{here}/host"), "invalid host");
            }
            if host.paths.is_empty() {
                self.add(format!("{here}/paths"), "one or more paths");
            }
            for (k, path) in host.paths.iter().enumerate() {
                let p = format!("{here}/paths/{k}");
                if !path.path.starts_with('/') {
                    self.add(format!("{p}/path"), "paths are absolute");
                }
                match self.services.get(path.service.as_str()) {
                    None => self.add(
                        format!("{p}/service"),
                        format!("no service {:?}", path.service),
                    ),
                    Some(svc) => {
                        if !svc.ports.iter().any(|port| port.name == path.port) {
                            self.add(
                                format!("{p}/port"),
                                format!("service {:?} has no port {:?}", path.service, path.port),
                            );
                        }
                    }
                }
            }
        }
        for (j, tls) in s.tls.iter().enumerate() {
            if tls.hosts.is_empty() || !tls.hosts.iter().all(|h| is_host(h)) {
                self.add(format!("{at}/tls/{j}/hosts"), "one or more hosts");
            }
            let is_tls = self
                .secrets
                .get(tls.secret.as_str())
                .is_some_and(|secret| secret.kind == SecretType::Tls);
            if !is_tls {
                self.add(
                    format!("{at}/tls/{j}/secret"),
                    format!("{:?} is not a tls secret", tls.secret),
                );
            }
        }
    }

    fn policy(&mut self, at: &str, p: &'a NetworkPolicy) {
        let targets: BTreeSet<_> = p.targets.iter().collect();
        if p.targets.is_empty() || targets.len() != p.targets.len() {
            self.add(format!("{at}/targets"), "one or more unique workloads");
        }
        for (j, target) in p.targets.iter().enumerate() {
            if !self.workloads.contains_key(target.as_str()) {
                self.add(
                    format!("{at}/targets/{j}"),
                    format!("no workload {target:?}"),
                );
            }
        }
        if p.ingress.is_none() && p.egress.is_none() {
            self.add(at.to_string(), "a policy restricts ingress, egress or both");
        }
        let ingress = p
            .ingress
            .iter()
            .flatten()
            .map(|r| (r.from.as_ref(), r.ports.as_ref()));
        let egress = p
            .egress
            .iter()
            .flatten()
            .map(|r| (r.to.as_ref(), r.ports.as_ref()));
        let rules: Vec<_> = ingress
            .enumerate()
            .map(|(j, r)| (format!("{at}/ingress/{j}"), "from", r))
            .chain(
                egress
                    .enumerate()
                    .map(|(j, r)| (format!("{at}/egress/{j}"), "to", r)),
            )
            .collect();
        for (here, field, (peers, ports)) in rules {
            for (k, peer) in peers.into_iter().flatten().enumerate() {
                let pp = format!("{here}/{field}/{k}");
                if !exactly_one(&[peer.workload.is_some(), peer.cidr.is_some()]) {
                    self.add(pp.clone(), "a peer is exactly one of workload or cidr");
                }
                if let Some(workload) = &peer.workload {
                    if !self.workloads.contains_key(workload.as_str()) {
                        self.add(
                            format!("{pp}/workload"),
                            format!("no workload {workload:?}"),
                        );
                    }
                }
                if peer.cidr.as_deref().is_some_and(|c| !is_cidr(c)) {
                    self.add(
                        format!("{pp}/cidr"),
                        "expected an IPv4 network such as 10.0.0.0/8",
                    );
                }
            }
            for (k, port) in ports.into_iter().flatten().enumerate() {
                self.positive(format!("{here}/ports/{k}/port"), port.port.into());
            }
        }
    }

    fn pod(&mut self, at: &str, pod: &'a Pod, templates: Option<&BTreeSet<&str>>) {
        if pod.containers.is_empty() {
            self.add(format!("{at}/containers"), "one or more containers");
        }
        self.id(format!("{at}/user"), pod.user);
        self.id(format!("{at}/group"), pod.group);
        self.id(format!("{at}/fs_group"), pod.fs_group);
        let mut pull = BTreeSet::new();
        for (j, name) in pod.image_pull_secrets.iter().enumerate() {
            let here = format!("{at}/image_pull_secrets/{j}");
            if !pull.insert(name.as_str()) {
                self.add(here.clone(), "duplicate pull secret");
            }
            let ok = self
                .secrets
                .get(name.as_str())
                .is_some_and(|s| s.keys.iter().any(|k| k == ".dockerconfigjson"));
            if !ok {
                self.add(
                    here,
                    format!("{name:?} is not a secret declaring .dockerconfigjson"),
                );
            }
        }
        let mut volumes = BTreeSet::new();
        for (j, volume) in pod.volumes.iter().enumerate() {
            self.volume(&format!("{at}/volumes/{j}"), volume, templates);
            if !volumes.insert(volume.name.as_str()) {
                self.add(format!("{at}/volumes/{j}/name"), "duplicate volume");
            }
        }
        let mut container_names = BTreeSet::new();
        let mut port_names = BTreeSet::new();
        for (field, j, container) in pod.all_containers() {
            let here = format!("{at}/{field}/{j}");
            if !container_names.insert(container.name.as_str()) {
                self.add(
                    format!("{here}/name"),
                    "duplicate container name in the pod",
                );
            }
            for (k, port) in container.ports.iter().enumerate() {
                if !port_names.insert(port.name.as_str()) {
                    self.add(
                        format!("{here}/ports/{k}/name"),
                        "duplicate port name in the pod",
                    );
                }
            }
            self.container(&here, pod, container, &volumes);
        }
    }

    fn volume(&mut self, at: &str, volume: &'a Volume, templates: Option<&BTreeSet<&str>>) {
        self.name(format!("{at}/name"), &volume.name, 63);
        let present = [
            volume.scratch.is_some(),
            volume.claim.is_some(),
            volume.config.is_some(),
            volume.secret.is_some(),
        ];
        if !exactly_one(&present) {
            self.add(at.to_string(), "a volume has exactly one source");
        }
        if let Some(scratch) = &volume.scratch {
            self.positive(format!("{at}/scratch/bytes"), scratch.bytes);
        }
        if let Some(claim) = &volume.claim {
            let known = self.claims.contains(claim.as_str())
                || templates.is_some_and(|t| t.contains(claim.as_str()));
            if !known {
                self.add(
                    format!("{at}/claim"),
                    format!("no volume_claim or volume claim template {claim:?}"),
                );
            }
        }
        if let Some(projection) = &volume.config {
            let keys = self
                .configs
                .get(projection.name.as_str())
                .map(|c| c.data.keys().map(String::as_str).collect::<BTreeSet<_>>());
            self.projection(&format!("{at}/config"), projection, "config", keys);
        }
        if let Some(projection) = &volume.secret {
            let keys = self
                .secrets
                .get(projection.name.as_str())
                .map(|s| s.keys.iter().map(String::as_str).collect::<BTreeSet<_>>());
            self.projection(&format!("{at}/secret"), projection, "secret", keys);
        }
    }

    fn projection(&mut self, at: &str, p: &Projection, kind: &str, keys: Option<BTreeSet<&str>>) {
        if p.mode.is_some_and(|m| m > 0o777) {
            self.add(format!("{at}/mode"), "file modes are at most 511 (0777)");
        }
        let Some(keys) = keys else {
            self.add(format!("{at}/name"), format!("no {kind} {:?}", p.name));
            return;
        };
        if let Some(items) = &p.items {
            if items.is_empty() {
                self.add(format!("{at}/items"), "omit items or list one or more");
            }
            for (k, item) in items.iter().enumerate() {
                if !keys.contains(item.key.as_str()) {
                    self.add(
                        format!("{at}/items/{k}/key"),
                        format!("{kind} {:?} has no key {:?}", p.name, item.key),
                    );
                }
                if !is_relative_path(&item.path) {
                    self.add(
                        format!("{at}/items/{k}/path"),
                        "a relative path without '..'",
                    );
                }
            }
        }
    }

    fn container(&mut self, at: &str, pod: &Pod, c: &'a Container, volumes: &BTreeSet<&str>) {
        self.name(format!("{at}/name"), &c.name, 63);
        if let Err(e) = validate_image(&c.image) {
            self.add(format!("{at}/image"), format!("{e:#}"));
        }
        let mut env_names = BTreeSet::new();
        for (k, env) in c.env.iter().enumerate() {
            let here = format!("{at}/env/{k}");
            if !is_env_name(&env.name) {
                self.add(format!("{here}/name"), "invalid environment variable name");
            }
            if !env_names.insert(env.name.as_str()) {
                self.add(format!("{here}/name"), "duplicate environment variable");
            }
            if !exactly_one(&[
                env.value.is_some(),
                env.secret.is_some(),
                env.pod_ip.is_some(),
            ]) {
                self.add(here.clone(), "exactly one of value, secret or pod_ip");
            }
            if env.pod_ip == Some(false) {
                self.add(format!("{here}/pod_ip"), "pod_ip is true when present");
            }
            if let Some(reference) = &env.secret {
                let declared = self
                    .secrets
                    .get(reference.name.as_str())
                    .is_some_and(|s| s.keys.contains(&reference.key));
                if !declared {
                    self.add(
                        format!("{here}/secret"),
                        format!(
                            "no secret {:?} declaring key {:?}",
                            reference.name, reference.key
                        ),
                    );
                }
            }
        }
        let own_ports: BTreeSet<&str> = c.ports.iter().map(|p| p.name.as_str()).collect();
        for (k, port) in c.ports.iter().enumerate() {
            if !is_port_name(&port.name) {
                self.add(format!("{at}/ports/{k}/name"), "invalid port name");
            }
            self.positive(format!("{at}/ports/{k}/port"), port.port.into());
        }
        for (field, probe) in [
            ("startup", &c.startup),
            ("readiness", &c.readiness),
            ("liveness", &c.liveness),
        ] {
            if let Some(probe) = probe {
                self.probe(&format!("{at}/{field}"), probe, &own_ports);
            }
        }
        if let Some(r) = &c.resources {
            for (field, value) in [
                ("cpu_request_millis", r.cpu_request_millis),
                ("cpu_limit_millis", r.cpu_limit_millis),
                ("memory_request_bytes", r.memory_request_bytes),
                ("memory_limit_bytes", r.memory_limit_bytes),
            ] {
                if let Some(value) = value {
                    self.positive(format!("{at}/resources/{field}"), value);
                }
            }
        }
        let security = c.security.as_ref();
        if let Some(s) = security {
            self.id(format!("{at}/security/user"), s.user);
            self.id(format!("{at}/security/group"), s.group);
            if s.allow_privilege_escalation == Some(true) {
                self.add(
                    format!("{at}/security/allow_privilege_escalation"),
                    "privilege escalation cannot be allowed",
                );
            }
            if s.drop_capabilities.as_ref().is_some_and(|d| d != &["all"]) {
                self.add(
                    format!("{at}/security/drop_capabilities"),
                    "only [\"all\"] is supported",
                );
            }
        }
        let non_root = security
            .and_then(|s| s.run_as_non_root)
            .or(pod.run_as_non_root);
        let user = security.and_then(|s| s.user).or(pod.user);
        if non_root == Some(true) && user == Some(0) {
            self.add(at.to_string(), "run_as_non_root with user 0");
        }
        for (k, mount) in c.mounts.iter().enumerate() {
            let here = format!("{at}/mounts/{k}");
            if !volumes.contains(mount.volume.as_str()) {
                self.add(
                    format!("{here}/volume"),
                    format!("no pod volume {:?}", mount.volume),
                );
            }
            if !mount.path.starts_with('/') {
                self.add(format!("{here}/path"), "mount paths are absolute");
            }
            if mount
                .sub_path
                .as_deref()
                .is_some_and(|p| !is_relative_path(p))
            {
                self.add(format!("{here}/sub_path"), "a relative path without '..'");
            }
        }
    }

    fn probe(&mut self, at: &str, probe: &Probe, ports: &BTreeSet<&str>) {
        if !exactly_one(&[probe.exec.is_some(), probe.http.is_some()]) {
            self.add(at.to_string(), "a probe is exactly one of exec or http");
        }
        if probe.exec.as_ref().is_some_and(Vec::is_empty) {
            self.add(format!("{at}/exec"), "a command is required");
        }
        if let Some(http) = &probe.http {
            if !http.path.starts_with('/') {
                self.add(format!("{at}/http/path"), "paths are absolute");
            }
            match &http.port {
                PortRef::Number(n) => self.positive(format!("{at}/http/port"), (*n).into()),
                PortRef::Name(name) => {
                    if !ports.contains(name.as_str()) {
                        self.add(
                            format!("{at}/http/port"),
                            format!("no port {name:?} on this container"),
                        );
                    }
                }
            }
        }
        for (field, value) in [
            ("period_seconds", probe.period_seconds),
            ("timeout_seconds", probe.timeout_seconds),
            ("failure_threshold", probe.failure_threshold),
        ] {
            if let Some(value) = value {
                self.positive(format!("{at}/{field}"), value.into());
            }
        }
    }
}
