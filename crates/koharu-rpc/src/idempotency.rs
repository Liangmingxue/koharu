//! Durable HTTP idempotency receipts for side-effecting gateway requests.
//!
//! A request is first persisted as `pending`, then replaced atomically with
//! its exact JSON response.  Completed requests replay that response.  A
//! process crash between the effect and completion remains `pending` and is
//! rejected rather than executed twice.

use std::fs::{self, File};
use std::io::Write;
use std::sync::Mutex;

use anyhow::{Context, Result};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppState;
use crate::error::{ApiError, ApiResult};

const RECEIPT_VERSION: &str = "koharu-http-receipt.v1";
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptState {
    Pending,
    Completed,
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
        fs::create_dir_all(reservation.root.as_std_path()).map_err(|error| {
            ApiError::internal(anyhow::Error::new(error).context("create idempotency receipt root"))
        })?;
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
            return match (receipt.state, receipt.response) {
                (ReceiptState::Completed, Some(response)) => Ok(Decision::Replay(response)),
                (ReceiptState::Pending, None) => Err(ApiError::new(
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
        write_receipt(
            &path,
            &IdempotencyReceipt {
                schema_version: RECEIPT_VERSION.to_string(),
                key: reservation.key,
                scope: reservation.scope,
                request_hash: reservation.request_hash,
                state: ReceiptState::Completed,
                response: Some(response),
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
        _ => Ok(None),
    }
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

fn read_receipt(path: &Utf8Path) -> Result<IdempotencyReceipt> {
    let bytes = fs::read(path.as_std_path()).with_context(|| format!("read receipt {path}"))?;
    let receipt: IdempotencyReceipt =
        serde_json::from_slice(&bytes).with_context(|| format!("decode receipt {path}"))?;
    if receipt.schema_version != RECEIPT_VERSION {
        anyhow::bail!("unsupported receipt schema {}", receipt.schema_version);
    }
    Ok(receipt)
}

fn write_receipt(path: &Utf8Path, receipt: &IdempotencyReceipt) -> Result<()> {
    let parent = path.parent().context("receipt path has no parent")?;
    fs::create_dir_all(parent.as_std_path())?;
    let temporary = parent.join(format!(".receipt-{}.tmp", uuid::Uuid::new_v4()));
    let bytes = serde_json::to_vec(receipt)?;
    let mut file = File::create(temporary.as_std_path())?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(temporary.as_std_path(), path.as_std_path())?;
    File::open(parent.as_std_path())?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::{Decision, IdempotencyStore, complete_json, replay_json, request_hash};

    #[test]
    fn completed_receipt_replays_and_conflicting_request_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(temp.path()).unwrap();
        let store = IdempotencyStore::default();
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", HeaderValue::from_static("request-1"));
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
        assert!(store.begin(root, &headers, "test", other).is_err());
    }
}
