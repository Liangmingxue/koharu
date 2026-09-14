//! `POST /pipelines` — start a pipeline run as a long-running operation.
//!
//! Returns an `operationId`. Progress + completion flow through SSE
//! (`JobStarted` / `JobProgress` / `JobFinished`). Cancellation goes to
//! `DELETE /operations/{id}`.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use koharu_app::pipeline::{
    self, PipelineRunOptions, PipelineSpec, ProgressTick, Scope, WarningTick,
};
use koharu_core::{
    AppEvent, JobFinishedEvent, JobStatus, JobSummary, JobWarningEvent, NodeId, PageId,
    PipelineProgress, PipelineStatus, ReadingOrder, Region,
};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::idempotency::{complete_json, fail_json, replay_json, request_hash};
use crate::operation_journal::OperationJournal;
use crate::routes::history::{map_apply_error, parse_epoch_precondition};
use crate::routes::operations::{register_cancel, unregister_cancel};

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header::IF_MATCH};

    use super::required_epoch_precondition;

    #[test]
    fn pipeline_requires_a_scene_epoch_precondition() {
        assert_eq!(
            required_epoch_precondition(&HeaderMap::new())
                .unwrap_err()
                .status,
            428
        );
        let mut headers = HeaderMap::new();
        headers.insert(IF_MATCH, HeaderValue::from_static("\"17\""));
        assert_eq!(required_epoch_precondition(&headers).unwrap(), 17);
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::default().routes(routes!(start_pipeline))
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StartPipelineRequest {
    /// Engine ids (`inventory::submit!` ids) to run in order.
    pub steps: Vec<String>,
    /// `None` → whole project, `Some(pages)` → just those pages.
    #[serde(default)]
    pub pages: Option<Vec<PageId>>,
    /// Optional bounding-box hint for inpainter engines (repair-brush).
    #[serde(default)]
    pub region: Option<Region>,
    /// Optional text-node ids for engines that can operate on individual blocks.
    #[serde(default)]
    pub text_node_ids: Option<Vec<NodeId>>,
    #[serde(default)]
    pub target_language: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub default_font: Option<String>,
    #[serde(default)]
    pub reading_order: Option<ReadingOrder>,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub engine_config: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StartPipelineResponse {
    pub operation_id: String,
}

#[utoipa::path(
    post,
    path = "/pipelines",
    params(("If-Match" = String, Header, description = "Quoted or unquoted expected scene epoch")),
    request_body = StartPipelineRequest,
    responses(
        (status = 200, body = StartPipelineResponse),
        (status = 412, description = "Scene epoch differs from If-Match"),
        (status = 428, description = "If-Match is required")
    )
)]
async fn start_pipeline(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<StartPipelineRequest>,
) -> ApiResult<Json<StartPipelineResponse>> {
    let expected_epoch = required_epoch_precondition(&headers)?;
    let session = app
        .current_session()
        .ok_or_else(|| ApiError::bad_request("no project open"))?;
    // Validate every step resolves to a registered engine before spawning.
    for id in &req.steps {
        pipeline::Registry::find(id).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    }
    if req.seed.unwrap_or(0) != 0 {
        return Err(ApiError::bad_request(
            "current deterministic pipeline adapter only supports seed=0",
        ));
    }
    if req
        .engine_config
        .as_ref()
        .map(|value| match value.as_object() {
            Some(object) => !object.is_empty(),
            None => true,
        })
        .unwrap_or(false)
    {
        return Err(ApiError::bad_request(
            "current deterministic pipeline adapter only supports an empty engineConfig",
        ));
    }
    let config = (**app.config.load()).clone();
    let pipeline_request_hash = request_hash(
        "POST /pipelines",
        &(session.dir.as_str(), expected_epoch, &req),
    )?;
    let decision = app.idempotency().begin(
        &config.data.path,
        &headers,
        "POST /pipelines",
        pipeline_request_hash.clone(),
    )?;
    if let Some(response) = replay_json::<StartPipelineResponse>(&decision)? {
        return Ok(Json(response));
    }
    let actual_epoch = session.epoch();
    if actual_epoch != expected_epoch {
        let error = map_apply_error(
            koharu_app::session::SceneEpochMismatch {
                expected: expected_epoch,
                actual: actual_epoch,
            }
            .into(),
        );
        return Err(fail_json(app.idempotency(), decision, error));
    }
    let spec = PipelineSpec {
        scope: match req.pages {
            Some(pages) => Scope::Pages(pages),
            None => Scope::WholeProject,
        },
        steps: req.steps,
        options: PipelineRunOptions {
            target_language: req.target_language,
            system_prompt: req.system_prompt,
            default_font: req.default_font,
            text_node_ids: req.text_node_ids,
            region: req.region,
            reading_order: req.reading_order,
            seed: req.seed,
            engine_config: req.engine_config,
        },
        expected_epoch,
    };

    let operation_id = Uuid::new_v4().to_string();
    let running = JobSummary {
        id: operation_id.clone(),
        kind: "pipeline".to_string(),
        status: JobStatus::Running,
        error: None,
    };
    let journal = OperationJournal::default();
    if let Err(error) = journal.record_running(
        &config.data.path,
        session.dir.as_str(),
        &pipeline_request_hash,
        &running,
    ) {
        return Err(fail_json(
            app.idempotency(),
            decision,
            ApiError::internal(error),
        ));
    }

    let response = StartPipelineResponse {
        operation_id: operation_id.clone(),
    };
    if let Err(error) = complete_json(app.idempotency(), decision, &response) {
        let _ = journal.record_terminal(
            &config.data.path,
            &operation_id,
            JobStatus::Failed,
            Some(error.message.clone()),
        );
        app.jobs.remove(&operation_id);
        return Err(error);
    }
    app.jobs.insert(operation_id.clone(), running);
    app.bus.publish(AppEvent::JobStarted {
        id: operation_id.clone(),
        kind: "pipeline".to_string(),
    });

    // The accepted operation and its receipt are durable before it is
    // detached. On restart, a still-running record becomes an explicit
    // failure instead of disappearing from `/operations`.
    let cancel = Arc::new(AtomicBool::new(false));
    register_cancel(operation_id.clone(), cancel.clone());
    let app_c = app.clone();
    let session_c = session.clone();
    let op_id_c = operation_id.clone();
    let registry_c = app.registry.clone();
    let runtime_c = app.runtime.clone();
    let llm_c = app.llm.clone();
    let renderer_c = app.renderer.clone();
    let cpu = app.cpu_only();
    let data_root_c = config.data.path.clone();
    let progress_bus = app.bus.clone();
    let progress_op_id = operation_id.clone();
    let progress_sink: pipeline::ProgressSink = Arc::new(move |tick: ProgressTick| {
        progress_bus.publish(AppEvent::JobProgress(PipelineProgress {
            job_id: progress_op_id.clone(),
            status: PipelineStatus::Running,
            step: tick.step,
            current_page: tick.page_index,
            total_pages: tick.total_pages,
            current_step_index: tick.step_index,
            total_steps: tick.total_steps,
            overall_percent: tick.overall_percent,
        }));
    });
    let warning_bus = app.bus.clone();
    let warning_op_id = operation_id.clone();
    let warning_sink: pipeline::WarningSink = Arc::new(move |tick: WarningTick| {
        warning_bus.publish(AppEvent::JobWarning(JobWarningEvent {
            job_id: warning_op_id.clone(),
            page_index: tick.page_index,
            total_pages: tick.total_pages,
            step_id: tick.step_id,
            message: tick.message,
        }));
    });
    tokio::spawn(async move {
        let result = pipeline::run(
            session_c,
            registry_c,
            runtime_c,
            cpu,
            llm_c,
            renderer_c,
            spec,
            cancel,
            Some(progress_sink),
            Some(warning_sink),
        )
        .await;
        let (status, error) = match &result {
            Ok(outcome) if outcome.warning_count == 0 => (JobStatus::Completed, None),
            Ok(outcome) => (
                JobStatus::CompletedWithErrors,
                Some(format!(
                    "{} step(s) failed; see warnings for details",
                    outcome.warning_count
                )),
            ),
            Err(e) if e.to_string().contains("cancelled") => (JobStatus::Cancelled, None),
            Err(e) => {
                tracing::warn!(operation_id = %op_id_c, "pipeline run failed: {e:#}");
                (JobStatus::Failed, Some(format!("{e:#}")))
            }
        };
        let terminal = OperationJournal::default()
            .record_terminal(&data_root_c, &op_id_c, status, error.clone())
            .unwrap_or_else(|journal_error| JobSummary {
                id: op_id_c.clone(),
                kind: "pipeline".to_string(),
                status: JobStatus::Failed,
                error: Some(format!("persist terminal operation: {journal_error:#}")),
            });
        let status = terminal.status;
        let error = terminal.error.clone();
        app_c.jobs.insert(op_id_c.clone(), terminal);
        app_c.bus.publish(AppEvent::JobFinished(JobFinishedEvent {
            id: op_id_c.clone(),
            status,
            error,
        }));
        unregister_cancel(&op_id_c);
    });

    Ok(Json(response))
}

fn required_epoch_precondition(headers: &HeaderMap) -> ApiResult<u64> {
    parse_epoch_precondition(headers)?.ok_or_else(|| {
        ApiError::new(
            axum::http::StatusCode::PRECONDITION_REQUIRED,
            "POST /pipelines requires If-Match with the expected scene epoch",
        )
    })
}
