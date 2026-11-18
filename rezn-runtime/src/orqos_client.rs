//! Client for Orqos's workload API (`ORQOS_BACKENDS=workload`): pods of containers run as owned systemd/Podman
//! units. See Orqos `docs/WORKLOAD_API.md`.
use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use reqwest::{Client, RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Orqos's pod spec. Rezn always sends every field explicitly, so an observed spec compares equal to the desired
/// one exactly when Orqos runs what Rezn asked for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PodSpec {
    pub hostname: String,
    pub containers: Vec<ContainerSpec>,
    /// Run once, in order, before sidecars and containers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub init: Vec<ContainerSpec>,
    /// Started before containers, always restarted; a startup probe gates the containers' start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sidecars: Vec<ContainerSpec>,
    /// Orqos secrets with `.dockerconfigjson` credentials for the pod's pulls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_pull_secrets: Vec<String>,
    pub restart: Restart,
    pub grace_seconds: u32,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Network>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supplementary_groups: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<PodVolume>,
}

/// A pod volume: an owned volume (`volume`, created with `PUT /workload/volumes/{name}`), a scratch volume, or
/// files from an owned secret (`secret`, created with `PUT /workload/secrets/{name}`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PodVolume {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch: Option<Scratch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<SecretFiles>,
}

/// Keys of an owned secret written as read-only files, owned by root and `group`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SecretFiles {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<SecretItem>>,
    pub mode: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SecretItem {
    pub key: String,
    pub path: String,
}

/// One key of an owned secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SecretKeyRef {
    pub secret: String,
    pub key: String,
}

/// An owned secret's spec: base64 values (immutable in Orqos) and Rezn's labels.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct SecretSpec {
    pub data: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
}

impl std::fmt::Debug for SecretSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretSpec")
            .field("keys", &self.data.keys().collect::<Vec<_>>())
            .field("labels", &self.labels)
            .finish()
    }
}

/// One owned secret as Orqos reports it (its keys too, which Rezn does not need; never values).
#[derive(Clone, Debug, Deserialize)]
pub struct SecretStatus {
    pub name: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Pods whose records use it.
    pub pods: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Scratch {
    pub medium: Medium,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Medium {
    Disk,
    Memory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MountSpec {
    pub volume: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_path: Option<String>,
}

/// An owned volume's spec: the group owning its root (setgid 2775) and Rezn's labels, kept by Orqos.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct VolumeSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<u32>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// One owned volume as Orqos reports it.
#[derive(Clone, Debug, Deserialize)]
pub struct VolumeStatus {
    pub name: String,
    pub exists: bool,
    pub spec: VolumeSpec,
    /// Pods whose records mount it.
    pub pods: Vec<String>,
}

/// An Orqos-owned Podman network the pod joins, with the DNS names that resolve to it there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Network {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Restart {
    Always,
    OnFailure,
    Never,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_env: BTreeMap<String, SecretKeyRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only_root: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_new_privileges: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub drop_all_capabilities: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MountSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup: Option<Probe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<Probe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liveness: Option<Probe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull: Option<Pull>,
}

/// Orqos's pull policy: IR v2's `if_not_present` is Podman's `missing`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Pull {
    Missing,
    Always,
    Never,
}

/// Cgroup limits and requests of one container.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_limit_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_request_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_request_bytes: Option<u64>,
}

/// The host's CPUs (× 1000) and memory, as Orqos reports them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Capacity {
    pub cpu_millis: u64,
    pub memory_bytes: u64,
}

