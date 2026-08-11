use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use rbpmn_engine::testing::TestDb;
use rbpmn_server::{Tokens, app};
use tower::ServiceExt;

const TOKEN: &str = "test-token-0123456789abcdef-0123456789abcdef";

// The lint corpus is the single source of truth for BPMN test inputs.
const INCLUSIVE_XML: &str =
    include_str!("../../rbpmn-model/tests/fixtures/reject/inclusive-gateway.bpmn");
const MINIMAL_XML: &str = include_str!("../../rbpmn-model/tests/fixtures/accept/01-minimal.bpmn");

async fn test_app() -> (axum::Router, TestDb) {
    let db = TestDb::create().await;
    let engine = rbpmn_engine::Engine::builder(db.pool.clone()).build();
    engine.migrate().await.unwrap();
    (app(Tokens::from_list([TOKEN]).unwrap(), engine), db)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn authed(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn healthz_is_public_and_static() {
    let (app, db) = test_app().await;
    let resp = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "ok");
    assert!(json.get("version").is_none());
    db.drop().await;
}

#[tokio::test]
async fn v1_requires_bearer_token_uniformly() {
    let (app, db) = test_app().await;
    let resp = app
        .clone()
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

    // Unknown paths cannot be probed without auth either.
    let resp = app
        .oneshot(
            Request::post("/v1/definitely/not/a/route")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    db.drop().await;
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    let (app, db) = test_app().await;
    let resp = app
        .oneshot(
            Request::post("/v1/definitions/lint")
                .header(
                    header::AUTHORIZATION,
                    "Bearer definitely-not-the-token-but-long-enough",
                )
                .body(Body::from(INCLUSIVE_XML))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    db.drop().await;
}

#[tokio::test]
async fn authed_unknown_paths_are_404() {
    let (app, db) = test_app().await;
    let resp = app
        .oneshot(authed(
            "POST",
            "/v1/definitely/not/a/route",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    db.drop().await;
}

#[tokio::test]
async fn lint_reports_diagnostics() {
    let (app, db) = test_app().await;
    let resp = app
        .clone()
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
    // The stable rule id is the contract, not just "not ok".
    assert!(
        json["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["rule"] == "no-inclusive-gateway")
    );

    // Input that is not BPMN at all is a client error, not a lint result.
    let resp = app
        .oneshot(
            Request::post("/v1/definitions/lint")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::from("this is not xml"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    db.drop().await;
}

#[tokio::test]
async fn deploy_start_complete_inspect_over_http() {
    let (app, db) = test_app().await;

    // Deploy: atomic body, idempotent on repeat.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/definitions",
            serde_json::json!({ "bpmn": MINIMAL_XML }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let deployed = body_json(resp).await;
    assert_eq!(deployed["version"], 1);
    assert_eq!(deployed["reused"], false);

    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/definitions",
            serde_json::json!({ "bpmn": MINIMAL_XML }),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["reused"], true);

    // Start.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/instances",
            serde_json::json!({ "definitionKey": "p", "variables": { "orderId": 42 } }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let instance_id = body_json(resp).await["instanceId"]
        .as_str()
        .unwrap()
        .to_string();

    // Inspect: the token-overlay read model.
    let resp = app
        .clone()
        .oneshot(authed(
            "GET",
            &format!("/v1/instances/{instance_id}/inspect"),
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let inspection = body_json(resp).await;
    assert_eq!(inspection["status"], "active");
    assert_eq!(inspection["tokens"][0]["elementId"], "review");
    assert!(
        inspection["bpmnXml"]
            .as_str()
            .unwrap()
            .contains("bpmn:process")
    );
    let work_item = inspection["workItems"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Complete; repeat answers with the idempotent no-op.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/v1/work-items/{work_item}/complete"),
            serde_json::json!({ "patch": { "done": true } }),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["outcome"], "advanced");

    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/v1/work-items/{work_item}/complete"),
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["outcome"], "alreadyClosed");

    let resp = app
        .oneshot(authed(
            "GET",
            &format!("/v1/instances/{instance_id}/inspect"),
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    let inspection = body_json(resp).await;
    assert_eq!(inspection["status"], "completed");
    assert_eq!(
        inspection["variables"],
        serde_json::json!({ "orderId": 42, "done": true })
    );
    db.drop().await;
}

#[tokio::test]
async fn rejected_deploys_return_diagnostics() {
    let (app, db) = test_app().await;

    // Lint-dirty model.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/definitions",
            serde_json::json!({ "bpmn": INCLUSIVE_XML }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let json = body_json(resp).await;
    assert!(
        json["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["rule"] == "no-inclusive-gateway")
    );

    // Wiring gap -> unresolved-topic; grow the environment over the API and retry.
    let foreign =
        include_str!("../../rbpmn-model/tests/fixtures/accept/16-foreign-binding-warn.bpmn");
    let deploy_body = serde_json::json!({
        "bpmn": foreign,
        "bindings": { "topics": { "st": "payments" } }
    });
    let resp = app
        .clone()
        .oneshot(authed("POST", "/v1/definitions", deploy_body.clone()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let json = body_json(resp).await;
    assert!(
        json["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["rule"] == "unresolved-topic")
    );

    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/topics",
            serde_json::json!({ "name": "payments" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .oneshot(authed("POST", "/v1/definitions", deploy_body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    db.drop().await;
}

/// Message ingress: the full deploy -> start -> correlate flow over HTTP,
/// plus the loud no-match (404) and ambiguity (409) contracts.
#[tokio::test]
async fn message_ingress_delivers_and_is_loud_about_misses() {
    let (app, db) = test_app().await;
    let message_catch =
        include_str!("../../rbpmn-model/tests/fixtures/accept/17-message-catch.bpmn");
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/definitions",
            serde_json::json!({
                "bpmn": message_catch,
                "bindings": { "correlations": { "c": "order.id" } },
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // No subscription yet: 404, never dropped silently.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/messages",
            serde_json::json!({ "name": "WarehouseAck", "correlationKey": "o-1" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/instances",
            serde_json::json!({
                "definitionKey": "p",
                "variables": { "order": { "id": "o-1" } },
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let instance = body_json(resp).await["instanceId"]
        .as_str()
        .unwrap()
        .to_string();

    // Second instance with the same key makes delivery ambiguous: 409.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/instances",
            serde_json::json!({
                "definitionKey": "p",
                "variables": { "order": { "id": "o-1" } },
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/messages",
            serde_json::json!({ "name": "WarehouseAck", "correlationKey": "o-1" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // A unique key delivers, patches, and reports the receiving instance.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/instances",
            serde_json::json!({
                "definitionKey": "p",
                "variables": { "order": { "id": "o-unique" } },
            }),
        ))
        .await
        .unwrap();
    let unique = body_json(resp).await["instanceId"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/messages",
            serde_json::json!({
                "name": "WarehouseAck",
                "correlationKey": "o-unique",
                "patch": { "shipped": true },
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["instanceId"], unique.as_str());

    let resp = app
        .clone()
        .oneshot(authed(
            "GET",
            &format!("/v1/instances/{unique}/inspect"),
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    let inspection = body_json(resp).await;
    assert_eq!(inspection["status"], "completed");
    assert_eq!(inspection["variables"]["shipped"], true);
    let _ = instance;
    db.drop().await;
}
