//! Issue #2305: a `semantic` query sent to a wildcard (or any other
//! multi-index pattern) must not fail the whole request when some resolved
//! indices have no `semantic_text` field for the queried name.
//!
//! Autoindex puts several datasets behind one prefix (`proj-*`). Only some
//! of them elect `semantic_text`. Those that do not still 400 a `semantic`
//! query on their own (#530) — an explicitly named single index keeps that
//! error — but a search across the prefix skips them and returns hits from
//! the indices that can answer.
//!
//! Elasticsearch is referenced for wire semantics only; no code from it is
//! reproduced here.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn app() -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = xerj_common::config::Config::default();
    config.server.data_dir = dir.path().to_string_lossy().into_owned();
    config.storage.wal_sync = xerj_common::config::WalSync::Async;
    let metrics = xerj_common::metrics::Metrics::new().expect("metrics");
    let engine = xerj_engine::Engine::new(config.clone()).expect("engine");
    let state = xerj_api::state::AppState::new(config, engine, metrics);
    (xerj_api::router::build_es_compat_router(state), dir)
}

async fn json_req(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn index_pair(app: &axum::Router) {
    let (status, body) = json_req(
        app,
        "PUT",
        "/sem2305-docs",
        json!({ "mappings": { "properties": {
            "body": { "type": "semantic_text", "dimensions": 32 }
        } } }),
    )
    .await;
    assert!(
        status.is_success(),
        "create semantic index: {status} {body}"
    );

    let (status, body) = json_req(
        app,
        "PUT",
        "/sem2305-rows",
        json!({ "mappings": { "properties": {
            "body": { "type": "text" },
            "tag": { "type": "keyword" }
        } } }),
    )
    .await;
    assert!(status.is_success(), "create plain index: {status} {body}");

    let (status, body) = json_req(
        app,
        "POST",
        "/sem2305-docs/_doc/doc-1",
        json!({ "body": "graph edges connect nodes in a network" }),
    )
    .await;
    assert!(status.is_success(), "index semantic doc: {status} {body}");

    let (status, body) = json_req(
        app,
        "POST",
        "/sem2305-rows/_doc/row-1",
        json!({ "body": "quarterly revenue spreadsheet", "tag": "finance" }),
    )
    .await;
    assert!(status.is_success(), "index plain doc: {status} {body}");

    for index in ["sem2305-docs", "sem2305-rows"] {
        let (status, body) = json_req(app, "POST", &format!("/{index}/_refresh"), json!({})).await;
        assert!(status.is_success(), "refresh {index}: {status} {body}");
    }
}

fn semantic_body(field: &str) -> Value {
    json!({
        "query": { "semantic": { "field": field, "query": "graph edges", "k": 10 } },
        "size": 10
    })
}

fn hit_indices(body: &Value) -> Vec<&str> {
    body.pointer("/hits/hits")
        .and_then(Value::as_array)
        .map(|hits| {
            hits.iter()
                .filter_map(|hit| hit.get("_index").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default()
}

/// Two indices behind `sem2305-*`, only one with `body` as `semantic_text`.
/// The wildcard search returns HTTP 200 and hits from that index only.
#[tokio::test]
async fn semantic_wildcard_skips_indices_without_the_field() {
    let (app, _dir) = app().await;
    index_pair(&app).await;

    let (status, body) = json_req(&app, "POST", "/sem2305-*/_search", semantic_body("body")).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "a semantic query over a wildcard must skip indices with no semantic_text \
         field instead of failing the request (#2305): {status} {body}"
    );
    let indices = hit_indices(&body);
    assert!(
        indices.contains(&"sem2305-docs"),
        "hits must come from the index that has the semantic field: {body}"
    );
    assert!(
        indices.iter().all(|name| *name == "sem2305-docs"),
        "the plain-text index must not contribute hits: {indices:?} {body}"
    );
    let failed = body
        .pointer("/_shards/failed")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    assert!(
        failed >= 1,
        "the skipped index is a shard failure, not a silent omission (#2305): {body}"
    );
    let note = body
        .pointer("/_shards/failures")
        .map(|v| v.to_string())
        .unwrap_or_default();
    assert!(
        note.contains("sem2305-rows") && note.contains("body"),
        "the shard note must name the skipped index and the field: {body}"
    );
}

/// A comma-separated multi-index pattern is the same fan-out: the incapable
/// member is skipped, the capable member's hits are returned.
#[tokio::test]
async fn semantic_comma_pattern_skips_indices_without_the_field() {
    let (app, _dir) = app().await;
    index_pair(&app).await;

    let (status, body) = json_req(
        &app,
        "POST",
        "/sem2305-docs,sem2305-rows/_search",
        semantic_body("body"),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "a semantic query over a comma-separated pattern must skip the index \
         with no semantic_text field (#2305): {status} {body}"
    );
    let indices = hit_indices(&body);
    assert!(
        indices.contains(&"sem2305-docs") && indices.iter().all(|name| *name == "sem2305-docs"),
        "hits must come only from the semantic index: {indices:?} {body}"
    );
}

/// An explicitly named index without the field still 400s (#530). Skipping
/// is only for a pattern that resolved to more than one index.
#[tokio::test]
async fn semantic_on_one_named_index_without_the_field_stays_400() {
    let (app, _dir) = app().await;
    index_pair(&app).await;

    let (status, body) =
        json_req(&app, "POST", "/sem2305-rows/_search", semantic_body("body")).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a single named index without semantic_text must still 400 (#530): {status} {body}"
    );
    assert!(
        body.pointer("/error/reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("body"),
        "the 400 must name the field: {body}"
    );
}

/// A wildcard whose resolved indices all lack the field still 400s. Skipping
/// an incapable index must not turn "nothing can answer" into an empty 200.
#[tokio::test]
async fn semantic_wildcard_400s_when_no_index_has_the_field() {
    let (app, _dir) = app().await;
    index_pair(&app).await;

    let (status, body) = json_req(
        &app,
        "POST",
        "/sem2305-*/_search",
        semantic_body("not_a_field"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "when every matched index lacks the field the request must still 400, \
         not answer a silent empty 200 (#2305/#530): {status} {body}"
    );
    assert!(
        body.pointer("/error/reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("not_a_field"),
        "the 400 must name the field: {body}"
    );
}
