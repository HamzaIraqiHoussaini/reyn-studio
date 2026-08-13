# ParaView VTK field-export smoke

Version-pinned release smoke for Reyn Studio's legacy VTK `STRUCTURED_GRID`
engineering-field export (`src/vtk_export.rs`).

## Pin

| Item | Value |
|---|---|
| ParaView | **6.0.1** (exact; see `pin.json`) |
| VTK | **9.4.*** prefix from that ParaView build |
| Fixture | `fixture.vtk` (3³ deterministic exporter fixture) |

Release gates must run with `--require-paraview` against a 6.0.1 install.
Ordinary CI still validates the golden fixture against the Rust exporter and
parses the ASCII VTK without ParaView.

## Run

Structural + golden checks (no ParaView required):

```bash
python3 scripts/paraview_vtk_smoke.py
```

Full ParaView reader smoke (fail-closed if missing or wrong version):

```bash
python3 scripts/paraview_vtk_smoke.py --require-paraview
```

Optional local override when iterating on a newer ParaView:

```bash
python3 scripts/paraview_vtk_smoke.py --require-paraview --allow-version-mismatch
```

Point at a specific `pvpython`:

```bash
python3 scripts/paraview_vtk_smoke.py --require-paraview \
  --pvpython "/Applications/ParaView-6.0.1.app/Contents/bin/pvpython"
```

## Regenerate fixture

When the exporter fixture changes intentionally:

```bash
REYN_WRITE_PARAVIEW_VTK_FIXTURE=1 cargo test --bin reyn-studio \
  golden_paraview_smoke_fixture_matches_exporter
```

Then re-run the Python smoke and commit `fixture.vtk` with the exporter change.
