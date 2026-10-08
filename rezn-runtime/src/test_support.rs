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
        Self::start_async(move |request| std::future::ready(respond(&request))).await
    }

    pub async fn start_async<F, Fut>(respond: F) -> Self
    where
        F: Fn(RecordedRequest) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = (StatusCode, String)> + Send + 'static,
    {
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
                captured.lock().unwrap().push(request.clone());
                respond(request).await
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

pub fn signed(program: Vec<serde_json::Value>) -> common::types::InstructionWrapper {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use ed25519_dalek::{Signer, SigningKey};
    let key = SigningKey::from_bytes(&[42; 32]);
    let bytes = serde_json_canonicalizer::to_vec(&program).unwrap();
    common::types::InstructionWrapper {
        program,
        signature: common::types::Signature {
            algorithm: "ed25519".into(),
            pubkey: STANDARD.encode(key.verifying_key().as_bytes()),
            sig: STANDARD.encode(key.sign(&bytes).to_bytes()),
        },
    }
}

pub fn pod(name: &str, image: &str, replicas: usize, ports: &[u16]) -> serde_json::Value {
    serde_json::json!({"kind":"pod", "name":name, "fields":{"image":image, "replicas":replicas, "ports":ports}})
}

pub fn app(client: OrqosClient, db: Option<sled::Db>) -> Arc<crate::AppState> {
    let db = db.unwrap_or_else(|| sled::Config::new().temporary(true).open().unwrap());
    crate::store::initialize(&db).unwrap();
    Arc::new(crate::AppState {
        db: Arc::new(db),
        orqos: Arc::new(client),
        stats: Default::default(),
        stats_tx: tokio::sync::broadcast::channel(10).0,
        secret_store: crate::secret::SecretStore::temporary(),
        controller: Default::default(),
    })
}

pub struct HttpRezn {
    pub url: String,
    task: JoinHandle<()>,
}
impl HttpRezn {
    pub async fn start(app: Arc<crate::AppState>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, crate::router::build_router(app))
                .await
                .unwrap()
        });
        Self { url, task }
    }
}
impl Drop for HttpRezn {
    fn drop(&mut self) {
        self.task.abort();
    }
}
