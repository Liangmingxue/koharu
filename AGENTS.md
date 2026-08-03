# Koharu integration instructions

## Repository role

This repository is the deterministic image-processing and editing backend for
Chart Agent. It owns OCR integration, scene persistence, mask/inpaint,
rendering, conditional scene writes, HTTP idempotency, and binary media
operations. It does not own LangGraph orchestration or any model Agent.

The adjacent repository
`/home/omnisky/xlm/newtask/charttrans-workspace/Chart_agent` owns the workflow,
contracts, Supervisor runtime, quality evaluator, and model-Agent clients.

## Authoritative sources

Use this order when descriptions disagree:

1. Current Rust/TypeScript source and tests in this repository.
2. Machine contracts and policy in the adjacent `Chart_agent/contracts/`.
3. `Chart_agent/chart-workflow.md` and component design documents.
4. Current status documents; never infer completion from chat history, an
   empty directory, `build/`, generated files, or old staging copies.

Do not reintroduce the former Text Perception/Actor/Executor Agent design.
Koharu operations are deterministic Gateway capabilities, not additional
Agents or LangGraph nodes.

## Verified baseline (2026-08-03)

- Branch `main` is two commits ahead of `origin/main`.
- The worktree also contains intentional, uncommitted Gateway, OCR, inpaint,
  renderer, reopen, scene-epoch, and HTTP-idempotency changes. Preserve them.
- Committed extensions include conditional scene-epoch writes and conditional
  mask/direction capabilities.
- The current worktree preserves PaddleOCR-VL quadrilateral/line polygons and
  source direction, scopes inpaint masks to selected text nodes, preserves
  explicit text style, and persists idempotency receipts on the server.
- Verified locally: 56 `koharu-app`, 51 `koharu-renderer`, and 5 `koharu-rpc`
  library tests pass; the 7 `RenderControlsPanel` tests pass.

This is a tested development baseline, not a production end-to-end claim.
Real-service crash recovery, stale/reopen/idempotency acceptance and the full
Chart Agent flow still require integration testing.

## Working rules

- Preserve unrelated user changes; do not reset or overwrite the dirty tree.
- Keep source OCR geometry immutable. A translated string must not change the
  recorded source polygon, direction, rotation, or detector evidence.
- Side effects must use absolute setters plus epoch/hash preconditions and
  durable idempotency receipts.
- A per-block edit/inpaint scope may include only blocks explicitly authorized
  by the plan; `preserve` and `skip` blocks must remain outside the edit mask.
- Do not claim a capability is ready until the HTTP route, backend operation,
  capability handshake, receipt behavior, and tests agree.

## Validation

Run the narrowest relevant checks first, then the integration set:

```bash
cargo test -p koharu-rpc -p koharu-app -p koharu-renderer --lib
bun run --cwd ui test -- tests/components/RenderControlsPanel.test.tsx
cargo fmt --check
```
