use serde::Deserialize;

#[derive(Deserialize)]
pub struct GenericItem {
    pub kind: String,
    pub name: String,
    pub fields: Option<serde_json::Value>,
    pub options: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct PodFields {
    pub image: String,
    pub replicas: usize,
    pub ports: Vec<u16>,
    pub secure: Option<bool>,
}

pub struct PodSpec {
    pub name: String,
    pub image: String,
    pub replicas: usize,
    pub ports: Vec<u16>,
}
