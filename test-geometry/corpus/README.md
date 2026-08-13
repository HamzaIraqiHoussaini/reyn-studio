# STEP qualification corpus

**Purpose:** Expand beyond the two shipped Truck fixtures so translator ceilings
are evidence-backed before any OCCT bridge is wired into Studio import.

**Policy:** Truck remains the default in-process translator. Real SolidWorks /
NX / CATIA / Creo / Inventor / Fusion / Onshape exports should be added here as
single-part files with the metadata table below. Do not claim “full STEP” until
every **Supported** row has deterministic triangle identity on macOS arm64 and
Windows x64.

## Inventory

| Fixture | Source / exporter | Schema | Units | Expected Truck outcome | Status |
|---|---|---|---|---|---|
| `../cuboid_ap214.step` | Formlabs / foxtrot (AP214) | AP214 | m | Closed manifold; voxelizes | **In suite** |
| `../part_ap242.step` | Onshape AP242 Ed. 2 curved | AP242 | m | Imports; open-boundary defects stay visible | **In suite** |
| `assembly_occurrence.step` | Synthetic minimal | — | — | Hard reject (`assemblies`) | **In suite** |
| `conflicting_units.step` | Synthetic minimal | — | m + mm | Hard reject (multiple length units) | **In suite** |
| `malformed_truncated.step` | Synthetic truncated | — | — | Hard reject (malformed) | **In suite** |
| `vendor/nist_ctc01_ap203_geom.step` | NIST CTC-01 geometry-only (CAD system scrubbed) | AP203 | per file | Deterministic import | **In suite** |
| `vendor/nist_ctc03_ap203_geom.step` | NIST CTC-03 geometry-only | AP203 | per file | Multi-shell → pick-one; deterministic | **In suite** |
| `vendor/nist_ctc05_ap203_geom.step` | NIST CTC-05 geometry-only | AP203 | per file | Multi-shell → pick-one; deterministic | **In suite** |
| `vendor/nist_ftc06_ap203_geom.step` | NIST FTC-06 geometry-only | AP203 | per file | Import or honest ceiling | **In suite** |
| `vendor/nist_ftc11_ap203_geom.step` | NIST FTC-11 geometry-only (tiny / older schema) | AP203-ish | per file | Import or honest ceiling | **In suite** |
| `vendor/nist_ctc01_ap242_e1.step` | NIST CTC-01 AP242 ed.1 (+PMI) | AP242 | per file | Import or honest ceiling | **In suite** |
| `vendor/nist_stc06_ap242_e3.step` | NIST STC-06 AP242 ed.3 (often tessellated) | AP242 e3 | per file | Import or honest ceiling | **In suite** |
| `vendor/nist_ftc08_ap242_e1_tessellated.step` | NIST FTC-08 AP242 ed.1 tessellated surfaces | AP242 | per file | Import or honest ceiling | **In suite** |
| `vendor/native-sources/solidworks-mbd-2018/*.SLDPRT` | NIST SolidWorks MBD 2018 CTC-01 native | — | — | Not imported; source for re-export | **Staged** |
| `vendor/native-sources/nx-1980/*.prt` | NIST NX 1980 CTC-01 native | — | — | Not imported; source for re-export | **Staged** |
| `vendor/fusion_simple_ap214.step` | Autodesk Fusion (Translation Framework v15.8) personal export | AP214 | mm | Deterministic single-body import | **In suite** |
| `vendor/onshape_extra_ap242.step` | Onshape by PTC 1.219 personal export | AP242 Ed. 2 | m | Deterministic single-body import | **In suite** |
| `vendor/solidworks_nist_ctc01_ap214.step` | SolidWorks 2026 online trial (SwSTEP 2.0) from NIST CTC-01 native | AP214 | mm | Deterministic single-body import | **In suite** |
| `vendor/nx_nist_ctc01_*.step` | *needs NX export from native above* | AP214/242 | TBD | Record identity + topology | **You retrieve** |

NIST STEP packs intentionally remove which CAD system wrote the file. Do **not**
rename NIST STEP fixtures as `solidworks_*` / `nx_*` — use the native sources +
a real export for those slots.

## Adding a vendor fixture

1. Export a **single-part** solid (no assembly occurrence graph).
2. Record exporter product + version, schema, declared units, and expected
   bounding-box extents (metres) in this table.
3. Drop the file under `test-geometry/corpus/vendor/` (git-lfs if large).
4. Add a Rust test in `src/cad_step.rs` locking units, triangle identity across
   two parses, and topology/voxel gates (or the honest failure mode).
5. If Truck leaves open seams or fails tessellation on a valid closed solid,
   keep the failure visible — that evidence gates OCCT slice 3.

## Face identity / assemblies

Bridge protocol (stub, no OCCT): `list_occurrences`, occurrence transforms, and
per-triangle `bridge_face_id` fields are covered in `src/cad_bridge.rs` unit
tests. Host remap fail-closed rules live in `src/cad_identity.rs`. Real vendor
assemblies remain out of scope until occurrence transforms come from an OCCT
backend and vendor fixtures fill the slots above.

## Non-goals

- Silent healing.
- Treating assemblies as one body.
- Shipping OCCT because a slot is empty — empty slots are not a ceiling.
- Claiming a NIST STEP file is a SolidWorks/NX export when NIST scrubbed that metadata.
