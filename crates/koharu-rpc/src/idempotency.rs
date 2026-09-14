//! Durable HTTP idempotency receipts for side-effecting gateway requests.
//!
//! A request is first persisted as `pending`, then atomically replaced by its
//! exact success response or a known no-effect failure. Both terminal states
//! replay exactly. A crash with an ambiguous outcome remains `pending` and is
//! rejected rather than executed twice.

use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::sync::Mutex;

use anyhow::{Context, Result};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppState;
use crate::durable_json;
use crate::error::{ApiError, ApiResult};

const RECEIPT_VERSION: &str = "koharu-http-receipt.v2";
const LEGACY_RECEIPT_VERSION: &str = "koharu-http-receipt.v1";
const IDEMPOTENCY_HEADER: &str = "idempotency-key";

pub fn router() -> utoipa_axum::router::OpenApiRouter<AppState> {
    use utoipa_axum::routes;
    utoipa_axum::router::OpenApiRouter::default().routes(routes!(get_receipt))
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct ReceiptQuery {
    pub key: String,
}

#[utoipa::path(
    get,
    path = "/idempotency/receipt",
    params(ReceiptQuery),
    responses(
        (status = 200, body = IdempotencyReceipt),
        (status = 404, description = "Receipt not found")
    )
)]
async fn get_receipt(
    State(app): State<AppState>,
    Query(query): Query<ReceiptQuery>,
) -> ApiResult<Json<IdempotencyReceipt>> {
    let config = (**app.config.load()).clone();
    app.idempotency()
        .lookup(&config.data.path, &query.key)
        .map(Json)
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct IdempotencyReceipt {
    pub schema_version: String,
    pub key: String,
    pub scope: String,
    pub request_hash: String,
    pub state: ReceiptState,
    pub response: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_status: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptState {
    Pending,
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
pub struct Reservation {
    root: Utf8PathBuf,
    key: String,
    scope: String,
    request_hash: String,
}

#[derive(Debug)]
pub enum Decision {
    Disabled,
    Fresh(Reservation),
    Replay(Value),
    ReplayFailure(ApiError),
}

#[derive(Default)]
pub struct IdempotencyStore {
    lock: Mutex<()>,
}

impl IdempotencyStore {
    pub fn begin(
        &self,
        data_root: &Utf8Path,
        headers: &HeaderMap,
        scope: &str,
        request_hash: String,
    ) -> ApiResult<Decision> {
        let Some(key) = parse_key(headers)? else {
            return Ok(Decision::Disabled);
        };
        let root = data_root.join("http-idempotency");
        let reservation = Reservation {
            root,
            key,
            scope: scope.to_string(),
            request_hash,
        };
        let _guard = self
            .lock
            .lock()
            .map_err(|_| ApiError::internal(anyhow::anyhow!("idempotency lock poisoned")))?;
        ensure_receipt_root(data_root).map_err(ApiError::internal)?;
        let path = receipt_path(&reservation.root, &reservation.key);
        if path.exists() {
            let receipt = read_receipt(&path).map_err(ApiError::internal)?;
            if receipt.key != reservation.key
                || receipt.scope != reservation.scope
                || receipt.request_hash != reservation.request_hash
            {
                return Err(ApiError::new(
                    axum::http::StatusCode::CONFLICT,
                    "Idempotency-Key was already used for a different request",
                ));
            }
            return match (receipt.state, receipt.response, receipt.response_status) {
                (ReceiptState::Completed, Some(response), _) => Ok(Decision::Replay(response)),
                (ReceiptState::Failed, Some(response), Some(status)) => {
                    let mut error: ApiError = serde_json::from_value(response)
                        .map_err(|error| ApiError::internal(anyhow::Error::new(error)))?;
                    error.status = status;
                    Ok(Decision::ReplayFailure(error))
                }
                (ReceiptState::Pending, None, None) => Err(ApiError::new(
                    axum::http::StatusCode::CONFLICT,
                    "idempotent request outcome is pending or ambiguous; query its receipt",
                )),
                _ => Err(ApiError::internal(anyhow::anyhow!(
                    "invalid idempotency receipt state"
                ))),
            };
        }
        write_receipt(
            &path,
            &IdempotencyReceipt {
                schema_version: RECEIPT_VERSION.to_string(),
                key: reservation.key.clone(),
                scope: reservation.scope.clone(),
                request_hash: reservation.request_hash.clone(),
                state: ReceiptState::Pending,
                response: None,
                response_status: None,
            },
        )
        .map_err(ApiError::internal)?;
        Ok(Decision::Fresh(reservation))
    }

    pub fn complete(&self, reservation: Reservation, response: Value) -> ApiResult<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| ApiError::internal(anyhow::anyhow!("idempotency lock poisoned")))?;
        let path = receipt_path(&reservation.root, &reservation.key);
        let current = read_receipt(&path).map_err(ApiError::internal)?;
        if current.key != reservation.key
            || current.scope != reservation.scope
            || current.request_hash != reservation.request_hash
        {
            return Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "idempotency receipt binding changed",
            ));
        }
        if current.state == ReceiptState::Completed {
            if current.response.as_ref() == Some(&response) {
                return Ok(());
            }
            return Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "idempotency receipt response changed",
            ));
        }
        if current.state == ReceiptState::Failed {
            return Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "failed idempotency receipt cannot complete",
            ));
        }
        write_receipt(
            &path,
            &IdempotencyReceipt {
                schema_version: RECEIPT_VERSION.to_string(),
                key: reservation.key,
                scope: reservation.scope,
                request_hash: reservation.request_hash,
                state: ReceiptState::Completed,
                response: Some(response),
                response_status: Some(200),
            },
        )
        .map_err(ApiError::internal)
    }

    pub fn fail(&self, reservation: Reservation, error: &ApiError) -> ApiResult<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| ApiError::internal(anyhow::anyhow!("idempotency lock poisoned")))?;
        let path = receipt_path(&reservation.root, &reservation.key);
        let current = read_receipt(&path).map_err(ApiError::internal)?;
        if current.key != reservation.key
            || current.scope != reservation.scope
            || current.request_hash != reservation.request_hash
        {
            return Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "idempotency receipt binding changed",
            ));
        }
        let response = serde_json::to_value(error)
            .map_err(|error| ApiError::internal(anyhow::Error::new(error)))?;
        if current.state == ReceiptState::Failed {
            if current.response.as_ref() == Some(&response)
                && current.response_status == Some(error.status)
            {
                return Ok(());
            }
            return Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "idempotency receipt failure changed",
            ));
        }
        if current.state == ReceiptState::Completed {
            return Err(ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "completed idempotency receipt cannot fail",
            ));
        }
        write_receipt(
            &path,
            &IdempotencyReceipt {
                schema_version: RECEIPT_VERSION.to_string(),
                key: reservation.key,
                scope: reservation.scope,
                request_hash: reservation.request_hash,
                state: ReceiptState::Failed,
                response: Some(response),
                response_status: Some(error.status),
            },
        )
        .map_err(ApiError::internal)
    }

    pub fn lookup(&self, data_root: &Utf8Path, key: &str) -> ApiResult<IdempotencyReceipt> {
        validate_key(key)?;
        let _guard = self
            .lock
            .lock()
            .map_err(|_| ApiError::internal(anyhow::anyhow!("idempotency lock poisoned")))?;
        let path = receipt_path(&data_root.join("http-idempotency"), key);
        if !path.exists() {
            return Err(ApiError::not_found("idempotency receipt not found"));
        }
        let receipt = read_receipt(&path).map_err(ApiError::internal)?;
        if receipt.key != key {
            return Err(ApiError::internal(anyhow::anyhow!(
                "idempotency receipt key digest collision"
            )));
        }
        Ok(receipt)
    }
}

