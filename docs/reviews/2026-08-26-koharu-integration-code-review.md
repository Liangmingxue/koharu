# Koharu integration code review

Date: 2026-08-26

Review range: `9512a821e1ddb46c746ca5972bcc02a6cd1dea9e..70c30bacfd391dc94f6128927536a3c9615e1c13`

Additional scope: untracked `contracts/execution-manifest.v5.schema.json` and `contracts/evaluator-request.v4.schema.json`

## Assessment

The reviewed changes are not ready for a Gateway capability declaration or
production integration. The immutable OCR evidence and rendering-style work
has a sound foundation, but the pipeline precondition model, asynchronous
idempotency lifecycle, crash recovery, capability negotiation, and scoped
inpaint composition still have correctness gaps.

The untracked contracts must not be committed to this repository. Chart Agent
owns workflow contracts, and its active registry remains on
`execution-manifest.v4` and `evaluator-request.v3`.

## High-priority findings

### 1. Pipeline writes have no scene epoch or hash precondition

`crates/koharu-rpc/src/routes/pipelines.rs:103` binds the idempotency receipt to
the project path and request body, but the route accepts no `If-Match` scene
epoch. Engine results are subsequently committed with unconditional
`session.apply` in `crates/koharu-app/src/pipeline/mod.rs:246`.

A concurrent edit can therefore invalidate the engine snapshot while the old
result is still committed over the newer scene. The route should require and
bind an initial epoch/hash precondition, and every pipeline commit should
enforce the expected epoch sequence.

### 2. A pipeline receipt completes before the pipeline effect completes

The pipeline is detached at
`crates/koharu-rpc/src/routes/pipelines.rs:183`, while its receipt is marked
completed at `crates/koharu-rpc/src/routes/pipelines.rs:229`. Job state exists
only in the in-memory jobs registry.

After a process crash, replay returns the old operation ID even though the
operation no longer exists and cannot be resumed. Pipeline operation state
must be durably journaled and restored, or the receipt must remain in a
non-terminal accepted/running state until a durable terminal outcome exists.

### 3. Chart container regions can suppress real text labels

`crates/koharu-app/src/pipeline/engines/pp_doclayout.rs:65` treats a `chart`
region as a text block. The containment-based deduplication at line 93 rejects
a region when at least 90 percent of either region overlaps.

If a chart container is emitted before its contained labels, the labels are
discarded and OCR receives only one chart-sized block. Chart regions should
remain structural/ROI evidence, or be emitted as a fallback only when no
contained text regions exist. A chart-container-plus-label ordering regression
test is required.

### 4. Repeated scoped inpaint can restore earlier edits

When no explicit repair region is present, scoped AOT, Lama, and Flux runs
still start from the Source image:

- `crates/koharu-app/src/pipeline/engines/aot.rs:47`
- `crates/koharu-app/src/pipeline/engines/lama.rs:51`
- `crates/koharu-app/src/pipeline/engines/flux2_klein.rs:41`

Inpainting block A and then block B can restore A from Source when the second
full-page output replaces the Inpainted layer. A scoped run should use the
existing Inpainted layer as its base even when `region` is absent. This needs a
two-successive-scope regression test.

### 5. Receipt replacement is not cross-platform durable

`crates/koharu-rpc/src/idempotency.rs:291` uses `std::fs::rename` to replace an
existing pending receipt. Replacement by `rename` fails on Windows when the
destination exists, so a successful side effect can be followed by a receipt
completion error and a permanently pending receipt.

The first creation of `http-idempotency` also does not sync the data-root
directory after creating the receipt directory. A crash can therefore lose
the directory entry despite syncing the receipt file and receipt directory.
Use a tested cross-platform atomic replacement operation and explicitly make
new receipt-root creation durable.

### 6. Known no-effect failures remain permanently ambiguous

A pending receipt is persisted at
`crates/koharu-rpc/src/idempotency.rs:141` before conditional application, but
the handlers complete only successful responses. A normal stale-epoch 412,
invalid operation, missing page, or known pre-effect engine error leaves the
receipt pending. A retry with the same key then reports an ambiguous outcome.

Receipts need a terminal no-effect failure state containing the exact HTTP
status and body. A genuinely ambiguous crash window must remain distinct from
a failure that is known not to have applied a side effect.

### 7. Capability readiness is not negotiated with the running Koharu binary

`crates/koharu-rpc/src/routes/meta.rs:17` exposes version/device information and
the engine catalog, but no versioned Gateway capability handshake advertises
conditional writes, scoped masks, rendered-direction support, receipt
protocol, or supported action mappings.

Chart Agent can therefore claim configured capabilities that are absent from
the running binary. Add a versioned capability endpoint, consume and hash its
actual response in Chart Agent, and test route/backend/handshake/receipt
agreement.

### 8. The OpenAPI surface was not regenerated

The new `/idempotency/receipt` route is registered in
`crates/koharu-rpc/src/api.rs`, but `ui/openapi.json` and the RPC path snapshot
were not updated. A fresh run of the OpenAPI integration test fails with that
missing route.

