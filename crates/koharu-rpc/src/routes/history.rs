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
use koharu_app::session::{ProjectSession, SceneEpochMismatch};
use koharu_core::Op;
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::idempotency::{complete_json, replay_json, request_hash};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::default()
        .routes(routes!(apply_command))
        .routes(routes!(undo))
        .routes(routes!(redo))
}

#[cfg(test)]
mod tests {
    use super::{apply_to_session, history_request_hash, parse_epoch_precondition};
    use axum::http::{HeaderMap, HeaderValue, header::IF_MATCH};
    use koharu_app::ProjectSession;
    use koharu_core::Op;

    use crate::idempotency::{Decision, IdempotencyStore, complete_json};

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

    #[test]
    fn history_receipt_conflicts_across_sessions_and_mutation_stays_pinned() {
        let temp = tempfile::tempdir().unwrap();
        let data_root = camino::Utf8Path::from_path(temp.path()).unwrap();
        let session_a = ProjectSession::create(
            camino::Utf8PathBuf::from_path_buf(temp.path().join("a.khrproj")).unwrap(),
            "A",
        )
        .unwrap();
        let session_b = ProjectSession::create(
            camino::Utf8PathBuf::from_path_buf(temp.path().join("b.khrproj")).unwrap(),
            "B",
        )
        .unwrap();
        let op = Op::UpdateProjectMeta {
            patch: Default::default(),
            prev: Default::default(),
        };
        let expected = Some(0);
        let hash_a = history_request_hash(&session_a, expected, &op).unwrap();
        let hash_b = history_request_hash(&session_b, expected, &op).unwrap();
        assert_ne!(hash_a, hash_b);

        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", HeaderValue::from_static("request-1"));
        let store = IdempotencyStore::default();
        let decision = store
            .begin(data_root, &headers, "POST /history/apply", hash_a)
            .unwrap();
        assert!(matches!(decision, Decision::Fresh(_)));
        complete_json(&store, decision, &serde_json::json!({"epoch": 1})).unwrap();
        let conflict = store
            .begin(data_root, &headers, "POST /history/apply", hash_b)
            .unwrap_err();
        assert_eq!(conflict.status, 409);

        assert_eq!(apply_to_session(&session_a, expected, op).unwrap(), 1);
        assert_eq!(session_a.epoch(), 1);
        assert_eq!(session_b.epoch(), 0);
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
    let session = app
        .current_session()
        .ok_or_else(|| ApiError::bad_request("no project open"))?;
    let config = (**app.config.load()).clone();
    let decision = app.idempotency().begin(
        &config.data.path,
        &headers,
        "POST /history/apply",
        history_request_hash(&session, expected, &op)?,
    )?;
    if let Some(response) = replay_json::<HistoryResult>(&decision)? {
        return Ok(Json(response));
    }
    let epoch = apply_to_session(&session, expected, op).map_err(map_apply_error)?;
    let response = HistoryResult { epoch: Some(epoch) };
    complete_json(app.idempotency(), decision, &response)?;
    Ok(Json(response))
}

fn history_request_hash(
    session: &ProjectSession,
    expected: Option<u64>,
    op: &Op,
) -> ApiResult<String> {
    request_hash("POST /history/apply", &(session.dir.as_str(), expected, op))
}

fn apply_to_session(
    session: &ProjectSession,
    expected: Option<u64>,
    op: Op,
) -> anyhow::Result<u64> {
    match expected {
        Some(epoch) => session.apply_if_epoch(epoch, op),
        None => session.apply(op),
    }
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