pub fn request_hash<T: Serialize>(scope: &str, request: &T) -> ApiResult<String> {
    let bytes = serde_json::to_vec(&(scope, request))
        .map_err(|error| ApiError::internal(anyhow::Error::new(error)))?;
    Ok(format!("blake3:{}", blake3::hash(&bytes).to_hex()))
}

pub fn complete_json<T: Serialize>(
    store: &IdempotencyStore,
    decision: Decision,
    response: &T,
) -> ApiResult<()> {
    if let Decision::Fresh(reservation) = decision {
        let value = serde_json::to_value(response)
            .map_err(|error| ApiError::internal(anyhow::Error::new(error)))?;
        store.complete(reservation, value)?;
    }
    Ok(())
}

pub fn replay_json<T: for<'de> Deserialize<'de>>(decision: &Decision) -> ApiResult<Option<T>> {
    match decision {
        Decision::Replay(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| ApiError::internal(anyhow::Error::new(error))),
        Decision::ReplayFailure(error) => Err(error.clone()),
        _ => Ok(None),
    }
}

/// Persist a known no-effect handler error so retries replay it exactly.
pub fn fail_json(store: &IdempotencyStore, decision: Decision, error: ApiError) -> ApiError {
    if let Decision::Fresh(reservation) = decision
        && let Err(persist_error) = store.fail(reservation, &error)
    {
        return persist_error;
    }
    error
}

