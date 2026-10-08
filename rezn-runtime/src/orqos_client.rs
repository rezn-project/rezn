use anyhow::{bail, Context, Result};
use reqwest::{Client, RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;

#[derive(Clone)]
pub struct OrqosClient {
    base_url: String,
    client: Client,
}

#[derive(Serialize, Debug)]
pub struct CreateReq {
    pub name: String,
    pub image: String,
    pub cpu: Option<String>,
    pub ports: Vec<PortMap>,
    pub labels: HashMap<String, String>,
}

#[derive(Serialize, Debug)]
pub struct PortMap {
    pub container: u16,
    pub host: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerSummary {
    #[serde(rename = "Id")]
    pub id: String,
    pub labels: HashMap<String, String>,
    pub image: String,
    pub state: String,
    pub ports: Vec<ObservedPort>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ObservedPort {
    #[serde(rename = "IP")]
    pub ip: Option<String>,
    pub private_port: u16,
    pub public_port: Option<u16>,
    #[serde(rename = "Type")]
    pub protocol: String,
}

impl OrqosClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect("Failed to build HTTP client"),
        }
    }

    pub async fn list_owned_containers(&self, owner: &str) -> Result<Vec<ContainerSummary>> {
        let res = self
            .send_request(
                "list containers",
                self.client
                    .get(format!("{}/docker/containers", self.base_url))
                    .query(&[
                        ("label", format!("dev.rezn.owner={owner}")),
                        ("all", "true".into()),
                    ]),
            )
            .await?
            .json::<Vec<ContainerSummary>>()
            .await
            .context("Failed to parse list response")?;

        Ok(res)
    }

    pub async fn start_container(&self, req: CreateReq) -> Result<()> {
        tracing::debug!(
            "Creating container request:\n{}",
            serde_json::to_string_pretty(&req).unwrap()
        );

        self.send_request(
            "create container",
            self.client
                .post(format!("{}/docker/containers", self.base_url))
                .json(&req),
        )
        .await?;

        Ok(())
    }

    #[cfg(test)]
    pub async fn stop_container(&self, id: &str) -> Result<()> {
        self.send_request(
            "stop container",
            self.client
                .post(format!("{}/docker/containers/{}/stop", self.base_url, id)),
        )
        .await?;
        Ok(())
    }

    pub async fn remove_container(&self, id: &str) -> Result<()> {
        self.send_request(
            "remove container",
            self.client
                .post(format!("{}/docker/containers/{}/remove", self.base_url, id))
                .json(&serde_json::json!({ "force": true })),
        )
        .await?;
        Ok(())
    }

    async fn send_request(&self, operation: &str, request: RequestBuilder) -> Result<Response> {
        let response = request
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
    use crate::test_support::MockOrqos;

    fn create_request() -> CreateReq {
        CreateReq {
            name: "prod-web-1".into(),
            image: "nginx:alpine".into(),
            cpu: None,
            ports: vec![PortMap {
                container: 80,
                host: 0,
            }],
            labels: HashMap::from([("pod".into(), "prod:web".into())]),
        }
    }

    #[tokio::test]
    async fn lifecycle_requests_follow_current_docker_contract() {
        let server =
            MockOrqos::start(
                |request| match (request.method.as_str(), request.uri.path()) {
                    ("GET", "/docker/containers") => (
                        StatusCode::OK,
                        json!([{"Id": "container-1", "Labels": {}, "Image": "nginx:alpine", "State": "running", "Ports": [{"IP":"0.0.0.0", "PrivatePort":80, "PublicPort":32768, "Type":"tcp"}]}]).to_string(),
                    ),
                    ("POST", "/docker/containers") => (
                        StatusCode::OK,
                        json!({"id": "container-1", "name": "prod-web-1", "ports": {}}).to_string(),
                    ),
                    ("POST", "/docker/containers/container-1/stop")
                    | ("POST", "/docker/containers/container-1/remove") => {
                        (StatusCode::NO_CONTENT, String::new())
                    }
                    _ => (StatusCode::NOT_FOUND, "Unknown route".into()),
                },
            )
            .await;

        let containers = server
            .client
            .list_owned_containers("runtime-1")
            .await
            .unwrap();
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].id, "container-1");
        assert_eq!(containers[0].ports[0].public_port, Some(32768));
        server
            .client
            .start_container(create_request())
            .await
            .unwrap();
        server.client.stop_container("container-1").await.unwrap();
        server.client.remove_container("container-1").await.unwrap();

        let requests = server.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].method, Method::GET);
        assert_eq!(
            requests[0].query(),
            HashMap::from([
                ("label".into(), "dev.rezn.owner=runtime-1".into()),
                ("all".into(), "true".into())
            ])
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[1].body).unwrap(),
            json!({
                "name": "prod-web-1", "image": "nginx:alpine", "cpu": null,
                "ports": [{"container": 80, "host": 0}], "labels": {"pod": "prod:web"}
            })
        );
        assert!(requests[2].body.is_empty());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[3].body).unwrap(),
            json!({"force": true})
        );
    }

    #[tokio::test]
    async fn lifecycle_errors_preserve_operation_status_and_response_body() {
        for (status, body) in [
            (
                StatusCode::NOT_FOUND,
                "Image missing; pull into the selected Docker engine.",
            ),
            (StatusCode::CONFLICT, "Container name already exists."),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                r#"{"enabled":true,"available":false,"error":"Docker socket unavailable"}"#,
            ),
        ] {
            let server = MockOrqos::start(move |_| (status, body.into())).await;
            let results = [
                (
                    "list containers",
                    server
                        .client
                        .list_owned_containers("runtime-1")
                        .await
                        .map(|_| ()),
                ),
                (
                    "create container",
                    server.client.start_container(create_request()).await,
                ),
                (
                    "stop container",
                    server.client.stop_container("container-1").await,
                ),
                (
                    "remove container",
                    server.client.remove_container("container-1").await,
                ),
            ];
            for (operation, result) in results {
                let error = format!("{:#}", result.unwrap_err());
                assert!(error.contains(operation), "{error}");
                assert!(error.contains(&format!("HTTP {status}")), "{error}");
                assert!(error.contains("/docker/containers"), "{error}");
                assert!(error.contains(body), "{error}");
                assert!(!error.contains("Failed to parse list response"), "{error}");
            }
        }
    }

    #[tokio::test]
    async fn malformed_successful_list_is_an_error() {
        let server = MockOrqos::start(|_| (StatusCode::OK, "not JSON".into())).await;
        let result = server.client.list_owned_containers("runtime-1").await;
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("Failed to parse list response"));
    }
}