/// A probe Orqos runs: `exec` in the container, or HTTP(S) from inside the pod's network namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Probe {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpProbe>,
    pub period_seconds: u32,
    pub timeout_seconds: u32,
    pub failure_threshold: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HttpProbe {
    pub path: String,
    pub port: u16,
    pub scheme: Scheme,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Scheme {
    Http,
    Https,
}

/// One pod as Orqos reports it.
#[derive(Clone, Debug, Deserialize)]
pub struct PodStatus {
    pub id: String,
    /// `running`, `starting`, `restarting`, `degraded`, `failed` or `stopped`.
    pub state: String,
    /// It runs and every container is ready (a container without probes is ready once it runs).
    pub ready: bool,
    pub containers: Vec<ContainerStatus>,
    pub spec: PodSpec,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ContainerStatus {
    pub name: String,
    /// `init`, `sidecar` or `container`.
    pub kind: String,
    pub active_state: String,
    pub sub_state: String,
    pub result: String,
    /// Automatic restarts by systemd since the container was last started explicitly.
    pub restarts: u32,
    /// The startup probe has passed (or there is none and it runs).
    pub started: bool,
    /// The readiness probe passes (or there is none and it has started).
    pub ready: bool,
    /// Restarts caused by failing startup or liveness probes.
    pub probe_restarts: u32,
    pub last_probe_failure: Option<String>,
    /// The image the container runs (Podman's image ID).
    #[serde(default)]
    pub image_id: Option<String>,
}

#[derive(Clone)]
pub struct OrqosClient {
    base_url: String,
    token: String,
    client: Client,
    /// Applying a pod can pull images and waits for its units to start; removing waits for the grace period.
    slow: Client,
}

impl OrqosClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let base_url = base_url.into();
        ensure!(
            base_url.starts_with("http://") || base_url.starts_with("https://"),
            "ORQOS_API_URL must start with http:// or https://"
        );
        let token = token.into();
        ensure!(
            token.len() >= 32 && !token.chars().any(char::is_control),
            "the Orqos workload token must be at least 32 bytes without control characters"
        );
        let build = |seconds| {
            Client::builder()
                .timeout(Duration::from_secs(seconds))
                .build()
                .context("building the HTTP client")
        };
        Ok(Self {
            base_url: base_url.trim_end_matches('/').into(),
            token,
            client: build(20)?,
            slow: build(15 * 60)?,
        })
    }

    pub async fn list_pods(&self) -> Result<Vec<PodStatus>> {
        self.send(
            "list workload pods",
            self.client.get(format!("{}/workload/pods", self.base_url)),
        )
        .await?
        .json()
        .await
        .context("Failed to parse the workload pod list")
    }

    pub async fn put_pod(&self, id: &str, spec: &PodSpec) -> Result<()> {
        self.send(
            &format!("apply workload pod {id}"),
            self.slow
                .put(format!("{}/workload/pods/{id}", self.base_url))
                .json(spec),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_pod(&self, id: &str) -> Result<()> {
        self.send(
            &format!("remove workload pod {id}"),
            self.slow
                .delete(format!("{}/workload/pods/{id}", self.base_url)),
        )
        .await?;
        Ok(())
    }

    pub async fn list_volumes(&self) -> Result<Vec<VolumeStatus>> {
        self.send(
            "list workload volumes",
            self.client
                .get(format!("{}/workload/volumes", self.base_url)),
        )
        .await?
        .json()
        .await
        .context("Failed to parse the workload volume list")
    }

    pub async fn put_volume(&self, name: &str, spec: &VolumeSpec) -> Result<()> {
        self.send(
            &format!("apply workload volume {name}"),
            self.client
                .put(format!("{}/workload/volumes/{name}", self.base_url))
                .json(spec),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_volume(&self, name: &str) -> Result<()> {
        self.send(
            &format!("remove workload volume {name}"),
            self.client
                .delete(format!("{}/workload/volumes/{name}", self.base_url)),
        )
        .await?;
        Ok(())
    }

    pub async fn capacity(&self) -> Result<Capacity> {
        self.send(
            "read the host's capacity",
            self.client
                .get(format!("{}/workload/capacity", self.base_url)),
        )
        .await?
        .json()
        .await
        .context("Failed to parse the host's capacity")
    }

    pub async fn list_secrets(&self) -> Result<Vec<SecretStatus>> {
        self.send(
            "list workload secrets",
            self.client
                .get(format!("{}/workload/secrets", self.base_url)),
        )
        .await?
        .json()
        .await
        .context("Failed to parse the workload secret list")
    }

    pub async fn put_secret(&self, name: &str, spec: &SecretSpec) -> Result<()> {
        self.send(
            &format!("apply workload secret {name}"),
            self.client
                .put(format!("{}/workload/secrets/{name}", self.base_url))
                .json(spec),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_secret(&self, name: &str) -> Result<()> {
        self.send(
            &format!("remove workload secret {name}"),
            self.client
                .delete(format!("{}/workload/secrets/{name}", self.base_url)),
        )
        .await?;
        Ok(())
    }

    async fn send(&self, operation: &str, request: RequestBuilder) -> Result<Response> {
        let response = request
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("Failed to {operation}"))?;
        let status = response.status();
        if !status.is_success() {
            let url = response.url().to_string();
            let body = response.text().await.with_context(|| {
                format!("Failed to {operation}: HTTP {status} from {url}; unreadable error body")
            })?;
            bail!("Failed to {operation}: HTTP {status} from {url}: {body}");
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::json;

    use super::*;
    use crate::test_support::{MockOrqos, TOKEN};

    fn spec() -> PodSpec {
        PodSpec {
            hostname: "web-0".into(),
            init: vec![],
            sidecars: vec![],
            image_pull_secrets: vec!["rz-registry".into()],
            containers: vec![ContainerSpec {
                name: "web".into(),
                image: "docker.io/library/nginx:1.27-alpine".into(),
                command: None,
                args: Some(vec!["-g".into(), "daemon off;".into()]),
                env: BTreeMap::from([("LEVEL".into(), "info".into())]),
                secret_env: BTreeMap::from([(
                    "DB_PASSWORD".into(),
                    SecretKeyRef {
                        secret: "rz-db".into(),
                        key: "password".into(),
                    },
                )]),
                user: Some(1000),
                group: None,
                read_only_root: true,
                no_new_privileges: false,
                drop_all_capabilities: true,
                mounts: vec![MountSpec {
                    volume: "data".into(),
                    path: "/data".into(),
                    read_only: false,
                    sub_path: Some("v1".into()),
                }],
                startup: None,
                readiness: Some(Probe {
                    exec: None,
                    http: Some(HttpProbe {
                        path: "/ready".into(),
                        port: 80,
                        scheme: Scheme::Https,
                    }),
                    period_seconds: 10,
                    timeout_seconds: 1,
                    failure_threshold: 3,
                }),
                liveness: None,
                resources: Some(Resources {
                    cpu_limit_millis: Some(1000),
                    cpu_request_millis: Some(100),
                    memory_limit_bytes: Some(1 << 30),
                    memory_request_bytes: Some(384 << 20),
                }),
                pull: Some(Pull::Always),
            }],
            restart: Restart::Always,
            grace_seconds: 30,
            labels: BTreeMap::from([("dev.rezn.owner".into(), "ab".into())]),
            network: Some(Network {
                name: "rz-net".into(),
                aliases: vec!["web".into(), "web.store.svc".into()],
            }),
            supplementary_groups: vec![2000],
            volumes: vec![
                PodVolume {
                    name: "data".into(),
                    volume: Some("rz-vol".into()),
                    scratch: None,
                    secret: None,
                },
                PodVolume {
                    name: "tls".into(),
                    volume: None,
                    scratch: None,
                    secret: Some(SecretFiles {
                        name: "rz-tls".into(),
                        items: Some(vec![SecretItem {
                            key: "tls.crt".into(),
                            path: "certs/tls.crt".into(),
                        }]),
                        mode: 0o440,
                        group: Some(2000),
                    }),
                },
            ],
        }
    }

    #[tokio::test]
    async fn requests_follow_the_workload_contract_with_bearer_auth() {
        let observed = json!([{
            "id": "rz-1", "desired": "running", "state": "running", "ready": true,
            "pod": {"unit": "orqos-rz-1-pod.service", "active_state": "active", "sub_state": "running", "result": "success", "restarts": 0},
            "containers": [{"name": "web", "kind": "container", "unit": "orqos-rz-1-c-web.service", "active_state": "active", "sub_state": "running", "result": "success", "restarts": 2,
                            "started": true, "ready": true, "probe_restarts": 1, "last_probe_failure": "liveness: HTTP 500"}],
            "spec": {"hostname": "web-0", "restart": "always", "grace_seconds": 30, "labels": {"dev.rezn.owner": "ab"},
                     "network": {"name": "rz-net", "aliases": ["web", "web.store.svc"]},
                     "supplementary_groups": [2000], "volumes": [{"name": "data", "volume": "rz-vol"},
                       {"name": "tls", "secret": {"name": "rz-tls", "items": [{"key": "tls.crt", "path": "certs/tls.crt"}], "mode": 288, "group": 2000}}],
                     "containers": [{"name": "web", "image": "docker.io/library/nginx:1.27-alpine", "args": ["-g", "daemon off;"], "env": {"LEVEL": "info"},
                                     "secret_env": {"DB_PASSWORD": {"secret": "rz-db", "key": "password"}},
                                     "user": 1000, "read_only_root": true, "drop_all_capabilities": true,
                                     "mounts": [{"volume": "data", "path": "/data", "sub_path": "v1"}],
                                     "readiness": {"http": {"path": "/ready", "port": 80, "scheme": "https"},
                                                   "period_seconds": 10, "timeout_seconds": 1, "failure_threshold": 3},
                                     "resources": {"cpu_limit_millis": 1000, "cpu_request_millis": 100,
                                                   "memory_limit_bytes": 1073741824, "memory_request_bytes": 402653184},
                                     "pull": "always"}], "image_pull_secrets": ["rz-registry"]},
            "error": null, "updated_at": "2026-11-18T00:00:00Z"
        }]);
        let server = MockOrqos::start(move |request| match request.method.as_str() {
            "GET" => (StatusCode::OK, observed.to_string()),
            "PUT" | "DELETE" => (StatusCode::OK, "{}".into()),
            _ => (StatusCode::NOT_FOUND, "unknown".into()),
        })
        .await;
        let pods = server.client.list_pods().await.unwrap();
        assert_eq!(
            pods[0].spec,
            spec(),
            "an observed spec equals the one Rezn sends"
        );
        assert_eq!(pods[0].containers[0].restarts, 2);
        server.client.put_pod("rz-1", &spec()).await.unwrap();
        server.client.delete_pod("rz-1").await.unwrap();

        let requests = server.requests();
        let routes: Vec<_> = requests
            .iter()
            .map(|r| (r.method.clone(), r.uri.path().to_string()))
            .collect();
        assert_eq!(
            routes,
            vec![
                (Method::GET, "/workload/pods".into()),
                (Method::PUT, "/workload/pods/rz-1".into()),
                (Method::DELETE, "/workload/pods/rz-1".into()),
            ]
        );
        assert!(requests
            .iter()
            .all(|r| r.authorization.as_deref() == Some(&format!("Bearer {TOKEN}")[..])));
        let sent: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(sent["restart"], "always");
        assert_eq!(sent["grace_seconds"], 30);
        assert!(sent["containers"][0].get("command").is_none());
    }

    #[tokio::test]
    async fn errors_keep_operation_status_url_and_body() {
        let server =
            MockOrqos::start(|_| (StatusCode::CONFLICT, r#"{"error":"not owned"}"#.into())).await;
        for (operation, result) in [
            (
                "list workload pods",
                server.client.list_pods().await.map(|_| ()),
            ),
            (
                "apply workload pod rz-1",
                server.client.put_pod("rz-1", &spec()).await,
            ),
            (
                "remove workload pod rz-1",
                server.client.delete_pod("rz-1").await,
            ),
        ] {
            let error = format!("{:#}", result.unwrap_err());
            assert!(error.contains(operation), "{error}");
            assert!(error.contains("HTTP 409"), "{error}");
            assert!(error.contains("/workload/pods"), "{error}");
            assert!(error.contains("not owned"), "{error}");
        }
        let server = MockOrqos::start(|_| (StatusCode::OK, "not JSON".into())).await;
        assert!(
            format!("{:#}", server.client.list_pods().await.unwrap_err())
                .contains("Failed to parse")
        );
    }

    #[test]
    fn configuration_is_checked() {
        assert!(OrqosClient::new("localhost:9090", TOKEN).is_err());
        assert!(OrqosClient::new("http://127.0.0.1:9090", "short").is_err());
        assert!(OrqosClient::new("http://127.0.0.1:9090/", TOKEN).is_ok());
    }
}
