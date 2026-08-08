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
        capability_ids: vec!["scene.batch_patch"],
    }))
}
