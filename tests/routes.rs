use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router as AxumRouter};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use srvcs_divide::{api::Deps, health, router, telemetry};
use tower::ServiceExt;

/// Spin up a mock dependency that answers `POST /` with a fixed status + body,
/// and return its base URL. Lets us test orchestration without the real fleet.
async fn spawn_mock(status: StatusCode, body: Value) -> String {
    let app = AxumRouter::new().route(
        "/",
        post(move || {
            let body = body.clone();
            async move { (status, Json(body)) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Spin up a mock `srvcs-isnumber` that actually inspects the request body and
/// answers truthfully: `{"result": true}` when `value` is a JSON number,
/// `{"result": false}` otherwise. This genuinely exercises the per-operand
/// validation path rather than rubber-stamping every call.
async fn spawn_isnumber() -> String {
    let app = AxumRouter::new().route(
        "/",
        post(|Json(req): Json<Value>| async move {
            let is_number = req.get("value").map(Value::is_number).unwrap_or(false);
            (StatusCode::OK, Json(json!({ "result": is_number })))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn app(isnumber_url: &str) -> axum::Router {
    router(
        telemetry::metrics_handle_for_tests(),
        Deps {
            isnumber_url: isnumber_url.to_string(),
        },
    )
}

async fn eval(isnumber_url: &str, a: Value, b: Value) -> (StatusCode, Value) {
    let res = app(isnumber_url)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "a": a, "b": b }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

// A base URL with nothing listening — exercises the degraded path.
const DEAD_URL: &str = "http://127.0.0.1:1";

async fn status_of(uri: &str) -> StatusCode {
    app(DEAD_URL)
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn index_ok() {
    assert_eq!(status_of("/").await, StatusCode::OK);
}

#[tokio::test]
async fn healthz_ok() {
    assert_eq!(status_of("/healthz").await, StatusCode::OK);
}

#[tokio::test]
async fn readyz_reflects_state() {
    health::set_ready(true);
    assert_eq!(status_of("/readyz").await, StatusCode::OK);
}

#[tokio::test]
async fn metrics_ok() {
    assert_eq!(status_of("/metrics").await, StatusCode::OK);
}

#[tokio::test]
async fn openapi_ok() {
    assert_eq!(status_of("/openapi.json").await, StatusCode::OK);
}

#[tokio::test]
async fn generates_request_id_when_absent() {
    let res = app(DEAD_URL)
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        res.headers().contains_key("x-request-id"),
        "response must carry a generated x-request-id"
    );
}

#[tokio::test]
async fn divides_truncating_toward_zero_when_isnumber_agrees() {
    let isnumber = spawn_mock(StatusCode::OK, json!({ "result": true })).await;

    let (status, body) = eval(&isnumber, json!(10), json!(3)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["a"], 10);
    assert_eq!(body["b"], 3);
    assert_eq!(body["result"], 3);

    let (status, body) = eval(&isnumber, json!(-7), json!(2)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"], -3);
}

#[tokio::test]
async fn rejects_division_by_zero() {
    let isnumber = spawn_mock(StatusCode::OK, json!({ "result": true })).await;
    let (status, body) = eval(&isnumber, json!(10), json!(0)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "division by zero");
}

#[tokio::test]
async fn rejects_operand_isnumber_says_is_not_a_number() {
    let isnumber = spawn_mock(StatusCode::OK, json!({ "result": false })).await;
    let (status, _) = eval(&isnumber, json!("nope"), json!(3)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn degrades_when_isnumber_is_unreachable() {
    let (status, body) = eval(DEAD_URL, json!(10), json!(3)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["dependency"], "srvcs-isnumber");
}

#[tokio::test]
async fn truthful_isnumber_accepts_numbers_and_rejects_non_numbers() {
    // A mock that actually inspects the body: a numeric operand divides,
    // a string operand is rejected 422.
    let isnumber = spawn_isnumber().await;

    let (status, body) = eval(&isnumber, json!(9), json!(4)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"], 2);

    let (status, _) = eval(&isnumber, json!(9), json!("four")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
