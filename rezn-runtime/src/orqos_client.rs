use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerSummary {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Names")]
    pub names: Vec<String>,
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

    pub async fn list_pod_containers(&self, pod_label: &str) -> Result<Vec<ContainerSummary>> {
        let res = self
            .client
            .get(format!("{}/containers", self.base_url))
            .query(&[("label", format!("pod={}", pod_label))])
            .send()
            .await
            .context("Failed to send list request")?
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

        self.client
            .post(format!("{}/containers", self.base_url))
            .json(&req)
            .send()
            .await
            .context("Failed to send create request")?
            .error_for_status()
            .context("Container creation failed")?;

        Ok(())
    }

    pub async fn stop_container(&self, name: &str) -> Result<()> {
        self.client
            .post(format!("{}/containers/{}/stop", self.base_url, name))
            .send()
            .await
            .context("Failed to send stop container request")?
            .error_for_status()
            .context("Failed to stop container")?;
        Ok(())
    }

    pub async fn remove_container(&self, name: &str) -> Result<()> {
        self.client
            .post(format!("{}/containers/{}/remove", self.base_url, name))
            .json(&serde_json::json!({
                "force": true,
            }))
            .send()
            .await
            .context("Failed to send remove container request")?
            .error_for_status()
            .context("Failed to remove container")?;
        Ok(())
    }
}
