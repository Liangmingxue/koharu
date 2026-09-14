//! Lama Manga inpainter. Reads source + segmentation mask from the page,
//! runs the model, writes the output as `Image { role: Inpainted }`.
//!
//! Box subdivision (the "which regions to run the model on" question) is
//! driven by the **mask itself** via `boxes_from_mask` — mirrors IOPaint's
//! `InpaintModel.__call__`. Text detections are no longer consulted; the
//! segmentation mask already encodes which pixels to remove.
//!
//! When `ctx.options.region` is set (repair-brush re-inpaint), we composite
//! onto the existing `Image { Inpainted }` if present (falling back to
//! `Source`) and zero out mask pixels outside the region before dispatch —
//! so only that region is reprocessed.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use image::{DynamicImage, GrayImage, Luma};
use koharu_core::{ImageRole, MaskRole, NodeId, Op, Region, TextData, Transform};
use koharu_ml::inpainting::{
    expand_mask_for_inpainting, mask::expand_mask_to_bubble_region_for_inpainting,
};
use koharu_ml::lama::Lama;
use koharu_ml::types::TextRegion;

use crate::pipeline::artifacts::Artifact;
use crate::pipeline::engine::{Engine, EngineCtx, EngineInfo};
use crate::pipeline::engines::support::{
    clip_mask_to_text_nodes, exclude_unselected_text_nodes, find_mask_node, image_dimensions,
    load_inpaint_base_image, text_node_to_region, text_nodes_in_scope, upsert_image_blob,
};

pub struct Model(Lama);
const TEXT_SCOPE_PADDING_PX: u32 = 4;

