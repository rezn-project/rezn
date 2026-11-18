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
    pub restart: Restart,
    pub grace_seconds: u32,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
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
}

/// One pod as Orqos reports it.
#[derive(Clone, Debug, Deserialize)]
pub struct PodStatus {
    pub id: String,
    /// `running`, `starting`, `restarting`, `degraded`, `failed` or `stopped`.
    pub state: String,
    pub containers: Vec<ContainerStatus>,
    pub spec: PodSpec,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ContainerStatus {
    pub name: String,
    pub active_state: String,
    pub sub_state: String,
    pub result: String,
    /// Automatic restarts by systemd since the container was last started explicitly.
    pub restarts: u32,
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
            containers: vec![ContainerSpec {
                name: "web".into(),
                image: "docker.io/library/nginx:1.27-alpine".into(),
                command: None,
                args: Some(vec!["-g".into(), "daemon off;".into()]),
                env: BTreeMap::from([("LEVEL".into(), "info".into())]),
            }],
            restart: Restart::Always,
            grace_seconds: 30,
            labels: BTreeMap::from([("dev.rezn.owner".into(), "ab".into())]),
        }
    }

    #[tokio::test]
    async fn requests_follow_the_workload_contract_with_bearer_auth() {
        let observed = json!([{
            "id": "rz-1", "desired": "running", "state": "running",
            "pod": {"unit": "orqos-rz-1-pod.service", "active_state": "active", "sub_state": "running", "result": "success", "restarts": 0},
            "containers": [{"name": "web", "unit": "orqos-rz-1-c-web.service", "active_state": "active", "sub_state": "running", "result": "success", "restarts": 2}],
            "spec": {"hostname": "web-0", "restart": "always", "grace_seconds": 30, "labels": {"dev.rezn.owner": "ab"},
                     "containers": [{"name": "web", "image": "docker.io/library/nginx:1.27-alpine", "args": ["-g", "daemon off;"], "env": {"LEVEL": "info"}}]},
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
