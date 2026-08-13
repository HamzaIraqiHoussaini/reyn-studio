# STEP import implementation and engineering review

**Status:** included in Reyn Studio 0.3.0. This is safe single-part STEP
support, not universal CAD import.

## What is implemented

- `.stp` and `.step` are accepted by the picker, drag-and-drop path, QA import
  hook, project persistence, reopen/hydration path, and body-orientation
  re-voxelization path.
- STEP Part 21 is parsed and B-rep shells are tessellated in-process with the
  Apache-2.0 Truck crates. No GPL code, static OpenCASCADE, C++, or new runtime
  sidecar enters the desktop binary.
- The source bytes and SHA-256 remain authoritative. The derived triangle mesh
  is not substituted for the source object in the project bundle.
- Imports record source format, source-declared units, translator and version,
  B-rep shell count, relative and absolute chord tolerance, and face-boundary
  weld tolerance in `GeometryPreflight`, so those facts enter the immutable
  case contract.
- STEP units prefill the existing unit control but do not approve it. The
  operator must still approve units, orientation, scale, and solver placement.
- Files with missing, unsupported, or multiple length units are rejected.
- Assemblies with occurrence transforms are rejected with an actionable message.
  Multi-shell STEP without an assembly graph opens a Fusion-like pick-one solid
  chooser; Truck does not currently preserve occurrence transforms, so treating
  every assembly definition as a body would analyze the wrong geometry.
- Input is bounded to 128 MiB, 256 shells, and 250,000 triangles. Translator
  panics become an import error rather than taking down the app.
- The existing hard topology and three-axis voxel-classification gates apply to
  the tessellated mesh. Open boundaries, non-manifold edges, intersections,
  disconnected bodies, and excessive axis disagreement cannot be waived.

## Verified behavior

The automated corpus includes two real exporter outputs:

1. An AP214 cuboid from Formlabs/foxtrot. It tessellates deterministically into
   one closed manifold component and passes topology diagnostics.
2. An Onshape AP242 Edition 2 curved part. Translation is deterministic, but
   Truck currently leaves face-boundary seams. Reyn reports the resulting open
   edges and blocks execution. The test locks this behavior so an upstream
   translator change cannot turn a partial mesh into an unreviewed run.

Unit tests also cover millimetre/metre/inch detection, conflicting unit
contexts, assembly rejection, deterministic repeat import, malformed sources,
and the existing orientation worker failure boundary.

The 0.3.0 release qualification reruns the Rust, bridge, corpus, macOS, and
Windows package checks from the tagged source. Package byte counts and hashes
are recorded from final outputs rather than copied from an earlier candidate.

## Current support boundary

Supported now:

- Single resolved part/body represented by one or more B-rep shells.
- AP203/AP214-style exact geometry using the Truck-supported curves and
  surfaces.
- AP242 files when their exact geometry subset tessellates successfully and the
  resulting mesh passes the same hard gates as STL.
- Source units of millimetres, centimetres, metres, inches, or feet.
- Local import, project save/reopen, reorientation, voxel preflight, and
  evidence provenance.

Not supported now:

- Assemblies with occurrence transforms, suppressed/configured components, or
  analyzing an entire assembly as one body.
- Semantic or graphical PMI, GD&T, materials, colours, layers, or product
  metadata. None affects the current occupancy-mask model.
- Stable face IDs or internal-flow boundary assignment (named region labels can
  be authored on structural candidates for future mapping).
- Automatic hole filling, self-intersection repair, shell deletion, or other
  geometry-changing healing.
- STEP tessellated-representation-only files, point clouds, wireframes, and
  open surface models.
- Native CAD formats such as SLDPRT, CATPart, IPT, or Parasolid.

Supported for multi-body STEP without an assembly graph: pick-one solid
chooser (operator selects a B-rep shell entity before tessellation).

## What is still needed before calling STEP production-grade

### 1. A broader qualification corpus — slice 2 started

Automated corpus now covers:

- Existing: AP214 cuboid (closed) + Onshape AP242 curved (open seams visible).
- New fail-closed synthetics under `test-geometry/corpus/`: assembly occurrence,
  conflicting length units, truncated/malformed STEP (locked in `src/cad_step.rs`).
- Inventory + vendor slots: `test-geometry/corpus/README.md` (SolidWorks / NX /
  Fusion / Onshape extras still **pending real exports** — empty slots are not
  an OCCT ceiling).

Still needed before calling Truck production-grade: real single-part exports
from SolidWorks, NX, CATIA, Creo, Inventor, Fusion, and additional Onshape
parts across AP203/AP214/AP242 with recorded extents. Release criterion: every
**Supported** fixture has deterministic triangle identity, units, extents,
topology diagnostics, and voxel occupancy across repeated runs on macOS arm64
and Windows x64.

### 2. Move expensive translation off the UI thread — DONE

Initial STEP/3MF/STL read → translate → diagnose → voxelize now runs on the
`reyn-geometry-import` worker with generation IDs and stale-result suppression,
matching the orientation worker. Multi-shell (non-assembly) sources open a
pick-one solid chooser; true assemblies with occurrence transforms remain
rejected. Cancel by starting another import (stale generations are discarded).