fn parse_key(headers: &HeaderMap) -> ApiResult<Option<String>> {
    let Some(value) = headers.get(IDEMPOTENCY_HEADER) else {
        return Ok(None);
    };
    let key = value
        .to_str()
        .map_err(|_| ApiError::bad_request("Idempotency-Key must be ASCII"))?
        .to_string();
    validate_key(&key)?;
    Ok(Some(key))
}

fn validate_key(key: &str) -> ApiResult<()> {
    if key.is_empty()
        || key.len() > 255
        || key
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(ApiError::bad_request(
            "Idempotency-Key must be 1..255 printable non-whitespace ASCII characters",
        ));
    }
    Ok(())
}

fn receipt_path(root: &Utf8Path, key: &str) -> Utf8PathBuf {
    root.join(format!("{}.json", blake3::hash(key.as_bytes()).to_hex()))
}

fn ensure_receipt_root(data_root: &Utf8Path) -> Result<()> {
    let root = data_root.join("http-idempotency");
    #[cfg(unix)]
    let created = !root.exists();
    fs::create_dir_all(root.as_std_path())
        .with_context(|| format!("create idempotency receipt root {root}"))?;
    #[cfg(unix)]
    if created {
        File::open(data_root.as_std_path())
            .with_context(|| format!("open idempotency receipt parent {data_root}"))?
            .sync_all()
            .with_context(|| format!("sync idempotency receipt parent {data_root}"))?;
    }
    Ok(())
}

fn read_receipt(path: &Utf8Path) -> Result<IdempotencyReceipt> {
    let bytes = fs::read(path.as_std_path()).with_context(|| format!("read receipt {path}"))?;
    let receipt: IdempotencyReceipt =
        serde_json::from_slice(&bytes).with_context(|| format!("decode receipt {path}"))?;
    if receipt.schema_version != RECEIPT_VERSION && receipt.schema_version != LEGACY_RECEIPT_VERSION
    {
        anyhow::bail!("unsupported receipt schema {}", receipt.schema_version);
    }
    Ok(receipt)
}

