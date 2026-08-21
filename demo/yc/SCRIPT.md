---
title: Reyn Studio YC demo
duration_seconds: 160
format: screen-only with founder narration
---

# Timed storyboard and exact narration

This is the current supported flow. `REFERENCE_RUN_BLOCKER.md` explains why a
reference-solver field is not loaded into Results yet. Do not show a fabricated result.
Research 3D evidence is the 32³ one-jump obstacle operator (RelL2 0.052 at 128 steps,
32,768 cells). CAD Run voxels the case to that declared grid.

## 00:00–00:15 — Start with the engineering problem

**Action:** Start after the capsule is visible in **Case Setup**. Keep the geometry and
workflow rail on screen.

**Expected UI:** `primary_capsule_d80_l260mm` is loaded from isolated demo state.

**Narration:** “A flow result is only useful if the geometry and assumptions behind it
survive review. Reyn Studio starts there: one case that carries the source, setup,
diagnostics, and evidence together.”

## 00:15–00:43 — Prove the source

**Action:** Open **Source / Preflight**. Point to SHA-256, 1,920 triangles, millimetre
extents, zero open edges, and zero non-manifold edges.

**Expected UI:** The fixture hash begins `ca2b82f7`; the source topology is closed.

**Narration:** “This is a deterministic 1,920-triangle capsule STL in millimetres.
Reyn hashes the bytes, records the source extents, and checks the mesh itself. There
are no open or non-manifold edges, so the source identity and topology are explicit
before any compute.”

## 00:43–01:10 — Inspect the solver transform

**Action:** Set geometry units to **mm**. Expand transform and voxel diagnostics.
Approve the proposed transform only after pointing to orientation, scale, 64³ grid,
solid occupancy, connected components, minimum cells across, and boundary clearance.

**Expected UI:** Source-to-solver transform is approved; geometry preflight passes.

**Narration:** “I confirm millimetres instead of accepting a guess. Reyn records the
orientation and scale into the solver frame, then checks occupancy in all three axes,
component count, feature resolution, and domain clearance. This is the transform the
case will keep, not a hidden preprocessing step.”

## 01:10–01:40 — Build the operating point

**Action:** Set speed to **0.01 m/s**; retain density `1.225 kg/m³`, dynamic viscosity
`1.81e-5 Pa·s`, reference pressure `101325 Pa`, direction `+X`, and horizon `4`.
Point to the computed Reynolds number.

**Expected UI:** Reynolds number is about 203; each entered or derived value is visible.

**Narration:** “Now I set speed, density, viscosity, reference pressure, direction, and
horizon. Reyn derives Reynolds number—about two hundred and three here—while keeping
entered values separate from derived ones. A valid operating point does not silently
waive geometry or applicability checks.”

## 01:40–02:02 — Show durable evidence boundaries

**Action:** Open **Evidence**. Point to the source revision and content hash, then hover
the disabled result export.

**Expected UI:** Source evidence is present. Run-linked field/report exports remain
disabled because no completed immutable run exists.

**Narration:** “The source revision is already content-addressed in the project.
Field and report exports require a completed immutable run, so they stay unavailable.
Reyn distinguishes what was imported, what was derived, and what was never computed.”

## 02:02–02:30 — Prove fail-closed rejection

**Action:** Import
`demo/yc/assets/defective_sphere_missing_cap_r50mm.stl`. Open Preflight and point to
the open-boundary diagnostic.

**Expected UI:** 1,680 triangles, 48 open boundary edges, `mesh.open_boundary`, and a
non-waivable execution block.

**Narration:** “This second STL is intentionally missing a cap. Reyn finds forty-eight
open boundary edges and blocks the case. I cannot dismiss that with a note, because
inside-outside classification and any downstream surface quantity would be
untrustworthy.”

## 02:30–02:40 — Close on the product boundary

**Action:** Return to the capsule case. Stop recording at 2:40.

**Expected UI:** Real source diagnostics and setup; no completed result.

**Narration:** “The qualified neural model is the next validation step. What is real
today is the reviewable workflow around it: deterministic geometry checks, explicit
physics setup, durable provenance, and hard stops when evidence is missing.”

# Contingencies

- **Capsule has not imported when recording starts:** Wait off-camera. If it still does
  not appear, click **Import Geometry…** and select the primary fixture. Say:
  “I’m loading the same checked fixture manually; this is the ordinary import path.”
- **Engine reports unavailable:** Continue. Say: “The compute sidecar is unavailable,
  but local geometry review remains usable and the missing dependency is explicit.”
- **Capsule viewport is slow:** Reset and use `fallback_cube_100mm.stl`. Replace
  “capsule” with “100 millimetre cube” and “1,920” with “12”; keep every other claim.
- **Defective import takes too long:** Show `fixture-manifest.json` and say:
  “The automated package test measures 48 boundary edges and pins the same hard-gate
  diagnostic.”
- **A model unexpectedly appears:** Do not run it. Keep the close unchanged.
- **Evidence navigation differs:** Stay in Case Setup and point to the visible source
  hash plus disabled Run. Do not imply that an export was produced.