Regenerate the OpenAPI document and clients, review the generated changes, and
update the snapshot as part of the same capability change.

## Medium-priority findings

### 9. History receipts do not bind project/session identity

`crates/koharu-rpc/src/routes/history.rs:86` hashes only the expected epoch and
operation. Receipts share a data-root-wide namespace, so the same key and
operation against duplicated projects can replay a response without mutating
the current project. Include immutable project/session identity as the other
mutation routes do, and add a cross-session route test.

### 10. Scope padding can admit adjacent preserve/skip pixels

`crates/koharu-app/src/pipeline/engines/support.rs:162` dilates selected-block
authorization by four pixels but does not subtract non-selected text regions.
Segment pixels from a nearby preserve/skip block can therefore enter the edit
mask. The final authorization mask must explicitly exclude non-authorized
blocks, including close and overlapping geometry cases.

### 11. Rotated sprite placement is inconsistent in the UI

`ui/components/canvas/TextBlockLayer.tsx:262` scales sprites around their
center while positioning them from a scaled top-left coordinate. At zoom
levels other than 100 percent this shifts the sprite. During drag/resize,
`TextBlockLayer.tsx:117` also overwrites the transform with translation only,
temporarily removing rotation.

Use separate positioning/scaling and center-rotation wrappers, preserve the
rotation during imperative drag updates, and add zoomed rotated-sprite tests.

### 12. Headless data-root isolation is not wired through

Startup still loads the ordinary persisted config at
`crates/koharu/src/app.rs:78`; the CLI has no data-root option, and the renderer
constructs its font service from the platform default root at
`crates/koharu-app/src/renderer.rs:94`.

Headless runs expecting `KOHARU_DATA_ROOT` isolation can therefore share
projects, receipts, models, or font caches. Implement and test explicit
CLI/environment precedence and thread the resolved root through all services.

### 13. Gateway-side receipts remain optional for some side effects

Missing `Idempotency-Key` returns `Decision::Disabled` at
`crates/koharu-rpc/src/idempotency.rs:101`. The adjacent Chart Agent HTTP
adapter omits idempotency headers for project creation and page upload, so
those side effects still have no durable receipt.

Chart Agent-facing mutations must carry stable keys, and each mutation's
OpenAPI operation should document its idempotency and precondition behavior.

### 14. The untracked v5/v4 schemas are misplaced and incomplete

The files under the untracked Koharu `contracts/` directory belong in the
adjacent Chart Agent repository and have not been activated in its contract
registry or policy. In addition, `execution-manifest.v5` permits only PNG/JPEG
sprite encodings while Koharu persists renderer sprites using its raw-RGBA
blob wrapper.

Do not add these files to Koharu. If the v5 representation gate requires the
migration, publish and register the schemas atomically in Chart Agent while
retaining the old versions for reopen compatibility, and define the exact
sprite media representation.

## Strengths

- Source geometry consumers prefer immutable `recorded_source_*` evidence.
- Rendered direction is separated from source direction instead of mutating
  OCR evidence.
- Explicit text scopes reject duplicate, missing, and non-text node IDs.
- Renderer patches preserve explicit styles and avoid persisting synthetic
  defaults.
- Receipt files use temporary writes, file sync, rename, and receipt-directory
  sync as a useful base for the remaining durability work.

## Verification evidence

The existing repository `target/` directory was newer than several source
files. Its RPC test binary contained tests that do not exist in the current
source, so its apparent nine-test result is not valid evidence for this
revision.

A clean build target was used instead:

```bash
CARGO_TARGET_DIR=/tmp/koharu-review-target \
  cargo test -p koharu-rpc -p koharu-app -p koharu-renderer --lib
```

Results:

- `koharu-app`: 58 passed
- `koharu-renderer`: 51 passed
- `koharu-rpc`: 5 passed
- `RenderControlsPanel`: 7 passed
- `cargo fmt --check`: passed
- `git diff --check`: passed

The following fresh integration check fails:

```bash
CARGO_TARGET_DIR=/tmp/koharu-review-target \
  cargo test -p koharu-rpc --test openapi
```

Failure: `openapi_paths_snapshot` is missing the newly registered
`GET /idempotency/receipt` route.

Passing library tests do not cover the stale/reopen/idempotency crash windows,
cross-platform receipt replacement, sequential scoped inpaint, capability
negotiation, or full Chart Agent integration described above.

## Required disposition

1. Fix the pipeline CAS and asynchronous receipt lifecycle before declaring
   Gateway side-effect safety.
2. Correct chart-region handling and sequential scoped inpaint composition.
3. Implement a real capability handshake and regenerate the OpenAPI surface.
4. Add stale, reopen, crash-recovery, cross-session, and Windows receipt
   acceptance coverage.
5. Keep the untracked schemas out of Koharu and perform any contract migration
   atomically in Chart Agent.