#[async_trait]
impl Engine for Model {
    async fn run(&self, ctx: EngineCtx<'_>) -> Result<Vec<Op>> {
        let is_scoped = ctx.options.text_node_ids.is_some();
        let (_, mask_ref) = find_mask_node(ctx.scene, ctx.page, MaskRole::Segment)
            .ok_or_else(|| anyhow!("no Segment mask on page"))?;
        let (_, bubble_ref) = find_mask_node(ctx.scene, ctx.page, MaskRole::Bubble)
            .ok_or_else(|| anyhow!("no Bubble mask on page"))?;
        let mut mask = ctx.blobs.load_image(&mask_ref)?;
        let mut bubble_mask = ctx.blobs.load_image(&bubble_ref)?;
        let scoped_nodes =
            text_nodes_in_scope(ctx.scene, ctx.page, ctx.options.text_node_ids.as_deref())?;
        let unselected_nodes = match ctx.options.text_node_ids.as_deref() {
            Some(selected_ids) => text_nodes_in_scope(ctx.scene, ctx.page, None)?
                .into_iter()
                .filter(|(id, _, _)| !selected_ids.contains(id))
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        if ctx.options.text_node_ids.is_some() {
            if scoped_nodes.is_empty() {
                return Ok(Vec::new());
            }
            mask = clip_mask_to_text_nodes(&mask, &scoped_nodes, TEXT_SCOPE_PADDING_PX);
            bubble_mask =
                clip_mask_to_text_nodes(&bubble_mask, &scoped_nodes, TEXT_SCOPE_PADDING_PX);
        }

        let image = load_inpaint_base_image(
            ctx.scene,
            ctx.page,
            ctx.blobs,
            ctx.options.region.is_some() || ctx.options.text_node_ids.is_some(),
        )?;
        let (mask, bubble_mask) = match ctx.options.region {
            Some(r) => {
                let clipped_mask = clip_mask_to_region(&mask, &r);
                let clipped_bubble = clip_mask_to_region(&bubble_mask, &r);
                (clipped_mask, clipped_bubble)
            }
            None => (mask, bubble_mask),
        };

        let text_blocks = scoped_nodes
            .into_iter()
            .map(|(_, transform, text)| text_node_to_region(transform, text))
            .collect::<Vec<_>>();
        let expanded = expand_text_mask(&mask, &bubble_mask, &text_blocks, is_scoped);
        let expanded = DynamicImage::ImageLuma8(expanded);
        let expanded = if ctx.options.text_node_ids.is_some() {
            let scoped_nodes =
                text_nodes_in_scope(ctx.scene, ctx.page, ctx.options.text_node_ids.as_deref())?;
            let clipped = clip_mask_to_text_nodes(&expanded, &scoped_nodes, TEXT_SCOPE_PADDING_PX);
            exclude_unselected_text_nodes(&clipped, ctx.scene, ctx.page, &scoped_nodes)
        } else {
            expanded
        };
        let expanded = exclude_text_nodes(&expanded, &unselected_nodes);
        let mask = match ctx.options.region {
            Some(r) => clip_mask_to_region(&expanded, &r),
            None => expanded,
        };

        let result = if text_blocks.is_empty() {
            self.0.inference(&image, &mask, &bubble_mask)?
        } else {
            self.0
                .inference_with_blocks(&image, &mask, &bubble_mask, &text_blocks)?
        };
        let (w, h) = image_dimensions(&result);
        let blob = ctx.blobs.put_webp(&result)?;
        Ok(vec![upsert_image_blob(
            ctx.scene,
            ctx.page,
            ImageRole::Inpainted,
            blob,
            w,
            h,
        )])
    }
}

fn exclude_text_nodes(
    mask: &DynamicImage,
    nodes: &[(NodeId, &Transform, &TextData)],
) -> DynamicImage {
    if nodes.is_empty() {
        return mask.clone();
    }
    let source = mask.to_luma8();
    let (width, height) = source.dimensions();
    let white = DynamicImage::ImageLuma8(GrayImage::from_pixel(width, height, Luma([255])));
    let protected = clip_mask_to_text_nodes(&white, nodes, 0).to_luma8();
    DynamicImage::ImageLuma8(GrayImage::from_fn(width, height, |x, y| {
        if protected.get_pixel(x, y).0[0] == 0 {
            *source.get_pixel(x, y)
        } else {
            Luma([0])
        }
    }))
}

fn expand_text_mask(
    mask: &DynamicImage,
    bubble_mask: &DynamicImage,
    text_blocks: &[TextRegion],
    is_scoped: bool,
) -> GrayImage {
    if is_scoped {
        // Chart Agent supplies an explicit block allow-list. Filling those
        // small text regions removes anti-aliased glyph remnants without
        // broadening edits to unrelated scene text.
        expand_mask_to_bubble_region_for_inpainting(mask, bubble_mask, text_blocks)
    } else {
        expand_mask_for_inpainting(mask, bubble_mask, text_blocks)
    }
}

/// Zero out every pixel of `mask` that falls outside `region`. The Crop
/// strategy's `boxes_from_mask` then only finds contours inside the region,
/// so the inpainter only touches that area.
fn clip_mask_to_region(mask: &DynamicImage, region: &Region) -> DynamicImage {
    let src = mask.to_luma8();
    let (w, h) = src.dimensions();
    let x0 = region.x.min(w);
    let y0 = region.y.min(h);
    let x1 = region.x.saturating_add(region.width).min(w);
    let y1 = region.y.saturating_add(region.height).min(h);

    let mut clipped = GrayImage::new(w, h);
    for y in y0..y1 {
        for x in x0..x1 {
            clipped.put_pixel(x, y, Luma([src.get_pixel(x, y).0[0]]));
        }
    }
    DynamicImage::ImageLuma8(clipped)
}

inventory::submit! {
    EngineInfo {
        id: "lama-manga",
        name: "Lama Manga",
        needs: &[Artifact::SegmentMask, Artifact::BubbleMask],
        produces: &[Artifact::Inpainted],
        load: |runtime, cpu| Box::pin(async move {
            let m = Lama::load(runtime, cpu).await?;
            Ok(Box::new(Model(m)) as Box<dyn Engine>)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;

    #[test]
    fn scoped_cleanup_fills_the_selected_text_region_only() {
        let mut mask = GrayImage::new(64, 64);
        let bubbles = GrayImage::new(64, 64);
        mask.put_pixel(28, 28, Luma([255]));
        let blocks = [TextRegion {
            x: 22.0,
            y: 24.0,
            width: 20.0,
            height: 10.0,
            detected_font_size_px: Some(18.0),
            ..TextRegion::default()
        }];

        let expanded = expand_text_mask(
            &DynamicImage::ImageLuma8(mask),
            &DynamicImage::ImageLuma8(bubbles),
            &blocks,
            true,
        );

        assert_eq!(expanded.get_pixel(40, 31).0[0], 255);
        assert_eq!(expanded.get_pixel(10, 10).0[0], 0);
    }

    #[test]
    fn scoped_cleanup_excludes_unselected_text_geometry() {
        let mask = DynamicImage::ImageLuma8(GrayImage::from_pixel(64, 64, Luma([255])));
        let id = NodeId::new();
        let transform = Transform {
            x: 20.0,
            y: 20.0,
            width: 10.0,
            height: 8.0,
            ..Transform::default()
        };
        let text = TextData::default();

        let protected = exclude_text_nodes(&mask, &[(id, &transform, &text)]);

        assert_eq!(protected.to_luma8().get_pixel(24, 24).0[0], 0);
        assert_eq!(protected.to_luma8().get_pixel(10, 10).0[0], 255);
    }
}
