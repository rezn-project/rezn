//! Typed IR v2 program (docs/IR-v2.md). Every struct rejects unknown fields; integers are unsigned, so floats,
//! negatives and quantity strings fail to decode. Values are produced by `super::validate`, which also enforces
//! the domain and cross-reference rules the types cannot express.
use std::collections::BTreeMap;

use serde::Deserialize;

/// The exact `ir` value of an IR v2 program.
pub const IR: &str = "rezn.dev/v2";

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub ir: String,
    pub deployment: String,
    pub namespace: String,
    pub resources: Vec<Resource>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resource {
    pub name: String,
    pub phase: Option<Phase>,
    pub spec: Spec,
}

impl Resource {
    pub fn kind(&self) -> Kind {
        match self.spec {
            Spec::ReplicaSet(_) => Kind::ReplicaSet,
            Spec::OrderedReplicaSet(_) => Kind::OrderedReplicaSet,
            Spec::Job(_) => Kind::Job,
            Spec::ScheduledJob(_) => Kind::ScheduledJob,
            Spec::Service(_) => Kind::Service,
            Spec::VolumeClaim(_) => Kind::VolumeClaim,
            Spec::Config(_) => Kind::Config,
            Spec::Secret(_) => Kind::Secret,
            Spec::Ingress(_) => Kind::Ingress,
            Spec::NetworkPolicy(_) => Kind::NetworkPolicy,
        }
    }

    /// The pod template of a workload kind.
    pub fn pod(&self) -> Option<&Pod> {
        match &self.spec {
            Spec::ReplicaSet(s) => Some(&s.pod),
            Spec::OrderedReplicaSet(s) => Some(&s.pod),
            Spec::Job(s) => Some(&s.pod),
            Spec::ScheduledJob(s) => Some(&s.job.pod),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    ReplicaSet,
    OrderedReplicaSet,
    Job,
    ScheduledJob,
    Service,
    VolumeClaim,
    Config,
    Secret,
    Ingress,
    NetworkPolicy,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::ReplicaSet => "replica_set",
            Kind::OrderedReplicaSet => "ordered_replica_set",
            Kind::Job => "job",
            Kind::ScheduledJob => "scheduled_job",
            Kind::Service => "service",
            Kind::VolumeClaim => "volume_claim",
            Kind::Config => "config",
            Kind::Secret => "secret",
            Kind::Ingress => "ingress",
            Kind::NetworkPolicy => "network_policy",
        }
    }

