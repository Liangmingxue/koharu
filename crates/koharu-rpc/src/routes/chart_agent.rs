use axum::{extract::State, Json};
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::error::{ApiError, ApiResult};

#[derive(Serialize, utoipa::ToSchema)]
pub struct ChartAgentCapabilities {
    pub protocol_version: &'static str,
    pub runtime_version: &'static str,
    pub scene_epoch: u64,
    pub capability_ids: Vec<&'static str>,
    pub supported_edit_types: Vec<&'static str>,
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::default().routes(routes!(get_capabilities))
}

#[utoipa::path(
    get,
    path = "/chart-agent/capabilities",
    responses((status = 200, body = ChartAgentCapabilities))
)]
async fn get_capabilities(State(app): State<AppState>) -> ApiResult<Json<ChartAgentCapabilities>> {
    let session = app
        .current_session()
        .ok_or_else(|| ApiError::bad_request("no project open"))?;

    Ok(Json(ChartAgentCapabilities {
        protocol_version: "koharu-chart-agent.v1",
        runtime_version: env!("CARGO_PKG_VERSION"),
        scene_epoch: session.epoch(),
        capability_ids: vec![
            "scene.conditional_epoch",
            "scene.batch_patch",
            "idempotency.receipt",
            "pipeline.run",
            "pipeline.text_node_scope",
            "mask.upload",
        ],
        supported_edit_types: vec![
            "set_translation",
            "set_line_breaks",
            "set_font_size",
            "set_text_align",
            "set_font_families",
            "set_text_color",
            "set_rotation",
            "set_layout_box",
            "set_rendered_direction",
            "update_inpaint_mask",
            "run_local_inpaint",
        ],
    }))
}