### 3. Process isolation and resource enforcement

`catch_unwind` handles Rust panics but not allocator exhaustion, infinite
translator work, or native faults introduced by a future kernel. Production
CAD translation should run in a separate `reyn-cad-bridge` process with a
versioned IPC contract, wall-clock timeout, memory limit where the OS permits,
captured stderr, and atomic output.

Wire protocol: `docs/occt_bridge_protocol.v1.json` and
`docs/OCCT_BRIDGE_SPIKE.md`. Slice 1 is implemented: `reyn-cad-bridge` stub +
`src/cad_bridge.rs` framing/client with IPC tests (hello, fixture mesh, cancel,
timeout, oversize). No OCCT in the desktop binary; Truck remains the import
path until corpus evidence gates a fallback.

Release criterion: crash, timeout, oversized output, malformed IPC, and user
cancellation all leave the project unchanged and produce a specific error.

### Packaging honesty (macOS research closure)

The research-sidecar allowlist remains an exact import closure. Missing
contract resources fail packaging; release qualification must not expand the
allowlist unless the pinned research checkout contains and imports those files.

### 4. A production translator decision

Truck is permissively licensed and keeps packaging simple, but the AP242
fixture demonstrates that its current B-rep tessellation is not broad enough
for a universal STEP claim. Keep it as the first translator and qualification
target. If partner files repeatedly fail:

- Preferred open route: an out-of-process OpenCASCADE bridge using dynamic
  libraries, LGPL notices, replaceable libraries, signed nested binaries, and
  explicit macOS notarization/Windows packaging tests.
- Preferred commercial route: CAD Exchanger or HOOPS Exchange when native CAD
  support becomes a paid requirement.
- Forbidden route: static OpenCASCADE inside `reyn-studio`, or GPL mesh-repair
  code in a distributed proprietary binary.

### 5. Tessellation controls and convergence evidence

The current chord tolerance is fixed at 0.1% of the source point-cloud
diameter, followed by a 0.005% relative face-boundary weld. This is deterministic
and recorded, but one setting does not fit both thin features and large simple
bodies. Add a preflight-only quality selector or automatic convergence check
that compares occupancy at two tolerances.

Release criterion: tightening tessellation leaves the target-grid occupancy
unchanged, or the case is labeled tessellation-sensitive and blocked.

### 6. Derived-geometry identity — DONE for the 0.3.0 contract

`GeometryPreflight` now records:

- `analyzed_mesh_sha256` — SHA-256 of the source-space triangle mesh
  (`cad::analyzed_mesh_sha256`, encoding `reyn.analyzed-mesh.v1`)
- `import_steps` — ordered `translate` → (`tessellate`/`weld` for STEP) →
  `diagnose` → `voxelize`, plus `orient` on body re-voxelization

Case Setup shows the short analyzed-mesh digest next to the source digest.
The exact case contract serializes both fields, and HTML/PDF/PNG evidence
renders the digest and ordered derivation steps before optional signing. A
known-answer test fixes the little-endian `f32` mesh encoding across targets.
Legacy projects with empty steps remain readable (no hard fail on reopen).
Release qualification still compares every Supported corpus fixture on Windows
x64 and macOS arm64.

### 7. Assemblies and stable region identity

Assembly support requires occurrence transforms, operator selection of analysis
bodies, duplicate-instance handling, and component lineage. Internal flow also
requires stable face-region IDs and a reimport diff for preserved, changed,
added, removed, and ambiguous regions. Do not infer these from triangle order.

**Landed (protocol / host, no OCCT yet):** bridge `list_occurrences` +
`occurrence_transform_4x4` / per-triangle `bridge_face_id` fields
(`docs/occt_bridge_protocol.v1.json`, stub in `src/cad_bridge.rs`); remap
classifier and fail-closed helpers in `src/cad_identity.rs`; optional
`identity_kind` / `stable_face_id` on `NamedRegionAssignment`. Heuristic
`component-*` candidates remain draft-only and never count as preserved across
reimport.

Release criterion: an edited assembly cannot silently remap a boundary
assignment or analyze an untransformed part definition.

### 8. Cross-platform package and performance qualification

The new Rust crates are portable, but both release targets need fresh package
verification: clean-machine launch, STEP import, project save/reopen,
reorientation, archive-size delta, cold-start delta, peak import memory, and
malformed-file behavior. Dependency licenses and notices must be included in
the shipped archive.

Release criterion: the macOS arm64 and Windows x64 packages pass the same fixture
manifest and produce matching source/mesh/voxel hashes.

## Release language

Safe wording for Reyn Studio 0.3.0:

> Imports STL and supported single-part STEP files. STEP geometry is
> deterministically tessellated, units and translator settings are recorded,
> and the result must pass hard topology and voxel-fidelity checks before use.

Do not say “full STEP support,” “all CAD files,” “assemblies,” “CAD healing,” or
“production-qualified STEP” until the corresponding acceptance criteria above
are met.
