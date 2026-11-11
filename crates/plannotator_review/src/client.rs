//! JSON over HTTP to a local Plannotator server.

use anyhow::{Context as _, Result};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request};
use serde::{Serialize, de::DeserializeOwned};
use std::sync::Arc;

pub async fn send_json(
    http: &Arc<dyn HttpClient>,
    method: Method,
    url: &str,
    body: &impl Serialize,
) -> Result<()> {
    let request = Request::builder()
        .method(method)
        .uri(url)
        .header("Content-Type", "application/json")
        .body(AsyncBody::from(serde_json::to_string(body)?))?;
    let response = http.send(request).await.context("Plannotator did not answer")?;
    anyhow::ensure!(response.status().is_success(), "Plannotator answered {}", response.status());
    Ok(())
}

pub async fn get_json<T: DeserializeOwned>(http: &Arc<dyn HttpClient>, url: &str) -> Result<T> {
    let request = Request::builder()
        .method(Method::GET)
        .uri(url)
        .body(AsyncBody::empty())?;
    let mut response = http.send(request).await.context("Plannotator did not answer")?;
    anyhow::ensure!(response.status().is_success(), "Plannotator answered {}", response.status());
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;
    Ok(serde_json::from_str(&body)?)
}
