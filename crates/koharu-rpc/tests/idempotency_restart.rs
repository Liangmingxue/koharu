use axum::http::{HeaderMap, HeaderValue};
use koharu_rpc::idempotency::{Decision, IdempotencyStore, complete_json, request_hash};

#[test]
fn completed_receipt_survives_store_recreation() {
    let temp = tempfile::tempdir().unwrap();
    let root = camino::Utf8Path::from_path(temp.path()).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("idempotency-key", HeaderValue::from_static("request-1"));
    let hash = request_hash("test", &serde_json::json!({"value": 1})).unwrap();

    let first = IdempotencyStore::default();
    let decision = first.begin(root, &headers, "test", hash).unwrap();
    assert!(matches!(decision, Decision::Fresh(_)));
    complete_json(&first, decision, &serde_json::json!({"ok": true})).unwrap();
    drop(first);

    let reopened = IdempotencyStore::default();
    let receipt = reopened.lookup(root, "request-1").unwrap();
    assert_eq!(
        receipt.state,
        koharu_rpc::idempotency::ReceiptState::Completed
    );
    assert_eq!(receipt.response, Some(serde_json::json!({"ok": true})));
}
