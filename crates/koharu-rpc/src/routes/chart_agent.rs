use axum::Json;
use axum::extract::State;
use koharu_app::ProjectSession;
use koharu_core::FontFaceInfo;
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::error::{ApiError, ApiResult};

const SUPPORTED_EDIT_TYPES: &[&str] = &[
    "set_font_families",
    "set_font_size",
    "set_layout_box",
    "set_line_breaks",
    "set_rendered_direction",
    "set_rotation",
    "set_text_align",
    "set_text_color",
    "set_translation",
];

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChartAgentCapabilitiesV1 {
    schema_version: &'static str,
    capability_protocol_version: &'static str,
    runtime_version: String,
    scene_epoch: u64,
    supported_edit_types: Vec<&'static str>,
    renderer: RendererCapabilitiesV1,
    inpaint: InpaintCapabilitiesV1,
    font_inventory: FontInventoryBindingV1,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct RendererCapabilitiesV1 {
    explicit_rendered_direction: bool,
    explicit_text_style: bool,
    immutable_source_geometry: bool,
    supported_alignments: Vec<&'static str>,
    supported_directions: Vec<&'static str>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct InpaintCapabilitiesV1 {
    available: bool,
    engine_ids: Vec<String>,
    optimizer_mask_edits: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct FontInventoryBindingV1 {
    inventory_version: &'static str,
    inventory_hash: String,
    family_names: Vec<String>,
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::default().routes(routes!(get_chart_agent_capabilities))
}

#[utoipa::path(
    get,
    path = "/chart-agent/capabilities",
    responses((status = 200, body = ChartAgentCapabilitiesV1))
)]
async fn get_chart_agent_capabilities(
    State(app): State<AppState>,
) -> ApiResult<Json<ChartAgentCapabilitiesV1>> {
    let session = app
        .current_session()
        .ok_or_else(|| ApiError::bad_request("no project open"))?;
    let fonts = app.renderer.available_fonts().map_err(ApiError::internal)?;
    Ok(Json(capabilities_for(app.version, &session, fonts)))
}

fn capabilities_for(
    runtime_version: &str,
    session: &ProjectSession,
    mut fonts: Vec<FontFaceInfo>,
) -> ChartAgentCapabilitiesV1 {
    fonts.sort();
    let mut family_names = fonts
        .iter()
        .map(|font| font.family_name.clone())
        .collect::<Vec<_>>();
    family_names.sort();
    family_names.dedup();
    let font_bytes = serde_json::to_vec(&fonts).expect("font inventory is serializable");

    let mut engine_ids = koharu_app::pipeline::catalog()
        .inpainters
        .into_iter()
        .map(|engine| engine.id)
        .collect::<Vec<_>>();
    engine_ids.sort();
    engine_ids.dedup();

    ChartAgentCapabilitiesV1 {
        schema_version: "koharu-chart-agent-capabilities.v1",
        capability_protocol_version: "gateway-capability-handshake.v1",
        runtime_version: runtime_version.to_owned(),
        scene_epoch: session.epoch(),
        supported_edit_types: SUPPORTED_EDIT_TYPES.to_vec(),
        renderer: RendererCapabilitiesV1 {
            explicit_rendered_direction: true,
            explicit_text_style: true,
            immutable_source_geometry: true,
            supported_alignments: vec!["center", "left", "right"],
            supported_directions: vec!["horizontal", "vertical"],
        },
        inpaint: InpaintCapabilitiesV1 {
            available: !engine_ids.is_empty(),
            engine_ids,
            optimizer_mask_edits: false,
        },
        font_inventory: FontInventoryBindingV1 {
            inventory_version: "koharu-font-inventory.v1",
            inventory_hash: format!("blake3:{}", blake3::hash(&font_bytes).to_hex()),
            family_names,
        },
    }
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use koharu_app::ProjectSession;
    use serde_json::json;

    use super::capabilities_for;

    #[test]
    fn capabilities_are_read_only_deterministic_and_epoch_bound() {
        let temp = tempfile::tempdir().unwrap();
        let session = ProjectSession::create(
            Utf8PathBuf::from_path_buf(temp.path().join("capabilities.khrproj")).unwrap(),
            "capabilities",
        )
        .unwrap();

        let body =
            serde_json::to_value(capabilities_for("test-runtime", &session, Vec::new())).unwrap();

        assert_eq!(body["schemaVersion"], "koharu-chart-agent-capabilities.v1");
        assert_eq!(body["sceneEpoch"], session.epoch());
        assert!(body["sceneEpoch"].is_u64());
        assert!(
            body["supportedEditTypes"]
                .as_array()
                .unwrap()
                .contains(&json!("set_translation"))
        );
        assert!(
            !body["supportedEditTypes"]
                .as_array()
                .unwrap()
                .contains(&json!("update_inpaint_mask"))
        );
    }
}
