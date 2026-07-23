//! Scene mutation routes — the only way a client changes the scene.
//!
//! - `POST /history/apply` — apply an `Op` (including `Op::Batch`)
//! - `POST /history/undo`  — revert the last applied op
//! - `POST /history/redo`  — re-apply the last undone op
//!
//! Three distinct sub-resource actions under `/history` (Stripe-style
//! named-action URLs). Each returns `{ epoch }` — populated if the action
//! advanced the scene, `None` for a no-op boundary.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use koharu_app::session::SceneEpochMismatch;
use koharu_core::Op;
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::error::{ApiError, ApiResult};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::default()
        .routes(routes!(apply_command))
        .routes(routes!(undo))
        .routes(routes!(redo))
}

#[cfg(test)]
mod tests {
    use super::parse_epoch_precondition;
    use axum::http::{HeaderMap, HeaderValue, header::IF_MATCH};

    fn headers(value: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(IF_MATCH, HeaderValue::from_static(value));
        headers
    }

    #[test]
    fn epoch_precondition_accepts_one_strong_numeric_value() {
        assert_eq!(
            parse_epoch_precondition(&headers("\"42\"")).unwrap(),
            Some(42)
        );
        assert_eq!(parse_epoch_precondition(&headers("42")).unwrap(), Some(42));
    }

    #[test]
    fn epoch_precondition_rejects_weak_multiple_or_mismatched_quotes() {
        for value in ["W/\"42\"", "\"42\", \"43\"", "\"42", "42\"", "\"\"42\"\""] {
            assert!(
                parse_epoch_precondition(&headers(value)).is_err(),
                "{value}"
            );
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HistoryResult {
    /// New epoch. `None` only for a no-op undo/redo at the stack boundary.
    pub epoch: Option<u64>,
}

#[utoipa::path(
    post,
    path = "/history/apply",
    params(("If-Match" = Option<String>, Header, description = "Quoted or unquoted expected scene epoch")),
    request_body = Op,
    responses(
        (status = 200, body = HistoryResult),
        (status = 412, description = "Scene epoch differs from If-Match"),
        (status = 400, description = "Malformed If-Match epoch")
    )
)]
async fn apply_command(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(op): Json<Op>,
) -> ApiResult<Json<HistoryResult>> {
    let expected = parse_epoch_precondition(&headers)?;
    let epoch = match expected {
        Some(epoch) => app.apply_if_epoch(epoch, op).map_err(map_apply_error)?,
        None => app.apply(op).map_err(ApiError::internal)?,
    };
    Ok(Json(HistoryResult { epoch: Some(epoch) }))
}

pub(crate) fn parse_epoch_precondition(headers: &HeaderMap) -> ApiResult<Option<u64>> {
    let Some(value) = headers.get(axum::http::header::IF_MATCH) else {
        return Ok(None);
    };
    let raw = value
        .to_str()
        .map_err(|_| ApiError::bad_request("If-Match must be an ASCII scene epoch"))?;
    let raw = raw.trim();
    if raw.is_empty() || raw == "*" || raw.contains(',') || raw.starts_with("W/") {
        return Err(ApiError::bad_request(
            "If-Match must contain exactly one strong numeric scene epoch",
        ));
    }
    let normalized = match (raw.strip_prefix('"'), raw.strip_suffix('"')) {
        (Some(without_prefix), Some(_)) if raw.len() >= 2 => {
            &without_prefix[..without_prefix.len() - 1]
        }
        (None, None) => raw,
        _ => {
            return Err(ApiError::bad_request(
                "If-Match scene epoch must be either unquoted or enclosed by one quote pair",
            ));
        }
    };
    if normalized.is_empty() || normalized.contains('"') {
        return Err(ApiError::bad_request(
            "If-Match scene epoch must be an unsigned integer",
        ));
    }
    normalized
        .parse::<u64>()
        .map(Some)
        .map_err(|_| ApiError::bad_request("If-Match scene epoch is not an unsigned integer"))
}

pub(crate) fn map_apply_error(error: anyhow::Error) -> ApiError {
    if let Some(mismatch) = error.downcast_ref::<SceneEpochMismatch>() {
        return ApiError::new(StatusCode::PRECONDITION_FAILED, mismatch.to_string());
    }
    ApiError::internal(error)
}

#[utoipa::path(post, path = "/history/undo", responses((status = 200, body = HistoryResult)))]
async fn undo(State(app): State<AppState>) -> ApiResult<Json<HistoryResult>> {
    let epoch = app.undo().map_err(ApiError::internal)?;
    Ok(Json(HistoryResult { epoch }))
}

#[utoipa::path(post, path = "/history/redo", responses((status = 200, body = HistoryResult)))]
async fn redo(State(app): State<AppState>) -> ApiResult<Json<HistoryResult>> {
    let epoch = app.redo().map_err(ApiError::internal)?;
    Ok(Json(HistoryResult { epoch }))
}
