use anyhow::{Context, Result};
use reqwest::Client;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Clone)]
pub struct OrqosClient {
    base_url: String,
    client: Client,
}

impl OrqosClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: Client::new(),
        }
    }

    pub async fn list_pod_containers(&self, pod_name: &str) -> Result<Vec<String>> {
        let res = self
            .client
            .get(format!("{}/containers", self.base_url))
            .query(&[("label", format!("pod:{}", pod_name))])
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
        pod_name: &str,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct CreateReq<'a> {
            name: &'a str,
            image: &'a str,
            ports: Vec<PortMap>,
            labels: HashMap<String, String>,
        }

        #[derive(Serialize)]
        struct PortMap {
            container: u16,
            host: u16,
        }

        let labels = HashMap::from([("pod".into(), pod_name.into())]);

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