    pub fn is_workload(self) -> bool {
        matches!(
            self,
            Kind::ReplicaSet | Kind::OrderedReplicaSet | Kind::Job | Kind::ScheduledJob
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Spec {
    ReplicaSet(ReplicaSet),
    OrderedReplicaSet(OrderedReplicaSet),
    Job(JobSpec),
    ScheduledJob(ScheduledJob),
    Service(Service),
    VolumeClaim(VolumeClaim),
    Config(Config),
    Secret(Secret),
    Ingress(Ingress),
    NetworkPolicy(NetworkPolicy),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase {
    pub when: When,
    pub on: Vec<Stage>,
    pub weight: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum When {
    Before,
    After,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Install,
    Upgrade,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaSet {
    pub replicas: u32,
    pub pod: Pod,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderedReplicaSet {
    pub replicas: u32,
    pub service: String,
    pub pod_management: Option<PodManagement>,
    #[serde(default)]
    pub volume_claims: Vec<ClaimTemplate>,
    pub retain: Option<Retain>,
    pub pod: Pod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PodManagement {
    Parallel,
    Ordered,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimTemplate {
    pub name: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retain {
    pub on_scale_down: Option<bool>,
    pub on_removal: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    pub backoff_limit: Option<u32>,
    pub pod: Pod,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledJob {
    pub schedule: String,
    pub concurrency: Option<Concurrency>,
    pub keep_succeeded: Option<u32>,
    pub keep_failed: Option<u32>,
    pub job: JobSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Concurrency {
    Forbid,
    Replace,
    Allow,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub target: String,
    pub headless: Option<bool>,
    pub ports: Vec<ServicePort>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServicePort {
    pub name: String,
    pub port: u16,
    pub target_port: PortRef,
}

/// A port number or the name of a container port.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum PortRef {
    Number(u16),
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeClaim {
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub data: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    #[serde(rename = "type")]
    pub kind: SecretType,
    pub keys: Vec<String>,
    pub source: SecretSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretType {
    Opaque,
    Tls,
}

/// Exactly one member is set (checked by the validator).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSource {
    pub store: Option<Empty>,
    pub seed_once: Option<Empty>,
    pub random: Option<BTreeMap<String, Random>>,
    pub ca: Option<Ca>,
    pub certificate: Option<Certificate>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Random {
    pub length: u32,
    pub alphabet: Alphabet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Alphabet {
    Alphanumeric,
    Hex,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ca {
    pub common_name: String,
    pub days: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Certificate {
    pub ca: String,
    pub dns_names: Vec<String>,
    pub days: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ingress {
    pub hosts: Vec<IngressHost>,
    #[serde(default)]
    pub tls: Vec<IngressTls>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressHost {
    pub host: String,
    pub paths: Vec<IngressPath>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressPath {
    pub path: String,
    pub service: String,
    pub port: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressTls {
    pub hosts: Vec<String>,
    pub secret: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkPolicy {
    pub targets: Vec<String>,
    /// Present: inbound traffic is denied unless allowed; `[]` denies all.
    pub ingress: Option<Vec<IngressRule>>,
    /// Present: outbound traffic is denied unless allowed.
    pub egress: Option<Vec<EgressRule>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressRule {
    pub from: Option<Vec<Peer>>,
    pub ports: Option<Vec<PolicyPort>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressRule {
    pub to: Option<Vec<Peer>>,
    pub ports: Option<Vec<PolicyPort>>,
}

/// Exactly one member is set.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub workload: Option<String>,
    pub cidr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyPort {
    pub port: u16,
    pub protocol: Protocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pod {
    pub containers: Vec<Container>,
    #[serde(default)]
    pub init: Vec<Container>,
    #[serde(default)]
    pub sidecars: Vec<Container>,
    pub restart: Option<Restart>,
    pub grace_seconds: Option<u32>,
    pub user: Option<u32>,
    pub group: Option<u32>,
    pub run_as_non_root: Option<bool>,
    pub fs_group: Option<u32>,
    pub fs_group_policy: Option<FsGroupPolicy>,
    pub seccomp: Option<Seccomp>,
    #[serde(default)]
    pub image_pull_secrets: Vec<String>,
    #[serde(default)]
    pub volumes: Vec<Volume>,
}

impl Pod {
    pub fn restart(&self) -> Restart {
        self.restart.unwrap_or(Restart::Always)
    }

    /// Init containers, sidecars and containers, with the field each list comes from.
    pub fn all_containers(&self) -> impl Iterator<Item = (&'static str, usize, &Container)> {
        let tag = |field: &'static str| move |(i, c)| (field, i, c);
        self.init
            .iter()
            .enumerate()
            .map(tag("init"))
            .chain(self.sidecars.iter().enumerate().map(tag("sidecars")))
            .chain(self.containers.iter().enumerate().map(tag("containers")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Restart {
    Always,
    OnFailure,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsGroupPolicy {
    OnRootMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Seccomp {
    RuntimeDefault,
}

/// Exactly one source is set.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    pub name: String,
    pub scratch: Option<Scratch>,
    pub claim: Option<String>,
    pub config: Option<Projection>,
    pub secret: Option<Projection>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scratch {
    pub medium: Medium,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Medium {
    Disk,
    Memory,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Projection {
    pub name: String,
    pub items: Option<Vec<Item>>,
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub key: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Container {
    pub name: String,
    pub image: String,
    pub pull: Option<Pull>,
    pub command: Option<Vec<String>>,
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: Vec<Env>,
    #[serde(default)]
    pub ports: Vec<ContainerPort>,
    pub startup: Option<Probe>,
    pub readiness: Option<Probe>,
    pub liveness: Option<Probe>,
    pub resources: Option<Resources>,
    pub security: Option<Security>,
    #[serde(default)]
    pub mounts: Vec<Mount>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pull {
    IfNotPresent,
    Always,
    Never,
}

/// Exactly one of `value`, `secret` or `pod_ip` is set.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Env {
    pub name: String,
    pub value: Option<String>,
    pub secret: Option<SecretKeyRef>,
    pub pod_ip: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretKeyRef {
    pub name: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerPort {
    pub name: String,
    pub port: u16,
}

/// Exactly one of `exec` or `http` is set.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub exec: Option<Vec<String>>,
    pub http: Option<HttpProbe>,
    pub period_seconds: Option<u32>,
    pub timeout_seconds: Option<u32>,
    pub failure_threshold: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpProbe {
    pub path: String,
    pub port: PortRef,
    pub scheme: Scheme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scheme {
    Http,
    Https,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub cpu_request_millis: Option<u64>,
    pub cpu_limit_millis: Option<u64>,
    pub memory_request_bytes: Option<u64>,
    pub memory_limit_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Security {
    pub user: Option<u32>,
    pub group: Option<u32>,
    pub run_as_non_root: Option<bool>,
    pub read_only_root: Option<bool>,
    pub allow_privilege_escalation: Option<bool>,
    pub drop_capabilities: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub volume: String,
    pub path: String,
    pub read_only: Option<bool>,
    pub sub_path: Option<String>,
}
