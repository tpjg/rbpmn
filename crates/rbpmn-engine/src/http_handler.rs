//! The default push-mode handler: POSTs the work item as JSON to a
//! **operator-configured** URL (never from request or model data — see
//! docs/http-security.md on SSRF) and applies the JSON response body as an
//! RFC 7386 merge patch.
//!
//! Contract: 2xx -> completion, response body = merge patch (empty body =
//! no patch). Any other status or transport error -> failure; an
//! `x-rbpmn-error-code` response header names the error for boundary
//! matching once retries are exhausted.

use crate::{HandlerFailure, ServiceTaskHandler, WorkItem};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub struct HttpPostHandler {
    url: String,
    client: reqwest::Client,
}

impl HttpPostHandler {
    pub fn new(url: impl Into<String>) -> Self {
        HttpPostHandler {
            url: url.into(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
        }
    }
}

impl ServiceTaskHandler for HttpPostHandler {
    fn execute(
        &self,
        item: WorkItem,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, HandlerFailure>> + Send + '_>> {
        Box::pin(async move {
            let payload = serde_json::json!({
                "workItemId": item.id,
                "instanceId": item.instance_id,
                "definitionKey": item.definition_key,
                "elementId": item.element_id,
                "topic": item.topic,
                "variables": item.variables,
            });
            let response = self
                .client
                .post(&self.url)
                .json(&payload)
                .send()
                .await
                .map_err(|e| HandlerFailure {
                    code: None,
                    message: format!("transport error: {e}"),
                })?;

            if response.status().is_success() {
                let bytes = response.bytes().await.map_err(|e| HandlerFailure {
                    code: None,
                    message: format!("reading response: {e}"),
                })?;
                if bytes.is_empty() {
                    return Ok(serde_json::json!({}));
                }
                serde_json::from_slice(&bytes).map_err(|e| HandlerFailure {
                    code: None,
                    message: format!("response is not JSON: {e}"),
                })
            } else {
                let code = response
                    .headers()
                    .get("x-rbpmn-error-code")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                Err(HandlerFailure {
                    code,
                    message: format!("handler answered {}", response.status()),
                })
            }
        })
    }
}
