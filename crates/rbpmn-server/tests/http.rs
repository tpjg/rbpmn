use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use rbpmn_server::{Tokens, app};
use tower::ServiceExt;

const TOKEN: &str = "test-token-0123456789abcdef-0123456789abcdef";

fn test_app() -> axum::Router {
    app(Tokens::parse([TOKEN]).unwrap())
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

const INCLUSIVE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  id="defs" targetNamespace="urn:test">
  <bpmn:process id="p" isExecutable="true">
    <bpmn:startEvent id="start"/>
    <bpmn:inclusiveGateway id="ig"/>
    <bpmn:endEvent id="end"/>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="ig"/>
    <bpmn:sequenceFlow id="f2" sourceRef="ig" targetRef="end"/>
  </bpmn:process>
</bpmn:definitions>"#;

#[tokio::test]
async fn healthz_is_public() {
    let resp = test_app()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn v1_requires_bearer_token() {
    let resp = test_app()
        .oneshot(
            Request::post("/v1/definitions/lint")
                .body(Body::from(INCLUSIVE_XML))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers().get(header::WWW_AUTHENTICATE).unwrap(),
        "Bearer"
    );
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    let resp = test_app()
        .oneshot(
            Request::post("/v1/definitions/lint")
                .header(
                    header::AUTHORIZATION,
                    "Bearer definitely-not-the-token-but-long",
                )
                .body(Body::from(INCLUSIVE_XML))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn lint_reports_diagnostics() {
    let resp = test_app()
        .oneshot(
            Request::post("/v1/definitions/lint")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::from(INCLUSIVE_XML))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["ok"], false);
    let rules: Vec<&str> = json["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["rule"].as_str().unwrap())
        .collect();
    assert!(rules.contains(&"no-inclusive-gateway"), "got {rules:?}");
}

#[tokio::test]
async fn malformed_xml_is_bad_request() {
    let resp = test_app()
        .oneshot(
            Request::post("/v1/definitions/lint")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::from("this is not xml"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
