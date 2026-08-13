# Feature gap audit — customization and day-one essentials

**Research date:** 2026-07-24
**Scope:** (1) how incumbent CAD/CFD products handle customization; (2) what a professional engineer expects on day one of any credible flow-analysis tool, graded against Reyn Studio's shipped code; (3) a prioritized, model-honest roadmap.
**Evidence policy:** Competitor behavior is sourced from vendor documentation checked on the research date and linked inline. Reyn behavior is cited as `file:line` against the repository as inspected on 2026-07-24 (post-N6 shell: case-centered IA, native menu bar, command palette, status bar). Statements labeled **Reyn recommendation** are synthesis. Nothing below is a claim that Reyn should reach feature parity with incumbents; the PRD's non-goals (PRD.md §1.2) stand.

Companions: [`CFD_APP_LANDSCAPE.md`](CFD_APP_LANDSCAPE.md) (lifecycle/CAD strategy), [`DESIGN_OVERHAUL.md`](DESIGN_OVERHAUL.md) (visual system), [`../PRD.md`](../PRD.md) (canonical contract). This report does not duplicate them; it audits what neither covered: the customization surface and the day-one essential-feature floor.

---

## Executive findings

1. **Customization in mature CAD/CFD tools is not a preferences page; it is a three-layer architecture.** Every incumbent separates (a) global application options, (b) per-document/per-case authoritative settings, and (c) reusable templates/defaults that seed new documents. SolidWorks makes the split explicit (System Options vs Document Properties saved into templates); Ansys Discovery scopes simulation units "only to new documents"; NX layers Site → Group → User customer defaults; Workbench keeps per-project unit systems above a global default. Reyn has layer (a) only (`src/settings.rs`) and an implicit layer (b) in the case contract (`src/engineering.rs`); layer (c) does not exist.

**Status refresh (0.2.0, feature/step-import):** R2 cancel/progress, R3 horizon playback, R4 body orientation, R7 fluid presets/templates, and R8 display units/precision are **SHIPPED**. R1 viewport nav is **SHIPPED** including axis triad and Reyn/SolidWorks/Fusion/ParaView schemes. R5 engineering report is **SHIPPED** for HTML + PNG/PDF lab sheets (optional Ed25519 sidecar when a Keychain key is configured). R6 viewport capture is **SHIPPED** with provenance footer. R9 has surface probe, Cp mid-line profile, and force-vs-variant Cd/Cs/Cl bars **SHIPPED**. R10 colormap presets are **SHIPPED** for app settings + case `view_state` reopen. STEP is **PARTIAL** (single-part Truck + async import worker + multi-shell pick-one; assemblies/OCCT bridge still open). 3MF is **SHIPPED** for Core mesh+units. Model-field streamlines are **SHIPPED** on Results; ABC demo remains sandbox-quarantined. Named regions, small run queue, eng lab-sheet signing, Results Run detail, engineering-result digest signing, and determinate CAD stage progress are **SHIPPED** under Scope A. Semigroup self-consistency stays in the Developer Research Sandbox only — not on the engineering CAD hot path or customer Results rail. STEP/OCCT depth: slice 1 stub bridge + framing tests landed (`reyn-cad-bridge`, `src/cad_bridge.rs`); Truck remains the import path; assemblies/OCCT backend still open.

2. **Competitor-scheme mouse emulation is a category-standard feature, not a nicety.** Onshape ships SolidWorks/NX/Creo/AutoCAD view-manipulation presets; Fusion ships SolidWorks/Inventor/Alias/Tinkercad/PowerMill presets; Discovery and ParaView expose full per-button rebinding. The vendors treat orbit muscle memory as a switching cost to be neutralized. Reyn now ships Reyn / SolidWorks / Fusion / **ParaView** schemes with pan, fit, and standard views.

3. **Numeric display precision is a first-class engineering setting everywhere.** SolidWorks exposes 0–8 decimal places per document down to tolerance precision; Discovery exposes significant digits; even Fluent's colormap has `number-format-precision` and `number-format-type` preferences. Reyn hardcodes every format string (`{:.5}`, `{:.3e}` in `src/app.rs:1753-1848`).

4. **Units are the sharpest gap between Reyn's input honesty and its output inflexibility.** Reyn's per-case source-unit confirmation gate (`src/engineering.rs:141-143`, N5X-CAD-02) is *stronger* than the category norm on the input side. But every output is fixed SI (`m/s`, `kg/m³`, `Pa·s`, `N`, `N·m` suffixes at `src/app.rs:1444-1509`, `1770-1818`). No incumbent ships a single hardwired display-unit system; a US aero/automotive engineer expects lbf/psf/in on demand.

5. **The day-one essentials audit originally found the solve interaction weakest.** The 0.1.1 corrective release provides cancellation with persisted `Cancelled` attempts and retry, horizon playback, 3D probe, viewport image export, and engineering-case report; later Scope A work adds sidecar stage progress (prepare → develop → predict → recover) with an honest stage-based fraction.

6. **0.1.1 uses the existing request identity and warmup cache.** Horizon previews remain display-only; cancellation terminates the blocking sidecar, persists the terminal attempt, and replaces the engine before retry.

7. **Streamlines honesty (resolved on Results):** the ABC analytic field remains reachable only from the Research Sandbox. Engineering Results streamlines advect the model velocity volume and are labeled `MODEL · streamlines from predicted velocity` (`src/viewport.rs` model-streamline path).

