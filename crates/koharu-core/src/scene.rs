//! Scene graph: Project → Pages → flat Nodes.
//!
//! Three primitives: `Node`, `Blob` (via `BlobRef`), `Op` (in `op.rs`).
//! Everything visual on a page is a `Node`; scene mutations flow through `Op`s.

// `NodeKind::Text` naturally carries more data than `Image`/`Mask`, and
// boxing would change the wire format. Same reasoning as in `op.rs`.
#![allow(clippy::large_enum_variant)]

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::blob::BlobRef;
use crate::font::{FontPrediction, TextDirection};
use crate::style::TextStyle;

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

#[derive(
    Copy,
    Clone,
    Debug,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    Deserialize,
    JsonSchema,
    ToSchema,
)]
#[serde(transparent)]
pub struct PageId(pub Uuid);

#[derive(
    Copy,
    Clone,
    Debug,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    Deserialize,
    JsonSchema,
    ToSchema,
)]
#[serde(transparent)]
pub struct NodeId(pub Uuid);

impl PageId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl NodeId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for PageId {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for PageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

// ---------------------------------------------------------------------------
// Scene / Project
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Scene {
    pub project: ProjectMeta,
    /// Pages in insertion order; `IndexMap` ordering *is* the page order.
    pub pages: IndexMap<PageId, Page>,
}

impl Default for Scene {
    fn default() -> Self {
        Self {
            project: ProjectMeta::default(),
            pages: IndexMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectMeta {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub style: ProjectStyle,
}

impl Default for ProjectMeta {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            name: String::new(),
            created_at: now,
            updated_at: now,
            style: ProjectStyle::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStyle {
    #[serde(default)]
    pub default_font: Option<String>,
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub id: PageId,
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Stacking = insertion order. Bottom-first: `source` is typically first,
    /// `rendered` typically last.
    pub nodes: IndexMap<NodeId, Node>,
}

impl Page {
    pub fn new(name: impl Into<String>, width: u32, height: u32) -> Self {
        Self {
            id: PageId::new(),
            name: name.into(),
            width,
            height,
            nodes: IndexMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Node
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: NodeId,
    #[serde(default)]
    pub transform: Transform,
    pub visible: bool,
    pub kind: NodeKind,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum NodeKind {
    Image(ImageData),
    Text(TextData),
    Mask(MaskData),
}

impl NodeKind {
    pub fn discriminant(&self) -> NodeKindTag {
        match self {
            NodeKind::Image(_) => NodeKindTag::Image,
            NodeKind::Text(_) => NodeKindTag::Text,
            NodeKind::Mask(_) => NodeKindTag::Mask,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NodeKindTag {
    Image,
    Text,
    Mask,
}

// ---------------------------------------------------------------------------
// Image node
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImageData {
    /// Role tags differentiate source / inpainted / rendered / user-imported images.
    /// Role is immutable on an existing node — switching roles = delete + add.
    pub role: ImageRole,
    pub blob: BlobRef,
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    pub natural_width: u32,
    pub natural_height: u32,
    #[serde(default)]
    pub name: Option<String>,
}

const fn default_opacity() -> f32 {
    1.0
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum ImageRole {
    /// Immutable page input; exactly one per page.
    Source,
    /// Pipeline output; text removed from `Source`.
    Inpainted,
    /// Pipeline output; final composite.
    Rendered,
    /// User-imported free layer, movable / selectable.
    Custom,
}

// ---------------------------------------------------------------------------
// Mask node
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MaskData {
    pub role: MaskRole,
    pub blob: BlobRef,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum MaskRole {
    /// Manual brush strokes driving local inpaint.
    BrushInpaint,
    /// Text-detector segmentation preview (text-pixel mask).
    Segment,
    /// Bubble-interior mask from `speech-bubble-segmentation`. The
    /// renderer grows text layout boxes inside this mask so English
    /// wraps into the available bubble space without leaking past the
    /// bubble border.
    Bubble,
}

// ---------------------------------------------------------------------------
// Text node
// ---------------------------------------------------------------------------

/// Version written into every immutable OCR geometry payload.
pub const SOURCE_GEOMETRY_EVIDENCE_VERSION: &str = "source-geometry-evidence.v1";

/// Confidence attached to one line polygon.  Text recognition confidence and
/// polygon confidence are intentionally separate: many OCR services expose
/// only the former.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LineDetectorEvidence {
    #[serde(default)]
    pub text_confidence: Option<f32>,
    #[serde(default)]
    pub polygon_confidence: Option<f32>,
}

/// Detector identity and the confidence channels that produced source
/// geometry.  Missing upstream evidence is represented by `None`, never by a
/// fabricated zero.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DetectorEvidence {
    pub detector_id: String,
    pub detector_version: String,
    /// Canonical SHA-256 of the detector configuration, including endpoint
    /// identity but excluding the image payload.
    pub config_hash: String,
    #[serde(default)]
    pub block_polygon_confidence: Option<f32>,
    #[serde(default)]
    pub line_evidence: Vec<LineDetectorEvidence>,
    #[serde(default)]
    pub direction_confidence: Option<f32>,
    #[serde(default)]
    pub rotation_confidence: Option<f32>,
}

/// Immutable OCR geometry as recorded when a text node is created.
///
/// Target layout remains on [`Transform`] and `rendered_direction`; neither
/// translation nor rendering may rewrite this evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceGeometryEvidence {
    pub schema_version: String,
    pub block_polygon: [[f32; 2]; 4],
    pub line_polygons: Vec<[[f32; 2]; 4]>,
    pub source_direction: TextDirection,
    pub source_direction_source: String,
    pub source_rotation_deg: f32,
    pub detector_evidence: DetectorEvidence,
}

impl SourceGeometryEvidence {
    /// Validate geometry against its source-image canvas without reordering or
    /// normalising detector points.
    pub fn validate(&self, width: u32, height: u32) -> Result<(), String> {
        if self.schema_version != SOURCE_GEOMETRY_EVIDENCE_VERSION {
            return Err("unsupported source geometry evidence version".into());
        }
        validate_quad(&self.block_polygon, width, height, "block polygon")?;
        if self.line_polygons.is_empty() {
            return Err("line polygons must not be empty".into());
        }
        for (index, polygon) in self.line_polygons.iter().enumerate() {
            validate_quad(polygon, width, height, &format!("line polygon {index}"))?;
        }
        if self.detector_evidence.line_evidence.len() != self.line_polygons.len() {
            return Err("line evidence count differs from line polygon count".into());
        }
        if self.source_direction_source.trim().is_empty() {
            return Err("source direction source must not be empty".into());
        }
        if !self.source_rotation_deg.is_finite()
            || !(-180.0..=180.0).contains(&self.source_rotation_deg)
        {
            return Err("source rotation must be finite and within [-180, 180]".into());
        }
        let detector = &self.detector_evidence;
        if detector.detector_id.trim().is_empty() || detector.detector_version.trim().is_empty() {
            return Err("detector id and version must not be empty".into());
        }
        if !is_sha256(&detector.config_hash) {
            return Err("detector config hash must be lowercase sha256".into());
        }
        validate_confidence(
            detector.block_polygon_confidence,
            "block polygon confidence",
        )?;
        validate_confidence(detector.direction_confidence, "direction confidence")?;
        validate_confidence(detector.rotation_confidence, "rotation confidence")?;
        for (index, line) in detector.line_evidence.iter().enumerate() {
            validate_confidence(
                line.text_confidence,
                &format!("line {index} text confidence"),
            )?;
            validate_confidence(
                line.polygon_confidence,
                &format!("line {index} polygon confidence"),
            )?;
        }
        Ok(())
    }
}

fn validate_confidence(value: Option<f32>, label: &str) -> Result<(), String> {
    if value.is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value)) {
        return Err(format!("{label} must be within [0, 1]"));
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn validate_quad(
    polygon: &[[f32; 2]; 4],
    width: u32,
    height: u32,
    label: &str,
) -> Result<(), String> {
    for point in polygon {
        if !point[0].is_finite() || !point[1].is_finite() {
            return Err(format!("{label} contains a non-finite coordinate"));
        }
        if point[0] < 0.0 || point[1] < 0.0 || point[0] > width as f32 || point[1] > height as f32 {
            return Err(format!("{label} lies outside the source canvas"));
        }
    }
    for index in 0..4 {
        let a = polygon[index];
        let b = polygon[(index + 1) % 4];
        if (a[0] - b[0]).abs() <= f32::EPSILON && (a[1] - b[1]).abs() <= f32::EPSILON {
            return Err(format!("{label} contains a zero-length edge"));
        }
    }
    let signed_area = (0..4)
        .map(|index| {
            polygon[index][0] * polygon[(index + 1) % 4][1]
                - polygon[(index + 1) % 4][0] * polygon[index][1]
        })
        .sum::<f32>()
        * 0.5;
    if signed_area.abs() <= 1e-3 {
        return Err(format!("{label} has zero area"));
    }
    if segments_intersect(polygon[0], polygon[1], polygon[2], polygon[3])
        || segments_intersect(polygon[1], polygon[2], polygon[3], polygon[0])
    {
        return Err(format!("{label} is self-intersecting"));
    }
    Ok(())
}

fn segments_intersect(a: [f32; 2], b: [f32; 2], c: [f32; 2], d: [f32; 2]) -> bool {
    fn cross(a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> f32 {
        (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
    }
    let ab_c = cross(a, b, c);
    let ab_d = cross(a, b, d);
    let cd_a = cross(c, d, a);
    let cd_b = cross(c, d, b);
    ab_c * ab_d <= 0.0 && cd_a * cd_b <= 0.0
}

#[derive(Clone, Debug, Default, Serialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextData {
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub source_lang: Option<String>,
    #[serde(default)]
    pub source_direction: Option<TextDirection>,
    #[serde(default)]
    pub rendered_direction: Option<TextDirection>,
    #[serde(default)]
    pub line_polygons: Option<Vec<[[f32; 2]; 4]>>,
    #[serde(default)]
    pub rotation_deg: Option<f32>,
    #[serde(default)]
    pub detected_font_size_px: Option<f32>,
    #[serde(default)]
    pub detector: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub translation: Option<String>,
    #[serde(default)]
    pub style: Option<TextStyle>,
    #[serde(default)]
    pub font_prediction: Option<FontPrediction>,
    /// Renderer-produced sprite for this block.
    #[serde(default)]
    pub sprite: Option<BlobRef>,
    /// Sprite placement when the renderer expands past the bubble geometry.
    #[serde(default)]
    pub sprite_transform: Option<Transform>,
    #[serde(default)]
    pub lock_layout_box: bool,
    /// Authoritative, immutable source evidence.  Kept at the end of the
    /// postcard struct so existing project snapshots remain readable.
    #[serde(default)]
    pub source_geometry: Option<SourceGeometryEvidence>,
}

impl TextData {
    pub fn source_line_polygons(&self) -> Option<&[[[f32; 2]; 4]]> {
        self.source_geometry
            .as_ref()
            .map(|geometry| geometry.line_polygons.as_slice())
            .or_else(|| self.line_polygons.as_deref())
    }

    pub fn recorded_source_direction(&self) -> Option<TextDirection> {
        self.source_geometry
            .as_ref()
            .map(|geometry| geometry.source_direction)
            .or(self.source_direction)
    }

    pub fn recorded_source_rotation_deg(&self) -> Option<f32> {
        self.source_geometry
            .as_ref()
            .map(|geometry| geometry.source_rotation_deg)
            .or(self.rotation_deg)
    }

    pub fn recorded_detector_id(&self) -> Option<&str> {
        self.source_geometry
            .as_ref()
            .map(|geometry| geometry.detector_evidence.detector_id.as_str())
            .or(self.detector.as_deref())
    }
}

/// Map-form helper used by the custom deserializer below.  Postcard encodes
/// structs as sequences, while JSON/OpenAPI clients use maps.
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TextDataMap {
    confidence: f32,
    source_lang: Option<String>,
    source_direction: Option<TextDirection>,
    rendered_direction: Option<TextDirection>,
    line_polygons: Option<Vec<[[f32; 2]; 4]>>,
    rotation_deg: Option<f32>,
    detected_font_size_px: Option<f32>,
    detector: Option<String>,
    text: Option<String>,
    translation: Option<String>,
    style: Option<TextStyle>,
    font_prediction: Option<FontPrediction>,
    sprite: Option<BlobRef>,
    sprite_transform: Option<Transform>,
    lock_layout_box: bool,
    source_geometry: Option<SourceGeometryEvidence>,
}

impl From<TextDataMap> for TextData {
    fn from(value: TextDataMap) -> Self {
        Self {
            confidence: value.confidence,
            source_lang: value.source_lang,
            source_direction: value.source_direction,
            rendered_direction: value.rendered_direction,
            line_polygons: value.line_polygons,
            rotation_deg: value.rotation_deg,
            detected_font_size_px: value.detected_font_size_px,
            detector: value.detector,
            text: value.text,
            translation: value.translation,
            style: value.style,
            font_prediction: value.font_prediction,
            sprite: value.sprite,
            sprite_transform: value.sprite_transform,
            lock_layout_box: value.lock_layout_box,
            source_geometry: value.source_geometry,
        }
    }
}

impl<'de> Deserialize<'de> for TextData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const FIELDS: &[&str] = &[
            "confidence",
            "sourceLang",
            "sourceDirection",
            "renderedDirection",
            "linePolygons",
            "rotationDeg",
            "detectedFontSizePx",
            "detector",
            "text",
            "translation",
            "style",
            "fontPrediction",
            "sprite",
            "spriteTransform",
            "lockLayoutBox",
            "sourceGeometry",
        ];

        struct TextDataVisitor;

        impl<'de> serde::de::Visitor<'de> for TextDataVisitor {
            type Value = TextData;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("TextData map or postcard sequence")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let confidence = sequence.next_element()?.unwrap_or_default();
                let source_lang = sequence.next_element()?.unwrap_or_default();
                let source_direction = sequence.next_element()?.unwrap_or_default();
                let rendered_direction = sequence.next_element()?.unwrap_or_default();
                let line_polygons = sequence.next_element()?.unwrap_or_default();
                let rotation_deg = sequence.next_element()?.unwrap_or_default();
                let detected_font_size_px = sequence.next_element()?.unwrap_or_default();
                let detector = sequence.next_element()?.unwrap_or_default();
                let text = sequence.next_element()?.unwrap_or_default();
                let translation = sequence.next_element()?.unwrap_or_default();
                let style = sequence.next_element()?.unwrap_or_default();
                let font_prediction = sequence.next_element()?.unwrap_or_default();
                let sprite = sequence.next_element()?.unwrap_or_default();
                let sprite_transform = sequence.next_element()?.unwrap_or_default();
                let lock_layout_box = sequence.next_element()?.unwrap_or_default();
                let source_geometry = match sequence.next_element() {
                    Ok(value) => value.unwrap_or_default(),
                    Err(error) if format!("{error:?}").contains("DeserializeUnexpectedEnd") => None,
                    Err(error) => return Err(error),
                };
                Ok(TextData {
                    confidence,
                    source_lang,
                    source_direction,
                    rendered_direction,
                    line_polygons,
                    rotation_deg,
                    detected_font_size_px,
                    detector,
                    text,
                    translation,
                    style,
                    font_prediction,
                    sprite,
                    sprite_transform,
                    lock_layout_box,
                    source_geometry,
                })
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let value =
                    TextDataMap::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(value.into())
            }
        }

        deserializer.deserialize_struct("TextData", FIELDS, TextDataVisitor)
    }
}

// ---------------------------------------------------------------------------
// Transform
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Transform {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub rotation_deg: f32,
}

// ---------------------------------------------------------------------------
// Scene convenience helpers
// ---------------------------------------------------------------------------

impl Scene {
    pub fn page(&self, id: PageId) -> Option<&Page> {
        self.pages.get(&id)
    }

    pub fn page_mut(&mut self, id: PageId) -> Option<&mut Page> {
        self.pages.get_mut(&id)
    }

    pub fn node(&self, page: PageId, node: NodeId) -> Option<&Node> {
        self.page(page)?.nodes.get(&node)
    }

    pub fn node_mut(&mut self, page: PageId, node: NodeId) -> Option<&mut Node> {
        self.page_mut(page)?.nodes.get_mut(&node)
    }
}

impl Page {
    pub fn source_node(&self) -> Option<(&NodeId, &Node)> {
        self.nodes.iter().find(|(_, node)| {
            matches!(
                &node.kind,
                NodeKind::Image(img) if img.role == ImageRole::Source
            )
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Exact pre-sourceGeometry field order.  This guards compatibility with
    /// existing postcard snapshots instead of assuming JSON-style defaults.
    #[derive(Serialize)]
    struct LegacyTextData {
        confidence: f32,
        source_lang: Option<String>,
        source_direction: Option<TextDirection>,
        rendered_direction: Option<TextDirection>,
        line_polygons: Option<Vec<[[f32; 2]; 4]>>,
        rotation_deg: Option<f32>,
        detected_font_size_px: Option<f32>,
        detector: Option<String>,
        text: Option<String>,
        translation: Option<String>,
        style: Option<TextStyle>,
        font_prediction: Option<FontPrediction>,
        sprite: Option<BlobRef>,
        sprite_transform: Option<Transform>,
        lock_layout_box: bool,
    }

    #[test]
    fn bare_datetime_postcard_round_trips() {
        let now: DateTime<Utc> = Utc::now();
        let bytes = postcard::to_allocvec(&now).expect("serialize");
        let decoded: DateTime<Utc> = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(decoded.timestamp(), now.timestamp());
    }

    #[test]
    fn project_style_postcard_round_trips() {
        let style = ProjectStyle::default();
        let bytes = postcard::to_allocvec(&style).expect("serialize");
        let _: ProjectStyle = postcard::from_bytes(&bytes).expect("deserialize");
    }

    #[test]
    fn legacy_text_data_postcard_decodes_without_source_geometry() {
        let legacy = LegacyTextData {
            confidence: 0.75,
            source_lang: Some("en".into()),
            source_direction: Some(TextDirection::Horizontal),
            rendered_direction: None,
            line_polygons: Some(vec![[[1.0, 2.0], [20.0, 2.0], [20.0, 10.0], [1.0, 10.0]]]),
            rotation_deg: Some(0.0),
            detected_font_size_px: Some(12.0),
            detector: Some("legacy".into()),
            text: Some("source".into()),
            translation: None,
            style: None,
            font_prediction: None,
            sprite: None,
            sprite_transform: None,
            lock_layout_box: true,
        };
        let bytes = postcard::to_allocvec(&legacy).expect("serialize legacy TextData");
        let decoded: TextData = postcard::from_bytes(&bytes).expect("decode current TextData");

        assert_eq!(decoded.detector.as_deref(), Some("legacy"));
        assert_eq!(decoded.line_polygons, legacy.line_polygons);
        assert!(decoded.source_geometry.is_none());
    }

    #[test]
    fn project_meta_postcard_round_trips() {
        let meta = ProjectMeta::default();
        let bytes = postcard::to_allocvec(&meta).expect("serialize");
        let decoded: ProjectMeta = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(decoded.name, meta.name);
    }

    #[test]
    fn empty_scene_postcard_round_trips() {
        let scene = Scene::default();
        let bytes = postcard::to_allocvec(&scene).expect("serialize");
        let decoded: Scene = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(decoded.pages.len(), 0);
    }

    #[test]
    fn scene_with_one_page_postcard_round_trips() {
        let mut scene = Scene::default();
        scene.project.name = "hello".into();
        let page = Page::new("p1", 800, 600);
        let page_id = page.id;
        scene.pages.insert(page_id, page);
        let bytes = postcard::to_allocvec(&scene).expect("serialize");
        let decoded: Scene = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(decoded.pages.len(), 1);
        assert_eq!(decoded.project.name, "hello");
        assert!(decoded.pages.contains_key(&page_id));
    }
}
