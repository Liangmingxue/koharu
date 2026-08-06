//! PaddleOCR-VL spotting through a PaddleX serving endpoint.

use std::io::Cursor;

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use base64::Engine as _;
use image::ImageFormat;
use koharu_core::{
    DetectorEvidence, LineDetectorEvidence, Op, SOURCE_GEOMETRY_EVIDENCE_VERSION,
    SourceGeometryEvidence, TextData, TextDirection,
};
use koharu_runtime::RuntimeHttpClient;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::pipeline::artifacts::Artifact;
use crate::pipeline::engine::{Engine, EngineCtx, EngineInfo};
use crate::pipeline::engines::support::{
    clear_text_nodes_ops, load_source_image, new_text_node, page_node_count,
    sort_manga_reading_order,
};

const ENGINE_ID: &str = "paddleocr-vl-spotting-api";
const ENGINE_VERSION: &str = "paddleocr-vl-spotting-api.v1";
const DIRECTION_SOURCE: &str = "koharu.polygon-long-axis.v1";
const ENDPOINT_ENV: &str = "PADDLEOCR_VL_API_URL";
const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8080/layout-parsing";

pub struct Model {
    http: RuntimeHttpClient,
    endpoint: String,
    config_hash: String,
}

impl Model {
    fn new(http: RuntimeHttpClient) -> Self {
        let endpoint = std::env::var(ENDPOINT_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
        let config_hash = detector_config_hash(&endpoint);
        Self {
            http,
            endpoint,
            config_hash,
        }
    }
}

#[async_trait]
impl Engine for Model {
    async fn run(&self, ctx: EngineCtx<'_>) -> Result<Vec<Op>> {
        let image = load_source_image(ctx.scene, ctx.page, ctx.blobs)?;
        let (source_width, source_height) = (image.width(), image.height());
        let mut png = Cursor::new(Vec::new());
        image
            .write_to(&mut png, ImageFormat::Png)
            .context("failed to encode source image as PNG")?;
        let file = base64::engine::general_purpose::STANDARD.encode(png.into_inner());

        let response = self
            .http
            .post(&self.endpoint)
            .json(&SpottingRequest::new(&file))
            .send()
            .await
            .with_context(|| format!("failed to call PaddleOCR-VL at {}", self.endpoint))?;
        let status = response.status();
        ensure!(
            status.is_success(),
            "PaddleOCR-VL returned HTTP status {status}"
        );
        let response: ApiResponse = response
            .json()
            .await
            .context("failed to decode PaddleOCR-VL response")?;
        let mut pairs = response.into_text_pairs(source_width, source_height, &self.config_hash)?;
        sort_manga_reading_order(&mut pairs, ctx.options.reading_order.unwrap_or_default());

        let mut ops = clear_text_nodes_ops(ctx.scene, ctx.page);
        let removed = ops.len();
        let insertion_start = page_node_count(ctx.scene, ctx.page).saturating_sub(removed);
        ops.reserve(pairs.len());
        for (at, (bbox, text)) in (insertion_start..).zip(pairs) {
            ops.push(Op::AddNode {
                page: ctx.page,
                node: new_text_node(bbox, text),
                at,
            });
        }
        Ok(ops)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SpottingRequest<'a> {
    file: &'a str,
    file_type: u8,
    use_layout_detection: bool,
    prompt_label: &'static str,
    format_block_content: bool,
    visualize: bool,
}

impl<'a> SpottingRequest<'a> {
    fn new(file: &'a str) -> Self {
        Self {
            file,
            file_type: 1,
            use_layout_detection: false,
            prompt_label: "spotting",
            format_block_content: false,
            visualize: false,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DetectorConfig<'a> {
    endpoint: &'a str,
    file_type: u8,
    use_layout_detection: bool,
    prompt_label: &'static str,
    format_block_content: bool,
    visualize: bool,
}

fn detector_config_hash(endpoint: &str) -> String {
    let config = DetectorConfig {
        endpoint,
        file_type: 1,
        use_layout_detection: false,
        prompt_label: "spotting",
        format_block_content: false,
        visualize: false,
    };
    let bytes = serde_json::to_vec(&config).expect("detector config is serializable");
    let digest = Sha256::digest(bytes);
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiResponse {
    error_code: i64,
    #[serde(default)]
    error_msg: String,
    result: Option<ApiResult>,
}

impl ApiResponse {
    fn into_text_pairs(
        self,
        source_width: u32,
        source_height: u32,
        config_hash: &str,
    ) -> Result<Vec<([f32; 4], TextData)>> {
        if self.error_code != 0 {
            bail!("PaddleOCR-VL error {}: {}", self.error_code, self.error_msg);
        }
        let result = self.result.context("PaddleOCR-VL response has no result")?;
        let page = result
            .layout_parsing_results
            .into_iter()
            .next()
            .context("PaddleOCR-VL response has no page result")?;
        let spotting = page
            .pruned_result
            .spotting_res
            .context("PaddleOCR-VL response has no spotting result")?;
        spotting.into_text_pairs(source_width, source_height, config_hash)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiResult {
    layout_parsing_results: Vec<LayoutParsingResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LayoutParsingResult {
    pruned_result: PrunedResult,
}

#[derive(Deserialize)]
struct PrunedResult {
    spotting_res: Option<SpottingResult>,
}

#[derive(Deserialize)]
struct SpottingResult {
    rec_polys: Vec<Vec<[f32; 2]>>,
    rec_texts: Vec<String>,
    #[serde(default)]
    rec_scores: Option<Vec<f32>>,
}

impl SpottingResult {
    fn into_text_pairs(
        self,
        source_width: u32,
        source_height: u32,
        config_hash: &str,
    ) -> Result<Vec<([f32; 4], TextData)>> {
        ensure!(
            self.rec_polys.len() == self.rec_texts.len(),
            "PaddleOCR-VL returned {} polygons but {} texts",
            self.rec_polys.len(),
            self.rec_texts.len()
        );

        let scores = match self.rec_scores {
            Some(scores) => {
                ensure!(
                    scores.len() == self.rec_texts.len(),
                    "PaddleOCR-VL returned {} scores but {} texts",
                    scores.len(),
                    self.rec_texts.len()
                );
                scores.into_iter().map(Some).collect::<Vec<_>>()
            }
            None => vec![None; self.rec_texts.len()],
        };

        self.rec_polys
            .into_iter()
            .zip(self.rec_texts)
            .zip(scores)
            .enumerate()
            .filter(|(_, ((_, text), _))| !text.trim().is_empty())
            .map(|(index, ((points, text), text_confidence))| {
                let polygon: [[f32; 2]; 4] = points.try_into().map_err(|points: Vec<_>| {
                    anyhow::anyhow!(
                        "PaddleOCR-VL polygon {index} has {} points; expected 4",
                        points.len()
                    )
                })?;
                ensure!(
                    polygon.iter().flatten().all(|value| value.is_finite()),
                    "PaddleOCR-VL polygon {index} contains a non-finite coordinate"
                );
                let pose = text_pose(&polygon, &text);
                ensure!(
                    pose.bbox[2] > pose.bbox[0] && pose.bbox[3] > pose.bbox[1],
                    "PaddleOCR-VL polygon {index} has an empty bounding box"
                );
                if let Some(confidence) = text_confidence {
                    ensure!(
                        confidence.is_finite() && (0.0..=1.0).contains(&confidence),
                        "PaddleOCR-VL text confidence {index} is outside [0,1]"
                    );
                }
                let source_geometry = SourceGeometryEvidence {
                    schema_version: SOURCE_GEOMETRY_EVIDENCE_VERSION.to_string(),
                    block_polygon: polygon,
                    line_polygons: vec![polygon],
                    source_direction: pose.direction,
                    source_direction_source: DIRECTION_SOURCE.to_string(),
                    source_rotation_deg: pose.rotation_deg,
                    detector_evidence: DetectorEvidence {
                        detector_id: ENGINE_ID.to_string(),
                        detector_version: ENGINE_VERSION.to_string(),
                        config_hash: config_hash.to_string(),
                        block_polygon_confidence: None,
                        line_evidence: vec![LineDetectorEvidence {
                            text_confidence,
                            polygon_confidence: None,
                        }],
                        direction_confidence: None,
                        rotation_confidence: None,
                    },
                };
                source_geometry
                    .validate(source_width, source_height)
                    .map_err(|message| {
                        anyhow::anyhow!("PaddleOCR-VL polygon {index}: {message}")
                    })?;
                Ok((
                    pose.bbox,
                    TextData {
                        source_direction: Some(pose.direction),
                        line_polygons: Some(vec![polygon]),
                        rotation_deg: Some(pose.rotation_deg),
                        detector: Some(ENGINE_ID.to_string()),
                        text: Some(text),
                        lock_layout_box: true,
                        source_geometry: Some(source_geometry),
                        ..Default::default()
                    },
                ))
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy)]
struct TextPose {
    bbox: [f32; 4],
    direction: TextDirection,
    rotation_deg: f32,
}

/// Separate the glyph writing direction from the rotation of the whole line.
/// Direction follows the polygon's long axis and therefore remains stable
/// when OCR text is replaced by a translation. The local bbox stays unrotated
/// so layout and rotation remain independent.
fn text_pose(polygon: &[[f32; 2]; 4], _text: &str) -> TextPose {
    let center = polygon.iter().fold([0.0, 0.0], |[x, y], point| {
        [x + point[0] * 0.25, y + point[1] * 0.25]
    });
    let top = edge(polygon[0], polygon[1]);
    let side = edge(polygon[1], polygon[2]);
    let (axis, cross) = if side.length > top.length {
        (side, top)
    } else {
        (top, side)
    };
    let axis_angle = normalize_axis_degrees(axis.angle_deg);
    let (width, height, direction, rotation_deg) = if axis_angle.abs() > 45.0 {
        let downward_angle = if axis_angle < 0.0 {
            axis_angle + 180.0
        } else {
            axis_angle
        };
        (
            cross.length,
            axis.length,
            TextDirection::Vertical,
            normalize_degrees(downward_angle - 90.0),
        )
    } else {
        (
            axis.length,
            cross.length,
            TextDirection::Horizontal,
            axis_angle,
        )
    };
    let half_w = width * 0.5;
    let half_h = height * 0.5;
    TextPose {
        bbox: [
            center[0] - half_w,
            center[1] - half_h,
            center[0] + half_w,
            center[1] + half_h,
        ],
        direction,
        rotation_deg,
    }
}

#[derive(Debug, Clone, Copy)]
struct Edge {
    length: f32,
    angle_deg: f32,
}

fn edge(from: [f32; 2], to: [f32; 2]) -> Edge {
    let dx = to[0] - from[0];
    let dy = to[1] - from[1];
    Edge {
        length: dx.hypot(dy),
        angle_deg: dy.atan2(dx).to_degrees(),
    }
}

fn normalize_degrees(angle: f32) -> f32 {
    let normalized = (angle + 180.0).rem_euclid(360.0) - 180.0;
    if normalized.abs() < 0.01 {
        0.0
    } else {
        normalized
    }
}

fn normalize_axis_degrees(angle: f32) -> f32 {
    let normalized = normalize_degrees(angle);
    if normalized > 90.0 {
        normalized - 180.0
    } else if normalized <= -90.0 {
        normalized + 180.0
    } else {
        normalized
    }
}

inventory::submit! {
    EngineInfo {
        id: ENGINE_ID,
        name: "PaddleOCR-VL Spotting API",
        needs: &[],
        produces: &[Artifact::TextBoxes, Artifact::OcrText],
        load: |runtime, _cpu| Box::pin(async move {
            Ok(Box::new(Model::new(runtime.http_client())) as Box<dyn Engine>)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn into_pairs(response: ApiResponse) -> Result<Vec<([f32; 4], TextData)>> {
        response.into_text_pairs(100, 100, &detector_config_hash(DEFAULT_ENDPOINT))
    }

    fn response(polys: &str, texts: &str) -> ApiResponse {
        serde_json::from_str(&format!(
            r#"{{
                "errorCode": 0,
                "errorMsg": "Success",
                "result": {{
                    "layoutParsingResults": [{{
                        "prunedResult": {{
                            "spotting_res": {{
                                "rec_polys": {polys},
                                "rec_texts": {texts}
                            }}
                        }}
                    }}]
                }}
            }}"#
        ))
        .unwrap()
    }

    #[test]
    fn converts_spotting_results_to_independent_text_nodes() {
        let pairs = response(
            r#"[[[10,20],[40,20],[40,30],[10,30]],[[5,50],[15,50],[15,80],[5,80]]]"#,
            r#"["Jun-23","Rate"]"#,
        );
        let pairs = into_pairs(pairs).unwrap();

        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, [10.0, 20.0, 40.0, 30.0]);
        assert_eq!(pairs[0].1.text.as_deref(), Some("Jun-23"));
        assert_eq!(pairs[0].1.source_direction, Some(TextDirection::Horizontal));
        assert_eq!(pairs[1].1.source_direction, Some(TextDirection::Vertical));
        assert_eq!(pairs[1].1.rotation_deg, Some(0.0));
        assert!(pairs.iter().all(|(_, text)| text.lock_layout_box));
        let source = pairs[0].1.source_geometry.as_ref().unwrap();
        assert_eq!(
            source.block_polygon,
            [[10.0, 20.0], [40.0, 20.0], [40.0, 30.0], [10.0, 30.0]]
        );
        assert_eq!(source.line_polygons, vec![source.block_polygon]);
        assert_eq!(source.source_direction_source, DIRECTION_SOURCE);
        assert_eq!(source.detector_evidence.detector_id, ENGINE_ID);
        assert_eq!(source.detector_evidence.detector_version, ENGINE_VERSION);
        assert_eq!(
            source.detector_evidence.line_evidence[0].text_confidence,
            None
        );
    }

    #[test]
    fn keeps_a_slanted_column_top_to_bottom() {
        let polygon = [[30.0, 10.0], [40.0, 12.0], [25.0, 82.0], [15.0, 80.0]];

        let pose = text_pose(&polygon, "Aug-15");

        assert_eq!(pose.direction, TextDirection::Vertical);
        assert!((pose.rotation_deg - 12.1).abs() < 0.2);
        assert!((pose.bbox[2] - pose.bbox[0] - 10.2).abs() < 0.2);
        assert!((pose.bbox[3] - pose.bbox[1] - 71.6).abs() < 0.2);
    }

    #[test]
    fn keeps_a_wide_slanted_line_left_to_right() {
        let polygon = [[10.0, 10.0], [80.0, 50.0], [75.0, 59.0], [5.0, 19.0]];

        let pose = text_pose(&polygon, "Aug-15");

        assert_eq!(pose.direction, TextDirection::Horizontal);
        assert!((pose.rotation_deg - 29.7).abs() < 0.2);
        assert!(pose.bbox[2] - pose.bbox[0] > pose.bbox[3] - pose.bbox[1]);
    }

    #[test]
    fn classifies_a_steep_long_top_edge_as_vertical() {
        let polygon = [[10.0, 10.0], [50.0, 80.0], [42.0, 84.0], [2.0, 14.0]];

        let pose = text_pose(&polygon, "label");

        assert_eq!(pose.direction, TextDirection::Vertical);
        assert!((pose.rotation_deg + 29.7).abs() < 0.2);
        assert!(pose.bbox[3] - pose.bbox[1] > pose.bbox[2] - pose.bbox[0]);
    }

    #[test]
    fn rejects_mismatched_polygon_and_text_counts() {
        let error = into_pairs(response(
            r#"[[[10,20],[40,20],[40,30],[10,30]]]"#,
            r#"["one","two"]"#,
        ))
        .unwrap_err();

        assert!(error.to_string().contains("1 polygons but 2 texts"));
    }

    #[test]
    fn rejects_non_quad_polygon() {
        let error =
            into_pairs(response(r#"[[[10,20],[40,20],[40,30]]]"#, r#"["text"]"#)).unwrap_err();

        assert!(error.to_string().contains("has 3 points; expected 4"));
    }

    #[test]
    fn propagates_api_error() {
        let response: ApiResponse = serde_json::from_str(
            r#"{"errorCode": 17, "errorMsg": "model unavailable", "result": null}"#,
        )
        .unwrap();

        let error = into_pairs(response).unwrap_err();
        assert!(error.to_string().contains("error 17: model unavailable"));
    }

    #[test]
    fn preserves_recognition_confidence_without_fabricating_geometry_confidence() {
        let response: ApiResponse = serde_json::from_str(
            r#"{
                "errorCode": 0,
                "errorMsg": "Success",
                "result": {"layoutParsingResults": [{"prunedResult": {"spotting_res": {
                    "rec_polys": [[[10,20],[40,20],[40,30],[10,30]]],
                    "rec_texts": ["Jun-23"],
                    "rec_scores": [0.875]
                }}}]}
            }"#,
        )
        .unwrap();

        let pairs = into_pairs(response).unwrap();
        let detector = &pairs[0]
            .1
            .source_geometry
            .as_ref()
            .unwrap()
            .detector_evidence;
        assert_eq!(detector.line_evidence[0].text_confidence, Some(0.875));
        assert_eq!(detector.line_evidence[0].polygon_confidence, None);
        assert_eq!(detector.block_polygon_confidence, None);
        assert_eq!(detector.direction_confidence, None);
        assert_eq!(detector.rotation_confidence, None);
    }
}
