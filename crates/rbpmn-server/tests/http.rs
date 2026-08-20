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
/// A user task with an interrupting message boundary: the payment that ends
/// a contested ticket while a clerk holds the task.
const BOUNDARY_XML: &str =
    include_str!("../../rbpmn-model/tests/fixtures/accept/29-message-boundary.bpmn");

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

    // A non-object patch would replace the entire variables document: 400.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/messages",
            serde_json::json!({ "name": "WarehouseAck", "correlationKey": "o-1", "patch": 5 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

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

/// The pull-mode task lifecycle over HTTP: claim (200/204), heartbeat
/// (extended / 409 lockLost), owner-checked completion.
#[tokio::test]
async fn task_api_lifecycle_over_http() {
    let (app, db) = test_app().await;
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
    let v1 = body_json(resp).await;
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/instances",
            serde_json::json!({ "definitionKey": "p", "variables": { "region": "north" } }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // A newer version of the same key: the running instance stays on v1.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/definitions",
            serde_json::json!({
                "bpmn": MINIMAL_XML.replace("<bpmn:process", "<!-- v2 --><bpmn:process"),
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["version"], 2);

    // Count, then claim FIFO.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/tasks/count",
            serde_json::json!({ "topic": "review" }),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["count"], 1);
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/tasks/get",
            serde_json::json!({ "topic": "review", "owner": "alice" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let task = body_json(resp).await["task"].clone();
    let lease_no = task["leaseNo"].as_i64().expect("the claim's lease epoch");
    assert_eq!(task["elementId"], "review");
    assert_eq!(task["variables"]["region"], "north");
    // The pinned definition identity, not max(version): a version-pinned
    // per-task screen manifest resolves against exactly this pair.
    assert_eq!(task["definitionKey"], "p");
    assert_eq!(task["definitionId"], v1["definitionId"]);
    assert_eq!(task["definitionVersion"], 1);
    let task_id = task["id"].as_str().unwrap().to_string();

    // Nothing left to claim: 204.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/tasks/get",
            serde_json::json!({ "topic": "review", "owner": "bob" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Heartbeat: owner extends; a stranger gets the typed 409.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/v1/tasks/{task_id}/extend"),
            serde_json::json!({ "owner": "alice", "ttlSeconds": 600 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["outcome"], "extended");
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/v1/tasks/{task_id}/extend"),
            serde_json::json!({ "owner": "bob", "ttlSeconds": 600 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["outcome"], "lockLost");
    // The item's own state, so the client can tell "somebody else has it"
    // from "the process took it away" — here it is still alice's.
    assert_eq!(body["state"], "locked");

    // Release hands the task back without deciding it — same vocabulary as
    // the heartbeat: a stranger's release is the typed 409 and changes
    // nothing, the owner's returns it to the queue at once.
    for (owner, lease) in [("bob", lease_no), ("alice", lease_no + 1)] {
        let resp = app
            .clone()
            .oneshot(authed(
                "POST",
                &format!("/v1/tasks/{task_id}/release"),
                serde_json::json!({ "owner": owner, "leaseNo": lease }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT, "{owner}/{lease}");
        let body = body_json(resp).await;
        assert_eq!(body["outcome"], "lockLost", "{owner}/{lease}");
        assert_eq!(body["state"], "locked", "{owner}/{lease}");
    }
    // The epoch is required, not defaulted: without it the request is a 400,
    // never a release scoped to whatever claim happens to be current.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/v1/tasks/{task_id}/release"),
            serde_json::json!({ "owner": "alice" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/v1/tasks/{task_id}/release"),
            serde_json::json!({ "owner": "alice", "leaseNo": lease_no }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["outcome"], "released");
    // Replaying it is the whole point of the epoch: it names a spent claim.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/v1/tasks/{task_id}/release"),
            serde_json::json!({ "owner": "alice", "leaseNo": lease_no }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Claimable again immediately, and by someone else — carol takes the
    // task alice just handed back, which is the point of releasing it.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/tasks/get",
            serde_json::json!({ "topic": "review", "owner": "carol" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["task"]["id"], task_id);

    // Completion is owner-checked: a stranger is refused — and after the
    // hand-back that includes alice, who no longer holds anything.
    for stranger in ["bob", "alice"] {
        let resp = app
            .clone()
            .oneshot(authed(
                "POST",
                &format!("/v1/tasks/{task_id}/complete"),
                serde_json::json!({ "owner": stranger, "patch": {} }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT, "{stranger}");
    }
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/v1/tasks/{task_id}/complete"),
            serde_json::json!({ "owner": "carol", "patch": { "approved": true } }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["outcome"], "advanced");
    db.drop().await;
}

/// The other half of `lockLost`, and the reason it grew a `state`: the
/// **process** withdrew the task. A payment correlated to the ticket fires
/// the interrupting boundary while the clerk is holding `handle_contest`, so
/// every verb the clerk has left answers about a `cancelled` item — the
/// frontend can say "the ticket was paid" instead of "you were reassigned".
#[tokio::test]
async fn a_withdrawn_task_reports_cancelled_on_every_verb() {
    let (app, db) = test_app().await;
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/definitions",
            serde_json::json!({
                "bpmn": BOUNDARY_XML,
                "bindings": { "correlations": { "paid_during_contest": "ticket.reference" } },
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/instances",
            serde_json::json!({
                "definitionKey": "ticket",
                "variables": { "ticket": { "reference": "T-1" } },
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let instance = body_json(resp).await["instanceId"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/tasks/get",
            serde_json::json!({ "topic": "handle_contest", "owner": "clerk" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let task = body_json(resp).await["task"].clone();
    let task_id = task["id"].as_str().unwrap().to_string();
    let lease_no = task["leaseNo"].as_i64().unwrap();

    // The payment arrives while the clerk holds the task. `POST /v1/messages`
    // is unchanged in every respect — the boundary subscription is a row like
    // any other.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/messages",
            serde_json::json!({ "name": "PAID", "correlationKey": "T-1" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["instanceId"], instance.as_str());

    // The heartbeat: 409, and the state says the process took it, not a peer.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/v1/tasks/{task_id}/extend"),
            serde_json::json!({ "owner": "clerk", "ttlSeconds": 600 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["outcome"], "lockLost");
    assert_eq!(body["state"], "cancelled");

    // Handing it back: the same answer. Note the lease columns still name
    // the clerk — cancellation writes the state column and nothing else —
    // so the state is the only thing that could have told the truth here.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/v1/tasks/{task_id}/release"),
            serde_json::json!({ "owner": "clerk", "leaseNo": lease_no }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["outcome"], "lockLost");
    assert_eq!(body["state"], "cancelled");

    // And the completion the clerk was about to send: the idempotent
    // already-closed answer, 200, with the same state — never a 5xx, never
    // a success that would have applied a patch to a finished ticket.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            &format!("/v1/tasks/{task_id}/complete"),
            serde_json::json!({ "owner": "clerk", "patch": { "contest": { "upheld": true } } }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["outcome"], "alreadyClosed");
    assert_eq!(body["state"], "cancelled");

    let resp = app
        .clone()
        .oneshot(authed(
            "GET",
            &format!("/v1/instances/{instance}/inspect"),
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    let inspection = body_json(resp).await;
    assert_eq!(inspection["status"], "completed");
    assert!(
        inspection["variables"].get("contest").is_none(),
        "the refused completion's patch must not have landed: {}",
        inspection["variables"]
    );
    db.drop().await;
}

/// Topic lifecycle over HTTP: declare, protected undeclare (409 naming the
/// dependent definitions), successful undeclare (204).
#[tokio::test]
async fn topic_undeclare_over_http() {
    let (app, db) = test_app().await;
    for name in ["payments", "unused"] {
        let resp = app
            .clone()
            .oneshot(authed(
                "POST",
                "/v1/topics",
                serde_json::json!({ "name": name }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }
    let with_service_task =
        include_str!("../../rbpmn-model/tests/fixtures/accept/16-foreign-binding-warn.bpmn");
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/definitions",
            serde_json::json!({
                "bpmn": with_service_task,
                "bindings": { "topics": { "st": "payments" } },
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(authed(
            "DELETE",
            "/v1/topics/payments",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert!(body["definitions"][0].as_str().unwrap().starts_with("p v1"));

    let resp = app
        .clone()
        .oneshot(authed("DELETE", "/v1/topics/unused", serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    db.drop().await;
}

/// The event stream over HTTP: auth, the camelCase cursor params, the
/// envelope shape, and loud rejection of a malformed cursor.
#[tokio::test]
async fn event_stream_over_http() {
    let (app, db) = test_app().await;
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
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/instances",
            serde_json::json!({ "definitionKey": "p" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Unauthenticated reads are refused like every other /v1 path.
    let resp = app
        .clone()
        .oneshot(Request::get("/v1/events").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // The horizon is cluster-wide, so a concurrent test's transaction can
    // briefly hold events back — poll for the first batch.
    let mut events = serde_json::Value::Null;
    for _ in 0..100 {
        let resp = app
            .clone()
            .oneshot(authed("GET", "/v1/events", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        events = body_json(resp).await["events"].clone();
        if !events.as_array().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let first = &events.as_array().unwrap()[0];
    assert_eq!(first["kind"], "instance-started");
    assert!(first["txid"].is_i64() && first["id"].is_i64());
    assert!(first["instanceId"].is_string());

    // The cursor params are camelCase, and paging past the last id is empty.
    let last = events.as_array().unwrap().last().unwrap();
    let uri = format!(
        "/v1/events?afterTxid={}&afterId={}",
        last["txid"].as_i64().unwrap(),
        last["id"].as_i64().unwrap()
    );
    let resp = app
        .clone()
        .oneshot(authed("GET", &uri, serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        body_json(resp).await["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // A malformed cursor is a 400, not a 500 or a silently dead stream.
    let resp = app
        .clone()
        .oneshot(authed(
            "GET",
            "/v1/events?afterTxid=-1",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = app
        .clone()
        .oneshot(authed("GET", "/v1/events?limit=0", serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    db.drop().await;
}

// ---------------------------------------------------------------------------
// The UI documents (feature `ui`)
// ---------------------------------------------------------------------------

#[cfg(feature = "ui")]
mod ui {
    use super::*;

    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn authed_get(uri: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap()
    }

    /// The documents sit behind the same bearer as everything else. A browser
    /// cannot set that header on a navigation, which is deliberate: reaching
    /// these pages goes through an application that authenticated somebody.
    #[tokio::test]
    async fn ui_routes_require_the_bearer() {
        let (app, db) = test_app().await;
        for uri in [
            "/ui/editor",
            "/ui/editor/api/environment",
            "/ui/inspect/00000000-0000-0000-0000-000000000000",
        ] {
            let resp = app
                .clone()
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "{uri} was reachable"
            );
        }
        db.drop().await;
    }

    /// Both spellings of the mount serve the document, because the editor
    /// resolves its API call relative to its own location and a user who
    /// omitted the slash should not get a 404.
    #[tokio::test]
    async fn editor_is_served_with_or_without_the_trailing_slash() {
        let (app, db) = test_app().await;
        for uri in ["/ui/editor", "/ui/editor/"] {
            let resp = app.clone().oneshot(authed_get(uri)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            assert_eq!(
                resp.headers().get(header::CONTENT_TYPE).unwrap(),
                "text/html; charset=utf-8"
            );
            assert_eq!(
                resp.headers().get(header::CACHE_CONTROL).unwrap(),
                "no-store, max-age=0"
            );
            assert_eq!(
                resp.headers().get("x-content-type-options").unwrap(),
                "nosniff"
            );
            let html = body_text(resp).await;
            assert!(html.starts_with("<!doctype html>"));
            assert!(html.contains("Content-Security-Policy"));
        }

        // ... but only those two. Serving the editor from an arbitrary
        // subpath would hand it a location whose API sibling does not exist:
        // the page resolves `api/environment` relative to itself, and a
        // catch-all would answer that with more HTML and a 200, failing
        // inside JSON.parse. A 404 is the honest answer.
        let resp = app
            .clone()
            .oneshot(authed_get("/ui/editor/not-a-page"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        db.drop().await;
    }

    /// L3: a list of topic names, not a validation endpoint. The editor does
    /// the subtraction itself, so the model never leaves the browser.
    #[tokio::test]
    async fn environment_returns_the_covered_topic_set() {
        let (app, db) = test_app().await;
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
            .clone()
            .oneshot(authed_get("/ui/editor/api/environment"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["topics"], serde_json::json!(["payments"]));
        db.drop().await;
    }

    /// The whole chain: deploy, start, render. The document must actually
    /// carry this instance's state — that is the difference between a page
    /// and a debug tool.
    #[tokio::test]
    async fn inspector_renders_a_real_instance() {
        let (app, db) = test_app().await;
        app.clone()
            .oneshot(authed(
                "POST",
                "/v1/definitions",
                serde_json::json!({ "bpmn": MINIMAL_XML }),
            ))
            .await
            .unwrap();
        let resp = app
            .clone()
            .oneshot(authed(
                "POST",
                "/v1/instances",
                serde_json::json!({ "definitionKey": "p", "variables": { "orderId": 42 } }),
            ))
            .await
            .unwrap();
        let instance_id = body_json(resp).await["instanceId"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = app
            .clone()
            .oneshot(authed_get(&format!("/ui/inspect/{instance_id}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = body_text(resp).await;

        // The data block is the document's whole payload; parse it back and
        // check it is this instance rather than merely "some HTML".
        let start = html.find("id=\"rbpmn-data\">").unwrap() + "id=\"rbpmn-data\">".len();
        let end = start + html[start..].find("</script>").unwrap();
        let data: serde_json::Value = serde_json::from_str(&html[start..end]).unwrap();
        assert_eq!(data["id"], instance_id);
        assert_eq!(data["status"], "active");
        assert_eq!(data["tokens"][0]["elementId"], "review");
        assert_eq!(data["variables"]["orderId"], 42);
        // The manifest travels with it, even when empty.
        assert!(data["bindings"].is_object());
        db.drop().await;
    }

    #[tokio::test]
    async fn unknown_instance_is_a_404_not_a_blank_page() {
        let (app, db) = test_app().await;
        let resp = app
            .clone()
            .oneshot(authed_get(
                "/ui/inspect/00000000-0000-0000-0000-000000000000",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        db.drop().await;
    }
}

/// The deploy body is the bundle: process, manifest and decision artifacts in
/// one atomic call. The editor this server ships validates exactly this shape
/// offline, so the server must accept what the editor approved — otherwise
/// the two surfaces reach different verdicts about the same JSON.
#[cfg(feature = "dmn")]
#[tokio::test]
async fn deploy_carries_decision_artifacts() {
    const PRICING: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<definitions xmlns="https://www.omg.org/spec/DMN/20191111/MODEL/"
             namespace="https://rbpmn.example/pricing" name="pricing" id="_pricing">
  <inputData name="Amount" id="amount"><variable name="Amount" typeRef="number"/></inputData>
  <decision name="Discount" id="discount">
    <variable name="Discount" typeRef="number"/>
    <informationRequirement><requiredInput href="#amount"/></informationRequirement>
    <literalExpression><text>Amount * 0.1</text></literalExpression>
  </decision>
</definitions>"##;

    let (app, db) = test_app().await;

    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/definitions",
            serde_json::json!({
                "bpmn": MINIMAL_XML,
                "decisions": [PRICING],
                "bindings": { "decisions": { "st": { "decision": "Discount", "result": "order.discount" } } },
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // A binding naming a decision the bundle does not carry is refused, with
    // the rule id — the same answer the editor gives without a server.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/definitions",
            serde_json::json!({
                "bpmn": MINIMAL_XML,
                "bindings": { "decisions": { "st": { "decision": "Missing", "result": "order.x" } } },
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert!(
        body["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["rule"] == "unresolved-decision"),
        "{body}"
    );
    db.drop().await;
}
