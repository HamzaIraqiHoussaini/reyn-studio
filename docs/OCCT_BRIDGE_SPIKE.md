# OCCT out-of-process bridge — design spike

**Status:** slice 1 landed (stub bridge + framing tests). Not wired into STEP import UI. No OCCT linked.
**Constraint:** never statically link OpenCASCADE into the Reyn Studio desktop binary (LGPL + packaging / notarization cost).

## When to build

Only after the in-process Truck STEP path hits a hard ceiling on a representative customer corpus (curved AP242 seams that remain open after honest weld limits, unsupported surfaces, or assemblies that must be analyzed as multi-body with transforms). See `docs/STEP_IMPORT_REVIEW.md` corpus criteria.

## Shape

```
Reyn Studio (Rust, egui)
    │  length-prefixed JSON request over stdin/stdout
    ▼
reyn-cad-bridge (separate process)
    │  dynamically linked OCCT (LGPL-safe distribution)
    ▼
Tessellated mesh + units + shell inventory + warnings
```

Wire contract: [`docs/occt_bridge_protocol.v1.json`](occt_bridge_protocol.v1.json).

## Contract

- Input: absolute path or bytes handle, requested chord tolerance, max triangles/shells.
- Output: triangle soup in source units, declared length unit, shell count, translator/version, deterministic hash of tessellation parameters.
- Fail closed on assemblies unless the request explicitly selects one occurrence path.
- Crash isolation: bridge panics must not take down the Studio UI; Studio records a structured import error.
- Licensing: ship OCCT as a separate dylib/so/dll with LGPL notices; Studio remains Apache/MIT Rust.

## Concrete next engineering slices (ordered)

1. **Protocol compliance tests (no OCCT). — DONE**
   - Binary: `reyn-cad-bridge` (`src/bin/reyn_cad_bridge.rs`)
   - Shared framing/client/stub: `src/cad_bridge.rs` (lib `reyn_studio`)
   - Frozen contract: `docs/occt_bridge_protocol.v1.json`
   - Coverage: hello, fixture `tessellate_step`, assembly-without-occurrence fail-closed, cancel during `__slow__`, host timeout kill, oversize length-prefix fail-closed (`tests/cad_bridge_ipc.rs` + unit tests).
   - Studio STEP import still uses in-process Truck; the bridge is not on the import path yet.
2. **Occurrence listing + face-identity contract (no OCCT). — DONE (slice 1.5)**
   - `list_occurrences` stub op with assembly/part transforms.
   - `mesh_ok` optional `face_identity_kind`, per-triangle face ids, and `occurrence_transform_4x4`.
   - Host remap library: `src/cad_identity.rs` (heuristic ids never preserve; ambiguous/removed assigned regions fail closed).
   - `NamedRegionAssignment` persists optional `identity_kind` / `stable_face_id` (legacy reopen defaults).
3. **Qualification corpus expansion (Truck stays default).** Add SolidWorks / NX / Fusion / Onshape single-part fixtures per `STEP_IMPORT_REVIEW.md` §1. Record when Truck tessellation identity or open-shell diagnostics fail — that evidence gates slice 3.
4. **OCCT tessellate backend behind the same protocol.** Dynamic link only inside the bridge process; Studio still speaks JSON. Assemblies remain opt-in via `occurrence_path` chosen from `list_occurrences`.
5. **Wire Studio import fallback (optional before/with 4).** Host spawns `reyn-cad-bridge` only when Truck hits a recorded corpus ceiling; keep Truck as default.
6. **Packaging.** Separate bridge artifact + LGPL notices; never merge OCCT into the Studio Mach-O / PE. Notarization/Authenticode treat the bridge as its own signed nested binary.

## Non-goals

- Silent healing, PMI, native CAD SDKs, Parasolid/JT kernels inside Studio.
- Claiming “full STEP” or “assemblies supported” before corpus + occurrence selection ship.
