//! Scene mutation: multipart page import, /history/apply + undo/redo, batch.
//!
//! The generated client doesn't wire multipart bodies or typed `Op` unions
//! well, so scene mutations go through reqwest directly with `koharu_core::Op`
//! serialized as JSON — end-to-end round trip through axum's JSON extractor
//! exercises exactly the wire format the frontend will use.

use koharu_client::apis::default_api as api;
use koharu_client::models;
use koharu_core::{
    DetectorEvidence, ImageRole, LineDetectorEvidence, Node, NodeId, NodeKind, Op, PagePatch,
    SOURCE_GEOMETRY_EVIDENCE_VERSION, SourceGeometryEvidence, TextData, TextDirection, Transform,
};
use koharu_integration_tests::TestApp;
use reqwest::multipart::{Form, Part};
use serde_json::Value;

async fn apply(app: &TestApp, op: Op) -> anyhow::Result<u64> {
    let resp = app
        .client_config
        .client
        .post(format!("{}/history/apply", app.base_url))
        .json(&op)
        .send()
        .await?
        .error_for_status()?;
    let v: Value = resp.json().await?;
    Ok(v["epoch"].as_u64().expect("epoch in response"))
}

async fn apply_at_epoch(
    app: &TestApp,
    expected_epoch: u64,
    op: Op,
) -> anyhow::Result<reqwest::Response> {
    Ok(app
        .client_config
        .client
        .post(format!("{}/history/apply", app.base_url))
        .header(reqwest::header::IF_MATCH, format!("\"{expected_epoch}\""))
        .json(&op)
        .send()
        .await?)
}

async fn undo(app: &TestApp) -> anyhow::Result<Option<u64>> {
    let resp = app
        .client_config
        .client
        .post(format!("{}/history/undo", app.base_url))
        .send()
        .await?
        .error_for_status()?;
    let v: Value = resp.json().await?;
    Ok(v["epoch"].as_u64())
}

async fn redo(app: &TestApp) -> anyhow::Result<Option<u64>> {
    let resp = app
        .client_config
        .client
        .post(format!("{}/history/redo", app.base_url))
        .send()
        .await?
        .error_for_status()?;
    let v: Value = resp.json().await?;
    Ok(v["epoch"].as_u64())
}

async fn import_pages(app: &TestApp, files: Vec<(&str, Vec<u8>)>) -> anyhow::Result<Vec<String>> {
    let mut form = Form::new();
    for (name, bytes) in files {
        form = form.part(
            "file",
            Part::bytes(bytes)
                .file_name(name.to_string())
                .mime_str("image/png")?,
        );
    }
    let resp = app
        .client_config
        .client
        .post(format!("{}/pages", app.base_url))
        .multipart(form)
        .send()
        .await?
        .error_for_status()?;
    let v: Value = resp.json().await?;
    Ok(v["pages"]
        .as_array()
        .expect("pages array")
        .iter()
        .map(|id| id.as_str().expect("uuid string").to_string())
        .collect())
}

#[tokio::test]
async fn import_pages_creates_source_nodes() -> anyhow::Result<()> {
    let app = TestApp::spawn().await?;
    app.open_fresh_project("p").await?;

    let png = TestApp::tiny_png(32, 16, [255, 0, 0, 255]);
    let ids = import_pages(&app, vec![("a.png", png.clone()), ("b.png", png.clone())]).await?;
    assert_eq!(ids.len(), 2);

    let session = app.app.current_session().expect("session");
    let scene = session.scene.read();
    assert_eq!(scene.pages.len(), 2);
    for page in scene.pages.values() {
        assert_eq!(page.width, 32);
        assert_eq!(page.height, 16);
        // Each page has exactly one Source image node.
        let sources = page
            .nodes
            .values()
            .filter(|n| matches!(&n.kind, NodeKind::Image(i) if i.role == ImageRole::Source))
            .count();
        assert_eq!(sources, 1);
    }
    Ok(())
}