8. **What Reyn should not copy:** per-seat customization sprawl (NX's hundreds of customer defaults), a shortcut editor before there are enough commands to warrant one, or unit systems that silently re-interpret stored numbers. Every customization Reyn adds must be a *display or default* concern; the recorded evidence (SI values, exact contract, hashes) stays canonical.

---

## 1. Customization deep-dive

### 1.1 Product-by-product: where customization lives

| Product | Global options | Per-document / per-case | Templates & org defaults | Sourced behavior (checked 2026-07-24) |
|---|---|---|---|---|
| **SolidWorks** | System Options tab (application-wide; e.g. decimal separator, colors, performance) | Document Properties tab: unit system (MKS/CGS/MMGS/IPS/custom), decimal places 0–8 per quantity, drafting standard, precision — all stored *in the file* | Document Properties saved into part/assembly/drawing **templates**; Settings Wizard exports System Options + toolbars + shortcuts + mouse gestures as `.sldreg` for other machines/users | [System Options vs Document Properties](https://help.solidworks.com/2025/english/SolidWorks/sldworks/HIDD_OPTIONS_SYSTEM_GENERAL.htm), [Document units & precision](https://help.solidworks.com/2024/english/SolidWorks/sldworks/hidd_options_units_new.htm), [Units and dimension standard / default templates](https://help.solidworks.com/2023/english/SolidWorks/sldworks/hidd_units_dim_std.htm), [Settings Wizard](https://support.hawkridgesys.com/hc/en-us/articles/360032797831-Save-Restore-or-Reset-SOLIDWORKS-Settings) |
| **Fusion (360)** | Preferences under the profile menu: language, orientation (Y-up/Z-up), graphics, **Pan/Zoom/Orbit preset** (Fusion, SolidWorks, Inventor, Alias, Tinkercad, PowerMill), default units per workspace (Design / CAM / Simulation) | Document Settings → Change Active Units on the open design; "Set as default" checkbox promotes the choice globally | Default-units preference seeds all new designs | [Pan/zoom/orbit presets](https://www.autodesk.com/products/fusion-360/blog/quick-tip-pan-zoom-orbit-preferences/), [default vs active units](https://autocadeverything.com/how-to-change-units-in-fusion-360/), [preferences tour](https://vdci.edu/learn/cad/how-to-edit-user-preferences-in-fusion-360) |
| **Siemens NX** | Customer Defaults dialog spanning thousands of settings | Part-file settings inherit from defaults at creation | **Three administered levels — Site (`UGII_SITE_DIR`), Group (`UGII_GROUP_DIR`), User — as `.dpv` files; higher levels can lock settings against lower-level override** | [Defaults levels](https://www.swooshtech.com/2023/04/06/how-to-define-the-defaults-level-of-customer-defaults/), [Siemens community on site/group `.dpv` mechanics](https://community.sw.siemens.com/s/question/0D54O00006LW6hiSAD/basics-of-setting-group-customer-defaults-and-site-customer-defaults) |
| **Ansys Fluent** | Preferences dialog (File > Preferences): case-independent; appearance/dark theme, application font size, default colormap, colormap levels, number format type/precision, ruler/axes — stored in `~/.fluentconf/<version>/preferences` | Quantity units are case-level (Workbench explicitly does **not** pass its unit systems into Fluent); local in-session display overrides are not retained beyond the session | Journals/case templates and PyFluent scripting carry reusable setups | [Preferences](https://ansyshelp.ansys.com/public/Views/Secured/corp/v252/en/flu_ug/flu_ug_gui_preferences.html), [GUI customization incl. dark theme and colormap fonts](https://ansyshelp.ansys.com/public/Views/Secured/corp/v252/en/flu_ug/flu_ug_cx_gui_customize.html), [TUI `preferences/graphics/colormap-settings` incl. `number-format-precision`](https://ansyshelp.ansys.com/public/Views/Secured/corp/v252/en/flu_tcl/flu_tui_preferences.html), [Workbench units are not passed to Fluent](https://ansyshelp.ansys.com/public/Views/Secured/corp/v252/en/wb2_help/wb2h_workingwithunits.html) |
| **Ansys Discovery** | File > Settings: General, **Navigation (rebindable spin/pan/zoom mouse mapping, zoom direction, spin center)**, Units and Display Precision, Physics, Results, Customize | Model unit system per document from the status bar; **"Any changes to the Simulation Units and Display Precision settings apply only to new documents"** — the per-document/new-document split is explicit; significant-digits display setting | Settings act as new-document seeds | [Settings overview](https://ansyshelp.ansys.com/public/Views/Secured/corp/v242/en/discovery/UDA/user_manual/environment/topics/c_settings_general.html), [Units and display precision](https://ansyshelp.ansys.com/public/Views/Secured/corp/v252/en/discovery/UDA/user_manual/environment/topics/r_settings_units.html), [Navigation mapping](https://ansyshelp.ansys.com/public/Views/Secured/corp/v252/en/discovery/UDA/user_manual/environment/topics/r_settings_navigation.html) |
| **Ansys Workbench** | Units menu: predefined systems (SI, US Customary, …) plus **user-defined custom unit systems**, import/export as XML | Active-project unit system vs default unit system are separate choices | Custom unit systems are shareable files | [Configuring units in Workbench](https://ansyshelp.ansys.com/public/Views/Secured/corp/v252/en/wb2_help/wb2h_workingwithunits.html) |
| **SimScale** | Minimal global customization: the workbench is one fixed web UI; no user theme, no rebindable navigation documented | Everything lives in the per-simulation tree; analysis type constrains which settings exist at all; units are contextual per field | Materials can be saved as reusable "user materials"; projects copied as templates | [Platform/workbench](https://www.simscale.com/docs/platform/), [simulation setup](https://www.simscale.com/docs/simulation-setup/), [user materials](https://www.simscale.com/docs/simulation-setup/materials/) |
| **ParaView** (OpenFOAM's de-facto GUI) | Settings dialog: General, **Camera tab = full per-mouse-button × modifier rebinding for 3D and 2D interaction separately**, Render View, Color Palette; all persisted as `ParaView-UserSettings.json` in the home directory (site-level JSON also supported) | Per-source property defaults savable ("save current settings as defaults"); per-array colormap defaults | **Any colormap + opacity function can be saved as the application default or as the default for arrays of a given name**; preset manager imports/exports presets as files | [Customizing ParaView / settings JSON / Camera tab](https://docs.paraview.org/en/v6.0.1/ReferenceManual/customizingParaView.html), [color map defaults & presets](https://docs.paraview.org/en/latest/ReferenceManual/colorMapping.html) |
| **Onshape** | Account-level Preferences: language, default units, **View manipulation presets (Onshape, SolidWorks, NX, Creo, AutoCAD) plus scroll-direction reverse**, customizable keyboard shortcuts, customizable shortcut toolbars | Workspace units per document | **Company/Enterprise-level preference administration** mirrors the user page | [My Account Preferences (last updated 2026-07-01)](https://cad.onshape.com/help/Content/Plans/my_account_preferences.htm), [view-manipulation tech tip](https://www.onshape.com/en/resource-center/tech-tips/tech-tip-changing-rotate-pan-and-zoom) |

### 1.2 What Reyn's customization surface is today

The complete persisted settings inventory is `AppSettings` (`src/settings.rs:58-79`): compute device, Python path, research checkout, project directory, autosave interval, theme (Instrument Dark / Instrument High Contrast, `src/settings.rs:40-54`), reduced motion, telemetry (forced off on load, `src/settings.rs:117-119`), Developer Research Sandbox, and the Ed25519 signing-key reference. Settings persist as human-readable JSON at a documented per-OS path with atomic writes (`src/settings.rs:237-260`, `777-786`) and legacy-field-tolerant deserialization (`src/settings.rs:832-845`).

Case-level "settings" are the operating point and preflight of the active external-flow case (`src/engineering.rs:95-196`, `208-437`) — authoritative, revisioned, and recorded into run evidence. This is the correct skeleton for the industry-standard three-layer model; the layers are just sparsely populated:

- **No display-unit system.** Source length unit is per-case and gated (`src/engineering.rs:44-91`); every displayed output is fixed SI (`src/app.rs:1444-1509`, `1763-1848`).
- **No precision/formatting control.** All format strings are hardcoded.
- **No navigation options.** Orbit = primary-drag, zoom = scroll with fixed sensitivity and clamp 2.4–9.5; there is no pan at all (`src/viewport.rs:227-237`). "Reset camera" exists only as a menu command (`src/app.rs:2901`).
- **No keyboard customization.** Fixed set: ⌘N/O/S/⇧⌘S/W/Q (`src/app.rs:4991-5005`), ⌘K palette (`src/app.rs:2980`), native menu accelerators (`src/menubar.rs:59-64`). No editor; acceptable for now given ~15 commands.
- **No templates or new-case defaults.** Every case starts from `OperatingPoint::default()` (`src/engineering.rs:106-118`) — air-like density/viscosity, unknown units, horizon 4.
- **No colormap or legend-range options.** One hardwired blue→ember→gold ramp (`src/viewport.rs:195-207`); section legends are calibrated but not user-rangeable (`src/app.rs:10684`).
- **No export defaults.** Every export opens a fresh file dialog with a fixed suggested name (`src/app.rs:2846-2852`, `7100-7105`).
- **Workspace layout:** side panels are resizable within fixed ranges (`src/app.rs:3457-3460`, `3591-3594`); no saved layouts, no multi-viewport.

### 1.3 The patterns that matter, applied to Reyn

Priority meanings follow the PRD: **P0** credibility-blocking for the external-flow release audience; **P1** next coherent capability; **P2** post-v1; **Reject** = deliberately not adopted.

| # | Pattern (category evidence) | Applies to Reyn? | Priority | Notes |
|---|---|---|---|---|
| C1 | **Global default + per-document authoritative units** (SolidWorks, Discovery, Workbench, Fluent-per-case) | Yes — add a *display* unit system (SI / US customary) as a global default with per-project override, converting presentation only. Stored evidence and exports keep SI + exact contract; the FEA CSV schema (`src/engineering.rs:863-885`) is untouched or gains explicit unit columns, never silent reinterpretation. | **P1 (high)** | Reyn's input-unit gate already exceeds the category norm; the gap is one-directional (output). |
| C2 | **Precision / significant-digits control** (SolidWorks per-document decimals, Discovery significant digits, Fluent colormap `number-format-precision`) | Yes — one significant-digits setting applied through a single formatting helper replacing the scattered `format!` calls in `measure_row`/`diag` call sites. | **P1** | Cheap once formatting is centralized; also fixes inconsistent 3/5/6-decimal mixing visible in the loads table (`src/app.rs:1753-1848`). |
| C3 | **Mouse-scheme presets emulating competitors** (Onshape 4 schemes, Fusion 6, Discovery/ParaView rebinding) | Partially. First close the *absolute* gap: pan, zoom-to-fit, standard views. Scheme presets ("Match SolidWorks / Fusion / ParaView") come after, as one enum setting mapping button/modifier → orbit/pan/zoom in `viewport::show`. | **P0 for pan/fit/views; P2 for presets** | A CFD reviewer who cannot pan or snap to +X view will not reach minute two. |
| C4 | **Keyboard shortcut editor** (SolidWorks Customize, Onshape account shortcuts) | Not yet. Command surface is ~15 actions and the ⌘K palette (`src/app.rs:2979-3100`) already provides discoverability. Revisit if the command count triples. | **P2 / defer** | |
| C5 | **Templates / new-document defaults** (SolidWorks document templates, NX customer defaults, STAR `.simt`, SimScale user materials) | Yes — (a) fluid presets (Air 15 °C, Air 25 °C, Water 20 °C) for the operating point; (b) "save as case template" capturing operating point + preferred views under a versioned schema. Aligns with the already-planned REQ-P-REF-01 "versioned supported case templates" (PRD §9.4). | **P1** | Templates must record their schema version and never bypass the readiness gates (`src/engineering.rs:520-536`). |
| C6 | **Org-level administered defaults** (NX Site/Group, Onshape enterprise preferences) | Not now. Local-first single-seat product. The JSON settings file (`src/settings.rs:237-260`) is already scriptable for teams that care. | **P2 / reject for v1** | |
| C7 | **Appearance / colormap defaults** (ParaView save-as-default per array, Fluent default colormap + levels) | Yes, narrowly: 2–3 vetted scientific colormaps (current instrument ramp + a colorblind-safe perceptually-uniform option) selectable per quantity class, plus manual legend range lock recorded in the case's view definitions so evidence reopens identically (N6-PROJ-02). | **P1** | Guardrail: colormap identity must be recorded in exported evidence; PRD §3.1 calibrated-scale doctrine holds. |
| C8 | **Export defaults** (SolidWorks export options; Fluent autosave cadence) | Minor: remember last export directory per kind; keep provenance-bearing default filenames. | **P2** | |
| C9 | **Settings portability** (SolidWorks `.sldreg` wizard, ParaView user/site JSON) | Mostly already satisfied by the documented JSON file + `REYN_STUDIO_CONFIG_DIR` override (`src/settings.rs:238-240`). Add a "Reveal settings file" affordance; skip a wizard. | **Done-ish / P2** | |
| C10 | **Workspace layouts / docking** (ParaView layouts, Fluent window arrangements) | No. Panels are resizable (`src/app.rs:3457-3460`) and the IA is deliberately fixed (PRD §3.2 anti-pattern: no app-to-app fragmentation). Persist panel widths; stop there. | **Reject beyond width persistence** | |

**Reyn recommendation.** Implement C1+C2 as one "Units & precision" settings section plus a per-project override, C3's absolute-navigation floor immediately, and C5 fluid presets in the case rail. Everything customization-related must obey one invariant: *customization changes presentation and defaults, never the recorded contract.* That invariant is what lets Reyn add these without endangering N5X-EV-01/N6-PROJ-02.

---

## 2. Essential-feature checklist

Grades: **SHIPPED** (works today, evidence cited) · **PARTIAL** (exists with material limits) · **MISSING** (absent) · **N/A-BY-DESIGN** (deliberately excluded per PRD, with the reason). "Evidence" cites the current repo; PRD acceptance IDs are noted where the repo and PRD §4.1 record them as passed.

### 2.1 Geometry import & preflight

| Item | Grade | Evidence / gap |
|---|---|---|
| STL import, binary + ASCII | SHIPPED | `src/cad.rs:1-9` parser; landing action `src/app.rs:2193-2204` |
| Source identity (bytes, SHA-256, revision) | SHIPPED | `src/engineering.rs:209-210`; revision IDs in case header `src/app.rs:2214-2231` |
| Topology diagnostics (watertight, degenerate, non-manifold, components, winding) | SHIPPED | `src/cad.rs:32-121` (`diagnose_mesh`); surfaced in preflight spine `src/app.rs:2308-2316` |
| Unit declaration gate + transform approval with visible 4×4 | SHIPPED | `src/engineering.rs:141-143`, `385-391`; 4×4 display `src/app.rs:1311-1325`; approval checkbox `src/app.rs:1341-1346` (N5X-CAD-01/02 per PRD §4.1) |
| Voxel adequacy gates (empty, clearance, cells-across, disconnected) + named waivers | SHIPPED | `src/engineering.rs:344-437`; waiver UI `src/app.rs:1583-1607` (N5X-CAD-03) |
| STEP/IGES/neutral B-rep | PARTIAL | 0.1.2+: single-part STEP via Truck + async import worker + multi-shell pick-one; assemblies with occurrence transforms, IGES, and healing remain open. OCCT out-of-process bridge slice 1: `reyn-cad-bridge` stub + framing/IPC tests (`src/cad_bridge.rs`); not yet on the import path; no OCCT linked (`docs/OCCT_BRIDGE_SPIKE.md`, `docs/occt_bridge_protocol.v1.json`). 3MF Core is SHIPPED. |
| User-controlled placement/orientation (incl. angle of attack) | SHIPPED | Body α/β/roll re-voxelization with off-thread orientation worker; stream remains +X (model contract). |
| Geometry repair | N/A-BY-DESIGN | PRD §8.1: Reyn "does not silently repair source geometry"; waivers + external repair instead |
| Named regions / face selections | PARTIAL | Operator-authored labels on structural candidates persist with the case (`named_regions`); stable CAD face IDs and internal-flow BC mapping remain open. |
| Reimport with revision lineage | PARTIAL | Managed STL revisions exist (PRD §8.1 "Implemented for STL revisions"); no assignment-mapping diff UI |

### 2.2 Case setup

| Item | Grade | Evidence / gap |
|---|---|---|
| Operating point with full validation (units, positivity, Re envelope 60–400, horizon ≤ model support) | SHIPPED | `src/engineering.rs:139-196`; live Reynolds + dynamic pressure readouts `src/app.rs:1512-1535` |
| Model compatibility filtering + support display | SHIPPED | Picker filters to 3D, grid-matched, geometry-conditioned obstacle checkpoints `src/app.rs:1363-1373`; `ModelSupport::validation` `src/engineering.rs:452-479` (N5X-CAD-04) |
| Fluid/material presets | SHIPPED | Air/Water presets + case templates seed operating points; gates still apply. |
| Case templates / duplicate-as-variant with defaults | SHIPPED | Named case templates (operating point + preferred view) save/apply/import/export via Settings and Case Setup; duplicate-as-variant keeps parent lineage. Templates seed drafts only — readiness gates still run. |
| Boundary-condition / turbulence-model editors | N/A-BY-DESIGN | PRD §1.2 non-goal; the locked contract is displayed instead (`src/app.rs:1420-1433`) |
| Internal-flow case kind | N/A-BY-DESIGN (blocked) | Reference-only contract, execution-blocked with structured blockers (`src/engineering.rs:697-736`); reference card in UI `src/app.rs:11454` |
| Staleness on contract edit | SHIPPED | Edits invalidate the draft result and notify; completed runs immutable (`src/app.rs:1629-1636`; N6-PROJ-03) |

### 2.3 Solve / run UX

| Item | Grade | Evidence / gap |
|---|---|---|
| Gated run with visible blocking reason (never a dead control) | SHIPPED | Single source of truth `run_gate_reason` `src/app.rs:2953-2974`; gated top-bar button `src/app.rs:3289-3312` |
| Asynchronous execution, responsive shell | SHIPPED | Engine channel + non-blocking drain (`src/app.rs:329-330`); PERF-AC-01 per PRD |
| Run start records exact contract | SHIPPED | `run_external_flow` commits the case revision before dispatch `src/app.rs:895-951`; exact contract JSON `src/engineering.rs:542-559` |
| Progress indication | SHIPPED | Sidecar streams stage frames during `predict_cad` (prepare → develop → predict → recover); Case Setup and status bar show stage index/count, detail, and a stage-based progress bar plus elapsed time + Cancel. Fraction is ordinal/stage-local, not wall-clock. |
| **Cancellation** | SHIPPED | Cancel terminalizes the immutable attempt, terminates the blocking sidecar, replaces the engine, and exposes retry after readiness; stale correlated responses are ignored |
| Queueing / batch runs | PARTIAL | Small follow-on FIFO (`RunQueue`) drains after the in-flight attempt; no parameter sweeps (P-SWEEP-01 remains post-N6). |
| Run history with immutable lineage + deep links | SHIPPED | Run ledger `src/app.rs:2695-2729`; rerun-with-parent (N6-PROJ-04) |
| Run log / stop-reason inspection | SHIPPED | Results Run detail card shows stop reason, runtime, and warnings for the selected attempt; Evidence run ledger deep-links every stored attempt. A dedicated scrolling multi-run console remains optional polish. |

### 2.4 Post-processing

| Item | Grade | Evidence / gap |
|---|---|---|
| Applicability verdict before numbers | SHIPPED | Banner first in Results (`src/app.rs:1730-1743`) |
| Integrated quantities table with source-class chips and units | SHIPPED | Force/moment coefficients, physical forces/moments, Cp range, area, pressure share, wake deficit — each row tagged DERIVED/MODEL/RECOVERED. Divergence RMS stays in evidence JSON only (not Results / lab sheets / variant scalars). |
| Cp with recorded reference state | SHIPPED | Physical Cp from engine with p∞, ρ∞, V∞ recorded (`src/engine.rs:178`, `src/app.rs:1800-1813`; N5X-PHYS-01 scope per PRD §4.2) |
| 3D volume + Cp surface + clipping planes | SHIPPED | Layers + clip sliders `src/app.rs:1889-1908`; wgpu raymarch `src/viewport.rs:242-267` |
| Geometry-linked 2D sections with calibrated legend + probe | SHIPPED | Section axis/quantity controls `src/app.rs:1910-1958`; legend `src/app.rs:10684`; section probe `src/app.rs:10743` |
| Load/suction hotspots | SHIPPED | Billboarded markers (`src/viewport.rs:44-49`, layer toggle `src/app.rs:1907`) |
| **3D point probe** (click → local velocity/Cp/traction) | SHIPPED | Surface probe on the engineering viewport with source-class chips; section probe remains. |
| **Streamlines on the model field** | SHIPPED | Results path advects model velocity (`model_streamline_*`); ABC analytic field stays sandbox-quarantined. |
| XY plots (Cp vs. position, force vs. variant) | SHIPPED | Cp mid-line profile under section view; force-coefficient parent/current grouped bars (Cd/Cs/Cl) above the shared-unit scalar delta table on Results. |
| **Horizon/time scrubbing within model support** | SHIPPED | `HorizonPlayback` fetches/caches per-step model fields for scrubbing and play-at-reading-rate; recorded run evidence stays immutable (`request_horizon_step` / `show_horizon_step` in `src/app.rs`). |
| Convergence/confidence indicators | PARTIAL | Warnings + applicability banner on the customer path; honest "spatial error unavailable without reference" notice remains. Divergence RMS and semigroup stay out of customer Results/lab sheets (evidence JSON / Developer Research Sandbox only). Solver-reference RelL2 for CAD remains unavailable. |
| Colormap / legend range control | SHIPPED | Ember/Viridis/Magma + Auto/Pinned Cp range in settings; persisted on case `view_state` for reopen. |
| Shared-scale run/variant comparison | SHIPPED | Shared-unit parent/current comparison with evidence deep links (`src/app.rs:1850-1887`; N6-COMP-01 candidate per PRD §4.1) |

### 2.5 Reporting & export

| Item | Grade | Evidence / gap |
|---|---|---|
| FEA surface-load CSV with full provenance columns | SHIPPED | Schema-versioned, provenance-validated, source-frame-mapped (`src/engineering.rs:834-887`; export flow `src/app.rs:2757-2871`; N5X-LOAD-03) |
| Benchmark evidence: canonical JSON + PNG/PDF + Ed25519 signed sidecars | SHIPPED (sandbox scope) | `src/benchmark_export.rs`; N5X-EXPORT-01 passed; signing slice per PRD §4.1 (production Keychain gate open) |
| **Engineering-case report (PDF/PNG)** | SHIPPED | HTML + PNG/PDF lab sheets from persisted engineering evidence (`src/engineering_export.rs`); optional Ed25519 sidecar when a Keychain key is configured. |
| **Viewport image export / screenshot with legend + provenance** | SHIPPED | Viewport PNG capture with provenance footer composition (`src/app.rs` screenshot worker). |
| Field export (VTK or equivalent) for external post-processing | SHIPPED (automated format/provenance gate + version-pinned ParaView smoke harness) | `src/vtk_export.rs` streams a legacy VTK `STRUCTURED_GRID` from the completed selected run's persisted `REYNENG1` blob, mapping coordinates and vectors into the approved source frame and embedding units, source classes, methods, transform, and source/case/run/model/field identity. Results and Evidence affordances live in `src/app.rs`; malformed, non-finite, stale/incomplete, unapproved, missing-content, and non-canonical inputs are rejected. Release smoke: `python3 scripts/paraview_vtk_smoke.py --require-paraview` against ParaView **6.0.1** (`docs/qa/paraview-vtk-smoke/`). |
| Diagnostics CSV | PARTIAL | `src/app.rs:7100-7118` exports procedural-particle diagnostics — sandbox-grade, not case evidence |

### 2.6 Project management

| Item | Grade | Evidence / gap |
|---|---|---|
| New/Open/Save/Save As, recents, autosave, crash recovery, migration | SHIPPED | Shortcuts `src/app.rs:4982-5136`; unsaved-changes guard `src/app.rs:5233+`; N6-PROJ-01..07 passed per PRD §4.1 |
| Portable content-addressed project bundle, read-only degraded reopen | SHIPPED | PRD §4.1 external-case row; read-only banner `src/app.rs:2629-2641` (N6-PROJ-05/06) |
| Staleness + immutable runs + evidence locking | SHIPPED | `src/app.rs:724+`, ledger `src/app.rs:2646-2729` |
| **Undo/redo for case edits** | SHIPPED | Bounded, transaction-coalesced history covers reversible Case Setup operating/preflight draft inputs (`CaseDraftHistory` in `src/engineering.rs`); ⌘Z / ⇧⌘Z (Ctrl equivalents), native Edit menu, and command-palette actions route restores through normal result invalidation/readiness while immutable source/model/run/evidence identity stays outside snapshots (`src/app.rs`, `src/menubar.rs`) |
| Project search / filtering | MISSING | Recents list only (`src/app.rs:4030+`); acceptable at current scale |

### 2.7 Collaboration & versioning

| Item | Grade | Evidence / gap |
|---|---|---|
| Local source/case/run revisioning | SHIPPED | Above |
| Signed, verifiable evidence bundle | SHIPPED | Benchmark reports and engineering-result digests sign with detached Ed25519 sidecars (`src/signing.rs`); lab sheets optionally sign on export; Evidence → Sign engineering result appends `engineering_signature` authenticity evidence. Organization-key trust / Keychain provisioning remain operator-side. |
| Sharing, comments, cloud sync, accounts | N/A-BY-DESIGN | PRD §1.2 / REQ-LOCAL-01; P-COLLAB-01 is deliberately P2 |

---

## 3. Prioritized roadmap

### 3.1 Blocked-by-model — not app gaps

The engine contract is fixed-body external incompressible flow: geometry-conditioned 3D velocity prediction over 1–`max_steps` horizon steps at the checkpoint's training grid, +X free stream, qualified Re 60–400, with recovered pressure → physical Cp, diffuse-interface traction, and integrated forces/moments (`src/engine.rs:172-200`; `src/engineering.rs:452-479`, `188-194`). The following engineer-expected features **cannot be honestly built on this model** and must stay labeled blocked-by-model, not scheduled as app work:

- **Internal/HVAC flow** — contract exists and is execution-blocked with structured blockers (`src/engineering.rs:697-736`); stays blocked until a distinct qualified internal model exists (P-INTERNAL-01).
- **Thermal / conjugate heat transfer, compressibility, multiphase** — outside the physics contract entirely.
- **Long transients, shedding spectra, animations beyond the horizon** — the model predicts ≤ `max_steps` H-step fields; anything longer is extrapolation. Playback *within* the horizon is supported physics (see R3); marketing it as transient CFD is not.
- **Arbitrary Reynolds number** — the 60–400 envelope gate (`src/engineering.rs:188-194`) is the model's qualification boundary, not a UI limitation.
- **Mesh refinement / grid-independence studies** — the voxel grid is the checkpoint's training grid (`src/engineering.rs:460-465`); resolution is a model property. A discretization study needs the external-reference path (P-VV-01).
- **Spatial error maps without a reference** — correctly refused today (`src/app.rs:1999-2004`); unblockable except by importing solver references (REQ-P-REF-01).
- **Moving/deforming bodies, rotating machinery** — fixed-body contract.
- **Non-+X free stream as a solver input** — solver contract is +X; but *body orientation relative to the stream* is app-feasible via geometry rotation before voxelization (R4 below). The distinction matters: the app gap is orientation, the model constraint is stream direction.

### 3.2 Ranking method

Rank = (engineer-credibility impact on day one of a J1 evaluation) × (feasibility given the shipped engine contract). Items that require new model qualification are excluded by §3.1. Items already covered by open PRD gates (packaging, Keychain signing, N5X-VV) are not re-ranked here.

### 3.3 Top 10 next actions

**R1 — Viewport navigation floor: pan, zoom-to-fit, standard views. (P0)**
`Camera` gains a `target: [f32; 3]` (currently only yaw/pitch/dist, `src/viewport.rs:12-25`); secondary-button or modifier-drag pans by offsetting the target in the view plane; a `fit(bounds)` method sets dist from the domain/geometry bounds. Add +X/−X/±Y/±Z/isometric snaps to the View menu (`src/menubar.rs`) and the ⌘K palette (`src/app.rs:2994-3001`), plus a small axis triad painted in the viewport corner (`src/app.rs:7346+`). Both `project_volume` and the CPU projector already consume the camera in one place each (`src/viewport.rs:57-90`, `277-297`), so target support is a contained change. This is the single cheapest credibility repair in the codebase.

**R2 — Run progress + cancellation. (Implemented in 0.1.1)**
The app records the attempt before dispatch, shows stage-based progress from sidecar frames (plus elapsed time), and offers Cancel. Cancel persists `Cancelled`, terminates the blocking sidecar, starts a fresh engine, and permits retry after readiness. Correlated stale responses cannot alter the retry.

**R3 — Horizon playback within model support. (P0)**
The engine caches solver warmup per mask so each horizon is one model pass (`src/engine.rs:142-144`). Extend `CadPredict` to return (or fetch per-step on demand) fields for steps 1..H; add a horizon scrubber to Results labeled with the honest vocabulary — "Model horizon step k of H" — and re-derive the displayed section/volume from the selected step. Loads in the measurement table stay pinned to the run's recorded horizon unless the user explicitly inspects per-step values (each labeled MODEL, per N5X-EV-02). Files: `src/engine.rs` protocol + `engine/` sidecar, `CadCase` field storage (`src/app.rs:38-54`), `controls_engineering_results`.

**R4 — Body orientation (angle of attack/yaw) in preflight. (P0)**
The model is geometry-conditioned; rotating the *body* before voxelization is inside the contract even though the stream is fixed +X. Add explicit rotation controls (yaw/pitch about the body centroid) to the Source & transform stage; compose the rotation into the recorded 4×4 (`src/engineering.rs:219`), re-run voxelization + preflight gates, and require re-approval (N5X-CAD-02 semantics unchanged). Display "flow-relative orientation" as declared case input so variants can sweep it (J4). Files: `src/cad.rs` (rotate mesh before auto-fit), `src/engineering.rs` (orientation field in `OperatingPoint` or preflight), `controls_engineering_case`. Combined with R3 this converts Reyn from "one canned pose" to a real what-if instrument — the highest setup-credibility item that needs zero new model capability.

**R5 — Engineering-case report export (PDF/PNG). (P1)**
The benchmark path already renders deterministic PNG/PDF from canonical data with hash lineage (`src/benchmark_export.rs`, N5X-EXPORT-01). Create `engineering_export.rs` on the same pattern: applicability verdict, operating point + Reynolds, preflight summary + waivers, loads table with source classes, one section image, lineage hashes, warnings. Derive strictly from the persisted run evidence (`ENGINEERING_RESULT_SCHEMA`, `src/engineering.rs:12`) so a report can be regenerated from a reopened project. Hook into the Results `Export…` menu (`src/app.rs:2030-2046`). This is the artifact an engineer forwards to a colleague — its absence is why demos currently end in a screenshot of the app.

**R6 — Viewport image capture with calibrated legend + provenance footer. (P1)**
Read back the wgpu target (or render offscreen at 2×) in `src/gpu.rs`, composite the legend, source-class chip, run ID and model hash into the image footer, and save PNG. Never export an unlabeled field picture — the footer is what distinguishes evidence from decoration (SCI-AC-01).

**R7 — Fluid presets + case templates. (P1)**
Preset table (Air 15 °C sea level, Air 25 °C, Water 20 °C) filling density/viscosity/reference pressure with the preset name recorded in the contract; "Save as template" persisting a versioned operating-point + view-definition snapshot in the project (and optionally app-level for new projects). Templates seed drafts only; every gate still runs (`src/engineering.rs:520-536`). Files: `src/engineering.rs` (preset consts + template struct), `controls_engineering_case`, `src/project.rs`. Fulfils the first slice of REQ-P-REF-01.

**R8 — Display units & precision. (P1)**
`src/units.rs`: a `DisplayUnits` enum (SI / US customary) + significant-digits setting in `AppSettings` with per-project override; one `format_quantity(value_si, kind)` helper adopted by `measure_row`/`diag`/section legend call sites. Presentation-layer only: manifests, evidence, FEA CSV stay SI; the CSV header already names units explicitly (`src/engineering.rs:863-865`), so add converted *additional* columns only if partners ask, never replace. This is pattern C1+C2 and unblocks US-market evaluations.

**R9 — 3D probe and Cp line plot. (P1)**
Click in the 3D viewport → ray-cast to the nearest surface/volume cell → pinned probe chip (velocity, pressure, Cp, |traction|) reusing the existing marker vocabulary (`src/viewport.rs:95-150`); and a section-line Cp extraction plotted as a small XY panel under the section view (data already lives in `SectionPlane`, `src/engineering_section.rs`). Every probe value carries its source class chip. Files: `src/viewport.rs` (picking), `controls_engineering_results`, `src/engineering_section.rs`.

**R10 — Colormap presets + legend range lock. (P1)**
Two additions, both recorded in the case's view definitions so reopening reproduces the exact image (N6-PROJ-02): (a) a second, colorblind-safe perceptually-uniform ramp selectable beside the instrument ramp — implement as a small LUT consulted by `colormap_rgb` (`src/viewport.rs:195-207`) and the raymarch shader in `src/gpu.rs`; (b) manual min/max lock on the section/surface legends with the active range printed on the legend (`src/app.rs:10684`, `10838`). Exported images (R5/R6) must name the colormap and range.

**Guardrails across all ten.** No item above adds a physics claim. Every new number carries a source class (N5X-EV-02); every new persisted view property enters the schema with a migration test (N6-PROJ-07); cancellation and playback write honest stop reasons and step labels; customization never rewrites stored SI evidence. The streamline path (`src/viewport.rs:442-475`) stays quarantined in the sandbox until it advects real model velocity — if that is ever built, it belongs after R9 and must interpolate `CadField.vel`, seed from user-placed rakes, and label the integrator.

---

## 4. Dated bibliography

All sources accessed **2026-07-24**.

### SolidWorks
- Dassault Systèmes, *System Options – General*, SOLIDWORKS 2025 Help: https://help.solidworks.com/2025/english/SolidWorks/sldworks/HIDD_OPTIONS_SYSTEM_GENERAL.htm
- Dassault Systèmes, *Document Properties – Units*, SOLIDWORKS 2024 Help: https://help.solidworks.com/2024/english/SolidWorks/sldworks/hidd_options_units_new.htm
- Dassault Systèmes, *Document Properties – Dimensions*, SOLIDWORKS 2025 Help: https://help.solidworks.com/2025/english/SolidWorks/sldworks/HIDD_OPTIONS_DIMS_New.htm
- Dassault Systèmes, *Units and Dimension Standard / Default Templates*, SOLIDWORKS 2023 Help: https://help.solidworks.com/2023/english/SolidWorks/sldworks/hidd_units_dim_std.htm
- Dassault Systèmes, *Customize Keyboard Shortcut Keys*, SOLIDWORKS 2025 Help: https://help.solidworks.com/2025/english/SWConnected/swdotworks/HIDD_CUSTOMIZE_KEYBOARD.htm
- Dassault Systèmes, *Mouse Gestures*, SOLIDWORKS 2021 Help via MySolidWorks: https://my.solidworks.com/reader/onlinehelp/2021%252Fenglish%252Fsolidworks%252Fsldworks%252Fc_mouse_sestures.htm/mouse-gestures
- Hawk Ridge Systems, *Save, Restore, or Reset SOLIDWORKS Settings (Settings Wizard)*: https://support.hawkridgesys.com/hc/en-us/articles/360032797831-Save-Restore-or-Reset-SOLIDWORKS-Settings

### Autodesk Fusion
- Autodesk, *How to Set Your Pan, Zoom, & Orbit Controls*, Fusion Blog: https://www.autodesk.com/products/fusion-360/blog/quick-tip-pan-zoom-orbit-preferences/
- VDCI, *How to Edit User Preferences in Fusion 360* (default units per Design/CAM/Simulation, orientation): https://vdci.edu/learn/cad/how-to-edit-user-preferences-in-fusion-360
- AutocadEverything, *Default vs active units in Fusion 360*: https://autocadeverything.com/how-to-change-units-in-fusion-360/

### Siemens NX
- Swoosh Technologies, *How To Define the Defaults Level of Customer Defaults* (Site/Group/User, `.dpv`, locking): https://www.swooshtech.com/2023/04/06/how-to-define-the-defaults-level-of-customer-defaults/
- Siemens community, *Basics of setting Group/Site Customer Defaults* (`UGII_SITE_DIR`/`UGII_GROUP_DIR` mechanics): https://community.sw.siemens.com/s/question/0D54O00006LW6hiSAD/basics-of-setting-group-customer-defaults-and-site-customer-defaults

### Ansys
- Ansys, *Setting User Preferences/Options*, Fluent User's Guide 2025 R2: https://ansyshelp.ansys.com/public/Views/Secured/corp/v252/en/flu_ug/flu_ug_gui_preferences.html
- Ansys, *Customizing the Graphical User Interface* (dark theme, colormap fonts), Fluent 2025 R2: https://ansyshelp.ansys.com/public/Views/Secured/corp/v252/en/flu_ug/flu_ug_cx_gui_customize.html
- Ansys, *TUI `preferences/`* (colormap default, levels, number-format precision/type), Fluent 2025 R2: https://ansyshelp.ansys.com/public/Views/Secured/corp/v252/en/flu_tcl/flu_tui_preferences.html
- Ansys, *Configuring Units in Workbench* (predefined + custom systems; units not passed to Fluent), 2025 R2: https://ansyshelp.ansys.com/public/Views/Secured/corp/v252/en/wb2_help/wb2h_workingwithunits.html
- Ansys, *Discovery Settings* overview: https://ansyshelp.ansys.com/public/Views/Secured/corp/v242/en/discovery/UDA/user_manual/environment/topics/c_settings_general.html
- Ansys, *Discovery — Units and Display Precision* ("apply only to new documents"; significant digits), 2025 R2: https://ansyshelp.ansys.com/public/Views/Secured/corp/v252/en/discovery/UDA/user_manual/environment/topics/r_settings_units.html
- Ansys, *Discovery — Navigation* (rebindable spin/pan/zoom): https://ansyshelp.ansys.com/public/Views/Secured/corp/v252/en/discovery/UDA/user_manual/environment/topics/r_settings_navigation.html

### SimScale
- SimScale, *Platform & Dashboard*: https://www.simscale.com/docs/platform/
- SimScale, *Simulation Setup*: https://www.simscale.com/docs/simulation-setup/
- SimScale, *Materials* (user materials as reusable defaults): https://www.simscale.com/docs/simulation-setup/materials/

### ParaView
- Kitware, *Customizing ParaView* (settings JSON, Camera interaction mapping tab, save-current-as-default), ParaView 6.0.1: https://docs.paraview.org/en/v6.0.1/ReferenceManual/customizingParaView.html
- Kitware, *Color maps and transfer functions* (save as default / per-array default, preset manager): https://docs.paraview.org/en/latest/ReferenceManual/colorMapping.html

### Onshape
- PTC/Onshape, *My Account – Preferences* (units, mouse controls incl. other-CAD presets, keyboard shortcuts, toolbars; last updated 2026-07-01): https://cad.onshape.com/help/Content/Plans/my_account_preferences.htm
- Onshape, *Tech Tip: Changing Rotate Pan and Zoom* (SolidWorks/NX/Creo/AutoCAD schemes): https://www.onshape.com/en/resource-center/tech-tips/tech-tip-changing-rotate-pan-and-zoom
- Onshape, *View Navigation and the View Cube*: https://cad.onshape.com/help/Content/View/view_navigation_and_the_view_cube.htm
