use std::{collections::HashMap, sync::Arc, sync::Mutex};

use axum::{body::to_bytes, extract::Request, http::Method, http::StatusCode, http::Uri, Router};
use tokio::{net::TcpListener, task::JoinHandle};

use crate::orqos_client::OrqosClient;

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: Method,
    pub uri: Uri,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn query(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(self.uri.query().unwrap_or_default().as_bytes())
            .into_owned()
            .collect()
    }
}

pub struct MockOrqos {
    pub client: OrqosClient,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    task: JoinHandle<()>,
}

impl MockOrqos {
    pub async fn start(
        respond: impl Fn(&RecordedRequest) -> (StatusCode, String) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let respond = Arc::new(respond);
        let router = Router::new().fallback(move |request: Request| {
            let captured = captured.clone();
            let respond = respond.clone();
            async move {
                let (parts, body) = request.into_parts();
                let request = RecordedRequest {
                    method: parts.method,
                    uri: parts.uri,
                    body: to_bytes(body, 64 * 1024).await.unwrap().to_vec(),
                };
                let response = respond(&request);
                captured.lock().unwrap().push(request);
                response
            }
        });
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            client: OrqosClient::new(format!("http://{address}")),
            requests,
            task,
        }
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockOrqos {
    fn drop(&mut self) {
        self.task.abort();
    }
}
