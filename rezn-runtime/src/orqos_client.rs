use anyhow::{Context, Result};
use reqwest::Client;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Clone)]
pub struct OrqosClient {
    base_url: String,
    client: Client,
}

#[derive(Serialize, Debug)]
struct CreateReq<'a> {
    name: &'a str,
    image: &'a str,
    ports: Vec<PortMap>,
    labels: HashMap<String, String>,
}

#[derive(Serialize, Debug)]
struct PortMap {
    container: u16,
    host: u16,
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

    pub async fn list_pod_containers(&self, pod_label: &str) -> Result<Vec<String>> {
        let res = self
            .client
            .get(format!("{}/containers", self.base_url))
            .query(&[("label", pod_label)])
            .send()
            .await
            .context("Failed to send list request")?
            .json::<Vec<String>>()
            .await
            .context("Failed to parse list response")?;

        Ok(res)
    }

    pub async fn start_container(
        &self,
        name: &str,
        image: &str,
        ports: &[u16],
        labels: HashMap<String, String>,
    ) -> Result<()> {
        let port_maps: Vec<PortMap> = ports
            .iter()
            .map(|p| PortMap {
                container: *p,
                host: 0, // or whatever logic you want here
            })
            .collect();

        let req = CreateReq {
            name,
            image,
            ports: port_maps,
            labels,
        };

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
            .delete(format!("{}/containers/{}", self.base_url, name))
            .send()
            .await
            .context("Failed to send delete request")?
            .error_for_status()
            .context("Failed to stop container")?;
        Ok(())
    }
}