fn write_receipt(path: &Utf8Path, receipt: &IdempotencyReceipt) -> Result<()> {
    durable_json::write(path.as_std_path(), receipt)
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::{
        Decision, IdempotencyStore, ReceiptState, complete_json, ensure_receipt_root, fail_json,
        replay_json, request_hash,
    };
    use crate::ApiError;

    fn request_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", HeaderValue::from_static("request-1"));
        headers
    }

    #[test]
    fn completed_receipt_replays_and_conflicting_request_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(temp.path()).unwrap();
        let store = IdempotencyStore::default();
        let headers = request_headers();
        let hash = request_hash("test", &serde_json::json!({"value": 1})).unwrap();
        let decision = store.begin(root, &headers, "test", hash.clone()).unwrap();
        assert!(matches!(decision, Decision::Fresh(_)));
        complete_json(&store, decision, &serde_json::json!({"ok": true})).unwrap();

        let replay = store.begin(root, &headers, "test", hash).unwrap();
        assert_eq!(
            replay_json::<serde_json::Value>(&replay).unwrap(),
            Some(serde_json::json!({"ok": true}))
        );
        let other = request_hash("test", &serde_json::json!({"value": 2})).unwrap();
        let error = store.begin(root, &headers, "test", other).unwrap_err();
        assert_eq!(error.status, 409);
    }

    #[test]
    fn pending_receipt_is_visible_to_a_new_store() {
        let temp = tempfile::tempdir().unwrap();
        let data_root = camino::Utf8Path::from_path(temp.path()).unwrap();
        let store = IdempotencyStore::default();
        let headers = request_headers();
        let request_hash = request_hash("test", &serde_json::json!({"value": 1})).unwrap();

        let decision = store
            .begin(data_root, &headers, "test", request_hash)
            .unwrap();
        assert!(matches!(decision, Decision::Fresh(_)));

        let reopened = IdempotencyStore::default();
        let receipt = reopened.lookup(data_root, "request-1").unwrap();
        assert_eq!(receipt.state, ReceiptState::Pending);
        assert_eq!(receipt.response, None);
    }

    #[test]
    fn completed_receipt_is_visible_to_a_new_store() {
        let temp = tempfile::tempdir().unwrap();
        let data_root = camino::Utf8Path::from_path(temp.path()).unwrap();
        let store = IdempotencyStore::default();
        let headers = request_headers();
        let request_hash = request_hash("test", &serde_json::json!({"value": 1})).unwrap();
        let expected_response = serde_json::json!({"ok": true});

        let decision = store
            .begin(data_root, &headers, "test", request_hash)
            .unwrap();
        complete_json(&store, decision, &expected_response).unwrap();

        let reopened = IdempotencyStore::default();
        assert_eq!(
            reopened.lookup(data_root, "request-1").unwrap().state,
            ReceiptState::Completed
        );
        assert_eq!(
            reopened.lookup(data_root, "request-1").unwrap().response,
            Some(expected_response)
        );
    }

    #[test]
    fn failed_receipt_replays_the_exact_no_effect_error() {
        let temp = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(temp.path()).unwrap();
        let store = IdempotencyStore::default();
        let headers = request_headers();
        let hash = request_hash("test", &serde_json::json!({"value": 1})).unwrap();
        let decision = store.begin(root, &headers, "test", hash.clone()).unwrap();
        let error = ApiError::new(axum::http::StatusCode::PRECONDITION_FAILED, "stale epoch");

        let returned = fail_json(&store, decision, error);
        assert_eq!(returned.status, 412);
        let receipt = store.lookup(root, "request-1").unwrap();
        assert_eq!(receipt.state, ReceiptState::Failed);
        assert_eq!(receipt.response_status, Some(412));

        let replay = store.begin(root, &headers, "test", hash).unwrap();
        let replayed = replay_json::<serde_json::Value>(&replay).unwrap_err();
        assert_eq!(replayed.status, 412);
        assert_eq!(replayed.message, "stale epoch");
    }

    #[test]
    fn receipt_root_creation_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let data_root = camino::Utf8Path::from_path(temp.path())
            .unwrap()
            .join("data");
        std::fs::create_dir(data_root.as_std_path()).unwrap();

        ensure_receipt_root(&data_root).unwrap();
        assert!(data_root.join("http-idempotency").is_dir());

        ensure_receipt_root(&data_root).unwrap();
    }
}
