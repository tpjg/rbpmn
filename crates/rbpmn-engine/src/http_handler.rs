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
                // The operator configured THIS url; a redirect elsewhere
                // would re-POST the instance variables to a different host.
                .redirect(reqwest::redirect::Policy::none())
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
                const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
                // Enforce the cap while streaming — buffering first would
                // let a misbehaving endpoint exhaust memory before the
                // check ever ran.
                if response
                    .content_length()
                    .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
                {
                    return Err(HandlerFailure {
                        code: None,
                        message: "response too large (limit 1 MiB)".to_string(),
                    });
                }
                let mut response = response;
                let mut bytes: Vec<u8> = Vec::new();
                loop {
                    match response.chunk().await {
                        Ok(Some(chunk)) => {
                            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                                return Err(HandlerFailure {
                                    code: None,
                                    message: "response too large (limit 1 MiB)".to_string(),
                                });
                            }
                            bytes.extend_from_slice(&chunk);
                        }
                        Ok(None) => break,
                        Err(e) => {
                            return Err(HandlerFailure {
                                code: None,
                                message: format!("reading response: {e}"),
                            });
                        }
                    }
                }
                if bytes.is_empty() {
                    return Ok(serde_json::json!({}));
                }
                let patch: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(|e| HandlerFailure {
                        code: None,
                        message: format!("response is not JSON: {e}"),
                    })?;
                // A 200 with a non-object body was never meant as an RFC 7386
                // whole-document replacement — refuse it here, at the
                // ingestion boundary, before it can wipe the variables.
                if !patch.is_object() {
                    return Err(HandlerFailure {
                        code: None,
                        message: "response body must be a JSON object (an RFC 7386 merge patch)"
                            .to_string(),
                    });
                }
                Ok(patch)
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
