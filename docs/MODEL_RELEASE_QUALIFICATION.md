# First production `.reynmodel`: release-qualification gate

**Assessment date:** 2026-08-07  
**Decision:** **BLOCKED — no production-qualified external-CAD model**  
**Intended product lane:** Reyn Studio external-CAD execution  
**Required model lane:** geometry-conditioned 3D obstacle flow (`fixed_body_brinkman.v1`, 4→3, grid-matched)  
**Bundled preview:** `packaging/models/yc-preview-h64/` exists and is authenticated, but it is a **2D H128 YC research preview**, not a production external-CAD qualifier

## Executive decision

Studio now fail-closes external-flow CAD on `qualification_class == production` plus the exact 3D contract (`src/engine.rs`). Authenticated preview / research packs remain inspectable in the model library and packaging manifests, but they cannot bind a case run.

The checked-in YC preview pack (`com.reyn.yc-preview-model-release/1`) is honest about its boundary:

- schema + digests validated by `validate_yc_preview_model_release`
- bundle identity `reyn-h64-tail-brinkman-seed0` / `1.0.0-yc-preview`
- architecture is **2D** (`reyn.direct-flow-map.2d/1`, 4→2, grid 128)
- physics id is `fixed_body_brinkman.v2` (not the Studio CAD contract `fixed_body_brinkman.v1`)
- limitations explicitly say not production-qualified CFD

Therefore:

1. The preview pack may ship for authenticity / library / packaging smoke.
2. It must not be presented as the Studio external-CAD production model.
3. The first production model still requires the four gates below for one immutable 3D artifact.

## Production gates (unchanged bar)

The first production model is releasable only when all four gates pass for the
same immutable artifact:

1. **Scientific gate:** sealed held-out H32/H64/H128 evaluation, physics/load
   guardrails, and three-training-seed aggregation pass.
2. **Artifact gate:** provenance, calibration report, source checkpoint identity,
   benchmark hashes, deterministic conversion, and `.reynmodel` verification pass.
3. **Runtime/app gate:** CPU and MPS model smoke, 3D CAD geometry cases, save/reopen,
   evidence export, and rollback pass on supported clean machines.
4. **Distribution gate:** the exact model and app/runtime are authenticated; the app is
   Developer ID signed, notarized, stapled, and Gatekeeper-assessed.

A bundle that verifies TUF + Ed25519 is structurally safe. That is not production
scientific qualification.

## Runtime fail-closed rules (code)

| Check | Preview pack today | Production CAD requirement |
| --- | --- | --- |
| `qualification_class` | `preview` (from `yc-preview` version / limitations) | `production` |
| dimension / channels | 2D · 4→2 | 3D · 4→3 |
| physics contract | `fixed_body_brinkman.v2` | `fixed_body_brinkman.v1` |
| authenticity | verified when TUF/state present | verified |
| packaging manifest schema | `com.reyn.yc-preview-model-release/1` with incomplete boundary | separate production release schema (not yet published) |

## Historical note

Earlier drafts of this document (2026-07-25) said no `.reynmodel` existed locally.
That is obsolete for the YC preview lane: the preview bundle is present under
`packaging/models/yc-preview-h64/`. The production external-CAD decision remains blocked.