#[tokio::test]
async fn multiline_source_geometry_round_trips_through_http_and_reopen() -> anyhow::Result<()> {
    let app = TestApp::spawn().await?;
    let project = api::create_project(
        &app.client_config,
        models::CreateProjectRequest {
            name: "Geometry Roundtrip".into(),
        },
    )
    .await?;
    let page_ids = import_pages(
        &app,
        vec![(
            "geometry.png",
            TestApp::tiny_png(160, 100, [255, 255, 255, 255]),
        )],
    )
    .await?;
    let page_id = page_ids[0].parse::<uuid::Uuid>().map(koharu_core::PageId)?;
    let node_id = NodeId::new();
    let line_one = [[10.0, 20.0], [140.0, 24.0], [139.0, 40.0], [9.0, 36.0]];
    let line_two = [[12.0, 50.0], [130.0, 54.0], [129.0, 70.0], [11.0, 66.0]];
    let geometry = SourceGeometryEvidence {
        schema_version: SOURCE_GEOMETRY_EVIDENCE_VERSION.into(),
        block_polygon: [[9.0, 20.0], [140.0, 24.0], [129.0, 70.0], [11.0, 66.0]],
        line_polygons: vec![line_one, line_two],
        source_direction: TextDirection::Horizontal,
        source_direction_source: "fixture.direction.v1".into(),
        source_rotation_deg: 1.76,
        detector_evidence: DetectorEvidence {
            detector_id: "fixture-detector".into(),
            detector_version: "fixture-detector.v1".into(),
            config_hash: format!("sha256:{}", "b".repeat(64)),
            block_polygon_confidence: Some(0.88),
            line_evidence: vec![
                LineDetectorEvidence {
                    text_confidence: Some(0.95),
                    polygon_confidence: Some(0.9),
                },
                LineDetectorEvidence {
                    text_confidence: Some(0.93),
                    polygon_confidence: Some(0.89),
                },
            ],
            direction_confidence: Some(0.8),
            rotation_confidence: None,
        },
    };
    apply(
        &app,
        Op::AddNode {
            page: page_id,
            node: Node {
                id: node_id,
                transform: Transform {
                    x: 9.0,
                    y: 20.0,
                    width: 131.0,
                    height: 50.0,
                    rotation_deg: 1.76,
                },
                visible: true,
                kind: NodeKind::Text(TextData {
                    text: Some("line one\nline two".into()),
                    source_geometry: Some(geometry.clone()),
                    ..Default::default()
                }),
            },
            at: 1,
        },
    )
    .await?;

    let before = scene_geometry_json(&app, page_id, node_id).await?;
    assert_eq!(before["linePolygons"].as_array().unwrap().len(), 2);

    let binary = app
        .client_config
        .client
        .get(format!("{}/scene.bin", app.base_url))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    #[derive(serde::Deserialize)]
    struct WireSnapshot {
        epoch: u64,
        scene: koharu_core::Scene,
    }
    let snapshot: WireSnapshot = postcard::from_bytes(&binary)?;
    assert!(snapshot.epoch > 0);
    let NodeKind::Text(text) = &snapshot.scene.node(page_id, node_id).unwrap().kind else {
        panic!("expected text node");
    };
    assert_eq!(text.source_geometry.as_ref(), Some(&geometry));

    api::delete_current_project(&app.client_config).await?;
    api::put_current_project(
        &app.client_config,
        models::OpenProjectRequest { id: project.id },
    )
    .await?;
    let reopened = scene_geometry_json(&app, page_id, node_id).await?;
    assert_eq!(reopened, before);
    Ok(())
}

