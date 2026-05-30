use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use utoipa::{OpenApi, ToSchema};

use crate::client::{self, DepError};

pub const SERVICE: &str = "srvcs-divide";
pub const CONCERN: &str = "arithmetic: a / b (integer division)";
pub const DEPENDS_ON: &[&str] = &["srvcs-isnumber"];

/// Dependency endpoints, injected as router state so tests can point them at
/// mock services.
#[derive(Clone)]
pub struct Deps {
    pub isnumber_url: String,
}

#[derive(Serialize, ToSchema)]
pub struct Info {
    pub service: &'static str,
    pub concern: &'static str,
    pub depends_on: Vec<&'static str>,
}

/// `GET /` — service identity (srvcs service standard).
#[utoipa::path(get, path = "/", responses((status = 200, body = Info)))]
pub async fn index() -> Json<Info> {
    Json(Info {
        service: SERVICE,
        concern: CONCERN,
        depends_on: DEPENDS_ON.to_vec(),
    })
}

#[derive(Deserialize, ToSchema)]
pub struct EvalRequest {
    #[schema(value_type = Object)]
    pub a: Value,
    #[schema(value_type = Object)]
    pub b: Value,
}

#[derive(Serialize, ToSchema)]
pub struct QuotientResponse {
    #[schema(value_type = Object)]
    pub a: Value,
    #[schema(value_type = Object)]
    pub b: Value,
    pub result: i64,
}

/// Coerce a validated numeric JSON value to an integer, accepting whole floats
/// (`4.0`) but rejecting genuinely fractional ones (`4.5`).
fn as_integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        value
            .as_f64()
            .filter(|f| f.fract() == 0.0)
            .map(|f| f as i64)
    })
}

/// The single concern: integer (truncating) division `a / b`.
///
/// Truncates toward zero, matching Rust's `i64` division: `10 / 3 == 3` and
/// `(-7) / 2 == -3`. Callers must reject `b == 0` before calling this.
pub fn divide(a: i64, b: i64) -> i64 {
    a / b
}

fn ok(a: Value, b: Value, result: i64) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "a": a, "b": b, "result": result })),
    )
        .into_response()
}

fn invalid(reason: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({ "error": reason })),
    )
        .into_response()
}

fn degraded(dependency: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "dependency unavailable", "dependency": dependency })),
    )
        .into_response()
}

/// Forward an explicit error body with the given status (used for the
/// `division by zero` 422 this service produces itself).
fn forward(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

/// Ask `srvcs-isnumber` whether `value` is a number, then coerce it to an
/// integer.
///
/// Returns `Ok(i64)` on success, or an error `Response` (422/503) the caller
/// should return verbatim.
async fn ask_is_integer(isnumber_url: &str, value: &Value) -> Result<i64, Response> {
    // 1. Delegate "is this a number" to srvcs-isnumber.
    match client::call(isnumber_url, &json!({ "value": value })).await {
        Err(DepError::Unreachable) => return Err(degraded("srvcs-isnumber")),
        Ok((200, body)) => {
            let is_number = body.get("result").and_then(Value::as_bool).unwrap_or(false);
            if !is_number {
                return Err(invalid("value is not a number"));
            }
        }
        Ok(_) => return Err(degraded("srvcs-isnumber")),
    }

    // 2. Division is defined on integers.
    as_integer(value).ok_or_else(|| invalid("value is not an integer"))
}

/// `POST /` — compute `a / b` (integer division).
///
/// Both operands are validated as integers via `srvcs-isnumber` over HTTP (the
/// single source of truth for "is this a number"), once per operand. If that
/// dependency is unreachable, this service reports itself degraded rather than
/// guessing. Division by zero is rejected with `422`.
#[utoipa::path(
    post,
    path = "/",
    request_body = EvalRequest,
    responses(
        (status = 200, body = QuotientResponse),
        (status = 422, description = "an operand is not an integer, or division by zero"),
        (status = 503, description = "a dependency is unavailable"),
        (status = 500, description = "internal error")
    )
)]
pub async fn evaluate(State(deps): State<Deps>, Json(req): Json<EvalRequest>) -> Response {
    let a = match ask_is_integer(&deps.isnumber_url, &req.a).await {
        Ok(n) => n,
        Err(resp) => return resp,
    };
    let b = match ask_is_integer(&deps.isnumber_url, &req.b).await {
        Ok(n) => n,
        Err(resp) => return resp,
    };

    if b == 0 {
        return forward(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({ "error": "division by zero" }),
        );
    }

    ok(req.a, req.b, divide(a, b))
}

#[derive(OpenApi)]
#[openapi(
    paths(index, evaluate),
    components(schemas(Info, EvalRequest, QuotientResponse))
)]
pub struct ApiDoc;

/// Serve OpenAPI document
pub async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_documents_routes() {
        let doc = ApiDoc::openapi();
        let root = doc.paths.paths.get("/").expect("path / present");
        assert!(root.get.is_some());
        assert!(root.post.is_some());
    }

    #[test]
    fn quotient_truncates_toward_zero() {
        assert_eq!(divide(10, 3), 3);
        assert_eq!(divide(-7, 2), -3);
        assert_eq!(divide(7, -2), -3);
        assert_eq!(divide(-7, -2), 3);
        assert_eq!(divide(6, 2), 3);
        assert_eq!(divide(0, 5), 0);
    }

    #[test]
    fn whole_floats_are_integers_but_fractions_are_not() {
        assert_eq!(as_integer(&json!(4)), Some(4));
        assert_eq!(as_integer(&json!(4.0)), Some(4));
        assert_eq!(as_integer(&json!(-6.0)), Some(-6));
        assert_eq!(as_integer(&json!(4.5)), None);
    }

    #[tokio::test]
    async fn index_reports_concern_and_dependency() {
        let Json(info) = index().await;
        assert_eq!(info.service, "srvcs-divide");
        assert_eq!(info.concern, "arithmetic: a / b (integer division)");
        assert_eq!(info.depends_on, vec!["srvcs-isnumber"]);
    }
}