async fn scene_geometry_json(
    app: &TestApp,
    page_id: koharu_core::PageId,
    node_id: NodeId,
) -> anyhow::Result<Value> {
    let snapshot: Value = app
        .client_config
        .client
        .get(format!("{}/scene.json", app.base_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(snapshot["scene"]["pages"][page_id.to_string()]["nodes"][node_id.to_string()]["kind"]
        ["text"]["sourceGeometry"]
        .clone())
}

#[tokio::test]
async fn update_page_then_undo_round_trips() -> anyhow::Result<()> {
    let app = TestApp::spawn().await?;
    app.open_fresh_project("r").await?;

    let png = TestApp::tiny_png(10, 10, [0, 128, 0, 255]);
    let ids = import_pages(&app, vec![("pg.png", png)]).await?;
    let page_id: koharu_core::PageId = ids[0].parse::<uuid::Uuid>().map(koharu_core::PageId)?;

    let epoch_before = app.app.current_session().unwrap().epoch();

    let op = Op::UpdatePage {
        id: page_id,
        patch: PagePatch {
            name: Some("renamed".into()),
            width: None,
            height: None,
        },
        prev: Default::default(),
    };
    let e1 = apply(&app, op).await?;
    assert!(e1 > epoch_before);
    {
        let session = app.app.current_session().unwrap();
        let scene = session.scene.read();
        assert_eq!(scene.page(page_id).unwrap().name, "renamed");
    }

    let e2 = undo(&app).await?.expect("undo produced epoch");
    assert!(e2 > e1);
    {
        let session = app.app.current_session().unwrap();
        let scene = session.scene.read();
        assert_eq!(scene.page(page_id).unwrap().name, "pg.png");
    }

    let e3 = redo(&app).await?.expect("redo produced epoch");
    assert!(e3 > e2);
    {
        let session = app.app.current_session().unwrap();
        let scene = session.scene.read();
        assert_eq!(scene.page(page_id).unwrap().name, "renamed");
    }
    Ok(())
}

#[tokio::test]
async fn conditional_apply_rejects_stale_epoch_without_mutating_scene() -> anyhow::Result<()> {
    let app = TestApp::spawn().await?;
    app.open_fresh_project("conditional").await?;
    let page_ids = import_pages(
        &app,
        vec![("page.png", TestApp::tiny_png(10, 10, [0, 0, 0, 255]))],
    )
    .await?;
    let page_id: koharu_core::PageId =
        page_ids[0].parse::<uuid::Uuid>().map(koharu_core::PageId)?;
    let current_epoch = app.app.current_session().unwrap().epoch();
    let op = Op::UpdatePage {
        id: page_id,
        patch: PagePatch {
            name: Some("must-not-apply".into()),
            width: None,
            height: None,
        },
        prev: Default::default(),
    };

    let response = apply_at_epoch(&app, current_epoch - 1, op).await?;
    assert_eq!(response.status(), reqwest::StatusCode::PRECONDITION_FAILED);
    let session = app.app.current_session().unwrap();
    assert_eq!(session.epoch(), current_epoch);
    assert_eq!(session.scene.read().page(page_id).unwrap().name, "page.png");
    Ok(())
}

#[tokio::test]
async fn batch_op_is_one_undo_step() -> anyhow::Result<()> {
    let app = TestApp::spawn().await?;
    app.open_fresh_project("b").await?;

    let png = TestApp::tiny_png(8, 8, [0, 0, 255, 255]);
    let ids = import_pages(&app, vec![("x.png", png.clone()), ("y.png", png)]).await?;
    let p1 = ids[0].parse::<uuid::Uuid>().map(koharu_core::PageId)?;
    let p2 = ids[1].parse::<uuid::Uuid>().map(koharu_core::PageId)?;

    let batch = Op::Batch {
        ops: vec![
            Op::UpdatePage {
                id: p1,
                patch: PagePatch {
                    name: Some("A".into()),
                    ..Default::default()
                },
                prev: Default::default(),
            },
            Op::UpdatePage {
                id: p2,
                patch: PagePatch {
                    name: Some("B".into()),
                    ..Default::default()
                },
                prev: Default::default(),
            },
        ],
        label: "rename both".into(),
    };
    apply(&app, batch).await?;
    {
        let session = app.app.current_session().unwrap();
        let scene = session.scene.read();
        assert_eq!(scene.page(p1).unwrap().name, "A");
        assert_eq!(scene.page(p2).unwrap().name, "B");
    }

    // One undo rolls back both renames.
    undo(&app).await?;
    {
        let session = app.app.current_session().unwrap();
        let scene = session.scene.read();
        assert_eq!(scene.page(p1).unwrap().name, "x.png");
        assert_eq!(scene.page(p2).unwrap().name, "y.png");
    }
    Ok(())
}

#[tokio::test]
async fn replace_flag_clears_prior_pages() -> anyhow::Result<()> {
    let app = TestApp::spawn().await?;
    app.open_fresh_project("rep").await?;

    let png = TestApp::tiny_png(4, 4, [1, 2, 3, 255]);
    import_pages(&app, vec![("old.png", png.clone())]).await?;

    // Replace with fresh import.
    let mut form = Form::new();
    form = form.text("replace", "true");
    form = form.part(
        "file",
        Part::bytes(TestApp::tiny_png(6, 6, [4, 5, 6, 255]))
            .file_name("new.png".to_string())
            .mime_str("image/png")?,
    );
    let resp = app
        .client_config
        .client
        .post(format!("{}/pages", app.base_url))
        .multipart(form)
        .send()
        .await?
        .error_for_status()?;
    let body: serde_json::Value = resp.json().await?;
    assert_eq!(body["pages"].as_array().unwrap().len(), 1);

    let session = app.app.current_session().unwrap();
    let scene = session.scene.read();
    assert_eq!(scene.pages.len(), 1);
    let page = scene.pages.values().next().unwrap();
    assert_eq!(page.name, "new.png");
    assert_eq!(page.width, 6);
    Ok(())
}

#[tokio::test]
async fn image_layer_adds_custom_node() -> anyhow::Result<()> {
    let app = TestApp::spawn().await?;
    app.open_fresh_project("il").await?;

    let png = TestApp::tiny_png(100, 100, [10, 20, 30, 255]);
    let ids = import_pages(&app, vec![("base.png", png)]).await?;
    let page_id = &ids[0];

    let form = Form::new().part(
        "file",
        Part::bytes(TestApp::tiny_png(20, 20, [200, 0, 0, 255]))
            .file_name("logo.png".to_string())
            .mime_str("image/png")?,
    );
    let resp = app
        .client_config
        .client
        .post(format!("{}/pages/{}/image-layers", app.base_url, page_id))
        .multipart(form)
        .send()
        .await?
        .error_for_status()?;
    let body: serde_json::Value = resp.json().await?;
    assert!(body["node"].is_string());

    let session = app.app.current_session().unwrap();
    let scene = session.scene.read();
    let page_uuid = page_id.parse::<uuid::Uuid>().map(koharu_core::PageId)?;
    let page = scene.page(page_uuid).unwrap();
    // Source + Custom.
    assert_eq!(page.nodes.len(), 2);
    let custom = page
        .nodes
        .values()
        .find(|n| matches!(&n.kind, NodeKind::Image(i) if i.role == ImageRole::Custom))
        .expect("custom node");
    let NodeKind::Image(img) = &custom.kind else {
        unreachable!()
    };
    assert_eq!(img.natural_width, 20);
    assert_eq!(img.natural_height, 20);
    Ok(())
}
