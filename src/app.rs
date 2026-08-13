//! Reyn Studio shell — matches the 3D Volumetric Analysis mockup. (egui 0.35 API.)
use crate::benchmark_evidence::{
    InspectorVariable, INSPECTOR_DERIVATIVE, INSPECTOR_DOMAIN, INSPECTOR_LAYOUT,
    INSPECTOR_PRESSURE, INSPECTOR_PROTOCOL_VERSION, INSPECTOR_SCHEMA,
};
use crate::field2d::{self, FieldVar};
#[cfg(target_os = "macos")]
use crate::menubar::{MenuBar, MenuCommand, MenuSignal, MenuSyncState};
use crate::signing::LocalSigningKeyStore;
use crate::theme::*;
use crate::{
    cad, cad_field_ready, engine, engineering, engineering_export, engineering_section, flow, gpu,
    library, painter, project, project_lifecycle, report, settings, signing, units, viewport,
    vtk_export,
};
use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Margin, Rect, RichText, Sense,
    Stroke, Vec2,
};
use egui_phosphor::regular as ph;
use sha2::{Digest, Sha256};

#[derive(PartialEq, Clone, Copy)]
enum Nav {
    Projects,
    Case,
    Results,
    Evidence,
    Models,
    Settings,
    // Permanently available only when Settings → Developer enables the sandbox.
    FlowPainter,
    Fields2D,
    Metrics,
    Benchmark,
}

/// Detail rails should describe a real object. Empty Results/Evidence states
/// own the center column instead of repeating the same absence in a second
/// panel (and leaving less room for the recovery action).
fn should_show_detail_rail(nav: Nav, has_case: bool, has_result: bool) -> bool {
    match nav {
        Nav::Models => false,
        Nav::Results => has_result,
        Nav::Evidence => has_case,
        _ => true,
    }
}

/// A Results destination is a scientific canvas only after a real engineering
/// result exists. Procedural placeholder particles belong to the research
/// sandbox and must never fill an empty engineering result.
fn uses_scientific_canvas(nav: Nav, has_result: bool) -> bool {
    match nav {
        Nav::Results => has_result,
        Nav::Metrics | Nav::Fields2D | Nav::FlowPainter | Nav::Benchmark => true,
        _ => false,
    }
}

/// Rail measurement rows stack below this width so labels, vector values,
/// units, and source chips never overprint one another.
fn measurement_row_stacks(available_width: f32) -> bool {
    available_width < 360.0
}

fn field2d_cache_signature(generation: u64, variable: FieldVar, reference_visible: bool) -> u64 {
    let variable_id = match variable {
        FieldVar::Velocity => 0,
        FieldVar::Vorticity => 1,
        FieldVar::Pressure => 2,
    };
    generation.wrapping_mul(131) ^ (variable_id << 1) ^ ((reference_visible as u64) << 4)
}

fn background_repaint_delays(
    live: bool,
    project_dirty: bool,
    autosave_blocked: bool,
    now_utc_unix: u64,
    next_autosave_utc_unix: u64,
) -> (Option<std::time::Duration>, Option<std::time::Duration>) {
    let live_delay = live.then_some(LIVE_REPAINT_INTERVAL);
    let autosave_delay = (project_dirty && !autosave_blocked).then(|| {
        std::time::Duration::from_secs(next_autosave_utc_unix.saturating_sub(now_utc_unix).max(1))
    });
    (live_delay, autosave_delay)
}

fn autosave_deadline_after_attempt(
    completed_utc_unix: u64,
    interval_seconds: u64,
    succeeded: bool,
) -> u64 {
    completed_utc_unix.saturating_add(if succeeded {
        interval_seconds.max(1)
    } else {
        5
    })
}

fn has_unsaved_project_work(
    document_dirty: bool,
    case_draft_dirty: bool,
    orientation_draft: Option<[f64; 3]>,
    orientation_pending: bool,
) -> bool {
    document_dirty || case_draft_dirty || orientation_draft.is_some() || orientation_pending
}

fn orientation_geometry_gate(
    operation: &str,
    orientation_draft: Option<[f64; 3]>,
    pending: Option<&PendingOrientation>,
) -> Option<String> {
    if let Some(pending) = pending {
        return Some(format!(
            "{operation} is blocked while body orientation request {} is re-voxelizing. Wait for its deterministic completion before using or saving geometry.",
            short_id(&pending.request_id)
        ));
    }
    orientation_draft.map(|_| {
        format!(
            "{operation} is blocked because the visible body orientation is not in the voxel mask. Apply it and wait for re-voxelization to complete."
        )
    })
}

fn project_mutation_rejection(
    access_mode: project_lifecycle::ProjectAccessMode,
    operation: &str,
) -> Option<String> {
    (access_mode == project_lifecycle::ProjectAccessMode::ReadOnlyEvidence).then(|| {
        format!(
            "{operation} is blocked while this project is in read-only evidence mode. Stored runs and evidence remain unchanged and inspectable."
        )
    })
}

fn is_project_write_conflict(error: &project_lifecycle::LifecycleError) -> bool {
    matches!(
        error,
        project_lifecycle::LifecycleError::Project(project::ProjectError::WriteConflict { .. })
    )
}

fn conflict_copy_path(
    original: &std::path::Path,
    now_utc_unix: u64,
    unique_suffix: &str,
) -> std::path::PathBuf {
    let parent = original
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let stem = original
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("project");
    let suffix: String = unique_suffix
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(8)
        .collect();
    parent.join(format!(
        "{stem} conflict {now_utc_unix} {}.reynproj",
        if suffix.is_empty() { "copy" } else { &suffix }
    ))
}

#[derive(Clone)]
struct ProjectWriteConflict {
    path: std::path::PathBuf,
    detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectConflictAction {
    Reload,
    SaveAs,
    ConflictCopy,
    Dismiss,
}

#[derive(Debug, PartialEq, Eq)]
enum ProjectConflictResolution {
    Reload(std::path::PathBuf),
    PromptSaveAs,
    SaveConflictCopy(std::path::PathBuf),
    Dismiss,
}

fn resolve_project_conflict_action(
    action: ProjectConflictAction,
    conflict_path: &std::path::Path,
    now_utc_unix: u64,
    unique_suffix: &str,
) -> ProjectConflictResolution {
    match action {
        ProjectConflictAction::Reload => {
            ProjectConflictResolution::Reload(conflict_path.to_path_buf())
        }
        ProjectConflictAction::SaveAs => ProjectConflictResolution::PromptSaveAs,
        ProjectConflictAction::ConflictCopy => ProjectConflictResolution::SaveConflictCopy(
            conflict_copy_path(conflict_path, now_utc_unix, unique_suffix),
        ),
        ProjectConflictAction::Dismiss => ProjectConflictResolution::Dismiss,
    }
}

/// The active external-flow case and its exact CAD/result data.
struct CadCase {
    mask: std::sync::Arc<Vec<f32>>,
    /// Geometry bounds are O(N³) to derive but change only with the voxel mask.
    /// Camera motion and ordinary paints reuse this exact cached result.
    mask_bounds: Option<([f32; 3], [f32; 3])>,
    model: String,
    steps: u32,
    surf: Option<gpu::SurfaceData>,
    /// Float mask identity used to build `surf.mask`. Retaining the Arc lets a
    /// displayed horizon step prove whether those bytes are reusable.
    surf_mask_source: Option<std::sync::Arc<Vec<f32>>>,
    name: String,
    workflow: engineering::ExternalFlowCase,
    velocity: Vec<f32>,
    pressure: Vec<f32>,
    cp: Vec<f32>,
    traction: Vec<f32>,
    result_grid: usize,
    /// Solver time between horizon steps, as reported by the run. Zero until a
    /// run completes, which is also the signal that physical time is unknown.
    dt_frame: f32,
    active_run_id: Option<String>,
    pending: bool,
    pending_request_id: Option<String>,
    pending_run: Option<PendingCadRun>,
    /// Horizon scrubbing state. Preview steps are display-only.
    playback: HorizonPlayback,
}

fn has_complete_fea_load_field(case: &CadCase) -> bool {
    let n = case.result_grid;
    let Some(cube) = n.checked_mul(n).and_then(|square| square.checked_mul(n)) else {
        return false;
    };
    !case.pending
        && n >= 3
        && case.active_run_id.is_some()
        && case.workflow.result.is_some()
        && case.workflow.source_revision_id.is_some()
        && case.workflow.case_revision_id.is_some()
        && case.workflow.model_sha256.is_some()
        && case.mask.len() == cube
        && case.cp.len() == cube
        && case.traction.len() == 3 * cube
}

#[derive(Clone)]
struct PendingCadRun {
    request_id: String,
    run_id: String,
    workflow: engineering::ExternalFlowCase,
    parent_run_id: Option<String>,
    started_utc_unix: u64,
    started_at: std::time::Instant,
    manifest: project::RunManifest,
    /// Latest stage frame from the sidecar (`progress: true`), if any.
    progress: Option<engine::CadProgress>,
}

/// Small FIFO of external-flow run requests (Phase 0.4). Only one executes at a
/// time; completions drain the next entry.
#[derive(Default)]
struct RunQueue {
    waiting: std::collections::VecDeque<QueuedRunRequest>,
}

/// Queued follow-on external-flow request (Phase 0.4). Only one run executes;
/// completions drain the next entry against the live case draft.
#[derive(Clone)]
struct QueuedRunRequest {
    case_id: String,
    queued_utc_unix: u64,
    note: String,
}

struct PendingOrientation {
    generation: u64,
    request_id: String,
    case_id: String,
    source_sha256: String,
    angles: [f64; 3],
    started_at: std::time::Instant,
    kind: PendingOrientationKind,
}

enum PendingOrientationKind {
    Draft,
    Hydrate(Box<PendingOrientationHydration>),
}

struct PendingOrientationHydration {
    workflow: engineering::ExternalFlowCase,
    selected_run_id: Option<String>,
    dt_frame: f32,
}

#[derive(Clone)]
struct PendingOrientationView {
    request_id: String,
    angles: [f64; 3],
    started_at: std::time::Instant,
}

impl PendingOrientation {
    fn mutates_project(&self) -> bool {
        matches!(&self.kind, PendingOrientationKind::Draft)
    }

    fn view(&self) -> PendingOrientationView {
        PendingOrientationView {
            request_id: self.request_id.clone(),
            angles: self.angles,
            started_at: self.started_at,
        }
    }
}

/// One fetched horizon step. Preview frames live in memory only: they are model
/// predictions the operator asked to look at, not the run that was recorded, so
/// they never enter the content store or the evidence chain.
struct HorizonFrame {
    n: usize,
    velocity: Vec<f32>,
    pressure: Vec<f32>,
    cp: Vec<f32>,
    traction: Vec<f32>,
    mask: std::sync::Arc<Vec<f32>>,
    force_coefficients: [f32; 3],
    cp_min: f32,
    cp_max: f32,
}

/// Playback over the model's prediction horizon for a completed case. Each step
/// is one engine request (the solver warmup is cached per mask, so a step costs
/// about one model pass) and every fetched step is cached for instant scrubbing.
struct HorizonPlayback {
    /// The horizon step currently on screen.
    step: u32,
    playing: bool,
    /// egui clock time of the last automatic advance.
    last_advance: f64,
    frames: std::collections::BTreeMap<u32, HorizonFrame>,
    /// In-flight fetch, as (step, request_id).
    fetching: Option<(u32, String)>,
    /// Steps whose fetch failed, so the view can say so instead of blanking.
    failed: std::collections::BTreeSet<u32>,
}

/// Bound on cached preview frames. A 64³ step is ~7 MB across five fields, so
/// this caps the preview cache at a few hundred MB in the worst case.
const HORIZON_CACHE_LIMIT: usize = 24;
/// Bound one UI frame's engine drain. Most messages are cheap, but a queued
/// burst must not monopolize input indefinitely; the wake bridge schedules the
/// continuation frame when this budget is exhausted.
const ENGINE_MESSAGES_PER_FRAME: usize = 8;
const LIVE_REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
/// Model-independent grid used only to inspect geometry before a qualified
/// model exists. Execution remains blocked until a verified model with this
/// grid is selected.
const DIAGNOSTIC_PREFLIGHT_GRID: usize = 64;

struct OrientationWorkRequest {
    generation: u64,
    request_id: String,
    case_id: String,
    source_sha256: String,
    source_name: String,
    angles: [f64; 3],
    grid: usize,
    source_bytes: Vec<u8>,
    selected_shell_entity_id: Option<u64>,
}

struct OrientationWorkResult {
    generation: u64,
    request_id: String,
    case_id: String,
    source_sha256: String,
    angles: [f64; 3],
    completed_utc_unix: u64,
    result: Result<cad::VoxelMask, String>,
}

struct OrientationWorker {
    request_tx: std::sync::mpsc::Sender<OrientationWorkRequest>,
    result_rx: std::sync::mpsc::Receiver<OrientationWorkResult>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OrientationResultDisposition {
    Apply,
    Failed,
    DiscardStale,
}

fn coalesce_orientation_requests(
    mut request: OrientationWorkRequest,
    request_rx: &std::sync::mpsc::Receiver<OrientationWorkRequest>,
) -> OrientationWorkRequest {
    while let Ok(newest) = request_rx.try_recv() {
        request = newest;
    }
    request
}

fn classify_orientation_result(
    completed: &OrientationWorkResult,
    pending: Option<&PendingOrientation>,
    current_case: Option<(&str, &str)>,
) -> OrientationResultDisposition {
    let Some(pending) = pending else {
        return OrientationResultDisposition::DiscardStale;
    };
    let current_request = pending.generation == completed.generation
        && pending.request_id == completed.request_id
        && pending.case_id == completed.case_id
        && pending.source_sha256 == completed.source_sha256
        && pending.angles == completed.angles;
    let current_context = match &pending.kind {
        PendingOrientationKind::Draft => current_case.is_some_and(|(case_id, source_sha256)| {
            pending.case_id == case_id && pending.source_sha256 == source_sha256
        }),
        PendingOrientationKind::Hydrate(_) => current_case.is_none(),
    };
    if !current_request || !current_context {
        return OrientationResultDisposition::DiscardStale;
    }
    if completed.result.is_ok() {
        OrientationResultDisposition::Apply
    } else {
        OrientationResultDisposition::Failed
    }
}

fn geometry_setup_engine_status(
    name: &str,
    preflight: &engineering::GeometryPreflight,
) -> String {
    let geometry = format!(
        "● {name}: {} triangles → {} solid voxels @ {}³",
        preflight.triangles, preflight.solid_voxels, preflight.target_grid
    );
    if preflight.ready() {
        format!("{geometry} · geometry accepted")
    } else if !preflight.transform_approved {
        format!("{geometry} · transform approval required")
    } else {
        format!("{geometry} · preflight incomplete")
    }
}

struct GeometryImportWorkRequest {
    generation: u64,
    request_id: String,
    path: std::path::PathBuf,
    source_name: String,
    source_bytes: Vec<u8>,
    grid: usize,
    selected_shell_entity_id: Option<u64>,
}

struct GeometryImportReady {
    imported: cad::GeometryImport,
    diagnostics: cad::MeshDiagnostics,
    voxel: cad::VoxelMask,
}

enum GeometryImportFailure {
    Message(String),
    ChooseShell(crate::cad_step::ShellChoiceRequired),
}

struct GeometryImportWorkResult {
    generation: u64,
    request_id: String,
    path: std::path::PathBuf,
    source_name: String,
    source_bytes: Vec<u8>,
    source_sha256: String,
    #[allow(dead_code)]
    selected_shell_entity_id: Option<u64>,
    outcome: Result<GeometryImportReady, GeometryImportFailure>,
}

struct GeometryImportWorker {
    request_tx: std::sync::mpsc::Sender<GeometryImportWorkRequest>,
    result_rx: std::sync::mpsc::Receiver<GeometryImportWorkResult>,
}

struct CadFieldReadyWorkRequest {
    field: engine::CadField,
    identity: cad_field_ready::CadFieldReadyIdentity,
}

struct CadFieldReadyWorkResult {
    request_id: String,
    outcome: Result<cad_field_ready::CadFieldReady, String>,
}

struct CadFieldReadyWorker {
    request_tx: std::sync::mpsc::Sender<CadFieldReadyWorkRequest>,
    result_rx: std::sync::mpsc::Receiver<CadFieldReadyWorkResult>,
}

struct PendingCadFieldReady {
    request_id: String,
    #[allow(dead_code)]
    started_at: std::time::Instant,
}

impl CadFieldReadyWorker {
    fn spawn(repaint_context: Option<egui::Context>) -> Result<Self, String> {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<CadFieldReadyWorkRequest>();
        let (result_tx, result_rx) = std::sync::mpsc::channel::<CadFieldReadyWorkResult>();
        std::thread::Builder::new()
            .name("reyn-cad-field-ready".into())
            .spawn(move || {
                while let Ok(request) = request_rx.recv() {
                    let request_id = request.field.request_id.clone();
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cad_field_ready::prepare_cad_field_ready(request.field, request.identity)
                    }))
                    .unwrap_or_else(|_| {
                        Err("CAD field preparation worker panicked while encoding the completed field".into())
                    });
                    let _ = result_tx.send(CadFieldReadyWorkResult {
                        request_id,
                        outcome,
                    });
                    if let Some(ctx) = &repaint_context {
                        ctx.request_repaint();
                    }
                }
            })
            .map_err(|error| format!("CAD field preparation worker failed to start: {error}"))?;
        Ok(Self {
            request_tx,
            result_rx,
        })
    }
}

struct PendingGeometryImport {
    generation: u64,
    request_id: String,
    path: std::path::PathBuf,
    started_at: std::time::Instant,
}

#[derive(Clone)]
struct PendingShellChoice {
    path: std::path::PathBuf,
    source_name: String,
    source_bytes: Vec<u8>,
    source_sha256: String,
    declared_unit: String,
    shells: Vec<crate::cad_step::ShellCandidate>,
}

impl GeometryImportWorker {
    fn spawn(repaint_context: Option<egui::Context>) -> Result<Self, String> {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<GeometryImportWorkRequest>();
        let (result_tx, result_rx) = std::sync::mpsc::channel::<GeometryImportWorkResult>();
        std::thread::Builder::new()
            .name("reyn-geometry-import".into())
            .spawn(move || {
                while let Ok(mut request) = request_rx.recv() {
                    // Keep only the newest queued import; a pass already running
                    // may finish, but the UI suppresses stale generations.
                    while let Ok(newest) = request_rx.try_recv() {
                        request = newest;
                    }
                    let source_sha256 = format!("{:x}", Sha256::digest(&request.source_bytes));
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        match cad::parse_geometry_selecting(
                            &request.source_name,
                            &request.source_bytes,
                            request.selected_shell_entity_id,
                        ) {
                            Ok(imported) => {
                                let diagnostics = cad::diagnose_mesh(&imported.mesh);
                                let orientation = cad::BodyOrientation::align_longest_extent_to_stream(
                                    diagnostics.extents,
                                );
                                match cad::voxelize_oriented(
                                    &imported.mesh,
                                    request.grid,
                                    orientation,
                                ) {
                                    Ok(voxel) => Ok(GeometryImportReady {
                                        imported,
                                        diagnostics,
                                        voxel,
                                    }),
                                    Err(error) => Err(GeometryImportFailure::Message(error)),
                                }
                            }
                            Err(cad::GeometryParseError::ChooseShell(choice)) => {
                                Err(GeometryImportFailure::ChooseShell(choice))
                            }
                            Err(cad::GeometryParseError::Message(message)) => {
                                Err(GeometryImportFailure::Message(message))
                            }
                        }
                    }))
                    .unwrap_or_else(|_| {
                        Err(GeometryImportFailure::Message(
                            "geometry import worker panicked while translating or voxelizing"
                                .into(),
                        ))
                    });
                    let completed = GeometryImportWorkResult {
                        generation: request.generation,
                        request_id: request.request_id,
                        path: request.path,
                        source_name: request.source_name,
                        source_bytes: request.source_bytes,
                        source_sha256,
                        selected_shell_entity_id: request.selected_shell_entity_id,
                        outcome,
                    };
                    if result_tx.send(completed).is_err() {
                        break;
                    }
                    if let Some(context) = &repaint_context {
                        context.request_repaint();
                    }
                }
            })
            .map_err(|error| format!("geometry import worker could not start: {error}"))?;
        Ok(Self {
            request_tx,
            result_rx,
        })
    }
}

impl OrientationWorker {
    fn spawn(repaint_context: Option<egui::Context>) -> Result<Self, String> {
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("reyn-orientation-voxelizer".into())
            .spawn(move || {
                while let Ok(request) = request_rx.recv() {
                    // Only the newest queued attitude starts the expensive pass.
                    // A pass already executing is allowed to finish, but its
                    // generation is suppressed by the UI if it was superseded.
                    let request = coalesce_orientation_requests(request, &request_rx);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cad::parse_geometry_selecting(
                            &request.source_name,
                            &request.source_bytes,
                            request.selected_shell_entity_id,
                        )
                        .map_err(String::from)
                        .and_then(|imported| {
                            cad::voxelize_oriented(
                                &imported.mesh,
                                request.grid,
                                cad::BodyOrientation::from_degrees(request.angles),
                            )
                        })
                    }))
                    .unwrap_or_else(|_| {
                        Err("orientation worker panicked while re-voxelizing geometry".into())
                    });
                    let completed = OrientationWorkResult {
                        generation: request.generation,
                        request_id: request.request_id,
                        case_id: request.case_id,
                        source_sha256: request.source_sha256,
                        angles: request.angles,
                        completed_utc_unix: now_utc_unix(),
                        result,
                    };
                    if result_tx.send(completed).is_err() {
                        break;
                    }
                    if let Some(context) = &repaint_context {
                        context.request_repaint();
                    }
                }
            })
            .map_err(|error| format!("orientation worker could not start: {error}"))?;
        Ok(Self {
            request_tx,
            result_rx,
        })
    }
}

#[derive(Clone, Copy)]
enum ScreenshotWriteKind {
    Viewport,
    Qa,
}

#[derive(Clone, Debug, Default)]
struct ViewportShotProvenance {
    footer_lines: Vec<String>,
}

struct ScreenshotWriteResult {
    kind: ScreenshotWriteKind,
    path: std::path::PathBuf,
    result: Result<(), String>,
}

impl Default for HorizonPlayback {
    fn default() -> Self {
        Self {
            step: 0,
            playing: false,
            last_advance: f64::NEG_INFINITY,
            frames: std::collections::BTreeMap::new(),
            fetching: None,
            failed: std::collections::BTreeSet::new(),
        }
    }
}

impl HorizonPlayback {
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// Keep the recorded step and the neighbourhood of the displayed step.
    fn trim(&mut self, keep: u32) {
        while self.frames.len() > HORIZON_CACHE_LIMIT {
            let victim = self
                .frames
                .keys()
                .copied()
                .filter(|step| *step != keep && *step != self.step)
                .max_by_key(|step| step.abs_diff(self.step));
            match victim {
                Some(step) => {
                    self.frames.remove(&step);
                }
                None => break,
            }
        }
    }
}

/// What an arriving engine field is allowed to become. Cancellation terminates
/// the blocking sidecar and starts a fresh engine; this classifier remains a
/// second-line stale-generation guard for already queued UI messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CadResultDisposition {
    /// A horizon-playback fetch for this step: display-only.
    Preview(u32),
    /// The in-flight run: record it as an immutable run.
    Record,
    /// Nothing is waiting for this request (superseded case edit, reimport…).
    DiscardStale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EngineErrorDisposition {
    CurrentRun,
    Preview(u32),
    Field2D,
    Benchmark,
    BenchmarkInspector,
    Library(engine::RequestKind),
    Global,
    DiscardStale,
}

fn classify_engine_error(
    request: Option<&engine::RequestContext>,
    preview_fetch: Option<(u32, &str)>,
    pending_run: Option<&str>,
) -> EngineErrorDisposition {
    let Some(request) = request else {
        return EngineErrorDisposition::Global;
    };
    match request.kind {
        engine::RequestKind::CadPredict => {
            if pending_run == Some(request.id.as_str()) {
                EngineErrorDisposition::CurrentRun
            } else if let Some((step, fetching)) = preview_fetch {
                if fetching == request.id {
                    EngineErrorDisposition::Preview(step)
                } else {
                    EngineErrorDisposition::DiscardStale
                }
            } else {
                EngineErrorDisposition::DiscardStale
            }
        }
        engine::RequestKind::Predict2D | engine::RequestKind::PredictIc => {
            EngineErrorDisposition::Field2D
        }
        engine::RequestKind::RunBenchmark => EngineErrorDisposition::Benchmark,
        engine::RequestKind::InspectBenchmarkCell => EngineErrorDisposition::BenchmarkInspector,
        engine::RequestKind::ListModels
        | engine::RequestKind::ImportModel
        | engine::RequestKind::DeleteModel => EngineErrorDisposition::Library(request.kind),
        engine::RequestKind::Predict => EngineErrorDisposition::Global,
    }
}

fn library_response_is_current(
    pending: Option<&engine::RequestContext>,
    request: Option<&engine::RequestContext>,
) -> bool {
    pending
        .zip(request)
        .is_some_and(|(pending, request)| pending == request)
}

fn engine_error_is_stale(
    disposition: EngineErrorDisposition,
    pending_library: Option<&engine::RequestContext>,
    request: Option<&engine::RequestContext>,
) -> bool {
    disposition == EngineErrorDisposition::DiscardStale
        || matches!(disposition, EngineErrorDisposition::Library(_))
            && !library_response_is_current(pending_library, request)
}

fn classify_cad_result(
    request_id: &str,
    preview_fetch: Option<(u32, &str)>,
    pending_run: Option<&str>,
) -> CadResultDisposition {
    if let Some((step, fetching)) = preview_fetch {
        if fetching == request_id {
            return CadResultDisposition::Preview(step);
        }
    }
    if pending_run == Some(request_id) {
        return CadResultDisposition::Record;
    }
    CadResultDisposition::DiscardStale
}

/// A completed engine payload is returned to the UI only after its durable
/// project write succeeds. Injecting an error here deterministically exercises
/// the no-transient-result guarantee without filesystem timing or sleeps.
fn retain_after_persistence<T, O>(
    transient: T,
    persist: impl FnOnce(&T) -> Result<O, String>,
) -> Result<(O, T), String> {
    let durable = persist(&transient)?;
    Ok((durable, transient))
}

/// Map a CAD vorticity volume onto raymarch thresholds the operator can see.
///
/// Sandbox defaults (`density_lo ≈ 0.85`) assume dense procedural fields. A
/// fixed-body CAD prediction concentrates |ω| in a thin wake, so those defaults
/// erase the fluid and leave only a tiny unfitted body on a black canvas.
fn calibrate_cad_volume_density(volume: &[u8]) -> (f32, f32) {
    let mut nonzero: Vec<u8> = volume.iter().copied().filter(|&value| value > 0).collect();
    if nonzero.is_empty() {
        return (0.05, 0.45);
    }
    nonzero.sort_unstable();
    let percentile = |p: f32| -> f32 {
        let index = ((p * (nonzero.len() as f32 - 1.0)).round() as usize).min(nonzero.len() - 1);
        nonzero[index] as f32 / 255.0
    };
    let mut lo = percentile(0.45).clamp(0.08, 0.45);
    let mut hi = percentile(0.92).clamp(lo + 0.10, 0.95);
    if hi - lo < 0.10 {
        hi = (lo + 0.22).min(0.95);
        lo = (hi - 0.22).max(0.08);
    }
    (lo, hi)
}

/// The fields the result views should draw this frame, and which horizon step
/// they belong to.
struct DisplayFields<'a> {
    n: usize,
    velocity: &'a [f32],
    pressure: &'a [f32],
    cp: &'a [f32],
    traction: &'a [f32],
    mask: &'a [f32],
    step: u32,
    /// True when this is the run's own recorded horizon — the evidence step.
    recorded: bool,
}

/// A picked point on the 3D body surface, reported with the same quantities and
/// source classes as the 2D section probe.
#[derive(Clone)]
struct SurfaceProbe {
    /// Centre of the picked cell in render-domain coordinates, so the readout
    /// stays pinned to the body when the camera moves.
    anchor: [f32; 3],
    cell: [usize; 3],
    /// Position in the approved source frame, in metres, when the transform is
    /// invertible; `None` when it is not.
    source_m: Option<[f64; 3]>,
    cp: f32,
    pressure_pa: f32,
    traction_pa: [f32; 3],
    /// Horizon step the values came from, and whether that is the recorded run.
    step: u32,
    recorded: bool,
}

impl CadCase {
    /// Does the case hold a full set of result fields at the recorded horizon?
    fn has_recorded_fields(&self) -> bool {
        let cube = self.result_grid.saturating_pow(3);
        self.result_grid >= 3
            && self.velocity.len() == 3 * cube
            && self.pressure.len() == cube
            && self.cp.len() == cube
            && self.traction.len() == 3 * cube
            && self.mask.len() == cube
    }

    /// The step the views are showing: the recorded horizon until the operator
    /// scrubs somewhere else.
    fn display_step(&self) -> u32 {
        if self.playback.step == 0 {
            self.steps
        } else {
            self.playback.step
        }
    }

    /// Fields for the displayed step, or `None` when that step has not been
    /// fetched yet (the view then says so rather than showing another step's
    /// data under this step's label).
    fn display_fields(&self) -> Option<DisplayFields<'_>> {
        let step = self.display_step();
        if step == self.steps && self.has_recorded_fields() {
            return Some(DisplayFields {
                n: self.result_grid,
                velocity: &self.velocity,
                pressure: &self.pressure,
                cp: &self.cp,
                traction: &self.traction,
                mask: self.mask.as_ref(),
                step,
                recorded: true,
            });
        }
        let frame = self.playback.frames.get(&step)?;
        Some(DisplayFields {
            n: frame.n,
            velocity: &frame.velocity,
            pressure: &frame.pressure,
            cp: &frame.cp,
            traction: &frame.traction,
            mask: frame.mask.as_ref(),
            step,
            recorded: false,
        })
    }
}

#[derive(PartialEq, Clone, Copy)]
enum PMethod {
    Spectral,
    Fd,
}
#[derive(PartialEq, Clone, Copy)]
enum PBoundary {
    Periodic,
    Dirichlet,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum CaseEditTransaction {
    ReferenceLength,
    Velocity,
    Density,
    Viscosity,
    ReferencePressure,
    Horizon,
    MomentOrigin,
    ReferenceArea,
}

#[derive(Clone, Copy)]
enum CaseHistoryAction {
    Undo,
    Redo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ModelInventoryState {
    Loading,
    Ready,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ModelSelectorMode {
    Loading,
    Failed(String),
    NoneQualified,
    OneQualified(String),
    MultipleQualified,
    StoredModelMissing(String),
}

fn model_selector_mode(
    state: &ModelInventoryState,
    models: &[engine::ModelCard],
    stored_model_id: &str,
    target_grid: usize,
) -> ModelSelectorMode {
    match state {
        ModelInventoryState::Loading => ModelSelectorMode::Loading,
        ModelInventoryState::Failed(error) => ModelSelectorMode::Failed(error.clone()),
        ModelInventoryState::Ready => {
            let qualified = models
                .iter()
                .filter(|model| engine::is_qualified_external_flow_model(model, target_grid))
                .collect::<Vec<_>>();
            if !stored_model_id.trim().is_empty()
                && !qualified.iter().any(|model| model.id == stored_model_id)
            {
                ModelSelectorMode::StoredModelMissing(stored_model_id.into())
            } else {
                match qualified.as_slice() {
                    [] => ModelSelectorMode::NoneQualified,
                    [model] => ModelSelectorMode::OneQualified(model.id.clone()),
                    _ => ModelSelectorMode::MultipleQualified,
                }
            }
        }
    }
}

fn bind_external_flow_model(case: &mut CadCase, model: &engine::ModelCard) {
    case.model = model.id.clone();
    case.workflow.model_id = model.id.clone();
    case.workflow.model_sha256 = Some(model.checkpoint_sha256.clone());
    case.workflow.model_max_steps = model.max_steps;
    case.workflow.model_support = engineering::ModelSupport {
        status: model.status.clone(),
        dimension: model.dimension,
        grid: model.grid,
        input_channels: model.in_channels,
        output_channels: model.out_channels,
        scenario: model.scenario.clone(),
        physics_contract: model.physics_contract.clone(),
    };
}

pub struct ReynApp {
    nav: Nav,
    volumetric: bool,
    slice: [bool; 3],
    slice_pos: [f32; 3],
    density_lo: f32,
    density_hi: f32,
    opacity: f32,
    shadows: bool,
    streamlines: bool,
    cam: viewport::Camera,
    particles: Vec<flow::Particle>,
    seed: u64,
    engine: engine::EngineHandle,
    /// Runtime-only context used by the engine receiver bridge. `Default`
    /// remains context-free for tests; `new` installs the native wake hook.
    repaint_context: Option<egui::Context>,
    engine_status: String,
    engine_ok: bool,
    /// Dependency reconciliation is expensive and only changes after an engine
    /// event, model inventory change, project mutation, or content relink.
    dependencies_dirty: bool,
    /// UTC deadline for the next recovery snapshot check. This preserves
    /// autosave without a permanent idle repaint poll.
    next_autosave_utc_unix: u64,
    last_window_title: String,
    current_model: String,
    models: Vec<engine::ModelCard>,
    model_inventory_state: ModelInventoryState,
    library: library::LibraryState,
    library_pending_request: Option<engine::RequestContext>,
    settings: settings::AppSettings,
    settings_draft: settings::AppSettings,
    settings_notice: Option<(String, bool)>,
    signing_notice: Option<(String, bool)>,
    settings_ui: settings::SettingsUiState,
    updater: Option<crate::updater::Updater>,
    sign_out_requested: bool,
    study_focus_stage: Option<engineering::ReadinessStage>,
    /// Session copy of the per-field Case Setup entry units (seeded from
    /// settings; switching a unit here never changes the stored SI value).
    input_units: units::InputUnitPrefs,
    preset_name_draft: String,
    preset_notice: Option<(String, bool)>,
    template_name_draft: String,
    template_notice: Option<(String, bool)>,
    /// Last frame's render-viewport rect (points) for PNG capture cropping.
    last_render_rect: Option<Rect>,
    /// Destination for an in-flight composited-frame screenshot.
    pending_viewport_shot: Option<std::path::PathBuf>,
    /// Dev/QA hook (REYN_STUDIO_SHOT=path): save a full-window PNG once the UI
    /// has settled, without needing OS screen-recording permission.
    qa_shot_path: Option<std::path::PathBuf>,
    qa_shot_frames: u32,
    screenshot_result_tx: std::sync::mpsc::Sender<ScreenshotWriteResult>,
    screenshot_result_rx: std::sync::mpsc::Receiver<ScreenshotWriteResult>,
    /// Dev/QA hook (REYN_STUDIO_IMPORT=path.stl|path.stp): import geometry through the
    /// ordinary import path once the model inventory has arrived, so captures
    /// of the case and viewport screens do not need a file dialog. Nothing is
    /// faked — the case is gated exactly as a hand-imported one.
    qa_import_path: Option<std::path::PathBuf>,
    qa_import_waited: u32,
    /// Dev/QA hook (REYN_STUDIO_APPROVE_TRANSFORM=1): after a successful import,
    /// tick the ordinary units/transform approval checkbox so Case Setup can be
    /// captured past the hard setup gate. Does not waive geometry defects.
    qa_approve_transform: bool,
    /// Dev/QA hook (REYN_STUDIO_RUN_ANALYSIS=1): once the case is ready and a
    /// qualified model is installed, start the ordinary external-flow run.
    qa_run_analysis: bool,
    project: project_lifecycle::ProjectLifecycle,
    project_name_draft: String,
    project_guard: project_lifecycle::UnsavedChangesGuard,
    project_notice: Option<(String, bool)>,
    project_conflict: Option<ProjectWriteConflict>,
    /// True when visible Case Setup inputs differ from the active persisted
    /// case revision. Pending orientation has its own exact draft below.
    case_draft_dirty: bool,
    /// Read-only evidence navigation is session-local and must never dirty the
    /// authoritative project manifest.
    review_selection: Option<project::ProjectSelection>,
    allow_close: bool,
    live: bool,
    live_timer: f32,
    gpu_ready: bool,
    render_volume: bool,
    /// Thin Q-criterion iso via the volume raymarch when wake volume is off.
    q_iso_on: bool,
    volume_data: std::sync::Arc<Vec<u8>>,
    volume_dims: [u32; 3],
    volume_version: u64,
    /// Positive-Q scalar volume for the optional thin iso layer.
    q_volume_data: std::sync::Arc<Vec<u8>>,
    q_volume_dims: [u32; 3],
    // N3 — 2D pressure-recovery view
    f2d: Option<engine::Field2D>,
    f2d_var: FieldVar,
    f2d_horizon: u32,
    f2d_truth: bool,
    f2d_model: String,
    f2d_pending: bool,
    f2d_dirty: bool,
    f2d_req_at: Option<std::time::Instant>,
    f2d_latency_ms: u32,
    f2d_gen: u64,
    f2d_tex: Vec<egui::TextureHandle>,
    f2d_sig: u64,
    f2d_insights: Vec<field2d::Insight>,
    f2d_insights_key: Option<(u64, FieldVar, bool)>,
    f2d_method: PMethod,
    f2d_tol_exp: i32, // FD tolerance = 10^-exp
    f2d_boundary: PBoundary,
    insights_on: bool,
    insight_classes: [bool; 5], // P▲, P▼, ω, v, ε — mirrors InsightKind order
    insights3d: Vec<flow::Insight3D>, // critical points of the current 3D field
    f2d_scale: f32,             // shared model/reference colormap scale (for the legend)
    f2d_signed: bool,           // diverging vs sequential legend
    // N4 — Flow Painter
    paint: painter::PaintField,
    paint_sym: painter::Symmetry,
    brush_radius: f32,
    brush_strength: f32,
    paint_tex: Option<egui::TextureHandle>,
    paint_dirty: bool,
    paint_last: Option<(f32, f32)>, // previous stamp in grid coords (stroke interp)
    f2d_painted: Option<std::sync::Arc<Vec<f32>>>, // active painted IC in Fields (2D)
    // CAD flow analysis
    cad: Option<CadCase>,
    /// FIFO of additional external-flow runs requested while one is in flight.
    run_queue: RunQueue,
    waiver_draft: String,
    waiver_code: Option<String>,
    surface_on: bool,
    cad_version: u64,
    section_axis: engineering_section::SectionAxis,
    section_quantity: engineering_section::SectionQuantity,
    section_tex: Option<egui::TextureHandle>,
    section_data: Option<engineering_section::SectionPlane>,
    section_sig: u64,
    section_error: Option<String>,
    // N5 — Benchmark Lab
    bench: Option<engine::BenchResult>,
    bench_running: bool,
    bench_seeds: u32,
    bench_seed_start: u32,
    bench_selected: Option<(usize, usize)>,
    bench_inspector: Option<engine::BenchInspector>,
    bench_inspector_pending: bool,
    bench_error: Option<String>,
    bench_inspector_error: Option<String>,
    bench_tex: Vec<egui::TextureHandle>,
    bench_var: InspectorVariable,
    active_benchmark_run_id: Option<String>,
    // Shell chrome + motion bookkeeping
    #[cfg(target_os = "macos")]
    menubar: Option<MenuBar>,
    last_nav: Nav,
    nav_changed_at: f64,
    // Command palette (⌘K, §6 Tier 3.5) — actions only, same gating as nav.
    palette_open: bool,
    palette_query: String,
    palette_selected: usize,
    // Viewport navigation requests, consumed by `viewport::show` on the next
    // frame: a standard-view snap and a zoom-to-fit.
    view_snap: Option<viewport::StandardView>,
    view_fit: bool,
    /// Last 3D surface pick, cleared when the displayed field changes.
    probe3d: Option<SurfaceProbe>,
    /// Edited body orientation (α, β, roll in degrees) awaiting re-voxelization.
    /// `None` while the controls match the case's approved orientation.
    orientation_draft: Option<[f64; 3]>,
    /// Lazily spawned single worker. It drains queued requests to the newest
    /// generation before each expensive voxelization and wakes egui on result.
    orientation_worker: Option<OrientationWorker>,
    orientation_generation: u64,
    /// The only orientation result allowed to mutate the active case. Replacing
    /// this value cancels the old generation logically; its result is stale.
    orientation_pending: Option<PendingOrientation>,
    geometry_import_worker: Option<GeometryImportWorker>,
    geometry_import_generation: u64,
    geometry_import_pending: Option<PendingGeometryImport>,
    cad_field_ready_worker: Option<CadFieldReadyWorker>,
    cad_field_ready_pending: Option<PendingCadFieldReady>,
    pending_shell_choice: Option<PendingShellChoice>,
    /// Session-only, bounded history of reversible case-draft inputs. Immutable
    /// source/model/run/evidence identity never enters these snapshots.
    case_draft_history: engineering::CaseDraftHistory,
    /// Active numeric interaction, used to coalesce DragValue/slider repaints
    /// into one meaningful undo transaction.
    case_edit_transaction: Option<CaseEditTransaction>,
}

impl Default for ReynApp {
    fn default() -> Self {
        let (settings, settings_warning) = settings::AppSettings::load();
        let now = now_utc_unix();
        let project_state_directory = settings::config_path()
            .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_else(|| std::path::PathBuf::from(".reyn-studio"));
        let (project, project_warnings) =
            project_lifecycle::ProjectLifecycle::load(project_state_directory, now);
        let project_name_draft = project.display_name().to_owned();
        let engine = engine::EngineHandle::spawn_with_config(settings.engine_config());
        let current_model = settings.default_3d_model.clone();
        let f2d_model = settings.default_2d_model.clone();
        let library_pending_request = engine.send(engine::Cmd::ListModels).ok();
        let next_autosave_utc_unix =
            now.saturating_add(settings.autosave_interval_seconds.max(1) as u64);
        let (screenshot_result_tx, screenshot_result_rx) = std::sync::mpsc::channel();
        let (vol, vdims) = flow::procedural_volume(48, 1); // placeholder until a field arrives
        Self {
            nav: Nav::Projects,
            volumetric: true,
            slice: [true, false, false],
            slice_pos: [0.50, 0.0, 0.0],
            density_lo: 0.85,
            density_hi: 1.0,
            opacity: 0.75,
            shadows: true,
            streamlines: false,
            cam: viewport::Camera::default(),
            particles: flow::generate(6000, 1), // procedural until the model field arrives
            seed: 1,
            engine,
            repaint_context: None,
            engine_status: "○ Starting engine…".into(),
            engine_ok: false,
            dependencies_dirty: true,
            next_autosave_utc_unix,
            last_window_title: String::new(),
            current_model,
            models: Vec::new(),
            model_inventory_state: if library_pending_request.is_some() {
                ModelInventoryState::Loading
            } else {
                ModelInventoryState::Failed(
                    "The model inventory request could not be started.".into(),
                )
            },
            library: library::LibraryState::default(),
            library_pending_request,
            settings_draft: settings.clone(),
            settings,
            settings_notice: settings_warning.map(|warning| (warning, true)),
            signing_notice: None,
            settings_ui: settings::SettingsUiState::default(),
            updater: None,
            sign_out_requested: false,
            study_focus_stage: None,
            input_units: units::InputUnitPrefs::default(),
            preset_name_draft: String::new(),
            preset_notice: None,
            template_name_draft: String::new(),
            template_notice: None,
            last_render_rect: None,
            pending_viewport_shot: None,
            qa_shot_path: std::env::var("REYN_STUDIO_SHOT")
                .ok()
                .map(std::path::PathBuf::from),
            qa_shot_frames: 0,
            screenshot_result_tx,
            screenshot_result_rx,
            qa_import_path: std::env::var("REYN_STUDIO_IMPORT")
                .ok()
                .map(std::path::PathBuf::from),
            qa_import_waited: 0,
            qa_approve_transform: matches!(
                std::env::var("REYN_STUDIO_APPROVE_TRANSFORM").as_deref(),
                Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
            ),
            qa_run_analysis: matches!(
                std::env::var("REYN_STUDIO_RUN_ANALYSIS").as_deref(),
                Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
            ),
            project,
            project_name_draft,
            project_guard: project_lifecycle::UnsavedChangesGuard::default(),
            project_notice: (!project_warnings.is_empty())
                .then(|| (project_warnings.join(" "), true)),
            project_conflict: None,
            case_draft_dirty: false,
            review_selection: None,
            allow_close: false,
            live: false,
            live_timer: 0.0,
            gpu_ready: false,
            render_volume: false,
            q_iso_on: false,
            volume_data: std::sync::Arc::new(vol),
            volume_dims: vdims,
            volume_version: 1,
            q_volume_data: std::sync::Arc::new(Vec::new()),
            q_volume_dims: [1, 1, 1],
            f2d: None,
            f2d_var: FieldVar::Vorticity,
            f2d_horizon: 8,
            f2d_truth: false,
            f2d_model,
            f2d_pending: false,
            f2d_dirty: false,
            f2d_req_at: None,
            f2d_latency_ms: 0,
            f2d_gen: 0,
            f2d_tex: Vec::new(),
            f2d_sig: u64::MAX,
            f2d_insights: Vec::new(),
            f2d_insights_key: None,
            f2d_method: PMethod::Spectral,
            f2d_tol_exp: 5,
            f2d_boundary: PBoundary::Periodic,
            insights_on: true,
            insight_classes: [true; 5],
            insights3d: Vec::new(),
            f2d_scale: 1.0,
            f2d_signed: true,
            paint: painter::PaintField::default(),
            paint_sym: painter::Symmetry::default(),
            brush_radius: 7.0,
            brush_strength: 1.2,
            paint_tex: None,
            paint_dirty: true,
            paint_last: None,
            f2d_painted: None,
            cad: None,
            run_queue: RunQueue::default(),
            waiver_draft: String::new(),
            waiver_code: None,
            surface_on: false,
            cad_version: 0,
            section_axis: engineering_section::SectionAxis::X,
            section_quantity: engineering_section::SectionQuantity::PhysicalCp,
            section_tex: None,
            section_data: None,
            section_sig: u64::MAX,
            section_error: None,
            bench: None,
            bench_running: false,
            bench_seeds: 3,
            bench_seed_start: 70000,
            bench_selected: None,
            bench_inspector: None,
            bench_inspector_pending: false,
            bench_error: None,
            bench_inspector_error: None,
            bench_tex: Vec::new(),
            bench_var: InspectorVariable::Velocity,
            active_benchmark_run_id: None,
            #[cfg(target_os = "macos")]
            menubar: None,
            last_nav: Nav::Projects,
            nav_changed_at: f64::NEG_INFINITY,
            palette_open: false,
            palette_query: String::new(),
            palette_selected: 0,
            view_snap: None,
            view_fit: false,
            probe3d: None,
            orientation_draft: None,
            orientation_worker: None,
            geometry_import_worker: None,
            geometry_import_generation: 0,
            geometry_import_pending: None,
            cad_field_ready_worker: None,
            cad_field_ready_pending: None,
            pending_shell_choice: None,
            orientation_generation: 0,
            orientation_pending: None,
            case_draft_history: engineering::CaseDraftHistory::default(),
            case_edit_transaction: None,
        }
    }
}

/// GPU/UI state that is safe to prepare before an official build authenticates.
///
/// Starting from this value is the boundary that loads settings and projects,
/// scans models, and launches the Python sidecar. The access gate keeps the
/// bootstrap but does not cross that boundary until a session is granted.
#[derive(Clone)]
pub struct AppBootstrap {
    repaint_context: egui::Context,
    gpu_ready: bool,
}

impl AppBootstrap {
    pub fn prepare(cc: &eframe::CreationContext<'_>) -> Self {
        let gpu_ready = cc.wgpu_render_state.as_ref().is_some_and(|render_state| {
            gpu::install(render_state);
            true
        });
        Self {
            repaint_context: cc.egui_ctx.clone(),
            gpu_ready,
        }
    }

    pub fn start_with_updater(&self, updater: crate::updater::Updater) -> ReynApp {
        let mut app = ReynApp::new_from_bootstrap(self);
        app.updater = Some(updater);
        app
    }
}

impl ReynApp {
    pub fn take_sign_out_request(&mut self) -> bool {
        std::mem::take(&mut self.sign_out_requested)
    }

    fn new_from_bootstrap(bootstrap: &AppBootstrap) -> Self {
        let mut app = Self {
            repaint_context: Some(bootstrap.repaint_context.clone()),
            ..Self::default()
        };
        app.attach_engine_repaint_wake();
        apply_with_contrast(
            &bootstrap.repaint_context,
            app.settings.theme == settings::ThemeMode::HighContrast,
        );
        set_reduced_motion(&bootstrap.repaint_context, app.settings.reduced_motion);
        // Persisted display preferences take effect from the first frame.
        bootstrap
            .repaint_context
            .set_zoom_factor(app.settings.ui_scale);
        field2d::set_view_colormap(app.settings.colormap);
        app.section_axis = app.settings.default_section_axis;
        app.section_quantity = app.settings.default_section_quantity;
        app.input_units = app.settings.input_units;
        app.gpu_ready = bootstrap.gpu_ready;
        // Native macOS menu bar replaces the deleted in-app menu row (§4.1).
        // If installation fails, keyboard shortcuts and in-screen actions
        // keep every command reachable.
        #[cfg(target_os = "macos")]
        {
            app.menubar = MenuBar::install();
        }
        // Dev/QA affordance: pick the startup screen without clicking through
        // the rail (e.g. REYN_STUDIO_START_NAV=settings). Screens stay gated
        // exactly as in the UI — no state is faked to reach them.
        if let Ok(start) = std::env::var("REYN_STUDIO_START_NAV") {
            // `settings:<category>` deep-links a settings category for QA
            // (e.g. settings:units, settings:appearance).
            let (screen, detail) = match start.split_once(':') {
                Some((screen, detail)) => (screen, Some(detail)),
                None => (start.as_str(), None),
            };
            // The research-sandbox screens are only reachable when the operator
            // has turned the sandbox on in Settings → Developer; the deep-link
            // respects that gate rather than opening a back door.
            let sandbox = app.settings.developer_research_sandbox;
            app.nav = match screen {
                "case" => Nav::Case,
                "results" => Nav::Results,
                "evidence" => Nav::Evidence,
                "models" => Nav::Models,
                "settings" => Nav::Settings,
                "metrics" if sandbox => Nav::Metrics,
                "fields2d" if sandbox => Nav::Fields2D,
                "painter" if sandbox => Nav::FlowPainter,
                "benchmark" if sandbox => Nav::Benchmark,
                _ => Nav::Projects,
            };
            if app.nav == Nav::Settings {
                if let Some(category) = detail.and_then(settings::SettingsCategory::from_qa_id) {
                    app.settings_ui.category = category;
                }
            }
        }
        app
    }

    /// Forward engine messages through a receiver that wakes egui exactly when
    /// new state arrives. This removes the need to poll while ready or
    /// unavailable, and is reinstalled whenever settings/project changes
    /// replace the engine handle.
    fn attach_engine_repaint_wake(&mut self) {
        let Some(repaint_context) = self.repaint_context.clone() else {
            return;
        };
        let (forward_tx, forward_rx) = std::sync::mpsc::channel();
        let (_, placeholder_rx) = std::sync::mpsc::channel();
        let source_rx = std::mem::replace(&mut self.engine.rx, placeholder_rx);
        self.engine.rx = forward_rx;
        std::thread::Builder::new()
            .name("reyn-engine-ui-wake".into())
            .spawn(move || {
                while let Ok(message) = source_rx.recv() {
                    if forward_tx.send(message).is_err() {
                        break;
                    }
                    repaint_context.request_repaint();
                }
            })
            .expect("engine repaint bridge thread should start");
    }

    fn schedule_autosave_from_now(&mut self) {
        self.next_autosave_utc_unix =
            now_utc_unix().saturating_add(self.settings.autosave_interval_seconds.max(1).into());
    }

    fn has_unsaved_project_work(&self) -> bool {
        has_unsaved_project_work(
            self.project.is_dirty(),
            self.case_draft_dirty,
            self.orientation_draft,
            self.orientation_pending
                .as_ref()
                .is_some_and(PendingOrientation::mutates_project),
        )
    }

    fn mark_case_draft_dirty(&mut self) {
        if !self.has_unsaved_project_work() {
            self.schedule_autosave_from_now();
        }
        self.case_draft_dirty = true;
    }

    fn project_write_access(&self, operation: &str) -> Result<(), String> {
        project_mutation_rejection(self.project.availability().access_mode, operation)
            .map_or(Ok(()), Err)
    }

    fn reject_project_mutation(&mut self, operation: &str) -> bool {
        match self.project_write_access(operation) {
            Ok(()) => false,
            Err(reason) => {
                self.project_notice = Some((reason, true));
                true
            }
        }
    }

    fn transact_project<T>(
        &mut self,
        operation: &str,
        now_utc_unix: u64,
        edit: impl FnOnce(&mut project::ProjectManifest) -> Result<T, project::ProjectError>,
    ) -> Result<T, String> {
        self.project_write_access(operation)?;
        self.project
            .transact(now_utc_unix, edit)
            .map_err(|error| error.to_string())
    }

    fn add_project_content(
        &mut self,
        operation: &str,
        bytes: Vec<u8>,
        media_type: impl Into<String>,
        expected_digest: &str,
    ) -> Result<project::ContentInsert, String> {
        self.project_write_access(operation)?;
        self.project
            .add_content_with_digest(bytes, media_type, expected_digest)
            .map_err(|error| error.to_string())
    }

    /// Make every visible project-scoped draft durable before Save or recovery.
    /// Orientation work is never flushed synchronously here: saving stale mask
    /// bytes under draft angles would be false evidence, and re-voxelizing on
    /// this call path would block the UI. Save/recovery therefore wait for the
    /// event-driven worker completion.
    fn flush_project_drafts_for_persistence(&mut self) -> Result<bool, String> {
        self.project_write_access("Saving project drafts")?;
        if let Some(reason) = orientation_geometry_gate(
            "Saving project drafts",
            self.orientation_draft,
            self.orientation_pending.as_ref(),
        ) {
            return Err(reason);
        }
        if self.case_draft_dirty {
            self.commit_active_case_revision()?;
        }
        Ok(false)
    }
}

impl eframe::App for ReynApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_orientation_results();
        self.handle_geometry_import_results();
        self.handle_cad_field_ready_results();
        self.draw_shell_choice_modal(ui.ctx());
        self.handle_screenshot_write_results();
        // Complete any in-flight viewport PNG capture first: the screenshot
        // event carries the previous composited frame.
        self.handle_qa_shot(ui.ctx());
        self.handle_screenshot_events(ui.ctx());
        self.handle_qa_import();
        self.handle_qa_approve_transform();
        self.handle_qa_run_analysis();
        // Drain engine messages without allowing a queued burst to monopolize
        // the UI thread. The forwarding bridge wakes the first frame; hitting
        // the budget below explicitly schedules the continuation.
        let mut engine_messages = 0usize;
        while engine_messages < ENGINE_MESSAGES_PER_FRAME {
            let Ok(msg) = self.engine.rx.try_recv() else {
                break;
            };
            engine_messages += 1;
            self.dependencies_dirty = true;
            let (request_context, msg) = match msg {
                engine::Msg::Correlated { request, response } => (Some(request), *response),
                uncorrelated => (None, uncorrelated),
            };
            match msg {
                engine::Msg::Correlated { .. } => {
                    self.engine_status = "○ malformed nested engine response".into();
                    self.engine_ok = false;
                }
                engine::Msg::Status(s) => {
                    self.engine_status = s;
                    self.engine_ok = true;
                    if self.nav == Nav::Fields2D && self.f2d.is_none() && !self.f2d_pending {
                        self.request_2d();
                    }
                }
                engine::Msg::Models(m) => {
                    if !library_response_is_current(
                        self.library_pending_request.as_ref(),
                        request_context.as_ref(),
                    ) {
                        continue;
                    }
                    self.models = m;
                    self.model_inventory_state = ModelInventoryState::Ready;
                    self.library.busy = false;
                    self.library_pending_request = None;
                }
                engine::Msg::ModelImported { model, models } => {
                    if !library_response_is_current(
                        self.library_pending_request.as_ref(),
                        request_context.as_ref(),
                    ) {
                        continue;
                    }
                    self.library.busy = false;
                    self.library_pending_request = None;
                    self.library.validation = None;
                    self.library.notice = Some((
                        format!(
                            "Imported {}; verified model-bundle contract accepted",
                            model.name
                        ),
                        false,
                    ));
                    self.models = models;
                    self.model_inventory_state = ModelInventoryState::Ready;
                    self.activate_model(&model.id);
                }
                engine::Msg::ModelImportRejected(validation) => {
                    if !library_response_is_current(
                        self.library_pending_request.as_ref(),
                        request_context.as_ref(),
                    ) {
                        continue;
                    }
                    self.library.busy = false;
                    self.library_pending_request = None;
                    // Single owner: the structured-validation panel renders the
                    // rejection (summary + verbatim issue codes) once; a second
                    // notice with the same string would duplicate it (A3).
                    self.library.notice = None;
                    self.library.validation = Some(validation);
                }
                engine::Msg::ModelDeleted { model, models } => {
                    if !library_response_is_current(
                        self.library_pending_request.as_ref(),
                        request_context.as_ref(),
                    ) {
                        continue;
                    }
                    self.library.busy = false;
                    self.library_pending_request = None;
                    self.library.pending_delete = None;
                    self.library.notice = Some((format!("Deleted {model}"), false));
                    self.models = models;
                    self.model_inventory_state = ModelInventoryState::Ready;
                }
                engine::Msg::Field(f) => {
                    let ps = flow::from_field(&f.shape, &f.data);
                    if !ps.is_empty() {
                        self.particles = ps;
                    }
                    if let Some((vol, dims)) = flow::vorticity_volume(&f.shape, &f.data) {
                        self.volume_data = std::sync::Arc::new(vol);
                        self.volume_dims = dims;
                        self.volume_version = self.volume_version.wrapping_add(1);
                    }
                    self.insights3d = flow::insights3d(&f.shape, &f.data);
                    let n = f.shape.get(1).copied().unwrap_or(0);
                    self.engine_status = format!("● Model field {n}³ · {}", f.scenario);
                    self.engine_ok = true;
                }
                engine::Msg::Field2D(f) => {
                    if let Some(t) = self.f2d_req_at.take() {
                        self.f2d_latency_ms = t.elapsed().as_millis() as u32;
                    }
                    self.f2d = Some(f);
                    self.f2d_gen = self.f2d_gen.wrapping_add(1);
                    self.f2d_pending = false;
                    self.engine_ok = true;
                    if self.f2d_dirty {
                        self.f2d_dirty = false;
                        self.request_2d();
                    }
                }
                engine::Msg::CadProgress(progress) => {
                    if let Some(case) = self.cad.as_mut() {
                        if let Some(pending) = case.pending_run.as_mut() {
                            if pending.request_id == progress.request_id {
                                pending.progress = Some(progress);
                                continue;
                            }
                        }
                    }
                    // Horizon-preview fetches may also stream stages; keep status calm.
                    self.engine_status = format!(
                        "◐ {} · stage {}/{}",
                        cad_progress_stage_label(&progress.stage),
                        progress.stage_index,
                        progress.stage_count
                    );
                }
                engine::Msg::CadField(f) => {
                    let disposition = classify_cad_result(
                        &f.request_id,
                        self.cad.as_ref().and_then(|case| {
                            case.playback
                                .fetching
                                .as_ref()
                                .map(|(step, request_id)| (*step, request_id.as_str()))
                        }),
                        self.cad
                            .as_ref()
                            .and_then(|case| case.pending_run.as_ref())
                            .map(|pending| pending.request_id.as_str()),
                    );
                    match disposition {
                        // A horizon-playback fetch: display-only, so it lands in
                        // the preview cache and nothing else is touched.
                        CadResultDisposition::Preview(step) => {
                            self.install_horizon_frame(step, f);
                            continue;
                        }
                        CadResultDisposition::DiscardStale => {
                            self.project_notice = Some((
                                format!(
                                    "Discarded CAD result for stale request {}. The active case and immutable runs were not changed.",
                                    short_id(&f.request_id)
                                ),
                                true,
                            ));
                            continue;
                        }
                        CadResultDisposition::Record => {}
                    }
                    self.enqueue_cad_field_ready(f);
                }
                engine::Msg::Benchmark(b) => {
                    self.bench_running = false;
                    let expected_seeds: Vec<u32> = (0..self.bench_seeds)
                        .map(|offset| self.bench_seed_start.saturating_add(offset))
                        .collect();
                    let current_request = b.model == self.f2d_model
                        && b.seeds == expected_seeds
                        && b.horizons == [1, 4, 8, 16];
                    if current_request {
                        match retain_after_persistence(b, |benchmark| {
                            self.persist_benchmark_run(benchmark)
                        }) {
                            Ok((run_id, benchmark)) => {
                                self.active_benchmark_run_id = Some(run_id);
                                self.bench = Some(benchmark);
                                self.bench_error = None;
                                self.engine_ok = true;
                                self.select_bench_cell(0, 0);
                            }
                            Err(error) => {
                                self.active_benchmark_run_id = None;
                                self.bench = None;
                                self.bench_error = Some(format!(
                                    "Suite completed, but immutable persistence failed and the transient result was discarded: {error}"
                                ));
                                self.project_notice = Some((
                                    format!(
                                        "Suite completed, but its immutable project run was not recorded; the transient result was discarded: {error}"
                                    ),
                                    true,
                                ));
                            }
                        }
                    } else {
                        self.bench_error = Some(
                            "discarded a suite result after its model or seed controls changed"
                                .into(),
                        );
                    }
                }
                engine::Msg::BenchmarkInspector(cell) => {
                    let belongs_to_selection = self.bench.as_ref().is_some_and(|benchmark| {
                        self.bench_selected
                            .is_some_and(|(seed_index, horizon_index)| {
                                benchmark.seeds.get(seed_index) == Some(&cell.seed)
                                    && benchmark.horizons.get(horizon_index) == Some(&cell.horizon)
                            })
                    });
                    if belongs_to_selection {
                        self.bench_inspector_pending = false;
                        match retain_after_persistence(cell, |inspector| {
                            self.persist_benchmark_inspector(inspector)
                        }) {
                            Ok(((), inspector)) => {
                                self.bench_inspector = Some(inspector);
                                self.bench_inspector_error = None;
                                self.bench_tex.clear();
                            }
                            Err(error) => {
                                self.bench_inspector = None;
                                self.bench_inspector_error = Some(format!(
                                    "Cell computation completed, but immutable persistence failed and the transient evidence was discarded: {error}"
                                ));
                                self.project_notice = Some((
                                    format!(
                                        "Cell evidence was not recorded and its transient result was discarded: {error}"
                                    ),
                                    true,
                                ));
                            }
                        }
                    } else if self.bench_inspector_pending {
                        self.bench_inspector_pending = false;
                        self.bench_inspector_error =
                            Some("discarded stale cell evidence after selection changed".into());
                    }
                    self.engine_ok = true;
                }
                engine::Msg::Error(e) => {
                    let disposition = classify_engine_error(
                        request_context.as_ref(),
                        self.cad.as_ref().and_then(|case| {
                            case.playback
                                .fetching
                                .as_ref()
                                .map(|(step, request_id)| (*step, request_id.as_str()))
                        }),
                        self.cad
                            .as_ref()
                            .and_then(|case| case.pending_run.as_ref())
                            .map(|pending| pending.request_id.as_str()),
                    );
                    if disposition == EngineErrorDisposition::CurrentRun {
                        let run_id = self
                            .cad
                            .as_ref()
                            .and_then(|case| case.pending_run.as_ref())
                            .map(|pending| pending.run_id.clone())
                            .expect("current-run errors have a matching pending run");
                        let is_timeout = e.to_ascii_lowercase().contains("timed out")
                            || e.to_ascii_lowercase().contains("timeout");
                        let reason = if is_timeout {
                            format!("timeout: {e}")
                        } else {
                            format!("sidecar_failure: {e}")
                        };
                        let persisted = self.finish_pending_external_flow_attempt(
                            project::LifecycleState::Failed,
                            &reason,
                        );
                        self.clear_pending_external_flow();
                        self.interrupt_and_restart_engine();
                        self.project_notice = Some(match persisted {
                            Ok(_) => (
                                format!(
                                    "Attempt {} {} and was persisted. The blocking sidecar was terminated; a fresh engine is starting for retry.",
                                    short_id(&run_id),
                                    if is_timeout { "timed out" } else { "failed" }
                                ),
                                true,
                            ),
                            Err(error) => (
                                format!(
                                    "Attempt {} failed and the sidecar was terminated, but terminal persistence failed: {error}",
                                    short_id(&run_id)
                                ),
                                true,
                            ),
                        });
                        continue;
                    }
                    if engine_error_is_stale(
                        disposition,
                        self.library_pending_request.as_ref(),
                        request_context.as_ref(),
                    ) {
                        continue;
                    }
                    let fatal = e.starts_with("engine io:")
                        || e.starts_with("engine unavailable:")
                        || e.starts_with("engine timeout:");
                    self.engine_status = format!("○ {e}");
                    if fatal {
                        self.engine_ok = false;
                    }
                    match disposition {
                        EngineErrorDisposition::Preview(step) => {
                            if let Some(case) = self.cad.as_mut() {
                                case.playback.fetching = None;
                                case.playback.failed.insert(step);
                                case.playback.playing = false;
                            }
                        }
                        EngineErrorDisposition::Field2D => {
                            self.f2d_pending = false;
                        }
                        EngineErrorDisposition::Benchmark => {
                            self.bench_running = false;
                            self.bench_error = Some(e);
                        }
                        EngineErrorDisposition::BenchmarkInspector => {
                            self.bench_inspector_pending = false;
                            self.bench_inspector_error = Some(e);
                        }
                        EngineErrorDisposition::Library(kind) => {
                            if library_response_is_current(
                                self.library_pending_request.as_ref(),
                                request_context.as_ref(),
                            ) && self
                                .library_pending_request
                                .as_ref()
                                .is_some_and(|request| request.kind == kind)
                            {
                                self.library.busy = false;
                                self.library_pending_request = None;
                                if kind == engine::RequestKind::ListModels {
                                    self.model_inventory_state =
                                        ModelInventoryState::Failed(e.clone());
                                }
                                self.library.notice = Some((e, true));
                            }
                        }
                        EngineErrorDisposition::Global => {}
                        EngineErrorDisposition::DiscardStale => {}
                        EngineErrorDisposition::CurrentRun => unreachable!(),
                    }
                }
            }
        }
        if engine_messages == ENGINE_MESSAGES_PER_FRAME {
            ui.ctx().request_repaint();
        }
        if self.is_research_sandbox() && ui.input(|i| i.key_pressed(egui::Key::G)) {
            self.regenerate();
        }
        if self.nav == Nav::Benchmark {
            self.bench_keyboard(ui);
        }
        self.handle_viewport_shortcuts(ui);
        self.advance_horizon_playback(ui.ctx());
        self.handle_case_edit_shortcuts(ui);
        self.handle_project_shortcuts(ui);
        if self.dependencies_dirty {
            self.project.reconcile_dependencies(
                self.engine_ok,
                self.models
                    .iter()
                    .filter(|model| model.status != "invalid")
                    .map(|model| model.checkpoint_sha256.as_str()),
            );
            self.dependencies_dirty = false;
        }
        let now = now_utc_unix();
        let orientation_blocks_persistence =
            self.orientation_draft.is_some() || self.orientation_pending.is_some();
        if self.has_unsaved_project_work()
            && !orientation_blocks_persistence
            && now >= self.next_autosave_utc_unix
        {
            let recovery_result = self.flush_project_drafts_for_persistence().and_then(|_| {
                self.project
                    .autosave_if_due(now, self.settings.autosave_interval_seconds as u64)
                    .map_err(|error| error.to_string())
            });
            let completed_utc_unix = now_utc_unix();
            match recovery_result {
                Ok(_) => {
                    self.next_autosave_utc_unix = autosave_deadline_after_attempt(
                        completed_utc_unix,
                        self.settings.autosave_interval_seconds as u64,
                        true,
                    );
                }
                Err(error) => {
                    self.project_notice =
                        Some((format!("Recovery snapshot failed: {error}"), true));
                    // Match the lifecycle's bounded retry without an idle poll:
                    // schedule one exact wake five seconds after the failed
                    // persistence work completed.
                    self.next_autosave_utc_unix =
                        autosave_deadline_after_attempt(completed_utc_unix, 0, false);
                }
            }
        }
        if ui.input(|input| input.viewport().close_requested()) && !self.allow_close {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.project_guard.pending().is_none() {
                self.request_project_action(
                    project_lifecycle::DeferredProjectAction::Quit,
                    ui.ctx(),
                );
            }
        }
        if self.live && self.is_research_sandbox() {
            self.live_timer += ui.input(|i| i.stable_dt);
            if self.live_timer > 2.5 {
                self.live_timer = 0.0;
                self.regenerate();
            }
        } else if !self.is_research_sandbox() {
            self.live = false;
            self.live_timer = 0.0;
        }
        #[cfg(target_os = "macos")]
        self.handle_menu_signals(ui.ctx());
        self.top_bar(ui);
        self.status_bar(ui);
        self.left_sidebar(ui);
        self.right_controls(ui);
        self.viewport(ui);
        self.command_palette(ui.ctx());
        self.show_unsaved_changes_dialog(ui.ctx());
        self.show_project_conflict_dialog(ui.ctx());
        // A project mutation can occur inside the UI callbacks above, after
        // this frame's reconciliation phase. Schedule exactly one follow-up
        // frame so availability is current without restoring idle polling.
        if self.dependencies_dirty {
            ui.ctx().request_repaint();
        }
        // Engine delivery is event-driven, widgets/animations own their own
        // cadence, and the sandbox live loop is intentionally low-frequency.
        // A dirty project wakes exactly for its next recovery deadline.
        let repaint_now = now_utc_unix();
        let (live_delay, autosave_delay) = background_repaint_delays(
            self.live,
            self.has_unsaved_project_work(),
            orientation_blocks_persistence,
            repaint_now,
            self.next_autosave_utc_unix,
        );
        for delay in [live_delay, autosave_delay].into_iter().flatten() {
            ui.ctx().request_repaint_after(delay);
        }
    }
}

/// Live diagnostics derived from the current field: (helicity, enstrophy, q, count).
fn diagnostics(ps: &[flow::Particle]) -> (f32, f32, f32, usize) {
    if ps.is_empty() {
        return (0.0, 0.0, 0.0, 0);
    }
    let n = ps.len() as f32;
    let mut hel = 0.0;
    let mut ens = 0.0;
    let mut q = 0.0;
    for p in ps {
        hel += p.vort * p.speed;
        ens += p.vort * p.vort;
        q += 0.5 * (p.speed * p.speed - p.vort * p.vort);
    }
    (hel / n * 0.1, ens / n, q / n, ps.len())
}

fn write_calculation_export<W: std::io::Write>(
    writer: &mut W,
    provenance: &serde_json::Value,
    samples: usize,
    helicity: f32,
    enstrophy: f32,
    q_criterion: f32,
    density_lo: f32,
    opacity: f32,
) -> Result<(), String> {
    let provenance_json = serde_json::to_string(provenance)
        .map_err(|error| format!("Could not serialize calculation provenance: {error}"))?;
    writeln!(
        writer,
        "# reyn_calculation_provenance_json={provenance_json}"
    )
    .and_then(|_| {
        writeln!(
            writer,
            "metric,value,units,source_class\n\
             samples,{samples},count,rendered_model_field\n\
             helicity,{helicity:.6e},display_normalized,derived_from_rendered_model_field\n\
             enstrophy,{enstrophy:.6e},display_normalized,derived_from_rendered_model_field\n\
             q_criterion,{q_criterion:.6},display_normalized,derived_from_rendered_model_field\n\
             density_lo,{density_lo:.3},display_fraction,viewport_setting\n\
             opacity,{opacity:.3},display_fraction,viewport_setting"
        )
    })
    .map_err(|error| format!("Calculation export write failed: {error}"))
}

fn calculation_export_provenance(
    project_manifest: &project::ProjectManifest,
    active_run_id: Option<&str>,
) -> serde_json::Value {
    let selected = active_run_id.and_then(|run_id| {
        project_manifest.cases().iter().find_map(|case| {
            let run = case.runs().iter().find(|run| run.run_id() == run_id)?;
            let revision = case
                .revisions()
                .iter()
                .find(|revision| revision.case_revision_id == run.case_revision_id())?;
            Some((case, run, revision))
        })
    });
    let sources = selected
        .map(|(_, _, revision)| {
            revision
                .source_revision_ids
                .iter()
                .filter_map(|source_id| {
                    project_manifest
                        .source_revisions()
                        .iter()
                        .find(|source| source.source_revision_id == *source_id)
                })
                .map(|source| {
                    serde_json::json!({
                        "source_revision_id": source.source_revision_id,
                        "source_sha256": source.content_sha256,
                        "source_kind": source.source_kind,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let solver = selected.and_then(|(_, run, _)| {
        run.manifest()
            .solver
            .as_ref()
            .or(run.manifest().engine.as_ref())
    });
    serde_json::json!({
        "schema": "reyn_calculation_export.v1",
        "project_id": project_manifest.project_id(),
        "case_id": selected.map(|(case, _, _)| case.case_id()),
        "case_revision_id": selected.map(|(_, run, _)| run.case_revision_id()),
        "run_id": selected.map(|(_, run, _)| run.run_id()),
        "parent_run_id": selected.and_then(|(_, run, _)| run.parent_run_id()),
        "sources": sources,
        "model_id": selected.and_then(|(_, run, _)| run.manifest().model.as_ref().map(|model| model.name.as_str())),
        "model_sha256": selected.and_then(|(_, run, _)| run.manifest().model.as_ref().and_then(|model| model.sha256.as_deref())),
        "solver": solver.map(|component| serde_json::json!({
            "name": component.name,
            "version": component.version,
            "sha256": component.sha256,
        })),
        "run_exact_contract": selected.map(|(_, run, _)| &run.manifest().exact_contract),
        "run_exact_settings": selected.map(|(_, run, _)| &run.manifest().exact_settings),
        "field_source": "current rendered model field",
        "limitations": "Viewport diagnostics are display-normalized derived quantities, not solver-reference validation metrics.",
    })
}

fn now_utc_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Section eyebrow — the single sanctioned caps style (`overline`, §3.1):
/// tracked, Medium weight, tertiary color. Scientific-state tokens use
/// `theme::chip_text`; everything else is sentence case.
/// Caption label that AccessKit controls can attach via [`egui::Response::labelled_by`].
fn a11y_caption(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.label(RichText::new(text).text_style(caption()).color(TEXT_MUTE))
}

/// Build engine request fields for the case's moment origin / aero reference.
fn cad_moment_engine_args(
    operating: &engineering::OperatingPoint,
    transform_4x4: [f64; 16],
) -> Result<(String, Option<[f32; 3]>, Option<f32>), String> {
    let mode = match operating.moment_origin_mode {
        engineering::MomentOriginMode::DiffuseSurfaceCentroid => {
            "diffuse_surface_centroid".to_string()
        }
        engineering::MomentOriginMode::SourceFramePoint => "source_frame_point".to_string(),
    };
    let origin = match operating.moment_origin_mode {
        engineering::MomentOriginMode::DiffuseSurfaceCentroid => None,
        engineering::MomentOriginMode::SourceFramePoint => {
            let scale = operating.length_unit.meters_per_unit().ok_or_else(|| {
                "Moment origin requires confirmed geometry units.".to_string()
            })?;
            let source_m = operating.moment_origin_source_m().ok_or_else(|| {
                "Moment origin coordinates are incomplete or non-finite.".to_string()
            })?;
            let solver =
                engineering::source_m_to_solver_point(source_m, transform_4x4, scale)?;
            Some([solver[0] as f32, solver[1] as f32, solver[2] as f32])
        }
    };
    Ok((
        mode,
        origin,
        operating
            .aero_reference_area_m2()
            .map(|area| area as f32),
    ))
}

fn caps(text: &str) -> RichText {
    overline_text(text)
}
fn mono(text: &str, color: Color32) -> RichText {
    RichText::new(text).monospace().color(color)
}

fn is_research_nav(nav: Nav) -> bool {
    matches!(
        nav,
        Nav::FlowPainter | Nav::Fields2D | Nav::Metrics | Nav::Benchmark
    )
}

fn nav_is_available(nav: Nav, developer_research_sandbox: bool) -> bool {
    !is_research_nav(nav) || developer_research_sandbox
}

impl ReynApp {
    fn is_research_sandbox(&self) -> bool {
        self.settings.developer_research_sandbox && is_research_nav(self.nav)
    }

    fn invalidate_cad_section(&mut self) {
        self.section_tex = None;
        self.section_data = None;
        self.section_sig = u64::MAX;
        self.section_error = None;
    }

    /// Drop only the colormapped texture caches (underlying data stays) so an
    /// appearance-preference change re-renders every field view immediately.
    fn invalidate_field_textures(&mut self) {
        self.invalidate_cad_section();
        self.f2d_tex.clear();
        self.f2d_sig = u64::MAX;
        self.bench_tex.clear();
        self.paint_tex = None;
    }

    fn invalidate_active_case_result(&mut self) {
        self.invalidate_cad_section();
        self.probe3d = None;
        if let Some(case) = &mut self.cad {
            if case.workflow.result.is_some() {
                case.workflow.parent_run_id = case.active_run_id.clone();
            }
            case.workflow.result = None;
            case.workflow.stage = engineering::CaseStage::Setup;
            case.surf = None;
            case.surf_mask_source = None;
            case.velocity.clear();
            case.pressure.clear();
            case.cp.clear();
            case.traction.clear();
            case.result_grid = 0;
            case.dt_frame = 0.0;
            case.pending = false;
            case.pending_request_id = None;
            case.pending_run = None;
            // Preview frames belong to the contract that produced them.
            case.playback.reset();
        }
        self.surface_on = false;
    }

    fn active_case_draft_scope(&self) -> Option<engineering::CaseDraftScope> {
        let case = self.cad.as_ref()?;
        Some(engineering::CaseDraftScope::new(
            self.project.manifest().project_id(),
            &case.workflow.case_id,
            case.workflow.source_revision_id.clone(),
            &case.workflow.preflight.source_sha256,
        ))
    }

    fn rebase_case_draft_history(&mut self) {
        self.case_edit_transaction = None;
        match self.active_case_draft_scope() {
            Some(scope) => self.case_draft_history.rebase(scope),
            None => self.case_draft_history.clear(),
        }
    }

    fn case_history_gate_reason(&self, action: CaseHistoryAction) -> Option<String> {
        if self.nav != Nav::Case {
            return Some("Open Case Setup to edit its draft.".into());
        }
        let Some(case) = self.cad.as_ref() else {
            return Some("No external-flow case draft is active.".into());
        };
        if self.project.availability().is_read_only_evidence() {
            return Some("This project is in read-only evidence mode.".into());
        }
        if case.pending {
            return Some("Cancel the in-flight run before changing its draft.".into());
        }
        let Some(scope) = self.active_case_draft_scope() else {
            return Some("No external-flow case draft is active.".into());
        };
        let available = match action {
            CaseHistoryAction::Undo => self.case_draft_history.can_undo(&scope),
            CaseHistoryAction::Redo => self.case_draft_history.can_redo(&scope),
        };
        (!available).then(|| {
            format!(
                "No reversible draft edit to {}. Immutable identity is excluded.",
                match action {
                    CaseHistoryAction::Undo => "undo",
                    CaseHistoryAction::Redo => "redo",
                }
            )
        })
    }

    fn apply_case_history_action(&mut self, action: CaseHistoryAction) {
        if let Some(reason) = self.case_history_gate_reason(action) {
            self.project_notice = Some((reason, false));
            return;
        }
        let Some(scope) = self.active_case_draft_scope() else {
            return;
        };
        let Some(current) = self
            .cad
            .as_ref()
            .map(|case| engineering::CaseDraftSnapshot::capture(&case.workflow))
        else {
            return;
        };
        let restored = match action {
            CaseHistoryAction::Undo => self.case_draft_history.undo(scope, current),
            CaseHistoryAction::Redo => self.case_draft_history.redo(scope, current),
        };
        let Some(restored) = restored else {
            return;
        };
        if let Some(case) = self.cad.as_mut() {
            restored.restore(&mut case.workflow);
        }
        self.case_edit_transaction = None;
        self.mark_case_draft_dirty();
        // Exactly the normal case-edit pathway: previews/current display state
        // become stale, readiness is recalculated from the restored draft, and
        // completed runs/evidence in the project manifest remain untouched.
        self.invalidate_active_case_result();
        self.project_notice = Some((
            format!(
                "{} case-draft edit. Completed runs and evidence remain immutable; readiness gates were re-evaluated.",
                match action {
                    CaseHistoryAction::Undo => "Undid",
                    CaseHistoryAction::Redo => "Redid",
                }
            ),
            false,
        ));
    }

    fn record_case_draft_change(
        &mut self,
        before: engineering::CaseDraftSnapshot,
        after: engineering::CaseDraftSnapshot,
        changed_transaction: Option<CaseEditTransaction>,
        active_transaction: Option<CaseEditTransaction>,
    ) {
        if self.project.availability().is_read_only_evidence() {
            self.rebase_case_draft_history();
            return;
        }
        let Some(scope) = self.active_case_draft_scope() else {
            self.case_draft_history.clear();
            self.case_edit_transaction = None;
            return;
        };
        let coalesce =
            changed_transaction.is_some() && changed_transaction == self.case_edit_transaction;
        self.case_draft_history
            .record_change(scope, before, &after, coalesce);
        self.case_edit_transaction = active_transaction;
        self.mark_case_draft_dirty();
    }

    fn handle_case_edit_shortcuts(&mut self, ui: &mut egui::Ui) {
        if self.nav != Nav::Case || self.palette_open {
            return;
        }
        // Let ordinary text fields own their native text undo. A focused case
        // numeric editor is tracked above and intentionally routes to the case
        // transaction instead.
        if ui.ctx().egui_wants_keyboard_input() && self.case_edit_transaction.is_none() {
            return;
        }
        let action = ui.input_mut(|input| {
            if input.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::Z,
            ) {
                return Some(CaseHistoryAction::Redo);
            }
            #[cfg(not(target_os = "macos"))]
            if input.consume_key(egui::Modifiers::COMMAND, egui::Key::Y) {
                return Some(CaseHistoryAction::Redo);
            }
            input
                .consume_key(egui::Modifiers::COMMAND, egui::Key::Z)
                .then_some(CaseHistoryAction::Undo)
        });
        if let Some(action) = action {
            self.apply_case_history_action(action);
        }
    }

    fn finish_pending_external_flow_attempt(
        &mut self,
        state: project::LifecycleState,
        reason: &str,
    ) -> Result<String, String> {
        if !matches!(
            state,
            project::LifecycleState::Failed | project::LifecycleState::Cancelled
        ) {
            return Err("external-flow interruption requires failed or cancelled state".into());
        }
        self.project_write_access("Persisting the terminal run attempt")?;
        let pending = self
            .cad
            .as_ref()
            .and_then(|case| case.pending_run.clone())
            .ok_or_else(|| "no external-flow attempt is pending".to_string())?;
        let now = now_utc_unix();
        let mut manifest = pending.manifest.clone();
        manifest.runtime_ms = pending.started_at.elapsed().as_millis() as u64;
        manifest.stop_reason = reason.replace(['\r', '\n'], " ");
        if state == project::LifecycleState::Failed {
            manifest.warnings.push(manifest.stop_reason.clone());
        }
        let run = project::RunRecord::new(
            pending.run_id.clone(),
            pending.parent_run_id,
            pending
                .workflow
                .case_revision_id
                .clone()
                .ok_or_else(|| "pending run has no case revision".to_string())?,
            pending.started_utc_unix,
            now,
            state,
            manifest,
            Vec::new(),
        );
        let case_id = pending.workflow.case_id;
        let run_id = pending.run_id;
        self.transact_project(
            "Persisting the terminal run attempt",
            now,
            move |project_manifest| project_manifest.finish_run_attempt(&case_id, run, now),
        )?;
        self.project
            .checkpoint_recovery(now)
            .map_err(|error| error.to_string())?;
        Ok(run_id)
    }

    fn clear_pending_external_flow(&mut self) {
        self.cad_field_ready_pending = None;
        if let Some(case) = self.cad.as_mut() {
            case.pending = false;
            case.pending_request_id = None;
            case.pending_run = None;
            case.workflow.stage = if case.workflow.ready() {
                engineering::CaseStage::Ready
            } else {
                engineering::CaseStage::Setup
            };
        }
        if let Some(next) = self.run_queue.waiting.pop_front() {
            let case_matches = self
                .cad
                .as_ref()
                .is_some_and(|case| case.workflow.case_id == next.case_id);
            if !case_matches {
                self.project_notice = Some((
                    format!(
                        "Queued follow-on for case {} was skipped because another case is active.",
                        short_id(&next.case_id)
                    ),
                    true,
                ));
                return;
            }
            let remaining = self.run_queue.waiting.len();
            self.project_notice = Some((
                format!(
                    "Run finished · starting queued follow-on ({}) · {} still waiting · queued at {}.",
                    next.note, remaining, next.queued_utc_unix
                ),
                false,
            ));
            self.run_external_flow();
        }
    }

    fn apply_case_view_state_from_active(&mut self) {
        let Some(view) = self
            .cad
            .as_ref()
            .map(|case| case.workflow.view_state.clone())
        else {
            return;
        };
        if let Some(name) = view.colormap.as_deref() {
            if let Ok(map) =
                serde_json::from_value::<field2d::FieldColormap>(serde_json::json!(name))
            {
                self.settings.colormap = map;
                field2d::set_view_colormap(map);
            }
        }
        if let Some(mode) = view.cp_range_mode.as_deref() {
            if let Ok(parsed) =
                serde_json::from_value::<settings::CpRangeMode>(serde_json::json!(mode))
            {
                self.settings.cp_range_mode = parsed;
            }
        }
        if let Some(extent) = view.cp_pinned_extent {
            self.settings.cp_pinned_extent = extent;
        }
        self.streamlines = view.streamlines;
        self.settings_draft = self.settings.clone();
    }

    fn interrupt_and_restart_engine(&mut self) {
        self.engine.terminate();
        self.engine = engine::EngineHandle::spawn_with_config(self.settings.engine_config());
        self.attach_engine_repaint_wake();
        self.engine_status = "○ Restarting engine after interrupted computation…".into();
        self.engine_ok = false;
        self.dependencies_dirty = true;
    }

    /// Persist cancellation, interrupt the blocking sidecar request, terminate
    /// the old process, and start a fresh engine so retry is immediately safe.
    fn cancel_external_flow(&mut self) {
        let Some((request_id, run_id)) = self.cad.as_ref().and_then(|case| {
            Some((
                case.pending_request_id.clone()?,
                case.pending_run.as_ref()?.run_id.clone(),
            ))
        }) else {
            return;
        };
        let persisted = self.finish_pending_external_flow_attempt(
            project::LifecycleState::Cancelled,
            "operator_cancelled",
        );
        self.clear_pending_external_flow();
        self.interrupt_and_restart_engine();
        self.project_notice = Some(match persisted {
            Ok(_) => (
                format!(
                    "Cancelled attempt {} (request {}). The blocking sidecar was terminated and a fresh engine is starting; retry is available when it is ready.",
                    short_id(&run_id),
                    short_id(&request_id)
                ),
                false,
            ),
            Err(error) => (
                format!(
                    "The sidecar was terminated, but cancelled attempt {} could not be checkpointed: {error}",
                    short_id(&run_id)
                ),
                true,
            ),
        });
    }

    /// Ask the engine for one horizon step of the completed case, for playback.
    /// The engine caches the solver warmup per mask and Reynolds number, so this
    /// is about one model pass, and every fetched step is cached here.
    fn request_horizon_step(&mut self, step: u32) {
        if self.project.availability().is_read_only_evidence() {
            self.project_notice = Some((
                "Uncomputed horizon previews are unavailable in read-only evidence mode. Stored run fields remain inspectable."
                    .into(),
                true,
            ));
            return;
        }
        if !self.engine_ok {
            self.project_notice = Some((
                "The local engine is unavailable, so uncomputed horizon steps cannot be fetched. Stored steps remain viewable.".into(),
                true,
            ));
            return;
        }
        let Some(case) = self.cad.as_mut() else {
            return;
        };
        if case.pending || case.workflow.result.is_none() {
            return;
        }
        if step == 0 || step > case.workflow.model_max_steps {
            return;
        }
        if step == case.steps || case.playback.frames.contains_key(&step) {
            return;
        }
        if case.playback.fetching.is_some() {
            return; // one preview pass in flight at a time
        }
        let scale = case
            .workflow
            .operating
            .length_unit
            .meters_per_unit()
            .unwrap_or(1.0);
        let (moment_origin_mode, moment_origin_solver, reference_area_m2) =
            match cad_moment_engine_args(
                &case.workflow.operating,
                case.workflow.preflight.transform_4x4,
            ) {
                Ok(args) => args,
                Err(error) => {
                    case.playback.failed.insert(step);
                    self.project_notice = Some((error, true));
                    return;
                }
            };
        let request_id = format!("cad-horizon-{}", uuid::Uuid::new_v4());
        let request = engine::Cmd::CadPredict {
            request_id: request_id.clone(),
            model: case.model.clone(),
            steps: step,
            mask: case.mask.clone(),
            reynolds: case.workflow.operating.reynolds().unwrap_or_default() as f32,
            characteristic_length_solver: case.workflow.preflight.solver_characteristic_length
                as f32,
            reference_length_m: (case.workflow.operating.reference_length * scale) as f32,
            velocity_mps: case.workflow.operating.velocity as f32,
            density_kg_m3: case.workflow.operating.density as f32,
            reference_pressure_pa: case.workflow.operating.reference_pressure as f32,
            moment_origin_mode,
            moment_origin_solver,
            reference_area_m2,
        };
        if self.engine.send(request).is_err() {
            case.playback.failed.insert(step);
            self.project_notice = Some(("Engine request channel is unavailable.".into(), true));
            return;
        }
        case.playback.failed.remove(&step);
        case.playback.fetching = Some((step, request_id));
    }

    /// Move playback to `step`, fetching it if it is not cached yet.
    fn show_horizon_step(&mut self, step: u32) {
        let Some(case) = self.cad.as_mut() else {
            return;
        };
        let max = case.workflow.model_max_steps.max(case.steps).max(1);
        let step = step.clamp(1, max);
        if case.display_step() == step {
            return;
        }
        case.playback.step = step;
        case.playback.trim(case.steps);
        self.probe3d = None;
        self.invalidate_cad_section();
        if self
            .cad
            .as_ref()
            .is_some_and(|case| case.display_fields().is_some())
        {
            self.refresh_display_layers();
        } else {
            self.request_horizon_step(step);
        }
    }

    /// Advance playback while it is playing. One step per 500 ms of wall clock:
    /// this is a sequence of model predictions, not a real-time simulation, so
    /// the rate is a reading rate and is labeled as such.
    fn advance_horizon_playback(&mut self, ctx: &egui::Context) {
        let Some(case) = self.cad.as_ref() else {
            return;
        };
        if !case.playback.playing || case.workflow.result.is_none() {
            return;
        }
        let horizon_max = case.workflow.model_max_steps.max(case.steps).max(1);
        let current = case.display_step();
        let waiting = case.playback.fetching.is_some();
        let now = ctx.input(|input| input.time);
        let due = now - case.playback.last_advance >= 0.5;
        ctx.request_repaint_after(std::time::Duration::from_millis(120));
        if waiting || !due {
            return;
        }
        let next = if current >= horizon_max {
            1
        } else {
            current + 1
        };
        if let Some(case) = self.cad.as_mut() {
            case.playback.last_advance = now;
        }
        self.show_horizon_step(next);
    }

    /// Cache a fetched playback step and, if it is the step on screen, show it.
    /// Preview frames never touch `workflow.result`, the content store, or the
    /// run ledger: the recorded run stays exactly as it was recorded.
    fn install_horizon_frame(&mut self, step: u32, field: engine::CadField) {
        let Some(case) = self.cad.as_mut() else {
            return;
        };
        case.playback.fetching = None;
        case.playback.failed.remove(&step);
        // The engine echoes the request mask. Preserve Arc identity when it is
        // byte-for-byte the active geometry so downstream display refresh can
        // reuse the already-transposed GPU mask and its upload version.
        let frame_mask = if field.mask.as_slice() == case.mask.as_slice() {
            case.mask.clone()
        } else {
            std::sync::Arc::new(field.mask)
        };
        case.playback.frames.insert(
            step,
            HorizonFrame {
                n: field.n,
                velocity: field.vel,
                pressure: field.pressure,
                cp: field.cp.clone(),
                traction: field.traction,
                mask: frame_mask,
                force_coefficients: field.force_coefficients,
                cp_min: field.cp.iter().copied().fold(f32::INFINITY, f32::min),
                cp_max: field.cp.iter().copied().fold(f32::NEG_INFINITY, f32::max),
            },
        );
        let recorded = case.steps;
        case.playback.trim(recorded);
        self.engine_ok = true;
        if self.cad.as_ref().map(CadCase::display_step) == Some(step) {
            self.invalidate_cad_section();
            self.refresh_display_layers();
            self.engine_status = format!("● Horizon step {step} · model prediction preview");
        }
    }

    /// Rebuild the 3D layers (particles, vorticity volume, insight markers, and
    /// the surface texture) from whichever horizon step is displayed.
    fn refresh_display_layers(&mut self) {
        let Some((
            particles,
            volume,
            insights,
            new_mask_bytes,
            reused_mask,
            displayed_mask_source,
            cp_bytes,
            n,
        )) = self.cad.as_ref().and_then(|case| {
            let fields = case.display_fields()?;
            let shape = [3usize, fields.n, fields.n, fields.n];
            let particles = flow::from_field(&shape, fields.velocity);
            let volume = flow::vorticity_volume(&shape, fields.velocity);
            let mut insights = flow::insights3d(&shape, fields.velocity);
            insights.extend(cad::surface_insights(fields.mask, fields.cp, fields.n));
            let n = fields.n;
            let displayed_mask_source = if fields.recorded {
                case.mask.clone()
            } else {
                case.playback.frames.get(&fields.step)?.mask.clone()
            };
            let cp_scale = fields
                .cp
                .iter()
                .fold(0.0f32, |scale, value| scale.max(value.abs()))
                .max(1e-6);
            let reused_mask = case
                .surf
                .as_ref()
                .filter(|surface| {
                    surface.dims == [n as u32; 3]
                        && case.surf_mask_source.as_ref().is_some_and(|source| {
                            std::sync::Arc::ptr_eq(source, &displayed_mask_source)
                        })
                })
                .map(|surface| (surface.mask.clone(), surface.mask_version));
            let mut mask_u8 = reused_mask.is_none().then(|| vec![0u8; n * n * n]);
            let mut cp_u8 = vec![128u8; n * n * n];
            if let Some(mask_u8) = mask_u8.as_mut() {
                for i in 0..n {
                    for j in 0..n {
                        for k in 0..n {
                            let source = i * n * n + j * n + k;
                            let target = (k * n + j) * n + i;
                            mask_u8[target] = cad_field_ready::surface_mask_u8_sample(fields.mask[source]);
                        }
                    }
                }
            }
            for i in 0..n {
                for j in 0..n {
                    for k in 0..n {
                        let source = i * n * n + j * n + k;
                        let target = (k * n + j) * n + i;
                        let normalized = (fields.cp[source] / cp_scale) * 0.5 + 0.5;
                        cp_u8[target] = (normalized.clamp(0.0, 1.0) * 255.0) as u8;
                    }
                }
            }
            Some((
                particles,
                volume,
                insights,
                mask_u8,
                reused_mask,
                displayed_mask_source,
                cp_u8,
                n,
            ))
        })
        else {
            return;
        };
        if !particles.is_empty() {
            self.particles = particles;
        }
        if let Some((data, dims)) = volume {
            self.volume_data = std::sync::Arc::new(data);
            self.volume_dims = dims;
            self.volume_version = self.volume_version.wrapping_add(1);
        }
        self.insights3d = insights;
        self.cad_version = self.cad_version.wrapping_add(1);
        let (mask, mask_version) = match reused_mask {
            Some(reused) => reused,
            None => (
                std::sync::Arc::new(
                    new_mask_bytes.expect("a non-reused surface mask was converted"),
                ),
                self.cad_version,
            ),
        };
        let surface = gpu::SurfaceData {
            mask,
            pressure: std::sync::Arc::new(cp_bytes),
            dims: [n as u32; 3],
            mask_version,
            pressure_version: self.cad_version,
        };
        if let Some(case) = self.cad.as_mut() {
            case.surf = Some(surface);
            case.surf_mask_source = Some(displayed_mask_source);
        }
    }

    /// Physical seconds per horizon step for the active case, when the run
    /// reported the frame interval and the operating point is complete.
    fn seconds_per_horizon_step(&self) -> Option<f64> {
        let case = self.cad.as_ref()?;
        let scale = case.workflow.operating.length_unit.meters_per_unit()?;
        engineering::seconds_per_horizon_step(
            case.dt_frame as f64,
            case.workflow.preflight.solver_characteristic_length,
            case.workflow.operating.reference_length * scale,
            case.workflow.operating.velocity,
        )
    }

    /// Report the surface values under a 3D click: march the displayed mask to
    /// the first solid cell, then read the adjacent fluid cell the diffuse-
    /// interface loads are defined on — the same cells the 2D probe reads.
    fn probe_surface_at(&mut self, rect: Rect, screen: egui::Pos2) {
        let slice = [
            self.slice[0].then(|| self.slice_pos[0] * 2.0 - 1.0),
            self.slice[1].then(|| self.slice_pos[1] * 2.0 - 1.0),
            self.slice[2].then(|| self.slice_pos[2] * 2.0 - 1.0),
        ];
        let (origin, direction) = self.cam.ray(rect, screen);
        let probe = self.cad.as_ref().and_then(|case| {
            let fields = case.display_fields()?;
            let (_, surface) =
                cad::pick_solid_voxel(fields.mask, fields.n, origin, direction, slice)?;
            let index = surface[0] * fields.n * fields.n + surface[1] * fields.n + surface[2];
            let solver_point = std::array::from_fn(|axis| {
                (surface[axis] as f64 + 0.5) * std::f64::consts::TAU / fields.n as f64
            });
            let meters_per_source_unit = case
                .workflow
                .operating
                .length_unit
                .meters_per_unit()
                .unwrap_or(1.0);
            Some(SurfaceProbe {
                anchor: cad::voxel_center(surface, fields.n),
                cell: surface,
                source_m: engineering::solver_point_to_source_m(
                    solver_point,
                    case.workflow.preflight.transform_4x4,
                    meters_per_source_unit,
                )
                .ok(),
                cp: fields.cp.get(index).copied().unwrap_or(f32::NAN),
                pressure_pa: fields.pressure.get(index).copied().unwrap_or(f32::NAN),
                traction_pa: std::array::from_fn(|axis| {
                    fields
                        .traction
                        .get(axis * fields.n.pow(3) + index)
                        .copied()
                        .unwrap_or(f32::NAN)
                }),
                step: fields.step,
                recorded: fields.recorded,
            })
        });
        match probe {
            Some(probe) => self.probe3d = Some(probe),
            None => {
                self.probe3d = None;
                if self
                    .cad
                    .as_ref()
                    .is_some_and(|case| case.display_fields().is_some())
                {
                    self.project_notice = Some((
                        "No body surface under that point — the probe reports values only where the stored mask is solid.".into(),
                        false,
                    ));
                }
            }
        }
    }

    /// Queue a body-attitude re-voxelization without running mesh parsing or the
    /// O(N³) classification on egui's thread. The latest request replaces the
    /// active generation; queued generations are coalesced by the worker and a
    /// generation that was already computing is discarded when it arrives.
    fn apply_body_orientation(&mut self, angles: [f64; 3]) {
        if self.reject_project_mutation("Changing body orientation") {
            return;
        }
        let Some(case) = self.cad.as_ref() else {
            return;
        };
        if case.pending {
            self.project_notice = Some((
                "A run is in flight. Cancel it before changing body orientation.".into(),
                true,
            ));
            return;
        }
        let digest = case.workflow.preflight.source_sha256.clone();
        let grid = case.workflow.preflight.target_grid;
        let case_id = case.workflow.case_id.clone();
        let source_name = case.workflow.source_name.clone();
        let selected_shell_entity_id = case.workflow.preflight.selected_shell_entity_id;
        let Some(bytes) = self.project.content_bytes(&digest).map(<[u8]>::to_vec) else {
            self.project_notice = Some((
                format!(
                    "Geometry object {} must be relinked before orientation can be re-applied.",
                    short_hash(&digest)
                ),
                true,
            ));
            return;
        };
        if self.orientation_worker.is_none() {
            self.orientation_worker = match OrientationWorker::spawn(self.repaint_context.clone()) {
                Ok(worker) => Some(worker),
                Err(error) => {
                    self.next_autosave_utc_unix =
                        autosave_deadline_after_attempt(now_utc_unix(), 0, false);
                    self.project_notice = Some((error, true));
                    return;
                }
            };
        }
        self.orientation_generation = self.orientation_generation.wrapping_add(1).max(1);
        let generation = self.orientation_generation;
        let request_id = format!("orientation-{generation}-{}", uuid::Uuid::new_v4().simple());
        let request = OrientationWorkRequest {
            generation,
            request_id: request_id.clone(),
            case_id: case_id.clone(),
            source_sha256: digest.clone(),
            source_name,
            angles,
            grid,
            source_bytes: bytes,
            selected_shell_entity_id,
        };
        let sent = self
            .orientation_worker
            .as_ref()
            .expect("orientation worker was initialized")
            .request_tx
            .send(request);
        if sent.is_err() {
            self.orientation_worker = None;
            self.next_autosave_utc_unix = autosave_deadline_after_attempt(now_utc_unix(), 0, false);
            self.project_notice = Some((
                "Body orientation was not queued because the re-voxelization worker stopped. The existing mask and immutable evidence were not changed."
                    .into(),
                true,
            ));
            return;
        }
        self.orientation_pending = Some(PendingOrientation {
            generation,
            request_id: request_id.clone(),
            case_id,
            source_sha256: digest,
            angles,
            started_at: std::time::Instant::now(),
            kind: PendingOrientationKind::Draft,
        });
        self.engine_status = format!(
            "● Re-voxelizing body orientation · request {}",
            short_id(&request_id)
        );
        self.project_notice = Some((
            "Body orientation re-voxelization is running off the UI thread. Progress is indeterminate; runs, saves, and recovery snapshots remain gated until the matching generation completes."
                .into(),
            false,
        ));
    }

    fn handle_orientation_results(&mut self) {
        let completed: Vec<_> = self
            .orientation_worker
            .as_ref()
            .map(|worker| worker.result_rx.try_iter().collect())
            .unwrap_or_default();
        for mut completed in completed {
            let current_case = self.cad.as_ref().map(|case| {
                (
                    case.workflow.case_id.as_str(),
                    case.workflow.preflight.source_sha256.as_str(),
                )
            });
            match classify_orientation_result(
                &completed,
                self.orientation_pending.as_ref(),
                current_case,
            ) {
                OrientationResultDisposition::DiscardStale => {
                    let completed_was_active =
                        self.orientation_pending.as_ref().is_some_and(|pending| {
                            pending.generation == completed.generation
                                && pending.request_id == completed.request_id
                        });
                    if completed_was_active {
                        let mutates_project = self
                            .orientation_pending
                            .as_ref()
                            .is_some_and(PendingOrientation::mutates_project);
                        self.orientation_pending = None;
                        if mutates_project {
                            self.next_autosave_utc_unix = autosave_deadline_after_attempt(
                                completed.completed_utc_unix,
                                0,
                                false,
                            );
                        }
                    }
                    continue;
                }
                OrientationResultDisposition::Failed => {
                    let error = completed
                        .result
                        .err()
                        .unwrap_or_else(|| "unknown orientation worker failure".into());
                    let pending = self
                        .orientation_pending
                        .take()
                        .expect("current orientation failure has pending state");
                    match pending.kind {
                        PendingOrientationKind::Draft => self.finish_orientation_failure(
                            completed.completed_utc_unix,
                            &completed.request_id,
                            &error,
                        ),
                        PendingOrientationKind::Hydrate(_) => {
                            self.finish_orientation_hydration_failure(&completed.request_id, &error)
                        }
                    }
                }
                OrientationResultDisposition::Apply => {
                    let vm = std::mem::replace(
                        &mut completed.result,
                        Err("orientation result was already consumed".into()),
                    )
                    .expect("successful orientation disposition has a voxel mask");
                    let pending = self
                        .orientation_pending
                        .take()
                        .expect("current orientation completion has pending state");
                    match pending.kind {
                        PendingOrientationKind::Draft => {
                            self.install_body_orientation(completed, vm)
                        }
                        PendingOrientationKind::Hydrate(hydration) => {
                            self.install_hydrated_body_orientation(*hydration, completed, vm)
                        }
                    }
                }
            }
        }
    }

    fn finish_orientation_failure(
        &mut self,
        completed_utc_unix: u64,
        request_id: &str,
        error: &str,
    ) {
        self.orientation_pending = None;
        self.next_autosave_utc_unix = autosave_deadline_after_attempt(completed_utc_unix, 0, false);
        self.engine_status = "○ Body orientation re-voxelization failed".into();
        self.project_notice = Some((
            format!(
                "Oriented voxel preflight {} failed: {error}. The prior mask, result view, immutable runs, and evidence remain unchanged; the draft angles are still pending.",
                short_id(request_id)
            ),
            true,
        ));
    }

    fn finish_orientation_hydration_failure(&mut self, request_id: &str, error: &str) {
        self.orientation_pending = None;
        self.engine_status = "○ Stored case geometry reconstruction failed".into();
        self.project_notice = Some((
            format!(
                "Stored case geometry request {} could not be reconstructed: {error}. The project manifest and immutable evidence remain available and unchanged.",
                short_id(request_id)
            ),
            true,
        ));
    }

    /// Install only the currently requested generation. Result invalidation and
    /// lineage changes occur here, after successful voxelization, so failed or
    /// stale work cannot erase the last valid geometry/result.
    fn install_body_orientation(&mut self, completed: OrientationWorkResult, vm: cad::VoxelMask) {
        if let Err(error) = self.project_write_access("Completing body orientation") {
            self.finish_orientation_failure(
                completed.completed_utc_unix,
                &completed.request_id,
                &format!(
                    "{error} The completed geometry was discarded before it could mutate the case"
                ),
            );
            return;
        }
        if self.cad.as_ref().is_some_and(|case| case.pending) {
            self.finish_orientation_failure(
                completed.completed_utc_unix,
                &completed.request_id,
                "a run entered flight before completion; the completed geometry was discarded",
            );
            return;
        }
        let Some(case) = self.cad.as_mut() else {
            self.finish_orientation_failure(
                completed.completed_utc_unix,
                &completed.request_id,
                "the active case no longer exists",
            );
            return;
        };
        let preflight = &mut case.workflow.preflight;
        // Record the attitude the mask was actually built with, not the request.
        let applied = vm.orientation.to_degrees();
        preflight.angle_of_attack_deg = applied[0];
        preflight.yaw_deg = applied[1];
        preflight.roll_deg = applied[2];
        preflight.proposed_scale = vm.scale;
        preflight.solver_characteristic_length = vm.char_len as f64;
        preflight.transform_4x4 = vm.transform_4x4;
        preflight.solid_voxels = vm.solid_voxels;
        preflight.voxel_components = vm.components;
        preflight.minimum_cells_across = vm.minimum_cells_across;
        preflight.boundary_clearance_cells = vm.boundary_clearance_cells;
        preflight.voxel_axis_disagreement_fraction = vm.axis_disagreement_fraction;
        preflight.voxel_odd_crossing_rows = vm.odd_crossing_rows;
        preflight.voxel_classification_version = vm.classification_version;
        if !preflight.analyzed_mesh_sha256.is_empty() {
            preflight
                .import_steps
                .push(engineering::GeometryImportStep {
                    evidence_class: "orient".into(),
                    operation: "body_orientation_revoxelize".into(),
                    parameters: vec![
                        ("angle_of_attack_deg".into(), format!("{:.6}", applied[0])),
                        ("yaw_deg".into(), format!("{:.6}", applied[1])),
                        ("roll_deg".into(), format!("{:.6}", applied[2])),
                        (
                            "voxel_classification_version".into(),
                            vm.classification_version.to_string(),
                        ),
                        ("solid_voxels".into(), vm.solid_voxels.to_string()),
                    ],
                    input_mesh_sha256: Some(preflight.analyzed_mesh_sha256.clone()),
                    output_mesh_sha256: Some(preflight.analyzed_mesh_sha256.clone()),
                    source_signed_volume: Some(preflight.source_signed_volume),
                    source_extents: Some(preflight.source_extents),
                });
        }
        // Approval covers units, orientation, scale, and placement, so a new
        // attitude is a new thing to approve.
        preflight.transform_approved = false;
        case.mask_bounds = cad::mask_bounds(&vm.mask, vm.n);
        case.mask = std::sync::Arc::new(vm.mask);
        let summary = case.workflow.preflight.body_orientation_summary();
        self.orientation_pending = None;
        self.invalidate_active_case_result();
        if let Some(case) = &mut self.cad {
            case.workflow.stage = engineering::CaseStage::Preflight;
        }
        self.mark_case_draft_dirty();
        if self.orientation_draft == Some(completed.angles) {
            self.orientation_draft = None;
        }
        let revision_result = self.commit_active_case_revision();
        self.next_autosave_utc_unix = autosave_deadline_after_attempt(
            completed.completed_utc_unix,
            self.settings.autosave_interval_seconds as u64,
            revision_result.is_ok(),
        );
        if let Err(error) = revision_result {
            self.project_notice = Some((
                format!(
                    "Body orientation request {} completed and the old result was invalidated, but the case revision was not recorded: {error}",
                    short_id(&completed.request_id)
                ),
                true,
            ));
            return;
        }
        self.engine_status = format!("● Body orientation: {summary} · re-approve the transform");
        self.project_notice = Some((
            format!(
                "Body re-oriented ({summary}) against the fixed +X stream. Transform approval re-opened and results cleared."
            ),
            false,
        ));
    }

    /// Complete recovery/open reconstruction without changing the persisted
    /// preflight or creating lineage. The stored transform remains
    /// authoritative; this worker result supplies only its deterministic mask.
    fn install_hydrated_body_orientation(
        &mut self,
        hydration: PendingOrientationHydration,
        completed: OrientationWorkResult,
        vm: cad::VoxelMask,
    ) {
        let workflow = hydration.workflow;
        let model = workflow.model_id.clone();
        let name = workflow.source_name.clone();
        let steps = workflow.operating.horizon_steps;
        let has_result = workflow.result.is_some();
        let mask_bounds = cad::mask_bounds(&vm.mask, vm.n);
        let mask = std::sync::Arc::new(vm.mask);
        self.orientation_pending = None;
        self.orientation_draft = None;
        self.current_model = model.clone();
        self.invalidate_cad_section();
        self.cad = Some(CadCase {
            mask,
            mask_bounds,
            model,
            steps,
            surf: None,
            surf_mask_source: None,
            name: name.clone(),
            workflow,
            velocity: Vec::new(),
            pressure: Vec::new(),
            cp: Vec::new(),
            traction: Vec::new(),
            result_grid: 0,
            dt_frame: hydration.dt_frame,
            active_run_id: hydration.selected_run_id,
            pending: false,
            pending_request_id: None,
            pending_run: None,
            playback: HorizonPlayback::default(),
        });
        self.rebase_case_draft_history();
        self.nav = if has_result { Nav::Evidence } else { Nav::Case };
        self.engine_status = format!(
            "● Reconstructed stored case {name} @ {}³ · request {}",
            vm.n,
            short_id(&completed.request_id)
        );
        self.project_notice = Some((
            "Stored body orientation and voxel mask were reconstructed off the UI thread. The project manifest, case lineage, runs, and evidence were not mutated."
                .into(),
            false,
        ));
    }

    fn commit_active_case_revision(&mut self) -> Result<(), String> {
        self.project_write_access("Recording the Case Setup draft")?;
        let case = self
            .cad
            .as_ref()
            .ok_or_else(|| "No external-flow case is active.".to_string())?;
        let mut workflow = case.workflow.clone();
        let parent_revision_id = workflow
            .case_revision_id
            .clone()
            .ok_or_else(|| "The active case has no persisted revision.".to_string())?;
        let source_revision_id = workflow
            .source_revision_id
            .clone()
            .ok_or_else(|| "The active case has no source revision.".to_string())?;
        let current_source = self
            .project
            .manifest()
            .source_revisions()
            .iter()
            .find(|source| source.source_revision_id == source_revision_id)
            .cloned()
            .ok_or_else(|| "The active geometry source revision is unavailable.".to_string())?;
        let approved_units = workflow.operating.length_unit.symbol().to_string();
        let approved_frame =
            "approved geometry source frame to fixed-body solver frame".to_string();
        let source_changed = current_source.declared_units.as_deref()
            != Some(approved_units.as_str())
            || current_source.frame.as_deref() != Some(approved_frame.as_str())
            || current_source.transform_4x4 != workflow.preflight.transform_4x4;
        let approved_source = source_changed.then(|| {
            let approved_source_id = format!("source-{}", uuid::Uuid::new_v4());
            workflow.source_revision_id = Some(approved_source_id.clone());
            let mut warnings = current_source.warnings.clone();
            warnings.extend(workflow.preflight.warnings.clone());
            warnings.extend(
                workflow
                    .preflight
                    .waivers
                    .iter()
                    .map(|waiver| format!("APPROVED WAIVER · {waiver}")),
            );
            warnings.sort();
            warnings.dedup();
            project::SourceRevision {
                source_revision_id: approved_source_id,
                source_kind: project::SourceKind::Geometry,
                revision: current_source.revision.saturating_add(1),
                imported_utc_unix: now_utc_unix(),
                uri_hint: current_source.uri_hint.clone(),
                byte_size: current_source.byte_size,
                content_sha256: current_source.content_sha256.clone(),
                declared_units: Some(approved_units),
                frame: Some(approved_frame),
                transform_4x4: workflow.preflight.transform_4x4,
                parent_revision_id: Some(current_source.source_revision_id.clone()),
                warnings,
            }
        });
        let discretization = serde_json::json!({
                "grid": [
                    workflow.preflight.target_grid,
                    workflow.preflight.target_grid,
                    workflow.preflight.target_grid,
                ],
                "solid_voxels": workflow.preflight.solid_voxels,
                "voxel_components": workflow.preflight.voxel_components,
                "minimum_cells_across": workflow.preflight.minimum_cells_across,
                "boundary_clearance_cells": workflow.preflight.boundary_clearance_cells,
                "axis_disagreement_fraction": workflow.preflight.voxel_axis_disagreement_fraction,
                "odd_crossing_rows_xyz": workflow.preflight.voxel_odd_crossing_rows,
                "classification_version": workflow.preflight.voxel_classification_version,
                "transform_4x4": workflow.preflight.transform_4x4,
        });
        let outputs = serde_json::json!({
                "velocity": {"source": "model_prediction", "units": "m/s"},
                "pressure": {"source": "recovered", "units": "Pa"},
                "cp": {"source": "derived_from_recovered_pressure", "units": "1"},
                "surface_loads": {
                    "source": "derived",
                    "method": engineering::SURFACE_LOAD_METHOD,
                    "downstream_use": "FEA mapping",
                },
                "field_artifact": {
                    "schema": engineering::ENGINEERING_FIELD_SCHEMA,
                    "layout": ["velocity_x", "velocity_y", "velocity_z", "pressure_pa", "mask", "cp", "traction_x_pa", "traction_y_pa", "traction_z_pa"],
                },
        });
        let active_case = self
            .project
            .manifest()
            .case(&workflow.case_id)
            .cloned()
            .ok_or_else(|| "The active external-flow case is unavailable.".to_string())?;
        workflow.case_revision_id = Some(parent_revision_id.clone());
        let active_contract = workflow.exact_contract();
        let unchanged = approved_source.is_none()
            && active_case.active_revision().contract == active_contract
            && active_case.active_revision().discretization == discretization
            && active_case.active_revision().outputs == outputs;
        if unchanged {
            self.case_draft_dirty = false;
            return Ok(());
        }
        let revision_id = format!("case-revision-{}", uuid::Uuid::new_v4());
        workflow.case_revision_id = Some(revision_id.clone());
        let source_revision_id = workflow
            .source_revision_id
            .clone()
            .expect("approved workflow retains a source revision");
        let revision = project::CaseRevision {
            case_revision_id: revision_id.clone(),
            parent_revision_id: Some(parent_revision_id),
            created_utc_unix: now_utc_unix(),
            source_revision_ids: vec![source_revision_id],
            contract: workflow.exact_contract(),
            discretization,
            outputs,
        };
        let case_id = workflow.case_id.clone();
        self.transact_project(
            "Recording the Case Setup draft",
            now_utc_unix(),
            move |manifest| {
                if let Some(source) = approved_source {
                    manifest.add_source_revision(source, now_utc_unix())?;
                }
                manifest.append_case_revision(&case_id, revision, now_utc_unix())?;
                Ok(())
            },
        )?;
        self.dependencies_dirty = true;
        if let Some(case) = &mut self.cad {
            case.workflow.case_revision_id = Some(revision_id);
            case.workflow.source_revision_id = workflow.source_revision_id;
        }
        // A persisted revision/source transition is a history boundary. Undo
        // remains draft-only and never walks lineage backwards.
        self.rebase_case_draft_history();
        self.case_draft_dirty = false;
        Ok(())
    }

    fn external_flow_attempt_manifest(
        &self,
        workflow: &engineering::ExternalFlowCase,
        request_id: &str,
    ) -> project::RunManifest {
        project::RunManifest {
            schema_version: project::PROJECT_SCHEMA_VERSION,
            app: project::VersionedComponent {
                name: "Reyn Studio".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                sha256: None,
            },
            engine: Some(project::VersionedComponent {
                name: "reyn_engine.py".into(),
                version: "engineering_result.v1".into(),
                sha256: None,
            }),
            model: Some(project::VersionedComponent {
                name: workflow.model_id.clone(),
                version: "reynmodel-bundle-v1".into(),
                sha256: workflow
                    .model_sha256
                    .clone()
                    .filter(|digest| digest.len() == 64),
            }),
            solver: Some(project::VersionedComponent {
                name: "ObstacleFlowSolver3D warmup".into(),
                version: "fixed_body".into(),
                sha256: None,
            }),
            converter: Some(project::VersionedComponent {
                name: engineering::SURFACE_LOAD_METHOD.into(),
                version: "1".into(),
                sha256: None,
            }),
            exact_contract: workflow.exact_contract(),
            exact_settings: serde_json::json!({
                "submitted_request_id": request_id,
                "preprocessing_transform": workflow.preflight.transform_4x4,
                "grid": workflow.preflight.target_grid,
                "flow_direction": workflow.operating.flow_direction,
                "approved_waivers": workflow.preflight.waivers,
            }),
            seeds: vec![7],
            device: self.settings.compute_device.engine_value().into(),
            runtime_ms: 0,
            stop_reason: "running".into(),
            warnings: workflow.preflight.warnings.clone(),
            waivers: workflow.preflight.waivers.clone(),
            missing_dependencies: Vec::new(),
            output_sha256: Vec::new(),
            scalar_outputs: Vec::new(),
            determinism: None,
        }
    }

    fn run_external_flow(&mut self) {
        if self.cad.as_ref().is_some_and(|case| case.pending) {
            if self.run_queue.waiting.len() < 8 {
                let case_id = self
                    .cad
                    .as_ref()
                    .map(|case| case.workflow.case_id.clone())
                    .unwrap_or_default();
                let active = self
                    .cad
                    .as_ref()
                    .and_then(|case| case.pending_run.as_ref())
                    .map(|pending| short_id(&pending.run_id))
                    .unwrap_or_else(|| "in-flight".into());
                self.run_queue.waiting.push_back(QueuedRunRequest {
                    case_id,
                    queued_utc_unix: now_utc_unix(),
                    note: format!("follow-on after {active}"),
                });
                self.project_notice = Some((
                    format!(
                        "Queued follow-on run · {} waiting behind the in-flight attempt.",
                        self.run_queue.waiting.len()
                    ),
                    false,
                ));
                return;
            }
            self.project_notice = Some((
                "Run queue is full (8). Wait for the in-flight attempt to finish.".into(),
                true,
            ));
            return;
        }
        if let Some(reason) = self.run_gate_reason() {
            self.project_notice = Some((reason, true));
            return;
        }
        let issues = match self.cad.as_ref() {
            Some(case) => case.workflow.readiness_issues(),
            None => {
                self.import_cad();
                return;
            }
        };
        if !issues.is_empty() {
            self.project_notice = Some((format!("Run blocked: {}", issues.join(" ")), true));
            return;
        }
        if let Err(error) = self.commit_active_case_revision() {
            self.project_notice = Some((format!("Run revision was not persisted: {error}"), true));
            return;
        }
        // Once execution starts, the inputs are a persisted revision rather
        // than an editable history branch. A later edit starts a fresh stack.
        self.rebase_case_draft_history();
        let Some(case) = self.cad.as_ref() else {
            return;
        };
        let workflow = case.workflow.clone();
        let mask = case.mask.clone();
        let model = case.model.clone();
        let case_name = case.name.clone();
        let steps = workflow.operating.horizon_steps;
        let parent_run_id = workflow
            .parent_run_id
            .clone()
            .or_else(|| case.active_run_id.clone());
        let case_revision_id = match workflow.case_revision_id.clone() {
            Some(revision) => revision,
            None => {
                self.project_notice = Some((
                    "Run blocked: the persisted case revision is missing.".into(),
                    true,
                ));
                return;
            }
        };
        let scale = case
            .workflow
            .operating
            .length_unit
            .meters_per_unit()
            .unwrap_or(1.0);
        let reference_length_m = (workflow.operating.reference_length * scale) as f32;
        let reynolds = workflow.operating.reynolds().unwrap_or_default() as f32;
        let request_id = format!("cad-request-{}", uuid::Uuid::new_v4());
        let run_id = format!("run-{}", uuid::Uuid::new_v4());
        let started_utc_unix = now_utc_unix();
        let manifest = self.external_flow_attempt_manifest(&workflow, &request_id);
        let running_attempt = project::RunRecord::new(
            run_id.clone(),
            parent_run_id.clone(),
            case_revision_id,
            started_utc_unix,
            0,
            project::LifecycleState::Running,
            manifest.clone(),
            Vec::new(),
        );
        let case_id = workflow.case_id.clone();
        if let Err(error) = self
            .project_write_access("Persisting the immutable run attempt")
            .and_then(|()| {
                self.project
                    .start_run_attempt_checkpointed(&case_id, running_attempt, started_utc_unix)
                    .map_err(|error| error.to_string())
            })
        {
            self.project_notice = Some((
                format!(
                    "Run did not start because its immutable attempt could not be safely checkpointed: {error}"
                ),
                true,
            ));
            return;
        }

        let (moment_origin_mode, moment_origin_solver, reference_area_m2) =
            match cad_moment_engine_args(&workflow.operating, workflow.preflight.transform_4x4) {
                Ok(args) => args,
                Err(error) => {
                    self.project_notice = Some((error, true));
                    return;
                }
            };
        let request = engine::Cmd::CadPredict {
            request_id: request_id.clone(),
            model,
            steps,
            mask,
            reynolds,
            characteristic_length_solver: workflow.preflight.solver_characteristic_length as f32,
            reference_length_m,
            velocity_mps: workflow.operating.velocity as f32,
            density_kg_m3: workflow.operating.density as f32,
            reference_pressure_pa: workflow.operating.reference_pressure as f32,
            moment_origin_mode,
            moment_origin_solver,
            reference_area_m2,
        };
        let Some(case) = self.cad.as_mut() else {
            return;
        };
        case.workflow.stage = engineering::CaseStage::Running;
        case.pending = true;
        case.steps = steps;
        case.pending_request_id = Some(request_id.clone());
        case.pending_run = Some(PendingCadRun {
            request_id: request_id.clone(),
            run_id: run_id.clone(),
            workflow,
            parent_run_id,
            started_utc_unix,
            started_at: std::time::Instant::now(),
            manifest,
            progress: None,
        });
        if self.engine.send(request).is_err() {
            let error = "engine request channel unavailable";
            let _ =
                self.finish_pending_external_flow_attempt(project::LifecycleState::Failed, error);
            self.clear_pending_external_flow();
            self.interrupt_and_restart_engine();
            self.project_notice = Some((
                format!(
                    "Run {} failed before dispatch and was persisted; the engine is restarting.",
                    short_id(&run_id)
                ),
                true,
            ));
            return;
        }
        self.nav = Nav::Case;
        self.engine_status = format!("● Running {} · Re {:.0} · H{}", case_name, reynolds, steps);
        self.project_notice = Some((
            "Immutable run started from the approved source, transform, operating point, and model revision."
                .into(),
            false,
        ));
    }

    fn enqueue_cad_field_ready(&mut self, field: engine::CadField) {
        let Some(case) = self.cad.as_ref() else {
            self.project_notice = Some((
                "CAD result arrived without an active case and was discarded.".into(),
                true,
            ));
            return;
        };
        let Some(pending) = case
            .pending_run
            .as_ref()
            .filter(|pending| pending.request_id == field.request_id)
        else {
            self.project_notice = Some((
                format!(
                    "CAD result request {} does not match the submitted run contract and was discarded.",
                    short_id(&field.request_id)
                ),
                true,
            ));
            return;
        };
        let case_revision_id = match pending.workflow.case_revision_id.clone() {
            Some(revision) => revision,
            None => {
                self.fail_cad_field_ready_queue(
                    "active case revision missing".into(),
                );
                return;
            }
        };
        if self.cad_field_ready_worker.is_none() {
            self.cad_field_ready_worker =
                match CadFieldReadyWorker::spawn(self.repaint_context.clone()) {
                    Ok(worker) => Some(worker),
                    Err(error) => {
                        self.fail_cad_field_ready_queue(error);
                        return;
                    }
                };
        }
        let request_id = field.request_id.clone();
        let request = CadFieldReadyWorkRequest {
            field,
            identity: cad_field_ready::CadFieldReadyIdentity {
                run_id: pending.run_id.clone(),
                case_revision_id,
            },
        };
        let Some(worker) = self.cad_field_ready_worker.as_ref() else {
            self.fail_cad_field_ready_queue(
                "CAD field preparation worker is unavailable.".into(),
            );
            return;
        };
        if worker.request_tx.send(request).is_err() {
            self.fail_cad_field_ready_queue(
                "CAD field preparation worker stopped.".into(),
            );
            return;
        }
        self.cad_field_ready_pending = Some(PendingCadFieldReady {
            request_id: request_id.clone(),
            started_at: std::time::Instant::now(),
        });
        self.engine_status = format!(
            "◐ Preparing immutable field package · {}",
            short_id(&request_id)
        );
        self.project_notice = Some((
            "Prediction completed. Encoding the immutable field package off the UI thread before recording the run."
                .into(),
            false,
        ));
    }

    fn fail_cad_field_ready_queue(&mut self, error: String) {
        self.cad_field_ready_pending = None;
        if let Some(case) = self.cad.as_mut() {
            case.pending = false;
            case.pending_request_id = None;
            case.pending_run = None;
            case.workflow.stage = if case.workflow.ready() {
                engineering::CaseStage::Ready
            } else {
                engineering::CaseStage::Setup
            };
        }
        self.project_notice = Some((
            format!(
                "Prediction completed, but immutable run persistence failed and the transient result was discarded: {error}"
            ),
            true,
        ));
    }

    fn handle_cad_field_ready_results(&mut self) {
        let completed: Vec<CadFieldReadyWorkResult> = self
            .cad_field_ready_worker
            .as_ref()
            .map(|worker| worker.result_rx.try_iter().collect())
            .unwrap_or_default();
        for completed in completed {
            let is_current = self
                .cad_field_ready_pending
                .as_ref()
                .is_some_and(|pending| pending.request_id == completed.request_id)
                && self.cad.as_ref().is_some_and(|case| {
                    case.pending_run
                        .as_ref()
                        .is_some_and(|pending| pending.request_id == completed.request_id)
                });
            if !is_current {
                continue;
            }
            self.cad_field_ready_pending = None;
            let ready = match completed.outcome {
                Ok(ready) => ready,
                Err(error) => {
                    self.fail_cad_field_ready_queue(error);
                    continue;
                }
            };
            match retain_after_persistence(ready, |package| {
                self.persist_prepared_external_flow_run(package)
            }) {
                Ok((persisted_run_id, package)) => {
                    self.install_prepared_cad_field(persisted_run_id, package);
                }
                Err(error) => {
                    self.fail_cad_field_ready_queue(error);
                }
            }
        }
    }

    fn install_prepared_cad_field(
        &mut self,
        persisted_run_id: String,
        ready: cad_field_ready::CadFieldReady,
    ) {
        self.invalidate_cad_section();
        if !ready.particles.is_empty() {
            self.particles = ready.particles;
        }
        if let Some((vol, dims)) = ready.volume_data {
            let (lo, hi) = calibrate_cad_volume_density(&vol);
            // Keep wake filaments subordinate to the Cp shell.
            self.density_lo = lo.clamp(0.28, 0.55);
            self.density_hi = hi.clamp(self.density_lo + 0.15, 0.92);
            self.volume_data = std::sync::Arc::new(vol);
            self.volume_dims = dims;
            self.volume_version = self.volume_version.wrapping_add(1);
        }
        let shape = [3usize, ready.n, ready.n, ready.n];
        if let Some((q_vol, q_dims)) = flow::q_criterion_volume(&shape, &ready.velocity) {
            self.q_volume_data = std::sync::Arc::new(q_vol);
            self.q_volume_dims = q_dims;
            self.volume_version = self.volume_version.wrapping_add(1);
        }
        self.insights3d = ready.insights;
        self.cad_version = self.cad_version.wrapping_add(1);
        let surf = gpu::SurfaceData {
            mask: std::sync::Arc::new(ready.surface_mask_u8),
            pressure: std::sync::Arc::new(ready.surface_cp_u8),
            dims: [ready.n as u32; 3],
            mask_version: self.cad_version,
            pressure_version: self.cad_version,
        };
        let n = ready.n;
        let reynolds = ready.reynolds;
        let horizon = ready.horizon;
        if let Some(c) = &mut self.cad {
            c.mask = std::sync::Arc::new(ready.mask);
            c.mask_bounds = ready.mask_bounds;
            c.surf = Some(surf);
            c.surf_mask_source = Some(c.mask.clone());
            c.steps = ready.horizon;
            c.velocity = ready.velocity;
            c.pressure = ready.pressure;
            c.cp = ready.cp;
            c.traction = ready.traction;
            c.result_grid = ready.n;
            c.dt_frame = ready.dt_frame;
            c.pending = false;
            c.pending_request_id = None;
            c.pending_run = None;
            c.playback.reset();
            c.active_run_id = Some(persisted_run_id);
            c.workflow.stage = engineering::CaseStage::Results;
            c.workflow.parent_run_id = c.active_run_id.clone();
            c.workflow.result = Some(ready.engineering_result);
        }
        self.probe3d = None;
        // Fluent-style default stack: opaque Cp + streamlines. Q iso is optional
        // because it otherwise fogs the body on a 64³ Brinkman field.
        self.surface_on = true;
        self.streamlines = true;
        self.q_iso_on = false;
        self.volumetric = true;
        self.render_volume = false;
        // Default Case/sandbox state enables an X mid-plane clip. A fitted body
        // often sits entirely upstream of x=0, so that clip erases the solid and
        // the wake and leaves a black Results canvas.
        self.slice = [false, false, false];
        self.shadows = false;
        // Three-quarter station + fit so a flat 64³ brick is not an edge-on speck.
        self.view_snap = Some(viewport::StandardView::Iso);
        self.view_fit = true;
        self.nav = Nav::Results;
        let horizon_span = self
            .cad
            .as_ref()
            .map(|case| case.workflow.model_max_steps.max(case.steps).max(1))
            .unwrap_or(horizon.max(1));
        self.engine_status = format!(
            "● Engineering result {}³ · Re {:.0} · horizon step {} of {}",
            n, reynolds, horizon, horizon_span
        );
        self.engine_ok = true;
        let notice = if self.q_volume_data.is_empty() {
            "Immutable run recorded. Body surface and model streamlines are ready for review."
        } else {
            "Immutable run recorded. Body surface and model streamlines are on; Q iso is available under Layers."
        };
        self.project_notice = Some((notice.into(), false));
    }

    fn persist_prepared_external_flow_run(
        &mut self,
        ready: &cad_field_ready::CadFieldReady,
    ) -> Result<String, String> {
        self.project_write_access("Recording the completed immutable run")?;
        let case = self
            .cad
            .as_ref()
            .ok_or_else(|| "CAD result arrived without an active case".to_string())?;
        let pending = case
            .pending_run
            .clone()
            .filter(|pending| pending.request_id == ready.request_id)
            .ok_or_else(|| {
                format!(
                    "CAD result request {} does not match the submitted run contract",
                    ready.request_id
                )
            })?;
        let workflow = pending.workflow.clone();
        let active_run_id = case.active_run_id.clone();
        let runtime_ms = pending.started_at.elapsed().as_millis() as u64;
        let case_revision_id = workflow
            .case_revision_id
            .clone()
            .ok_or_else(|| "active case revision missing".to_string())?;
        if case_revision_id != ready.case_revision_id || pending.run_id != ready.run_id {
            return Err(
                "Prepared CAD field package identity does not match the pending run.".into(),
            );
        }
        let run_id = pending.run_id.clone();
        let field_sha256 = ready.field.digest().to_owned();
        let result_sha256 = ready.result.digest().to_owned();
        self.project_write_access("Recording the completed immutable run")?;
        self.project.add_digested_content(
            ready.field.clone(),
            "application/vnd.reyn.engineering-field.f32le",
        );
        self.project.add_digested_content(
            ready.result.clone(),
            "application/vnd.reyn.engineering-result+json",
        );
        let scalar = |key: &str, value: f64, units: &str| project::ScalarOutput {
            key: key.into(),
            value,
            units: units.into(),
            abs_tolerance: 1e-6,
        };
        let result = &ready.engineering_result;
        let scalar_outputs = vec![
            scalar("force_coefficient_x", result.force_coefficients[0], "1"),
            scalar("force_coefficient_y", result.force_coefficients[1], "1"),
            scalar("force_coefficient_z", result.force_coefficients[2], "1"),
            scalar("moment_coefficient_x", result.moment_coefficients[0], "1"),
            scalar("moment_coefficient_y", result.moment_coefficients[1], "1"),
            scalar("moment_coefficient_z", result.moment_coefficients[2], "1"),
            scalar("cp_min", result.cp_min, "1"),
            scalar("cp_max", result.cp_max, "1"),
            scalar("wake_deficit_peak", result.wake_deficit_peak, "1"),
            scalar("wake_deficit_mean", result.wake_deficit_mean, "1"),
        ];
        let parent_run_id = pending.parent_run_id.clone().or(active_run_id);
        let parent_run = parent_run_id.as_deref().and_then(|parent_id| {
            self.project
                .manifest()
                .cases()
                .iter()
                .find(|record| record.case_id() == workflow.case_id)
                .and_then(|record| {
                    record
                        .runs()
                        .iter()
                        .find(|run| run.run_id() == parent_id)
                        .cloned()
                })
        });
        let mut manifest = pending.manifest.clone();
        manifest.runtime_ms = runtime_ms;
        manifest.stop_reason = "succeeded".into();
        manifest.warnings.extend(result.warnings.clone());
        manifest.output_sha256 = vec![result_sha256.clone(), field_sha256];
        manifest.scalar_outputs = scalar_outputs;
        if let Some(parent) = &parent_run {
            manifest.compare_scalars_against(parent);
        }
        let calibrated_views = vec![
            project::CalibratedView {
                view_id: format!("view-{run_id}-cp"),
                quantity: "pressure coefficient Cp".into(),
                units: "1".into(),
                scale_min: result.cp_min.min(result.cp_max - 1e-6),
                scale_max: result.cp_max.max(result.cp_min + 1e-6),
                source_class: project::EvidenceSourceClass::Derived,
                method: "Cp=(p_recovered-p_inf)/q_inf".into(),
            },
            project::CalibratedView {
                view_id: format!("view-{run_id}-velocity"),
                quantity: "velocity".into(),
                units: "m/s".into(),
                scale_min: 0.0,
                scale_max: workflow.operating.velocity.max(1e-6),
                source_class: project::EvidenceSourceClass::ModelPrediction,
                method: "DirectFlowMap fixed-body prediction".into(),
            },
        ];
        let run = project::RunRecord::new(
            run_id.clone(),
            parent_run.as_ref().map(|parent| parent.run_id().to_owned()),
            case_revision_id,
            pending.started_utc_unix,
            now_utc_unix(),
            project::LifecycleState::Succeeded,
            manifest,
            calibrated_views.clone(),
        );
        let evidence = project::EvidenceArtifact {
            evidence_id: format!("evidence-{}", uuid::Uuid::new_v4()),
            run_ids: vec![run_id.clone()],
            created_utc_unix: now_utc_unix(),
            source_class: project::EvidenceSourceClass::Derived,
            media_type: "application/vnd.reyn.engineering-result+json".into(),
            byte_size: ready.result.len() as u64,
            content_sha256: result_sha256,
            derivation_method: Some(engineering::SURFACE_LOAD_METHOD.into()),
            derivation_version: Some("1".into()),
            warnings: result.warnings.clone(),
            metadata: ready.result_json.clone(),
            calibrated_views,
        };
        let case_id = workflow.case_id;
        self.transact_project(
            "Recording the completed immutable run",
            now_utc_unix(),
            |project_manifest| {
                project_manifest.finish_run_attempt(&case_id, run, now_utc_unix())?;
                project_manifest.append_evidence(evidence, now_utc_unix())?;
                project_manifest.set_selection(
                    project::ProjectSelection {
                        active_case_id: Some(case_id),
                        selected_run_id: Some(run_id.clone()),
                        selected_evidence_id: None,
                        selected_view_id: None,
                    },
                    now_utc_unix(),
                )?;
                Ok(())
            },
        )?;
        self.project
            .checkpoint_recovery(now_utc_unix())
            .map_err(|error| error.to_string())?;
        self.dependencies_dirty = true;
        Ok(run_id)
    }

    fn engineering_notice(&self, ui: &mut egui::Ui) {
        let Some((message, is_error)) = &self.project_notice else {
            return;
        };
        let color = if *is_error { DANGER } else { OK };
        Frame::NONE
            .fill(if *is_error {
                tint_fill(DANGER)
            } else {
                SURFACE_LOW
            })
            .stroke(Stroke::new(
                1.0,
                if *is_error {
                    tint_hairline(DANGER)
                } else {
                    HAIRLINE
                },
            ))
            .corner_radius(CornerRadius::same(R1))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.label(
                        RichText::new(if *is_error { "!" } else { "✓" })
                            .text_style(mono_s())
                            .color(color),
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(message).text_style(caption()).color(TEXT_DIM),
                        )
                        .wrap(),
                    );
                });
            });
        ui.add_space(10.0);
    }

    fn controls_engineering_case(&mut self, ui: &mut egui::Ui) {
        let model_inventory = self.models.clone();
        let model_inventory_state = self.model_inventory_state.clone();
        let mut waiver_draft = std::mem::take(&mut self.waiver_draft);
        let mut waiver_code = self.waiver_code.take();
        let read_only = self.project.availability().is_read_only_evidence();
        ui.label(title_text("External-Flow Case"));
        ui.label(
            RichText::new("source → preflight → contract → run")
                .size(11.0)
                .color(TEXT_MUTE),
        );
        ui.add_space(12.0);
        if read_only {
            ui.label(
                RichText::new(
                    "READ-ONLY EVIDENCE · Case Setup and run controls are locked. Stored runs, fields, and evidence remain inspectable.",
                )
                .text_style(caption())
                .color(WARN),
            );
            ui.add_space(8.0);
        }
        if self.cad.is_none() {
            card(ui, |ui| {
                ui.label(RichText::new("No geometry imported").color(TEXT));
                ui.label(
                    RichText::new(
                        "Start with an STL, single-part STEP, or 3MF file. Reyn preserves the source bytes, hash, transform, and every case revision.",
                    )
                    .text_style(caption())
                    .color(TEXT_MUTE),
                );
            });
            ui.add_space(10.0);
            let import = ui
                .add_enabled(!read_only, egui::Button::new("Import Geometry…"))
                .on_disabled_hover_text(
                    "Geometry import is blocked while the project is in read-only evidence mode.",
                );
            import.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    import.enabled(),
                    "Import geometry for external-flow case",
                )
            });
            if import.clicked()
            {
                self.import_cad();
            }
            return;
        }
        let mut open_model_library = false;
        let mut retry_model_inventory = false;
        let undo_reason = self.case_history_gate_reason(CaseHistoryAction::Undo);
        let redo_reason = self.case_history_gate_reason(CaseHistoryAction::Redo);
        let mut history_action = None;
        Frame::NONE
            .fill(SURFACE_LOW)
            .corner_radius(CornerRadius::same(R1))
            .inner_margin(Margin::symmetric(10, 7))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(overline_text("Draft edits"));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let redo =
                            ui.add_enabled(redo_reason.is_none(), egui::Button::new("Redo  ⇧⌘Z"));
                        if redo.clicked() {
                            history_action = Some(CaseHistoryAction::Redo);
                        }
                        if let Some(reason) = &redo_reason {
                            redo.on_disabled_hover_text(reason);
                        }
                        let undo =
                            ui.add_enabled(undo_reason.is_none(), egui::Button::new("Undo  ⌘Z"));
                        if undo.clicked() {
                            history_action = Some(CaseHistoryAction::Undo);
                        }
                        if let Some(reason) = &undo_reason {
                            undo.on_disabled_hover_text(reason);
                        }
                    });
                });
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "Setup inputs only · source, run, and evidence identity never rewind.",
                        )
                        .text_style(caption())
                        .color(TEXT_MUTE),
                    )
                    .wrap(),
                );
            });
        ui.add_space(10.0);
        if let Some(action) = history_action {
            self.apply_case_history_action(action);
        }
        self.engineering_notice(ui);
        let had_unsaved_work = self.has_unsaved_project_work();
        let mut schedule_orientation_autosave = false;
        let focus_stage = self.study_focus_stage.take();
        let mut requested_study_stage = None;
        let orientation_pending = self
            .orientation_pending
            .as_ref()
            .map(PendingOrientation::view);
        let case = self.cad.as_mut().expect("checked above");
        let draft_before = engineering::CaseDraftSnapshot::capture(&case.workflow);
        let mut changed_transaction = None;
        let mut active_transaction = None;
        let mut identity_changed = false;
        // Honest in-flight state: stage frames from the sidecar give a
        // stage-based fraction (not wall-clock %). Show stage + elapsed + Cancel.
        // Rendered before `ui.disable()` so Cancel stays live.
        let mut cancel_run = false;
        let outline_blockers = case.workflow.readiness_blockers();
        card(ui, |ui| {
            ui.label(caps("Study outline"));
            for stage in [
                engineering::ReadinessStage::Geometry,
                engineering::ReadinessStage::QualifiedModel,
                engineering::ReadinessStage::FlowConditions,
                engineering::ReadinessStage::ReviewFocus,
                engineering::ReadinessStage::RunReadiness,
            ] {
                let count = outline_blockers
                    .iter()
                    .filter(|blocker| blocker.stage == stage)
                    .count();
                let a11y_name = if count == 0 {
                    format!("Study stage {} · ready", stage.label())
                } else {
                    format!(
                        "Study stage {} · {} blocker{}",
                        stage.label(),
                        count,
                        if count == 1 { "" } else { "s" }
                    )
                };
                let label = if count == 0 {
                    format!("✓  {}", stage.label())
                } else {
                    format!("!  {} · {count}", stage.label())
                };
                let response = ui.add_sized(
                    [ui.available_width(), 28.0],
                    egui::Button::new(label).selected(focus_stage == Some(stage)),
                );
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, response.enabled(), &a11y_name)
                });
                if response.clicked() {
                    requested_study_stage = Some(stage);
                }
            }
        });
        ui.add_space(10.0);
        if case.pending {
            let elapsed = case
                .pending_run
                .as_ref()
                .map(|pending| pending.started_at.elapsed().as_secs_f64())
                .unwrap_or(0.0);
            let horizon = case
                .pending_run
                .as_ref()
                .map(|pending| pending.workflow.operating.horizon_steps)
                .unwrap_or(case.steps);
            let request_id = case.pending_request_id.clone();
            let progress = case
                .pending_run
                .as_ref()
                .and_then(|pending| pending.progress.clone());
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(13.0));
                    ui.label(RichText::new("RUN IN FLIGHT").strong().color(BRAND));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("{elapsed:.0} s elapsed"))
                                .text_style(mono_s())
                                .color(TEXT_DIM),
                        );
                    });
                });
                ui.add_space(4.0);
                if let Some(progress) = &progress {
                    ui.label(
                        RichText::new(format!(
                            "Stage {} of {} · {}",
                            progress.stage_index,
                            progress.stage_count,
                            cad_progress_stage_label(&progress.stage)
                        ))
                        .text_style(caption())
                        .color(GOLD),
                    );
                    if !progress.detail.is_empty() {
                        ui.label(
                            RichText::new(&progress.detail)
                                .text_style(caption())
                                .color(TEXT_MUTE),
                        );
                    }
                    ui.add_space(4.0);
                    let mut bar = egui::ProgressBar::new(progress.fraction.clamp(0.0, 1.0))
                        .desired_width(ui.available_width())
                        .animate(true);
                    bar = bar.text(format!("{:.0}% stage progress", progress.fraction * 100.0));
                    ui.add(bar);
                    ui.label(
                        RichText::new(
                            "Bar tracks engine stages (prepare → develop → predict → recover), not wall-clock time.",
                        )
                        .text_style(caption())
                        .color(TEXT_MUTE),
                    );
                } else {
                    ui.label(
                        RichText::new(format!(
                            "Dispatching horizon step {horizon}. Waiting for the first engine stage frame…"
                        ))
                        .text_style(caption())
                        .color(TEXT_MUTE),
                    );
                }
                if let Some(request_id) = &request_id {
                    diag(ui, "Request", &short_hash(request_id), TEXT_MUTE);
                }
                ui.add_space(6.0);
                let cancel = ui.button("Cancel run").on_hover_text(
                    "Cancel this run, persist the attempt as cancelled, terminate the blocking sidecar, and start a fresh engine for retry. No result evidence is created.",
                );
                cancel.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        cancel.enabled(),
                        "Cancel in-flight external-flow run",
                    )
                });
                cancel_run = cancel.clicked();
            });
            ui.add_space(10.0);
        }
        if let Some(pending) = &orientation_pending {
            let elapsed = pending.started_at.elapsed().as_secs_f64();
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(13.0));
                    ui.label(RichText::new("RE-VOXELIZING").strong().color(BRAND));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("{elapsed:.0} s elapsed"))
                                .text_style(mono_s())
                                .color(TEXT_DIM),
                        );
                    });
                });
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Rotating the source geometry and rebuilding its three-axis occupancy mask off the UI thread. The classifier reports no intermediate progress, so this state is indeterminate.",
                    )
                    .text_style(caption())
                    .color(TEXT_MUTE),
                );
                diag(ui, "Request", &short_id(&pending.request_id), TEXT_MUTE);
                diag(
                    ui,
                    "Attitude",
                    &format!(
                        "α {:+.2}° · β {:+.2}° · φ {:+.2}°",
                        pending.angles[0], pending.angles[1], pending.angles[2]
                    ),
                    TEXT_DIM,
                );
                ui.label(
                    RichText::new(
                        "Runs, saves, and recovery snapshots are gated. Applying newer angles supersedes this generation; a stale completion cannot mutate the case.",
                    )
                    .text_style(caption())
                    .color(TEXT_MUTE),
                );
            });
            ui.add_space(10.0);
        }
        if case.pending || read_only {
            ui.disable();
        }
        let mut changed = false;
        let mut template_view_changed = false;
        card(ui, |ui| {
            if focus_stage == Some(engineering::ReadinessStage::Geometry) {
                let anchor = ui.label("");
                anchor.request_focus();
                ui.scroll_to_rect(anchor.rect, Some(egui::Align::Center));
            }
            ui.label(caps("Setup gate"));
            let units_known =
                case.workflow.operating.length_unit != engineering::LengthUnit::Unknown;
            let approved = case.workflow.preflight.transform_approved;
            let (color, glyph, message) = if !units_known {
                (
                    WARN,
                    "!",
                    "Declare the geometry units before this case can run.",
                )
            } else if !approved {
                (
                    WARN,
                    "!",
                    "Review and approve units, orientation, scale, and solver placement.",
                )
            } else {
                (
                    OK,
                    "✓",
                    "Source units and preprocessing transform are approved.",
                )
            };
            alert_line(ui, color, glyph, message);
            let length_unit_label = a11y_caption(ui, "Geometry length unit");
            let previous_unit = case.workflow.operating.length_unit;
            let length_unit_combo = egui::ComboBox::from_id_salt("engineering.length-unit")
                .selected_text(case.workflow.operating.length_unit.label())
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for unit in engineering::LengthUnit::ALL {
                        changed |= ui
                            .selectable_value(
                                &mut case.workflow.operating.length_unit,
                                unit,
                                unit.label(),
                            )
                            .changed();
                    }
                });
            length_unit_combo
                .response
                .labelled_by(length_unit_label.id);
            if case.workflow.operating.length_unit != previous_unit
                && case
                    .workflow
                    .operating
                    .retarget_velocity_to_qualified_reynolds()
            {
                changed = true;
                self.project_notice = Some((
                    format!(
                        "Free-stream set for Re ≈ {:.0} inside the qualified envelope.",
                        engineering::QUALIFIED_REYNOLDS_TARGET
                    ),
                    false,
                ));
            }
            changed |= ui
                .checkbox(
                    &mut case.workflow.preflight.transform_approved,
                    "Approve units, orientation, scale, and solver placement",
                )
                .changed();
            let re_out_of_band = case.workflow.operating.length_unit
                != engineering::LengthUnit::Unknown
                && !case.workflow.operating.reynolds_in_qualified_envelope();
            if re_out_of_band
                && ui
                    .add_sized(
                        [ui.available_width(), 28.0],
                        egui::Button::new(
                            RichText::new(format!(
                                "Set free-stream for Re ≈ {:.0}",
                                engineering::QUALIFIED_REYNOLDS_TARGET
                            ))
                            .text_style(caption()),
                        ),
                    )
                    .on_hover_text(
                        "Keeps density and viscosity; only adjusts free-stream so Reynolds lands mid-envelope.",
                    )
                    .clicked()
            {
                if case
                    .workflow
                    .operating
                    .retarget_velocity_to_qualified_reynolds()
                {
                    changed = true;
                }
            }
        });
        ui.add_space(8.0);
        inspector_group(
            ui,
            "case-source-preflight",
            "Source & preflight details",
            false,
            |ui| {
                diag(ui, "File", &case.workflow.source_name, TEXT);
                diag(
                    ui,
                    "Source SHA-256",
                    &short_hash(&case.workflow.preflight.source_sha256),
                    TEXT_MUTE,
                );
                if !case.workflow.preflight.analyzed_mesh_sha256.is_empty() {
                    diag(
                        ui,
                        "Analyzed mesh SHA-256",
                        &short_hash(&case.workflow.preflight.analyzed_mesh_sha256),
                        TEXT_MUTE,
                    );
                }
                let source_format = if case.workflow.preflight.source_format.is_empty() {
                    "STL · legacy record".into()
                } else {
                    case.workflow.preflight.source_format.to_ascii_uppercase()
                };
                diag(ui, "Source format", &source_format, TEXT_DIM);
                if case.workflow.preflight.source_format == "step"
                    || case.workflow.preflight.source_format == "3mf"
                {
                    diag(
                        ui,
                        "Declared units",
                        case.workflow
                            .preflight
                            .source_declared_units
                            .as_deref()
                            .unwrap_or("missing"),
                        if case.workflow.preflight.source_declared_units.is_some() {
                            TEXT_DIM
                        } else {
                            WARN
                        },
                    );
                    diag(
                        ui,
                        if case.workflow.preflight.source_format == "step" {
                            "STEP translator"
                        } else {
                            "3MF translator"
                        },
                        &format!(
                            "{} {}",
                            case.workflow.preflight.geometry_translator,
                            case.workflow.preflight.geometry_translator_version
                        ),
                        TEXT_DIM,
                    );
                    if let Some(tolerance) =
                        case.workflow.preflight.tessellation_tolerance_source_units
                    {
                        diag(
                            ui,
                            "Tessellation tolerance",
                            &format!("{tolerance:.6} source units"),
                            TEXT_DIM,
                        );
                    }
                    if let Some(tolerance) = case.workflow.preflight.vertex_weld_relative_tolerance
                    {
                        diag(
                            ui,
                            "Face-boundary weld",
                            &format!("{:.4}% of source diagonal", tolerance * 100.0),
                            TEXT_DIM,
                        );
                    }
                }
                diag(
                    ui,
                    "Triangles",
                    &case.workflow.preflight.triangles.to_string(),
                    TEXT_DIM,
                );
                // Bounding box in the declared units (SI echo when not meters);
                // before units are declared, it is honestly labeled "source units".
                let extents = case.workflow.preflight.source_extents;
                let unit = case.workflow.operating.length_unit;
                let extents_text = match unit.meters_per_unit() {
                    Some(scale) => {
                        let si = if (scale - 1.0).abs() > 1e-12 {
                            format!(
                                "  ({:.4} × {:.4} × {:.4} m)",
                                extents[0] * scale,
                                extents[1] * scale,
                                extents[2] * scale
                            )
                        } else {
                            String::new()
                        };
                        format!(
                            "{:.4} × {:.4} × {:.4} {}{si}",
                            extents[0],
                            extents[1],
                            extents[2],
                            unit.symbol()
                        )
                    }
                    None => format!(
                        "{:.4} × {:.4} × {:.4} source units — declare units below",
                        extents[0], extents[1], extents[2]
                    ),
                };
                diag(
                    ui,
                    "Bounding box",
                    &extents_text,
                    if unit.meters_per_unit().is_some() {
                        TEXT_DIM
                    } else {
                        WARN
                    },
                );
                diag(
                    ui,
                    "Surface components",
                    &case.workflow.preflight.components.to_string(),
                    TEXT_DIM,
                );
                // Watertightness verdict in words (never color-only).
                let watertight = case.workflow.preflight.boundary_edges == 0
                    && case.workflow.preflight.non_manifold_edges == 0;
                diag(
                    ui,
                    "Watertight",
                    if watertight {
                        "yes · closed manifold surface"
                    } else {
                        "no · open or non-manifold edges"
                    },
                    if watertight { SUCCESS } else { WARN },
                );
                diag(
                    ui,
                    "Defects",
                    &format!(
                        "{} degenerate · {} open · {} non-manifold · {} winding · {} intersections",
                        case.workflow.preflight.degenerate_triangles,
                        case.workflow.preflight.boundary_edges,
                        case.workflow.preflight.non_manifold_edges,
                        case.workflow.preflight.inconsistent_winding_edges,
                        case.workflow.preflight.self_intersection_pairs
                    ),
                    if case.workflow.preflight.degenerate_triangles == 0
                        && case.workflow.preflight.inconsistent_winding_edges == 0
                        && case.workflow.preflight.self_intersection_pairs == 0
                        && watertight
                    {
                        SUCCESS
                    } else {
                        WARN
                    },
                );
                diag(
                    ui,
                    "Source winding",
                    &format!(
                        "signed volume {:+.6e} source³ · {}",
                        case.workflow.preflight.source_signed_volume,
                        if case.workflow.preflight.source_signed_volume < 0.0 {
                            "inward source orientation; mask-gradient loads unaffected"
                        } else {
                            "outward source orientation"
                        }
                    ),
                    if case.workflow.preflight.inconsistent_winding_edges == 0 {
                        TEXT_DIM
                    } else {
                        DANGER
                    },
                );
                diag(
                    ui,
                    "Grid",
                    &format!("{}³", case.workflow.preflight.target_grid),
                    BRAND,
                );
                diag(
                    ui,
                    "Proposed scale",
                    &format!("{:.6}", case.workflow.preflight.proposed_scale),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Solver characteristic length",
                    &format!(
                        "{:.6} solver units",
                        case.workflow.preflight.solver_characteristic_length
                    ),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Voxel adequacy",
                    &format!(
                        "{} solid · {}-cell resolved core · {} cells clear",
                        case.workflow.preflight.solid_voxels,
                        case.workflow.preflight.minimum_cells_across,
                        case.workflow.preflight.boundary_clearance_cells
                    ),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Axis agreement",
                    &format!(
                        "{:.2}% disagree · odd rows X/Y/Z {} / {} / {} · classifier v{}",
                        case.workflow.preflight.voxel_axis_disagreement_fraction * 100.0,
                        case.workflow.preflight.voxel_odd_crossing_rows[0],
                        case.workflow.preflight.voxel_odd_crossing_rows[1],
                        case.workflow.preflight.voxel_odd_crossing_rows[2],
                        case.workflow.preflight.voxel_classification_version,
                    ),
                    if case.workflow.preflight.voxel_axis_disagreement_fraction
                        <= engineering::GeometryPreflight::MAX_AXIS_DISAGREEMENT_FRACTION
                    {
                        SUCCESS
                    } else {
                        DANGER
                    },
                );
                ui.collapsing("Preprocessing transform 4×4", |ui| {
                    for row in 0..4 {
                        ui.label(
                            RichText::new(format!(
                                "{:>10.5} {:>10.5} {:>10.5} {:>10.5}",
                                case.workflow.preflight.transform_4x4[row],
                                case.workflow.preflight.transform_4x4[4 + row],
                                case.workflow.preflight.transform_4x4[8 + row],
                                case.workflow.preflight.transform_4x4[12 + row],
                            ))
                            .text_style(mono_s())
                            .color(TEXT_MUTE),
                        );
                    }
                });
                ui.add_space(8.0);
                for warning in &case.workflow.preflight.warnings {
                    ui.label(
                        RichText::new(format!("NOTICE · {warning}"))
                            .text_style(caption())
                            .color(WARN),
                    );
                }
            },
        );
        ui.add_space(4.0);
        // Body attitude. The model's free stream is fixed on +X and cannot be
        // rotated, so attitude is applied to the geometry before voxelization.
        let applied_orientation = case.workflow.preflight.body_orientation_degrees();
        let mut draft = self.orientation_draft.unwrap_or(applied_orientation);
        let mut apply_orientation = false;
        let mut reset_orientation = false;
        let orientation_open =
            !case.workflow.preflight.body_is_aligned() || self.orientation_draft.is_some();
        inspector_group(
            ui,
            "case-body-orientation",
            "Body orientation",
            orientation_open,
            |ui| {
                ui.label(
                RichText::new(
                    "The free stream is fixed on +X, so attitude is applied by rotating the body before voxelization — exactly what is computed. Angles are recorded in the case revision and folded into the preprocessing transform.",
                )
                .text_style(caption())
                .color(TEXT_MUTE),
            );
                ui.add_space(8.0);
                let angle_row = |ui: &mut egui::Ui, label: &str, hint: &str, value: &mut f64| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(label).text_style(caption()).color(TEXT_MUTE))
                            .on_hover_text(hint);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add(
                                egui::DragValue::new(value)
                                    .speed(0.25)
                                    .range(-180.0..=180.0)
                                    .fixed_decimals(2)
                                    .suffix("°"),
                            )
                            .on_hover_text(hint);
                        });
                    });
                };
                angle_row(
                    ui,
                    "Angle of attack α",
                    "Nose-up pitch about +Y. Positive α lifts the nose into the +X stream.",
                    &mut draft[0],
                );
                angle_row(
                    ui,
                    "Yaw β",
                    "Sideslip about +Z. Positive β swings the nose toward +Y.",
                    &mut draft[1],
                );
                angle_row(
                    ui,
                    "Roll φ",
                    "Bank about the +X stream axis.",
                    &mut draft[2],
                );
                ui.add_space(4.0);
                diag(
                    ui,
                    "Applied",
                    &case.workflow.preflight.body_orientation_summary(),
                    if case.workflow.preflight.body_is_aligned() {
                        TEXT_DIM
                    } else {
                        BRAND
                    },
                );
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Coefficient frame").color(TEXT_DIM));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(mono("wind axes · fixed stream", TEXT_DIM)).truncate(),
                        )
                        .on_hover_text(engineering::COEFFICIENT_REFERENCE_FRAME);
                    });
                });
                // The auto-fit sizes the rotated silhouette into the training band,
                // so a pitched body lands at a different solver scale. Say so here
                // rather than letting the preflight number change silently.
                ui.label(
                RichText::new(
                    "Applying an attitude refits the rotated silhouette into the trained size band, so the proposed scale above changes with it.",
                )
                .text_style(caption())
                .color(TEXT_MUTE),
            );
                ui.add_space(7.0);
                let dirty = draft
                    .iter()
                    .zip(applied_orientation.iter())
                    .any(|(next, current)| (next - current).abs() > 1e-9);
                let request_matches_draft = orientation_pending
                    .as_ref()
                    .is_some_and(|pending| pending.angles == draft);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                let apply = ui.add_enabled(
                    dirty && !request_matches_draft,
                    egui::Button::new(if orientation_pending.is_some() && !request_matches_draft {
                        "Apply newer orientation"
                    } else {
                        "Apply orientation"
                    }),
                );
                apply_orientation = apply.clicked();
                if request_matches_draft {
                    apply.on_disabled_hover_text(
                        "These exact angles are already re-voxelizing. Completion will wake the UI.",
                    );
                } else if dirty {
                    apply.on_hover_text(
                        "Re-voxelize the body at this attitude. Re-opens transform approval and clears any result.",
                    );
                } else {
                    apply.on_hover_text("These angles are already applied to the voxel mask.");
                }
                if !case.workflow.preflight.body_is_aligned() || dirty {
                    reset_orientation = ui
                        .button("Level")
                        .on_hover_text("Return the body to its imported attitude (0°, 0°, 0°).")
                        .clicked();
                }
            });
                if dirty {
                    ui.label(
                        RichText::new(
                            if request_matches_draft {
                                "RE-VOXELIZING · these angles are not in the mask until this request completes."
                            } else if orientation_pending.is_some() {
                                "PENDING · these edited angles are not queued. Apply to supersede the in-flight generation."
                            } else {
                                "PENDING · these angles are not in the mask yet. Apply to re-voxelize."
                            },
                        )
                        .text_style(caption())
                        .color(WARN),
                    );
                }
            },
        );
        if reset_orientation {
            draft = [0.0; 3];
            apply_orientation = true;
        }
        let next_orientation_draft = (draft != applied_orientation).then_some(draft);
        if self.orientation_draft != next_orientation_draft {
            if self.orientation_draft.is_none() && next_orientation_draft.is_some() {
                schedule_orientation_autosave = !had_unsaved_work;
            }
            self.orientation_draft = next_orientation_draft;
        }
        ui.add_space(10.0);
        card(ui, |ui| {
            ui.label(caps("Named regions"));
            ui.label(
                RichText::new(
                    "Author labels on structural candidates for future boundary mapping. External-flow screening does not require them; they persist with the case.",
                )
                .text_style(caption())
                .color(TEXT_MUTE),
            );
            ui.add_space(6.0);
            let component_count = case.workflow.preflight.components.max(1);
            for index in 0..component_count {
                let candidate_id = format!("component-{index}");
                let existing = case
                    .workflow
                    .named_regions
                    .iter()
                    .find(|region| region.candidate_id == candidate_id)
                    .cloned()
                    .unwrap_or_else(|| engineering::NamedRegionAssignment {
                        name: String::new(),
                        candidate_id: candidate_id.clone(),
                        role: "unassigned".into(),
                        identity_kind: "heuristic_index".into(),
                        stable_face_id: candidate_id.clone(),
                    });
                let mut name = existing.name;
                let mut role = existing.role;
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&candidate_id)
                            .text_style(mono_s())
                            .color(TEXT_DIM),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut name)
                            .hint_text("Label")
                            .desired_width(120.0),
                    );
                    egui::ComboBox::from_id_salt(format!("region.role.{candidate_id}"))
                        .selected_text(&role)
                        .width(110.0)
                        .show_ui(ui, |ui| {
                            for option in ["unassigned", "wall", "inlet", "outlet", "symmetry"] {
                                ui.selectable_value(&mut role, option.to_owned(), option);
                            }
                        });
                });
                let Some(slot) = case
                    .workflow
                    .named_regions
                    .iter_mut()
                    .find(|region| region.candidate_id == candidate_id)
                else {
                    if !name.trim().is_empty() || role != "unassigned" {
                        case.workflow
                            .named_regions
                            .push(engineering::NamedRegionAssignment {
                                name,
                                stable_face_id: candidate_id.clone(),
                                identity_kind: "heuristic_index".into(),
                                candidate_id,
                                role,
                            });
                    }
                    continue;
                };
                if slot.name != name || slot.role != role {
                    slot.name = name;
                    slot.role = role;
                    self.case_draft_dirty = true;
                }
            }
            if ui.add(egui::Button::new("Clear region labels")).clicked() {
                case.workflow.named_regions.clear();
                self.case_draft_dirty = true;
            }
        });
        ui.add_space(10.0);
        card(ui, |ui| {
            if focus_stage == Some(engineering::ReadinessStage::QualifiedModel) {
                let anchor = ui.label("");
                anchor.request_focus();
                ui.scroll_to_rect(anchor.rect, Some(egui::Align::Center));
            }
            ui.label(caps("Operating point"));
            ui.label(
                RichText::new("Qualified model")
                    .text_style(caption())
                    .color(TEXT_MUTE),
            );
            let target_grid = case.workflow.preflight.target_grid;
            let qualified = model_inventory
                .iter()
                .filter(|model| engine::is_qualified_external_flow_model(model, target_grid))
                .collect::<Vec<_>>();
            match model_selector_mode(
                &model_inventory_state,
                &model_inventory,
                &case.workflow.model_id,
                target_grid,
            ) {
                ModelSelectorMode::Loading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            RichText::new("Checking installed models…")
                                .text_style(caption())
                                .color(TEXT_MUTE),
                        );
                    });
                }
                ModelSelectorMode::Failed(error) => {
                    ui.label(
                        RichText::new("Model inventory is unavailable")
                            .text_style(caption())
                            .color(WARN),
                    );
                    ui.label(RichText::new(error).text_style(caption()).color(TEXT_MUTE));
                    if ui.button("Retry model check").clicked() {
                        retry_model_inventory = true;
                    }
                }
                ModelSelectorMode::StoredModelMissing(model_id) => {
                    ui.label(
                        RichText::new(format!("Stored model unavailable: {model_id}"))
                            .text_style(caption())
                            .color(WARN),
                    );
                    ui.label(
                        RichText::new(
                            "The case keeps the recorded model identity, but running is blocked until that exact qualified bundle is installed or the draft is explicitly rebound.",
                        )
                        .text_style(caption())
                        .color(TEXT_MUTE),
                    );
                    if ui.button("Open Model Library").clicked() {
                        open_model_library = true;
                    }
                }
                ModelSelectorMode::NoneQualified => {
                    ui.label(
                        RichText::new("No qualified 3D model installed")
                            .text_style(caption())
                            .color(WARN),
                    );
                    ui.label(
                        RichText::new(
                            "Geometry review remains available. Reyn will not substitute the bundled 2D model or invent a compatible option.",
                        )
                        .text_style(caption())
                        .color(TEXT_MUTE),
                    );
                    if ui.button("Open Model Library").clicked() {
                        open_model_library = true;
                    }
                }
                ModelSelectorMode::OneQualified(model_id) => {
                    if let Some(model) = qualified.iter().find(|model| model.id == model_id) {
                        if case.workflow.model_id.trim().is_empty() && !read_only {
                            bind_external_flow_model(case, model);
                            identity_changed = true;
                            changed = true;
                        }
                        ui.label(RichText::new(&model.name).color(TEXT));
                        ui.label(
                            RichText::new("Only qualified model for this geometry")
                                .text_style(caption())
                                .color(TEXT_MUTE),
                        );
                    }
                }
                ModelSelectorMode::MultipleQualified => {
                    let mut selected_id = case.workflow.model_id.clone();
                    let selected_text = qualified
                        .iter()
                        .find(|model| model.id == selected_id)
                        .map(|model| model.name.as_str())
                        .unwrap_or("Select a qualified model…");
                    let response = egui::ComboBox::from_id_salt("engineering.model")
                        .selected_text(selected_text)
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            for model in &qualified {
                                ui.selectable_value(
                                    &mut selected_id,
                                    model.id.clone(),
                                    &model.name,
                                );
                            }
                        });
                    if response.response.changed() {
                        if let Some(model) = qualified.iter().find(|model| model.id == selected_id)
                        {
                            bind_external_flow_model(case, model);
                            // Model hashes are immutable identity, not undo
                            // payload. Rebase draft history at this boundary.
                            identity_changed = true;
                            changed = true;
                        }
                    }
                }
            }
            if let Some(selected) = qualified
                .iter()
                .find(|model| model.id == case.workflow.model_id)
            {
                diag(ui, "Model grid", &format!("{}³", selected.grid), TEXT_DIM);
                diag(
                    ui,
                    "Channels",
                    &format!("{} → {}", selected.in_channels, selected.out_channels),
                    TEXT_DIM,
                );
                diag(ui, "Scenario", &selected.scenario, TEXT_DIM);
                diag(ui, "Physics", &selected.physics_contract, TEXT_DIM);
                diag(
                    ui,
                    "Qualification",
                    &selected.qualification_class,
                    if selected.qualification_class
                        == engine::MODEL_QUALIFICATION_PRODUCTION
                    {
                        SUCCESS
                    } else {
                        WARN
                    },
                );
                diag(
                    ui,
                    "Horizon support",
                    &format!("1–{} steps", selected.max_steps),
                    TEXT_DIM,
                );
            }
            ui.separator();
            diag(ui, "Flow direction", "+X · fixed-body contract", TEXT_DIM);
            let reference_length_label = a11y_caption(ui, "Reference length");
            let response = ui
                .add(
                    egui::DragValue::new(&mut case.workflow.operating.reference_length)
                        .speed(0.01)
                        .range(1e-9..=1e9)
                        .suffix(format!(" {}", case.workflow.operating.length_unit.symbol())),
                )
                .labelled_by(reference_length_label.id);
            track_case_edit_response(
                &response,
                CaseEditTransaction::ReferenceLength,
                &mut changed,
                &mut changed_transaction,
                &mut active_transaction,
            );
            // Preflight suggestion: the largest cross-flow extent (the frontal
            // dimensions for a +X free stream), stated with its rationale.
            let suggested_reference = case.workflow.preflight.source_extents[1]
                .max(case.workflow.preflight.source_extents[2]);
            if suggested_reference > 0.0
                && (case.workflow.operating.reference_length - suggested_reference).abs() > 1e-12
            {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "suggested {suggested_reference:.4} {} · max cross-flow extent",
                            case.workflow.operating.length_unit.symbol()
                        ))
                        .text_style(caption())
                        .color(TEXT_MUTE),
                    );
                    if ui.small_button("Use").clicked() {
                        case.workflow.operating.reference_length = suggested_reference;
                        changed = true;
                    }
                });
            }
            ui.add_space(6.0);
            let moment_origin_label = a11y_caption(ui, "Moment / CG origin");
            let moment_mode_label = case.workflow.operating.moment_origin_mode.label();
            let moment_combo = egui::ComboBox::from_id_salt("engineering.moment-origin-mode")
                .selected_text(moment_mode_label)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for mode in [
                        engineering::MomentOriginMode::DiffuseSurfaceCentroid,
                        engineering::MomentOriginMode::SourceFramePoint,
                    ] {
                        if ui
                            .selectable_label(
                                case.workflow.operating.moment_origin_mode == mode,
                                mode.label(),
                            )
                            .clicked()
                        {
                            case.workflow.operating.moment_origin_mode = mode;
                            changed = true;
                            changed_transaction = Some(CaseEditTransaction::MomentOrigin);
                        }
                    }
                });
            moment_combo.response.labelled_by(moment_origin_label.id);
            if case.workflow.operating.moment_origin_mode
                == engineering::MomentOriginMode::SourceFramePoint
            {
                let origin_label = a11y_caption(ui, "Source-frame origin (geometry units)");
                ui.horizontal(|ui| {
                    for (axis, value) in case
                        .workflow
                        .operating
                        .moment_origin_source
                        .iter_mut()
                        .enumerate()
                    {
                        let response = ui
                            .add(
                                egui::DragValue::new(value)
                                    .speed(0.01)
                                    .prefix(format!("{} ", ["X", "Y", "Z"][axis]))
                                    .suffix(format!(
                                        " {}",
                                        case.workflow.operating.length_unit.symbol()
                                    )),
                            )
                            .labelled_by(origin_label.id);
                        track_case_edit_response(
                            &response,
                            CaseEditTransaction::MomentOrigin,
                            &mut changed,
                            &mut changed_transaction,
                            &mut active_transaction,
                        );
                    }
                });
            }
            let aero_area_label = a11y_caption(ui, "Aero reference area (optional)");
            let mut use_reference_area = case.workflow.operating.reference_area_m2.is_some();
            let area_toggle = ui
                .checkbox(&mut use_reference_area, "Override coefficient area")
                .labelled_by(aero_area_label.id);
            if area_toggle.changed() {
                case.workflow.operating.reference_area_m2 = if use_reference_area {
                    Some(
                        case.workflow
                            .operating
                            .reference_area_m2
                            .filter(|area| *area > 0.0)
                            .unwrap_or(1.0),
                    )
                } else {
                    None
                };
                changed = true;
                changed_transaction = Some(CaseEditTransaction::ReferenceArea);
            }
            if let Some(area) = case.workflow.operating.reference_area_m2.as_mut() {
                let response = ui
                    .add(
                        egui::DragValue::new(area)
                            .speed(0.001)
                            .range(1e-12..=1e9)
                            .suffix(" m²"),
                    )
                    .labelled_by(aero_area_label.id);
                track_case_edit_response(
                    &response,
                    CaseEditTransaction::ReferenceArea,
                    &mut changed,
                    &mut changed_transaction,
                    &mut active_transaction,
                );
                ui.label(
                    RichText::new("Coefficients use q∞·A and q∞·A·L when set; dimensional loads stay unchanged.")
                        .text_style(caption())
                        .color(TEXT_MUTE),
                );
            } else {
                ui.label(
                    RichText::new("Coefficients default to q∞·L² and q∞·L³.")
                        .text_style(caption())
                        .color(TEXT_MUTE),
                );
            }
            ui.separator();
            if focus_stage == Some(engineering::ReadinessStage::FlowConditions) {
                let anchor = ui.label("");
                anchor.request_focus();
                ui.scroll_to_rect(anchor.rect, Some(egui::Align::Center));
            }
            ui.label(caps("Starting defaults"));
            ui.add_space(4.0);
            ui.label(
                RichText::new("Case template")
                    .text_style(caption())
                    .color(TEXT_MUTE),
            );
            egui::ComboBox::from_id_salt("engineering.case-template")
                .selected_text("Apply template…")
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    let templates = self.settings.case_templates.clone();
                    if templates.is_empty() {
                        ui.label(
                            RichText::new("No saved templates · create one below")
                                .text_style(caption())
                                .color(TEXT_MUTE),
                        );
                    }
                    for template in templates {
                        let availability = template.validate();
                        let response = ui.add_enabled(
                            availability.is_ok(),
                            egui::Button::selectable(false, &template.name),
                        );
                        if response.clicked() {
                            match template.apply_to(
                                &mut case.workflow.operating,
                                case.workflow.model_max_steps,
                            ) {
                                Ok(operating_changed) => {
                                    changed |= operating_changed;
                                    let preferred = template.preferred_view;
                                    template_view_changed |= self.section_axis
                                        != preferred.section_axis
                                        || self.section_quantity != preferred.section_quantity;
                                    self.section_axis = preferred.section_axis;
                                    self.section_quantity = preferred.section_quantity;
                                    self.template_notice = Some((
                                        format!(
                                            "Template “{}” applied to this draft. Readiness gates remain active.",
                                            template.name
                                        ),
                                        false,
                                    ));
                                }
                                Err(error) => {
                                    self.template_notice =
                                        Some((format!("Template was not applied: {error}"), true));
                                }
                            }
                        }
                        if let Err(error) = availability {
                            response.on_disabled_hover_text(error);
                        }
                    }
                });
            if let Some((message, is_error)) = &self.template_notice {
                ui.add(
                    egui::Label::new(
                        RichText::new(message)
                            .text_style(caption())
                            .color(if *is_error { WARN } else { SUCCESS }),
                    )
                    .wrap(),
                );
            }
            ui.label(
                RichText::new("Operating preset")
                    .text_style(caption())
                    .color(TEXT_MUTE),
            );
            egui::ComboBox::from_id_salt("engineering.preset")
                .selected_text("Apply preset…")
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    let saved_presets = self.settings.operating_presets.clone();
                    for preset in settings::built_in_presets()
                        .iter()
                        .chain(saved_presets.iter())
                    {
                        if ui.selectable_label(false, &preset.name).clicked() {
                            case.workflow.operating.velocity = preset.velocity_mps;
                            case.workflow.operating.density = preset.density_kg_m3;
                            case.workflow.operating.viscosity = preset.viscosity_pa_s;
                            case.workflow.operating.reference_pressure =
                                preset.reference_pressure_pa;
                            changed = true;
                        }
                    }
                });
            ui.separator();
            ui.label(caps("Operating values"));
            ui.add_space(4.0);
            ui.label(
                RichText::new("Free-stream speed")
                    .text_style(caption())
                    .color(TEXT_MUTE),
            );
            let response = unit_value_input(
                ui,
                "engineering.unit.velocity",
                &mut case.workflow.operating.velocity,
                &mut self.input_units.velocity,
                0.1,
                1e-6..=1e5,
            );
            track_case_edit_response(
                &response,
                CaseEditTransaction::Velocity,
                &mut changed,
                &mut changed_transaction,
                &mut active_transaction,
            );
            ui.label(
                RichText::new("Density")
                    .text_style(caption())
                    .color(TEXT_MUTE),
            );
            let response = unit_value_input(
                ui,
                "engineering.unit.density",
                &mut case.workflow.operating.density,
                &mut self.input_units.density,
                0.001,
                1e-9..=1e5,
            );
            track_case_edit_response(
                &response,
                CaseEditTransaction::Density,
                &mut changed,
                &mut changed_transaction,
                &mut active_transaction,
            );
            ui.label(
                RichText::new("Dynamic viscosity")
                    .text_style(caption())
                    .color(TEXT_MUTE),
            );
            let response = unit_value_input(
                ui,
                "engineering.unit.viscosity",
                &mut case.workflow.operating.viscosity,
                &mut self.input_units.viscosity,
                1e-6,
                1e-12..=1e3,
            );
            track_case_edit_response(
                &response,
                CaseEditTransaction::Viscosity,
                &mut changed,
                &mut changed_transaction,
                &mut active_transaction,
            );
            ui.label(
                RichText::new("Reference pressure")
                    .text_style(caption())
                    .color(TEXT_MUTE),
            );
            let response = unit_value_input(
                ui,
                "engineering.unit.pressure",
                &mut case.workflow.operating.reference_pressure,
                &mut self.input_units.pressure,
                10.0,
                0.0..=1e9,
            );
            track_case_edit_response(
                &response,
                CaseEditTransaction::ReferencePressure,
                &mut changed,
                &mut changed_transaction,
                &mut active_transaction,
            );
            ui.label(
                RichText::new("Prediction horizon")
                    .text_style(caption())
                    .color(TEXT_MUTE),
            );
            let response = ui.add(
                egui::Slider::new(
                    &mut case.workflow.operating.horizon_steps,
                    1..=case.workflow.model_max_steps.max(1),
                )
                .suffix(" steps"),
            );
            track_case_edit_response(
                &response,
                CaseEditTransaction::Horizon,
                &mut changed,
                &mut changed_transaction,
                &mut active_transaction,
            );
            let reynolds = case.workflow.operating.reynolds();
            diag(
                ui,
                "Reynolds number",
                &reynolds
                    .map(|value| format!("{value:.1}"))
                    .unwrap_or_else(|| "incomplete".into()),
                if reynolds.is_some_and(|value| (60.0..=400.0).contains(&value)) {
                    SUCCESS
                } else {
                    WARN
                },
            );
            diag(
                ui,
                "Dynamic pressure",
                &case
                    .workflow
                    .operating
                    .dynamic_pressure()
                    .map(|value| {
                        units::format_quantity(
                            units::Quantity::Pressure,
                            value,
                            self.settings.unit_system,
                            self.settings.value_format(),
                        )
                    })
                    .unwrap_or_else(|| "incomplete".into()),
                TEXT_DIM,
            );
            ui.add_space(6.0);
            inspector_group(
                ui,
                "case-save-defaults",
                "Save defaults for reuse",
                false,
                |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "Presets store the fluid state and speed. Portable case templates also store the horizon and preferred section view. Neither can include geometry, identity, waivers, runs, or evidence.",
                            )
                            .text_style(caption())
                            .color(TEXT_MUTE),
                        )
                        .wrap(),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.preset_name_draft)
                                .hint_text("Preset name")
                                .desired_width((ui.available_width() - 100.0).max(80.0)),
                        );
                        let named = !self.preset_name_draft.trim().is_empty();
                        if ui
                    .add_enabled(named, egui::Button::new("Save preset"))
                    .on_hover_text(
                        "Save this fluid state and speed as a named preset (Settings › Workflow)",
                    )
                    .clicked()
                {
                    let name = self.preset_name_draft.trim().to_string();
                    let preset = settings::OperatingPointPreset {
                        name: name.clone(),
                        velocity_mps: case.workflow.operating.velocity,
                        density_kg_m3: case.workflow.operating.density,
                        viscosity_pa_s: case.workflow.operating.viscosity,
                        reference_pressure_pa: case.workflow.operating.reference_pressure,
                    };
                    self.settings
                        .operating_presets
                        .retain(|existing| existing.name != name);
                    self.settings.operating_presets.push(preset);
                    self.settings_draft.operating_presets = self.settings.operating_presets.clone();
                    self.preset_notice = Some(match self.settings.save() {
                        Ok(_) => (format!("Preset \u{201c}{name}\u{201d} saved."), false),
                        Err(error) => (format!("Preset was not saved: {error}"), true),
                    });
                    self.preset_name_draft.clear();
                }
                    });
                    if let Some((message, is_error)) = &self.preset_notice {
                        ui.add(
                            egui::Label::new(
                                RichText::new(message)
                                    .text_style(caption())
                                    .color(if *is_error { WARN } else { SUCCESS }),
                            )
                            .wrap(),
                        );
                    }
                    ui.separator();
                    ui.label(
                RichText::new(
                    "Save a reusable case template · SI operating defaults + preferred section view",
                )
                .text_style(caption())
                .color(TEXT_MUTE),
            );
                    ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.template_name_draft)
                        .hint_text("Template name")
                        .desired_width((ui.available_width() - 112.0).max(80.0)),
                );
                let named = !self.template_name_draft.trim().is_empty();
                if ui
                    .add_enabled(named, egui::Button::new("Save template"))
                    .on_hover_text(
                        "Save defaults only. Geometry, model identity, transforms, waivers, runs, and evidence are excluded.",
                    )
                    .clicked()
                {
                    let name = self.template_name_draft.trim().to_owned();
                    let template = settings::CaseTemplate::from_draft(
                        &name,
                        &case.workflow.operating,
                        self.section_axis,
                        self.section_quantity,
                    );
                    let mut candidate = self.settings.clone();
                    self.template_notice = Some(match candidate.upsert_case_template(template) {
                        Ok(()) => match candidate.save() {
                            Ok(_) => {
                                self.settings = candidate;
                                self.settings_draft.case_templates =
                                    self.settings.case_templates.clone();
                                (
                                    format!(
                                        "Template “{name}” saved. Export it from Settings › Workflow."
                                    ),
                                    false,
                                )
                            }
                            Err(error) => (format!("Template was not saved: {error}"), true),
                        },
                        Err(error) => (format!("Template was not saved: {error}"), true),
                    });
                    if self
                        .template_notice
                        .as_ref()
                        .is_some_and(|(_, is_error)| !*is_error)
                    {
                        self.template_name_draft.clear();
                    }
                }
            });
                },
            );
        });
        ui.add_space(10.0);
        card(ui, |ui| {
            if focus_stage == Some(engineering::ReadinessStage::ReviewFocus) {
                let anchor = ui.label("");
                anchor.request_focus();
                ui.scroll_to_rect(anchor.rect, Some(egui::Align::Center));
            }
            ui.label(caps("Review Focus"));
            ui.label(
                RichText::new(
                    "Choose the supported quantity Results and variant comparison should lead with. This does not change solver convergence or the computed field.",
                )
                .text_style(caption())
                .color(TEXT_MUTE),
            );
            let previous = case.workflow.review_focus.primary;
            egui::ComboBox::from_id_salt("engineering.review-focus")
                .selected_text(case.workflow.review_focus.primary.label())
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for quantity in engineering::ReviewQuantity::ALL {
                        ui.selectable_value(
                            &mut case.workflow.review_focus.primary,
                            quantity,
                            quantity.label(),
                        );
                    }
                });
            if case.workflow.review_focus.primary != previous {
                case.workflow.review_focus.schema_version =
                    engineering::REVIEW_FOCUS_SCHEMA_VERSION;
                changed = true;
            }
        });
        ui.add_space(10.0);
        if focus_stage == Some(engineering::ReadinessStage::RunReadiness) {
            let anchor = ui.label("");
            anchor.request_focus();
            ui.scroll_to_rect(anchor.rect, Some(egui::Align::Center));
        }
        let blockers = case.workflow.readiness_blockers();
        if blockers.is_empty() && case.workflow.model_support.status == "clean" {
            case.workflow.stage = engineering::CaseStage::Ready;
            ui.label(
                RichText::new("READY · CONTRACT WITHIN QUALIFIED ENVELOPE")
                    .strong()
                    .text_style(mono_s())
                    .color(SUCCESS),
            );
        } else if blockers.is_empty() {
            case.workflow.stage = engineering::CaseStage::Ready;
            ui.label(
                RichText::new("READY WITH MODEL METADATA REVIEW")
                    .strong()
                    .text_style(mono_s())
                    .color(WARN),
            );
            ui.label(
                RichText::new(
                    "The executable channel/grid contract is compatible, but checkpoint provenance is not fully CLEAN.",
                )
                .text_style(caption())
                .color(TEXT_MUTE),
            );
        } else {
            ui.label(
                RichText::new(format!("{} BLOCKER(S)", blockers.len()))
                    .strong()
                    .text_style(mono_s())
                    .color(WARN),
            );
            for blocker in blockers.iter().take(8) {
                if ui
                    .button(format!("{} · {}", blocker.stage.label(), blocker.message))
                    .clicked()
                {
                    requested_study_stage = Some(blocker.stage);
                }
            }
            for issue in case
                .workflow
                .preflight
                .support_issues()
                .into_iter()
                .filter(|issue| {
                    blockers
                        .iter()
                        .any(|blocker| blocker.message == issue.message)
                })
                .take(6)
            {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("• {} · {}", issue.code, issue.message))
                            .text_style(caption())
                            .color(TEXT_MUTE),
                    );
                    if issue.waivable && ui.small_button("Name waiver…").clicked() {
                        waiver_code = Some(issue.code.into());
                    }
                });
            }
            if waiver_code.is_some() || !waiver_draft.is_empty() {
                ui.add(
                    egui::TextEdit::singleline(&mut waiver_draft)
                        .hint_text("Specific engineering rationale (8+ characters)"),
                );
                if let Some(code) = waiver_code.as_deref() {
                    if ui.button(format!("Apply waiver for {code}")).clicked() {
                        match case.workflow.preflight.record_waiver(code, &waiver_draft) {
                            Ok(()) => {
                                waiver_draft.clear();
                                waiver_code = None;
                                changed = true;
                            }
                            Err(error) => {
                                self.project_notice = Some((error, true));
                            }
                        }
                    }
                }
            }
        }
        ui.add_space(10.0);
        let running = case.pending;
        let orientation_run_gate = orientation_geometry_gate(
            "Starting a new run",
            self.orientation_draft,
            self.orientation_pending.as_ref(),
        );
        let local_model_ready = matches!(model_inventory_state, ModelInventoryState::Ready)
            && model_inventory.iter().any(|model| {
                model.id == case.workflow.model_id
                    && engine::is_qualified_external_flow_model(
                        model,
                        case.workflow.preflight.target_grid,
                    )
            });
        let model_run_gate = (!local_model_ready).then_some(match &model_inventory_state {
            ModelInventoryState::Loading => "Installed models are still being checked.",
            ModelInventoryState::Failed(_) => {
                "The installed-model inventory is unavailable. Retry the model check."
            }
            ModelInventoryState::Ready => {
                "The case's qualified model is not installed or no longer matches this geometry."
            }
        });
        let ready = case.workflow.ready()
            && local_model_ready
            && !running
            && orientation_run_gate.is_none();
        let mut run = ui.add_enabled(
            ready,
            egui::Button::new(
                RichText::new(if running {
                    "Running…"
                } else {
                    "Run qualified analysis"
                })
                .color(ON_EMBER),
            )
            .fill(EMBER),
        );
        run.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                run.enabled(),
                if running {
                    "External-flow analysis running"
                } else if ready {
                    "Run qualified external-flow analysis"
                } else {
                    "Run qualified analysis unavailable until study gates clear"
                },
            )
        });
        if let Some(reason) = &orientation_run_gate {
            run = run.on_disabled_hover_text(reason);
        } else if let Some(reason) = model_run_gate {
            run = run.on_disabled_hover_text(reason);
        }
        let run_requested = run.clicked();
        let draft_after = engineering::CaseDraftSnapshot::capture(&case.workflow);
        if changed {
            if identity_changed {
                self.mark_case_draft_dirty();
                self.rebase_case_draft_history();
            } else {
                self.record_case_draft_change(
                    draft_before,
                    draft_after,
                    changed_transaction,
                    active_transaction,
                );
            }
            self.invalidate_active_case_result();
            self.refresh_geometry_setup_status();
            self.project_notice = Some((
                "Case contract changed. Completed runs remain immutable; the draft requires a new run."
                    .into(),
                false,
            ));
        } else if self.case_edit_transaction != active_transaction {
            // A focus/drag transition with no value change ends the prior
            // coalescing transaction without pre-opening a new one.
            self.case_edit_transaction = None;
        }
        if run_requested {
            self.run_external_flow();
        }
        if template_view_changed {
            self.invalidate_cad_section();
        }
        if apply_orientation {
            self.apply_body_orientation(draft);
        }
        if cancel_run {
            self.cancel_external_flow();
        }
        self.waiver_draft = waiver_draft;
        self.waiver_code = waiver_code;
        if schedule_orientation_autosave {
            self.schedule_autosave_from_now();
        }
        if let Some(stage) = requested_study_stage {
            self.study_focus_stage = Some(stage);
        }
        if open_model_library {
            self.nav = Nav::Models;
        }
        if retry_model_inventory {
            self.library.busy = true;
            self.model_inventory_state = ModelInventoryState::Loading;
            match self.engine.send(engine::Cmd::ListModels) {
                Ok(request) => self.library_pending_request = Some(request),
                Err(error) => {
                    self.library.busy = false;
                    self.library_pending_request = None;
                    self.model_inventory_state = ModelInventoryState::Failed(format!(
                        "The model inventory request could not be started: {error}"
                    ));
                }
            }
        }
    }

    fn controls_engineering_results(&mut self, ui: &mut egui::Ui) {
        self.engineering_notice(ui);
        let comparison = self.cad.as_ref().and_then(|active| {
            let case = self
                .project
                .manifest()
                .cases()
                .iter()
                .find(|case| case.case_id() == active.workflow.case_id)?;
            let current = active
                .active_run_id
                .as_deref()
                .and_then(|run_id| case.runs().iter().find(|run| run.run_id() == run_id))?;
            let parent_id = current.parent_run_id()?;
            let parent = case.runs().iter().find(|run| run.run_id() == parent_id)?;
            let parent_scalars: std::collections::BTreeMap<_, _> = parent
                .manifest()
                .scalar_outputs
                .iter()
                .map(|scalar| (scalar.key.clone(), scalar.clone()))
                .collect();
            let focus_key = active.workflow.review_focus.primary.scalar_key();
            let mut rows = current
                .manifest()
                .scalar_outputs
                .iter()
                .filter(|scalar| scalar.key != "divergence_rms")
                .filter_map(|scalar| {
                    parent_scalars.get(&scalar.key).and_then(|prior| {
                        (prior.units == scalar.units).then(|| {
                            (
                                scalar.key.clone(),
                                prior.value,
                                scalar.value,
                                scalar.units.clone(),
                            )
                        })
                    })
                })
                .collect::<Vec<_>>();
            rows.sort_by_key(|(key, _, _, _)| (key != focus_key, key.clone()));
            Some((
                parent.run_id().to_owned(),
                current.run_id().to_owned(),
                rows,
            ))
        });
        // R9: designed empty states — a level-1 card with one action,
        // matching the library's empty-state grammar.
        let mut go_to_case = false;
        if self.cad.is_none() {
            card(ui, |ui| {
                ui.label(title_text("No engineering result"));
                ui.label(
                    RichText::new("Import geometry and qualify a case to produce loads.")
                        .text_style(caption())
                        .color(TEXT_MUTE),
                );
                ui.add_space(8.0);
                go_to_case = ui.button("Go to Case Setup").clicked();
            });
            if go_to_case {
                self.nav = Nav::Case;
            }
            return;
        }
        let case = self.cad.as_ref().expect("checked above");
        if case.workflow.result.is_none() {
            card(ui, |ui| {
                ui.label(title_text("No current result"));
                ui.label(
                    RichText::new("Complete the approved case workflow first.")
                        .text_style(caption())
                        .color(TEXT_MUTE),
                );
                ui.add_space(8.0);
                go_to_case = ui.button("Go to Case Setup").clicked();
            });
            if go_to_case {
                self.nav = Nav::Case;
            }
            return;
        }
        let result = case.workflow.result.as_ref().expect("checked above");
        let can_export_fea = has_complete_fea_load_field(case);
        let result_grid = case.result_grid;
        ui.label(title_text("Engineering Results"));
        ui.label(
            RichText::new("applicability → loads → geometry-linked evidence")
                .text_style(caption())
                .color(TEXT_MUTE),
        );
        ui.add_space(14.0);
        // §4.4 decision order: applicability verdict first, then the numbers.
        let support_clean = case.workflow.model_support.status == "clean";
        card(ui, |ui| {
            alert_line(
                ui,
                if support_clean { OK } else { WARN },
                if support_clean { "✓" } else { "!" },
                if support_clean {
                    "Supported fixed-body contract · model-derived loads"
                } else {
                    "Supported contract · model metadata review — provenance incomplete, preserved in evidence"
                },
            );
        });
        ui.add_space(12.0);
        card(ui, |ui| {
            ui.label(caps("Review Focus"));
            ui.label(
                RichText::new(case.workflow.review_focus.primary.label())
                    .text_style(body_strong())
                    .color(GOLD),
            );
            ui.label(
                RichText::new(units::format_value(
                    case.workflow.review_focus.primary.value(result),
                    self.settings.value_format(),
                ))
                .text_style(mono_s())
                .color(TEXT),
            );
            ui.label(
                RichText::new(
                    "Emphasis only · the selected quantity did not control solver convergence.",
                )
                .text_style(caption())
                .color(TEXT_MUTE),
            );
        });
        ui.add_space(12.0);
        // Measurement table (§4.4): label body / value mono right-aligned /
        // shared unit column / source class chip on every row (N5X-EV-02).
        let unit_system = self.settings.unit_system;
        let value_format = self.settings.value_format();
        let mut copied_summary = false;
        card(ui, |ui| {
            ui.label(caps("Loads & derived quantities"));
            // The frame is part of the number: state it before the values.
            ui.label(
                RichText::new(engineering::COEFFICIENT_REFERENCE_FRAME)
                    .text_style(caption())
                    .color(TEXT_MUTE),
            );
            if !case.workflow.preflight.body_is_aligned() {
                ui.label(
                    RichText::new(format!(
                        "Body held at {} — the geometry is rotated, these axes are not.",
                        case.workflow.preflight.body_orientation_summary()
                    ))
                    .text_style(caption())
                    .color(GOLD),
                );
            }
            ui.add_space(8.0);
            let fmt = |value: f64| units::format_value(value, value_format);
            let vector = |si: [f64; 3], quantity: units::Quantity| -> (String, &'static str) {
                let symbol = units::display_value(quantity, 0.0, unit_system).1;
                let component =
                    |value: f64| fmt(units::display_value(quantity, value, unit_system).0);
                (
                    format!(
                        "[{}, {}, {}]",
                        component(si[0]),
                        component(si[1]),
                        component(si[2])
                    ),
                    symbol,
                )
            };
            // Named force coefficients (the fixed-body contract puts the free
            // stream on +X, so Cx is the drag coefficient).
            measure_row(
                ui,
                "Cd · drag (+X)",
                &fmt(result.force_coefficients[0]),
                "–",
                "DERIVED",
                BRAND,
            )
            .on_hover_text("Streamwise force coefficient; +X is the free-stream direction.");
            measure_row(
                ui,
                "Cs · side (+Y)",
                &fmt(result.force_coefficients[1]),
                "–",
                "DERIVED",
                BRAND,
            );
            measure_row(
                ui,
                "Cl · vertical (+Z)",
                &fmt(result.force_coefficients[2]),
                "–",
                "DERIVED",
                BRAND,
            );
            let (force_text, force_unit) = vector(result.force_newtons, units::Quantity::Force);
            measure_row(ui, "Fluid force", &force_text, force_unit, "DERIVED", BRAND);
            measure_row(
                ui,
                "Moment coefficients",
                &format!(
                    "[{}, {}, {}]",
                    fmt(result.moment_coefficients[0]),
                    fmt(result.moment_coefficients[1]),
                    fmt(result.moment_coefficients[2])
                ),
                "–",
                "DERIVED",
                BRAND,
            );
            let (moment_text, moment_unit) =
                vector(result.moment_newton_meters, units::Quantity::Moment);
            let moment_label = match result.moment_origin_mode.as_str() {
                "source_frame_point" => "Fluid moment · source-frame origin",
                _ => "Fluid moment · surface centroid",
            };
            measure_row(
                ui,
                moment_label,
                &moment_text,
                moment_unit,
                "DERIVED",
                BRAND,
            );
            // Cp is derived from recovered pressure (N5X-PHYS-01): the
            // nondimensionalization note sits on hover, one disclosure away.
            measure_row(
                ui,
                "Cp range",
                &format!("{} … {}", fmt(result.cp_min), fmt(result.cp_max)),
                "–",
                "DERIVED",
                GOLD,
            )
            .on_hover_text(
                "Derived from recovered pressure, nondimensionalized by ½·ρ·U². \
                 Recovered fields are reconstructed, not directly solved.",
            );
            let (area_value, area_unit) =
                units::display_value(units::Quantity::Area, result.surface_area_m2, unit_system);
            measure_row(
                ui,
                "Diffuse surface area",
                &fmt(area_value),
                area_unit,
                "DERIVED",
                TEXT_DIM,
            );
            measure_row(
                ui,
                "Pressure share · component norms",
                &format!("{:.1}", result.pressure_force_fraction * 100.0),
                "%",
                "DERIVED",
                TEXT_DIM,
            );
            measure_row(
                ui,
                "Wake deficit · peak / mean",
                &format!(
                    "{} / {}",
                    fmt(result.wake_deficit_peak),
                    fmt(result.wake_deficit_mean)
                ),
                "–",
                "DERIVED",
                TEXT_DIM,
            );
            // Divergence RMS stays in immutable evidence JSON for developer
            // forensics; it is not a customer Results instrument.
        });
        ui.add_space(8.0);
        // Reference values the coefficients were scaled with — visible next
        // to the numbers, in the display unit system.
        card(ui, |ui| {
            ui.label(caps("Reference values"));
            ui.add_space(6.0);
            let operating = &case.workflow.operating;
            let reference = |quantity: units::Quantity, si: f64| {
                units::format_quantity(quantity, si, unit_system, value_format)
            };
            diag(
                ui,
                "V∞",
                &reference(units::Quantity::Velocity, operating.velocity),
                TEXT_DIM,
            );
            diag(
                ui,
                "ρ∞",
                &reference(units::Quantity::Density, operating.density),
                TEXT_DIM,
            );
            diag(
                ui,
                "q∞",
                &operating
                    .dynamic_pressure()
                    .map(|value| reference(units::Quantity::Pressure, value))
                    .unwrap_or_else(|| "incomplete".into()),
                TEXT_DIM,
            );
            diag(
                ui,
                "L reference",
                &format!(
                    "{} {}",
                    units::format_value(operating.reference_length, value_format),
                    operating.length_unit.symbol()
                ),
                TEXT_DIM,
            );
            diag(
                ui,
                "Reynolds",
                &operating
                    .reynolds()
                    .map(|value| units::format_value(value, value_format))
                    .unwrap_or_else(|| "incomplete".into()),
                TEXT_DIM,
            );
            ui.add_space(6.0);
            if ui
                .button(format!("{} Copy summary", ph::COPY))
                .on_hover_text(
                    "Copies the full results table (coefficients, loads, reference values, \
                     provenance ids) as tab-separated text for spreadsheets and notes.",
                )
                .clicked()
            {
                let text = results_summary_tsv(
                    &case.workflow,
                    case.active_run_id.as_deref(),
                    unit_system,
                    value_format,
                );
                ui.ctx().copy_text(text);
                copied_summary = true;
            }
        });
        if copied_summary {
            self.project_notice = Some(("Results summary copied to the clipboard.".into(), false));
        }
        ui.add_space(12.0);
        self.horizon_playback_card(ui);
        if let Some((parent_run_id, current_run_id, rows)) = comparison {
            ui.add_space(12.0);
            let force_series = force_variant_series(&rows);
            card(ui, |ui| {
                ui.label(caps("Variant comparison · shared units"));
                ui.label(
                    RichText::new(format!(
                        "{} → {}",
                        short_id(&parent_run_id),
                        short_id(&current_run_id)
                    ))
                    .text_style(mono_s())
                    .color(TEXT_MUTE),
                );
                ui.add_space(6.0);
                if force_series.is_empty() {
                    ui.label(
                        RichText::new(
                            "No shared force-coefficient scalars to plot for this parent/current pair.",
                        )
                        .text_style(caption())
                        .color(TEXT_MUTE),
                    );
                } else {
                    ui.label(
                        RichText::new("Force coefficients · parent vs current")
                            .text_style(caption())
                            .color(GOLD),
                    );
                    ui.label(
                        RichText::new(
                            "Gold = parent · ember = current · Δ labeled under each pair",
                        )
                        .text_style(caption())
                        .color(TEXT_MUTE),
                    );
                    ui.add_space(4.0);
                    draw_force_variant_plot(ui, &force_series);
                }
                ui.add_space(8.0);
                ui.label(caps("Scalar deltas"));
                for (key, parent, current, units) in rows.iter().take(7) {
                    diag(
                        ui,
                        key,
                        &format!(
                            "{parent:.5e} → {current:.5e}  Δ {:+.3e} {units}",
                            current - parent
                        ),
                        if (current - parent).abs() <= 1e-6 {
                            TEXT_DIM
                        } else {
                            GOLD
                        },
                    );
                }
                ui.horizontal(|ui| {
                    if ui.button("Inspect parent evidence").clicked() {
                        self.select_external_run(&parent_run_id);
                    }
                    if ui.button("Inspect current evidence").clicked() {
                        self.select_external_run(&current_run_id);
                    }
                });
            });
        }
        // Phase 0.4: stop-reason / warning console for the selected immutable run.
        if let Some((stop_reason, warnings, runtime_ms, run_id)) =
            self.cad.as_ref().and_then(|active| {
                let run_id = active.active_run_id.as_deref()?;
                let case = self
                    .project
                    .manifest()
                    .cases()
                    .iter()
                    .find(|case| case.case_id() == active.workflow.case_id)?;
                let run = case.runs().iter().find(|run| run.run_id() == run_id)?;
                Some((
                    run.manifest().stop_reason.clone(),
                    run.manifest().warnings.clone(),
                    run.manifest().runtime_ms,
                    run_id.to_owned(),
                ))
            })
        {
            ui.add_space(12.0);
            card(ui, |ui| {
                ui.label(caps("Run detail"));
                ui.label(
                    RichText::new(format!("Attempt {}", short_id(&run_id)))
                        .text_style(mono_s())
                        .color(TEXT_MUTE),
                );
                ui.add_space(4.0);
                diag(ui, "Stop reason", &stop_reason, TEXT_DIM);
                diag(
                    ui,
                    "Runtime",
                    &format!("{:.2} s", runtime_ms as f64 / 1000.0),
                    TEXT_DIM,
                );
                if warnings.is_empty() {
                    ui.label(
                        RichText::new("No warnings recorded on this attempt.")
                            .text_style(caption())
                            .color(TEXT_MUTE),
                    );
                } else {
                    ui.label(caps("Warnings"));
                    for warning in warnings.iter().take(8) {
                        ui.label(
                            RichText::new(format!("· {warning}"))
                                .text_style(caption())
                                .color(WARN),
                        );
                    }
                    if warnings.len() > 8 {
                        ui.label(
                            RichText::new(format!(
                                "… {} more in the evidence ledger",
                                warnings.len() - 8
                            ))
                            .text_style(caption())
                            .color(TEXT_MUTE),
                        );
                    }
                }
            });
        }
        ui.add_space(12.0);
        if self.volumetric {
            // Viewport controls collapse into an inspector accordion (§4.4)
            // so numbers stay above pictures without scrolling.
            inspector_group(ui, "results-clip", "Geometry clipping planes", true, |ui| {
                for (axis, label) in ["X", "Y", "Z"].iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.slice[axis], *label);
                        ui.add(
                            egui::Slider::new(&mut self.slice_pos[axis], 0.0..=1.0)
                                .show_value(true)
                                .trailing_fill(true),
                        );
                    });
                }
                if ui
                    .button(RichText::new("Mid-plane Y").text_style(caption()))
                    .on_hover_text("Clip at Y = 0.5 without changing Fit")
                    .clicked()
                {
                    self.slice = [false, true, false];
                    self.slice_pos[1] = 0.5;
                }
            });
            inspector_group(ui, "results-layers", "Layers", true, |ui| {
                ui.checkbox(&mut self.surface_on, "Cp surface");
                ui.checkbox(&mut self.streamlines, "Model streamlines")
                    .on_hover_text(viewport::MODEL_STREAMLINE_LABEL);
                ui.checkbox(&mut self.q_iso_on, "Q iso-surface")
                    .on_hover_text(
                        "Thin Q-criterion cores when vorticity volume is off. Disabled while the full wake volume is on.",
                    );
                ui.checkbox(&mut self.render_volume, "Vorticity volume")
                    .on_hover_text("Optional full-domain |ω| fog — off by default");
                ui.checkbox(&mut self.insights_on, "Load and suction hotspots");
            });
        } else {
            ui.label(caps("2D section quantity"));
            egui::ComboBox::from_id_salt("engineering_section_quantity")
                .width(ui.available_width())
                .selected_text(self.section_quantity.label())
                .show_ui(ui, |ui| {
                    for quantity in engineering_section::SectionQuantity::ALL {
                        ui.selectable_value(&mut self.section_quantity, quantity, quantity.label());
                    }
                });
            ui.add_space(10.0);
            ui.label(caps("Section plane"));
            // Concentric radii: thumb r-1 = 4 inside container r-2 = 6 (§3.5).
            Frame::NONE
                .fill(SURFACE)
                .corner_radius(CornerRadius::same(R2))
                .stroke(Stroke::new(1.0, HAIRLINE))
                .inner_margin(2)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for axis in engineering_section::SectionAxis::ALL {
                            if seg(ui, axis.label(), self.section_axis == axis) {
                                self.section_axis = axis;
                            }
                        }
                    });
                });
            let axis_index = self.section_axis.id() as usize;
            ui.add(
                egui::Slider::new(&mut self.slice_pos[axis_index], 0.0..=1.0)
                    .show_value(false)
                    .trailing_fill(true),
            );
            let section_index =
                engineering_section::section_index(result_grid.max(3), self.slice_pos[axis_index])
                    .unwrap_or_default();
            ui.label(
                mono(
                    &format!(
                        "{} = {:.3} domain · cell {}/{}",
                        self.section_axis.label(),
                        self.slice_pos[axis_index],
                        section_index,
                        result_grid.saturating_sub(1)
                    ),
                    TEXT_DIM,
                )
                .text_style(caption()),
            );
            ui.add_space(10.0);
            card(ui, |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} · {}",
                        self.section_quantity.label(),
                        self.section_quantity.units()
                    ))
                    .strong()
                    .color(TEXT),
                );
                ui.label(
                    RichText::new(self.section_quantity.source())
                        .text_style(mono_s())
                        .color(GOLD),
                );
                ui.label(
                    RichText::new(self.section_quantity.method())
                        .text_style(caption())
                        .color(TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!(
                        "VIEW +{} · +{} right · +{} up",
                        self.section_axis.label(),
                        self.section_axis.horizontal_axis(),
                        self.section_axis.vertical_axis()
                    ))
                    .text_style(mono_s())
                    .color(TEXT_MUTE),
                );
                ui.label(
                    RichText::new("GEOMETRY · stored diffuse CAD mask")
                        .text_style(mono_s())
                        .color(BRAND),
                );
            });
        }
        ui.add_space(8.0);
        // WARN is the status hue for missing capability; GOLD stays on data.
        alert_line(
            ui,
            WARN,
            "!",
            "Spatial error unavailable — attach an exact solver reference before displaying error.",
        );
        // Voxel diagnostics moved here from global nav (§4.1): they describe
        // the rendered model velocity field, so they live beside it, with an
        // explicit source class (SCI-AC-01).
        ui.add_space(8.0);
        inspector_group(ui, "results-voxel", "Voxel diagnostics", false, |ui| {
            ui.label(chip_text("MODEL · rendered velocity field").color(BRAND));
            ui.add_space(8.0);
            let (helicity, enstrophy, q_criterion, voxel_count) = diagnostics(&self.particles);
            diag(ui, "Helicity", &format!("{:.1e}", helicity), BRAND);
            diag(ui, "Enstrophy Vol.", &format!("{:.2e}", enstrophy), BRAND);
            diag(ui, "Q-Criterion", &format!("{:.2}", q_criterion), GOLD);
            diag(
                ui,
                "Voxel Count",
                &format!("{:.1}K", voxel_count as f32 / 1000.0),
                TEXT,
            );
        });
        ui.add_space(12.0);
        if ui
            .add_enabled(
                !self.project.availability().is_read_only_evidence(),
                egui::Button::new("Create Operating-Point Variant"),
            )
            .on_disabled_hover_text(
                "Case variants are blocked while the project is in read-only evidence mode.",
            )
            .clicked()
            && !self.reject_project_mutation("Creating an operating-point variant")
        {
            self.invalidate_active_case_result();
            self.rebase_case_draft_history();
            self.nav = Nav::Case;
        }
        // §4.4: one quiet export entry point; the disabled item explains
        // itself (UX-AC-01) instead of disappearing.
        let mut export_fea = false;
        let mut export_field = false;
        let mut export_report = false;
        let mut export_section = false;
        let mut export_viewport = false;
        let has_section = !self.volumetric && self.section_data.is_some();
        ui.menu_button("Export evidence…", |ui| {
            ui.label(overline_text("Immutable run artifacts"));
            ui.add_space(4.0);
            if ui
                .add_enabled(
                    can_export_fea,
                    egui::Button::new("Engineering report (HTML)…"),
                )
                .on_disabled_hover_text("A durable immutable run is required for provenance.")
                .on_hover_text(
                    "Self-contained HTML: provenance chain, operating point, preflight, \
                     coefficients, section figure, and the limitations block.",
                )
                .clicked()
            {
                export_report = true;
                ui.close();
            }
            if ui
                .add_enabled(
                    can_export_fea,
                    egui::Button::new("Surface loads for FEA (CSV)…"),
                )
                .on_disabled_hover_text("A durable immutable run is required for provenance.")
                .clicked()
            {
                export_fea = true;
                ui.close();
            }
            if ui
                .add_enabled(
                    can_export_fea,
                    egui::Button::new("Full field evidence (VTK)…"),
                )
                .on_disabled_hover_text("A durable immutable run is required for provenance.")
                .on_hover_text(
                    "ParaView-readable structured grid in the approved source frame: velocity, \
                     recovered pressure, Cp, fluid traction, occupancy, and provenance.",
                )
                .clicked()
            {
                export_field = true;
                ui.close();
            }
            ui.separator();
            ui.label(overline_text("Current view"));
            ui.add_space(4.0);
            if ui
                .add_enabled(has_section, egui::Button::new("Section view (PNG)…"))
                .on_disabled_hover_text("Open a 2D section in Results first.")
                .clicked()
            {
                export_section = true;
                ui.close();
            }
            if ui
                .add_enabled(self.volumetric, egui::Button::new("3D viewport (PNG)…"))
                .on_disabled_hover_text("Switch to the 3D view to capture the viewport.")
                .clicked()
            {
                export_viewport = true;
                ui.close();
            }
        });
        if export_fea {
            self.export_fea_loads();
        }
        if export_field {
            self.export_vtk_field();
        }
        if export_report {
            self.export_engineering_report();
        }
        if export_section {
            self.export_section_png();
        }
        if export_viewport {
            self.request_viewport_png(ui.ctx());
        }
        if ui.button("Open Evidence & Provenance").clicked() {
            self.nav = Nav::Evidence;
        }
    }

    fn select_external_run(&mut self, run_id: &str) {
        let Some(case_id) = self
            .project
            .manifest()
            .cases()
            .iter()
            .find(|case| case.runs().iter().any(|run| run.run_id() == run_id))
            .map(|case| case.case_id().to_owned())
        else {
            self.project_notice = Some(("The selected run no longer exists.".into(), true));
            return;
        };
        let selected_evidence_id = self
            .project
            .manifest()
            .evidence_for_run(run_id)
            .into_iter()
            .rev()
            .find(|artifact| {
                artifact
                    .metadata
                    .get("schema")
                    .and_then(serde_json::Value::as_str)
                    == Some(engineering::ENGINEERING_RESULT_SCHEMA)
            })
            .map(|artifact| artifact.evidence_id.clone());
        let run_id_owned = run_id.to_owned();
        let selection = project::ProjectSelection {
            active_case_id: Some(case_id),
            selected_run_id: Some(run_id_owned),
            selected_evidence_id,
            selected_view_id: None,
        };
        if self.project.availability().is_read_only_evidence() {
            self.review_selection = Some(selection);
            self.hydrate_project_runtime();
            self.project_notice = Some((
                "Opened immutable run evidence with a session-only review selection; the read-only project was not changed."
                    .into(),
                false,
            ));
            return;
        }
        self.review_selection = None;
        match self.transact_project("Selecting an immutable run", now_utc_unix(), |manifest| {
            manifest.set_selection(selection, now_utc_unix())
        }) {
            Ok(()) => self.hydrate_project_runtime(),
            Err(error) => {
                self.project_notice = Some((
                    format!("Evidence selection was not persisted: {error}"),
                    true,
                ));
            }
        }
    }

    fn controls_engineering_evidence(&mut self, ui: &mut egui::Ui) {
        ui.label(title_text("Evidence"));
        ui.label(
            RichText::new("exact inputs, lineage, warnings, and exports")
                .text_style(caption())
                .color(TEXT_MUTE),
        );
        ui.add_space(14.0);
        self.engineering_notice(ui);
        if let Some(case) = &self.cad {
            let has_persisted_run = case.active_run_id.is_some();
            let can_export_fea = has_complete_fea_load_field(case);
            card(ui, |ui| {
                diag(
                    ui,
                    "Source",
                    &short_hash(&case.workflow.preflight.source_sha256),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Case revision",
                    &case
                        .workflow
                        .case_revision_id
                        .as_deref()
                        .map(short_id)
                        .unwrap_or_else(|| "unavailable".into()),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Run",
                    &case
                        .active_run_id
                        .as_deref()
                        .map(short_id)
                        .unwrap_or_else(|| "not run".into()),
                    BRAND,
                );
                diag(ui, "Model", &case.workflow.model_id, TEXT);
                diag(
                    ui,
                    "Load method",
                    engineering::SURFACE_LOAD_METHOD,
                    TEXT_DIM,
                );
            });
            ui.add_space(10.0);
            let mut export_report = false;
            let mut export_fea = false;
            let mut export_field = false;
            let mut sign_result = false;
            let signing_key_ready = matches!(self.settings.configured_signing_key(), Ok(Some(_)))
                && !self.settings.signing_key_is_revoked();
            let export_menu = ui.add_enabled_ui(has_persisted_run, |ui| {
                ui.menu_button("Export evidence…", |ui| {
                    ui.label(overline_text("Immutable run artifacts"));
                    ui.add_space(4.0);
                    if ui.button("Engineering report (HTML)…").clicked() {
                        export_report = true;
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            can_export_fea,
                            egui::Button::new("Surface loads for FEA (CSV)…"),
                        )
                        .on_disabled_hover_text(
                            "A succeeded run with a complete mapped load field is required.",
                        )
                        .clicked()
                    {
                        export_fea = true;
                        ui.close();
                    }
                    if ui
                        .button("Full field evidence (VTK)…")
                        .on_hover_text(
                            "ParaView-readable structured grid in the approved source frame; \
                             previews and draft fields are never used.",
                        )
                        .clicked()
                    {
                        export_field = true;
                        ui.close();
                    }
                });
            });
            export_menu
                .response
                .on_disabled_hover_text("A durable immutable run is required for provenance.");
            ui.add_space(6.0);
            let sign_enabled = has_persisted_run && signing_key_ready;
            let sign_disabled_hint = if !signing_key_ready {
                "Configure an Ed25519 Keychain signing key in Settings before signing evidence."
            } else {
                "A durable immutable run is required before authenticity evidence can be attached."
            };
            if ui
                .add_enabled(sign_enabled, egui::Button::new("Sign engineering result…"))
                .on_hover_text(
                    "Detach an Ed25519 sidecar over the persisted engineering-result digest \
                     and append it as authenticity evidence. The result JSON stays immutable.",
                )
                .on_disabled_hover_text(sign_disabled_hint)
                .clicked()
            {
                sign_result = true;
            }
            if export_report {
                self.export_engineering_report();
            }
            if export_fea {
                self.export_fea_loads();
            }
            if export_field {
                self.export_vtk_field();
            }
            if sign_result {
                self.sign_engineering_result_evidence();
            }
            ui.add_space(4.0);
            if ui.button("Save project").clicked() {
                self.save_project_dialog();
            }
        } else {
            ui.label(RichText::new("No case evidence is active.").color(TEXT_MUTE));
        }
    }

    fn engineering_case_view(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            // One shared content column (§3.2, QA G3/G4): symmetric gutters
            // with the 34px minimum and the capped — never expanded — width.
            content_column(ui, CONTENT_MAX_WIDTH, |ui| {
                ui.add_space(28.0);
                    if self.cad.is_none() {
                        ui.add_space(30.0);
                        ui.label(
                            display_text("Start an engineering case"),
                        );
                        ui.label(
                            RichText::new(
                                "Import fixed-body STL or single-part STEP geometry to create a source-aware, revisioned external-flow analysis.",
                            )
                            .size(13.0)
                            .color(TEXT_MUTE),
                        );
                        ui.add_space(20.0);
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Import Geometry…").color(ON_EMBER),
                                )
                                .fill(EMBER)
                                .min_size(Vec2::new(190.0, 42.0)),
                            )
                            .clicked()
                        {
                            self.import_cad();
                        }
                        ui.add_space(24.0);
                        internal_flow_reference_card(ui);
                        return;
                    }
                    let case = self.cad.as_ref().expect("checked above");
                    ui.label(
                        display_text(&case.workflow.name),
                    );
                    ui.add_space(4.0);
                    let lineage = format!(
                        "{} · source revision {} · case revision {}",
                        case.workflow.source_name,
                        case.workflow
                            .source_revision_id
                            .as_deref()
                            .map(short_id)
                            .unwrap_or_else(|| "unknown".into()),
                        case.workflow
                            .case_revision_id
                            .as_deref()
                            .map(short_id)
                            .unwrap_or_else(|| "unknown".into())
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(&lineage)
                                .text_style(mono_s())
                                .color(TEXT_MUTE),
                        )
                        .truncate(),
                    )
                    .on_hover_text(lineage);
                    ui.add_space(24.0);
                    self.case_stage_spine(ui);
                    ui.add_space(28.0);
                });
        });
    }

    /// §4.3: the guided vertical stage spine — Source → Preflight → Contract
    /// → Operating point → Run. Each stage carries a verdict-first body
    /// (named gate result, then a facts table), the current stage opens by
    /// default, and nothing is hidden that a gate needs to explain.
    fn case_stage_spine(&self, ui: &mut egui::Ui) {
        let Some(case) = self.cad.as_ref() else {
            return;
        };
        let workflow = &case.workflow;
        let preflight = &workflow.preflight;
        let stage_index = workflow.stage.progress_index();
        let spine_units = self.settings.unit_system;
        let spine_format = self.settings.value_format();

        // -- Source ----------------------------------------------------------
        let source_summary = format!(
            "{} · {} triangles · {}",
            workflow.source_name,
            preflight.triangles,
            format_bytes(preflight.source_bytes),
        );
        spine_stage(
            ui,
            "source",
            "✓",
            OK,
            "Source",
            &source_summary,
            stage_index == 0,
            |ui| {
                diag(ui, "Source file", &workflow.source_name, TEXT_DIM);
                diag(
                    ui,
                    "Source SHA-256",
                    &short_hash(&preflight.source_sha256),
                    TEXT_DIM,
                );
                diag(ui, "Triangles", &preflight.triangles.to_string(), TEXT_DIM);
                diag(
                    ui,
                    "Components",
                    &preflight.components.to_string(),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Extents",
                    &format!(
                        "{:.3} × {:.3} × {:.3}",
                        preflight.source_extents[0],
                        preflight.source_extents[1],
                        preflight.source_extents[2]
                    ),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Transform",
                    if preflight.transform_approved {
                        "approved"
                    } else {
                        "awaiting approval"
                    },
                    if preflight.transform_approved {
                        TEXT_DIM
                    } else {
                        WARN
                    },
                );
            },
        );

        // -- Preflight -------------------------------------------------------
        let blocking = if preflight.ready() {
            Vec::new()
        } else {
            preflight.blocking_issues()
        };
        let watertight = preflight.boundary_edges == 0 && preflight.non_manifold_edges == 0;
        let (pf_glyph, pf_color, pf_summary) = if blocking.is_empty() {
            (
                "✓",
                OK,
                format!(
                    "Accepted · {} · {} component{}",
                    if watertight {
                        "watertight"
                    } else {
                        "open edges waived"
                    },
                    preflight.components,
                    if preflight.components == 1 { "" } else { "s" },
                ),
            )
        } else {
            (
                "!",
                DANGER,
                format!(
                    "Blocked · {} issue{}",
                    blocking.len(),
                    if blocking.len() == 1 { "" } else { "s" },
                ),
            )
        };
        spine_stage(
            ui,
            "preflight",
            pf_glyph,
            pf_color,
            "Preflight",
            &pf_summary,
            stage_index == 1 || !blocking.is_empty(),
            |ui| {
                if blocking.is_empty() {
                    alert_line(
                        ui,
                        OK,
                        "✓",
                        &format!(
                            "Geometry gate accepted · {} boundary edges · {} non-manifold edges",
                            preflight.boundary_edges, preflight.non_manifold_edges
                        ),
                    );
                } else {
                    for issue in &blocking {
                        alert_line(ui, DANGER, "×", issue);
                    }
                }
                for waiver in &preflight.waivers {
                    alert_line(ui, WARN, "!", &format!("Waived · {waiver}"));
                }
                ui.add_space(4.0);
                diag(
                    ui,
                    "Topology",
                    if watertight { "closed" } else { "open edges" },
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Boundary edges",
                    &preflight.boundary_edges.to_string(),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Non-manifold edges",
                    &preflight.non_manifold_edges.to_string(),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Minimum thickness",
                    &format!("{} cells", preflight.minimum_cells_across),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Boundary clearance",
                    &format!("{} cells", preflight.boundary_clearance_cells),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Solid voxels",
                    &preflight.solid_voxels.to_string(),
                    TEXT_DIM,
                );
            },
        );

        // -- Contract (model support) -----------------------------------------
        let support = &workflow.model_support;
        let contract_clean = support.status == "clean";
        let (ct_glyph, ct_color, ct_summary) = if contract_clean {
            (
                "✓",
                OK,
                format!(
                    "Compatible · {}³ grid · {} → {} channels",
                    support.grid, support.input_channels, support.output_channels
                ),
            )
        } else if support.status.is_empty() {
            ("○", TEXT_MUTE, "No model selected".to_owned())
        } else {
            (
                "!",
                WARN,
                format!("Metadata review · status {}", support.status),
            )
        };
        spine_stage(
            ui,
            "contract",
            ct_glyph,
            ct_color,
            "Contract",
            &ct_summary,
            stage_index == 2,
            |ui| {
                diag(ui, "Grid", &format!("{}³", support.grid), TEXT_DIM);
                diag(
                    ui,
                    "Channels",
                    &format!("{} → {}", support.input_channels, support.output_channels),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Horizon",
                    &format!("1–{}", workflow.model_max_steps),
                    TEXT_DIM,
                );
                diag(ui, "Physics contract", &support.physics_contract, TEXT_DIM);
                diag(ui, "Geometry regime", &support.scenario, TEXT_DIM);
            },
        );

        // -- Operating point ---------------------------------------------------
        let operating = &workflow.operating;
        let op_issues = operating.validation(workflow.model_max_steps);
        let op_summary = match operating.reynolds() {
            Some(reynolds) => format!(
                "Re {reynolds:.1} · {:.2} m/s · {} · horizon {}",
                operating.velocity,
                operating.length_unit.symbol(),
                operating.horizon_steps,
            ),
            None => "Units UNKNOWN — confirm geometry units".to_owned(),
        };
        let (op_glyph, op_color) = if op_issues.is_empty() {
            ("✓", OK)
        } else {
            ("!", WARN)
        };
        spine_stage(
            ui,
            "operating",
            op_glyph,
            op_color,
            "Operating point",
            &op_summary,
            stage_index == 2 || !op_issues.is_empty(),
            |ui| {
                for issue in &op_issues {
                    alert_line(ui, WARN, "!", issue);
                }
                if !op_issues.is_empty() {
                    ui.add_space(4.0);
                }
                diag(
                    ui,
                    "Geometry units",
                    operating.length_unit.symbol(),
                    if operating.length_unit == engineering::LengthUnit::Unknown {
                        WARN
                    } else {
                        TEXT_DIM
                    },
                );
                diag(
                    ui,
                    "Reference length",
                    &format!(
                        "{:.4} {}",
                        operating.reference_length,
                        operating.length_unit.symbol()
                    ),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Free-stream speed",
                    &units::format_quantity(
                        units::Quantity::Velocity,
                        operating.velocity,
                        spine_units,
                        spine_format,
                    ),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Density",
                    &units::format_quantity(
                        units::Quantity::Density,
                        operating.density,
                        spine_units,
                        spine_format,
                    ),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Dynamic viscosity",
                    &units::format_quantity(
                        units::Quantity::Viscosity,
                        operating.viscosity,
                        spine_units,
                        spine_format,
                    ),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Reynolds number",
                    &operating
                        .reynolds()
                        .map(|re| format!("{re:.1}"))
                        .unwrap_or_else(|| "UNKNOWN".into()),
                    TEXT_DIM,
                );
                diag(
                    ui,
                    "Horizon",
                    &format!("{} steps", operating.horizon_steps),
                    TEXT_DIM,
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Edit these values in the case controls on the right.")
                        .text_style(caption())
                        .color(TEXT_MUTE),
                );
            },
        );

        // -- Run (execution gate) ----------------------------------------------
        let issues = workflow.readiness_issues();
        let (run_glyph, run_color, run_summary) = if case.pending {
            ("●", EMBER, "Running · inputs locked".to_owned())
        } else if issues.is_empty() && contract_clean {
            ("✓", OK, "Ready".to_owned())
        } else if issues.is_empty() {
            ("!", WARN, "Ready · model metadata review".to_owned())
        } else {
            (
                "×",
                DANGER,
                format!(
                    "Blocked · {} issue{}",
                    issues.len(),
                    if issues.len() == 1 { "" } else { "s" },
                ),
            )
        };
        spine_stage(
            ui,
            "run",
            run_glyph,
            run_color,
            "Run",
            &run_summary,
            stage_index >= 3 || !issues.is_empty(),
            |ui| {
                if case.pending {
                    alert_line(
                        ui,
                        EMBER,
                        "●",
                        "Running — the result will be stored as a new immutable run with exact lineage.",
                    );
                } else if issues.is_empty() && contract_clean {
                    alert_line(
                        ui,
                        OK,
                        "✓",
                        "Geometry, transform, operating point, horizon, and model satisfy the current fixed-body support contract.",
                    );
                } else if issues.is_empty() {
                    alert_line(
                        ui,
                        WARN,
                        "!",
                        "The executable fixed-body contract is compatible. Checkpoint provenance or qualification metadata remains incomplete and is preserved in evidence.",
                    );
                } else {
                    for issue in &issues {
                        alert_line(ui, DANGER, "×", issue);
                    }
                }
            },
        );
    }

    fn engineering_results_empty_view(&mut self, ui: &mut egui::Ui) {
        let has_case = self.cad.is_some();
        let (title, detail, action) = if has_case {
            (
                "No completed engineering result",
                "This case has no current immutable run. Review the setup gates, then run the qualified case.",
                "Review Case Setup",
            )
        } else {
            (
                "No engineering result",
                "Start with fixed-body STL or single-part STEP geometry. Results appear here only after the supported case completes.",
                "Start in Case Setup",
            )
        };
        let mut go_to_case = false;
        egui::ScrollArea::vertical().show(ui, |ui| {
            content_column(ui, CONTENT_MAX_WIDTH, |ui| {
                ui.add_space(48.0);
                ui.label(display_text("Engineering results"));
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Applicability, physical loads, and source-labeled fields from an immutable run.",
                    )
                    .text_style(caption())
                    .color(TEXT_MUTE),
                );
                ui.add_space(24.0);
                card(ui, |ui| {
                    ui.label(title_text(title));
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(detail).text_style(caption()).color(TEXT_DIM),
                        )
                        .wrap(),
                    );
                    ui.add_space(12.0);
                    go_to_case = ui
                        .add(
                            egui::Button::new(RichText::new(action).color(ON_EMBER))
                                .fill(EMBER)
                                .min_size(Vec2::new(160.0, 34.0)),
                        )
                        .clicked();
                });
            });
        });
        if go_to_case {
            self.nav = Nav::Case;
        }
    }

    fn engineering_evidence_view(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            // One shared content column (§3.2, QA G3/G4).
            content_column(ui, CONTENT_MAX_WIDTH, |ui| {
                    ui.add_space(28.0);
                    ui.label(
                        display_text("Traceable engineering evidence"),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Every result is tied to exact source bytes, transform, operating point, model hash, run, and derivation method.",
                        )
                        .text_style(caption())
                        .color(TEXT_MUTE),
                    );
                    ui.add_space(20.0);
                    // §4.5: read-only review mode gets a designed banner
                    // (info recipe) instead of implied absence.
                    if !self.engine_ok {
                        card(ui, |ui| {
                            alert_line(
                                ui,
                                TEXT_DIM,
                                "○",
                                "Read-only review — engine unavailable. Stored fields and evidence remain inspectable.",
                            );
                        });
                        ui.add_space(14.0);
                    }
                    if self.cad.is_none() {
                        let mut go_to_case = false;
                        card(ui, |ui| {
                            ui.label(title_text("No case evidence yet"));
                            ui.add_space(4.0);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(
                                        "Start an engineering case. Source and draft lineage appear here before the first run; immutable artifacts follow after completion.",
                                    )
                                    .text_style(caption())
                                    .color(TEXT_DIM),
                                )
                                .wrap(),
                            );
                            ui.add_space(12.0);
                            go_to_case = ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("Start in Case Setup").color(ON_EMBER),
                                    )
                                    .fill(EMBER)
                                    .min_size(Vec2::new(160.0, 34.0)),
                                )
                                .clicked();
                        });
                        if go_to_case {
                            self.nav = Nav::Case;
                        }
                        return;
                    }
                    let case = self.cad.as_ref().expect("checked above");
                    // Lineage as a scannable ledger (§4.5): level-0 rows,
                    // hairline separated — source → case revision → run →
                    // model hash. Hash rows truncate to 12 chars with the
                    // full value on hover and a copy affordance.
                    ui.label(overline_text("Lineage"));
                    ui.add_space(8.0);
                    ledger_row(
                        ui,
                        "Geometry SHA-256",
                        Some(&case.workflow.preflight.source_sha256),
                        TEXT_DIM,
                        "unknown",
                    );
                    ledger_row(
                        ui,
                        "Source revision",
                        case.workflow.source_revision_id.as_deref(),
                        TEXT_DIM,
                        "unknown",
                    );
                    ledger_row(
                        ui,
                        "Case revision",
                        case.workflow.case_revision_id.as_deref(),
                        TEXT_DIM,
                        "unknown",
                    );
                    ledger_row(
                        ui,
                        "Immutable run",
                        case.active_run_id.as_deref(),
                        BRAND,
                        "not completed",
                    );
                    ledger_row(
                        ui,
                        "Model SHA-256",
                        case.workflow.model_sha256.as_deref(),
                        TEXT_DIM,
                        "unknown",
                    );
                    ledger_row(
                        ui,
                        "Load method",
                        Some(engineering::SURFACE_LOAD_METHOD),
                        TEXT_DIM,
                        "unknown",
                    );

                    // Run ledger: every stored run for this case, newest
                    // first; each row deep-links to its immutable run
                    // (N6-COMP-01 pattern).
                    let runs: Vec<(String, u64, project::LifecycleState, bool)> = self
                        .project
                        .manifest()
                        .cases()
                        .iter()
                        .find(|record| record.case_id() == case.workflow.case_id)
                        .map(|record| {
                            record
                                .runs()
                                .iter()
                                .map(|run| {
                                    (
                                        run.run_id().to_owned(),
                                        run.created_utc_unix(),
                                        run.state(),
                                        case.active_run_id.as_deref() == Some(run.run_id()),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut inspect_run: Option<String> = None;
                    if !runs.is_empty() {
                        ui.add_space(20.0);
                        ui.label(overline_text("Run ledger"));
                        ui.add_space(8.0);
                        for (run_id, created, state, active) in runs.iter().rev() {
                            if run_ledger_row(ui, run_id, *created, *state, *active) {
                                inspect_run = Some(run_id.clone());
                            }
                        }
                    }
                    if let Some(result) = &case.workflow.result {
                        ui.add_space(20.0);
                        ui.label(overline_text("Scientific labels"));
                        ui.add_space(8.0);
                        for (quantity, label) in [
                            ("Velocity", "model prediction"),
                            ("Pressure", "recovered from predicted velocity"),
                            ("Cp", "derived using the complete reference state"),
                            ("Surface traction", "pressure + Newtonian viscous fluid load"),
                            ("Structural stress", "not computed"),
                        ] {
                            diag(ui, quantity, label, TEXT_DIM);
                        }
                        if !result.warnings.is_empty() {
                            ui.add_space(8.0);
                            for warning in &result.warnings {
                                alert_line(ui, WARN, "!", &format!("Notice · {warning}"));
                            }
                        }
                    }
                    if let Some(run_id) = inspect_run {
                        self.select_external_run(&run_id);
                    }
                    ui.add_space(28.0);
                });
        });
    }

    /// Reconstruct a neutral field export strictly from the selected persisted
    /// run and its content-addressed field blob. The in-memory result view and
    /// horizon-preview cache are deliberately not export inputs.
    fn persisted_vtk_export(&self) -> Result<(String, vtk_export::VtkFieldExport), String> {
        let run_id = self
            .project
            .manifest()
            .selection()
            .selected_run_id
            .as_deref()
            .ok_or_else(|| "Select a completed persisted engineering run first.".to_string())?;
        if self
            .cad
            .as_ref()
            .and_then(|case| case.active_run_id.as_deref())
            != Some(run_id)
        {
            return Err(
                "The visible field is not the selected persisted run; reopen that run before exporting evidence."
                    .into(),
            );
        }
        let (case_record, run) = self
            .project
            .manifest()
            .cases()
            .iter()
            .find_map(|case| {
                case.runs()
                    .iter()
                    .find(|run| run.run_id() == run_id)
                    .map(|run| (case, run))
            })
            .ok_or_else(|| {
                "The selected run is absent from the persisted run ledger.".to_string()
            })?;
        if !matches!(
            run.state(),
            project::LifecycleState::Succeeded | project::LifecycleState::EvidenceLocked
        ) {
            return Err(
                "Neutral field evidence requires a completed persisted run; draft, running, stale, and failed runs are rejected."
                    .into(),
            );
        }
        let contract = &run.manifest().exact_contract;
        let contract_kind = contract
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "The selected run has no persisted contract kind.".to_string())?;
        if contract_kind != engineering::EXTERNAL_FLOW_CONTRACT {
            return Err("Only completed external-flow fields can be exported to VTK.".into());
        }
        let source_revision_id = contract
            .get("source_revision_id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "The selected run has no persisted source revision.".to_string())?;
        let contract_case_revision = contract
            .get("case_revision_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "The selected run has no contract case revision.".to_string())?;
        if contract_case_revision != run.case_revision_id() {
            return Err(
                "The selected run's contract and immutable ledger disagree on case revision."
                    .into(),
            );
        }
        let source_revision = self
            .project
            .manifest()
            .source_revisions()
            .iter()
            .find(|source| source.source_revision_id == source_revision_id)
            .ok_or_else(|| {
                "The selected run's source revision is absent from the project manifest."
                    .to_string()
            })?;
        let preflight: engineering::GeometryPreflight =
            serde_json::from_value(contract.get("preflight").cloned().ok_or_else(|| {
                "The selected run has no persisted geometry preflight.".to_string()
            })?)
            .map_err(|error| {
                format!("The selected run's geometry preflight is malformed: {error}")
            })?;
        if !preflight.transform_approved {
            return Err("The selected run does not carry an approved source transform.".into());
        }
        if !preflight
            .source_sha256
            .eq_ignore_ascii_case(&source_revision.content_sha256)
        {
            return Err(
                "The selected run's preflight and source revision disagree on source SHA-256."
                    .into(),
            );
        }
        let operating: engineering::OperatingPoint = serde_json::from_value(
            contract
                .get("operating_point")
                .cloned()
                .ok_or_else(|| "The selected run has no persisted operating point.".to_string())?,
        )
        .map_err(|error| format!("The selected run's operating point is malformed: {error}"))?;
        let meters_per_source_unit = operating.length_unit.meters_per_unit().ok_or_else(|| {
            "The selected run has no canonical source-unit conversion.".to_string()
        })?;

        let model_sha256 = run
            .manifest()
            .model
            .as_ref()
            .and_then(|model| model.sha256.as_deref())
            .filter(|digest| {
                digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .ok_or_else(|| {
                "The completed run has no canonical model SHA-256; it cannot become exported evidence."
                    .to_string()
            })?;
        let contract_model_sha256 = contract
            .pointer("/model/sha256")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "The selected run contract has no model SHA-256.".to_string())?;
        if !model_sha256.eq_ignore_ascii_case(contract_model_sha256) {
            return Err(
                "The selected run's contract and manifest disagree on model SHA-256.".into(),
            );
        }

        let result_artifact = self
            .project
            .manifest()
            .evidence_for_run(run_id)
            .into_iter()
            .rev()
            .find(|artifact| {
                artifact
                    .metadata
                    .get("schema")
                    .and_then(serde_json::Value::as_str)
                    == Some(engineering::ENGINEERING_RESULT_SCHEMA)
            })
            .ok_or_else(|| {
                "The completed run has no persisted engineering-result evidence.".to_string()
            })?;
        let summary = self.stored_artifact_snapshot(result_artifact)?;
        if summary
            .get("field_schema")
            .and_then(serde_json::Value::as_str)
            != Some(engineering::ENGINEERING_FIELD_SCHEMA)
        {
            return Err("The completed run's field schema is unsupported for VTK export.".into());
        }
        if summary.get("run_id").and_then(serde_json::Value::as_str) != Some(run_id) {
            return Err(
                "The engineering-result evidence does not identify the selected run.".into(),
            );
        }
        let field_sha256 = summary
            .get("field_sha256")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                "The completed run does not identify a persisted field artifact.".to_string()
            })?;
        if !run
            .manifest()
            .output_sha256
            .iter()
            .any(|digest| digest.eq_ignore_ascii_case(field_sha256))
        {
            return Err(
                "The field artifact is not recorded among the immutable run outputs.".into(),
            );
        }
        if self.project.content_state(field_sha256) != project::ContentState::Available {
            return Err(
                "The selected run's field artifact is missing or corrupt; the manifest remains reviewable but no field evidence can be exported."
                    .into(),
            );
        }
        let field_bytes = self
            .project
            .content_bytes(field_sha256)
            .ok_or_else(|| "The verified field artifact became unavailable.".to_string())?;
        let actual_field_sha256 = format!("{:x}", Sha256::digest(field_bytes));
        if !actual_field_sha256.eq_ignore_ascii_case(field_sha256) {
            return Err("The selected run's field artifact failed SHA-256 verification.".into());
        }
        let field = engineering::decode_engineering_field(field_bytes)?;
        if field.n != preflight.target_grid {
            return Err(format!(
                "The persisted field grid {} does not match the approved {}³ discretization.",
                field.n, preflight.target_grid
            ));
        }
        let traction_method = summary
            .get("method")
            .and_then(serde_json::Value::as_str)
            .filter(|method| !method.trim().is_empty())
            .ok_or_else(|| "The completed run has no recorded traction method.".to_string())?;
        let export = vtk_export::VtkFieldExport {
            field,
            source_to_solver_transform_4x4: preflight.transform_4x4,
            meters_per_source_unit,
            transform_approved: preflight.transform_approved,
            run_state: run.state(),
            provenance: vtk_export::VtkFieldProvenance {
                source_revision_id: source_revision_id.to_owned(),
                case_revision_id: run.case_revision_id().to_owned(),
                run_id: run_id.to_owned(),
                model_sha256: model_sha256.to_owned(),
                contract_kind: contract_kind.to_owned(),
                field_sha256: field_sha256.to_owned(),
                traction_method: traction_method.to_owned(),
            },
        };
        let file_name = vtk_export::default_file_name(case_record.name(), run_id, model_sha256);
        Ok((file_name, export))
    }

    fn export_vtk_field(&mut self) {
        let (file_name, export) = match self.persisted_vtk_export() {
            Ok(export) => export,
            Err(error) => {
                self.project_notice = Some((error, true));
                return;
            }
        };
        let points = export.field.n.saturating_pow(3);
        let run_id = export.provenance.run_id.clone();
        let Some(path) = self
            .export_dialog(&file_name)
            .add_filter("Legacy VTK StructuredGrid", &["vtk"])
            .save_file()
        else {
            return;
        };
        self.project_notice = Some(match vtk_export::write_atomic(&path, &export) {
            Ok(()) => (
                format!(
                    "Exported {points} source-frame field points from immutable run {} to {}.",
                    short_id(&run_id),
                    path.display()
                ),
                false,
            ),
            Err(error) => (format!("VTK field evidence was not written: {error}"), true),
        });
    }

    fn export_fea_loads(&mut self) {
        let Some(case) = self.cad.as_ref() else {
            self.project_notice = Some(("No case result is available to export.".into(), true));
            return;
        };
        let Some(run_id) = case.active_run_id.as_deref() else {
            self.project_notice = Some((
                "FEA export requires a completed immutable run.".into(),
                true,
            ));
            return;
        };
        let Some((case_record, run)) = self.project.manifest().cases().iter().find_map(|record| {
            record
                .runs()
                .iter()
                .find(|run| run.run_id() == run_id)
                .map(|run| (record, run))
        }) else {
            self.project_notice = Some((
                "The active FEA result is not present in the project run ledger.".into(),
                true,
            ));
            return;
        };
        if run.state() != project::LifecycleState::Succeeded {
            self.project_notice = Some((
                "Only a succeeded immutable run can provide FEA loads.".into(),
                true,
            ));
            return;
        }
        let contract = &run.manifest().exact_contract;
        let source_revision_id = contract
            .get("source_revision_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let Some(source) = self
            .project
            .manifest()
            .source_revisions()
            .iter()
            .find(|source| source.source_revision_id == source_revision_id)
        else {
            self.project_notice = Some((
                "The run's immutable source revision is unavailable.".into(),
                true,
            ));
            return;
        };
        let operating: engineering::OperatingPoint = match contract
            .get("operating_point")
            .cloned()
            .ok_or_else(|| "Run contract omits the operating point.".to_owned())
            .and_then(|value| {
                serde_json::from_value(value)
                    .map_err(|error| format!("Run operating point is invalid: {error}"))
            }) {
            Ok(operating) => operating,
            Err(error) => {
                self.project_notice = Some((error, true));
                return;
            }
        };
        let preflight: engineering::GeometryPreflight = match contract
            .get("preflight")
            .cloned()
            .ok_or_else(|| "Run contract omits geometry preflight.".to_owned())
            .and_then(|value| {
                serde_json::from_value(value)
                    .map_err(|error| format!("Run geometry preflight is invalid: {error}"))
            }) {
            Ok(preflight) => preflight,
            Err(error) => {
                self.project_notice = Some((error, true));
                return;
            }
        };
        let Some(result) = case.workflow.result.as_ref() else {
            self.project_notice = Some((
                "The active run has no complete engineering load result.".into(),
                true,
            ));
            return;
        };
        let n = case.result_grid;
        let cube = n.saturating_mul(n).saturating_mul(n);
        if n < 3
            || case.mask.len() != cube
            || case.cp.len() != cube
            || case.traction.len() != 3 * cube
        {
            self.project_notice = Some((
                "The active result has no complete mapped surface-load field.".into(),
                true,
            ));
            return;
        }
        let Some(scale) = operating.length_unit.meters_per_unit() else {
            self.project_notice = Some((
                "The run contract has no approved source-unit conversion.".into(),
                true,
            ));
            return;
        };
        let reference_length_m = operating.reference_length * scale;
        let physical_area_scale =
            (reference_length_m / preflight.solver_characteristic_length).powi(2);
        let physical_arm_scale = reference_length_m / preflight.solver_characteristic_length;
        let dx_solver = std::f64::consts::TAU / n as f64;
        let index = |i: usize, j: usize, k: usize| i * n * n + j * n + k;
        let mut positions = Vec::new();
        let mut tractions = Vec::new();
        let mut solver_positions = Vec::new();
        let mut solver_tractions = Vec::new();
        let mut area_weights_m2 = Vec::new();
        let mut coefficients = Vec::new();
        for i in 1..n - 1 {
            for j in 1..n - 1 {
                for k in 1..n - 1 {
                    let gradient = [
                        (case.mask[index(i + 1, j, k)] - case.mask[index(i - 1, j, k)]) as f64,
                        (case.mask[index(i, j + 1, k)] - case.mask[index(i, j - 1, k)]) as f64,
                        (case.mask[index(i, j, k + 1)] - case.mask[index(i, j, k - 1)]) as f64,
                    ];
                    let magnitude = gradient
                        .iter()
                        .map(|value| value * value)
                        .sum::<f64>()
                        .sqrt();
                    if magnitude <= 0.02 {
                        continue;
                    }
                    let cell = index(i, j, k);
                    let solver_point = [
                        (i as f64 + 0.5) * dx_solver,
                        (j as f64 + 0.5) * dx_solver,
                        (k as f64 + 0.5) * dx_solver,
                    ];
                    let position = match engineering::solver_point_to_source_m(
                        solver_point,
                        preflight.transform_4x4,
                        scale,
                    ) {
                        Ok(position) => position,
                        Err(error) => {
                            self.project_notice =
                                Some((format!("FEA coordinate mapping failed: {error}"), true));
                            return;
                        }
                    };
                    let solver_traction = [
                        case.traction[cell] as f64,
                        case.traction[cube + cell] as f64,
                        case.traction[2 * cube + cell] as f64,
                    ];
                    let source_traction = match engineering::solver_vector_to_source(
                        solver_traction,
                        preflight.transform_4x4,
                    ) {
                        Ok(traction) => traction,
                        Err(error) => {
                            self.project_notice =
                                Some((format!("FEA traction mapping failed: {error}"), true));
                            return;
                        }
                    };
                    positions.push(position);
                    tractions.push(source_traction);
                    solver_positions.push(solver_point);
                    solver_tractions.push(solver_traction);
                    area_weights_m2
                        .push(magnitude * dx_solver * dx_solver * 0.5 * physical_area_scale);
                    coefficients.push(case.cp[cell] as f64);
                }
            }
        }
        let exported_area_m2 = area_weights_m2.iter().sum::<f64>();
        let mut exported_force = [0.0; 3];
        let mut center = [0.0; 3];
        for ((position, traction), area) in solver_positions
            .iter()
            .zip(&solver_tractions)
            .zip(&area_weights_m2)
        {
            for axis in 0..3 {
                exported_force[axis] += traction[axis] * area;
                center[axis] += position[axis] * area;
            }
        }
        if exported_area_m2 > 0.0 {
            center
                .iter_mut()
                .for_each(|value| *value /= exported_area_m2);
        }
        let mut exported_moment = [0.0; 3];
        for ((position, traction), area) in solver_positions
            .iter()
            .zip(&solver_tractions)
            .zip(&area_weights_m2)
        {
            let arm = [
                (position[0] - center[0]) * physical_arm_scale,
                (position[1] - center[1]) * physical_arm_scale,
                (position[2] - center[2]) * physical_arm_scale,
            ];
            let moment_density = [
                arm[1] * traction[2] - arm[2] * traction[1],
                arm[2] * traction[0] - arm[0] * traction[2],
                arm[0] * traction[1] - arm[1] * traction[0],
            ];
            for axis in 0..3 {
                exported_moment[axis] += moment_density[axis] * area;
            }
        }
        let force_residual =
            std::array::from_fn(|axis| exported_force[axis] - result.force_newtons[axis]);
        let moment_residual =
            std::array::from_fn(|axis| exported_moment[axis] - result.moment_newton_meters[axis]);
        let model = contract.get("model").unwrap_or(&serde_json::Value::Null);
        let solver = run
            .manifest()
            .solver
            .as_ref()
            .or(run.manifest().engine.as_ref());
        let dynamic_pressure = operating.dynamic_pressure().unwrap_or(f64::NAN);
        let provenance = engineering::FeaLoadProvenance {
            project_id: self.project.manifest().project_id().to_owned(),
            case_id: case.workflow.case_id.clone(),
            case_name: case_record.name().to_owned(),
            source_revision_id: source_revision_id.to_owned(),
            source_name: source
                .uri_hint
                .clone()
                .unwrap_or_else(|| case.workflow.source_name.clone()),
            source_sha256: source.content_sha256.clone(),
            case_revision_id: run.case_revision_id().to_owned(),
            run_id: run_id.to_owned(),
            model_id: model
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            model_sha256: model
                .get("sha256")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            solver_name: solver
                .map(|component| component.name.clone())
                .unwrap_or_default(),
            solver_version: solver
                .map(|component| component.version.clone())
                .unwrap_or_default(),
            contract_kind: engineering::EXTERNAL_FLOW_CONTRACT.into(),
            coordinate_frame:
                "imported source axes; coordinates inverse-mapped from solver space into SI meters"
                    .into(),
            traction_frame:
                "imported source axes; solver/wind traction rotated without scaling; Pa".into(),
            body_frame_semantics:
                "body axes are the imported source axes before case orientation is applied".into(),
            wind_frame_semantics: engineering::COEFFICIENT_REFERENCE_FRAME.into(),
            source_to_solver_transform_4x4_column_major: preflight.transform_4x4,
            source_length_unit: operating.length_unit.symbol().into(),
            meters_per_source_unit: scale,
            position_units: "m".into(),
            traction_units: "Pa".into(),
            cp_units: "1".into(),
            reference_length_m,
            free_stream_velocity_mps: operating.velocity,
            density_kg_m3: operating.density,
            dynamic_viscosity_pa_s: operating.viscosity,
            reference_pressure_pa: operating.reference_pressure,
            dynamic_pressure_pa: dynamic_pressure,
            declared_flow_direction: operating.flow_direction,
            integration_method: format!(
                "{}; central-difference |grad(mask)| diffuse-area quadrature; exported rows use |raw gradient| > 0.02",
                engineering::SURFACE_LOAD_METHOD
            ),
            resultant_force_newtons_wind_axes: result.force_newtons,
            resultant_moment_newton_meters_wind_axes: result.moment_newton_meters,
            exported_sample_force_newtons_wind_axes: exported_force,
            exported_sample_moment_newton_meters_wind_axes: exported_moment,
            force_reconciliation_residual_newtons: force_residual,
            moment_reconciliation_residual_newton_meters: moment_residual,
            moment_reference: if result.moment_origin_mode.is_empty() {
                operating.moment_origin_mode.label().into()
            } else {
                result.moment_origin_mode.clone()
            },
            moment_origin_mode: if result.moment_origin_mode.is_empty() {
                match operating.moment_origin_mode {
                    engineering::MomentOriginMode::DiffuseSurfaceCentroid => {
                        "diffuse_surface_centroid".into()
                    }
                    engineering::MomentOriginMode::SourceFramePoint => {
                        "source_frame_point".into()
                    }
                }
            } else {
                result.moment_origin_mode.clone()
            },
            moment_origin_source_m: operating
                .moment_origin_source_m()
                .unwrap_or([0.0, 0.0, 0.0]),
            moment_origin_solver: result.moment_origin_solver,
            reference_area_m2: result
                .reference_area_m2
                .or_else(|| operating.aero_reference_area_m2()),
            coefficient_reference: if result.coefficient_reference.is_empty() {
                "q_inf * L_ref^2 ; q_inf * L_ref^3".into()
            } else {
                result.coefficient_reference.clone()
            },
            integrated_surface_area_m2: result.surface_area_m2,
            pressure_force_fraction: result.pressure_force_fraction,
            reconciliation_method:
                "exported thresholded sample quadrature minus full-field reported resultant".into(),
            reconciliation_status: if force_residual
                .iter()
                .chain(moment_residual.iter())
                .all(|value| value.abs() <= 1e-6)
            {
                "within_absolute_tolerance_1e-6".into()
            } else {
                "thresholded_export_differs_from_full_field; use reported residual metadata".into()
            },
        };
        let csv = match engineering::fea_load_csv(
            &positions,
            &tractions,
            &area_weights_m2,
            &coefficients,
            &provenance,
        ) {
            Ok(csv) => csv,
            Err(error) => {
                self.project_notice = Some((error, true));
                return;
            }
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV", &["csv"])
            .set_file_name(format!(
                "{}_surface_loads.csv",
                case.workflow.name.replace(' ', "_")
            ))
            .save_file()
        else {
            return;
        };
        match std::fs::write(&path, csv) {
            Ok(()) => {
                self.project_notice = Some((
                    format!(
                        "Exported {} mapped fluid-load points for downstream FEA to {}.",
                        positions.len(),
                        path.display()
                    ),
                    false,
                ));
            }
            Err(error) => {
                self.project_notice = Some((format!("FEA load export failed: {error}"), true));
            }
        }
    }

    /// A save dialog seeded with the Settings › Workflow default export
    /// directory (when set and present on disk).
    fn export_dialog(&self, file_name: &str) -> rfd::FileDialog {
        let mut dialog = rfd::FileDialog::new().set_file_name(file_name);
        let directory = self.settings.default_export_directory.trim();
        if !directory.is_empty() && std::path::Path::new(directory).is_dir() {
            dialog = dialog.set_directory(directory);
        }
        dialog
    }

    /// Export the currently displayed engineering section (stored field data,
    /// same renderer as the on-screen view) as a PNG.
    fn export_section_png(&mut self) {
        let Some(section) = self.section_data.as_ref() else {
            self.project_notice = Some((
                "No section is currently rendered — open a 2D section in Results first.".into(),
                true,
            ));
            return;
        };
        let image = engineering_section_image(section);
        let bytes = match color_image_png_bytes(&image, 512) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.project_notice = Some((format!("Section PNG failed: {error}"), true));
                return;
            }
        };
        let case_name = self
            .cad
            .as_ref()
            .map(|case| case.workflow.name.replace(' ', "_"))
            .unwrap_or_else(|| "case".into());
        let file_name = format!(
            "{case_name}_{}section_{}.png",
            section.axis.label(),
            section.quantity.label().to_lowercase().replace(' ', "_")
        );
        let Some(path) = self
            .export_dialog(&file_name)
            .add_filter("PNG", &["png"])
            .save_file()
        else {
            return;
        };
        self.project_notice = Some(match std::fs::write(&path, bytes) {
            Ok(()) => (format!("Section exported to {}.", path.display()), false),
            Err(error) => (format!("Section PNG was not written: {error}"), true),
        });
    }

    /// Dev/QA hook (REYN_STUDIO_IMPORT=path.stl|path.stp): run the ordinary import after
    /// a brief inventory wait. If no qualified model appears, the same
    /// model-independent diagnostic preflight used by a hand-import is shown.
    /// Failures surface as the normal import notice.
    fn handle_qa_import(&mut self) {
        const INVENTORY_WAIT_FRAMES: u32 = 30;
        if self.qa_import_path.is_none() {
            return;
        }
        if self.models.is_empty() {
            self.qa_import_waited = self.qa_import_waited.saturating_add(1);
            if self.qa_import_waited < INVENTORY_WAIT_FRAMES {
                return;
            }
        }
        let Some(path) = self.qa_import_path.take() else {
            return;
        };
        eprintln!("REYN_STUDIO_IMPORT importing: {}", path.display());
        self.import_cad_path(path);
    }

    /// Dev/QA hook (REYN_STUDIO_APPROVE_TRANSFORM=1): once geometry is on the
    /// case and no import is in flight, approve the units/transform gate the
    /// same way the Setup Gate checkbox does.
    fn handle_qa_approve_transform(&mut self) {
        if !self.qa_approve_transform {
            return;
        }
        if self.qa_import_path.is_some() || self.geometry_import_pending.is_some() {
            return;
        }
        {
            let Some(case) = self.cad.as_mut() else {
                return;
            };
            if case.workflow.preflight.transform_approved {
                self.qa_approve_transform = false;
                return;
            }
            case.workflow.preflight.transform_approved = true;
            let _ = case
                .workflow
                .operating
                .retarget_velocity_to_qualified_reynolds();
        }
        self.qa_approve_transform = false;
        self.case_draft_dirty = true;
        self.refresh_geometry_setup_status();
        eprintln!("REYN_STUDIO_APPROVE_TRANSFORM applied");
    }

    fn refresh_geometry_setup_status(&mut self) {
        let Some(case) = self.cad.as_ref() else {
            return;
        };
        self.engine_status = geometry_setup_engine_status(&case.name, &case.workflow.preflight);
    }

    /// Dev/QA hook (REYN_STUDIO_RUN_ANALYSIS=1): start the ordinary run once the
    /// case is ready with a qualified installed model.
    fn handle_qa_run_analysis(&mut self) {
        if !self.qa_run_analysis {
            return;
        }
        if self.qa_import_path.is_some()
            || self.geometry_import_pending.is_some()
            || self.qa_approve_transform
        {
            return;
        }
        if self.cad.is_none() {
            return;
        }
        if self
            .cad
            .as_ref()
            .is_some_and(|case| case.pending || case.workflow.result.is_some())
        {
            self.qa_run_analysis = false;
            return;
        }
        if !matches!(self.model_inventory_state, ModelInventoryState::Ready) {
            return;
        }
        let grid = self
            .cad
            .as_ref()
            .map(|case| case.workflow.preflight.target_grid)
            .unwrap_or(0);
        let needs_bind = self.cad.as_ref().is_some_and(|case| {
            case.workflow.model_id.trim().is_empty()
                || case.workflow.model_support.status == "unavailable"
                || !self.models.iter().any(|model| {
                    model.id == case.workflow.model_id
                        && engine::is_qualified_external_flow_model(model, grid)
                })
        });
        if needs_bind {
            if let Some(model) = self
                .models
                .iter()
                .filter(|model| engine::is_qualified_external_flow_model(model, grid))
                .find(|model| model.id == self.settings.default_3d_model)
                .or_else(|| {
                    self.models
                        .iter()
                        .filter(|model| engine::is_qualified_external_flow_model(model, grid))
                        .max_by_key(|model| model.grid)
                })
                .cloned()
            {
                if let Some(case) = self.cad.as_mut() {
                    bind_external_flow_model(case, &model);
                    eprintln!("REYN_STUDIO_RUN_ANALYSIS bound model {}", model.id);
                }
            }
        }
        let Some(case) = self.cad.as_ref() else {
            return;
        };
        let grid = case.workflow.preflight.target_grid;
        let model_id = case.workflow.model_id.clone();
        let local_model_ready = self.models.iter().any(|model| {
            model.id == model_id && engine::is_qualified_external_flow_model(model, grid)
        });
        if !case.workflow.ready() || !local_model_ready {
            return;
        }
        self.qa_run_analysis = false;
        eprintln!("REYN_STUDIO_RUN_ANALYSIS starting");
        self.run_external_flow();
    }

    /// Dev/QA hook (REYN_STUDIO_SHOT=path): once the UI has had a few frames
    /// to settle, request a composited screenshot and write the full window
    /// to the given path. Complements REYN_STUDIO_WINDOW/START_NAV and avoids
    /// depending on OS screen-recording permission during visual audits.
    fn handle_qa_shot(&mut self, ctx: &egui::Context) {
        const SETTLE_FRAMES: u32 = 45;
        let Some(path) = self.qa_shot_path.clone() else {
            return;
        };
        ctx.request_repaint();
        // A queued QA import has to land first, or the capture would show the
        // pre-import screen. The settle window starts after it.
        if self.qa_import_path.is_some() {
            return;
        }
        // STEP/mesh import is async; wait for the worker so the shot shows the
        // Case Setup viewport with geometry, not the "Importing…" overlay.
        if self.geometry_import_pending.is_some() {
            return;
        }
        // Optional transform approval must land before the shot, otherwise the
        // capture still shows the blocked Setup Gate.
        if self.qa_approve_transform {
            return;
        }
        // Optional analysis run must finish before a Results shot.
        if self.qa_run_analysis {
            return;
        }
        if let Some(case) = self.cad.as_ref() {
            if case.pending {
                return;
            }
            if std::env::var("REYN_STUDIO_START_NAV").as_deref() == Ok("results")
                && case.workflow.result.is_none()
            {
                return;
            }
            if case.workflow.result.is_some() && self.nav != Nav::Results {
                self.nav = Nav::Results;
                return;
            }
        }
        // Wait for Fit / station glides so the shot frames the body, not the
        // default domain orbit that makes a 64³ brick look like a speck.
        if self.cam.is_animating() || self.view_fit {
            return;
        }
        self.qa_shot_frames = self.qa_shot_frames.saturating_add(1);
        if self.qa_shot_frames < SETTLE_FRAMES {
            return;
        }
        // Re-request every 30 frames: a request sent before the wgpu surface
        // is fully ready can be dropped without an event.
        if self.qa_shot_frames % 30 == SETTLE_FRAMES % 30 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let image = ctx.input(|input| {
            input.events.iter().rev().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = image else { return };
        self.qa_shot_path = None;
        spawn_screenshot_write(
            self.screenshot_result_tx.clone(),
            ctx.clone(),
            image,
            None,
            path,
            ScreenshotWriteKind::Qa,
            ViewportShotProvenance::default(),
        );
    }

    /// Ask the windowing backend for a composited frame (includes the wgpu 3D
    /// pass) and save the current render-viewport region on arrival.
    fn request_viewport_png(&mut self, ctx: &egui::Context) {
        if self.last_render_rect.is_none() {
            self.project_notice = Some((
                "No render viewport is currently visible to capture.".into(),
                true,
            ));
            return;
        }
        let case_name = self
            .cad
            .as_ref()
            .map(|case| case.workflow.name.replace(' ', "_"))
            .unwrap_or_else(|| "viewport".into());
        let Some(path) = self
            .export_dialog(&format!("{case_name}_viewport.png"))
            .add_filter("PNG", &["png"])
            .save_file()
        else {
            return;
        };
        self.pending_viewport_shot = Some(path);
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
    }

    /// Consume a completed screenshot event and transfer ownership to a worker.
    /// Crop, RGBA conversion, PNG compression, and disk I/O stay off the UI
    /// thread; completion wakes egui through the result channel.
    fn handle_screenshot_events(&mut self, ctx: &egui::Context) {
        if self.pending_viewport_shot.is_none() {
            return;
        }
        let image = ctx.input(|input| {
            input.events.iter().rev().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = image else { return };
        let Some(path) = self.pending_viewport_shot.take() else {
            return;
        };
        let pixels_per_point = ctx.pixels_per_point();
        let provenance = self.viewport_shot_provenance();
        spawn_screenshot_write(
            self.screenshot_result_tx.clone(),
            ctx.clone(),
            image,
            self.last_render_rect.map(|rect| (rect, pixels_per_point)),
            path,
            ScreenshotWriteKind::Viewport,
            provenance,
        );
    }

    fn viewport_shot_provenance(&self) -> ViewportShotProvenance {
        let mut footer_lines = Vec::new();
        footer_lines.push(format!(
            "Reyn Studio {} · viewport evidence",
            env!("CARGO_PKG_VERSION")
        ));
        if let Some(case) = self.cad.as_ref() {
            if let Some(run_id) = case.active_run_id.as_deref() {
                footer_lines.push(format!(
                    "RUN {} · MODEL {}",
                    short_id(run_id),
                    case.workflow
                        .model_sha256
                        .as_deref()
                        .map(short_hash)
                        .unwrap_or_else(|| "UNKNOWN".into())
                ));
            }
            footer_lines.push(format!(
                "COLORMAP {:?} · CP RANGE {:?} · SOURCE MODEL/RECOVERED",
                self.settings.colormap, self.settings.cp_range_mode
            ));
        } else {
            footer_lines.push("No active engineering run · capture is unlabeled.".into());
        }
        ViewportShotProvenance { footer_lines }
    }

    fn handle_screenshot_write_results(&mut self) {
        while let Ok(completion) = self.screenshot_result_rx.try_recv() {
            match completion.kind {
                ScreenshotWriteKind::Viewport => {
                    self.project_notice = Some(match completion.result {
                        Ok(()) => (
                            format!("Viewport exported to {}.", completion.path.display()),
                            false,
                        ),
                        Err(error) => (format!("Viewport PNG failed: {error}"), true),
                    });
                }
                ScreenshotWriteKind::Qa => match completion.result {
                    Ok(()) => {
                        eprintln!("REYN_STUDIO_SHOT written: {}", completion.path.display())
                    }
                    Err(error) => eprintln!("REYN_STUDIO_SHOT failed: {error}"),
                },
            }
        }
    }

    /// Export the self-contained HTML engineering report for the active
    /// immutable run (provenance chain + limitations always included).
    fn export_engineering_report(&mut self) {
        let Some(case) = self.cad.as_ref() else {
            self.project_notice = Some(("No case is active.".into(), true));
            return;
        };
        let Some(run_id) = case.active_run_id.clone() else {
            self.project_notice = Some((
                "A durable immutable run is required before a report can be produced.".into(),
                true,
            ));
            return;
        };
        let run_created_utc_unix = self
            .project
            .manifest()
            .cases()
            .iter()
            .find(|record| record.case_id() == case.workflow.case_id)
            .and_then(|record| {
                record
                    .runs()
                    .iter()
                    .find(|run| run.run_id() == run_id)
                    .map(|run| run.created_utc_unix())
            })
            .unwrap_or_else(now_utc_unix);
        let section_figure = self.section_data.as_ref().and_then(|section| {
            let image = engineering_section_image(section);
            use base64::Engine as _;
            color_image_png_bytes(&image, 512)
                .ok()
                .map(|bytes| report::SectionFigure {
                    png_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                    caption: format!(
                        "{} section at {:.3} (domain) · {} [{}] — stored run field, same renderer as the interactive view",
                        section.axis.label(),
                        section.location,
                        section.quantity.label(),
                        section.quantity.units()
                    ),
                })
        });
        let input = report::ReportInput {
            case: &case.workflow,
            run_id: &run_id,
            run_created_utc_unix,
            generated_utc_unix: now_utc_unix(),
            app_version: env!("CARGO_PKG_VERSION"),
            unit_system: self.settings.unit_system,
            format: self.settings.value_format(),
            section_figure,
        };
        let html = match report::engineering_report_html(&input) {
            Ok(html) => html,
            Err(error) => {
                self.project_notice = Some((format!("Report was not produced: {error}"), true));
                return;
            }
        };
        let file_name = format!(
            "{}_{}_report",
            case.workflow.name.replace(' ', "_"),
            short_id(&run_id)
        );
        let Some(path) = self
            .export_dialog(&format!("{file_name}.html"))
            .add_filter("HTML", &["html"])
            .add_filter("PNG lab sheet", &["png"])
            .add_filter("PDF lab sheet", &["pdf"])
            .save_file()
        else {
            return;
        };
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("html")
            .to_ascii_lowercase();
        self.project_notice = Some(match extension.as_str() {
            "png" | "pdf" => {
                let format = if extension == "png" {
                    engineering_export::LabSheetFormat::Png
                } else {
                    engineering_export::LabSheetFormat::Pdf
                };
                let artifact = match self.settings.configured_signing_key() {
                    Ok(Some(key)) => engineering_export::engineering_report_labsheet_signed(
                        &input,
                        format,
                        &signing::NativeKeychainProvider,
                        &key,
                        self.settings.signing_key_is_revoked(),
                        now_utc_unix(),
                    ),
                    _ => engineering_export::engineering_report_labsheet(&input, format),
                };
                match artifact {
                    Ok(artifact) => {
                        let sidecar = artifact.signature.as_ref().and_then(|signed| {
                            let sidecar_path = if extension == "png" {
                                path.with_extension("png.sig.json")
                            } else {
                                path.with_extension("pdf.sig.json")
                            };
                            signed.to_json().ok().and_then(|json| {
                                std::fs::write(&sidecar_path, json).ok()?;
                                Some(sidecar_path)
                            })
                        });
                        match std::fs::write(&path, &artifact.bytes) {
                            Ok(()) => (
                                format!(
                                    "Engineering {} report exported to {} · content {}… · html {}….{}",
                                    format.extension().to_ascii_uppercase(),
                                    path.display(),
                                    &artifact.content_sha256[..12.min(artifact.content_sha256.len())],
                                    &artifact.html_sha256[..12.min(artifact.html_sha256.len())],
                                    sidecar
                                        .map(|path| format!(" Signed sidecar: {}.", path.display()))
                                        .unwrap_or_else(|| " Unsigned.".into())
                                ),
                                false,
                            ),
                            Err(error) => (format!("Report was not written: {error}"), true),
                        }
                    }
                    Err(error) => (
                        format!(
                            "{} report was not produced: {error}",
                            extension.to_ascii_uppercase()
                        ),
                        true,
                    ),
                }
            }
            _ => match std::fs::write(&path, html) {
                Ok(()) => (
                    format!("Engineering report exported to {}.", path.display()),
                    false,
                ),
                Err(error) => (format!("Report was not written: {error}"), true),
            },
        });
    }

    /// Route native macOS menu clicks through the same handlers the deleted
    /// in-app menus used, then mirror app state back onto the menu items.
    #[cfg(target_os = "macos")]
    fn handle_menu_signals(&mut self, ctx: &egui::Context) {
        let signals = match self.menubar.as_ref() {
            Some(menubar) => menubar.poll(),
            None => return,
        };
        for signal in signals {
            match signal {
                MenuSignal::Command(command) => match command {
                    MenuCommand::NewProject => self
                        .request_project_action(project_lifecycle::DeferredProjectAction::New, ctx),
                    MenuCommand::OpenProject => self.open_project_dialog(ctx),
                    MenuCommand::SaveProject => {
                        self.save_project_dialog();
                    }
                    MenuCommand::SaveProjectAs => {
                        self.save_project_as_dialog();
                    }
                    MenuCommand::ImportModel => self.import_model(),
                    MenuCommand::ImportCad => self.import_cad(),
                    MenuCommand::ExportCalculations => self.export(),
                    MenuCommand::Quit => self.request_project_action(
                        project_lifecycle::DeferredProjectAction::Quit,
                        ctx,
                    ),
                    MenuCommand::UndoCaseEdit => {
                        self.apply_case_history_action(CaseHistoryAction::Undo)
                    }
                    MenuCommand::RedoCaseEdit => {
                        self.apply_case_history_action(CaseHistoryAction::Redo)
                    }
                    MenuCommand::ResetControls => self.reset_controls(),
                    MenuCommand::ResetCamera => self.cam = viewport::Camera::default(),
                    MenuCommand::ToggleDimension => self.volumetric = !self.volumetric,
                    MenuCommand::RunAnalysis => {
                        if matches!(self.nav, Nav::Case | Nav::Results) {
                            self.run_external_flow();
                        }
                    }
                    MenuCommand::ExportFea => {
                        if matches!(self.nav, Nav::Case | Nav::Results) {
                            self.export_fea_loads();
                        }
                    }
                    MenuCommand::RegenerateSandbox => {
                        if self.settings.developer_research_sandbox {
                            self.regenerate();
                        }
                    }
                    MenuCommand::ToggleSandboxLive => {
                        if self.settings.developer_research_sandbox {
                            self.live = !self.live;
                        }
                    }
                    MenuCommand::OpenDocs => {
                        self.project_notice = Some(
                            match resolve_docs_path().and_then(|path| {
                                open_path(&path)?;
                                Ok(path)
                            }) {
                                Ok(path) => (
                                    format!("Opened local documentation from {}.", path.display()),
                                    false,
                                ),
                                Err(error) => {
                                    (format!("Documentation could not be opened: {error}"), true)
                                }
                            },
                        );
                    }
                    MenuCommand::CheckForUpdates => {
                        if let Some(updater) = &self.updater {
                            updater.check();
                            self.nav = Nav::Settings;
                            self.settings_ui.category = settings::SettingsCategory::Updates;
                        }
                    }
                    MenuCommand::SignOut => {
                        self.sign_out_requested = true;
                    }
                },
                MenuSignal::OpenRecent(path) => self.request_project_action(
                    project_lifecycle::DeferredProjectAction::Open(path),
                    ctx,
                ),
            }
        }
        let sync = MenuSyncState {
            can_save: self.has_unsaved_project_work() || self.project.path().is_none(),
            can_undo_case_edit: self
                .case_history_gate_reason(CaseHistoryAction::Undo)
                .is_none(),
            can_redo_case_edit: self
                .case_history_gate_reason(CaseHistoryAction::Redo)
                .is_none(),
            analysis_available: matches!(self.nav, Nav::Case | Nav::Results),
            fea_export_available: self.cad.as_ref().is_some_and(has_complete_fea_load_field),
            sandbox_enabled: self.settings.developer_research_sandbox,
            sandbox_live: self.live,
            recents: self
                .project
                .recent_projects()
                .iter()
                .map(|recent| (recent.name.clone(), recent.path.clone()))
                .collect(),
        };
        if let Some(menubar) = self.menubar.as_mut() {
            menubar.sync(sync);
        }
    }

    /// Why a new run cannot start right now (`None` = it can). One source of
    /// truth for the top-bar Run button, the command palette, and anything
    /// else that gates on runnability (UX-AC-01: reasons, not dead controls).
    fn run_gate_reason(&self) -> Option<String> {
        if let Some(reason) = project_mutation_rejection(
            self.project.availability().access_mode,
            "Starting a new run",
        ) {
            return Some(reason);
        }
        if self.cad.as_ref().is_some_and(|case| case.pending) {
            let queued = self.run_queue.waiting.len();
            if queued > 0 {
                return Some(format!(
                    "An immutable run is in flight · {queued} follow-on attempt(s) already queued."
                ));
            }
            return Some(
                "An immutable run attempt is already in flight. Start again after it finishes to queue a follow-on."
                    .to_owned(),
            );
        }
        if let Some(reason) = orientation_geometry_gate(
            "Starting a new run",
            self.orientation_draft,
            self.orientation_pending.as_ref(),
        ) {
            return Some(reason);
        }
        if !self.engine_ok {
            return Some(
                "Engine unavailable — stored evidence stays readable, but a new run cannot start."
                    .to_owned(),
            );
        }
        match self.cad.as_ref() {
            None => Some("No case yet — import geometry in Case Setup first.".to_owned()),
            Some(case) => {
                let issues = case.workflow.readiness_issues();
                if issues.is_empty() {
                    None
                } else {
                    Some(format!("Run blocked: {}", issues.join(" ")))
                }
            }
        }
    }

    /// ⌘K command palette (§6 Tier 3.5): navigation + actions with the same
    /// state-gating as the nav rail. A gated entry never executes — it shows
    /// its reason instead. Actions only; no fake content.
    fn command_palette(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::K)) {
            self.palette_open = !self.palette_open;
            self.palette_query.clear();
            self.palette_selected = 0;
        }
        if !self.palette_open {
            return;
        }
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.palette_open = false;
            return;
        }

        // Build the gated entry list from live state.
        enum PaletteAction {
            Nav(Nav),
            UndoCaseEdit,
            RedoCaseEdit,
            Run,
            ImportGeometry,
            OpenProject,
            SaveProject,
            OpenRecent(std::path::PathBuf),
        }
        let has_result = self
            .cad
            .as_ref()
            .and_then(|case| case.workflow.result.as_ref())
            .is_some();
        let mut entries: Vec<(String, PaletteAction, Option<String>)> = vec![
            (
                "Go to Project".into(),
                PaletteAction::Nav(Nav::Projects),
                None,
            ),
            (
                "Go to Case Setup".into(),
                PaletteAction::Nav(Nav::Case),
                None,
            ),
            (
                "Go to Results".into(),
                PaletteAction::Nav(Nav::Results),
                (!has_result).then(|| "No completed run yet — run the case first.".to_owned()),
            ),
            (
                "Go to Evidence".into(),
                PaletteAction::Nav(Nav::Evidence),
                None,
            ),
            (
                "Go to Model Library".into(),
                PaletteAction::Nav(Nav::Models),
                None,
            ),
            (
                "Go to Settings".into(),
                PaletteAction::Nav(Nav::Settings),
                None,
            ),
            (
                "Run analysis".into(),
                PaletteAction::Run,
                self.run_gate_reason(),
            ),
            (
                "Undo last safe case-draft edit".into(),
                PaletteAction::UndoCaseEdit,
                self.case_history_gate_reason(CaseHistoryAction::Undo),
            ),
            (
                "Redo last safe case-draft edit".into(),
                PaletteAction::RedoCaseEdit,
                self.case_history_gate_reason(CaseHistoryAction::Redo),
            ),
            (
                "Import geometry (STL / STEP)…".into(),
                PaletteAction::ImportGeometry,
                None,
            ),
            ("Open project…".into(), PaletteAction::OpenProject, None),
            ("Save project".into(), PaletteAction::SaveProject, None),
        ];
        if self.settings.developer_research_sandbox {
            entries.push((
                "Go to Procedural 3D (sandbox)".into(),
                PaletteAction::Nav(Nav::Metrics),
                None,
            ));
            entries.push((
                "Go to Flow Painter (sandbox)".into(),
                PaletteAction::Nav(Nav::FlowPainter),
                None,
            ));
            entries.push((
                "Go to Fields 2D (sandbox)".into(),
                PaletteAction::Nav(Nav::Fields2D),
                None,
            ));
            entries.push((
                "Go to Benchmark Lab (sandbox)".into(),
                PaletteAction::Nav(Nav::Benchmark),
                None,
            ));
        }
        for recent in self.project.recent_projects() {
            entries.push((
                format!("Open recent: {}", recent.name),
                PaletteAction::OpenRecent(recent.path.clone()),
                (!recent.path.is_file())
                    .then(|| "The file no longer exists at this path.".to_owned()),
            ));
        }
        let query = self.palette_query.to_lowercase();
        let filtered: Vec<(String, PaletteAction, Option<String>)> = entries
            .into_iter()
            .filter(|(title, _, _)| query.is_empty() || title.to_lowercase().contains(&query))
            .collect();
        if self.palette_selected >= filtered.len() {
            self.palette_selected = filtered.len().saturating_sub(1);
        }
        // Keyboard: arrows move, Enter fires (only when the entry is free).
        if ctx.input(|input| input.key_pressed(egui::Key::ArrowDown)) {
            self.palette_selected =
                (self.palette_selected + 1).min(filtered.len().saturating_sub(1));
        }
        if ctx.input(|input| input.key_pressed(egui::Key::ArrowUp)) {
            self.palette_selected = self.palette_selected.saturating_sub(1);
        }
        let enter = ctx.input(|input| input.key_pressed(egui::Key::Enter));

        let mut chosen: Option<PaletteAction> = None;
        let modal = egui::Modal::new(egui::Id::new("command-palette"))
            .backdrop_color(Color32::from_black_alpha(90))
            .show(ctx, |ui| {
                ui.set_width(520.0);
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.palette_query)
                        .hint_text("Navigate or act… (↑↓ to select, ↵ to run)")
                        .desired_width(f32::INFINITY)
                        .frame(Frame::NONE),
                );
                edit.request_focus();
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
                if filtered.is_empty() {
                    ui.label(
                        RichText::new("No matching command.")
                            .text_style(caption())
                            .color(TEXT_MUTE),
                    );
                }
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (index, (title, action, gate)) in filtered.into_iter().enumerate() {
                            let selected = index == self.palette_selected;
                            let gated = gate.is_some();
                            let (rect, resp) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), 32.0),
                                if gated {
                                    Sense::hover()
                                } else {
                                    Sense::click()
                                },
                            );
                            if resp.hovered() && !selected {
                                self.palette_selected = index;
                            }
                            let painter = ui.painter();
                            if selected {
                                painter.rect_filled(rect, CornerRadius::same(R1), SURFACE_HIGH);
                                if !gated {
                                    painter.rect_filled(
                                        Rect::from_min_size(
                                            rect.min + Vec2::new(0.0, 6.0),
                                            Vec2::new(2.0, rect.height() - 12.0),
                                        ),
                                        CornerRadius::same(1),
                                        EMBER,
                                    );
                                }
                            }
                            painter.text(
                                egui::pos2(rect.min.x + 12.0, rect.center().y),
                                Align2::LEFT_CENTER,
                                &title,
                                body_strong().resolve(ui.style()),
                                if gated {
                                    TEXT_MUTE
                                } else if selected {
                                    TEXT
                                } else {
                                    TEXT_DIM
                                },
                            );
                            if let Some(reason) = &gate {
                                painter.text(
                                    egui::pos2(rect.max.x - 12.0, rect.center().y),
                                    Align2::RIGHT_CENTER,
                                    format!("○ {reason}"),
                                    caption().resolve(ui.style()),
                                    TEXT_MUTE,
                                );
                            }
                            let fire = (!gated && resp.clicked()) || (!gated && selected && enter);
                            if fire {
                                chosen = Some(action);
                            }
                        }
                    });
            });
        if modal.should_close() {
            self.palette_open = false;
        }
        if let Some(action) = chosen {
            self.palette_open = false;
            match action {
                PaletteAction::Nav(nav) => {
                    self.nav = nav;
                    if nav == Nav::Fields2D && self.f2d.is_none() && !self.f2d_pending {
                        self.request_2d();
                    }
                }
                PaletteAction::UndoCaseEdit => {
                    self.apply_case_history_action(CaseHistoryAction::Undo)
                }
                PaletteAction::RedoCaseEdit => {
                    self.apply_case_history_action(CaseHistoryAction::Redo)
                }
                PaletteAction::Run => self.run_external_flow(),
                PaletteAction::ImportGeometry => self.import_cad(),
                PaletteAction::OpenProject => self.open_project_dialog(ctx),
                PaletteAction::SaveProject => {
                    self.save_project_dialog();
                }
                PaletteAction::OpenRecent(path) => self.request_project_action(
                    project_lifecycle::DeferredProjectAction::Open(path),
                    ctx,
                ),
            }
        }
    }

    /// 44px single-chrome top bar (§4.1): traffic-light inset, project
    /// identity + truthful dirty state, and the contextual run action. The
    /// bar itself is a window drag region.
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let dirty = self.has_unsaved_project_work();
        let window_title = format!(
            "{}{} — Reyn Studio",
            self.project.display_name(),
            if dirty { " •" } else { "" }
        );
        // Send the title command only when it changes (A21).
        if window_title != self.last_window_title {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Title(window_title.clone()));
            self.last_window_title = window_title;
        }
        let top_response = egui::Panel::top("top")
            .exact_size(44.0)
            .resizable(false)
            .frame(
                Frame::NONE
                    .fill(SURFACE_LOWEST)
                    .inner_margin(Margin::symmetric(14, 0)),
            )
            .show(ui, |ui| {
                // Window drag region behind the bar's widgets; double-click
                // zooms, matching native titlebar behavior.
                let drag = ui.interact(
                    ui.max_rect(),
                    egui::Id::new("topbar.drag"),
                    Sense::click_and_drag(),
                );
                if drag.drag_started_by(egui::PointerButton::Primary) {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if drag.double_clicked() {
                    let maximized = ui.input(|input| input.viewport().maximized.unwrap_or(false));
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
                ui.horizontal_centered(|ui| {
                    let fullscreen = ui.input(|input| input.viewport().fullscreen.unwrap_or(false));
                    // macOS traffic lights sit inside the full-size content
                    // view. Windows uses a standard title bar above this panel.
                    ui.add_space(if cfg!(target_os = "macos") && !fullscreen {
                        78.0
                    } else {
                        4.0
                    });
                    ui.label(
                        RichText::new("Reyn Studio")
                            .text_style(body_strong())
                            .color(BRAND),
                    );
                    ui.add_space(12.0);
                    // Project identity chip — the one place project name +
                    // dirty state appear in chrome (truthful, never implied).
                    let location = self
                        .project
                        .path()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "not saved to disk".into());
                    Frame::NONE
                        .fill(SURFACE)
                        .stroke(Stroke::new(1.0, HAIRLINE))
                        .corner_radius(CornerRadius::same(R1))
                        .inner_margin(Margin::symmetric(10, 4))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                ui.label(
                                    RichText::new(self.project.display_name())
                                        .text_style(body_strong())
                                        .color(TEXT),
                                );
                                if dirty {
                                    ui.label(RichText::new("●").size(8.0).color(WARN))
                                        .on_hover_text("Unsaved changes");
                                }
                            });
                        })
                        .response
                        .on_hover_text(location);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if matches!(self.nav, Nav::Case | Nav::Results) {
                            // Ember only when the run can actually start;
                            // otherwise the button is visibly disabled and
                            // carries the blocking reason (UX-AC-01: no dead
                            // or lying controls, ember budget §3.3).
                            let pending = self.cad.as_ref().is_some_and(|case| case.pending);
                            let reason = self.run_gate_reason();
                            if action_button_gated(
                                ui,
                                Some(ph::PLAY),
                                if pending {
                                    "Running…"
                                } else {
                                    "Run analysis"
                                },
                                EMBER,
                                ON_EMBER,
                                None,
                                34.0,
                                132.0,
                                reason.as_deref(),
                            ) {
                                self.run_external_flow();
                            }
                        } else if self.is_research_sandbox() {
                            let live_icon = if self.live { None } else { Some(ph::PLAY) };
                            let live_label = if self.live {
                                "◉  Sandbox live"
                            } else {
                                "Sandbox live"
                            };
                            // Ember only while running (running-state indicator);
                            // the idle toggle is a quiet/tonal button.
                            let (live_fill, live_fg, live_border) = if self.live {
                                (EMBER, ON_EMBER, None)
                            } else {
                                (SURFACE_HIGH, TEXT, Some(HAIRLINE))
                            };
                            if action_button(
                                ui,
                                live_icon,
                                live_label,
                                live_fill,
                                live_fg,
                                live_border,
                                34.0,
                                132.0,
                            ) {
                                self.live = !self.live;
                            }
                        }
                        ui.add_space(14.0);
                        // 2D | 3D VOLUMETRIC segmented toggle (left-to-right order)
                        let has_result = self
                            .cad
                            .as_ref()
                            .and_then(|case| case.workflow.result.as_ref())
                            .is_some();
                        if self.nav == Nav::Metrics || (self.nav == Nav::Results && has_result) {
                            // Concentric radii: thumb r-1 = 4 inside container
                            // r-2 = 6 with a 2px inset (§3.5).
                            Frame::NONE
                                .fill(SURFACE)
                                .corner_radius(CornerRadius::same(R2))
                                .stroke(Stroke::new(1.0, HAIRLINE))
                                .inner_margin(2)
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 2.0;
                                        if seg(ui, "2D section", !self.volumetric) {
                                            self.volumetric = false;
                                        }
                                        if seg(ui, "3D", self.volumetric) {
                                            self.volumetric = true;
                                        }
                                    });
                                });
                        }
                    });
                });
            });
        // Single meeting-edge hairline (QA C5): the bar owns only its bottom
        // border instead of stroking all four edges.
        let rect = top_response.response.rect;
        ui.painter().hline(
            rect.x_range(),
            rect.bottom() - 0.5,
            Stroke::new(1.0, OUTLINE_VARIANT),
        );
    }

    /// 24px status bar (§4.1): the single truthful home for engine state,
    /// long-operation progress, and the active model — replaces the floating
    /// engine pill that hovered over the viewport.
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let mut cancel_run = false;
        let status_response = egui::Panel::bottom("status")
            .exact_size(24.0)
            .resizable(false)
            .frame(
                Frame::NONE
                    .fill(SURFACE_LOWEST)
                    .inner_margin(Margin::symmetric(12, 0)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    // WARN is the status hue; GOLD stays reserved for data.
                    let engine_color = if self.engine_ok { SUCCESS } else { WARN };
                    // C6: long engine errors elide before the right-aligned
                    // run/model cluster; full text on hover.
                    ui.scope(|ui| {
                        ui.set_max_width((ui.available_width() - 330.0).max(140.0));
                        ui.add(
                            egui::Label::new(
                                RichText::new(&self.engine_status)
                                    .text_style(mono_s())
                                    .color(engine_color),
                            )
                            .truncate(),
                        )
                        .on_hover_text(&self.engine_status);
                    });
                    // Stage frames + elapsed: fraction is stage-based when present.
                    let run_busy = self.cad.as_ref().filter(|case| case.pending).map(|case| {
                        let elapsed = case
                            .pending_run
                            .as_ref()
                            .map(|pending| pending.started_at.elapsed().as_secs_f64())
                            .unwrap_or(0.0);
                        let stage = case
                            .pending_run
                            .as_ref()
                            .and_then(|pending| pending.progress.as_ref())
                            .map(|progress| {
                                format!(
                                    " · stage {}/{} {}",
                                    progress.stage_index,
                                    progress.stage_count,
                                    cad_progress_stage_label(&progress.stage)
                                )
                            })
                            .unwrap_or_default();
                        format!("◐ running immutable attempt · {elapsed:.0} s{stage}")
                    });
                    let run_in_flight = run_busy.is_some();
                    let busy = if let Some(busy) = run_busy {
                        Some(busy)
                    } else if self.bench_running {
                        Some("◐ benchmark suite running…".to_owned())
                    } else if self.f2d_pending {
                        Some("◐ 2D prediction pending…".to_owned())
                    } else if self.library.busy {
                        Some("◐ model-bundle operation in progress…".to_owned())
                    } else {
                        None
                    };
                    if let Some(busy) = busy {
                        ui.add_space(16.0);
                        // Busy is passive status — WARN, never the ember
                        // action accent (QA C6).
                        ui.label(RichText::new(busy).text_style(mono_s()).color(WARN));
                        if run_in_flight {
                            ui.add_space(8.0);
                            cancel_run = ui
                                .small_button("Cancel")
                                .on_hover_text(
                                    "Persist this attempt as cancelled, terminate the blocking sidecar, and start a fresh engine for retry. No result evidence is created.",
                                )
                                .clicked();
                        }
                        // The elapsed counter has to keep counting.
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_millis(250));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("model · {}", self.current_model))
                                .text_style(mono_s())
                                .color(TEXT_MUTE),
                        );
                        if let Some(run_id) = self
                            .cad
                            .as_ref()
                            .and_then(|case| case.active_run_id.as_deref())
                        {
                            ui.add_space(14.0);
                            ui.label(
                                RichText::new(format!("run · {}", short_id(run_id)))
                                    .text_style(mono_s())
                                    .color(TEXT_MUTE),
                            );
                        }
                    });
                });
            });
        // Single meeting-edge hairline (QA C5).
        let rect = status_response.response.rect;
        ui.painter().hline(
            rect.x_range(),
            rect.top() + 0.5,
            Stroke::new(1.0, OUTLINE_VARIANT),
        );
        if cancel_run {
            self.cancel_external_flow();
        }
    }

    /// Left rail (§4.1): project identity, then two visibly distinct groups —
    /// the workflow lifecycle with per-stage state glyphs, and workbench
    /// destinations. Resizable; the voxel-diagnostics card moved to the
    /// Results right rail where its field actually lives (SCI-AC-01).
    fn left_sidebar(&mut self, ui: &mut egui::Ui) {
        let sidebar_response = egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(248.0)
            .size_range(208.0..=320.0)
            .frame(
                Frame::NONE
                    .fill(SURFACE_LOWEST)
                    .inner_margin(Margin::same(16)),
            )
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.label(title_text(self.project.display_name()));
                let project_location = self
                    .project
                    .path()
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .unwrap_or("not saved");
                ui.label(
                    RichText::new(format!(
                        "{project_location} · schema v{}",
                        project::PROJECT_SCHEMA_VERSION
                    ))
                        .text_style(mono_s())
                        .color(TEXT_MUTE),
                );
                let (project_state, project_state_color) = if self.project.is_recovered() {
                    ("RECOVERED · UNSAVED", WARN)
                } else if self.has_unsaved_project_work() {
                    ("UNSAVED CHANGES", WARN)
                } else if self.project.path().is_some() {
                    ("SAVED LOCALLY", SUCCESS)
                } else {
                    ("LOCAL SESSION", TEXT_MUTE)
                };
                ui.label(chip_text(project_state).color(project_state_color));
                ui.add_space(24.0);

                ui.label(caps("Workflow"));
                ui.add_space(8.0);
                let summary = self.project.summary();
                let has_result = self
                    .cad
                    .as_ref()
                    .and_then(|case| case.workflow.result.as_ref())
                    .is_some();
                let stages = workflow_stage_states(
                    summary.cases > 0 || summary.runs > 0,
                    self.cad.is_some(),
                    has_result,
                    summary.runs > 0,
                );
                if stage_row(
                    ui,
                    ph::FOLDER,
                    "Project",
                    self.nav == Nav::Projects,
                    stages[0],
                ) {
                    self.nav = Nav::Projects;
                }
                if stage_row(ui, ph::WIND, "Case Setup", self.nav == Nav::Case, stages[1]) {
                    self.nav = Nav::Case;
                }
                // No silent redirect (A5): a blocked Results row explains
                // itself inline; stage_row never fires a click while blocked.
                if stage_row(
                    ui,
                    ph::CHART_BAR,
                    "Results",
                    self.nav == Nav::Results,
                    stages[2],
                ) {
                    self.nav = Nav::Results;
                }
                if stage_row(
                    ui,
                    ph::BOOK_OPEN,
                    "Evidence",
                    self.nav == Nav::Evidence,
                    stages[3],
                ) {
                    self.nav = Nav::Evidence;
                }

                ui.add_space(24.0);
                ui.label(caps("Workbench"));
                ui.add_space(8.0);
                if nav_row(ui, ph::CUBE, "Model Library", self.nav == Nav::Models) {
                    self.nav = Nav::Models;
                }
                if nav_row(ui, ph::GEAR, "Settings", self.nav == Nav::Settings) {
                    self.nav = Nav::Settings;
                }

                if self.settings.developer_research_sandbox {
                    ui.add_space(24.0);
                    ui.label(caps("Developer · Research Sandbox"));
                    ui.label(chip_text("NOT ENGINEERING EVIDENCE").color(WARN));
                    ui.add_space(8.0);
                    if nav_row(ui, ph::ATOM, "Procedural 3D", self.nav == Nav::Metrics) {
                        self.nav = Nav::Metrics;
                    }
                    if nav_row(
                        ui,
                        ph::PAINT_BRUSH,
                        "Flow Painter",
                        self.nav == Nav::FlowPainter,
                    ) {
                        self.nav = Nav::FlowPainter;
                    }
                    if nav_row(ui, ph::STACK, "Fields (2D)", self.nav == Nav::Fields2D) {
                        self.nav = Nav::Fields2D;
                        if self.f2d.is_none() && !self.f2d_pending {
                            self.request_2d();
                        }
                    }
                    if nav_row(ui, ph::FLASK, "Benchmark Lab", self.nav == Nav::Benchmark) {
                        self.nav = Nav::Benchmark;
                    }
                }
            });
        // Single meeting-edge hairline (QA C5).
        let rect = sidebar_response.response.rect;
        ui.painter().vline(
            rect.right() - 0.5,
            rect.y_range(),
            Stroke::new(1.0, OUTLINE_VARIANT),
        );
    }

    fn right_controls(&mut self, ui: &mut egui::Ui) {
        let has_case = self.cad.is_some();
        let has_result = self
            .cad
            .as_ref()
            .and_then(|case| case.workflow.result.as_ref())
            .is_some();
        // Model Library owns its whole screen (§4.6). Empty Results/Evidence
        // states do too: one composed center state is clearer than duplicate
        // absence copy in a narrow, otherwise empty detail rail.
        if !should_show_detail_rail(self.nav, has_case, has_result) {
            return;
        }
        let rail_response = egui::Panel::right("controls")
            .resizable(true)
            .default_size(330.0)
            .size_range(280.0..=420.0)
            .frame(Frame::NONE.fill(BG).inner_margin(Margin::same(16)))
            .show(ui, |ui| {
                // Every rail scrolls (QA CS1/R1/P3/X1): one wrapper at the
                // dispatch point so short windows can always reach the
                // bottom-most controls on every screen.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.right_controls_body(ui));
            });
        // Single meeting-edge hairline (QA C5).
        let rect = rail_response.response.rect;
        ui.painter().vline(
            rect.left() + 0.5,
            rect.y_range(),
            Stroke::new(1.0, OUTLINE_VARIANT),
        );
    }

    fn right_controls_body(&mut self, ui: &mut egui::Ui) {
        if self.nav == Nav::Projects {
            self.controls_project(ui);
            return;
        }
        if self.nav == Nav::Case {
            self.controls_engineering_case(ui);
            return;
        }
        if self.nav == Nav::Results {
            self.controls_engineering_results(ui);
            return;
        }
        if self.nav == Nav::Evidence {
            self.controls_engineering_evidence(ui);
            return;
        }
        if self.nav == Nav::Fields2D {
            self.controls_2d(ui);
            return;
        }
        if self.nav == Nav::FlowPainter {
            self.controls_painter(ui);
            return;
        }
        if self.nav == Nav::Benchmark {
            self.controls_bench(ui);
            return;
        }
        if self.nav == Nav::Settings {
            settings::show_controls(
                ui,
                &self.settings,
                &self.engine_status,
                self.engine_ok,
                self.settings_notice.as_ref(),
            );
            return;
        }
        ui.spacing_mut().slider_width = 120.0;
        ui.label(title_text("3D Controls"));
        ui.add_space(20.0);

        ui.label(caps("Slicing Planes"));
        ui.add_space(8.0);
        for (i, axis) in ["X", "Y", "Z"].iter().enumerate() {
            Frame::NONE
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, OUTLINE_VARIANT))
                .corner_radius(CornerRadius::same(3))
                .inner_margin(Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.slice[i], "");
                        ui.label(RichText::new(*axis).color(TEXT).strong());
                        ui.add(
                            egui::Slider::new(&mut self.slice_pos[i], 0.0..=1.0)
                                .show_value(false)
                                .trailing_fill(true),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(
                                mono(&format!("{:.2}", self.slice_pos[i]), TEXT_DIM).size(12.0),
                            );
                        });
                    });
                });
            ui.add_space(6.0);
        }

        ui.add_space(14.0);
        ui.label(caps("Isosurface Threshold"));
        ui.add_space(8.0);
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Density").color(TEXT_DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        mono(
                            &format!("{:.2} – {:.1}", self.density_lo, self.density_hi),
                            EMBER,
                        )
                        .size(12.0),
                    );
                });
            });
            ui.add_space(6.0);
            ui.spacing_mut().slider_width = ui.available_width() - 8.0;
            ui.add(
                egui::Slider::new(&mut self.density_lo, 0.0..=self.density_hi)
                    .show_value(false)
                    .trailing_fill(true),
            );
        });

        ui.add_space(18.0);
        ui.label(caps("Rendering Options"));
        ui.add_space(8.0);
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Global Opacity").color(TEXT_DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(mono(&format!("{:.0}%", self.opacity * 100.0), TEXT_DIM).size(12.0));
                });
            });
            ui.spacing_mut().slider_width = ui.available_width() - 8.0;
            ui.add(
                egui::Slider::new(&mut self.opacity, 0.0..=1.0)
                    .show_value(false)
                    .trailing_fill(true),
            );
            ui.add_space(8.0);
            ui.checkbox(
                &mut self.shadows,
                RichText::new("Volumetric Shadows").color(TEXT_DIM),
            );
            // HAZARD GATE (viewport::analytic_streamlines): these ribbons advect
            // a closed-form demo field, never model velocity, so the control
            // names that in place and renders only inside the sandbox.
            ui.add_enabled_ui(self.is_research_sandbox(), |ui| {
                ui.checkbox(
                    &mut self.streamlines,
                    RichText::new("Streamlines · analytic demo field").color(TEXT_DIM),
                )
                .on_hover_text(viewport::ANALYTIC_STREAMLINE_LABEL)
                .on_disabled_hover_text(
                    "Quarantined: the streamline overlay is not driven by model velocity, so it is unavailable outside the research sandbox.",
                );
            });
            ui.add_enabled_ui(self.volumetric, |ui| {
                ui.checkbox(
                    &mut self.render_volume,
                    RichText::new("Volume Raymarch").color(TEXT_DIM),
                );
            });
            ui.add_enabled_ui(!self.insights3d.is_empty(), |ui| {
                ui.checkbox(
                    &mut self.insights_on,
                    RichText::new("Pin Critical Points").color(TEXT_DIM),
                )
                .on_disabled_hover_text("needs a model field (engine prediction)");
            });
            ui.add_enabled_ui(self.cad.as_ref().is_some_and(|c| c.surf.is_some()), |ui| {
                        ui.checkbox(
                            &mut self.surface_on,
                            RichText::new("Normalized recovered pressure").color(TEXT_DIM),
                        )
                        .on_hover_text(
                            "Density-normalized recovered pressure, normalized again to the visible color range; a physical pressure coefficient is not computed.",
                        )
                        .on_disabled_hover_text("import a CAD geometry (File → Import CAD)");
                    });
        });

        ui.add_space(20.0);
        // Quiet export button — gold returns to data duty only. Flows
        // with the content (the rail scrolls) instead of bottom_up,
        // which would stretch inside the ScrollArea.
        if action_button(
            ui,
            Some(ph::DOWNLOAD_SIMPLE),
            "Export calculations…",
            SURFACE_HIGH,
            TEXT,
            Some(HAIRLINE),
            40.0,
            ui.available_width(),
        ) {
            self.export();
        }
    }

    fn controls_project(&mut self, ui: &mut egui::Ui) {
        let summary = self.project.summary();
        let availability = self.project.availability().clone();
        let mut relink_digest = None;
        ui.label(title_text("Project"));
        ui.label(
            RichText::new("local lifecycle and recovery")
                .size(11.5)
                .color(TEXT_MUTE),
        );
        ui.add_space(16.0);

        if let Some((message, is_error)) = &self.project_notice {
            // Calm alert recipe (§3.3): tint fill + same-hue hairline; full
            // color only on the leading glyph.
            let (fill, stroke) = if *is_error {
                (tint_fill(DANGER), tint_hairline(DANGER))
            } else {
                (SURFACE, HAIRLINE)
            };
            Frame::NONE
                .fill(fill)
                .stroke(Stroke::new(1.0, stroke))
                .corner_radius(CornerRadius::same(R1))
                .inner_margin(Margin::same(11))
                .show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        if *is_error {
                            ui.label(
                                RichText::new("!")
                                    .text_style(caption())
                                    .strong()
                                    .color(DANGER),
                            );
                        }
                        ui.add(
                            egui::Label::new(
                                RichText::new(message).text_style(caption()).color(TEXT_DIM),
                            )
                            .wrap(),
                        );
                    });
                });
            ui.add_space(12.0);
        }

        card(ui, |ui| {
            ui.label(caps("Active document"));
            ui.add_space(8.0);
            diag(
                ui,
                "Schema",
                &format!("v{} · strict", project::PROJECT_SCHEMA_VERSION),
                TEXT,
            );
            diag(ui, "Cases", &summary.cases.to_string(), TEXT_DIM);
            diag(ui, "Immutable runs", &summary.runs.to_string(), BRAND);
            diag(ui, "Evidence links", &summary.evidence.to_string(), GOLD);
            diag(
                ui,
                "Verified bundle objects",
                &format!(
                    "{} / {}",
                    availability.bundle.available_objects, availability.bundle.required_objects
                ),
                if availability.bundle.diagnostics == 0 {
                    SUCCESS
                } else {
                    WARN
                },
            );
            let state = if self.project.is_recovered() {
                "RECOVERED · SAVE REQUIRED"
            } else if self.has_unsaved_project_work() {
                "UNSAVED CHANGES"
            } else if self.project.path().is_some() {
                "SAVED LOCALLY"
            } else {
                "NO UNSAVED CHANGES"
            };
            ui.label(chip_text(state).color(if self.has_unsaved_project_work() {
                WARN
            } else {
                SUCCESS
            }));
        });

        ui.add_space(12.0);
        let project_id = self.project.manifest().project_id().to_owned();
        let mut name_changed = false;
        card(ui, |ui| {
            ui.label(caps("Project identity"));
            ui.add_space(7.0);
            name_changed = ui
                .add_enabled(
                    !availability.is_read_only_evidence(),
                    egui::TextEdit::singleline(&mut self.project_name_draft)
                        .char_limit(120)
                        .desired_width(ui.available_width()),
                )
                .on_hover_text(if availability.is_read_only_evidence() {
                    "Project identity is locked in read-only evidence mode."
                } else {
                    "Stored project name; edits create unsaved project metadata."
                })
                .changed();
            ui.label(
                RichText::new(project_id)
                    .text_style(mono_s())
                    .color(TEXT_MUTE),
            );
        });
        if name_changed && !self.reject_project_mutation("Renaming the project") {
            self.project
                .rename_project(self.project_name_draft.clone(), now_utc_unix());
            self.project_notice = Some(("Project name changed; save is required.".into(), false));
        }

        ui.add_space(12.0);
        card(ui, |ui| {
            let (title, detail, color) = if availability.is_read_only_evidence() {
                (
                    "READ-ONLY EVIDENCE MODE",
                    "Stored results remain inspectable. Compute or content dependencies must reconcile before rerun.",
                    WARN,
                )
            } else if self.engine_ok {
                (
                    "DEPENDENCIES RECONCILED",
                    "Portable content is verified and the validated compute inventory satisfies this project.",
                    SUCCESS,
                )
            } else {
                (
                    "COMPUTE UNAVAILABLE · STORAGE READY",
                    "Project files and stored evidence remain available. New engine-backed runs are unavailable.",
                    WARN,
                )
            };
            ui.label(
                RichText::new(title)
                    .strong()
                    .text_style(mono_s())
                    .color(color),
            );
            ui.label(RichText::new(detail).text_style(caption()).color(TEXT_MUTE));
        });

        if !availability.issues.is_empty() {
            ui.add_space(12.0);
            card(ui, |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} PRECISE DIAGNOSTIC{}",
                        availability.issues.len(),
                        if availability.issues.len() == 1 {
                            ""
                        } else {
                            "S"
                        }
                    ))
                    .strong()
                    .text_style(mono_s())
                    .color(WARN),
                );
                ui.label(
                    RichText::new(
                        "Stored runs remain immutable and inspectable; affected reruns stay unavailable.",
                    )
                    .text_style(caption())
                    .color(TEXT_MUTE),
                );
                ui.collapsing("Dependency & integrity details", |ui| {
                    for issue in availability.issues.iter().take(8) {
                        ui.label(
                            RichText::new(format!(
                                "{} · {}",
                                dependency_kind_label(issue.kind),
                                issue.detail
                            ))
                            .text_style(caption())
                            .color(
                                if issue.kind == project_lifecycle::DependencyKind::Integrity {
                                    TEXT_DIM
                                } else {
                                    WARN
                                },
                            ),
                        );
                        if issue.relinkable {
                            if let Some(digest) = issue.content_sha256.as_deref() {
                                if ui
                                    .small_button(format!(
                                        "Relink {}…",
                                        digest.get(..12).unwrap_or(digest)
                                    ))
                                    .clicked()
                                {
                                    relink_digest = Some(digest.to_owned());
                                }
                            }
                        }
                    }
                });
            });
        }

        ui.add_space(14.0);
        // Quiet save — the Projects screen's single ember action is the
        // "Import Geometry…" primary in the content region.
        if action_button(
            ui,
            Some(ph::DOWNLOAD_SIMPLE),
            "Save project",
            SURFACE_HIGH,
            TEXT,
            Some(HAIRLINE),
            40.0,
            ui.available_width(),
        ) {
            self.save_project_dialog();
        }
        ui.add_space(6.0);
        if action_button(
            ui,
            None,
            "Save As…",
            SURFACE_HIGH,
            TEXT,
            Some(OUTLINE),
            34.0,
            ui.available_width(),
        ) {
            self.save_project_as_dialog();
        }
        ui.add_space(6.0);
        if action_button(
            ui,
            Some(ph::FOLDER_OPEN),
            "Open Project…",
            Color32::TRANSPARENT,
            TEXT_DIM,
            Some(OUTLINE_VARIANT),
            34.0,
            ui.available_width(),
        ) {
            self.open_project_dialog(ui.ctx());
        }

        ui.add_space(12.0);
        ui.label(
            RichText::new(format!(
                "autosave {} s · {} recovery snapshot{}",
                self.settings.autosave_interval_seconds,
                self.project.recovery_entries().len(),
                if self.project.recovery_entries().len() == 1 {
                    ""
                } else {
                    "s"
                }
            ))
            .text_style(mono_s())
            .color(TEXT_MUTE),
        );
        if let Some(digest) = relink_digest {
            self.relink_project_content_dialog(&digest);
        }
    }

    fn project_view(&mut self, ui: &mut egui::Ui) {
        let summary = self.project.summary();
        let recent_projects = self.project.recent_projects().to_vec();
        let recovery_entries = self.project.recovery_entries().to_vec();
        let notice = self.project_notice.clone();
        let availability = self.project.availability().clone();
        let location = self
            .project
            .path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Not saved yet".into());
        let mut requested_action = None;
        let mut discard_recovery = None;
        let mut remove_recent = None;
        let mut save_current = false;
        let mut save_as = false;
        let mut open_dialog = false;
        let mut inspect_run = None;
        let mut relink_digest = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // One shared content column (§3.2, QA G3/G4).
                content_column(ui, CONTENT_MAX_WIDTH, |ui| {
                        ui.add_space(24.0);
                        // §4.2: first-run gets a deliberate landing; a
                        // returning user gets their working set first.
                        let first_run = recent_projects.is_empty()
                            && recovery_entries.is_empty()
                            && self.project.manifest().cases().is_empty()
                            && self.project.path().is_none();
                        if first_run {
                            ui.add_space(24.0);
                            ui.label(display_text("Start an analysis"));
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(
                                    "Fixed-body external flow · STL and single-part STEP preprocessing. Everything stays on this machine.",
                                )
                                .text_style(caption())
                                .color(TEXT_MUTE),
                            );
                            ui.add_space(24.0);
                            let zone_width = (ui.available_width() - 24.0) / 2.0;
                            ui.horizontal_top(|ui| {
                                // Left zone: the single ember action.
                                ui.allocate_ui(Vec2::new(zone_width, 160.0), |ui| {
                                    ui.vertical(|ui| {
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    RichText::new("Import geometry (STL / STEP)…")
                                                        .color(ON_EMBER),
                                                )
                                                .fill(EMBER)
                                                .min_size(Vec2::new(190.0, 38.0)),
                                            )
                                            .clicked()
                                        {
                                            self.import_cad();
                                        }
                                        ui.add_space(8.0);
                                        ui.label(
                                            RichText::new(
                                                "Import geometry, qualify the setup, run the fixed-body model, and produce traceable fluid-load evidence.",
                                            )
                                            .text_style(caption())
                                            .color(TEXT_MUTE),
                                        );
                                    });
                                });
                                ui.add_space(24.0);
                                // Right zone: quiet open + drop target.
                                ui.vertical(|ui| {
                                    ui.set_width(zone_width);
                                    if ui
                                        .add(egui::Button::new("Open project…")
                                            .min_size(Vec2::new(140.0, 30.0)))
                                        .clicked()
                                    {
                                        open_dialog = true;
                                    }
                                    ui.add_space(8.0);
                                    drop_target(ui, zone_width);
                                });
                            });
                            ui.add_space(32.0);
                            ui.label(overline_text("How Reyn works"));
                            ui.add_space(8.0);
                            // Three *text* steps in one flat group — no
                            // icon-card row (banned three-equal-cards).
                            for (step, title, body) in [
                                (
                                    "1",
                                    "Source",
                                    "Exact geometry bytes are hashed and preflighted; nothing enters the case unverified.",
                                ),
                                (
                                    "2",
                                    "Run",
                                    "The qualified model executes an immutable attempt with the full operating point recorded.",
                                ),
                                (
                                    "3",
                                    "Evidence",
                                    "Every number stays tied to its source, case revision, run, and model hash.",
                                ),
                            ] {
                                ui.horizontal_top(|ui| {
                                    ui.label(
                                        RichText::new(step)
                                            .text_style(mono_s())
                                            .color(TEXT_MUTE),
                                    );
                                    ui.add_space(8.0);
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        ui.label(
                                            RichText::new(title)
                                                .text_style(body_strong())
                                                .color(TEXT),
                                        );
                                        ui.label(
                                            RichText::new(body)
                                                .text_style(caption())
                                                .color(TEXT_MUTE),
                                        );
                                    });
                                });
                                ui.add_space(12.0);
                            }
                        } else {
                            ui.label(
                                display_text("Projects"),
                            );
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(
                                    "Local project documents, real recent paths, and recoverable unsaved work.",
                                )
                                .text_style(caption())
                                .color(TEXT_MUTE),
                            );
                            ui.add_space(20.0);

                            Frame::NONE
                                .fill(SURFACE)
                                .stroke(Stroke::new(1.0, HAIRLINE))
                                .corner_radius(CornerRadius::same(R2))
                                .inner_margin(Margin::same(20))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        // P1: cap the text column so the long
                                        // description wraps instead of pushing
                                        // the ember CTA past the panel edge.
                                        let text_width = (ui.available_width() - 150.0 - 24.0)
                                            .max(180.0);
                                        ui.vertical(|ui| {
                                            ui.set_max_width(text_width);
                                            ui.label(
                                                title_text("New external-flow analysis"),
                                            );
                                            ui.label(
                                                RichText::new(
                                                    "Import geometry, qualify the setup, run the fixed-body model, and produce traceable fluid-load evidence.",
                                                )
                                                .text_style(caption())
                                                .color(TEXT_MUTE),
                                            );
                                        });
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            if ui
                                                .add(
                                                    egui::Button::new(
                                                    RichText::new("Import geometry…")
                                                        .color(ON_EMBER),
                                                )
                                                .fill(EMBER)
                                                .min_size(Vec2::new(150.0, 38.0)),
                                                )
                                                .clicked()
                                            {
                                                self.import_cad();
                                            }
                                        });
                                    });
                                });
                        }
                        ui.add_space(16.0);

                        if let Some((message, is_error)) = &notice {
                            // Calm alert recipe (§3.3).
                            let (fill, stroke) = if *is_error {
                                (tint_fill(DANGER), tint_hairline(DANGER))
                            } else {
                                (SURFACE_LOW, HAIRLINE)
                            };
                            Frame::NONE
                                .fill(fill)
                                .stroke(Stroke::new(1.0, stroke))
                                .corner_radius(CornerRadius::same(R1))
                                .inner_margin(Margin::same(12))
                                .show(ui, |ui| {
                                    ui.horizontal_top(|ui| {
                                        if *is_error {
                                            ui.label(
                                                RichText::new("!")
                                                    .size(11.0)
                                                    .strong()
                                                    .color(DANGER),
                                            );
                                        }
                                        ui.label(
                                            RichText::new(message).size(11.0).color(TEXT_DIM),
                                        );
                                    });
                                });
                            ui.add_space(14.0);
                        }

                        if !first_run {
                        Frame::NONE
                            .fill(SURFACE)
                            .stroke(Stroke::new(1.0, OUTLINE_VARIANT))
                            .corner_radius(CornerRadius::same(R2))
                            .inner_margin(Margin::same(18))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    // P2: cap the identity column so long
                                    // paths elide instead of pushing the
                                    // state chip past the panel edge.
                                    let text_width =
                                        (ui.available_width() - 190.0).max(180.0);
                                    ui.vertical(|ui| {
                                        ui.set_max_width(text_width);
                                        ui.label(
                                            title_text(self.project.display_name()),
                                        );
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(&location)
                                                    .text_style(mono_s())
                                                    .color(TEXT_MUTE),
                                            )
                                            .truncate(),
                                        )
                                        .on_hover_text(&location);
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                                        let (state, color) = if self.project.is_recovered() {
                                            ("RECOVERED · UNSAVED", WARN)
                                        } else if self.has_unsaved_project_work() {
                                            ("UNSAVED CHANGES", WARN)
                                        } else if self.project.path().is_some() {
                                            ("SAVED LOCALLY ✓", SUCCESS)
                                        } else {
                                            ("LOCAL SESSION", TEXT_MUTE)
                                        };
                                        ui.label(chip_text(state).color(color));
                                    });
                                });
                                ui.add_space(14.0);
                                ui.separator();
                                ui.add_space(10.0);
                                ui.horizontal_wrapped(|ui| {
                                    project_fact(ui, "SCHEMA", "v2");
                                    project_fact(ui, "SOURCES", &summary.sources.to_string());
                                    project_fact(ui, "CASES", &summary.cases.to_string());
                                    project_fact(ui, "IMMUTABLE RUNS", &summary.runs.to_string());
                                    project_fact(ui, "EVIDENCE", &summary.evidence.to_string());
                                    project_fact(
                                        ui,
                                        "BUNDLE OBJECTS",
                                        &format!(
                                            "{} / {} verified",
                                            availability.bundle.available_objects,
                                            availability.bundle.required_objects
                                        ),
                                    );
                                });
                                ui.add_space(12.0);
                                ui.horizontal(|ui| {
                                    // Quiet save — the landing's single ember
                                    // action is "Import geometry…" above.
                                    if ui.button("Save project").clicked() {
                                        save_current = true;
                                    }
                                    if ui.button("Save As…").clicked() {
                                        save_as = true;
                                    }
                                    if ui.button("New Project").clicked() {
                                        requested_action =
                                            Some(project_lifecycle::DeferredProjectAction::New);
                                    }
                                    if ui.button("Open Project…").clicked() {
                                        open_dialog = true;
                                    }
                                });
                            });
                        }

                        if !availability.issues.is_empty() {
                            ui.add_space(14.0);
                            Frame::NONE
                                .fill(tint_fill(WARN))
                                .stroke(Stroke::new(1.0, tint_hairline(WARN)))
                                .corner_radius(CornerRadius::same(R1))
                                .inner_margin(Margin::same(14))
                                .show(ui, |ui| {
                                    ui.label(
                                        chip_text(if availability.is_read_only_evidence() {
                                            "READ-ONLY EVIDENCE · DEPENDENCIES UNAVAILABLE"
                                        } else {
                                            "BUNDLE INTEGRITY NOTICE"
                                        })
                                        .color(WARN),
                                    );
                                    ui.label(
                                        RichText::new(
                                            "Stored manifests, calibrated views, warnings, runs, and valid evidence remain inspectable. Reconciliation never rewrites a completed run.",
                                        )
                                        .size(11.0)
                                        .color(TEXT_DIM),
                                    );
                                    ui.collapsing("Dependency & integrity details", |ui| {
                                        for issue in availability.issues.iter().take(12) {
                                            ui.horizontal_wrapped(|ui| {
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{} · {}",
                                                        dependency_kind_label(issue.kind),
                                                        issue.detail
                                                    ))
                                                    .text_style(caption())
                                                    .color(if issue.kind
                                                        == project_lifecycle::DependencyKind::Integrity
                                                    {
                                                        TEXT_DIM
                                                    } else {
                                                        WARN
                                                    }),
                                                );
                                                if issue.relinkable {
                                                    if let Some(digest) =
                                                        issue.content_sha256.as_deref()
                                                    {
                                                        if ui
                                                            .small_button(format!(
                                                                "Relink {}…",
                                                                digest
                                                                    .get(..12)
                                                                    .unwrap_or(digest)
                                                            ))
                                                            .clicked()
                                                        {
                                                            relink_digest =
                                                                Some(digest.to_owned());
                                                        }
                                                    }
                                                }
                                            });
                                        }
                                    });
                                });
                        }

                        if !self.project.manifest().cases().is_empty() {
                            ui.add_space(24.0);
                            ui.label(caps("Cases and immutable run history"));
                            ui.label(
                                RichText::new(
                                    "Stored values open their exact run and evidence; stale lineage never rewrites a completed attempt.",
                                )
                                .text_style(caption())
                                .color(TEXT_MUTE),
                            );
                            ui.add_space(8.0);
                            for case in self.project.manifest().cases() {
                                Frame::NONE
                                    .fill(SURFACE_LOW)
                                    .stroke(Stroke::new(1.0, OUTLINE_VARIANT))
                                    .corner_radius(CornerRadius::same(3))
                                    .inner_margin(Margin::same(14))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.vertical(|ui| {
                                                ui.label(
                                                    RichText::new(case.name())
                                                        .strong()
                                                        .size(13.0)
                                                        .color(TEXT),
                                                );
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{} revisions · active {}",
                                                        case.revisions().len(),
                                                        short_id(case.active_revision_id())
                                                    ))
                                                    .text_style(mono_s())
                                                    .color(TEXT_MUTE),
                                                );
                                            });
                                            ui.with_layout(
                                                Layout::right_to_left(Align::Center),
                                                |ui| {
                                                    if case.stale_stages().is_empty() {
                                                        ui.label(
                                                            RichText::new("DEPENDENCIES CURRENT")
                                                                .strong().text_style(mono_s())
                                                                .color(SUCCESS),
                                                        );
                                                    } else {
                                                        ui.label(
                                                            RichText::new(format!(
                                                                "STALE · {}",
                                                                format_stages(
                                                                    case.stale_stages()
                                                                )
                                                            ))
                                                            .strong().text_style(mono_s())
                                                            .color(WARN),
                                                        );
                                                    }
                                                },
                                            );
                                        });
                                        for source_id in
                                            &case.active_revision().source_revision_ids
                                        {
                                            if let Some(source) = self
                                                .project
                                                .manifest()
                                                .source_revisions()
                                                .iter()
                                                .find(|source| {
                                                    source.source_revision_id == *source_id
                                                })
                                            {
                                                let (content_state, content_color) =
                                                    content_state_presentation(
                                                        self.project.content_state(
                                                            &source.content_sha256,
                                                        ),
                                                    );
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{} source · revision {} · sha256 {} · {}",
                                                        source_kind_label(source.source_kind),
                                                        source.revision,
                                                        short_hash(&source.content_sha256),
                                                        content_state
                                                    ))
                                                    .text_style(mono_s())
                                                    .color(content_color),
                                                );
                                                for warning in source.warnings.iter().take(2) {
                                                    ui.label(
                                                        RichText::new(format!(
                                                            "Source warning · {warning}"
                                                        ))
                                                        .text_style(caption())
                                                        .color(WARN),
                                                    );
                                                }
                                            }
                                        }
                                        ui.add_space(8.0);
                                        if case.runs().is_empty() {
                                            ui.label(
                                                RichText::new(
                                                    "No completed run yet. The configured case remains local and saveable.",
                                                )
                                                .text_style(caption())
                                                .color(TEXT_MUTE),
                                            );
                                        }
                                        for run in case.runs().iter().rev() {
                                            let evidence =
                                                self.project.manifest().evidence_for_run(run.run_id());
                                            let stale = case
                                                .stale_stages_for_run(run.run_id())
                                                .unwrap_or_default();
                                            Frame::NONE
                                                .fill(Color32::TRANSPARENT)
                                                .stroke(Stroke::new(
                                                    1.0,
                                                    OUTLINE_VARIANT.gamma_multiply(0.75),
                                                ))
                                                .corner_radius(CornerRadius::same(3))
                                                .inner_margin(Margin::symmetric(11, 9))
                                                .show(ui, |ui| {
                                                    ui.horizontal(|ui| {
                                                        ui.vertical(|ui| {
                                                            let state = if stale.contains(
                                                                &project::DependencyStage::Run,
                                                            ) {
                                                                "STALE INPUT LINEAGE"
                                                            } else {
                                                                "COMPLETE · IMMUTABLE"
                                                            };
                                                            ui.label(
                                                                RichText::new(format!(
                                                                    "{} · {}",
                                                                    short_id(run.run_id()),
                                                                    state
                                                                ))
                                                                .strong().text_style(mono_s())
                                                                .color(if stale.contains(
                                                                    &project::DependencyStage::Run,
                                                                ) {
                                                                    WARN
                                                                } else {
                                                                    BRAND
                                                                }),
                                                            );
                                                            let model_hash = run
                                                                .manifest()
                                                                .model
                                                                .as_ref()
                                                                .and_then(|model| {
                                                                    model.sha256.as_deref()
                                                                })
                                                                .map(short_hash)
                                                                .unwrap_or_else(|| "UNKNOWN".into());
                                                            ui.label(
                                                                RichText::new(format!(
                                                                    "model {model_hash} · {} scalar{} · {} evidence link{}",
                                                                    run.manifest().scalar_outputs.len(),
                                                                    if run.manifest().scalar_outputs.len() == 1 { "" } else { "s" },
                                                                    evidence.len(),
                                                                    if evidence.len() == 1 { "" } else { "s" },
                                                                ))
                                                                .text_style(mono_s())
                                                                .color(TEXT_MUTE),
                                                            );
                                                            if let Some(determinism) =
                                                                &run.manifest().determinism
                                                            {
                                                                ui.label(
                                                                    RichText::new(format!(
                                                                        "rerun of {} · {}",
                                                                        short_id(
                                                                            &determinism
                                                                                .parent_run_id
                                                                        ),
                                                                        determinism_status_label(
                                                                            determinism.status
                                                                        )
                                                                    ))
                                                                    .text_style(mono_s())
                                                                    .color(
                                                                        determinism_status_color(
                                                                            determinism.status,
                                                                        ),
                                                                    ),
                                                                );
                                                                if determinism.status
                                                                    != project::DeterminismStatus::WithinTolerance
                                                                {
                                                                    for difference in determinism
                                                                        .differences
                                                                        .iter()
                                                                        .filter(|difference| {
                                                                            match (
                                                                                difference.abs_difference,
                                                                                difference.abs_tolerance,
                                                                            ) {
                                                                                (Some(delta), Some(tolerance)) => {
                                                                                    delta > tolerance
                                                                                }
                                                                                _ => true,
                                                                            }
                                                                        })
                                                                        .take(2)
                                                                    {
                                                                        ui.label(
                                                                            RichText::new(format!(
                                                                                "{} · parent {} · rerun {} · |Δ| {} · tol {}",
                                                                                difference.key,
                                                                                optional_scalar(
                                                                                    difference
                                                                                        .parent_value
                                                                                ),
                                                                                optional_scalar(
                                                                                    difference
                                                                                        .current_value
                                                                                ),
                                                                                optional_scalar(
                                                                                    difference
                                                                                        .abs_difference
                                                                                ),
                                                                                optional_scalar(
                                                                                    difference
                                                                                        .abs_tolerance
                                                                                ),
                                                                            ))
                                                                            .text_style(mono_s())
                                                                            .color(DATA_RED),
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                            for warning in
                                                                run.manifest().warnings.iter().take(2)
                                                            {
                                                                ui.label(
                                                                    RichText::new(format!(
                                                                        "Warning · {warning}"
                                                                    ))
                                                                    .text_style(caption())
                                                                    .color(WARN),
                                                                );
                                                            }
                                                            for artifact in evidence.iter().take(3)
                                                            {
                                                                let (
                                                                    content_state,
                                                                    content_color,
                                                                ) = content_state_presentation(
                                                                    self.project.content_state(
                                                                        &artifact.content_sha256,
                                                                    ),
                                                                );
                                                                ui.label(
                                                                    RichText::new(format!(
                                                                        "Evidence {} · {} · sha256 {} · {}",
                                                                        short_id(
                                                                            &artifact.evidence_id
                                                                        ),
                                                                        artifact.media_type,
                                                                        short_hash(
                                                                            &artifact
                                                                                .content_sha256
                                                                        ),
                                                                        content_state
                                                                    ))
                                                                    .text_style(mono_s())
                                                                    .color(content_color),
                                                                );
                                                            }
                                                        });
                                                        ui.with_layout(
                                                            Layout::right_to_left(Align::Center),
                                                            |ui| {
                                                                if ui
                                                                    .add_enabled(
                                                                        !evidence.is_empty(),
                                                                        egui::Button::new(
                                                                            "Inspect evidence",
                                                                        ),
                                                                    )
                                                                    .on_disabled_hover_text(
                                                                        "This run has no stored evidence artifact",
                                                                    )
                                                                    .clicked()
                                                                {
                                                                    inspect_run =
                                                                        Some(run.run_id().to_owned());
                                                                }
                                                            },
                                                        );
                                                    });
                                                });
                                            ui.add_space(6.0);
                                        }
                                    });
                                ui.add_space(8.0);
                            }
                        }

                        if !recovery_entries.is_empty() {
                            ui.add_space(24.0);
                            ui.label(caps("Crash recovery"));
                            ui.label(
                                RichText::new(
                                    "Snapshots are separate from explicit saves and never overwrite a project until you choose Save.",
                                )
                                .text_style(caption())
                                .color(TEXT_MUTE),
                            );
                            ui.add_space(8.0);
                            for recovery in &recovery_entries {
                                // §4.2: recovery rows wear the warn recipe —
                                // tinted fill + hairline, not a gold border.
                                Frame::NONE
                                    .fill(tint_fill(WARN))
                                    .stroke(Stroke::new(1.0, tint_hairline(WARN)))
                                    .corner_radius(CornerRadius::same(R1))
                                    .inner_margin(Margin::symmetric(14, 11))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            let text_width =
                                                (ui.available_width() - 190.0).max(160.0);
                                            ui.vertical(|ui| {
                                                ui.set_max_width(text_width);
                                                ui.spacing_mut().item_spacing.y = 2.0;
                                                ui.label(
                                                    RichText::new(&recovery.name)
                                                        .text_style(body_strong())
                                                        .color(TEXT),
                                                );
                                                let source = recovery
                                                    .source_path
                                                    .as_ref()
                                                    .map(|path| path.display().to_string())
                                                    .unwrap_or_else(|| {
                                                        "Unsaved project · no original path".into()
                                                    });
                                                let detail = format!(
                                                    "{source} · autosaved {}",
                                                    format_utc(recovery.saved_utc_unix)
                                                );
                                                ui.add(
                                                    egui::Label::new(
                                                        RichText::new(&detail)
                                                            .text_style(mono_s())
                                                            .color(TEXT_MUTE),
                                                    )
                                                    .truncate(),
                                                )
                                                .on_hover_text(&detail);
                                            });
                                            ui.with_layout(
                                                Layout::right_to_left(Align::Center),
                                                |ui| {
                                                    if ui.button("Discard").clicked() {
                                                        discard_recovery =
                                                            Some(recovery.project_id.clone());
                                                    }
                                                    if ui
                                                        .button("Recover")
                                                        .clicked()
                                                    {
                                                        requested_action = Some(
                                                            project_lifecycle::DeferredProjectAction::Recover(
                                                                recovery.project_id.clone(),
                                                            ),
                                                        );
                                                    }
                                                },
                                            );
                                        });
                                    });
                                ui.add_space(6.0);
                            }
                        }

                        if !first_run {
                            ui.add_space(24.0);
                            ui.label(overline_text("Recent projects"));
                            ui.label(
                                RichText::new(
                                    "Machine-local shortcuts only; portable manifests rely on content hashes, not these paths.",
                                )
                                .text_style(caption())
                                .color(TEXT_MUTE),
                            );
                            ui.add_space(8.0);
                            if recent_projects.is_empty() {
                                ui.label(
                                    RichText::new(
                                        "No recent projects. Save or open a real .reynproj document to add one.",
                                    )
                                    .text_style(caption())
                                    .color(TEXT_MUTE),
                                );
                            }
                            // §4.2: recents as level-0 rows — name
                            // body-strong, path mono-s, opened-when caption,
                            // hover bg-3, no per-row boxes.
                            for recent in &recent_projects {
                                let available = recent.path.is_file();
                                // Reserve a shape slot so the hover wash
                                // paints *behind* the row content.
                                let wash = ui.painter().add(egui::Shape::Noop);
                                let row = Frame::NONE
                                    .inner_margin(Margin::symmetric(10, 8))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width() - 20.0);
                                        ui.horizontal(|ui| {
                                            // P6: cap the name/path column so
                                            // long paths elide instead of
                                            // pushing the row past the frame.
                                            let text_width =
                                                (ui.available_width() - 250.0).max(160.0);
                                            ui.vertical(|ui| {
                                                ui.set_max_width(text_width);
                                                ui.spacing_mut().item_spacing.y = 2.0;
                                                ui.label(
                                                    RichText::new(&recent.name)
                                                        .text_style(body_strong())
                                                        .color(if available {
                                                            TEXT
                                                        } else {
                                                            TEXT_MUTE
                                                        }),
                                                );
                                                let path_text =
                                                    recent.path.display().to_string();
                                                ui.add(
                                                    egui::Label::new(
                                                        RichText::new(&path_text)
                                                            .text_style(mono_s())
                                                            .color(TEXT_MUTE),
                                                    )
                                                    .truncate(),
                                                )
                                                .on_hover_text(&path_text);
                                            });
                                            ui.with_layout(
                                                Layout::right_to_left(Align::Center),
                                                |ui| {
                                                    if ui.button("Remove").clicked() {
                                                        remove_recent = Some(recent.path.clone());
                                                    }
                                                    if ui
                                                        .add_enabled(
                                                            available,
                                                            egui::Button::new("Open"),
                                                        )
                                                        .on_disabled_hover_text(
                                                            "The file no longer exists at this path.",
                                                        )
                                                        .clicked()
                                                    {
                                                        requested_action = Some(
                                                            project_lifecycle::DeferredProjectAction::Open(
                                                                recent.path.clone(),
                                                            ),
                                                        );
                                                    }
                                                    ui.label(
                                                        RichText::new(format!(
                                                            "opened {}",
                                                            format_utc(
                                                                recent.last_opened_utc_unix
                                                            )
                                                        ))
                                                        .text_style(caption())
                                                        .color(TEXT_MUTE),
                                                    );
                                                },
                                            );
                                        });
                                    })
                                    .response;
                                // Hover wash + hairline: the row grammar.
                                if ui.rect_contains_pointer(row.rect) {
                                    ui.painter().set(
                                        wash,
                                        egui::Shape::rect_filled(
                                            row.rect,
                                            CornerRadius::same(R1),
                                            SURFACE_HIGH,
                                        ),
                                    );
                                }
                                ui.painter().hline(
                                    row.rect.x_range(),
                                    row.rect.max.y + 1.0,
                                    Stroke::new(1.0, HAIRLINE),
                                );
                                ui.add_space(2.0);
                            }
                        }
                        ui.add_space(24.0);
                    });
            });

        // Landing drop target (§4.2): dropped STL/STEP geometry starts an import,
        // a dropped .reynproj opens through the same deferred (dirty-guarded)
        // path as the Open dialog. Anything else states why it was refused.
        let dropped: Vec<std::path::PathBuf> = ui.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });
        for path in dropped {
            match path
                .extension()
                .and_then(|extension| extension.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("stl" | "stp" | "step" | "3mf") => self.import_cad_path(path),
                Some("reynproj") => {
                    requested_action = Some(project_lifecycle::DeferredProjectAction::Open(path));
                }
                _ => {
                    self.project_notice = Some((
                        "Only STL, STEP, or 3MF geometry or .reynproj documents can be dropped here.".into(),
                        true,
                    ));
                }
            }
        }

        if save_current {
            self.save_project_dialog();
        }
        if save_as {
            self.save_project_as_dialog();
        }
        if open_dialog {
            self.open_project_dialog(ui.ctx());
        }
        if let Some(project_id) = discard_recovery {
            match self.project.discard_recovery(&project_id) {
                Ok(()) => {
                    self.project_notice = Some(("Recovery snapshot discarded.".into(), false));
                }
                Err(error) => {
                    self.project_notice = Some((
                        format!("Recovery snapshot was not discarded: {error}"),
                        true,
                    ));
                }
            }
        }
        if let Some(path) = remove_recent {
            match self.project.remove_recent(&path) {
                Ok(()) => {
                    self.project_notice = Some(("Recent shortcut removed.".into(), false));
                }
                Err(error) => {
                    self.project_notice =
                        Some((format!("Recent shortcut was not removed: {error}"), true));
                }
            }
        }
        if let Some(action) = requested_action {
            self.request_project_action(action, ui.ctx());
        }
        if let Some(run_id) = inspect_run {
            self.inspect_project_run(&run_id);
        }
        if let Some(digest) = relink_digest {
            self.relink_project_content_dialog(&digest);
        }
    }

    /// Viewport and playback keys. Unmodified single keys, so they are only read
    /// on the 3D screens and never while a text field has the keyboard.
    fn handle_viewport_shortcuts(&mut self, ui: &mut egui::Ui) {
        if !matches!(self.nav, Nav::Results | Nav::Metrics) || ui.ctx().egui_wants_keyboard_input()
        {
            return;
        }
        if self.nav == Nav::Results
            && self
                .cad
                .as_ref()
                .and_then(|case| case.workflow.result.as_ref())
                .is_none()
        {
            return;
        }
        enum Key {
            Fit,
            View(viewport::StandardView),
            PlayPause,
            Step(i64),
        }
        let pressed = ui.input_mut(|input| {
            if input.modifiers.any() {
                return None;
            }
            if input.consume_key(egui::Modifiers::NONE, egui::Key::F) {
                return Some(Key::Fit);
            }
            if input.consume_key(egui::Modifiers::NONE, egui::Key::Space) {
                return Some(Key::PlayPause);
            }
            if input.consume_key(egui::Modifiers::NONE, egui::Key::Comma) {
                return Some(Key::Step(-1));
            }
            if input.consume_key(egui::Modifiers::NONE, egui::Key::Period) {
                return Some(Key::Step(1));
            }
            for (index, view) in viewport::StandardView::ALL.iter().enumerate() {
                let key = match index {
                    0 => egui::Key::Num1,
                    1 => egui::Key::Num2,
                    2 => egui::Key::Num3,
                    3 => egui::Key::Num4,
                    4 => egui::Key::Num5,
                    5 => egui::Key::Num6,
                    _ => egui::Key::Num7,
                };
                if input.consume_key(egui::Modifiers::NONE, key) {
                    return Some(Key::View(*view));
                }
            }
            None
        });
        match pressed {
            Some(Key::Fit) => self.view_fit = true,
            Some(Key::View(view)) => self.view_snap = Some(view),
            Some(Key::PlayPause) => self.toggle_horizon_playback(),
            Some(Key::Step(delta)) => {
                if let Some(case) = self.cad.as_ref() {
                    let current = case.display_step() as i64;
                    self.show_horizon_step((current + delta).max(1) as u32);
                }
            }
            None => {}
        }
    }

    fn toggle_horizon_playback(&mut self) {
        let can_play = self
            .cad
            .as_ref()
            .is_some_and(|case| case.workflow.result.is_some() && !case.pending);
        if !can_play {
            return;
        }
        if let Some(case) = self.cad.as_mut() {
            case.playback.playing = !case.playback.playing;
            case.playback.last_advance = f64::NEG_INFINITY;
        }
    }

    fn handle_project_shortcuts(&mut self, ui: &mut egui::Ui) {
        enum Shortcut {
            New,
            Open,
            Save,
            SaveAs,
            Close,
            Quit,
        }
        let shortcut = ui.input_mut(|input| {
            if input.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::S,
            ) {
                Some(Shortcut::SaveAs)
            } else if input.consume_key(egui::Modifiers::COMMAND, egui::Key::S) {
                Some(Shortcut::Save)
            } else if input.consume_key(egui::Modifiers::COMMAND, egui::Key::O) {
                Some(Shortcut::Open)
            } else if input.consume_key(egui::Modifiers::COMMAND, egui::Key::N) {
                Some(Shortcut::New)
            } else if input.consume_key(egui::Modifiers::COMMAND, egui::Key::W) {
                Some(Shortcut::Close)
            } else if input.consume_key(egui::Modifiers::COMMAND, egui::Key::Q) {
                Some(Shortcut::Quit)
            } else {
                None
            }
        });
        match shortcut {
            Some(Shortcut::New) => {
                self.request_project_action(project_lifecycle::DeferredProjectAction::New, ui.ctx())
            }
            Some(Shortcut::Open) => self.open_project_dialog(ui.ctx()),
            Some(Shortcut::Save) => {
                self.save_project_dialog();
            }
            Some(Shortcut::SaveAs) => {
                self.save_project_as_dialog();
            }
            // Both route through the unsaved-changes guard, never a raw exit.
            Some(Shortcut::Close) | Some(Shortcut::Quit) => self
                .request_project_action(project_lifecycle::DeferredProjectAction::Quit, ui.ctx()),
            None => {}
        }
    }

    fn open_project_dialog(&mut self, ctx: &egui::Context) {
        let mut dialog = rfd::FileDialog::new().add_filter("Reyn project", &["reynproj"]);
        if let Some(directory) = self.project_dialog_directory() {
            dialog = dialog.set_directory(directory);
        }
        let Some(path) = dialog.pick_file() else {
            return;
        };
        self.request_project_action(project_lifecycle::DeferredProjectAction::Open(path), ctx);
    }

    fn save_project_dialog(&mut self) -> bool {
        if self.project.path().is_none() {
            return self.save_project_as_dialog();
        }
        let path = self
            .project
            .path()
            .expect("project path checked above")
            .to_path_buf();
        let orientation_applied = match self.flush_project_drafts_for_persistence() {
            Ok(applied) => applied,
            Err(error) => {
                self.project_notice = Some((
                    format!(
                        "Project was not saved because its visible draft is not durable: {error}"
                    ),
                    true,
                ));
                return false;
            }
        };
        match self.project.save(now_utc_unix()) {
            Ok(warning) => {
                self.project_name_draft = self.project.display_name().to_owned();
                self.project_conflict = None;
                self.schedule_autosave_from_now();
                self.project_notice = Some((
                    warning.unwrap_or_else(|| {
                        format!(
                            "Saved atomically to {}.{}",
                            path.display(),
                            if orientation_applied {
                                " Pending body orientation was applied and recorded before save"
                            } else {
                                ""
                            }
                        )
                    }),
                    false,
                ));
                true
            }
            Err(error) => {
                if is_project_write_conflict(&error) {
                    self.project_conflict = Some(ProjectWriteConflict {
                        path: path.clone(),
                        detail: error.to_string(),
                    });
                }
                self.project_notice = Some((format!("Project was not saved: {error}"), true));
                false
            }
        }
    }

    fn save_project_as_dialog(&mut self) -> bool {
        let mut dialog = rfd::FileDialog::new().add_filter("Reyn project", &["reynproj"]);
        if let Some(directory) = self.project_dialog_directory() {
            dialog = dialog.set_directory(directory);
        }
        if self.project.display_name() != "Unsaved project" {
            dialog = dialog.set_file_name(format!("{}.reynproj", self.project.display_name()));
        }
        let Some(path) = dialog.save_file() else {
            return false;
        };
        let path = with_project_extension(path);
        self.save_project_as_path(&path)
    }

    fn save_project_as_path(&mut self, path: &std::path::Path) -> bool {
        let orientation_applied = match self.flush_project_drafts_for_persistence() {
            Ok(applied) => applied,
            Err(error) => {
                self.project_notice = Some((
                    format!(
                        "Project was not saved as {} because its visible draft is not durable: {error}",
                        path.display()
                    ),
                    true,
                ));
                return false;
            }
        };
        match self.project.save_as(path, now_utc_unix()) {
            Ok(warning) => {
                self.project_name_draft = self.project.display_name().to_owned();
                self.project_conflict = None;
                self.schedule_autosave_from_now();
                self.project_notice = Some((
                    warning.unwrap_or_else(|| {
                        format!(
                            "Saved a new atomic project at {}.{}",
                            path.display(),
                            if orientation_applied {
                                " Pending body orientation was applied and recorded before save"
                            } else {
                                ""
                            }
                        )
                    }),
                    false,
                ));
                true
            }
            Err(error) => {
                if is_project_write_conflict(&error) {
                    self.project_conflict = Some(ProjectWriteConflict {
                        path: path.to_path_buf(),
                        detail: error.to_string(),
                    });
                }
                self.project_notice = Some((
                    format!("Project was not saved as {}: {error}", path.display()),
                    true,
                ));
                false
            }
        }
    }

    fn show_project_conflict_dialog(&mut self, ctx: &egui::Context) {
        let Some(conflict) = self.project_conflict.clone() else {
            return;
        };
        let mut action = None;
        egui::Window::new("Project changed on disk")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .fixed_size(Vec2::new(500.0, 0.0))
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Another writer changed this project after it was opened. Reyn refused to overwrite those bytes.",
                    )
                    .color(TEXT),
                );
                ui.label(
                    RichText::new(&conflict.detail)
                        .text_style(mono_s())
                        .color(TEXT_MUTE),
                );
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Reload opens the disk version and discards this in-memory draft. Save As keeps the draft at a path you choose. Conflict copy writes a timestamped sibling without touching the changed project.",
                    )
                    .text_style(caption())
                    .color(TEXT_DIM),
                );
                ui.add_space(14.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Dismiss").clicked() {
                        action = Some(ProjectConflictAction::Dismiss);
                    }
                    if ui.button("Reload disk version").clicked() {
                        action = Some(ProjectConflictAction::Reload);
                    }
                    if ui.button("Save As…").clicked() {
                        action = Some(ProjectConflictAction::SaveAs);
                    }
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Save conflict copy").color(ON_EMBER),
                            )
                            .fill(EMBER),
                        )
                        .clicked()
                    {
                        action = Some(ProjectConflictAction::ConflictCopy);
                    }
                });
            });
        let resolution = action.map(|action| {
            let unique = if action == ProjectConflictAction::ConflictCopy {
                uuid::Uuid::new_v4().simple().to_string()
            } else {
                String::new()
            };
            resolve_project_conflict_action(action, &conflict.path, now_utc_unix(), unique.as_str())
        });
        match resolution {
            Some(ProjectConflictResolution::Reload(path)) => {
                self.project_conflict = None;
                self.execute_project_action(
                    project_lifecycle::DeferredProjectAction::Open(path),
                    ctx,
                );
            }
            Some(ProjectConflictResolution::PromptSaveAs) => {
                self.save_project_as_dialog();
            }
            Some(ProjectConflictResolution::SaveConflictCopy(path)) => {
                self.save_project_as_path(&path);
            }
            Some(ProjectConflictResolution::Dismiss) => {
                self.project_conflict = None;
            }
            None => {}
        }
    }

    fn relink_project_content_dialog(&mut self, expected_digest: &str) {
        let mut dialog = rfd::FileDialog::new();
        if let Some(directory) = self.project_dialog_directory() {
            dialog = dialog.set_directory(directory);
        }
        let Some(path) = dialog.pick_file() else {
            return;
        };
        match self.project.relink_content(expected_digest, &path) {
            Ok(insert) => {
                self.dependencies_dirty = true;
                self.project_notice = Some((
                    format!(
                        "{} content {} from {}. Immutable runs and evidence records were not changed; save the project to retain the portable object.",
                        if insert.deduplicated {
                            "Verified existing"
                        } else {
                            "Relinked"
                        },
                        short_hash(&insert.content_sha256),
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("selected file")
                    ),
                    false,
                ));
            }
            Err(error) => {
                self.project_notice = Some((
                    format!(
                        "Relink rejected for {}: {error}. The project was not changed.",
                        short_hash(expected_digest)
                    ),
                    true,
                ));
            }
        }
    }

    fn project_dialog_directory(&self) -> Option<std::path::PathBuf> {
        self.project
            .path()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
            .or_else(|| {
                let configured = std::path::PathBuf::from(&self.settings.project_directory);
                configured.is_dir().then_some(configured)
            })
    }

    fn request_project_action(
        &mut self,
        action: project_lifecycle::DeferredProjectAction,
        ctx: &egui::Context,
    ) {
        match self
            .project_guard
            .request(action, self.has_unsaved_project_work())
        {
            project_lifecycle::GuardRequest::Execute(action) => {
                self.execute_project_action(action, ctx);
            }
            project_lifecycle::GuardRequest::ConfirmationRequired => {}
        }
    }

    fn execute_project_action(
        &mut self,
        action: project_lifecycle::DeferredProjectAction,
        ctx: &egui::Context,
    ) {
        match action {
            project_lifecycle::DeferredProjectAction::New => {
                self.project.new_project(now_utc_unix());
                self.project_name_draft = self.project.display_name().to_owned();
                self.reset_project_runtime();
                self.restart_engine_for_project_change();
                self.nav = Nav::Projects;
                self.project_notice = Some((
                    "New local project created. Choose Save to assign its real name and location."
                        .into(),
                    false,
                ));
            }
            project_lifecycle::DeferredProjectAction::Open(path) => {
                match self.project.open(&path, now_utc_unix()) {
                    Ok(warning) => {
                        self.project_name_draft = self.project.display_name().to_owned();
                        self.reset_project_runtime();
                        self.hydrate_project_runtime();
                        self.restart_engine_for_project_change();
                        self.project_notice = Some((
                            warning.unwrap_or_else(|| {
                                format!(
                                    "Opened {} without requiring the compute engine.",
                                    path.display()
                                )
                            }),
                            false,
                        ));
                    }
                    Err(error) => {
                        self.project_notice = Some((
                            format!(
                                "Could not open {}: {error}. The current project was not changed.",
                                path.display()
                            ),
                            true,
                        ));
                        self.nav = Nav::Projects;
                    }
                }
            }
            project_lifecycle::DeferredProjectAction::Recover(project_id) => {
                match self.project.recover(&project_id, now_utc_unix()) {
                    Ok(()) => {
                        self.project_name_draft = self.project.display_name().to_owned();
                        self.reset_project_runtime();
                        self.hydrate_project_runtime();
                        self.restart_engine_for_project_change();
                        self.project_notice = Some((
                            "Recovered unsaved work. The original project has not been overwritten; Save is required."
                                .into(),
                            false,
                        ));
                    }
                    Err(error) => {
                        self.project_notice =
                            Some((format!("Recovery could not be opened: {error}"), true));
                    }
                }
            }
            project_lifecycle::DeferredProjectAction::Quit => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn show_unsaved_changes_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.project_guard.pending() else {
            return;
        };
        let target = match pending {
            project_lifecycle::DeferredProjectAction::New => "create a new project",
            project_lifecycle::DeferredProjectAction::Open(_) => "open another project",
            project_lifecycle::DeferredProjectAction::Recover(_) => {
                "replace this session with recovered work"
            }
            project_lifecycle::DeferredProjectAction::Quit => "quit Reyn Studio",
        };
        let mut decision = None;
        egui::Window::new("Unsaved project changes")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .fixed_size(Vec2::new(430.0, 0.0))
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!(
                        "Save {} before you {target}?",
                        self.project.display_name()
                    ))
                    .size(13.0)
                    .color(TEXT),
                );
                ui.label(
                    RichText::new(
                        "Save writes atomically. Discard removes this session’s recovery snapshot. Cancel leaves everything unchanged.",
                    )
                    .text_style(caption())
                    .color(TEXT_MUTE),
                );
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        decision = Some(project_lifecycle::UnsavedDecision::Cancel);
                    }
                    if ui.button("Discard changes").clicked() {
                        decision = Some(project_lifecycle::UnsavedDecision::Discard);
                    }
                    if ui
                        .add(
                            egui::Button::new(RichText::new("Save").color(ON_EMBER)).fill(EMBER),
                        )
                        .clicked()
                    {
                        decision = Some(project_lifecycle::UnsavedDecision::Save);
                    }
                });
            });
        let Some(decision) = decision else {
            return;
        };
        match self.project_guard.resolve(decision) {
            project_lifecycle::GuardResolution::SaveThen(action) => {
                if self.save_project_dialog() {
                    self.execute_project_action(action, ctx);
                }
            }
            project_lifecycle::GuardResolution::Execute(action) => {
                match self.project.discard_active_recovery() {
                    Ok(()) => self.execute_project_action(action, ctx),
                    Err(error) => {
                        self.project_notice = Some((
                            format!(
                                "Unsaved work was not discarded because its recovery snapshot could not be removed: {error}"
                            ),
                            true,
                        ));
                    }
                }
            }
            project_lifecycle::GuardResolution::Cancelled
            | project_lifecycle::GuardResolution::Idle => {}
        }
    }

    fn invalidate_engine_results(&mut self) {
        self.invalidate_cad_section();
        self.particles.clear();
        self.volume_data = std::sync::Arc::new(vec![0; 8]);
        self.volume_dims = [2, 2, 2];
        self.volume_version = self.volume_version.wrapping_add(1);
        self.q_volume_data = std::sync::Arc::new(Vec::new());
        self.q_volume_dims = [1, 1, 1];
        self.render_volume = false;
        self.q_iso_on = false;
        self.streamlines = false;
        self.insights3d.clear();
        self.f2d = None;
        self.f2d_tex.clear();
        self.f2d_sig = u64::MAX;
        self.f2d_dirty = false;
        self.f2d_pending = false;
        self.f2d_req_at = None;
        self.bench = None;
        self.bench_running = false;
        self.bench_selected = None;
        self.bench_inspector = None;
        self.bench_inspector_pending = false;
        self.bench_error = None;
        self.bench_inspector_error = None;
        self.bench_tex.clear();
        self.active_benchmark_run_id = None;
        if let Some(cad) = &mut self.cad {
            cad.surf = None;
            cad.surf_mask_source = None;
        }
        self.surface_on = false;
    }

    fn reset_project_runtime(&mut self) {
        self.invalidate_engine_results();
        self.project_conflict = None;
        self.case_draft_dirty = false;
        self.review_selection = None;
        self.orientation_draft = None;
        self.orientation_pending = None;
        self.live = false;
        self.live_timer = 0.0;
        self.seed = 1;
        self.current_model = self.settings.default_3d_model.clone();
        self.f2d_model = self.settings.default_2d_model.clone();
        self.f2d_var = FieldVar::Vorticity;
        self.f2d_horizon = 8;
        self.f2d_truth = false;
        self.f2d_method = PMethod::Spectral;
        self.f2d_tol_exp = 5;
        self.f2d_boundary = PBoundary::Periodic;
        self.f2d_scale = 1.0;
        self.f2d_signed = true;
        self.cad = None;
        self.case_draft_history.clear();
        self.case_edit_transaction = None;
        self.f2d_painted = None;
        self.paint = painter::PaintField::default();
        self.paint_tex = None;
        self.paint_dirty = true;
        self.paint_last = None;
        self.bench_seeds = 3;
        self.bench_seed_start = 70000;
        self.bench_var = InspectorVariable::Velocity;
        self.reset_controls();
        self.cam = viewport::Camera::default();
    }

    fn restart_engine_for_project_change(&mut self) {
        // Replacing the handle drops the prior receiver, so an in-flight result
        // from the previous project cannot repopulate a cleared viewport.
        self.engine.terminate();
        self.engine = engine::EngineHandle::spawn_with_config(self.settings.engine_config());
        self.attach_engine_repaint_wake();
        self.engine_ok = false;
        self.dependencies_dirty = true;
        self.schedule_autosave_from_now();
        self.engine_status = "○ Project context changed · revalidating engine…".into();
        self.library.busy = true;
        self.model_inventory_state = ModelInventoryState::Loading;
        self.library_pending_request = self.engine.send(engine::Cmd::ListModels).ok();
    }

    fn prepare_benchmark_case(&mut self, clear_selection: bool) -> Result<String, String> {
        self.project_write_access("Preparing a benchmark run")?;
        let model = self
            .models
            .iter()
            .find(|model| model.id == self.f2d_model)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "model {} is not present in the validated inventory",
                    self.f2d_model
                )
            })?;
        if !is_sha256(&model.checkpoint_sha256) {
            return Err(format!(
                "{} has no valid model-bundle SHA-256; an evidence run cannot be created",
                model.name
            ));
        }
        if model.authenticity_status != "verified" {
            return Err(format!(
                "{} does not have verified publisher authenticity. {}",
                model.name,
                engine::TRUSTED_MODEL_CONVERSION_GUIDANCE
            ));
        }
        let now = now_utc_unix();
        let checkpoint_sha256 = model.checkpoint_sha256.to_ascii_lowercase();
        let model_id_path = std::path::PathBuf::from(&model.id);
        let model_path = if model_id_path.is_absolute() {
            model_id_path
        } else {
            std::path::PathBuf::from(&self.settings.research_dir).join(model_id_path)
        };
        engine::require_model_signature(&model_path).map_err(|error| {
            format!(
                "model bundle {} cannot be used without its detached signature: {error}",
                model.name
            )
        })?;
        let existing_source = self
            .project
            .manifest()
            .source_by_digest(&checkpoint_sha256)
            .cloned();
        let bundled_model_bytes = if self.project.content_bytes(&checkpoint_sha256).is_some() {
            None
        } else {
            let bytes = std::fs::read(&model_path).map_err(|error| {
                format!(
                    "model bundle {} cannot be copied from {}: {error}",
                    model.name,
                    model_path.display()
                )
            })?;
            let actual = project_sha256(&bytes);
            if actual != checkpoint_sha256 {
                return Err(format!(
                    "model bundle {} changed after validation: expected {}, received {}",
                    model.name,
                    short_hash(&checkpoint_sha256),
                    short_hash(&actual)
                ));
            }
            Some(bytes)
        };
        let source_id = existing_source
            .as_ref()
            .map(|source| source.source_revision_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let parent_source = self
            .project
            .manifest()
            .source_revisions()
            .iter()
            .rev()
            .find(|source| {
                source.source_kind == project::SourceKind::Model
                    && source.uri_hint.as_deref() == Some(model.name.as_str())
            })
            .cloned();
        let case_id = self
            .project
            .manifest()
            .case_by_contract_kind("benchmark_suite")
            .map(|case| case.case_id().to_owned())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let contract = serde_json::json!({
            "kind": "benchmark_suite",
            "physics_contract": model.physics_contract,
            "model_id": model.id,
            "model_checkpoint_sha256": checkpoint_sha256,
            "model_source_sha256": model.source_digest,
            "model_authenticity_status": model.authenticity_status,
            "model_publisher_key_id": model.publisher_key_id,
            "model_publisher_key_sha256": model.publisher_key_sha256,
            "model_release_sequence": model.release_sequence,
            "dimension": model.dimension,
            "input_channels": model.in_channels,
            "output_channels": model.out_channels,
            "maximum_supported_horizon": model.max_steps,
            "reference_semantics": "numerical solver reference",
        });
        let discretization = serde_json::json!({
            "grid": model.grid,
            "dimension": model.dimension,
            "domain": INSPECTOR_DOMAIN,
        });
        let horizons = vec![1_u32, 4, 8, 16];
        let seeds: Vec<u32> = (0..self.bench_seeds)
            .map(|offset| self.bench_seed_start.saturating_add(offset))
            .collect();
        let outputs = serde_json::json!({
            "seeds": seeds,
            "horizons": horizons,
            "scalars": ["relative_l2", "persistence_relative_l2", "improvement_ratio"],
            "selected_cell_evidence_schema": INSPECTOR_SCHEMA,
        });
        let checkpoint_sha256_for_source = checkpoint_sha256.clone();
        let new_source = existing_source.is_none().then(|| {
            let mut warnings = model.unknown_fields.clone();
            if model.status != "clean" && !model.status_detail.trim().is_empty() {
                warnings.push(model.status_detail.clone());
            }
            warnings.sort();
            warnings.dedup();
            project::SourceRevision {
                source_revision_id: source_id.clone(),
                source_kind: project::SourceKind::Model,
                revision: parent_source
                    .as_ref()
                    .map_or(1, |parent| parent.revision.saturating_add(1)),
                imported_utc_unix: now,
                uri_hint: Some(model.name.clone()),
                byte_size: model.size_bytes,
                content_sha256: checkpoint_sha256_for_source,
                declared_units: None,
                frame: None,
                transform_4x4: [
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
                ],
                parent_revision_id: parent_source.map(|parent| parent.source_revision_id),
                warnings,
            }
        });
        let existing_case = self.project.manifest().case(&case_id).cloned();
        let revision_id = uuid::Uuid::new_v4().to_string();
        let source_id_for_edit = source_id.clone();
        let case_id_for_edit = case_id.clone();
        self.transact_project("Preparing a benchmark run", now, move |manifest| {
            if let Some(source) = new_source {
                manifest.add_source_revision(source, now)?;
            }
            if let Some(case) = existing_case {
                let active = case.active_revision();
                if active.source_revision_ids != [source_id_for_edit.clone()]
                    || active.contract != contract
                    || active.discretization != discretization
                    || active.outputs != outputs
                {
                    manifest.append_case_revision(
                        &case_id_for_edit,
                        project::CaseRevision {
                            case_revision_id: revision_id,
                            parent_revision_id: Some(active.case_revision_id.clone()),
                            created_utc_unix: now,
                            source_revision_ids: vec![source_id_for_edit],
                            contract,
                            discretization,
                            outputs,
                        },
                        now,
                    )?;
                }
            } else {
                manifest.create_case(
                    case_id_for_edit.clone(),
                    "Model qualification",
                    project::CaseRevision {
                        case_revision_id: revision_id,
                        parent_revision_id: None,
                        created_utc_unix: now,
                        source_revision_ids: vec![source_id_for_edit],
                        contract,
                        discretization,
                        outputs,
                    },
                    now,
                )?;
            }
            if clear_selection {
                manifest.set_selection(
                    project::ProjectSelection {
                        active_case_id: Some(case_id_for_edit.clone()),
                        ..Default::default()
                    },
                    now,
                )?;
            }
            Ok(())
        })?;
        if let Some(bytes) = bundled_model_bytes {
            self.add_project_content(
                "Preparing a benchmark run",
                bytes,
                "application/vnd.reyn.model-bundle",
                &checkpoint_sha256,
            )
            .map_err(|error| format!("model bundle: {error}"))?;
        }
        self.dependencies_dirty = true;
        Ok(case_id)
    }

    fn persist_benchmark_run(&mut self, benchmark: &engine::BenchResult) -> Result<String, String> {
        self.project_write_access("Recording a benchmark run")?;
        let case_id = self.prepare_benchmark_case(false)?;
        let now = now_utc_unix();
        let model = self
            .models
            .iter()
            .find(|model| model.id == benchmark.model)
            .cloned()
            .ok_or_else(|| format!("model {} left the validated inventory", benchmark.model))?;
        let case = self
            .project
            .manifest()
            .case(&case_id)
            .cloned()
            .ok_or_else(|| "benchmark case was not created".to_string())?;
        let active_revision = case.active_revision().clone();
        let parent = case.latest_run_for_active_revision().cloned();
        let snapshot =
            serde_json::to_value(benchmark).map_err(|error| format!("suite snapshot: {error}"))?;
        let snapshot_bytes =
            serde_json::to_vec(&snapshot).map_err(|error| format!("suite snapshot: {error}"))?;
        let output_sha256 = project_sha256(&snapshot_bytes);
        let mut scalar_outputs = vec![project::ScalarOutput {
            key: "global_relative_l2".into(),
            value: benchmark.global_rel as f64,
            units: "dimensionless".into(),
            abs_tolerance: 1e-6,
        }];
        for (seed_index, seed) in benchmark.seeds.iter().enumerate() {
            for (horizon_index, horizon) in benchmark.horizons.iter().enumerate() {
                scalar_outputs.push(project::ScalarOutput {
                    key: format!("seed_{seed}.horizon_{horizon}.relative_l2"),
                    value: benchmark.rel[seed_index][horizon_index] as f64,
                    units: "dimensionless".into(),
                    abs_tolerance: 1e-6,
                });
                scalar_outputs.push(project::ScalarOutput {
                    key: format!("seed_{seed}.horizon_{horizon}.persistence_relative_l2"),
                    value: benchmark.persist[seed_index][horizon_index] as f64,
                    units: "dimensionless".into(),
                    abs_tolerance: 1e-6,
                });
            }
        }
        let mut warnings = benchmark.provenance.flags.clone();
        warnings.extend(
            benchmark
                .provenance
                .legacy_unknown
                .iter()
                .map(|field| format!("UNKNOWN provenance field: {field}")),
        );
        if benchmark.provenance.verdict == "unknown" {
            warnings.push(
                "RNG-stream collision status is UNKNOWN because required metadata is missing"
                    .into(),
            );
        }
        warnings.sort();
        warnings.dedup();
        let comparison_max = benchmark
            .rel
            .iter()
            .flatten()
            .chain(benchmark.persist.iter().flatten())
            .copied()
            .fold(1e-6_f32, f32::max) as f64;
        let suite_view = project::CalibratedView {
            view_id: "benchmark.relative_l2.compare".into(),
            quantity: "model and persistence relative L2 error".into(),
            units: "dimensionless".into(),
            scale_min: 0.0,
            scale_max: comparison_max,
            source_class: project::EvidenceSourceClass::Derived,
            method: "shared scale over model-vs-solver-reference and persistence errors".into(),
        };
        let run_id = uuid::Uuid::new_v4().to_string();
        let evidence_id = uuid::Uuid::new_v4().to_string();
        let parent_run_id = parent.as_ref().map(|run| run.run_id().to_owned());
        let mut run_manifest = project::RunManifest {
            schema_version: project::PROJECT_SCHEMA_VERSION,
            app: project::VersionedComponent {
                name: "Reyn Studio".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                sha256: None,
            },
            engine: Some(project::VersionedComponent {
                name: "Reyn Python engine".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                sha256: None,
            }),
            model: Some(project::VersionedComponent {
                name: model.name.clone(),
                version: format!("epoch {}", benchmark.epoch),
                sha256: Some(model.checkpoint_sha256.to_ascii_lowercase()),
            }),
            solver: Some(project::VersionedComponent {
                name: "numerical solver reference".into(),
                version: "engine-declared benchmark protocol".into(),
                sha256: None,
            }),
            converter: None,
            exact_contract: active_revision.contract.clone(),
            exact_settings: serde_json::json!({
                "seeds": benchmark.seeds,
                "horizons": benchmark.horizons,
                "grid": benchmark.grid,
                "dt_frame": benchmark.dt_frame,
            }),
            seeds: benchmark.seeds.iter().map(|seed| *seed as u64).collect(),
            device: self.settings.compute_device.engine_value().into(),
            runtime_ms: (benchmark.runtime_s.max(0.0) * 1000.0).round() as u64,
            stop_reason: "complete".into(),
            warnings: warnings.clone(),
            waivers: Vec::new(),
            missing_dependencies: Vec::new(),
            output_sha256: vec![output_sha256.clone()],
            scalar_outputs,
            determinism: None,
        };
        if let Some(parent) = &parent {
            run_manifest.compare_scalars_against(parent);
        }
        let run = project::RunRecord::new(
            run_id.clone(),
            parent_run_id,
            active_revision.case_revision_id,
            now,
            now,
            project::LifecycleState::Succeeded,
            run_manifest,
            vec![suite_view.clone()],
        );
        let evidence = project::EvidenceArtifact {
            evidence_id: evidence_id.clone(),
            run_ids: vec![run_id.clone()],
            created_utc_unix: now,
            source_class: project::EvidenceSourceClass::Derived,
            media_type: "application/vnd.reyn.benchmark-suite+json".into(),
            byte_size: snapshot_bytes.len() as u64,
            content_sha256: output_sha256.clone(),
            derivation_method: Some("seed_horizon_benchmark_suite".into()),
            derivation_version: Some("1".into()),
            warnings,
            metadata: serde_json::json!({
                "kind": "benchmark_suite",
                "snapshot": snapshot,
                "reference_semantics": "numerical solver reference",
                "integrity_only": true,
            }),
            calibrated_views: vec![suite_view.clone()],
        };
        let selected_view_id = suite_view.view_id;
        let case_id_for_edit = case_id.clone();
        let run_id_for_edit = run_id.clone();
        self.transact_project("Recording a benchmark run", now, move |manifest| {
            manifest.append_run(&case_id_for_edit, run, now)?;
            manifest.append_evidence(evidence, now)?;
            manifest.set_selection(
                project::ProjectSelection {
                    active_case_id: Some(case_id_for_edit),
                    selected_run_id: Some(run_id_for_edit),
                    selected_evidence_id: Some(evidence_id),
                    selected_view_id: Some(selected_view_id),
                },
                now,
            )?;
            Ok(())
        })?;
        self.add_project_content(
            "Recording a benchmark run",
            snapshot_bytes,
            "application/vnd.reyn.benchmark-suite+json",
            &output_sha256,
        )
        .map_err(|error| format!("suite artifact bundle: {error}"))?;
        self.dependencies_dirty = true;
        Ok(run_id)
    }

    fn persist_benchmark_inspector(
        &mut self,
        inspector: &engine::BenchInspector,
    ) -> Result<(), String> {
        self.project_write_access("Recording benchmark cell evidence")?;
        let run_id = self
            .active_benchmark_run_id
            .clone()
            .or_else(|| self.project.manifest().selection().selected_run_id.clone())
            .ok_or_else(|| "no immutable benchmark run is selected".to_string())?;
        let case_id = self
            .project
            .manifest()
            .cases()
            .iter()
            .find(|case| case.runs().iter().any(|run| run.run_id() == run_id))
            .map(|case| case.case_id().to_owned())
            .ok_or_else(|| format!("selected run {run_id} is not in this project"))?;
        let snapshot =
            serde_json::to_value(inspector).map_err(|error| format!("cell snapshot: {error}"))?;
        let snapshot_bytes =
            serde_json::to_vec(&snapshot).map_err(|error| format!("cell snapshot: {error}"))?;
        let mut calibrated_views = Vec::new();
        for variable in InspectorVariable::ALL {
            let Some((comparison_scale, error_scale)) = inspector.maps.scales(variable) else {
                continue;
            };
            let comparison_min = if variable.signed() {
                -(comparison_scale as f64)
            } else {
                0.0
            };
            let error_min = if variable.signed() {
                -(error_scale as f64)
            } else {
                0.0
            };
            calibrated_views.extend([
                project::CalibratedView {
                    view_id: format!("benchmark.{}.model", variable.key()),
                    quantity: variable.label().into(),
                    units: variable.unit_label().into(),
                    scale_min: comparison_min,
                    scale_max: comparison_scale as f64,
                    source_class: match variable {
                        InspectorVariable::Velocity => {
                            project::EvidenceSourceClass::ModelPrediction
                        }
                        InspectorVariable::Pressure => project::EvidenceSourceClass::Recovered,
                        InspectorVariable::Vorticity | InspectorVariable::Divergence => {
                            project::EvidenceSourceClass::Derived
                        }
                    },
                    method: variable.method_note().into(),
                },
                project::CalibratedView {
                    view_id: format!("benchmark.{}.solver_reference", variable.key()),
                    quantity: variable.label().into(),
                    units: variable.unit_label().into(),
                    scale_min: comparison_min,
                    scale_max: comparison_scale as f64,
                    source_class: match variable {
                        InspectorVariable::Velocity => {
                            project::EvidenceSourceClass::SolverReference
                        }
                        InspectorVariable::Pressure => project::EvidenceSourceClass::Recovered,
                        InspectorVariable::Vorticity | InspectorVariable::Divergence => {
                            project::EvidenceSourceClass::Derived
                        }
                    },
                    method: variable.method_note().into(),
                },
                project::CalibratedView {
                    view_id: format!("benchmark.{}.error", variable.key()),
                    quantity: format!("{} error", variable.label()),
                    units: variable.unit_label().into(),
                    scale_min: error_min,
                    scale_max: error_scale as f64,
                    source_class: project::EvidenceSourceClass::Derived,
                    method: variable.method_note().into(),
                },
            ]);
        }
        let evidence_id = uuid::Uuid::new_v4().to_string();
        let selected_view_id = format!("benchmark.{}.model", self.bench_var.key());
        let snapshot_sha256 = project_sha256(&snapshot_bytes);
        let evidence = project::EvidenceArtifact {
            evidence_id: evidence_id.clone(),
            run_ids: vec![run_id.clone()],
            created_utc_unix: now_utc_unix(),
            source_class: project::EvidenceSourceClass::Derived,
            media_type: "application/vnd.reyn.benchmark-cell+json".into(),
            byte_size: snapshot_bytes.len() as u64,
            content_sha256: snapshot_sha256.clone(),
            derivation_method: Some("selected_cell_spatial_and_spectral_evidence".into()),
            derivation_version: Some(INSPECTOR_PROTOCOL_VERSION.to_string()),
            warnings: Vec::new(),
            metadata: serde_json::json!({
                "kind": "benchmark_inspector",
                "snapshot": snapshot,
                "seed": inspector.seed,
                "horizon": inspector.horizon,
                "selected_variable": self.bench_var.key(),
                "reference_semantics": "numerical solver reference",
            }),
            calibrated_views,
        };
        let now = now_utc_unix();
        self.transact_project("Recording benchmark cell evidence", now, move |manifest| {
            manifest.append_evidence(evidence, now)?;
            manifest.set_selection(
                project::ProjectSelection {
                    active_case_id: Some(case_id),
                    selected_run_id: Some(run_id),
                    selected_evidence_id: Some(evidence_id),
                    selected_view_id: Some(selected_view_id),
                },
                now,
            )?;
            Ok(())
        })?;
        self.add_project_content(
            "Recording benchmark cell evidence",
            snapshot_bytes,
            "application/vnd.reyn.benchmark-cell+json",
            &snapshot_sha256,
        )
        .map_err(|error| format!("selected-cell artifact bundle: {error}"))?;
        self.dependencies_dirty = true;
        Ok(())
    }

    fn stored_artifact_snapshot(
        &self,
        artifact: &project::EvidenceArtifact,
    ) -> Result<serde_json::Value, String> {
        match self.project.content_state(&artifact.content_sha256) {
            project::ContentState::Available => {
                let bytes = self
                    .project
                    .content_bytes(&artifact.content_sha256)
                    .ok_or_else(|| "verified bundle object became unavailable".to_string())?;
                serde_json::from_slice(bytes).map_err(|error| {
                    format!(
                        "verified artifact {} is not valid JSON: {error}",
                        short_hash(&artifact.content_sha256)
                    )
                })
            }
            project::ContentState::Corrupt => Err(format!(
                "artifact {} failed content verification; its immutable manifest remains inspectable",
                short_hash(&artifact.content_sha256)
            )),
            project::ContentState::Missing => artifact
                .metadata
                .get("snapshot")
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "artifact {} is missing from the portable bundle",
                        short_hash(&artifact.content_sha256)
                    )
                }),
        }
    }

    fn hydrate_engineering_run(&mut self, run_id: &str) -> Result<bool, String> {
        let Some((case_record, run)) = self.project.manifest().cases().iter().find_map(|case| {
            case.runs()
                .iter()
                .find(|run| run.run_id() == run_id)
                .map(|run| (case.clone(), run.clone()))
        }) else {
            return Ok(false);
        };
        let contract = run.manifest().exact_contract.clone();
        if contract.get("kind").and_then(serde_json::Value::as_str)
            != Some(engineering::EXTERNAL_FLOW_CONTRACT)
        {
            return Ok(false);
        }
        let result_artifact = self
            .project
            .manifest()
            .evidence_for_run(run_id)
            .into_iter()
            .rev()
            .find(|artifact| {
                artifact
                    .metadata
                    .get("schema")
                    .and_then(serde_json::Value::as_str)
                    == Some(engineering::ENGINEERING_RESULT_SCHEMA)
            })
            .cloned()
            .ok_or_else(|| {
                format!(
                    "Engineering run {} has no result evidence artifact.",
                    short_id(run_id)
                )
            })?;
        let summary = self.stored_artifact_snapshot(&result_artifact)?;
        let field_sha256 = summary
            .get("field_sha256")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                "Engineering result does not identify its field artifact.".to_string()
            })?;
        let field_bytes = self.project.content_bytes(field_sha256).ok_or_else(|| {
            format!(
                "Engineering field {} is unavailable; the run remains readable from its manifest.",
                short_hash(field_sha256)
            )
        })?;
        let field = engineering::decode_engineering_field(field_bytes)?;
        let operating: engineering::OperatingPoint = serde_json::from_value(
            contract
                .get("operating_point")
                .cloned()
                .ok_or_else(|| "Engineering contract has no operating point.".to_string())?,
        )
        .map_err(|error| format!("Engineering operating point is invalid: {error}"))?;
        let preflight: engineering::GeometryPreflight = serde_json::from_value(
            contract
                .get("preflight")
                .cloned()
                .ok_or_else(|| "Engineering contract has no preflight record.".to_string())?,
        )
        .map_err(|error| format!("Engineering preflight is invalid: {error}"))?;
        let model = contract
            .get("model")
            .ok_or_else(|| "Engineering contract has no model record.".to_string())?;
        let model_id = model
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let model_support: engineering::ModelSupport =
            serde_json::from_value(model.get("support").cloned().unwrap_or_default())
                .unwrap_or_default();
        let source_revision_id = contract
            .get("source_revision_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let source = source_revision_id.as_deref().and_then(|source_id| {
            self.project
                .manifest()
                .source_revisions()
                .iter()
                .find(|source| source.source_revision_id == source_id)
        });
        let source_name = source
            .and_then(|source| source.uri_hint.as_deref())
            .and_then(|hint| std::path::Path::new(hint).file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("stored_geometry.stl")
            .to_string();
        let vec3 = |key: &str| -> [f64; 3] {
            serde_json::from_value(summary.get(key).cloned().unwrap_or_default())
                .unwrap_or([0.0; 3])
        };
        let warnings = summary
            .get("warnings")
            .and_then(serde_json::Value::as_array)
            .map(|warnings| {
                warnings
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let cp_min = field.cp.iter().copied().fold(f32::INFINITY, f32::min) as f64;
        let cp_max = field.cp.iter().copied().fold(f32::NEG_INFINITY, f32::max) as f64;
        let result = engineering::EngineeringResult {
            method: summary
                .get("method")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(engineering::SURFACE_LOAD_METHOD)
                .into(),
            cp_min,
            cp_max,
            force_coefficients: vec3("force_coefficients"),
            moment_coefficients: vec3("moment_coefficients"),
            force_newtons: vec3("force_newtons"),
            moment_newton_meters: vec3("moment_newton_meters"),
            surface_area_m2: summary
                .get("surface_area_m2")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
            pressure_force_fraction: summary
                .get("pressure_force_fraction")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
            load_hotspot: vec3("load_hotspot_m"),
            suction_hotspot: vec3("suction_hotspot_m"),
            divergence_rms: summary
                .get("divergence_rms")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
            wake_deficit_peak: summary
                .get("wake_deficit_peak")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
            wake_deficit_mean: summary
                .get("wake_deficit_mean")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
            moment_origin_mode: summary
                .get("moment_origin_mode")
                .or_else(|| summary.get("moment_reference"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("diffuse_surface_centroid")
                .into(),
            moment_origin_solver: vec3("moment_origin_solver"),
            surface_centroid_solver: vec3("surface_centroid_solver"),
            coefficient_reference: summary
                .get("coefficient_reference")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("q_inf * L_ref^2 ; q_inf * L_ref^3")
                .into(),
            reference_area_m2: summary
                .get("reference_area_m2")
                .and_then(serde_json::Value::as_f64),
            semigroup: summary.get("semigroup").and_then(serde_json::Value::as_f64),
            warnings,
        };
        let shape = [3usize, field.n, field.n, field.n];
        self.particles = flow::from_field(&shape, &field.velocity);
        if let Some((volume, dims)) = flow::vorticity_volume(&shape, &field.velocity) {
            self.volume_data = std::sync::Arc::new(volume);
            self.volume_dims = dims;
            self.volume_version = self.volume_version.wrapping_add(1);
        }
        if let Some((q_vol, q_dims)) = flow::q_criterion_volume(&shape, &field.velocity) {
            self.q_volume_data = std::sync::Arc::new(q_vol);
            self.q_volume_dims = q_dims;
            self.volume_version = self.volume_version.wrapping_add(1);
        }
        let mut insights = flow::insights3d(&shape, &field.velocity);
        insights.extend(cad::surface_insights(&field.mask, &field.cp, field.n));
        self.insights3d = insights;
        let cp_scale = field
            .cp
            .iter()
            .fold(0.0f32, |scale, value| scale.max(value.abs()))
            .max(1e-6);
        let mut mask_u8 = vec![0u8; field.n * field.n * field.n];
        let mut cp_u8 = vec![128u8; field.n * field.n * field.n];
        for i in 0..field.n {
            for j in 0..field.n {
                for k in 0..field.n {
                    let source_index = i * field.n * field.n + j * field.n + k;
                    let target_index = (k * field.n + j) * field.n + i;
                    mask_u8[target_index] =
                        cad_field_ready::surface_mask_u8_sample(field.mask[source_index]);
                    let normalized = (field.cp[source_index] / cp_scale) * 0.5 + 0.5;
                    cp_u8[target_index] = (normalized.clamp(0.0, 1.0) * 255.0) as u8;
                }
            }
        }
        self.cad_version = self.cad_version.wrapping_add(1);
        let surface = gpu::SurfaceData {
            mask: std::sync::Arc::new(mask_u8),
            pressure: std::sync::Arc::new(cp_u8),
            dims: [field.n as u32; 3],
            mask_version: self.cad_version,
            pressure_version: self.cad_version,
        };
        let workflow = engineering::ExternalFlowCase {
            stage: engineering::CaseStage::Results,
            case_id: case_record.case_id().to_owned(),
            name: case_record.name().to_owned(),
            source_name: source_name.clone(),
            source_revision_id,
            case_revision_id: Some(run.case_revision_id().to_owned()),
            model_id: model_id.clone(),
            model_sha256: model
                .get("sha256")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            model_max_steps: model
                .get("max_steps")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32,
            model_support,
            preflight,
            operating,
            review_focus: serde_json::from_value(
                contract
                    .get("review_focus")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            )
            .unwrap_or_default(),
            result: Some(result),
            parent_run_id: run.parent_run_id().map(str::to_owned),
            named_regions: serde_json::from_value(
                contract
                    .get("named_regions")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!([])),
            )
            .unwrap_or_default(),
            view_state: serde_json::from_value(
                contract
                    .get("view_state")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            )
            .unwrap_or_default(),
        };
        let horizon = summary
            .get("horizon")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(workflow.operating.horizon_steps as u64) as u32;
        self.invalidate_cad_section();
        let mask_bounds = cad::mask_bounds(&field.mask, field.n);
        let mask = std::sync::Arc::new(field.mask);
        self.orientation_pending = None;
        self.cad = Some(CadCase {
            mask: mask.clone(),
            mask_bounds,
            model: model_id,
            steps: horizon,
            surf: Some(surface),
            surf_mask_source: Some(mask),
            name: source_name,
            workflow,
            velocity: field.velocity,
            pressure: field.pressure_pa,
            cp: field.cp,
            traction: field.traction_pa,
            result_grid: field.n,
            dt_frame: summary
                .get("dt_frame")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0) as f32,
            active_run_id: Some(run_id.to_owned()),
            pending: false,
            pending_request_id: None,
            pending_run: None,
            playback: HorizonPlayback::default(),
        });
        self.apply_case_view_state_from_active();
        self.rebase_case_draft_history();
        self.surface_on = true;
        self.streamlines = true;
        self.q_iso_on = false;
        self.volumetric = true;
        self.render_volume = false;
        self.slice = [false, false, false];
        self.nav = Nav::Results;
        Ok(true)
    }

    fn hydrate_external_flow_runtime(&mut self, selection: &project::ProjectSelection) -> bool {
        let snapshot = {
            let Some(case_record) = self.project.manifest().cases().iter().find(|case| {
                case.active_revision()
                    .contract
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    == Some(engineering::EXTERNAL_FLOW_CONTRACT)
            }) else {
                return false;
            };
            let revision = case_record.active_revision();
            let Some(source_id) = revision.source_revision_ids.first() else {
                return false;
            };
            let Some(source) = self
                .project
                .manifest()
                .source_revisions()
                .iter()
                .find(|source| source.source_revision_id == *source_id)
            else {
                return false;
            };
            let source_bytes = self
                .project
                .content_bytes(&source.content_sha256)
                .map(<[u8]>::to_vec);
            let selected_run_id = selection
                .selected_run_id
                .as_ref()
                .filter(|run_id| case_record.runs().iter().any(|run| run.run_id() == *run_id))
                .cloned()
                .or_else(|| case_record.runs().last().map(|run| run.run_id().to_owned()));
            let result_metadata = selected_run_id.as_deref().and_then(|run_id| {
                self.project
                    .manifest()
                    .evidence_for_run(run_id)
                    .into_iter()
                    .rev()
                    .find(|artifact| {
                        artifact
                            .metadata
                            .get("schema")
                            .and_then(serde_json::Value::as_str)
                            == Some(engineering::ENGINEERING_RESULT_SCHEMA)
                    })
                    .map(|artifact| artifact.metadata.clone())
            });
            (
                case_record.case_id().to_owned(),
                case_record.name().to_owned(),
                revision.case_revision_id.clone(),
                revision.contract.clone(),
                source.clone(),
                source_bytes,
                selected_run_id,
                result_metadata,
            )
        };
        let (
            case_id,
            case_name,
            case_revision_id,
            contract,
            source,
            source_bytes,
            selected_run_id,
            result_metadata,
        ) = snapshot;
        let Some(source_bytes) = source_bytes else {
            self.project_notice = Some((
                format!(
                    "External-flow case {} is available in read-only evidence mode, but geometry object {} must be relinked.",
                    case_name,
                    short_hash(&source.content_sha256)
                ),
                true,
            ));
            return false;
        };
        let preflight = match serde_json::from_value::<engineering::GeometryPreflight>(
            contract
                .get("preflight")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ) {
            Ok(preflight) => preflight,
            Err(error) => {
                self.project_notice = Some((
                    format!("Stored external-flow preflight is malformed: {error}"),
                    true,
                ));
                return false;
            }
        };
        let operating = match serde_json::from_value::<engineering::OperatingPoint>(
            contract
                .get("operating_point")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ) {
            Ok(operating) => operating,
            Err(error) => {
                self.project_notice = Some((
                    format!("Stored external-flow operating point is malformed: {error}"),
                    true,
                ));
                return false;
            }
        };
        let model = contract.get("model").cloned().unwrap_or_default();
        let model_id = model
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let model_sha256 = model
            .get("sha256")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let model_max_steps = model
            .get("max_steps")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        let model_support = serde_json::from_value::<engineering::ModelSupport>(
            model
                .get("support")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .unwrap_or_default();
        let result = result_metadata.as_ref().map(|metadata| {
            let vector = |key: &str| -> [f64; 3] {
                std::array::from_fn(|axis| {
                    metadata
                        .get(key)
                        .and_then(|value| value.get(axis))
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0)
                })
            };
            engineering::EngineeringResult {
                method: metadata
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(engineering::SURFACE_LOAD_METHOD)
                    .into(),
                cp_min: metadata
                    .pointer("/cp/minimum")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0),
                cp_max: metadata
                    .pointer("/cp/maximum")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0),
                force_coefficients: vector("force_coefficients"),
                moment_coefficients: vector("moment_coefficients"),
                force_newtons: vector("force_newtons"),
                moment_newton_meters: vector("moment_newton_meters"),
                surface_area_m2: metadata
                    .get("surface_area_m2")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0),
                pressure_force_fraction: metadata
                    .get("pressure_force_fraction")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0),
                load_hotspot: vector("load_hotspot_m"),
                suction_hotspot: vector("suction_hotspot_m"),
                divergence_rms: metadata
                    .get("divergence_rms")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0),
                wake_deficit_peak: metadata
                    .get("wake_deficit_peak")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0),
                wake_deficit_mean: metadata
                    .get("wake_deficit_mean")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0),
                moment_origin_mode: metadata
                    .get("moment_origin_mode")
                    .or_else(|| metadata.get("moment_reference"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("diffuse_surface_centroid")
                    .into(),
                moment_origin_solver: vector("moment_origin_solver"),
                surface_centroid_solver: vector("surface_centroid_solver"),
                coefficient_reference: metadata
                    .get("coefficient_reference")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("q_inf * L_ref^2 ; q_inf * L_ref^3")
                    .into(),
                reference_area_m2: metadata
                    .get("reference_area_m2")
                    .and_then(serde_json::Value::as_f64),
                semigroup: metadata
                    .get("semigroup")
                    .and_then(serde_json::Value::as_f64),
                warnings: metadata
                    .get("warnings")
                    .and_then(serde_json::Value::as_array)
                    .map(|warnings| {
                        warnings
                            .iter()
                            .filter_map(|warning| warning.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        });
        let field_blob = result_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("field_sha256"))
            .and_then(serde_json::Value::as_str)
            .and_then(|digest| self.project.content_bytes(digest))
            .and_then(|bytes| engineering::decode_engineering_field(bytes).ok());
        let dt_frame = result_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("dt_frame"))
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0) as f32;
        let source_sha256 = source.content_sha256.clone();
        let workflow = engineering::ExternalFlowCase {
            stage: if result.is_some() {
                engineering::CaseStage::Results
            } else {
                engineering::CaseStage::Setup
            },
            case_id,
            name: case_name,
            source_name: source
                .uri_hint
                .as_deref()
                .and_then(|path| std::path::Path::new(path).file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("stored_geometry.stl")
                .into(),
            source_revision_id: Some(source.source_revision_id),
            case_revision_id: Some(case_revision_id),
            model_id: model_id.clone(),
            model_sha256,
            model_max_steps,
            model_support,
            preflight,
            operating,
            review_focus: serde_json::from_value(
                contract
                    .get("review_focus")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            )
            .unwrap_or_default(),
            result,
            parent_run_id: selected_run_id.clone(),
            named_regions: serde_json::from_value(
                contract
                    .get("named_regions")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!([])),
            )
            .unwrap_or_default(),
            view_state: serde_json::from_value(
                contract
                    .get("view_state")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            )
            .unwrap_or_default(),
        };
        let Some(field) = field_blob else {
            if self.orientation_worker.is_none() {
                self.orientation_worker =
                    match OrientationWorker::spawn(self.repaint_context.clone()) {
                        Ok(worker) => Some(worker),
                        Err(error) => {
                            self.project_notice = Some((error, true));
                            return false;
                        }
                    };
            }
            self.orientation_generation = self.orientation_generation.wrapping_add(1).max(1);
            let generation = self.orientation_generation;
            let request_id = format!(
                "orientation-hydrate-{generation}-{}",
                uuid::Uuid::new_v4().simple()
            );
            let angles = workflow.preflight.body_orientation_degrees();
            let request = OrientationWorkRequest {
                generation,
                request_id: request_id.clone(),
                case_id: workflow.case_id.clone(),
                source_sha256: source_sha256.clone(),
                source_name: workflow.source_name.clone(),
                angles,
                grid: workflow.preflight.target_grid,
                source_bytes,
                selected_shell_entity_id: workflow.preflight.selected_shell_entity_id,
            };
            if self
                .orientation_worker
                .as_ref()
                .expect("orientation worker was initialized")
                .request_tx
                .send(request)
                .is_err()
            {
                self.orientation_worker = None;
                self.project_notice = Some((
                    "Stored case geometry was not queued because the reconstruction worker stopped. The project manifest and immutable evidence remain available."
                        .into(),
                    true,
                ));
                return false;
            }
            self.orientation_pending = Some(PendingOrientation {
                generation,
                request_id: request_id.clone(),
                case_id: workflow.case_id.clone(),
                source_sha256,
                angles,
                started_at: std::time::Instant::now(),
                kind: PendingOrientationKind::Hydrate(Box::new(PendingOrientationHydration {
                    workflow,
                    selected_run_id,
                    dt_frame,
                })),
            });
            self.engine_status = format!(
                "● Reconstructing stored body orientation · request {}",
                short_id(&request_id)
            );
            self.project_notice = Some((
                "Stored case geometry is being re-voxelized off the UI thread. The project manifest and immutable evidence remain inspectable while reconstruction is pending."
                    .into(),
                false,
            ));
            return true;
        };
        let (mask, pressure, cp, traction, result_grid, velocity) = (
            field.mask,
            field.pressure_pa,
            field.cp,
            field.traction_pa,
            field.n,
            Some(field.velocity),
        );
        let mut surf = None;
        if result_grid > 0 && cp.len() == result_grid * result_grid * result_grid {
            let scale = cp
                .iter()
                .fold(0.0f32, |maximum, value| maximum.max(value.abs()))
                .max(1e-6);
            let mut mask_u8 = vec![0u8; mask.len()];
            let mut cp_u8 = vec![128u8; cp.len()];
            for i in 0..result_grid {
                for j in 0..result_grid {
                    for k in 0..result_grid {
                        let source_index = i * result_grid * result_grid + j * result_grid + k;
                        let target_index = (k * result_grid + j) * result_grid + i;
                        mask_u8[target_index] = (mask[source_index].clamp(0.0, 1.0) * 255.0) as u8;
                        cp_u8[target_index] = (((cp[source_index] / scale) * 0.5 + 0.5)
                            .clamp(0.0, 1.0)
                            * 255.0) as u8;
                    }
                }
            }
            self.cad_version = self.cad_version.wrapping_add(1);
            surf = Some(gpu::SurfaceData {
                mask: std::sync::Arc::new(mask_u8),
                pressure: std::sync::Arc::new(cp_u8),
                dims: [result_grid as u32; 3],
                mask_version: self.cad_version,
                pressure_version: self.cad_version,
            });
        }
        if let Some(velocity) = velocity.as_deref() {
            let shape = [3usize, result_grid, result_grid, result_grid];
            let particles = flow::from_field(&shape, velocity);
            if !particles.is_empty() {
                self.particles = particles;
            }
            if let Some((volume, dimensions)) = flow::vorticity_volume(&shape, velocity) {
                let (lo, hi) = calibrate_cad_volume_density(&volume);
                self.density_lo = lo;
                self.density_hi = hi;
                self.volume_data = std::sync::Arc::new(volume);
                self.volume_dims = dimensions;
                self.volume_version = self.volume_version.wrapping_add(1);
            }
            if let Some((q_vol, q_dims)) = flow::q_criterion_volume(&shape, velocity) {
                self.q_volume_data = std::sync::Arc::new(q_vol);
                self.q_volume_dims = q_dims;
                self.volume_version = self.volume_version.wrapping_add(1);
            }
            let mut insights = flow::insights3d(&shape, velocity);
            insights.extend(cad::surface_insights(&mask, &cp, result_grid));
            self.insights3d = insights;
            self.surface_on = true;
            self.streamlines = true;
            self.q_iso_on = false;
            self.render_volume = false;
            self.slice = [false, false, false];
            self.view_fit = true;
        }
        self.current_model = model_id.clone();
        self.invalidate_cad_section();
        let bounds_grid = if result_grid >= 3 {
            result_grid
        } else {
            workflow.preflight.target_grid
        };
        let mask_bounds = cad::mask_bounds(&mask, bounds_grid);
        let mask = std::sync::Arc::new(mask);
        let surf_mask_source = surf.as_ref().map(|_| mask.clone());
        self.orientation_pending = None;
        self.cad = Some(CadCase {
            mask,
            mask_bounds,
            model: model_id,
            steps: workflow.operating.horizon_steps,
            surf,
            surf_mask_source,
            name: workflow.source_name.clone(),
            workflow,
            velocity: velocity.unwrap_or_default(),
            pressure,
            cp,
            traction,
            result_grid,
            dt_frame,
            active_run_id: selected_run_id,
            pending: false,
            pending_request_id: None,
            pending_run: None,
            playback: HorizonPlayback::default(),
        });
        self.apply_case_view_state_from_active();
        self.rebase_case_draft_history();
        self.nav = if self
            .cad
            .as_ref()
            .and_then(|case| case.workflow.result.as_ref())
            .is_some()
        {
            Nav::Evidence
        } else {
            Nav::Case
        };
        true
    }

    fn hydrate_project_runtime(&mut self) {
        // Opening/recovering/selecting a persisted case is a scope transition;
        // prior session draft edits cannot cross it.
        self.case_draft_history.clear();
        self.case_edit_transaction = None;
        self.nav = Nav::Projects;
        self.bench = None;
        self.bench_selected = None;
        self.bench_inspector = None;
        self.bench_inspector_pending = false;
        self.bench_error = None;
        self.bench_inspector_error = None;
        self.bench_tex.clear();
        self.active_benchmark_run_id = None;

        let selection = self
            .review_selection
            .clone()
            .unwrap_or_else(|| self.project.manifest().selection().clone());
        let Some(run_id) = selection.selected_run_id.clone() else {
            let _ = self.hydrate_external_flow_runtime(&selection);
            return;
        };
        match self.hydrate_engineering_run(&run_id) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                self.project_notice = Some((error, true));
                return;
            }
        }
        let evidence = self.project.manifest().evidence_for_run(&run_id);
        let suite = evidence.iter().rev().find(|artifact| {
            artifact
                .metadata
                .get("kind")
                .and_then(serde_json::Value::as_str)
                == Some("benchmark_suite")
        });
        let Some(suite) = suite else {
            return;
        };
        let snapshot = match self.stored_artifact_snapshot(suite) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.project_notice = Some((error, true));
                return;
            }
        };
        let Ok(benchmark) = serde_json::from_value::<engine::BenchResult>(snapshot) else {
            self.project_notice = Some((
                "Stored benchmark evidence could not be decoded; the immutable manifest remains available."
                    .into(),
                true,
            ));
            return;
        };
        self.f2d_model = benchmark.model.clone();
        if let Some(first_seed) = benchmark.seeds.first() {
            self.bench_seed_start = *first_seed;
        }
        self.bench_seeds = benchmark.seeds.len().clamp(1, u32::MAX as usize) as u32;
        self.active_benchmark_run_id = Some(run_id.clone());
        self.bench = Some(benchmark);
        self.nav = Nav::Benchmark;

        let selected_evidence = selection
            .selected_evidence_id
            .as_deref()
            .and_then(|evidence_id| self.project.manifest().evidence_artifact(evidence_id));
        let inspector_evidence = selected_evidence.filter(|artifact| {
            artifact
                .metadata
                .get("kind")
                .and_then(serde_json::Value::as_str)
                == Some("benchmark_inspector")
        });
        let Some(inspector_evidence) = inspector_evidence else {
            return;
        };
        let snapshot = match self.stored_artifact_snapshot(inspector_evidence) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.project_notice = Some((error, true));
                return;
            }
        };
        let Ok(inspector) = serde_json::from_value::<engine::BenchInspector>(snapshot) else {
            self.project_notice = Some((
                "Stored selected-cell evidence could not be decoded; suite scalars remain inspectable."
                    .into(),
                true,
            ));
            return;
        };
        if let Some(benchmark) = &self.bench {
            let seed_index = benchmark
                .seeds
                .iter()
                .position(|seed| *seed == inspector.seed);
            let horizon_index = benchmark
                .horizons
                .iter()
                .position(|horizon| *horizon == inspector.horizon);
            self.bench_selected = seed_index.zip(horizon_index);
        }
        if let Some(view_id) = selection.selected_view_id {
            if let Some(variable) = view_id
                .split('.')
                .nth(1)
                .and_then(InspectorVariable::from_key)
            {
                self.bench_var = variable;
            }
        }
        self.bench_inspector = Some(inspector);
    }

    fn inspect_project_run(&mut self, run_id: &str) {
        let Some(case_id) = self
            .project
            .manifest()
            .cases()
            .iter()
            .find(|case| case.runs().iter().any(|run| run.run_id() == run_id))
            .map(|case| case.case_id().to_owned())
        else {
            return;
        };
        let evidence = self.project.manifest().evidence_for_run(run_id);
        let selected = evidence
            .iter()
            .rev()
            .find(|artifact| {
                artifact
                    .metadata
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    == Some("benchmark_inspector")
            })
            .or_else(|| evidence.last())
            .copied();
        let selected_evidence_id = selected.map(|artifact| artifact.evidence_id.clone());
        let selected_view_id = selected
            .and_then(|artifact| artifact.calibrated_views.first())
            .map(|view| view.view_id.clone())
            .or_else(|| {
                self.project
                    .manifest()
                    .run(run_id)
                    .and_then(|run| run.calibrated_views().first())
                    .map(|view| view.view_id.clone())
            });
        let now = now_utc_unix();
        let run_id = run_id.to_owned();
        let selection = project::ProjectSelection {
            active_case_id: Some(case_id),
            selected_run_id: Some(run_id),
            selected_evidence_id,
            selected_view_id,
        };
        if self.project.availability().is_read_only_evidence() {
            self.review_selection = Some(selection);
            self.hydrate_project_runtime();
            self.project_notice = Some((
                "Opened immutable evidence with a session-only review selection; the read-only project was not changed."
                    .into(),
                false,
            ));
            return;
        }
        self.review_selection = None;
        if let Err(error) =
            self.transact_project("Selecting stored run evidence", now, move |manifest| {
                manifest.set_selection(selection, now)
            })
        {
            self.project_notice =
                Some((format!("Stored run could not be selected: {error}"), true));
            return;
        }
        self.hydrate_project_runtime();
    }

    fn persist_benchmark_view_selection(&mut self) {
        let current = self
            .review_selection
            .clone()
            .unwrap_or_else(|| self.project.manifest().selection().clone());
        if current.selected_run_id.is_none() || current.selected_evidence_id.is_none() {
            return;
        }
        let selected_view_id = format!("benchmark.{}.model", self.bench_var.key());
        if self.project.availability().is_read_only_evidence() {
            self.review_selection = Some(project::ProjectSelection {
                selected_view_id: Some(selected_view_id),
                ..current
            });
            return;
        }
        let now = now_utc_unix();
        if let Err(error) =
            self.transact_project("Selecting a calibrated view", now, move |manifest| {
                manifest.set_selection(
                    project::ProjectSelection {
                        selected_view_id: Some(selected_view_id),
                        ..current
                    },
                    now,
                )
            })
        {
            self.project_notice = Some((
                format!("Selected calibrated view was not saved: {error}"),
                true,
            ));
        }
    }

    fn regenerate(&mut self) {
        self.seed = self.seed.wrapping_add(1);
        if self.engine_ok {
            let _ = self.engine.send(engine::Cmd::Predict {
                model: self.current_model.clone(),
                seed: self.seed,
            });
            self.engine_status = "● Predicting…".into();
        } else {
            self.particles = flow::generate(6000, self.seed);
            let (vol, dims) = flow::procedural_volume(48, self.seed);
            self.volume_data = std::sync::Arc::new(vol);
            self.volume_dims = dims;
            self.volume_version = self.volume_version.wrapping_add(1);
            self.insights3d.clear(); // procedural placeholder: no real field to annotate
        }
    }

    /// Request a 2D prediction, coalesced to one in-flight request (TimeJump can
    /// fire many per second while dragging; stale ones are dropped, the newest
    /// re-fires when the current result lands).
    fn request_2d(&mut self) {
        if !self.engine_ok {
            return;
        }
        if self.f2d_pending {
            self.f2d_dirty = true;
            return;
        }
        let cmd = if let Some(ic) = &self.f2d_painted {
            // painted IC: stateless — the projected field rides with every scrub
            engine::Cmd::PredictIC {
                model: self.f2d_model.clone(),
                steps: self.f2d_horizon,
                ic: ic.clone(),
            }
        } else {
            engine::Cmd::Predict2D {
                model: self.f2d_model.clone(),
                steps: self.f2d_horizon,
                seed: 1,
                want_truth: self.f2d_truth,
                method: match self.f2d_method {
                    PMethod::Spectral => "spectral",
                    PMethod::Fd => "fd",
                }
                .into(),
                tolerance: 10f32.powi(-self.f2d_tol_exp),
                boundary: match self.f2d_boundary {
                    PBoundary::Periodic => "periodic",
                    PBoundary::Dirichlet => "dirichlet",
                }
                .into(),
            }
        };
        let _ = self.engine.send(cmd);
        self.f2d_pending = true;
        self.f2d_req_at = Some(std::time::Instant::now());
    }

    /// Rebuild the colormapped textures only when the field, variable, or overlay
    /// changed (not every frame). Model prediction and solver reference share one
    /// colormap scale so the two panels are visually comparable (independent
    /// normalization would hide amplitude errors).
    fn ensure_f2d_textures(&mut self, ctx: &egui::Context) {
        let Some(f) = &self.f2d else { return };
        let sig = field2d_cache_signature(self.f2d_gen, self.f2d_var, self.f2d_truth);
        if sig == self.f2d_sig && !self.f2d_tex.is_empty() {
            return;
        }
        let (mut scale, signed) = field2d::scalar_scale(&f.ai, f.n, self.f2d_var);
        if self.f2d_truth {
            if let Some(truth) = &f.truth {
                scale = scale.max(field2d::scalar_scale(truth, f.n, self.f2d_var).0);
            }
        }
        let opts = egui::TextureOptions::NEAREST;
        let mut tex = vec![ctx.load_texture(
            "f2d.ai",
            field2d::image_scaled(f, &f.ai, self.f2d_var, scale),
            opts,
        )];
        if self.f2d_truth {
            if let Some(truth) = &f.truth {
                tex.push(ctx.load_texture(
                    "f2d.truth",
                    field2d::image_scaled(f, truth, self.f2d_var, scale),
                    opts,
                ));
                tex.push(ctx.load_texture(
                    "f2d.err",
                    field2d::error_image(f, &f.ai, truth, self.f2d_var),
                    opts,
                ));
            }
        }
        self.f2d_tex = tex;
        self.f2d_sig = sig;
        self.f2d_scale = scale;
        self.f2d_signed = signed;
    }

    /// Critical-point extraction scans full N² maps. Keep it off the paint hot
    /// path until the field generation, displayed variable, or reference
    /// visibility changes.
    fn ensure_f2d_insights(&mut self) {
        if !self.insights_on {
            return;
        }
        let key = (self.f2d_gen, self.f2d_var, self.f2d_truth);
        if self.f2d_insights_key == Some(key) {
            return;
        }
        let Some(field) = self.f2d.as_ref() else {
            self.f2d_insights.clear();
            self.f2d_insights_key = Some(key);
            return;
        };
        let truth = self.f2d_truth.then_some(field.truth.as_deref()).flatten();
        self.f2d_insights = field2d::insights(field, &field.ai, truth, self.f2d_var);
        self.f2d_insights_key = Some(key);
    }

    /// Import supported geometry, voxelize it onto the 3D model's grid, and
    /// send it to the engine (which develops the flow, then predicts).
    fn import_cad(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Geometry", &["stl", "stp", "step", "3mf"])
            .add_filter("STEP", &["stp", "step"])
            .add_filter("3MF", &["3mf"])
            .add_filter("STL", &["stl"])
            .pick_file()
        else {
            return;
        };
        self.import_cad_path(path);
    }

    /// Shared import path for the file dialog and the landing drop target.
    fn import_cad_path(&mut self, path: std::path::PathBuf) {
        self.enqueue_geometry_import(path, None, None);
    }

    fn enqueue_geometry_import(
        &mut self,
        path: std::path::PathBuf,
        selected_shell_entity_id: Option<u64>,
        preloaded_bytes: Option<Vec<u8>>,
    ) {
        if self.reject_project_mutation("Importing a geometry revision") {
            return;
        }
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let stage = match extension.as_str() {
            "stp" | "step" => "Detecting STEP · translating B-rep · tessellating · voxelizing…",
            "3mf" => "Reading 3MF package · tessellating · voxelizing…",
            _ => "Reading geometry · voxelizing…",
        };
        self.project_notice = Some((stage.into(), false));
        self.pending_shell_choice = None;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("mesh")
            .to_string();
        let bytes = match preloaded_bytes {
            Some(bytes) => bytes,
            None => match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(e) => {
                    self.project_notice = Some((format!("Geometry could not be read: {e}"), true));
                    return;
                }
            },
        };
        let compatible = |card: &&engine::ModelCard| {
            card.status != "invalid"
                && engine::is_model_bundle_id(&card.id)
                && engine::model_authenticity_allows_external_flow(&card.authenticity_status)
                && card.dimension == 3
                && card.in_channels > card.out_channels
                && card.grid > 0
        };
        let grid = self
            .models
            .iter()
            .filter(compatible)
            .find(|card| card.id == self.settings.default_3d_model)
            .or_else(|| {
                self.models
                    .iter()
                    .filter(compatible)
                    .max_by_key(|card| card.grid)
            })
            .map(|card| card.grid as usize)
            .unwrap_or(DIAGNOSTIC_PREFLIGHT_GRID);
        if self.geometry_import_worker.is_none() {
            self.geometry_import_worker =
                match GeometryImportWorker::spawn(self.repaint_context.clone()) {
                    Ok(worker) => Some(worker),
                    Err(error) => {
                        self.project_notice = Some((error, true));
                        return;
                    }
                };
        }
        self.geometry_import_generation = self.geometry_import_generation.wrapping_add(1).max(1);
        let generation = self.geometry_import_generation;
        let request_id = format!(
            "geometry-import-{generation}-{}",
            uuid::Uuid::new_v4().simple()
        );
        let request = GeometryImportWorkRequest {
            generation,
            request_id: request_id.clone(),
            path: path.clone(),
            source_name: name,
            source_bytes: bytes,
            grid,
            selected_shell_entity_id,
        };
        let sent = self
            .geometry_import_worker
            .as_ref()
            .expect("geometry import worker was initialized")
            .request_tx
            .send(request);
        if sent.is_err() {
            self.geometry_import_worker = None;
            self.project_notice = Some((
                "Geometry import was not queued because the worker stopped.".into(),
                true,
            ));
            return;
        }
        self.geometry_import_pending = Some(PendingGeometryImport {
            generation,
            request_id: request_id.clone(),
            path,
            started_at: std::time::Instant::now(),
        });
        self.engine_status = format!(
            "● Geometry import running off-thread · {}",
            short_id(&request_id)
        );
        self.project_notice = Some((
            "Geometry translation and voxelization are running off the UI thread. Progress is indeterminate; you can cancel by importing another file (stale results are discarded)."
                .into(),
            false,
        ));
    }

    fn handle_geometry_import_results(&mut self) {
        let completed: Vec<_> = self
            .geometry_import_worker
            .as_ref()
            .map(|worker| worker.result_rx.try_iter().collect())
            .unwrap_or_default();
        for completed in completed {
            let is_current = self
                .geometry_import_pending
                .as_ref()
                .is_some_and(|pending| {
                    pending.generation == completed.generation
                        && pending.request_id == completed.request_id
                });
            if !is_current {
                continue;
            }
            self.geometry_import_pending = None;
            match completed.outcome {
                Ok(ready) => {
                    self.apply_geometry_import_ready(
                        completed.path,
                        completed.source_name,
                        completed.source_bytes,
                        completed.source_sha256,
                        ready,
                    );
                }
                Err(GeometryImportFailure::ChooseShell(choice)) => {
                    self.pending_shell_choice = Some(PendingShellChoice {
                        path: completed.path,
                        source_name: completed.source_name,
                        source_bytes: completed.source_bytes,
                        source_sha256: completed.source_sha256,
                        declared_unit: choice.declared_unit,
                        shells: choice.shells,
                    });
                    self.project_notice = Some((
                        "Multiple B-rep solids found. Choose one shell to analyze — occurrence assemblies remain unsupported."
                            .into(),
                        false,
                    ));
                    self.nav = Nav::Case;
                }
                Err(GeometryImportFailure::Message(error)) => {
                    self.project_notice = Some((
                        format!(
                            "Geometry import blocked: {error}. Try exporting a single closed part as STL, STEP, or 3MF."
                        ),
                        true,
                    ));
                }
            }
        }
    }

    fn draw_shell_choice_modal(&mut self, ctx: &egui::Context) {
        if let Some(pending) = self.geometry_import_pending.as_ref() {
            let elapsed = pending.started_at.elapsed().as_secs();
            let path_label = pending
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("geometry")
                .to_owned();
            let mut cancel = false;
            egui::Window::new("Importing geometry")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_TOP, [0.0, 48.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Translating and voxelizing {path_label} · {elapsed}s · request {}",
                        short_id(&pending.request_id)
                    ));
                    ui.label(
                        egui::RichText::new(
                            "Work runs off the UI thread. Cancel discards this generation; a late result cannot mutate the project.",
                        )
                        .weak(),
                    );
                    if ui.button("Cancel import").clicked() {
                        cancel = true;
                    }
                });
            if cancel {
                self.geometry_import_generation =
                    self.geometry_import_generation.wrapping_add(1).max(1);
                self.geometry_import_pending = None;
                self.project_notice = Some((
                    "Geometry import cancelled. Any in-flight worker result will be discarded."
                        .into(),
                    false,
                ));
                self.engine_status = "○ Geometry import cancelled".into();
            }
        }
        let Some(choice) = self.pending_shell_choice.clone() else {
            return;
        };
        let mut open = true;
        let mut selected = None;
        let mut cancelled = false;
        egui::Window::new("Choose solid")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(
                        "This STEP file has multiple B-rep shells without an assembly graph. Pick one solid to voxelize. Assemblies with occurrence transforms are still rejected.",
                    )
                    .weak(),
                );
                ui.label(format!(
                    "Declared units: {} · {}",
                    choice.declared_unit, choice.source_name
                ));
                ui.add_space(8.0);
                for shell in &choice.shells {
                    if ui
                        .add_sized(
                            [ui.available_width(), 28.0],
                            egui::Button::new(&shell.label),
                        )
                        .clicked()
                    {
                        selected = Some(shell.entity_id);
                    }
                }
                ui.add_space(8.0);
                if ui.button("Cancel import").clicked() {
                    cancelled = true;
                }
            });
        if !open || cancelled {
            self.pending_shell_choice = None;
            self.project_notice = Some(("Geometry import cancelled.".into(), false));
            return;
        }
        if let Some(entity_id) = selected {
            let path = choice.path;
            let bytes = choice.source_bytes;
            let _ = choice.source_sha256;
            self.pending_shell_choice = None;
            self.enqueue_geometry_import(path, Some(entity_id), Some(bytes));
        }
    }

    fn apply_geometry_import_ready(
        &mut self,
        path: std::path::PathBuf,
        name: String,
        bytes: Vec<u8>,
        source_sha256: String,
        ready: GeometryImportReady,
    ) {
        let imported = ready.imported;
        let mesh_diagnostics = ready.diagnostics;
        let vm = ready.voxel;
        let model_card = self
            .models
            .iter()
            .filter(|card| engine::is_qualified_external_flow_model(card, vm.n))
            .find(|card| card.id == self.settings.default_3d_model)
            .or_else(|| {
                self.models
                    .iter()
                    .filter(|card| engine::is_qualified_external_flow_model(card, vm.n))
                    .max_by_key(|card| card.grid)
            })
            .cloned();
        let (model, model_sha256, model_max_steps, model_support, model_warning) = if let Some(
            model_card,
        ) = model_card
        {
            (
                model_card.id,
                Some(model_card.checkpoint_sha256),
                model_card.max_steps,
                engineering::ModelSupport {
                    status: model_card.status,
                    dimension: model_card.dimension,
                    grid: model_card.grid,
                    input_channels: model_card.in_channels,
                    output_channels: model_card.out_channels,
                    scenario: model_card.scenario,
                    physics_contract: model_card.physics_contract,
                },
                None,
            )
        } else {
            (
                    String::new(),
                    None,
                    0,
                    engineering::ModelSupport {
                        status: "unavailable".into(),
                        ..Default::default()
                    },
                    Some(format!(
                        "MODEL GATE · Geometry was inspected on the model-independent {}³ diagnostic grid. Inference remains blocked until a compatible verified 3D .reynmodel bundle with a matching grid is selected.",
                        vm.n
                    )),
                )
        };
        let analyzed_mesh_sha256 = cad::analyzed_mesh_sha256(&imported.mesh);
        let import_steps = geometry_import_steps(
            imported.format,
            &imported,
            &analyzed_mesh_sha256,
            &mesh_diagnostics,
            &vm,
        );
        let mask = std::sync::Arc::new(vm.mask);
        let source_revision_id = format!("source-{}", uuid::Uuid::new_v4());
        let case_id = self
            .cad
            .as_ref()
            .map(|case| case.workflow.case_id.clone())
            .unwrap_or_else(|| format!("case-{}", uuid::Uuid::new_v4()));
        let case_revision_id = format!("case-revision-{}", uuid::Uuid::new_v4());
        let parent_source_revision_id = self
            .cad
            .as_ref()
            .and_then(|case| case.workflow.source_revision_id.clone());
        let parent_case_revision_id = self
            .cad
            .as_ref()
            .and_then(|case| case.workflow.case_revision_id.clone());
        let source_revision_number = parent_source_revision_id
            .as_ref()
            .and_then(|parent| {
                self.project
                    .manifest()
                    .source_revisions()
                    .iter()
                    .find(|source| source.source_revision_id == *parent)
                    .map(|source| source.revision + 1)
            })
            .unwrap_or(1);
        let mut warnings = model_warning.into_iter().collect::<Vec<_>>();
        warnings.extend(imported.warnings.clone());
        if imported.format == cad::GeometryFormat::Step {
            warnings.push(format!(
                    "STEP TESSELLATION · {} {} · relative chord tolerance {:.6} (absolute {:.6} source units) · face-boundary weld tolerance {:.6} relative · {} B-rep shell(s). The original STEP bytes remain authoritative.",
                    imported.translator,
                    imported.translator_version,
                    crate::cad_step::RELATIVE_CHORD_TOLERANCE,
                    imported
                        .tessellation_tolerance_source_units
                        .unwrap_or_default(),
                    imported.vertex_weld_relative_tolerance.unwrap_or_default(),
                    imported.source_shells,
                ));
        }
        if mesh_diagnostics.inconsistent_winding_edges > 0 {
            warnings.push(format!(
                "{} edges have inconsistent winding.",
                mesh_diagnostics.inconsistent_winding_edges
            ));
        }
        if mesh_diagnostics.self_intersection_pairs > 0 {
            warnings.push(format!(
                "{} non-adjacent triangle pairs intersect.",
                mesh_diagnostics.self_intersection_pairs
            ));
        }
        if mesh_diagnostics.signed_volume < 0.0 {
            warnings.push(format!(
                    "The derived {} triangle winding is inward. The value is recorded; current diffuse-interface loads derive normals from the occupancy mask and are not sign-flipped by source winding.",
                    imported.format.label()
                ));
        }
        if mesh_diagnostics.components > 1 {
            warnings.push(format!(
                "{} disconnected {} components detected.",
                mesh_diagnostics.components,
                imported.format.label(),
            ));
        }
        if let Some(previous) = &self.cad {
            let prior = &previous.workflow.preflight;
            if prior.source_sha256 != source_sha256 {
                warnings.push(format!(
                    "REIMPORT CHANGE · source SHA-256 {} → {}.",
                    short_hash(&prior.source_sha256),
                    short_hash(&source_sha256)
                ));
            }
            if !prior.analyzed_mesh_sha256.is_empty()
                && prior.analyzed_mesh_sha256 != analyzed_mesh_sha256
            {
                warnings.push(format!(
                    "REIMPORT CHANGE · analyzed mesh SHA-256 {} → {}.",
                    short_hash(&prior.analyzed_mesh_sha256),
                    short_hash(&analyzed_mesh_sha256)
                ));
            }
            if prior.triangles != mesh_diagnostics.triangles {
                warnings.push(format!(
                    "REIMPORT CHANGE · triangle count {} → {}.",
                    prior.triangles, mesh_diagnostics.triangles
                ));
            }
            let next_extents = mesh_diagnostics.extents.map(f64::from);
            if prior.source_extents != next_extents {
                warnings.push(format!(
                        "REIMPORT CHANGE · source extents {:?} → {:?}; stable face-region identity is not preserved by the current {} import path.",
                        prior.source_extents,
                        next_extents,
                        imported.format.label(),
                    ));
            }
            if prior.boundary_edges != mesh_diagnostics.boundary_edges
                || prior.non_manifold_edges != mesh_diagnostics.non_manifold_edges
                || prior.degenerate_triangles != mesh_diagnostics.degenerate_triangles
            {
                warnings.push(format!(
                    "REIMPORT CHANGE · defects open/non-manifold/degenerate {}·{}·{} → {}·{}·{}.",
                    prior.boundary_edges,
                    prior.non_manifold_edges,
                    prior.degenerate_triangles,
                    mesh_diagnostics.boundary_edges,
                    mesh_diagnostics.non_manifold_edges,
                    mesh_diagnostics.degenerate_triangles
                ));
            }
        }
        let preflight = engineering::GeometryPreflight {
            source_sha256: source_sha256.clone(),
            source_bytes: bytes.len() as u64,
            source_format: match imported.format {
                cad::GeometryFormat::Stl => "stl",
                cad::GeometryFormat::Step => "step",
                cad::GeometryFormat::ThreeMf => "3mf",
            }
            .into(),
            source_declared_units: imported.declared_unit.clone(),
            geometry_translator: imported.translator.clone(),
            geometry_translator_version: imported.translator_version.clone(),
            tessellation_tolerance_source_units: imported.tessellation_tolerance_source_units,
            vertex_weld_relative_tolerance: imported.vertex_weld_relative_tolerance,
            analyzed_mesh_sha256: analyzed_mesh_sha256.clone(),
            import_steps,
            source_shells: imported.source_shells,
            selected_shell_entity_id: imported.selected_shell_entity_id,
            triangles: mesh_diagnostics.triangles,
            components: mesh_diagnostics.components,
            degenerate_triangles: mesh_diagnostics.degenerate_triangles,
            boundary_edges: mesh_diagnostics.boundary_edges,
            non_manifold_edges: mesh_diagnostics.non_manifold_edges,
            inconsistent_winding_edges: mesh_diagnostics.inconsistent_winding_edges,
            self_intersection_pairs: mesh_diagnostics.self_intersection_pairs,
            source_signed_volume: mesh_diagnostics.signed_volume,
            source_extents: mesh_diagnostics.extents.map(f64::from),
            proposed_scale: vm.scale,
            solver_characteristic_length: vm.char_len as f64,
            angle_of_attack_deg: vm.orientation.angle_of_attack_deg,
            yaw_deg: vm.orientation.yaw_deg,
            roll_deg: vm.orientation.roll_deg,
            transform_4x4: vm.transform_4x4,
            target_grid: vm.n,
            solid_voxels: vm.solid_voxels,
            voxel_components: vm.components,
            minimum_cells_across: vm.minimum_cells_across,
            boundary_clearance_cells: vm.boundary_clearance_cells,
            voxel_axis_disagreement_fraction: vm.axis_disagreement_fraction,
            voxel_odd_crossing_rows: vm.odd_crossing_rows,
            voxel_classification_version: vm.classification_version,
            warnings: warnings.clone(),
            waivers: Vec::new(),
            transform_approved: false,
        };
        let reference_length = mesh_diagnostics.extents[1]
            .max(mesh_diagnostics.extents[2])
            .max(1e-6) as f64;
        let declared_length_unit = match imported.declared_unit.as_deref() {
            Some("mm") => engineering::LengthUnit::Millimeter,
            Some("cm") => engineering::LengthUnit::Centimeter,
            Some("m") => engineering::LengthUnit::Meter,
            Some("in") => engineering::LengthUnit::Inch,
            Some("ft") => engineering::LengthUnit::Foot,
            _ => engineering::LengthUnit::Unknown,
        };
        let workflow = engineering::ExternalFlowCase {
            stage: engineering::CaseStage::Preflight,
            case_id: case_id.clone(),
            name: path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("Geometry")
                .to_string(),
            source_name: name.clone(),
            source_revision_id: Some(source_revision_id.clone()),
            case_revision_id: Some(case_revision_id.clone()),
            model_id: model.clone(),
            model_sha256,
            model_max_steps,
            model_support,
            preflight,
            operating: {
                let mut operating = engineering::OperatingPoint {
                    // STEP declarations prefill this control but do not
                    // approve the transform; the operator must still
                    // confirm it through the existing hard gate.
                    length_unit: declared_length_unit,
                    reference_length,
                    // Settings › Workflow default, clamped to the model.
                    horizon_steps: self
                        .settings
                        .default_horizon_steps
                        .clamp(1, model_max_steps.max(1)),
                    ..Default::default()
                };
                // Default free-stream is 1 m/s — fine for meter-scale bodies,
                // but mm CAD lands far outside the qualified Re band. Propose a
                // mid-envelope speed when units are already known from STEP.
                let _ = operating.retarget_velocity_to_qualified_reynolds();
                operating
            },
            review_focus: engineering::ReviewFocus::default(),
            result: None,
            parent_run_id: self
                .cad
                .as_ref()
                .and_then(|case| case.active_run_id.clone()),
            named_regions: Vec::new(),
            view_state: Default::default(),
        };
        if let Err(error) = self.add_project_content(
            "Importing a geometry revision",
            bytes.clone(),
            imported.format.media_type(),
            &source_sha256,
        ) {
            self.project_notice = Some((format!("Geometry content was not stored: {error}"), true));
            return;
        }
        let source = project::SourceRevision {
            source_revision_id: source_revision_id.clone(),
            source_kind: project::SourceKind::Geometry,
            revision: source_revision_number,
            imported_utc_unix: now_utc_unix(),
            uri_hint: Some(path.display().to_string()),
            byte_size: bytes.len() as u64,
            content_sha256: source_sha256,
            declared_units: imported.declared_unit.clone(),
            frame: Some("source frame; preprocessing transform pending approval".into()),
            transform_4x4: vm.transform_4x4,
            parent_revision_id: parent_source_revision_id,
            warnings,
        };
        let revision = project::CaseRevision {
            case_revision_id: case_revision_id.clone(),
            parent_revision_id: parent_case_revision_id,
            created_utc_unix: now_utc_unix(),
            source_revision_ids: vec![source_revision_id],
            contract: workflow.exact_contract(),
            discretization: serde_json::json!({
                "grid": [vm.n, vm.n, vm.n],
                "solid_voxels": vm.solid_voxels,
                "minimum_cells_across": vm.minimum_cells_across,
                "boundary_clearance_cells": vm.boundary_clearance_cells,
                "axis_disagreement_fraction": vm.axis_disagreement_fraction,
                "odd_crossing_rows_xyz": vm.odd_crossing_rows,
                "classification_version": vm.classification_version,
                "transform_4x4": vm.transform_4x4,
            }),
            outputs: serde_json::json!({
                "velocity": "model_prediction",
                "pressure": "recovered",
                "surface_loads": engineering::SURFACE_LOAD_METHOD,
            }),
        };
        let existing_case = self
            .project
            .manifest()
            .cases()
            .iter()
            .any(|case| case.case_id() == case_id);
        let persist_result = self.transact_project(
            "Importing a geometry revision",
            now_utc_unix(),
            |manifest| {
                manifest.add_source_revision(source, now_utc_unix())?;
                if existing_case {
                    manifest.append_case_revision(&case_id, revision, now_utc_unix())?;
                } else {
                    manifest.create_case(
                        case_id.clone(),
                        workflow.name.clone(),
                        revision,
                        now_utc_unix(),
                    )?;
                }
                Ok(())
            },
        );
        if let Err(error) = persist_result {
            self.project_notice = Some((format!("Case revision was not recorded: {error}"), true));
            return;
        }
        self.dependencies_dirty = true;
        self.invalidate_cad_section();
        self.cad = Some(CadCase {
            mask: mask.clone(),
            mask_bounds: cad::mask_bounds(mask.as_ref(), vm.n),
            model: model.clone(),
            steps: workflow.operating.horizon_steps,
            surf: None,
            surf_mask_source: None,
            name: name.clone(),
            workflow,
            velocity: Vec::new(),
            pressure: Vec::new(),
            cp: Vec::new(),
            traction: Vec::new(),
            result_grid: 0,
            dt_frame: 0.0,
            active_run_id: None,
            pending: false,
            pending_request_id: None,
            pending_run: None,
            playback: HorizonPlayback::default(),
        });
        self.case_draft_dirty = false;
        self.orientation_draft = None;
        self.orientation_pending = None;
        self.rebase_case_draft_history();
        self.nav = Nav::Case;
        self.refresh_geometry_setup_status();
        self.project_notice = Some((
                "Geometry revision stored. Confirm units, transform, preflight, and operating point before execution."
                    .into(),
                false,
            ));
    }

    fn import_model(&mut self) {
        if !self.engine_ok {
            self.library.notice = Some((
                "Engine unavailable; model-bundle validation cannot run.".into(),
                true,
            ));
            self.nav = Nav::Models;
            return;
        }
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Reyn model bundle", &["reynmodel"])
            .set_directory(&self.settings.research_dir)
            .pick_file()
        {
            if let Err(error) = engine::require_model_signature(&path) {
                self.library.busy = false;
                self.library.validation = None;
                self.library.notice = Some((error.to_string(), true));
                self.nav = Nav::Models;
                return;
            }
            self.library.busy = true;
            self.library.validation = None;
            self.library.notice = Some(("Validating model-bundle contract…".into(), false));
            self.library_pending_request = self
                .engine
                .send(engine::Cmd::ImportModel {
                    path: path.to_string_lossy().into_owned(),
                })
                .ok();
            self.nav = Nav::Models;
        }
    }

    fn export(&mut self) {
        if let Some(path) = self
            .export_dialog("reyn_diagnostics.csv")
            .add_filter("CSV", &["csv"])
            .save_file()
        {
            let (hel, ens, q, count) = diagnostics(&self.particles);
            let active_run_id = self
                .cad
                .as_ref()
                .and_then(|case| case.active_run_id.as_deref());
            let provenance = calculation_export_provenance(self.project.manifest(), active_run_id);
            let mut bytes = Vec::new();
            if let Err(error) = write_calculation_export(
                &mut bytes,
                &provenance,
                count,
                hel,
                ens,
                q,
                self.density_lo,
                self.opacity,
            ) {
                self.project_notice = Some((error.clone(), true));
                self.engine_status = format!("● Export failed: {error}");
                return;
            }
            match std::fs::write(&path, bytes) {
                Ok(()) => {
                    self.engine_status = format!(
                        "● Exported {}",
                        path.file_name().and_then(|s| s.to_str()).unwrap_or("file")
                    );
                    self.project_notice = Some((
                        format!("Exported calculations to {}.", path.display()),
                        false,
                    ));
                }
                Err(error) => {
                    let message = format!(
                        "Calculations were not exported to {}: {error}",
                        path.display()
                    );
                    self.engine_status = format!("● Export failed: {error}");
                    self.project_notice = Some((message, true));
                }
            }
        }
    }

    fn reset_controls(&mut self) {
        self.slice = [true, false, false];
        self.slice_pos = [0.5, 0.0, 0.0];
        self.section_axis = engineering_section::SectionAxis::X;
        self.section_quantity = engineering_section::SectionQuantity::PhysicalCp;
        self.density_lo = 0.85;
        self.opacity = 0.75;
        self.shadows = true;
        self.streamlines = false;
    }

    fn activate_model(&mut self, model_id: &str) {
        let Some(model) = self
            .models
            .iter()
            .find(|model| model.id == model_id)
            .cloned()
        else {
            self.library.notice =
                Some(("Model is no longer in the engine inventory.".into(), true));
            return;
        };
        if model.status == "invalid" {
            self.library.notice =
                Some(("Rejected model bundles cannot be made active.".into(), true));
            return;
        }
        if !engine::model_authenticity_allows_external_flow(&model.authenticity_status) {
            self.library.notice = Some((
                format!(
                    "Unsigned or unauthenticated model bundles cannot be made active. {}",
                    engine::TRUSTED_MODEL_CONVERSION_GUIDANCE
                ),
                true,
            ));
            return;
        }
        if !engine::is_model_bundle_id(&model.id) {
            self.library.notice = Some((engine::TRUSTED_MODEL_CONVERSION_GUIDANCE.into(), true));
            return;
        }
        self.current_model = model.id.clone();
        self.seed = self.seed.wrapping_add(1);
        if model.dimension == 2 {
            self.f2d_model = model.id.clone();
            self.f2d = None;
            self.f2d_tex.clear();
            self.f2d_sig = u64::MAX;
            self.bench = None;
            self.bench_selected = None;
            self.bench_inspector = None;
            self.bench_inspector_pending = false;
            self.bench_tex.clear();
            self.active_benchmark_run_id = None;
            if self
                .project
                .manifest()
                .case_by_contract_kind("benchmark_suite")
                .is_some()
            {
                match self.prepare_benchmark_case(true) {
                    Ok(_) => {
                        self.project_notice = Some((
                            "Active model changed; dependent benchmark stages are stale and completed runs remain immutable."
                                .into(),
                            false,
                        ));
                    }
                    Err(error) => {
                        self.project_notice = Some((
                            format!("Active model changed, but its case revision was not recorded: {error}"),
                            true,
                        ));
                    }
                }
            }
            if self.settings.developer_research_sandbox {
                self.nav = Nav::Fields2D;
                self.request_2d();
            } else {
                self.nav = Nav::Models;
            }
        } else {
            self.nav = Nav::Models;
        }
        let persistence = self.persist_model_default(&model.id, model.dimension);
        if self.engine_ok && model.dimension == 3 && self.settings.developer_research_sandbox {
            let _ = self.engine.send(engine::Cmd::Predict {
                model: self.current_model.clone(),
                seed: self.seed,
            });
            self.engine_status = "● Predicting…".into();
        }
        self.library.notice = Some(match persistence {
            Ok(()) => (format!("{} is now active", self.current_model), false),
            Err(error) => (
                format!(
                    "{} is active for this session, but the default was not saved: {error}",
                    self.current_model
                ),
                true,
            ),
        });
    }

    fn persist_model_default(&mut self, model_id: &str, dimension: u32) -> Result<(), String> {
        if !engine::is_model_bundle_id(model_id) {
            return Err(engine::TRUSTED_MODEL_CONVERSION_GUIDANCE.into());
        }
        match dimension {
            2 => self.settings.default_2d_model = model_id.into(),
            3 => self.settings.default_3d_model = model_id.into(),
            _ => return Err(format!("unsupported model dimension {dimension}")),
        }
        self.settings.normalize();
        self.settings_draft = self.settings.clone();
        self.settings.save().map(|_| ())
    }

    fn handle_library_action(&mut self, action: library::LibraryAction) {
        match action {
            library::LibraryAction::Activate(model) => self.activate_model(&model),
            library::LibraryAction::Delete(model) => {
                self.library.busy = true;
                self.library.notice = Some(("Deleting managed model bundle…".into(), false));
                self.library_pending_request =
                    self.engine.send(engine::Cmd::DeleteModel { model }).ok();
            }
            library::LibraryAction::Import => self.import_model(),
            library::LibraryAction::Refresh => {
                self.library.busy = true;
                self.library.notice = Some(("Refreshing model-bundle metadata…".into(), false));
                self.model_inventory_state = ModelInventoryState::Loading;
                self.library_pending_request = self.engine.send(engine::Cmd::ListModels).ok();
            }
        }
    }

    fn handle_settings_action(&mut self, ctx: &egui::Context, action: settings::SettingsAction) {
        match action {
            settings::SettingsAction::RestoreDefaults => {
                // Preferences reset; user data (signing identity, saved
                // operating-point presets, and case templates) is preserved as
                // promised in the confirmation copy.
                let defaults = settings::AppSettings {
                    signing_key_reference: self.settings.signing_key_reference.clone(),
                    signing_public_key_base64: self.settings.signing_public_key_base64.clone(),
                    signing_key_fingerprint_sha256: self
                        .settings
                        .signing_key_fingerprint_sha256
                        .clone(),
                    revoked_signing_key_fingerprints: self
                        .settings
                        .revoked_signing_key_fingerprints
                        .clone(),
                    protected_malformed_settings_path: self
                        .settings
                        .protected_malformed_settings_path
                        .clone(),
                    operating_presets: self.settings.operating_presets.clone(),
                    case_templates: self.settings.case_templates.clone(),
                    ..settings::AppSettings::default()
                };
                self.settings_draft = defaults;
                self.settings_notice = Some(("Defaults staged; save to apply.".into(), false));
            }
            settings::SettingsAction::ImportCaseTemplate => {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Reyn case template", &[settings::CASE_TEMPLATE_EXTENSION])
                    .pick_file()
                else {
                    return;
                };
                match settings::load_case_template(&path) {
                    Ok(template) => {
                        let name = template.name.clone();
                        match self.settings_draft.upsert_case_template(template) {
                            Ok(()) => {
                                self.settings_notice = Some((
                                    format!(
                                        "Imported template “{name}” from {}. Save settings to keep it.",
                                        path.display()
                                    ),
                                    false,
                                ));
                            }
                            Err(error) => {
                                self.settings_notice =
                                    Some((format!("Template was not imported: {error}"), true));
                            }
                        }
                    }
                    Err(error) => {
                        self.settings_notice =
                            Some((format!("Template was not imported: {error}"), true));
                    }
                }
            }
            settings::SettingsAction::ExportCaseTemplate(index) => {
                let Some(template) = self.settings_draft.case_templates.get(index).cloned() else {
                    self.settings_notice = Some((
                        "Template was not exported: selection is unavailable.".into(),
                        true,
                    ));
                    return;
                };
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Reyn case template", &[settings::CASE_TEMPLATE_EXTENSION])
                    .set_file_name(settings::case_template_file_name(&template))
                    .save_file()
                else {
                    return;
                };
                self.settings_notice = Some(match settings::save_case_template(&template, &path) {
                    Ok(()) => (
                        format!(
                            "Exported template “{}” to {}",
                            template.name,
                            path.display()
                        ),
                        false,
                    ),
                    Err(error) => (format!("Template was not exported: {error}"), true),
                });
            }
            settings::SettingsAction::CreateSigningKey => {
                let provider = signing::NativeKeychainProvider;
                match provider.create_key() {
                    Ok(key) => {
                        let mut candidate = self.settings.clone();
                        candidate.set_signing_key(&key);
                        match candidate.save() {
                            Ok(path) => {
                                self.settings = candidate.clone();
                                self.settings_draft = candidate;
                                self.settings_notice = Some((
                                    format!(
                                        "Ed25519 key created in the macOS Keychain; public settings saved to {}",
                                        path.display()
                                    ),
                                    false,
                                ));
                            }
                            Err(error) => {
                                let _ = provider.delete_key(&key.key_id);
                                self.settings_notice = Some((
                                    format!(
                                        "Key settings were not saved; the new Keychain item was removed: {error}"
                                    ),
                                    true,
                                ));
                            }
                        }
                    }
                    Err(error) => {
                        self.settings_notice =
                            Some((format!("Signing key was not created: {error}"), true));
                    }
                }
            }
            settings::SettingsAction::RevokeSigningKey => {
                let key_reference = self.settings.signing_key_reference.clone();
                let mut candidate = self.settings.clone();
                candidate.revoke_signing_key();
                match candidate.save() {
                    Ok(path) => {
                        self.settings = candidate.clone();
                        self.settings_draft = candidate;
                        let deletion = signing::NativeKeychainProvider.delete_key(&key_reference);
                        self.settings_notice = Some(match deletion {
                            Ok(()) => (
                                format!(
                                    "Key fingerprint revoked and private Keychain item deleted; settings saved to {}",
                                    path.display()
                                ),
                                false,
                            ),
                            Err(error) => (
                                format!(
                                    "Fingerprint is revoked and cannot sign. Keychain deletion needs attention: {error}"
                                ),
                                true,
                            ),
                        });
                    }
                    Err(error) => {
                        self.settings_notice = Some((
                            format!("Key was not revoked because settings could not save: {error}"),
                            true,
                        ));
                    }
                }
            }
            settings::SettingsAction::Save => {
                self.settings_draft.telemetry = false;
                self.settings_draft.normalize();
                if let Err(error) = self.settings_draft.validate_runtime() {
                    self.settings_notice = Some((error, true));
                    return;
                }
                let restart_engine =
                    settings::runtime_changed(&self.settings, &self.settings_draft);
                let display_changed = self.settings.colormap != self.settings_draft.colormap
                    || self.settings.cp_range_mode != self.settings_draft.cp_range_mode
                    || self.settings.cp_pinned_extent != self.settings_draft.cp_pinned_extent
                    || self.settings.unit_system != self.settings_draft.unit_system
                    || self.settings.significant_digits != self.settings_draft.significant_digits
                    || self.settings.number_notation != self.settings_draft.number_notation;
                match self.settings_draft.save() {
                    Ok(path) => {
                        self.settings = self.settings_draft.clone();
                        if !nav_is_available(self.nav, self.settings.developer_research_sandbox) {
                            self.nav = Nav::Projects;
                        }
                        apply_with_contrast(
                            ctx,
                            self.settings.theme == settings::ThemeMode::HighContrast,
                        );
                        set_reduced_motion(ctx, self.settings.reduced_motion);
                        ctx.set_zoom_factor(self.settings.ui_scale);
                        field2d::set_view_colormap(self.settings.colormap);
                        self.input_units = self.settings.input_units;
                        self.schedule_autosave_from_now();
                        if display_changed {
                            // Force colormapped textures to rebuild with the
                            // new appearance preferences.
                            self.invalidate_field_textures();
                            if let Some(case) = self.cad.as_mut() {
                                case.workflow.view_state.colormap = Some(
                                    serde_json::to_value(self.settings.colormap)
                                        .ok()
                                        .and_then(|value| value.as_str().map(str::to_owned))
                                        .unwrap_or_else(|| format!("{:?}", self.settings.colormap)),
                                );
                                case.workflow.view_state.cp_range_mode = Some(
                                    serde_json::to_value(self.settings.cp_range_mode)
                                        .ok()
                                        .and_then(|value| value.as_str().map(str::to_owned))
                                        .unwrap_or_else(|| {
                                            format!("{:?}", self.settings.cp_range_mode)
                                        }),
                                );
                                case.workflow.view_state.cp_pinned_extent =
                                    Some(self.settings.cp_pinned_extent);
                                case.workflow.view_state.streamlines = self.streamlines;
                            }
                        }
                        self.settings_notice =
                            Some((format!("Saved locally to {}", path.display()), false));
                        if restart_engine {
                            self.restart_engine();
                        }
                    }
                    Err(error) => {
                        self.settings_notice =
                            Some((format!("Settings were not saved: {error}"), true));
                    }
                }
            }
        }
    }

    fn restart_engine(&mut self) {
        // Drop every engine-backed result before replacing the receiver. This
        // prevents fields, benchmark cells, or recovered surfaces from the old
        // runtime from presenting as revalidated output.
        self.invalidate_engine_results();
        self.engine.terminate();
        self.engine = engine::EngineHandle::spawn_with_config(self.settings.engine_config());
        self.attach_engine_repaint_wake();
        self.engine_ok = false;
        self.dependencies_dirty = true;
        self.engine_status = "○ Restarting engine…".into();
        self.library.busy = true;
        self.model_inventory_state = ModelInventoryState::Loading;
        self.library_pending_request = self.engine.send(engine::Cmd::ListModels).ok();
    }

    fn viewport(&mut self, ui: &mut egui::Ui) {
        if let Some(updater) = &self.updater {
            egui::Panel::top("studio.update-banner")
                .show(ui, |ui| crate::updater::show_compact_banner(ui, updater));
        }
        // C7: the near-black well is for calibrated render viewports only;
        // document screens sit on the BG surface so the elevation ladder
        // (BG → SURFACE → SURFACE_HIGH) stays legible.
        let has_result = self
            .cad
            .as_ref()
            .and_then(|case| case.workflow.result.as_ref())
            .is_some();
        let is_render_screen = uses_scientific_canvas(self.nav, has_result);
        let canvas_fill = if is_render_screen {
            // Settings › Viewport: theme-sanctioned well surfaces only.
            self.settings.viewport_background.color()
        } else {
            BG
        };
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(canvas_fill))
            .show(ui, |ui| {
                let rect = ui.max_rect();
                self.last_render_rect = is_render_screen.then_some(rect);
                // Screen-switch crossfade (§3.7): a BG-colored veil fades out
                // *over* the incoming screen (drawn last, so it also covers
                // the wgpu 3D pass — no full-brightness pop, QA C8); skipped
                // entirely under reduced motion.
                if self.nav != self.last_nav {
                    self.last_nav = self.nav;
                    self.nav_changed_at = ui.input(|input| input.time);
                }
                let veil = if reduced_motion(ui.ctx()) {
                    0.0
                } else {
                    let progress = ((ui.input(|input| input.time) - self.nav_changed_at) / 0.18)
                        .clamp(0.0, 1.0) as f32;
                    if progress < 1.0 {
                        ui.ctx().request_repaint();
                    }
                    1.0 - egui::emath::easing::cubic_out(progress)
                };
                // The decorative 40px background grid is retired (audit A20):
                // it carried no units or scale and read as ornament under
                // real measurement views.

                if self.nav == Nav::Projects {
                    self.project_view(ui);
                }
                if self.nav == Nav::Case {
                    self.engineering_case_view(ui);
                }
                if self.nav == Nav::Results && !has_result {
                    self.engineering_results_empty_view(ui);
                } else if matches!(self.nav, Nav::Results | Nav::Metrics) {
                    if self.nav == Nav::Results && !self.volumetric {
                        self.cad_section_view(ui, rect);
                    } else {
                        let surface_live = self.surface_on
                            && self.cad.as_ref().is_some_and(|c| c.surf.is_some());
                        let q_live = self.q_iso_on
                            && !self.render_volume
                            && !self.q_volume_data.is_empty();
                        let volume_mode = self.volumetric
                            && (self.render_volume || surface_live || q_live);
                        let (vol_data, vol_dims, dens_lo, dens_hi) = if self.render_volume {
                            (
                                self.volume_data.clone(),
                                self.volume_dims,
                                self.density_lo,
                                self.density_hi,
                            )
                        } else if q_live {
                            // Tight TF window → thin vortex cores, not a fog blob.
                            (
                                self.q_volume_data.clone(),
                                self.q_volume_dims,
                                0.78,
                                0.99,
                            )
                        } else {
                            // Surface-only: density gate never opens, raymarch
                            // still shades the Cp body from the surface textures.
                            (
                                self.volume_data.clone(),
                                self.volume_dims,
                                2.0,
                                2.0,
                            )
                        };
                        let opts = viewport::ViewOpts {
                            opacity: self.opacity,
                            density_lo: dens_lo,
                            density_hi: dens_hi,
                            slice: [
                                if self.slice[0] {
                                    Some(self.slice_pos[0])
                                } else {
                                    None
                                },
                                if self.slice[1] {
                                    Some(self.slice_pos[1])
                                } else {
                                    None
                                },
                                if self.slice[2] {
                                    Some(self.slice_pos[2])
                                } else {
                                    None
                                },
                            ],
                            streamlines: self.streamlines,
                            shadows: self.shadows,
                            mode2d: !self.volumetric,
                            gpu: self.gpu_ready,
                            volume_mode,
                            volume: Some(gpu::VolumeData {
                                data: vol_data,
                                dims: vol_dims,
                                version: self.volume_version,
                            }),
                            surface: if self.surface_on {
                                self.cad.as_ref().and_then(|c| c.surf.clone())
                            } else {
                                None
                            },
                            markers: if self.insights_on {
                                self.insights3d
                                    .iter()
                                    .map(|ins| viewport::Marker3D {
                                        pos: ins.pos,
                                        color: insight3d_color(ins.kind),
                                        text: format!("{} {:.3}", ins.kind.glyph(), ins.value),
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            },
                            orbit_sensitivity: self.settings.orbit_sensitivity,
                            invert_scroll_zoom: self.settings.invert_scroll_zoom,
                            show_domain_bounds: self.settings.show_domain_bounds,
                            nav_scheme: self.settings.navigation_scheme,
                            // Frame the body itself, not the tunnel: zoom-to-fit
                            // is only useful if it fits what you imported.
                            fit_bounds: self.cad.as_ref().and_then(|case| case.mask_bounds),
                            snap_to: self.view_snap.take(),
                            fit_now: std::mem::take(&mut self.view_fit),
                            reduced_motion: reduced_motion(ui.ctx()),
                            research_sandbox: self.nav == Nav::Metrics
                                && self.settings.developer_research_sandbox,
                            model_velocity: if self.nav == Nav::Results {
                                self.cad.as_ref().and_then(|case| {
                                    let fields = case.display_fields()?;
                                    Some(viewport::ModelVelocityField {
                                        n: fields.n,
                                        vel: std::sync::Arc::<[f32]>::from(fields.velocity.to_vec()),
                                        mask: Some(std::sync::Arc::<[f32]>::from(
                                            fields.mask.to_vec(),
                                        )),
                                    })
                                })
                            } else {
                                None
                            },
                        };
                        let interaction =
                            viewport::show(ui, rect, &mut self.cam, &opts, &self.particles);
                        if let Some(screen) = interaction.picked {
                            self.probe_surface_at(rect, screen);
                        }
                    }
                }
                if self.nav == Nav::Fields2D {
                    self.field2d_view(ui, rect);
                }
                if self.nav == Nav::FlowPainter {
                    self.painter_view(ui, rect);
                }
                if self.nav == Nav::Benchmark {
                    self.bench_view(ui, rect);
                }
                if self.nav == Nav::Models {
                    let action = library::show_library(
                        ui,
                        &self.models,
                        &self.current_model,
                        &mut self.library,
                    );
                    if let Some(action) = action {
                        self.handle_library_action(action);
                    }
                }
                if self.nav == Nav::Settings {
                    let action = settings::show_settings(
                        ui,
                        &self.settings,
                        &mut self.settings_draft,
                        &mut self.settings_ui,
                        self.updater.as_ref(),
                    );
                    if let Some(action) = action {
                        self.handle_settings_action(ui.ctx(), action);
                    }
                }
                if self.nav == Nav::Evidence {
                    self.engineering_evidence_view(ui);
                }

                let p = ui.painter_at(rect);
                // §4.4 applicability banner: a slim strip above the viewport
                // naming model support status for *this* case — the verdict
                // reads before the pictures (glyph + words, never color-only).
                let mut banner_offset = 0.0;
                if self.nav == Nav::Results {
                    if let Some(case) = self
                        .cad
                        .as_ref()
                        .filter(|case| case.workflow.result.is_some())
                    {
                        let status = case.workflow.model_support.status.as_str();
                        let (glyph, color, text) = if status == "clean" {
                            (
                                "✓",
                                OK,
                                format!(
                                    "Within declared fixed-body support · horizon {} of 1–{} ✓",
                                    case.workflow.operating.horizon_steps,
                                    case.workflow.model_max_steps
                                ),
                            )
                        } else if status.is_empty() {
                            (
                                "○",
                                WARN,
                                "No applicability envelope declared — UNKNOWN".to_owned(),
                            )
                        } else {
                            (
                                "!",
                                WARN,
                                "Model metadata review — support contract compatible, provenance incomplete"
                                    .to_owned(),
                            )
                        };
                        let banner =
                            Rect::from_min_size(rect.min, Vec2::new(rect.width(), 28.0));
                        p.rect_filled(banner, CornerRadius::ZERO, SURFACE);
                        p.hline(banner.x_range(), banner.max.y, Stroke::new(1.0, HAIRLINE));
                        p.text(
                            egui::pos2(banner.min.x + 16.0, banner.center().y),
                            Align2::LEFT_CENTER,
                            glyph,
                            FontId::proportional(12.0),
                            color,
                        );
                        p.text(
                            egui::pos2(banner.min.x + 34.0, banner.center().y),
                            Align2::LEFT_CENTER,
                            text,
                            mono_s().resolve(ui.style()),
                            TEXT_DIM,
                        );
                        banner_offset = 28.0;
                    }
                }
                // camera chip — live azimuth / elevation / zoom (3D only)
                let mut camera_readout = None;
                let show_3d_controls = self.nav == Nav::Metrics
                    || (self.nav == Nav::Results && has_result && self.volumetric);
                if show_3d_controls {
                    let cam_text = format!(
                        "Perspective  ·  az {:>3.0}°  el {:>3.0}°  ·  zoom {:.2}×",
                        self.cam.yaw.to_degrees().rem_euclid(360.0),
                        self.cam.pitch.to_degrees(),
                        viewport::Camera::default().dist / self.cam.dist
                    );
                    let cg = p.layout_no_wrap(cam_text, mono_s().resolve(ui.style()), TEXT_DIM);
                    let chip = Rect::from_min_size(
                        rect.min + Vec2::new(16.0, 16.0 + banner_offset),
                        Vec2::new(cg.size().x + 24.0, 30.0),
                    );
                    p.rect_filled(chip, CornerRadius::same(3), SURFACE);
                    p.rect_stroke(
                        chip,
                        CornerRadius::same(3),
                        Stroke::new(1.0, OUTLINE_VARIANT),
                        egui::StrokeKind::Inside,
                    );
                    p.galley(
                        egui::pos2(chip.min.x + 12.0, chip.center().y - cg.size().y / 2.0),
                        cg,
                        TEXT_DIM,
                    );
                    camera_readout = Some(chip);
                }

                // Camera stations and zoom-to-fit, mirroring the 1–7 / F keys.
                if show_3d_controls {
                    self.viewport_camera_controls(ui, rect, banner_offset, camera_readout);
                    viewport::draw_axis_triad(&p, rect, &self.cam);
                    self.viewport_horizon_controls(ui, rect);
                    self.draw_surface_probe(&p, rect, mono_s().resolve(ui.style()));
                }

                // The floating engine pill is retired: engine state lives in
                // the status bar, the single status home (§4.1).

                // Interaction hint — dropped on short viewports where it
                // would collide with the section legend (QA R7), and
                // suppressible from Settings › Viewport.
                if (self.nav == Nav::Metrics || (self.nav == Nav::Results && has_result))
                    && rect.height() >= 420.0
                    && self.settings.show_viewport_hints
                {
                    // The hint names the bindings of the scheme that is actually
                    // active (Settings › Viewport & camera), so learning the
                    // viewport never requires guessing.
                    let mapping = self.settings.navigation_scheme.mapping();
                    let primary = |binding: &str| {
                        binding
                            .split(',')
                            .next()
                            .unwrap_or(binding)
                            .to_lowercase()
                    };
                    let gestures = format!(
                        "{} to orbit · {} to pan · scroll to zoom",
                        primary(mapping[0].1),
                        primary(mapping[1].1),
                    );
                    let hint = if self.nav == Nav::Results {
                        if self.volumetric {
                            format!("{gestures} · F fits · 1–7 stations · click the body to probe")
                        } else {
                            "stored engineering section · hover to inspect · geometry from active run mask".to_owned()
                        }
                    } else {
                        format!("research sandbox · {gestures} · F fits · G regenerates")
                    };
                    // A hint that runs off both edges teaches nothing, so on a
                    // narrow viewport it drops rather than clipping — the same
                    // bindings are printed in Settings › Keyboard shortcuts.
                    let galley =
                        p.layout_no_wrap(hint, egui::TextStyle::Small.resolve(ui.style()), TEXT_MUTE);
                    if galley.size().x <= rect.width() - 32.0 {
                        p.galley(
                            rect.center_bottom()
                                - Vec2::new(galley.size().x * 0.5, 22.0 + galley.size().y * 0.5),
                            galley,
                            TEXT_MUTE,
                        );
                    }
                }

                // Crossfade veil: painted last so it sits over everything in
                // this panel, including the wgpu paint callback (QA C8).
                if veil > 0.0 {
                    ui.painter()
                        .rect_filled(rect, CornerRadius::ZERO, canvas_fill.gamma_multiply(veil));
                }
            });
    }

    /// Camera stations and zoom-to-fit, top-right of the render viewport. Every
    /// station is named in flow terms and carries its meaning on hover, so the
    /// bindings are learnable without leaving the picture.
    fn viewport_camera_controls(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        banner_offset: f32,
        readout: Option<Rect>,
    ) {
        const STRIP: Vec2 = Vec2::new(320.0, 30.0);
        let mut snap = None;
        let mut fit = false;
        let mut strip = Rect::from_min_size(
            egui::pos2(
                rect.max.x - STRIP.x - 16.0,
                rect.min.y + 16.0 + banner_offset,
            ),
            STRIP,
        );
        // On a narrow viewport the strip would land on top of the camera
        // readout. Take the row underneath instead of overprinting it.
        if let Some(readout) = readout.filter(|chip| strip.min.x < chip.max.x + 12.0) {
            strip = Rect::from_min_size(egui::pos2(strip.min.x, readout.max.y + 8.0), strip.size());
        }
        if !rect.contains(strip.min) || !rect.contains(strip.max) {
            return; // too narrow to place honestly; keys still work
        }
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(strip)
                .layout(Layout::right_to_left(Align::Center)),
            |ui| {
                Frame::NONE
                    .fill(SURFACE)
                    .corner_radius(CornerRadius::same(R2))
                    .stroke(Stroke::new(1.0, OUTLINE_VARIANT))
                    .inner_margin(2)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            for view in viewport::StandardView::ALL.into_iter().rev() {
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(view.short())
                                                .text_style(mono_s())
                                                .color(TEXT_DIM),
                                        )
                                        .fill(Color32::TRANSPARENT)
                                        .stroke(Stroke::NONE)
                                        .corner_radius(CornerRadius::same(R1)),
                                    )
                                    .on_hover_text(format!("{} — {}", view.label(), view.detail()))
                                    .clicked()
                                {
                                    snap = Some(view);
                                }
                            }
                            ui.add(egui::Separator::default().vertical().spacing(6.0));
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(format!("{} Fit", ph::CROSSHAIR))
                                            .text_style(mono_s())
                                            .color(TEXT),
                                    )
                                    .fill(Color32::TRANSPARENT)
                                    .stroke(Stroke::NONE)
                                    .corner_radius(CornerRadius::same(R1)),
                                )
                                .on_hover_text(
                                    "Frame the imported geometry (F). With no geometry loaded it frames the solver domain.",
                                )
                                .clicked()
                            {
                                fit = true;
                            }
                        });
                    });
            },
        );
        if snap.is_some() {
            self.view_snap = snap;
        }
        if fit {
            self.view_fit = true;
        }
    }

    /// Horizon playback for a completed case: step through the model's own
    /// prediction sequence. The label always names the horizon step; physical
    /// time appears only when it can be derived from the recorded run.
    fn viewport_horizon_controls(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let Some(state) = self.cad.as_ref().and_then(|case| {
            case.workflow.result.as_ref()?;
            Some((
                case.display_step(),
                case.steps,
                case.workflow.model_max_steps.max(case.steps).max(1),
                case.playback.playing,
                case.playback.fetching.as_ref().map(|(step, _)| *step),
                case.playback.failed.contains(&case.display_step()),
                case.display_fields().is_some(),
                case.playback.frames.len(),
            ))
        }) else {
            return;
        };
        let (step, recorded_step, max_step, playing, fetching, failed, available, cached) = state;
        if rect.height() < 320.0 || rect.width() < 520.0 {
            return; // no honest room; the keys and Results panel still work
        }
        let bar = Rect::from_min_size(
            egui::pos2(rect.center().x - 250.0, rect.max.y - 76.0),
            Vec2::new(500.0, 56.0),
        );
        let seconds = self.seconds_per_horizon_step();
        let mut requested: Option<u32> = None;
        let mut toggle = false;
        ui.scope_builder(egui::UiBuilder::new().max_rect(bar), |ui| {
            Frame::NONE
                .fill(SURFACE)
                .corner_radius(CornerRadius::same(R2))
                .stroke(Stroke::new(1.0, OUTLINE_VARIANT))
                .inner_margin(Margin::symmetric(10, 7))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(step > 1, egui::Button::new(ph::SKIP_BACK).frame(false))
                            .on_hover_text("Previous horizon step (,)")
                            .on_disabled_hover_text("Already at horizon step 1")
                            .clicked()
                        {
                            requested = Some(step.saturating_sub(1));
                        }
                        if ui
                            .add(
                                egui::Button::new(if playing {
                                    ph::PAUSE
                                } else {
                                    ph::PLAY
                                })
                                .frame(false),
                            )
                            .on_hover_text(if playing {
                                "Pause (Space)"
                            } else {
                                "Play through the horizon (Space) — one model prediction per step, not real time"
                            })
                            .clicked()
                        {
                            toggle = true;
                        }
                        if ui
                            .add_enabled(
                                step < max_step,
                                egui::Button::new(ph::SKIP_FORWARD).frame(false),
                            )
                            .on_hover_text("Next horizon step (.)")
                            .on_disabled_hover_text(format!(
                                "Horizon step {max_step} is the model's declared limit"
                            ))
                            .clicked()
                        {
                            requested = Some(step + 1);
                        }
                        let mut slider_step = step;
                        if ui
                            .add(
                                egui::Slider::new(&mut slider_step, 1..=max_step)
                                    .show_value(false)
                                    .trailing_fill(true),
                            )
                            .changed()
                        {
                            requested = Some(slider_step);
                        }
                    });
                    // One line, always naming what the number means.
                    let mut label = format!("MODEL HORIZON STEP {step} of {max_step}");
                    if let Some(seconds) = seconds {
                        label.push_str(&format!(
                            " · t ≈ {:.4} s",
                            seconds * step as f64
                        ));
                    }
                    let (chip, chip_color) = if step == recorded_step {
                        ("RECORDED RUN", BRAND)
                    } else if fetching == Some(step) {
                        ("FETCHING", WARN)
                    } else if failed {
                        ("UNAVAILABLE", DANGER)
                    } else if available {
                        ("PREVIEW · NOT RECORDED", GOLD)
                    } else {
                        ("NOT COMPUTED", WARN)
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(label).text_style(mono_s()).color(TEXT_DIM));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(chip_text(chip).color(chip_color)).on_hover_text(
                                if step == recorded_step {
                                    "This step is the horizon the immutable run stored.".to_owned()
                                } else {
                                    format!(
                                        "Preview of the model at horizon step {step}. It is displayed only — the recorded run stays at step {recorded_step}. {cached} step(s) cached this session."
                                    )
                                },
                            );
                        });
                    });
                });
        });
        if toggle {
            self.toggle_horizon_playback();
        }
        if let Some(next) = requested {
            if let Some(case) = self.cad.as_mut() {
                case.playback.playing = false;
            }
            self.show_horizon_step(next);
        }
    }

    /// Horizon playback for the completed case, in the inspector so it also
    /// drives the 2D section view. Every step other than the recorded horizon is
    /// a display-only model preview and is labeled as one.
    fn horizon_playback_card(&mut self, ui: &mut egui::Ui) {
        let Some(state) = self.cad.as_ref().and_then(|case| {
            case.workflow.result.as_ref()?;
            let step = case.display_step();
            Some((
                step,
                case.steps,
                case.workflow.model_max_steps.max(case.steps).max(1),
                case.playback.playing,
                case.playback
                    .fetching
                    .as_ref()
                    .map(|(fetching, _)| *fetching),
                case.playback.failed.contains(&step),
                case.playback.frames.len(),
                case.playback
                    .frames
                    .get(&step)
                    .map(|frame| (frame.force_coefficients, frame.cp_min, frame.cp_max)),
            ))
        }) else {
            return;
        };
        let (step, recorded_step, max_step, playing, fetching, failed, cached, preview) = state;
        let seconds = self.seconds_per_horizon_step();
        let format = self.settings.value_format();
        let mut requested: Option<u32> = None;
        let mut toggle = false;
        card(ui, |ui| {
            ui.label(caps("Horizon playback"));
            ui.label(
                RichText::new(
                    "Each step is one model prediction at that lead time, not a time integration. Step controls also drive the 2D section view.",
                )
                .text_style(caption())
                .color(TEXT_MUTE),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(step > 1, egui::Button::new(ph::SKIP_BACK))
                    .on_hover_text("Previous horizon step (,)")
                    .on_disabled_hover_text("Already at horizon step 1")
                    .clicked()
                {
                    requested = Some(step.saturating_sub(1));
                }
                if ui
                    .add(egui::Button::new(if playing { ph::PAUSE } else { ph::PLAY }))
                    .on_hover_text(if playing {
                        "Pause (Space)"
                    } else {
                        "Play through the horizon (Space). Uncached steps are predicted on demand, so playback waits for the engine."
                    })
                    .clicked()
                {
                    toggle = true;
                }
                if ui
                    .add_enabled(step < max_step, egui::Button::new(ph::SKIP_FORWARD))
                    .on_hover_text("Next horizon step (.)")
                    .on_disabled_hover_text(format!(
                        "Horizon step {max_step} is the model's declared limit"
                    ))
                    .clicked()
                {
                    requested = Some(step + 1);
                }
                let mut slider_step = step;
                if ui
                    .add(
                        egui::Slider::new(&mut slider_step, 1..=max_step)
                            .show_value(false)
                            .trailing_fill(true),
                    )
                    .on_hover_text("Scrub the model horizon")
                    .changed()
                {
                    requested = Some(slider_step);
                }
            });
            ui.add_space(6.0);
            let (chip, chip_color, note) = if step == recorded_step {
                (
                    "RECORDED RUN",
                    BRAND,
                    "This step is the horizon the immutable run stored; it is the only step in the evidence chain.".to_owned(),
                )
            } else if fetching == Some(step) {
                (
                    "FETCHING",
                    WARN,
                    "The engine is predicting this step. The recorded run is unaffected."
                        .to_owned(),
                )
            } else if failed {
                (
                    "UNAVAILABLE",
                    DANGER,
                    "This step could not be fetched. Nothing is shown for it rather than showing another step's field.".to_owned(),
                )
            } else if preview.is_some() {
                (
                    "PREVIEW · NOT RECORDED",
                    GOLD,
                    format!("Display-only model preview at horizon step {step}. The recorded run stays at step {recorded_step}."),
                )
            } else {
                (
                    "NOT COMPUTED",
                    WARN,
                    "This step has not been predicted yet.".to_owned(),
                )
            };
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("STEP {step} of {max_step}"))
                        .text_style(mono_s())
                        .color(TEXT),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(chip_text(chip).color(chip_color))
                        .on_hover_text(note);
                });
            });
            ui.add_space(6.0);
            match seconds {
                Some(seconds) => diag(
                    ui,
                    "Lead time",
                    &format!("{:.4} s after the developed state", seconds * step as f64),
                    TEXT_DIM,
                ),
                None => diag(
                    ui,
                    "Lead time",
                    "not stated · declare units and a complete operating point",
                    TEXT_MUTE,
                ),
            }
            // Per-step coefficients, only for steps we actually hold.
            if let Some((coefficients, cp_min, cp_max)) = preview {
                measure_row(
                    ui,
                    "Cd at this step",
                    &units::format_value(coefficients[0] as f64, format),
                    "–",
                    "PREVIEW",
                    GOLD,
                )
                .on_hover_text(engineering::COEFFICIENT_REFERENCE_FRAME);
                measure_row(
                    ui,
                    "Cl at this step",
                    &units::format_value(coefficients[2] as f64, format),
                    "–",
                    "PREVIEW",
                    GOLD,
                );
                measure_row(
                    ui,
                    "Cp range at this step",
                    &format!(
                        "{} … {}",
                        units::format_value(cp_min as f64, format),
                        units::format_value(cp_max as f64, format)
                    ),
                    "–",
                    "PREVIEW",
                    GOLD,
                );
            }
            ui.label(
                RichText::new(format!(
                    "{cached} step(s) cached this session · previews are memory-only and are dropped when the case changes"
                ))
                .text_style(caption())
                .color(TEXT_MUTE),
            );
        });
        if toggle {
            self.toggle_horizon_playback();
        }
        if let Some(next) = requested {
            if let Some(case) = self.cad.as_mut() {
                case.playback.playing = false;
            }
            self.show_horizon_step(next);
        }
    }

    /// The 3D surface probe readout, anchored to the picked point.
    fn draw_surface_probe(&self, painter: &egui::Painter, rect: Rect, font: egui::FontId) {
        let Some(probe) = self.probe3d.as_ref() else {
            return;
        };
        let format = self.settings.value_format();
        let system = self.settings.unit_system;
        let traction = (probe.traction_pa[0].powi(2)
            + probe.traction_pa[1].powi(2)
            + probe.traction_pa[2].powi(2))
        .sqrt();
        let mut lines = vec![
            format!(
                "Cp {}  · DERIVED",
                units::format_value(probe.cp as f64, format)
            ),
            format!(
                "p  {}  · RECOVERED",
                units::format_quantity(
                    units::Quantity::Pressure,
                    probe.pressure_pa as f64,
                    system,
                    format
                )
            ),
            format!(
                "|t| {}  · DERIVED",
                units::format_quantity(units::Quantity::Pressure, traction as f64, system, format)
            ),
        ];
        lines.push(match probe.source_m {
            Some(point) => format!(
                "at [{:.4}, {:.4}, {:.4}] m · source frame",
                point[0], point[1], point[2]
            ),
            None => format!(
                "at cell [{}, {}, {}] · source frame unavailable",
                probe.cell[0], probe.cell[1], probe.cell[2]
            ),
        });
        lines.push(if probe.recorded {
            format!("horizon step {} · recorded run", probe.step)
        } else {
            format!("horizon step {} · preview, not recorded", probe.step)
        });
        let galleys: Vec<_> = lines
            .iter()
            .map(|line| painter.layout_no_wrap(line.clone(), font.clone(), TEXT))
            .collect();
        let width = galleys
            .iter()
            .fold(0.0f32, |widest, galley| widest.max(galley.size().x))
            + 22.0;
        let height = galleys.len() as f32 * (font.size + 4.0) + 18.0;
        // Re-project every frame so the readout follows the picked point. When
        // the point swings behind the camera there is nothing honest to anchor
        // to, so the readout steps aside rather than floating somewhere wrong.
        let Some((screen, _)) = self.cam.project(rect, probe.anchor) else {
            return;
        };
        let anchor = egui::pos2(
            (screen.x + 14.0).clamp(rect.min.x + 8.0, (rect.max.x - width - 8.0).max(rect.min.x)),
            (screen.y + 14.0).clamp(
                rect.min.y + 8.0,
                (rect.max.y - height - 8.0).max(rect.min.y),
            ),
        );
        let card = Rect::from_min_size(anchor, Vec2::new(width, height));
        painter.rect_filled(card, CornerRadius::same(R2), SURFACE);
        painter.rect_stroke(
            card,
            CornerRadius::same(R2),
            Stroke::new(1.0, OUTLINE),
            egui::StrokeKind::Inside,
        );
        painter.circle_stroke(screen, 5.0, Stroke::new(1.5, BRAND));
        painter.line_segment(
            [screen, egui::pos2(card.min.x, card.min.y)],
            Stroke::new(1.0, OUTLINE_VARIANT),
        );
        for (index, galley) in galleys.into_iter().enumerate() {
            painter.galley(
                egui::pos2(
                    card.min.x + 11.0,
                    card.min.y + 9.0 + index as f32 * (font.size + 4.0),
                ),
                galley,
                TEXT_DIM,
            );
        }
    }

    fn ensure_cad_section_texture(&mut self, ctx: &egui::Context) {
        let Some(case) = self.cad.as_ref() else {
            self.invalidate_cad_section();
            self.section_error = Some("No active engineering result is available.".into());
            return;
        };
        // The section follows horizon playback: it renders the step on screen,
        // and says so rather than showing another step's data under this label.
        let step = case.display_step();
        let Some(fields) = case.display_fields() else {
            let pending = case
                .playback
                .fetching
                .as_ref()
                .is_some_and(|(fetching, _)| *fetching == step);
            self.section_tex = None;
            self.section_data = None;
            self.section_sig = u64::MAX;
            self.section_error = Some(if pending {
                format!("Horizon step {step} is being predicted…")
            } else {
                format!(
                    "Horizon step {step} has not been computed. Use the horizon controls to predict it."
                )
            });
            return;
        };
        let axis_index = self.section_axis.id() as usize;
        let location = self.slice_pos[axis_index];
        let index = match engineering_section::section_index(fields.n, location) {
            Ok(index) => index,
            Err(error) => {
                self.invalidate_cad_section();
                self.section_error = Some(error);
                return;
            }
        };
        let signature = self.cad_version.wrapping_mul(131)
            ^ self.section_axis.id().wrapping_mul(17)
            ^ self.section_quantity.id().wrapping_mul(31)
            ^ (index as u64).wrapping_mul(47)
            ^ (step as u64).wrapping_mul(1_000_003);
        if signature == self.section_sig
            && (self.section_tex.is_some() || self.section_error.is_some())
        {
            return;
        }
        let reference_length_m = case.workflow.operating.reference_length
            * case
                .workflow
                .operating
                .length_unit
                .meters_per_unit()
                .unwrap_or(0.0);
        let input = engineering_section::SectionInput {
            n: fields.n,
            velocity: fields.velocity,
            pressure_pa: fields.pressure,
            mask: fields.mask,
            cp: fields.cp,
            traction_pa: fields.traction,
            free_stream_mps: case.workflow.operating.velocity as f32,
            reference_pressure_pa: case.workflow.operating.reference_pressure as f32,
            reference_length_m: reference_length_m as f32,
            solver_characteristic_length: case.workflow.preflight.solver_characteristic_length
                as f32,
        };
        match engineering_section::extract_section(
            &input,
            self.section_axis,
            location,
            self.section_quantity,
        ) {
            Ok(mut section) => {
                // Settings › Appearance: pinned symmetric Cp range so
                // sections compare across runs. The stored field is untouched.
                if self.section_quantity == engineering_section::SectionQuantity::PhysicalCp
                    && self.settings.cp_range_mode == settings::CpRangeMode::Pinned
                {
                    section.scale = section.scale.pinned(self.settings.cp_pinned_extent as f32);
                }
                let image = engineering_section_image(&section);
                self.section_tex = Some(ctx.load_texture(
                    format!("engineering.section.{signature}"),
                    image,
                    egui::TextureOptions::NEAREST,
                ));
                self.section_data = Some(section);
                self.section_error = None;
                self.section_sig = signature;
            }
            Err(error) => {
                self.section_tex = None;
                self.section_data = None;
                self.section_error = Some(error);
                self.section_sig = signature;
            }
        }
    }

    /// Geometry-linked 2D evidence from the active immutable engineering field.
    /// No standalone/sandbox Field2D state participates in this path.
    fn cad_section_view(&mut self, ui: &mut egui::Ui, rect: Rect) {
        self.ensure_cad_section_texture(ui.ctx());
        let painter = ui.painter_at(rect);
        let (Some(texture), Some(section)) =
            (self.section_tex.as_ref(), self.section_data.as_ref())
        else {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                self.section_error
                    .as_deref()
                    .unwrap_or("Stored engineering section is unavailable."),
                FontId::proportional(14.0),
                GOLD,
            );
            return;
        };

        let header_origin = rect.min + Vec2::new(18.0, 17.0);
        painter.text(
            header_origin,
            Align2::LEFT_TOP,
            format!(
                "{} · {}",
                section.quantity.label(),
                section.quantity.units()
            ),
            FontId::proportional(15.0),
            TEXT,
        );
        painter.text(
            header_origin + Vec2::new(0.0, 23.0),
            Align2::LEFT_TOP,
            section.quantity.source(),
            FontId::monospace(10.0),
            GOLD,
        );
        painter.text(
            header_origin + Vec2::new(0.0, 41.0),
            Align2::LEFT_TOP,
            section.quantity.method(),
            FontId::proportional(10.5),
            TEXT_DIM,
        );

        let available = Rect::from_min_max(
            rect.min + Vec2::new(46.0, 88.0),
            rect.max - Vec2::new(46.0, 168.0),
        );
        let side = available.width().min(available.height()).max(1.0);
        let panel = Rect::from_center_size(available.center(), Vec2::splat(side));
        let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        painter.image(texture.id(), panel, uv, Color32::WHITE);
        painter.rect_stroke(
            panel,
            CornerRadius::same(2),
            Stroke::new(1.0, OUTLINE),
            egui::StrokeKind::Outside,
        );
        draw_section_cp_profile(&painter, rect, panel, section);
        painter.text(
            egui::pos2(panel.center().x, panel.max.y + 12.0),
            Align2::CENTER_TOP,
            format!("+{} →", section.axis.horizontal_axis()),
            FontId::monospace(10.0),
            TEXT_DIM,
        );
        painter.text(
            egui::pos2(panel.min.x - 10.0, panel.min.y),
            Align2::RIGHT_TOP,
            format!("↑ +{}", section.axis.vertical_axis()),
            FontId::monospace(10.0),
            TEXT_DIM,
        );
        painter.text(
            egui::pos2(panel.max.x, panel.min.y - 8.0),
            Align2::RIGHT_BOTTOM,
            format!(
                "{} SECTION · location {:.3} · cell {}/{}",
                section.axis.label(),
                section.location,
                section.index,
                section.n - 1
            ),
            FontId::monospace(10.0),
            TEXT_MUTE,
        );
        let geometry_label = "■ STORED CAD MASK · solid section";
        painter.text(
            egui::pos2(panel.min.x, panel.min.y - 8.0),
            Align2::LEFT_BOTTOM,
            geometry_label,
            FontId::monospace(9.5),
            BRAND,
        );
        engineering_section_legend(&painter, panel, section);

        if let Some(position) = ui.input(|input| input.pointer.hover_pos()) {
            if panel.contains(position) {
                draw_engineering_section_probe(&painter, panel, rect, section, position);
            }
        }
    }

    /// Central 2D field render: colormapped model prediction and, when enabled,
    /// solver reference plus derived error on a shared scale. The instrument
    /// layer adds recovered-pressure extrema, max |ω|, max speed, max error, a
    /// live hover probe, and a calibrated legend.
    fn field2d_view(&mut self, ui: &mut egui::Ui, rect: Rect) {
        self.ensure_f2d_textures(ui.ctx());
        self.ensure_f2d_insights();
        let p = ui.painter_at(rect);
        if self.f2d_tex.is_empty() {
            let t = if self.f2d_pending {
                "predicting…"
            } else {
                "no field"
            };
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                t,
                FontId::proportional(15.0),
                TEXT_MUTE,
            );
            return;
        }
        let pad = 30.0;
        let avail = Rect::from_min_max(
            rect.min + Vec2::splat(pad),
            rect.max - Vec2::new(pad, pad + 52.0),
        );
        let n = self.f2d_tex.len();
        let gap = 16.0;
        let cell_w = (avail.width() - gap * (n as f32 - 1.0)) / n as f32;
        let side = cell_w.min(avail.height());
        let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        let titles = ["Model prediction", "Solver reference", "|error|"];
        let mut panels: Vec<Rect> = Vec::with_capacity(n);
        for (k, tex) in self.f2d_tex.iter().enumerate() {
            let x0 = avail.min.x + k as f32 * (cell_w + gap) + (cell_w - side) / 2.0;
            let y0 = avail.min.y + (avail.height() - side) / 2.0;
            let r = Rect::from_min_size(egui::pos2(x0, y0), Vec2::splat(side));
            p.image(tex.id(), r, uv, Color32::WHITE);
            p.rect_stroke(
                r,
                CornerRadius::same(3),
                Stroke::new(1.0, OUTLINE_VARIANT),
                egui::StrokeKind::Outside,
            );
            if n > 1 {
                p.text(
                    egui::pos2(r.center().x, r.max.y + 12.0),
                    Align2::CENTER_CENTER,
                    titles[k.min(2)],
                    FontId::proportional(12.0),
                    TEXT_DIM,
                );
            }
            panels.push(r);
        }

        let Some(f) = self.f2d.as_ref() else { return };

        // calibrated legend under the first panel (shared model/reference scale)
        legend_bar(&p, panels[0], self.f2d_scale, self.f2d_signed);

        // Auto-pinned critical points: field classes on the model panel and the
        // error hotspot on the |error| panel when the reference is shown.
        if self.insights_on {
            let mut chips: Vec<Rect> = Vec::new();
            for ins in &self.f2d_insights {
                let class = ins.kind as usize;
                if !self.insight_classes.get(class).copied().unwrap_or(true) {
                    continue;
                }
                let panel = match ins.kind {
                    field2d::InsightKind::MaxError => match panels.get(2) {
                        Some(r) => *r,
                        None => continue,
                    },
                    _ => panels[0],
                };
                draw_insight(&p, panel, f.n, ins, &mut chips);
            }
        }

        // live probe: hover any field panel for the full readout at that cell
        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
            for (k, r) in panels.iter().enumerate().take(2) {
                if r.contains(pos) {
                    let src: &[f32] = if k == 1 {
                        match f.truth.as_deref() {
                            Some(t) => t,
                            None => &f.ai,
                        }
                    } else {
                        &f.ai
                    };
                    draw_probe(&p, *r, rect, f.n, src, pos);
                    break;
                }
            }
        }

        let cap = format!(
            "{}  ·  {}  ·  t = {:.2}s ({} steps)",
            f.scenario,
            self.f2d_var.label(),
            f.horizon as f32 * f.dt_frame,
            f.horizon
        );
        p.text(
            rect.center_bottom() - Vec2::new(0.0, 14.0),
            Align2::CENTER_CENTER,
            cap,
            FontId::proportional(12.5),
            TEXT_MUTE,
        );
    }

    /// N4 — the painting canvas: ω as a diverging-colormap texture, left drag
    /// paints +ω, right drag −ω, with stroke interpolation and a brush preview.
    fn painter_view(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let n = painter::N;
        // canvas texture (rebuilt only when a stroke/preset changed it)
        if self.paint_dirty || self.paint_tex.is_none() {
            let scale = self
                .paint
                .omega
                .iter()
                .fold(1e-3f32, |m, &w| m.max(w.abs()));
            let pixels: Vec<Color32> = self
                .paint
                .omega
                .iter()
                .map(|&w| field2d::colormap_color(w / scale, true))
                .collect();
            let img = egui::ColorImage {
                size: [n, n],
                pixels,
                source_size: Vec2::new(n as f32, n as f32),
            };
            self.paint_tex = Some(ui.ctx().load_texture(
                "painter.omega",
                img,
                egui::TextureOptions::NEAREST,
            ));
            self.paint_dirty = false;
        }

        let pad = 30.0;
        let avail = Rect::from_min_max(
            rect.min + Vec2::splat(pad),
            rect.max - Vec2::new(pad, pad + 24.0),
        );
        let side = avail.width().min(avail.height());
        let panel = Rect::from_center_size(avail.center(), Vec2::splat(side));
        let p = ui.painter_at(rect);
        if let Some(tex) = &self.paint_tex {
            let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            p.image(tex.id(), panel, uv, Color32::WHITE);
        }
        p.rect_stroke(
            panel,
            CornerRadius::same(3),
            Stroke::new(1.0, OUTLINE_VARIANT),
            egui::StrokeKind::Outside,
        );

        // painting interaction (primary = +ω, secondary = −ω), stroke-interpolated
        let resp = ui.interact(panel, ui.id().with("paint.canvas"), Sense::click_and_drag());
        let sign = if resp.dragged_by(egui::PointerButton::Primary) {
            Some(1.0f32)
        } else if resp.dragged_by(egui::PointerButton::Secondary) {
            Some(-1.0f32)
        } else {
            None
        };
        if let (Some(sign), Some(pos)) = (sign, resp.interact_pointer_pos()) {
            let gx = ((pos.x - panel.min.x) / panel.width() * n as f32).clamp(0.0, n as f32 - 1.0);
            let gy = ((pos.y - panel.min.y) / panel.height() * n as f32).clamp(0.0, n as f32 - 1.0);
            let step = (self.brush_radius * 0.4).max(0.75);
            let (mut sx, mut sy) = self.paint_last.unwrap_or((gx, gy));
            let dist = ((gx - sx).powi(2) + (gy - sy).powi(2)).sqrt();
            let steps = (dist / step).ceil().max(1.0) as usize;
            for k in 1..=steps {
                let t = k as f32 / steps as f32;
                let (x, y) = (sx + (gx - sx) * t, sy + (gy - sy) * t);
                self.paint.stamp(
                    x,
                    y,
                    self.brush_radius,
                    self.brush_strength * 0.25,
                    sign,
                    self.paint_sym,
                );
            }
            (sx, sy) = (gx, gy);
            self.paint_last = Some((sx, sy));
            self.paint_dirty = true;
        } else {
            self.paint_last = None;
        }
        // brush preview ring under the cursor
        if let Some(pos) = resp.hover_pos() {
            let r = self.brush_radius / n as f32 * panel.width();
            p.circle_stroke(pos, r, Stroke::new(1.0, TEXT_DIM.gamma_multiply(0.8)));
        }

        // live diagnostics chip (top-left, instrument style)
        let (proj_line, proj_col) = if self.paint.velocity.is_some() {
            (
                format!(
                    "div {:.1e} · energy {:.3} · CG {}",
                    self.paint.div_max, self.paint.energy, self.paint.cg_iters
                ),
                if self.paint.div_max < 1e-6 {
                    SUCCESS
                } else {
                    GOLD
                },
            )
        } else {
            ("unprojected — apply Leray projection".into(), GOLD)
        };
        let lines = [
            (
                format!("enstrophy {:.4}", self.paint.mean_enstrophy()),
                TEXT_DIM,
            ),
            (proj_line, proj_col),
        ];
        let font = FontId::monospace(11.0);
        let galleys: Vec<_> = lines
            .iter()
            .map(|(t, c)| p.layout_no_wrap(t.clone(), font.clone(), *c))
            .collect();
        let w = galleys.iter().map(|g| g.size().x).fold(0.0, f32::max) + 24.0;
        let chip = Rect::from_min_size(rect.min + Vec2::new(16.0, 16.0), Vec2::new(w, 44.0));
        p.rect_filled(chip, CornerRadius::same(3), SURFACE);
        p.rect_stroke(
            chip,
            CornerRadius::same(3),
            Stroke::new(1.0, OUTLINE_VARIANT),
            egui::StrokeKind::Inside,
        );
        for (k, g) in galleys.into_iter().enumerate() {
            p.galley(
                chip.min + Vec2::new(12.0, 7.0 + k as f32 * 16.0),
                g,
                lines[k].1,
            );
        }

        p.text(
            rect.center_bottom() - Vec2::new(0.0, 14.0),
            Align2::CENTER_CENTER,
            format!("left drag +ω  ·  right drag −ω  ·  {n}² grid"),
            FontId::proportional(12.5),
            TEXT_MUTE,
        );
    }

    fn controls_painter(&mut self, ui: &mut egui::Ui) {
        ui.label(title_text("Flow Painter"));
        ui.add_space(4.0);
        ui.label(
            RichText::new("paint vorticity → project divergence-free → generate")
                .size(11.5)
                .color(TEXT_MUTE),
        );
        ui.add_space(16.0);

        ui.label(caps("Brush"));
        ui.add_space(8.0);
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Radius").color(TEXT_DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(mono(&format!("{:.0} cells", self.brush_radius), TEXT_DIM).size(12.0));
                });
            });
            ui.spacing_mut().slider_width = ui.available_width() - 8.0;
            ui.add(
                egui::Slider::new(&mut self.brush_radius, 2.0..=24.0)
                    .show_value(false)
                    .trailing_fill(true),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Strength").color(TEXT_DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(mono(&format!("{:.1}", self.brush_strength), TEXT_DIM).size(12.0));
                });
            });
            ui.spacing_mut().slider_width = ui.available_width() - 8.0;
            ui.add(
                egui::Slider::new(&mut self.brush_strength, 0.2..=3.0)
                    .show_value(false)
                    .trailing_fill(true),
            );
        });

        ui.add_space(14.0);
        ui.label(caps("Symmetry"));
        ui.add_space(8.0);
        card(ui, |ui| {
            ui.checkbox(
                &mut self.paint_sym.mirror_h,
                RichText::new("Mirror left–right").color(TEXT_DIM),
            );
            ui.checkbox(
                &mut self.paint_sym.mirror_v,
                RichText::new("Mirror top–bottom").color(TEXT_DIM),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Radial fold").color(TEXT_DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let label = if self.paint_sym.radial <= 1 {
                        "off".into()
                    } else {
                        format!("{}-fold", self.paint_sym.radial)
                    };
                    ui.label(mono(&label, TEXT_DIM).size(12.0));
                });
            });
            ui.spacing_mut().slider_width = ui.available_width() - 8.0;
            ui.add(
                egui::Slider::new(&mut self.paint_sym.radial, 1..=8)
                    .show_value(false)
                    .trailing_fill(true),
            );
            ui.label(
                RichText::new("mirrors counter-rotate (ω is a pseudoscalar)")
                    .text_style(caption())
                    .color(TEXT_MUTE),
            );
        });

        ui.add_space(14.0);
        ui.label(caps("Presets"));
        ui.add_space(8.0);
        card(ui, |ui| {
            let w = ui.available_width();
            if action_button(
                ui,
                None,
                "Vortex Pair",
                SURFACE_HIGH,
                TEXT,
                Some(OUTLINE),
                30.0,
                w,
            ) {
                self.paint.preset_vortex_pair();
                self.paint_dirty = true;
            }
            ui.add_space(4.0);
            if action_button(
                ui,
                None,
                "Shear Layer",
                SURFACE_HIGH,
                TEXT,
                Some(OUTLINE),
                30.0,
                w,
            ) {
                self.paint.preset_shear_layer();
                self.paint_dirty = true;
            }
            ui.add_space(4.0);
            if action_button(
                ui,
                None,
                "Kármán Street",
                SURFACE_HIGH,
                TEXT,
                Some(OUTLINE),
                30.0,
                w,
            ) {
                self.paint.preset_karman_street();
                self.paint_dirty = true;
            }
            ui.add_space(4.0);
            if action_button(
                ui,
                None,
                "Clear Canvas",
                Color32::TRANSPARENT,
                TEXT_MUTE,
                Some(OUTLINE_VARIANT),
                28.0,
                w,
            ) {
                self.paint.clear();
                self.paint_dirty = true;
            }
        });

        ui.add_space(14.0);
        ui.label(caps("Projection"));
        ui.add_space(8.0);
        card(ui, |ui| {
            if action_button(
                ui,
                None,
                "Apply Leray projection",
                SURFACE_HIGH,
                TEXT,
                Some(OUTLINE),
                32.0,
                ui.available_width(),
            ) {
                self.paint.project(1e-10, 4000);
            }
            if self.paint.velocity.is_some() {
                ui.add_space(8.0);
                let ok = self.paint.div_max < 1e-6;
                diag(
                    ui,
                    "Divergence check",
                    &format!("{:.1e}", self.paint.div_max),
                    if ok { SUCCESS } else { GOLD },
                );
                diag(
                    ui,
                    "Total energy",
                    &format!("{:.4}", self.paint.energy),
                    BRAND,
                );
                diag(
                    ui,
                    "Mean enstrophy",
                    &format!("{:.4}", self.paint.mean_enstrophy()),
                    BRAND,
                );
                diag(
                    ui,
                    "CG iterations",
                    &format!("{}", self.paint.cg_iters),
                    TEXT_DIM,
                );
            } else {
                ui.add_space(4.0);
                ui.label(
                    RichText::new("ω → ψ → u: divergence-free by construction")
                        .text_style(caption())
                        .color(TEXT_MUTE),
                );
            }
        });

        ui.add_space(16.0);
        if action_button(
            ui,
            Some(ph::PLAY),
            "Generate flow",
            EMBER,
            ON_EMBER,
            None,
            44.0,
            ui.available_width(),
        ) {
            self.generate_flow();
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new("auto-projects if needed · prefers the unified (mask-free) model")
                .text_style(caption())
                .color(TEXT_MUTE),
        );
    }

    /// Commit the painted IC: project if stale, hand it to the model (the
    /// unified checkpoint when available — the one trained for mask-free flow),
    /// and jump to Fields (2D) where TimeJump scrubs it.
    fn generate_flow(&mut self) {
        if self.paint.mean_enstrophy() <= 0.0 {
            return;
        } // empty canvas
        if self.paint.velocity.is_none() {
            self.paint.project(1e-10, 4000);
        }
        let Some(ic) = self.paint.ic_payload() else {
            return;
        };
        self.f2d_painted = Some(std::sync::Arc::new(ic));
        if let Some(model) = self
            .models
            .iter()
            .find(|model| model.dimension == 2 && model.id.contains("unified"))
        {
            self.f2d_model = model.id.clone();
        }
        self.f2d_truth = false; // a painted IC has no solver reference
        self.f2d = None;
        self.f2d_tex.clear();
        self.f2d_sig = u64::MAX;
        self.nav = Nav::Fields2D;
        self.request_2d();
    }

    fn select_bench_cell(&mut self, seed_index: usize, horizon_index: usize) {
        if self.project.availability().is_read_only_evidence() {
            self.bench_inspector_error = Some(
                "New cell inspection is blocked in read-only evidence mode; stored inspector evidence remains available."
                    .into(),
            );
            return;
        }
        if self.bench_inspector_pending {
            return;
        }
        let Some(bench) = self.bench.as_ref() else {
            return;
        };
        let Some(&seed) = bench.seeds.get(seed_index) else {
            return;
        };
        let Some(&horizon) = bench.horizons.get(horizon_index) else {
            return;
        };
        self.bench_selected = Some((seed_index, horizon_index));
        self.bench_inspector = None;
        self.bench_inspector_error = None;
        self.bench_tex.clear();
        if !self.engine_ok {
            self.bench_inspector_error =
                Some("engine unavailable; suite values remain inspectable".into());
            return;
        }
        if self
            .engine
            .tx
            .send(engine::Cmd::InspectBenchmarkCell {
                model: bench.model.clone(),
                seed,
                horizon,
            })
            .is_ok()
        {
            self.bench_inspector_pending = true;
        } else {
            self.bench_inspector_error = Some("engine worker is no longer available".into());
        }
    }

    fn bench_keyboard(&mut self, ui: &egui::Ui) {
        if ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        let Some(bench) = self.bench.as_ref() else {
            return;
        };
        if bench.seeds.is_empty() || bench.horizons.is_empty() || self.bench_inspector_pending {
            return;
        }
        let (mut seed_index, mut horizon_index) = self.bench_selected.unwrap_or((0, 0));
        let old = (seed_index, horizon_index);
        ui.input(|input| {
            if input.key_pressed(egui::Key::ArrowUp) {
                seed_index = seed_index.saturating_sub(1);
            }
            if input.key_pressed(egui::Key::ArrowDown) {
                seed_index = (seed_index + 1).min(bench.seeds.len() - 1);
            }
            if input.key_pressed(egui::Key::ArrowLeft) {
                horizon_index = horizon_index.saturating_sub(1);
            }
            if input.key_pressed(egui::Key::ArrowRight) {
                horizon_index = (horizon_index + 1).min(bench.horizons.len() - 1);
            }
        });
        if (seed_index, horizon_index) != old {
            self.select_bench_cell(seed_index, horizon_index);
        } else if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
            self.bench_inspector = None;
            self.bench_tex.clear();
            self.select_bench_cell(seed_index, horizon_index);
        }
    }

    fn ensure_bench_textures(&mut self, ctx: &egui::Context) {
        if self.bench_tex.len() == 3 {
            return;
        }
        let Some(cell) = self.bench_inspector.as_ref() else {
            return;
        };
        let Some(maps) = cell.maps.get(self.bench_var) else {
            return;
        };
        let Some((comparison_scale, error_scale)) = cell.maps.scales(self.bench_var) else {
            return;
        };
        let signed = self.bench_var.signed();
        let key = self.bench_var.key();
        let n = cell.maps.n();
        debug_assert_eq!(n, cell.n);
        let opts = egui::TextureOptions::NEAREST;
        self.bench_tex = vec![
            ctx.load_texture(
                format!("bench.model.{key}.{}.{}", cell.seed, cell.horizon),
                benchmark_map_image(&maps.model, n, comparison_scale, signed),
                opts,
            ),
            ctx.load_texture(
                format!("bench.reference.{key}.{}.{}", cell.seed, cell.horizon),
                benchmark_map_image(&maps.reference, n, comparison_scale, signed),
                opts,
            ),
            ctx.load_texture(
                format!("bench.error.{key}.{}.{}", cell.seed, cell.horizon),
                benchmark_map_image(&maps.error, n, error_scale, signed),
                opts,
            ),
        ];
    }

    /// N5 — the Model Suite Analysis: seeds × horizons RelL2 vs the solver, with
    /// the persistence floor per cell. The Reyn Verify seed.
    fn bench_view(&mut self, ui: &mut egui::Ui, rect: Rect) {
        self.ensure_bench_textures(ui.ctx());
        let p = ui.painter_at(rect);
        let Some(b) = self.bench.as_ref() else {
            let (title, detail, color) = if self.bench_running {
                (
                    "RUNNING MODEL SUITE",
                    "generating solver trajectories and classifying provenance; the interface remains live",
                    EMBER,
                )
            } else if let Some(error) = &self.bench_error {
                ("SUITE UNAVAILABLE", error.as_str(), DATA_RED)
            } else if !self.engine_ok {
                ("ENGINE UNAVAILABLE", "Benchmark Lab remains responsive; restore the Python engine, then run a suite.", GOLD)
            } else {
                (
                    "NO SUITE EVIDENCE",
                    "Choose exact benchmark seeds and run the full suite from the right panel.",
                    TEXT_MUTE,
                )
            };
            let empty =
                Rect::from_center_size(rect.center(), Vec2::new(rect.width().min(560.0), 116.0));
            p.rect_filled(empty, CornerRadius::same(3), SURFACE);
            p.rect_stroke(
                empty,
                CornerRadius::same(3),
                Stroke::new(1.0, OUTLINE_VARIANT),
                egui::StrokeKind::Inside,
            );
            p.text(
                empty.center_top() + Vec2::new(0.0, 27.0),
                Align2::CENTER_TOP,
                title,
                FontId::monospace(12.0),
                color,
            );
            p.text(
                empty.center() + Vec2::new(0.0, 14.0),
                Align2::CENTER_CENTER,
                detail,
                FontId::proportional(12.0),
                TEXT_MUTE,
            );
            return;
        };
        let pad = 30.0;
        let x0 = rect.min.x + pad;
        let mut y = rect.min.y + 42.0;
        let content_w = (rect.width() - 2.0 * pad).max(600.0);

        // header: model · grid · epoch · global RelL2 · runtime
        let stem = b.model.trim_end_matches(".reynmodel");
        p.text(
            egui::pos2(x0, y),
            Align2::LEFT_TOP,
            format!("Model Suite Analysis — {stem}"),
            FontId::proportional(18.0),
            TEXT,
        );
        y += 26.0;
        let ratio_all = {
            let (mut r, mut q) = (0f32, 0f32);
            for (rr, pp) in b.rel.iter().flatten().zip(b.persist.iter().flatten()) {
                r += rr;
                q += pp;
            }
            if r > 0.0 {
                q / r
            } else {
                0.0
            }
        };
        let head = format!(
            "{}² grid · epoch {} · global RelL2 {:.4} · {:.1}× persistence · suite {:.1}s",
            b.grid, b.epoch, b.global_rel, ratio_all, b.runtime_s
        );
        p.text(
            egui::pos2(x0, y),
            Align2::LEFT_TOP,
            head,
            FontId::monospace(12.0),
            TEXT_DIM,
        );
        y += 36.0;

        // Table at left, metadata-backed provenance at right.
        let gap = 20.0;
        let provenance_w = (content_w * 0.31).clamp(250.0, 320.0);
        let table_w = (content_w - provenance_w - gap).max(390.0);
        let row_label_w = 104.0;
        let cell_w = ((table_w - row_label_w) / b.horizons.len() as f32).max(58.0);
        let cell_h = 46.0;
        for (hi, h) in b.horizons.iter().enumerate() {
            p.text(
                egui::pos2(
                    x0 + row_label_w + hi as f32 * cell_w + (cell_w - 6.0) / 2.0,
                    y,
                ),
                Align2::CENTER_TOP,
                format!("{h}× · {:.2}s", *h as f32 * b.dt_frame),
                FontId::monospace(10.5),
                TEXT_MUTE,
            );
        }
        y += 20.0;
        let rows_top = y;
        let mut clicked = None;
        for (si, seed) in b.seeds.iter().enumerate() {
            let row_y = y + si as f32 * (cell_h + 6.0);
            let stream = b
                .provenance
                .benchmark_seeds
                .iter()
                .find(|record| record.seed == *seed)
                .map(|record| record.stream.as_str())
                .unwrap_or("unknown");
            p.text(
                egui::pos2(x0, row_y + 8.0),
                Align2::LEFT_TOP,
                format!("seed {seed}"),
                FontId::monospace(11.5),
                TEXT_DIM,
            );
            p.text(
                egui::pos2(x0, row_y + 27.0),
                Align2::LEFT_TOP,
                stream_label(stream),
                FontId::monospace(8.5),
                stream_color(stream),
            );
            for hi in 0..b.horizons.len() {
                let (rel, per) = (b.rel[si][hi], b.persist[si][hi]);
                let ratio = per / rel.max(1e-9);
                let col = if ratio >= 5.0 {
                    SUCCESS
                } else if ratio >= 2.0 {
                    GOLD
                } else if ratio >= 1.0 {
                    EMBER
                } else {
                    DATA_RED
                };
                let r = Rect::from_min_size(
                    egui::pos2(x0 + row_label_w + hi as f32 * cell_w, row_y),
                    Vec2::new(cell_w - 6.0, cell_h),
                );
                let response = ui.interact(
                    r,
                    ui.make_persistent_id(("benchmark.cell", *seed, b.horizons[hi])),
                    Sense::click(),
                ).on_hover_text(format!(
                    "seed {seed} · horizon {}×\nRelL2 {rel:.6} · persistence {per:.6}\nClick to inspect real field and spectral evidence",
                    b.horizons[hi]
                ));
                if response.clicked() {
                    clicked = Some((si, hi));
                }
                p.rect_filled(r, CornerRadius::same(3), col.gamma_multiply(0.16));
                let selected = self.bench_selected == Some((si, hi));
                let stroke = if selected {
                    Stroke::new(2.0, BRAND)
                } else if response.hovered() {
                    Stroke::new(1.5, TEXT_DIM)
                } else {
                    Stroke::new(1.0, col.gamma_multiply(0.8))
                };
                p.rect_stroke(r, CornerRadius::same(3), stroke, egui::StrokeKind::Inside);
                if selected {
                    p.text(
                        r.right_top() + Vec2::new(-5.0, 4.0),
                        Align2::RIGHT_TOP,
                        "SEL",
                        FontId::monospace(7.5),
                        BRAND,
                    );
                }
                p.text(
                    egui::pos2(r.center().x, r.min.y + 9.0),
                    Align2::CENTER_TOP,
                    format!("{rel:.4}"),
                    FontId::monospace(12.5),
                    TEXT,
                );
                p.text(
                    egui::pos2(r.center().x, r.max.y - 9.0),
                    Align2::CENTER_BOTTOM,
                    format!("{ratio:.1}× persist"),
                    FontId::monospace(10.0),
                    col,
                );
            }
        }
        let rows_bottom = rows_top + b.seeds.len() as f32 * (cell_h + 6.0);
        let legend_y = rows_bottom + 12.0;
        p.text(
            egui::pos2(x0, legend_y),
            Align2::LEFT_TOP,
            "cell = fresh-test RelL2 when row says FRESH TEST · fill = margin over persistence",
            FontId::proportional(10.5),
            TEXT_MUTE,
        );
        p.text(
            egui::pos2(x0, legend_y + 16.0),
            Align2::LEFT_TOP,
            "click a cell · arrow keys move selection · Enter reloads evidence",
            FontId::proportional(10.5),
            TEXT_MUTE,
        );

        let provenance_rect = Rect::from_min_size(
            egui::pos2(x0 + table_w + gap, y - 20.0),
            Vec2::new(provenance_w, (rows_bottom - y + 52.0).max(286.0)),
        );
        paint_bench_provenance(&p, provenance_rect, &b.provenance);

        let inspector_y = (legend_y + 48.0).max(provenance_rect.max.y + 22.0);
        p.text(
            egui::pos2(x0, inspector_y),
            Align2::LEFT_TOP,
            format!("CELL INSPECTOR · {}", self.bench_var.label().to_uppercase()),
            FontId::monospace(10.0),
            TEXT_MUTE,
        );
        if let Some((si, hi)) = self.bench_selected {
            if let (Some(seed), Some(horizon)) = (b.seeds.get(si), b.horizons.get(hi)) {
                p.text(
                    egui::pos2(x0 + 230.0, inspector_y),
                    Align2::LEFT_TOP,
                    format!("seed {seed} × {horizon} steps"),
                    FontId::monospace(10.0),
                    TEXT_DIM,
                );
            }
        }
        let evidence_top = inspector_y + 22.0;
        if self.bench_inspector_pending {
            paint_evidence_state(
                &p,
                Rect::from_min_size(egui::pos2(x0, evidence_top), Vec2::new(content_w, 76.0)),
                "LOADING CELL EVIDENCE",
                "model/reference fields, derived error, divergence, and spectra are computed on demand",
                EMBER,
            );
        } else if let Some(error) = &self.bench_inspector_error {
            paint_evidence_state(
                &p,
                Rect::from_min_size(egui::pos2(x0, evidence_top), Vec2::new(content_w, 76.0)),
                "INSPECTOR UNAVAILABLE",
                error,
                DATA_RED,
            );
        } else if let Some(cell) = self.bench_inspector.as_ref() {
            let (comparison_scale, error_scale) = cell
                .maps
                .scales(self.bench_var)
                .expect("validated inspector map scales");
            let (mean_error, p95_error, max_error) = cell
                .maps
                .error_stats(self.bench_var)
                .expect("validated inspector error statistics");
            let metrics =
                Rect::from_min_size(egui::pos2(x0, evidence_top), Vec2::new(content_w, 58.0));
            p.rect_filled(metrics, CornerRadius::same(3), SURFACE);
            p.rect_stroke(
                metrics,
                CornerRadius::same(3),
                Stroke::new(1.0, OUTLINE_VARIANT),
                egui::StrokeKind::Inside,
            );
            let metric_w = metrics.width() / 4.0;
            let metric_values = [
                ("MODEL ERROR · RelL2", format!("{:.5}", cell.rel_l2), TEXT),
                (
                    "PERSISTENCE ERROR",
                    format!("{:.5}", cell.persist_rel_l2),
                    TEXT_DIM,
                ),
                (
                    "IMPROVEMENT",
                    format!("{:.2}×", cell.improvement_ratio),
                    if cell.improvement_ratio >= 2.0 {
                        SUCCESS
                    } else if cell.improvement_ratio >= 1.0 {
                        GOLD
                    } else {
                        DATA_RED
                    },
                ),
                (
                    rng_stream_proposition(&cell.provenance_status),
                    stream_label(&cell.seed_stream).to_owned(),
                    provenance_color(&cell.provenance_status),
                ),
            ];
            for (index, (label, value, color)) in metric_values.iter().enumerate() {
                let mx = metrics.min.x + index as f32 * metric_w + 12.0;
                p.text(
                    egui::pos2(mx, metrics.min.y + 9.0),
                    Align2::LEFT_TOP,
                    *label,
                    FontId::monospace(8.5),
                    TEXT_MUTE,
                );
                p.text(
                    egui::pos2(mx, metrics.min.y + 29.0),
                    Align2::LEFT_TOP,
                    value,
                    FontId::monospace(12.0),
                    *color,
                );
            }

            let plots_top = metrics.max.y + 26.0;
            let map_gap = 10.0;
            let map_side = ((table_w - 2.0 * map_gap) / 3.0)
                .min((rect.max.y - plots_top - 38.0).max(72.0))
                .min(148.0);
            let comparison_range = if self.bench_var.signed() {
                format!("shared −{comparison_scale:.3} → +{comparison_scale:.3}")
            } else {
                format!("shared 0 → {comparison_scale:.3}")
            };
            let error_range = if self.bench_var.signed() {
                format!("−{error_scale:.4} → +{error_scale:.4}")
            } else {
                format!("0 → {error_scale:.4}")
            };
            let labels = [
                (
                    format!(
                        "{} · {}",
                        self.bench_var.model_source_label(),
                        self.bench_var.symbol()
                    ),
                    format!("{comparison_range} · {}", self.bench_var.unit_label()),
                ),
                (
                    format!(
                        "{} · {}",
                        self.bench_var.reference_source_label(),
                        self.bench_var.symbol()
                    ),
                    format!("{comparison_range} · {}", self.bench_var.unit_label()),
                ),
                (
                    format!("DERIVED ERROR · {}", self.bench_var.error_symbol()),
                    format!("{error_range} · {}", self.bench_var.unit_label()),
                ),
            ];
            for index in 0..3 {
                let image_rect = Rect::from_min_size(
                    egui::pos2(x0 + index as f32 * (map_side + map_gap), plots_top),
                    Vec2::splat(map_side),
                );
                p.text(
                    image_rect.left_top() - Vec2::new(0.0, 16.0),
                    Align2::LEFT_TOP,
                    &labels[index].0,
                    FontId::monospace(8.5),
                    TEXT_MUTE,
                );
                p.rect_filled(image_rect, CornerRadius::same(2), SURFACE_LOWEST);
                if let Some(texture) = self.bench_tex.get(index) {
                    p.image(
                        texture.id(),
                        image_rect,
                        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
                p.rect_stroke(
                    image_rect,
                    CornerRadius::same(2),
                    Stroke::new(1.0, OUTLINE_VARIANT),
                    egui::StrokeKind::Inside,
                );
                p.text(
                    image_rect.left_bottom() + Vec2::new(0.0, 5.0),
                    Align2::LEFT_TOP,
                    &labels[index].1,
                    FontId::monospace(8.5),
                    TEXT_MUTE,
                );
            }
            let stats_y = plots_top + map_side + 21.0;
            p.text(
                egui::pos2(x0, stats_y),
                Align2::LEFT_TOP,
                format!(
                    "{} |error| mean {:.4} · p95 {:.4} · max {:.4}",
                    self.bench_var.label(),
                    mean_error,
                    p95_error,
                    max_error
                ),
                FontId::monospace(9.0),
                TEXT_DIM,
            );

            let spectrum_rect = Rect::from_min_size(
                egui::pos2(x0 + table_w + gap, plots_top),
                Vec2::new(provenance_w, map_side),
            );
            p.text(
                spectrum_rect.left_top() - Vec2::new(0.0, 16.0),
                Align2::LEFT_TOP,
                "KINETIC-ENERGY SPECTRUM · E(k)",
                FontId::monospace(8.5),
                TEXT_MUTE,
            );
            paint_energy_spectrum(&p, spectrum_rect, cell);
            p.text(
                spectrum_rect.left_bottom() + Vec2::new(0.0, 5.0),
                Align2::LEFT_TOP,
                format!(
                    "spectrum RelL2 {:.4} · model div RMS {:.2e} · solver ref {:.2e} · Δdiv {:.2e}",
                    cell.spectrum_rel_l2,
                    cell.divergence_model_rms,
                    cell.divergence_truth_rms,
                    cell.divergence_error_rms
                ),
                FontId::monospace(8.0),
                TEXT_MUTE,
            );
        } else {
            paint_evidence_state(
                &p,
                Rect::from_min_size(egui::pos2(x0, evidence_top), Vec2::new(content_w, 76.0)),
                "SELECT A CELL",
                "Click a seed × horizon cell to request measured evidence from the engine.",
                TEXT_MUTE,
            );
        }

        if let Some((seed_index, horizon_index)) = clicked {
            self.select_bench_cell(seed_index, horizon_index);
        }
    }

    fn controls_bench(&mut self, ui: &mut egui::Ui) {
        let read_only = self.project.availability().is_read_only_evidence();
        let original_inputs = (
            self.f2d_model.clone(),
            self.bench_seed_start,
            self.bench_seeds,
        );
        ui.label(title_text("Benchmark Lab"));
        ui.add_space(4.0);
        ui.label(
            RichText::new("the Reyn Verify seed — honest suite runs")
                .size(11.5)
                .color(TEXT_MUTE),
        );
        ui.add_space(16.0);
        if read_only {
            ui.label(
                RichText::new(
                    "READ-ONLY EVIDENCE · Benchmark inputs, new suites, and new cell evidence are locked. Stored suite evidence remains inspectable.",
                )
                .text_style(caption())
                .color(WARN),
            );
            ui.add_space(8.0);
        }

        let stem = |m: &str| m.trim_end_matches(".reynmodel").to_string();
        let models: Vec<String> = self
            .models
            .iter()
            .filter(|model| model.dimension == 2 && model.status != "invalid")
            .map(|model| model.id.clone())
            .collect();
        let mut pick = self.f2d_model.clone();
        ui.add_enabled_ui(!read_only, |ui| {
            egui::ComboBox::from_id_salt("bench.model")
                .selected_text(RichText::new(stem(&pick)).color(TEXT).size(12.5))
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for m in &models {
                        ui.selectable_value(&mut pick, m.clone(), stem(m));
                    }
                });
        });
        if pick != self.f2d_model {
            self.f2d_model = pick;
            let selected_model = self.f2d_model.clone();
            if let Err(error) = self.persist_model_default(&selected_model, 2) {
                self.settings_notice =
                    Some((format!("The 2D model default was not saved: {error}"), true));
            }
            self.bench = None;
            self.bench_selected = None;
            self.bench_inspector = None;
            self.bench_inspector_pending = false;
            self.bench_inspector_error = None;
            self.bench_tex.clear();
        }

        ui.add_space(12.0);
        card(ui, |ui| {
            if read_only {
                ui.disable();
            }
            ui.horizontal(|ui| {
                ui.label(RichText::new("Exact seed start").color(TEXT_DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.bench_seed_start)
                            .range(0..=999_999)
                            .speed(1),
                    );
                });
            });
            ui.add_space(5.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Seed count").color(TEXT_DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(mono(&format!("{}", self.bench_seeds), TEXT_DIM).size(12.0));
                });
            });
            ui.spacing_mut().slider_width = ui.available_width() - 8.0;
            ui.add(
                egui::Slider::new(&mut self.bench_seeds, 2..=6)
                    .show_value(false)
                    .trailing_fill(true),
            );
            let last_seed = self.bench_seed_start.saturating_add(self.bench_seeds - 1);
            ui.label(
                RichText::new(format!(
                    "{}…{} exactly · no hidden offset",
                    self.bench_seed_start, last_seed
                ))
                .text_style(caption())
                .color(TEXT_MUTE),
            );
            ui.label(
                RichText::new("horizons 1× · 4× · 8× · 16×")
                    .text_style(caption())
                    .color(TEXT_MUTE),
            );
        });

        let inputs_changed = original_inputs
            != (
                self.f2d_model.clone(),
                self.bench_seed_start,
                self.bench_seeds,
            );
        if inputs_changed {
            self.bench = None;
            self.bench_selected = None;
            self.bench_inspector = None;
            self.bench_inspector_pending = false;
            self.bench_error = None;
            self.bench_inspector_error = None;
            self.bench_tex.clear();
            self.active_benchmark_run_id = None;
            match self.prepare_benchmark_case(true) {
                Ok(_) => {
                    self.project_notice = Some((
                        "Benchmark inputs changed; only dependent run and evidence stages are stale. Completed runs remain inspectable."
                            .into(),
                        false,
                    ));
                }
                Err(error) => {
                    self.project_notice =
                        Some((format!("Benchmark case was not revised: {error}"), true));
                }
            }
        }

        ui.add_space(12.0);
        let selected_model_available = self.models.iter().any(|model| {
            model.id == self.f2d_model
                && model.status != "invalid"
                && is_sha256(&model.checkpoint_sha256)
        });
        // Disabled-with-reason (X3): the button never renders as a live
        // control while blocked, and states why on hover.
        let (run_label, run_gate) = if read_only {
            (
                "Read-only evidence",
                Some("New benchmark runs are blocked in read-only evidence mode."),
            )
        } else if self.bench_running {
            ("Running…", Some("The benchmark suite is already running."))
        } else if self.bench_inspector_pending {
            ("Inspecting cell…", Some("A suite cell is being inspected."))
        } else if !self.engine_ok {
            (
                "Engine unavailable",
                Some("Restore the Python engine, then run a suite."),
            )
        } else if !selected_model_available {
            (
                "Model unavailable",
                Some(
                    "The selected model bundle is invalid or missing; pick another in Model Library.",
                ),
            )
        } else {
            ("Run full suite", None)
        };
        if action_button_gated(
            ui,
            Some(ph::PLAY),
            run_label,
            EMBER,
            ON_EMBER,
            None,
            40.0,
            ui.available_width(),
            run_gate,
        ) {
            if let Err(error) = self.prepare_benchmark_case(true) {
                self.bench_error = Some(format!(
                    "Run blocked because its project lineage could not be recorded: {error}"
                ));
                return;
            }
            self.bench = None;
            self.bench_selected = None;
            self.bench_inspector = None;
            self.bench_inspector_pending = false;
            self.bench_error = None;
            self.bench_inspector_error = None;
            self.bench_tex.clear();
            self.bench_running = true;
            let seeds = (0..self.bench_seeds)
                .map(|offset| self.bench_seed_start.saturating_add(offset))
                .collect();
            if self
                .engine
                .tx
                .send(engine::Cmd::RunBenchmark {
                    model: self.f2d_model.clone(),
                    seeds,
                    horizons: vec![1, 4, 8, 16],
                })
                .is_err()
            {
                self.bench_running = false;
                self.bench_error = Some("engine worker is no longer available".into());
            }
        }

        if self.bench_inspector.is_some() {
            ui.add_space(12.0);
            ui.label(caps("Cell Inspector Variable"));
            ui.add_space(8.0);
            let mut selected = self.bench_var;
            let inspector_maps = &self
                .bench_inspector
                .as_ref()
                .expect("inspector is present")
                .maps;
            card(ui, |ui| {
                egui::ComboBox::from_id_salt("bench.inspector.variable")
                    .selected_text(format!("{} · {}", selected.symbol(), selected.label()))
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for variable in InspectorVariable::ALL {
                            ui.selectable_value(
                                &mut selected,
                                variable,
                                format!("{} · {}", variable.symbol(), variable.label()),
                            );
                        }
                    });
                ui.label(
                    RichText::new(selected.method_note())
                        .text_style(caption())
                        .color(TEXT_MUTE),
                );
                ui.label(
                    RichText::new("model and solver reference share one calibrated scale")
                        .text_style(caption())
                        .color(TEXT_MUTE),
                );
                if let Some((model_rms, reference_rms, error_rms)) = inspector_maps.rms(selected) {
                    ui.label(
                        RichText::new(format!(
                            "RMS [{}] · model {model_rms:.2e} · reference {reference_rms:.2e} · error {error_rms:.2e}",
                            selected.unit_label()
                        ))
                        .text_style(mono_s())
                        .color(TEXT_DIM),
                    );
                }
            });
            if selected != self.bench_var {
                self.bench_var = selected;
                self.bench_tex.clear();
                self.persist_benchmark_view_selection();
            }
        }

        if self.bench.is_some() {
            ui.add_space(12.0);
            ui.label(caps("Evidence Export"));
            ui.add_space(8.0);
            card(ui, |ui| {
                if action_button(
                    ui,
                    Some(ph::DOWNLOAD_SIMPLE),
                    "Export CSV",
                    SURFACE_HIGH,
                    TEXT,
                    Some(OUTLINE),
                    30.0,
                    ui.available_width(),
                ) {
                    self.export_bench_csv();
                }
                ui.add_space(4.0);
                if action_button(
                    ui,
                    Some(ph::DOWNLOAD_SIMPLE),
                    "Report Card (JSON)",
                    SURFACE_HIGH,
                    TEXT,
                    Some(OUTLINE),
                    30.0,
                    ui.available_width(),
                ) {
                    self.export_report_card();
                }
                ui.add_space(4.0);
                if action_button(
                    ui,
                    Some(ph::DOWNLOAD_SIMPLE),
                    "Report Card (PNG + JSON)",
                    SURFACE_HIGH,
                    TEXT,
                    Some(OUTLINE),
                    30.0,
                    ui.available_width(),
                ) {
                    self.export_visual_report(crate::benchmark_export::ExportFormat::Png);
                }
                ui.add_space(4.0);
                if action_button(
                    ui,
                    Some(ph::DOWNLOAD_SIMPLE),
                    "Report Card (PDF + JSON)",
                    SURFACE_HIGH,
                    TEXT,
                    Some(OUTLINE),
                    30.0,
                    ui.available_width(),
                ) {
                    self.export_visual_report(crate::benchmark_export::ExportFormat::Pdf);
                }
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);
                let configured_key = self.settings.configured_signing_key();
                let signing_ready = configured_key.is_ok_and(|key| key.is_some());
                if action_button(
                    ui,
                    Some(ph::DOWNLOAD_SIMPLE),
                    if signing_ready {
                        "Sign JSON + PNG + PDF…"
                    } else {
                        "Signing key unavailable"
                    },
                    if signing_ready { EMBER } else { SURFACE_HIGH },
                    if signing_ready { ON_EMBER } else { TEXT_MUTE },
                    None,
                    32.0,
                    ui.available_width(),
                ) && signing_ready
                {
                    self.export_signed_report_bundle();
                }
                ui.add_space(4.0);
                if action_button(
                    ui,
                    None,
                    "Verify detached signature…",
                    SURFACE_HIGH,
                    TEXT,
                    Some(OUTLINE),
                    30.0,
                    ui.available_width(),
                ) {
                    self.verify_detached_signature();
                }
                ui.label(
                    RichText::new(if signing_ready {
                        "READY · Ed25519 signs the canonical payload hash · private key remains in macOS Keychain"
                    } else if self.settings.signing_key_is_revoked() {
                        "REVOKED · this fingerprint cannot produce a signed claim"
                    } else {
                        "UNSIGNED · SHA-256 integrity only · configure a valid key in Settings"
                    })
                    .text_style(caption())
                    .color(TEXT_MUTE),
                );
                if let Some((message, is_error)) = &self.signing_notice {
                    ui.label(
                        RichText::new(message)
                            .text_style(caption())
                            .color(if *is_error { DATA_RED } else { SUCCESS }),
                    );
                }
            });
        }
        ui.add_space(12.0);
        ui.label(
            RichText::new(
                "N5.3 · velocity / vorticity / recovered-pressure / spatial-divergence evidence",
            )
            .text_style(caption())
            .color(TEXT_MUTE),
        );
    }

    fn export_bench_csv(&mut self) {
        let Some(b) = self.bench.as_ref() else { return };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV", &["csv"])
            .set_file_name("reyn_benchmark.csv")
            .save_file()
        else {
            return;
        };
        let csv = benchmark_csv(b);
        let _ = std::fs::write(&path, csv);
        self.engine_status = format!(
            "● Exported {}",
            path.file_name().and_then(|s| s.to_str()).unwrap_or("csv")
        );
    }

    /// Machine-readable evidence with a SHA-256 hash over the canonical payload.
    /// This export does not produce an organization-key authenticity signature.
    fn export_report_card(&mut self) {
        let (run_id, model_sha256, timestamp_unix) = match self.benchmark_export_identity() {
            Ok(identity) => identity,
            Err(error) => {
                self.engine_status = format!("● Report export unavailable · {error}");
                return;
            }
        };
        let Some(b) = self.bench.as_ref() else { return };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .set_file_name("reyn_report_card.json")
            .save_file()
        else {
            return;
        };
        let (json, hex) = benchmark_report_card(
            b,
            self.bench_inspector.as_ref(),
            &run_id,
            &model_sha256,
            timestamp_unix,
        );
        if let Err(error) = crate::benchmark_export::verify_canonical_report(&json) {
            self.engine_status = format!("● Report export failed verification · {error}");
            return;
        }
        if let Err(error) = std::fs::write(&path, json) {
            self.engine_status = format!("● Report export failed · {error}");
            return;
        }
        self.engine_status = format!(
            "● Report card {} · canonical sha256 {}… · UNSIGNED",
            path.file_name().and_then(|s| s.to_str()).unwrap_or("json"),
            &hex[..12]
        );
    }

    fn benchmark_export_identity(&self) -> Result<(String, String, u64), String> {
        let benchmark = self
            .bench
            .as_ref()
            .ok_or_else(|| "no benchmark suite is available".to_string())?;
        let run_id = self
            .active_benchmark_run_id
            .clone()
            .ok_or_else(|| "the suite is not linked to an immutable run".to_string())?;
        let model_sha256 = self
            .models
            .iter()
            .find(|model| model.id == benchmark.model)
            .map(|model| model.checkpoint_sha256.to_ascii_lowercase())
            .filter(|digest| is_sha256(digest))
            .ok_or_else(|| "the selected model has no verified bundle SHA-256".to_string())?;
        let timestamp_unix = self
            .project
            .manifest()
            .run(&run_id)
            .map(project::RunRecord::completed_utc_unix)
            .ok_or_else(|| "the immutable benchmark run is no longer available".to_string())?;
        Ok((run_id, model_sha256, timestamp_unix))
    }

    fn export_visual_report(&mut self, format: crate::benchmark_export::ExportFormat) {
        let (run_id, model_sha256, timestamp_unix) = match self.benchmark_export_identity() {
            Ok(identity) => identity,
            Err(error) => {
                self.engine_status = format!("● Report export unavailable · {error}");
                return;
            }
        };
        let Some(benchmark) = self.bench.as_ref() else {
            return;
        };
        let label = match format {
            crate::benchmark_export::ExportFormat::Png => "PNG",
            crate::benchmark_export::ExportFormat::Pdf => "PDF",
        };
        let extension = format.extension();
        let Some(path) = rfd::FileDialog::new()
            .add_filter(label, &[extension])
            .set_file_name(format!("reyn_report_card.{extension}"))
            .save_file()
        else {
            return;
        };
        let (json, canonical_hash) = benchmark_report_card(
            benchmark,
            self.bench_inspector.as_ref(),
            &run_id,
            &model_sha256,
            timestamp_unix,
        );
        let artifact = match crate::benchmark_export::export_report(&json, format) {
            Ok(artifact) => artifact,
            Err(error) => {
                self.engine_status = format!("● {label} report export failed · {error}");
                return;
            }
        };
        let json_path = path.with_extension("json");
        if let Err(error) = std::fs::write(&json_path, json) {
            self.engine_status =
                format!("● {label} report export failed · canonical JSON: {error}");
            return;
        }
        if let Err(error) = std::fs::write(&path, &artifact.bytes) {
            self.engine_status = format!(
                "● {label} rendering failed · canonical JSON saved as {} · {error}",
                json_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("report.json")
            );
            return;
        }
        self.engine_status = format!(
            "● {label} + JSON exported · {} · canonical {}… · artifact {}… · UNSIGNED",
            artifact.media_type(),
            &canonical_hash[..12],
            &artifact.content_sha256[..12],
        );
    }

    fn export_signed_report_bundle(&mut self) {
        let (run_id, model_sha256, timestamp_unix) = match self.benchmark_export_identity() {
            Ok(identity) => identity,
            Err(error) => {
                self.signing_notice = Some((format!("UNSIGNED · {error}"), true));
                return;
            }
        };
        let configured_key = match self.settings.configured_signing_key() {
            Ok(Some(key)) => key,
            Ok(None) => {
                self.signing_notice =
                    Some(("UNSIGNED · no signing key is configured".into(), true));
                return;
            }
            Err(error) => {
                self.signing_notice = Some((format!("UNSIGNED · {error}"), true));
                return;
            }
        };
        let Some(benchmark) = self.bench.as_ref() else {
            return;
        };
        let Some(selected_path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .set_file_name("reyn_report_card.json")
            .save_file()
        else {
            return;
        };
        let report_path = selected_path.with_extension("json");
        let signature_path = selected_path.with_extension("sig.json");
        let png_path = selected_path.with_extension("png");
        let pdf_path = selected_path.with_extension("pdf");
        let (json, canonical_payload_sha256) = benchmark_report_card(
            benchmark,
            self.bench_inspector.as_ref(),
            &run_id,
            &model_sha256,
            timestamp_unix,
        );
        if let Err(error) = crate::benchmark_export::verify_canonical_report(&json) {
            self.signing_notice =
                Some((format!("UNSIGNED · canonical report failed: {error}"), true));
            return;
        }
        let lineage = signing::SigningLineage {
            run_id,
            report_schema: crate::benchmark_export::REPORT_CARD_SCHEMA.into(),
            canonical_report_sha256: signing::sha256_hex(json.as_bytes()),
            canonical_payload_sha256,
            created_utc_unix: timestamp_unix,
        };
        let signature = match signing::sign_canonical_payload(
            &signing::NativeKeychainProvider,
            &configured_key,
            self.settings.signing_key_is_revoked(),
            &lineage,
        ) {
            Ok(signature) => signature,
            Err(error) => {
                self.signing_notice = Some((format!("UNSIGNED · signing failed: {error}"), true));
                return;
            }
        };
        let signature_json = match signature.to_json() {
            Ok(json) => json,
            Err(error) => {
                self.signing_notice = Some((format!("UNSIGNED · sidecar failed: {error}"), true));
                return;
            }
        };
        let png = match crate::benchmark_export::export_report_with_signature(
            &json,
            crate::benchmark_export::ExportFormat::Png,
            &signature,
        ) {
            Ok(artifact) => artifact,
            Err(error) => {
                self.signing_notice =
                    Some((format!("UNSIGNED · signed PNG failed: {error}"), true));
                return;
            }
        };
        let pdf = match crate::benchmark_export::export_report_with_signature(
            &json,
            crate::benchmark_export::ExportFormat::Pdf,
            &signature,
        ) {
            Ok(artifact) => artifact,
            Err(error) => {
                self.signing_notice =
                    Some((format!("UNSIGNED · signed PDF failed: {error}"), true));
                return;
            }
        };
        for (path, bytes) in [
            (&report_path, json.as_bytes()),
            (&signature_path, signature_json.as_bytes()),
            (&png_path, png.bytes.as_slice()),
            (&pdf_path, pdf.bytes.as_slice()),
        ] {
            if let Err(error) = std::fs::write(path, bytes) {
                self.signing_notice = Some((
                    format!("Signed bundle write failed at {}: {error}", path.display()),
                    true,
                ));
                return;
            }
        }
        let project_status = match self.persist_signed_report(&json, &signature_json, &signature) {
            Ok(true) => "signature and report appended as derived project evidence",
            Ok(false) => "matching signature already exists; immutable evidence was not duplicated",
            Err(error) => {
                self.project_notice = Some((
                    format!(
                        "Signed files were exported, but project evidence was not appended: {error}"
                    ),
                    true,
                ));
                "external bundle verified; project append failed"
            }
        };
        let sidecar_short = signature
            .content_sha256()
            .map(|digest| short_hash(&digest))
            .unwrap_or_else(|_| "UNKNOWN".into());
        self.signing_notice = Some((
            format!(
                "VERIFIED CONFIGURED KEY · Ed25519 · {} · sidecar {} · {project_status}",
                short_hash(&configured_key.key_fingerprint_sha256),
                sidecar_short,
            ),
            false,
        ));
    }

    fn verify_detached_signature(&mut self) {
        let Some(report_path) = rfd::FileDialog::new()
            .add_filter("Canonical report JSON", &["json"])
            .pick_file()
        else {
            return;
        };
        let signature_path = report_path.with_extension("sig.json");
        let report = match std::fs::read_to_string(&report_path) {
            Ok(report) => report,
            Err(error) => {
                self.signing_notice = Some((format!("VERIFY FAILED · report read: {error}"), true));
                return;
            }
        };
        let signature_json = match std::fs::read_to_string(&signature_path) {
            Ok(signature) => signature,
            Err(error) => {
                self.signing_notice = Some((
                    format!(
                        "VERIFY FAILED · expected detached sidecar {}: {error}",
                        signature_path.display()
                    ),
                    true,
                ));
                return;
            }
        };
        let signature = match signing::SignedEvidenceArtifact::from_json(&signature_json) {
            Ok(signature) => signature,
            Err(error) => {
                self.signing_notice =
                    Some((format!("VERIFY FAILED · malformed sidecar: {error}"), true));
                return;
            }
        };
        let trusted: Vec<String> = self
            .settings
            .configured_signing_key()
            .ok()
            .flatten()
            .map(|key| vec![key.key_fingerprint_sha256])
            .unwrap_or_default();
        let policy = signing::VerificationPolicy::new(
            trusted,
            self.settings.revoked_signing_key_fingerprints.clone(),
        );
        let outcome =
            crate::benchmark_export::verify_report_signature(&report, &signature, &policy);
        self.signing_notice = Some((
            format!(
                "{:?} · {} · key {} · fingerprint {}",
                outcome.status,
                outcome.detail,
                outcome.key_id.as_deref().unwrap_or("UNKNOWN"),
                outcome
                    .key_fingerprint_sha256
                    .as_deref()
                    .map(short_hash)
                    .unwrap_or_else(|| "UNKNOWN".into()),
            ),
            !outcome.status.is_cryptographically_valid(),
        ));
    }

    /// Detach an Ed25519 authenticity sidecar over the selected run's
    /// persisted engineering-result digest and append it to the project ledger.
    fn sign_engineering_result_evidence(&mut self) {
        let configured_key = match self.settings.configured_signing_key() {
            Ok(Some(key)) if !self.settings.signing_key_is_revoked() => key,
            Ok(Some(_)) => {
                self.project_notice = Some((
                    "UNSIGNED · the configured signing key is revoked.".into(),
                    true,
                ));
                return;
            }
            Ok(None) => {
                self.project_notice = Some((
                    "UNSIGNED · configure an Ed25519 Keychain key in Settings first.".into(),
                    true,
                ));
                return;
            }
            Err(error) => {
                self.project_notice =
                    Some((format!("UNSIGNED · signing key unavailable: {error}"), true));
                return;
            }
        };
        let run_id = match self
            .cad
            .as_ref()
            .and_then(|case| case.active_run_id.clone())
            .or_else(|| self.project.manifest().selection().selected_run_id.clone())
        {
            Some(run_id) => run_id,
            None => {
                self.project_notice = Some((
                    "UNSIGNED · select a completed engineering run first.".into(),
                    true,
                ));
                return;
            }
        };
        let parent = match self
            .project
            .manifest()
            .evidence()
            .iter()
            .rev()
            .find(|artifact| {
                artifact.media_type == "application/vnd.reyn.engineering-result+json"
                    && artifact.run_ids.iter().any(|id| id == &run_id)
                    && artifact.source_class == project::EvidenceSourceClass::Derived
            })
            .cloned()
        {
            Some(parent) => parent,
            None => {
                self.project_notice = Some((
                    "UNSIGNED · this run has no persisted engineering-result evidence.".into(),
                    true,
                ));
                return;
            }
        };
        let created = now_utc_unix();
        let signature = match engineering_export::sign_engineering_result_digest(
            &signing::NativeKeychainProvider,
            &configured_key,
            self.settings.signing_key_is_revoked(),
            &run_id,
            &parent.content_sha256,
            created,
        ) {
            Ok(signature) => signature,
            Err(error) => {
                self.project_notice = Some((format!("UNSIGNED · signing failed: {error}"), true));
                return;
            }
        };
        let signature_json = match signature.to_json() {
            Ok(json) => json,
            Err(error) => {
                self.project_notice = Some((
                    format!("UNSIGNED · sidecar serialization failed: {error}"),
                    true,
                ));
                return;
            }
        };
        match self.persist_engineering_result_signature(&parent, &signature_json, &signature) {
            Ok(true) => {
                let fingerprint = short_hash(&configured_key.key_fingerprint_sha256);
                let digest = short_hash(&parent.content_sha256);
                self.project_notice = Some((
                    format!(
                        "VERIFIED CONFIGURED KEY · engineering result signed · key {fingerprint} · digest {digest}"
                    ),
                    false,
                ));
                if let Some(path) = self
                    .export_dialog(&format!(
                        "engineering_result_{}.sig.json",
                        short_id(&run_id)
                    ))
                    .add_filter("Evidence signature", &["json"])
                    .save_file()
                {
                    if let Err(error) = std::fs::write(&path, &signature_json) {
                        self.project_notice = Some((
                            format!(
                                "Signature was recorded in the project, but the sidecar file was not written: {error}"
                            ),
                            true,
                        ));
                    }
                }
            }
            Ok(false) => {
                self.project_notice = Some((
                    "Matching engineering signature already exists; immutable evidence was not duplicated."
                        .into(),
                    false,
                ));
            }
            Err(error) => {
                self.project_notice = Some((
                    format!("UNSIGNED · signature was not recorded: {error}"),
                    true,
                ));
            }
        }
    }

    fn persist_engineering_result_signature(
        &mut self,
        parent: &project::EvidenceArtifact,
        signature_json: &str,
        signature: &signing::SignedEvidenceArtifact,
    ) -> Result<bool, String> {
        self.project_write_access("Recording engineering signature")?;
        let signature_sha256 = signing::sha256_hex(signature_json.as_bytes());
        let canonical_payload_sha256 = &signature.source.canonical_payload_sha256;
        let key_fingerprint = &signature.authenticity.key_fingerprint_sha256;
        if self.project.manifest().evidence().iter().any(|artifact| {
            artifact.source_class == project::EvidenceSourceClass::AuthenticitySignature
                && artifact
                    .metadata
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    == Some("engineering_signature")
                && artifact
                    .metadata
                    .get("canonical_payload_sha256")
                    .and_then(serde_json::Value::as_str)
                    == Some(canonical_payload_sha256)
                && artifact
                    .metadata
                    .get("key_fingerprint_sha256")
                    .and_then(serde_json::Value::as_str)
                    == Some(key_fingerprint)
        }) {
            return Ok(false);
        }
        self.add_project_content(
            "Recording engineering signature",
            signature_json.as_bytes().to_vec(),
            "application/vnd.reyn.evidence-signature+json",
            &signature_sha256,
        )
        .map_err(|error| format!("signature sidecar bundle: {error}"))?;
        let run_id = signature.source.run_id.clone();
        let parent_evidence_id = parent.evidence_id.clone();
        let report_sha256 = parent.content_sha256.clone();
        let signature_evidence_id =
            format!("engineering-signature-{key_fingerprint}-{canonical_payload_sha256}");
        let created_utc_unix = signature.created_utc_unix;
        let signature_evidence = project::EvidenceArtifact {
            evidence_id: signature_evidence_id.clone(),
            run_ids: vec![run_id.clone()],
            created_utc_unix,
            source_class: project::EvidenceSourceClass::AuthenticitySignature,
            media_type: "application/vnd.reyn.evidence-signature+json".into(),
            byte_size: signature_json.len() as u64,
            content_sha256: signature_sha256,
            derivation_method: Some("ed25519_canonical_payload_signature".into()),
            derivation_version: Some("1".into()),
            warnings: vec![
                "The engineering-result JSON remains immutable and explicitly UNSIGNED; authenticity is carried by this detached signature evidence artifact."
                    .into(),
                "Signature validity and organization-key trust are separate; compare the fingerprint independently and apply current revocations."
                    .into(),
            ],
            metadata: serde_json::json!({
                "kind": "engineering_signature",
                "signature_schema": signing::SIGNATURE_SCHEMA,
                "parent_evidence_id": parent_evidence_id,
                "canonical_report_sha256": report_sha256,
                "canonical_payload_sha256": canonical_payload_sha256,
                "algorithm": signing::SIGNATURE_ALGORITHM,
                "key_id": signature.authenticity.key_id,
                "key_fingerprint_sha256": key_fingerprint,
                "verification_at_creation": "valid",
            }),
            calibrated_views: Vec::new(),
        };
        let now = now_utc_unix();
        let case_id = self.cad.as_ref().map(|case| case.workflow.case_id.clone());
        self.transact_project("Recording engineering signature", now, move |manifest| {
            manifest.append_evidence(signature_evidence, now)?;
            manifest.set_selection(
                project::ProjectSelection {
                    active_case_id: case_id.or_else(|| {
                        manifest
                            .cases()
                            .iter()
                            .find(|case| case.runs().iter().any(|run| run.run_id() == run_id))
                            .map(|case| case.case_id().to_owned())
                    }),
                    selected_run_id: Some(run_id),
                    selected_evidence_id: Some(signature_evidence_id),
                    selected_view_id: None,
                },
                now,
            )?;
            Ok(())
        })?;
        self.dependencies_dirty = true;
        Ok(true)
    }

    fn persist_signed_report(
        &mut self,
        canonical_report_json: &str,
        signature_json: &str,
        signature: &signing::SignedEvidenceArtifact,
    ) -> Result<bool, String> {
        self.project_write_access("Recording signed evidence")?;
        let report_sha256 = signing::sha256_hex(canonical_report_json.as_bytes());
        let signature_sha256 = signing::sha256_hex(signature_json.as_bytes());
        let canonical_payload_sha256 = &signature.source.canonical_payload_sha256;
        let key_fingerprint = &signature.authenticity.key_fingerprint_sha256;
        if self.project.manifest().evidence().iter().any(|artifact| {
            artifact.source_class == project::EvidenceSourceClass::AuthenticitySignature
                && artifact
                    .metadata
                    .get("canonical_payload_sha256")
                    .and_then(serde_json::Value::as_str)
                    == Some(canonical_payload_sha256)
                && artifact
                    .metadata
                    .get("key_fingerprint_sha256")
                    .and_then(serde_json::Value::as_str)
                    == Some(key_fingerprint)
        }) {
            return Ok(false);
        }
        self.add_project_content(
            "Recording signed evidence",
            canonical_report_json.as_bytes().to_vec(),
            "application/vnd.reyn.benchmark-report+json",
            &report_sha256,
        )
        .map_err(|error| format!("canonical report bundle: {error}"))?;
        self.add_project_content(
            "Recording signed evidence",
            signature_json.as_bytes().to_vec(),
            "application/vnd.reyn.evidence-signature+json",
            &signature_sha256,
        )
        .map_err(|error| format!("signature sidecar bundle: {error}"))?;

        let report_evidence_id = format!("benchmark-report-{report_sha256}");
        let signature_evidence_id =
            format!("benchmark-signature-{key_fingerprint}-{canonical_payload_sha256}");
        let report_exists = self
            .project
            .manifest()
            .evidence_artifact(&report_evidence_id)
            .is_some();
        let run_id = signature.source.run_id.clone();
        let created_utc_unix = signature.created_utc_unix;
        let report = project::EvidenceArtifact {
            evidence_id: report_evidence_id.clone(),
            run_ids: vec![run_id.clone()],
            created_utc_unix,
            source_class: project::EvidenceSourceClass::Derived,
            media_type: "application/vnd.reyn.benchmark-report+json".into(),
            byte_size: canonical_report_json.len() as u64,
            content_sha256: report_sha256.clone(),
            derivation_method: Some("canonical_benchmark_report".into()),
            derivation_version: Some("1".into()),
            warnings: vec![
                "The canonical report remains immutable and explicitly UNSIGNED; authenticity is carried by a separate signature evidence artifact."
                    .into(),
            ],
            metadata: serde_json::json!({
                "kind": "canonical_benchmark_report",
                "canonical_payload_sha256": canonical_payload_sha256,
                "authenticity": "detached_signature",
            }),
            calibrated_views: Vec::new(),
        };
        let signature_evidence = project::EvidenceArtifact {
            evidence_id: signature_evidence_id.clone(),
            run_ids: vec![run_id.clone()],
            created_utc_unix,
            source_class: project::EvidenceSourceClass::AuthenticitySignature,
            media_type: "application/vnd.reyn.evidence-signature+json".into(),
            byte_size: signature_json.len() as u64,
            content_sha256: signature_sha256,
            derivation_method: Some("ed25519_canonical_payload_signature".into()),
            derivation_version: Some("1".into()),
            warnings: vec![
                "Signature validity and organization-key trust are separate; compare the fingerprint independently and apply current revocations."
                    .into(),
            ],
            metadata: serde_json::json!({
                "kind": "benchmark_signature",
                "signature_schema": signing::SIGNATURE_SCHEMA,
                "parent_evidence_id": report_evidence_id,
                "canonical_report_sha256": report_sha256,
                "canonical_payload_sha256": canonical_payload_sha256,
                "algorithm": signing::SIGNATURE_ALGORITHM,
                "key_id": signature.authenticity.key_id,
                "key_fingerprint_sha256": key_fingerprint,
                "verification_at_creation": "valid",
            }),
            calibrated_views: Vec::new(),
        };
        let now = now_utc_unix();
        self.transact_project("Recording signed evidence", now, move |manifest| {
            if !report_exists {
                manifest.append_evidence(report, now)?;
            }
            manifest.append_evidence(signature_evidence, now)?;
            manifest.set_selection(
                project::ProjectSelection {
                    active_case_id: manifest
                        .cases()
                        .iter()
                        .find(|case| case.runs().iter().any(|run| run.run_id() == run_id))
                        .map(|case| case.case_id().to_owned()),
                    selected_run_id: Some(run_id),
                    selected_evidence_id: Some(signature_evidence_id),
                    selected_view_id: None,
                },
                now,
            )?;
            Ok(())
        })?;
        self.dependencies_dirty = true;
        Ok(true)
    }

    fn controls_2d(&mut self, ui: &mut egui::Ui) {
        ui.label(title_text("Pressure Recovery (2D)"));
        ui.add_space(8.0);
        // Model selector — verified obstacle-family 2D bundles.
        let stem = |m: &str| m.trim_end_matches(".reynmodel").to_string();
        let models: Vec<String> = self
            .models
            .iter()
            .filter(|model| model.dimension == 2 && model.status != "invalid")
            .map(|model| model.id.clone())
            .collect();
        let mut pick = self.f2d_model.clone();
        egui::ComboBox::from_id_salt("f2d.model")
            .selected_text(RichText::new(stem(&pick)).color(TEXT).size(12.5))
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for m in &models {
                    ui.selectable_value(&mut pick, m.clone(), stem(m));
                }
            });
        if pick != self.f2d_model {
            self.f2d_model = pick;
            let selected_model = self.f2d_model.clone();
            if let Err(error) = self.persist_model_default(&selected_model, 2) {
                self.settings_notice =
                    Some((format!("The 2D model default was not saved: {error}"), true));
            }
            self.f2d = None;
            self.f2d_tex.clear();
            self.f2d_sig = u64::MAX;
            self.request_2d();
        }
        ui.add_space(18.0);

        ui.label(caps("Field Variable"));
        ui.add_space(8.0);
        // Concentric radii: thumb r-1 = 4 inside container r-2 = 6 (§3.5).
        Frame::NONE
            .fill(SURFACE)
            .corner_radius(CornerRadius::same(R2))
            .stroke(Stroke::new(1.0, HAIRLINE))
            .inner_margin(2)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for v in [FieldVar::Velocity, FieldVar::Vorticity, FieldVar::Pressure] {
                        if seg(ui, v.label(), self.f2d_var == v) {
                            self.f2d_var = v;
                        }
                    }
                });
            });

        ui.add_space(16.0);
        ui.label(caps("TimeJump"));
        ui.add_space(8.0);
        let dt = self.f2d.as_ref().map(|f| f.dt_frame).unwrap_or(0.04);
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Horizon").color(TEXT_DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        mono(
                            &format!(
                                "t = {:.2}s · {} steps",
                                self.f2d_horizon as f32 * dt,
                                self.f2d_horizon
                            ),
                            EMBER,
                        )
                        .size(12.0),
                    );
                });
            });
            ui.add_space(6.0);
            ui.spacing_mut().slider_width = ui.available_width() - 8.0;
            let resp = ui.add(
                egui::Slider::new(&mut self.f2d_horizon, 1..=32)
                    .show_value(false)
                    .trailing_fill(true),
            );
            if resp.changed() {
                self.request_2d();
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let (hud, col) = if self.f2d_pending {
                    ("● predicting…".to_string(), EMBER)
                } else {
                    (format!("◌ {} ms", self.f2d_latency_ms), TEXT_MUTE)
                };
                ui.label(mono(&hud, col).size(11.0));
                if self.f2d_horizon > 16 {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(mono("beyond trained horizon", GOLD).size(11.0));
                    });
                }
            });
        });

        ui.add_space(16.0);
        ui.label(caps("Consistency check"));
        ui.add_space(8.0);
        card(ui, |ui| match self.f2d.as_ref().and_then(|f| f.semigroup) {
            Some(s) => {
                let pct = (1.0 - s).clamp(0.0, 1.0) * 100.0;
                let col = if pct >= 98.0 {
                    SUCCESS
                } else if pct >= 90.0 {
                    GOLD
                } else {
                    EMBER
                };
                diag(ui, "Self-consistency", &format!("{:.1}%", pct), col);
                ui.label(
                    RichText::new("semigroup: predict h  vs  h/2 then h/2")
                        .text_style(caption())
                        .color(TEXT_MUTE),
                );
            }
            None => {
                diag(ui, "Self-consistency", "— (odd h)", TEXT_MUTE);
            }
        });

        ui.add_space(16.0);
        ui.label(caps("Solver reference overlay"));
        ui.add_space(8.0);
        card(ui, |ui| {
            if self.f2d_painted.is_some() {
                ui.label(
                    RichText::new("painted IC — no solver reference exists")
                        .color(GOLD)
                        .size(12.0),
                );
                ui.label(
                    RichText::new(
                        "semigroup self-consistency is available; it is not an accuracy claim",
                    )
                    .text_style(caption())
                    .color(TEXT_MUTE),
                );
                ui.add_space(6.0);
                if action_button(
                    ui,
                    None,
                    "Release painted IC",
                    Color32::TRANSPARENT,
                    TEXT_MUTE,
                    Some(OUTLINE_VARIANT),
                    28.0,
                    ui.available_width(),
                ) {
                    self.f2d_painted = None;
                    self.f2d = None;
                    self.f2d_tex.clear();
                    self.f2d_sig = u64::MAX;
                    self.request_2d();
                }
                return;
            }
            if ui
                .checkbox(
                    &mut self.f2d_truth,
                    RichText::new("Compare to solver reference").color(TEXT_DIM),
                )
                .changed()
            {
                self.request_2d();
            }
            if self.f2d_truth {
                if let Some((rel, per)) = self.f2d.as_ref().and_then(|f| f.rel_l2.zip(f.persist)) {
                    ui.add_space(6.0);
                    diag(
                        ui,
                        "RelL2 vs solver reference",
                        &format!("{:.4}", rel),
                        if rel < per { SUCCESS } else { EMBER },
                    );
                    diag(ui, "Persistence floor", &format!("{:.4}", per), TEXT_DIM);
                    let x = per / rel.max(1e-6);
                    diag(
                        ui,
                        "Beats persistence",
                        &format!("{:.1}×", x),
                        if x > 1.0 { SUCCESS } else { EMBER },
                    );
                }
            }
        });

        ui.add_space(16.0);
        ui.label(caps("Field Insights"));
        ui.add_space(8.0);
        card(ui, |ui| {
            ui.checkbox(
                &mut self.insights_on,
                RichText::new("Pin critical points").color(TEXT_DIM),
            );
            if self.insights_on {
                ui.add_space(4.0);
                let kinds = [
                    field2d::InsightKind::PeakPressure,
                    field2d::InsightKind::SuctionPeak,
                    field2d::InsightKind::MaxVorticity,
                    field2d::InsightKind::MaxSpeed,
                    field2d::InsightKind::MaxError,
                ];
                for (k, kind) in kinds.iter().enumerate() {
                    let needs_truth = *kind == field2d::InsightKind::MaxError;
                    ui.add_enabled_ui(!needs_truth || self.f2d_truth, |ui| {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.insight_classes[k], "");
                            ui.label(RichText::new("●").color(insight_color(*kind)).size(9.0));
                            ui.label(RichText::new(kind.label()).color(TEXT_DIM).size(12.5));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(mono(kind.glyph(), TEXT_MUTE).size(11.0));
                            });
                        });
                    });
                }
            }
            ui.add_space(2.0);
            ui.label(
                RichText::new("hover a field panel to probe u · v · |v| · ω · recovered p/ρ")
                    .text_style(caption())
                    .color(TEXT_MUTE),
            );
        });

        ui.add_space(16.0);
        ui.label(caps("Pressure Recovery"));
        ui.add_space(8.0);
        let mut recompute = false;
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if seg(ui, "Spectral", self.f2d_method == PMethod::Spectral)
                    && self.f2d_method != PMethod::Spectral
                {
                    self.f2d_method = PMethod::Spectral;
                    recompute = true;
                }
                if seg(ui, "FD (iterative)", self.f2d_method == PMethod::Fd)
                    && self.f2d_method != PMethod::Fd
                {
                    self.f2d_method = PMethod::Fd;
                    recompute = true;
                }
            });
            if self.f2d_method == PMethod::Fd {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Tolerance").color(TEXT_DIM).size(12.5));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(mono(&format!("1e-{}", self.f2d_tol_exp), EMBER).size(12.0));
                    });
                });
                ui.spacing_mut().slider_width = ui.available_width() - 8.0;
                if ui
                    .add(
                        egui::Slider::new(&mut self.f2d_tol_exp, 2..=8)
                            .show_value(false)
                            .trailing_fill(true),
                    )
                    .changed()
                {
                    recompute = true;
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if seg(ui, "Periodic", self.f2d_boundary == PBoundary::Periodic)
                        && self.f2d_boundary != PBoundary::Periodic
                    {
                        self.f2d_boundary = PBoundary::Periodic;
                        recompute = true;
                    }
                    if seg(ui, "Dirichlet", self.f2d_boundary == PBoundary::Dirichlet)
                        && self.f2d_boundary != PBoundary::Dirichlet
                    {
                        self.f2d_boundary = PBoundary::Dirichlet;
                        recompute = true;
                    }
                });
            }
            ui.add_space(10.0);
            if action_button(
                ui,
                None,
                "RECOMPUTE RECOVERED PRESSURE",
                SURFACE_HIGH,
                TEXT,
                Some(OUTLINE),
                32.0,
                ui.available_width(),
            ) {
                recompute = true;
            }
            if let Some(f) = self.f2d.as_ref() {
                ui.add_space(10.0);
                let good = f.p_residual < 1e-3;
                diag(
                    ui,
                    &format!("Recovery error · {}", f.p_method),
                    &format!("{:.1e}", f.p_residual),
                    if good { SUCCESS } else { GOLD },
                );
                if f.p_iters > 0 {
                    diag(ui, "CG iterations", &format!("{}", f.p_iters), TEXT_DIM);
                }
                diag(
                    ui,
                    "Recovered max / min",
                    &format!("{:.2} / {:.2}", f.peak_p, f.low_p),
                    BRAND,
                );
            }
        });
        if recompute {
            self.request_2d();
        }
    }
}

fn benchmark_map_image(values: &[f32], n: usize, scale: f32, signed: bool) -> egui::ColorImage {
    if !signed {
        return field2d::magnitude_image(values, n, scale);
    }
    let pixels = values
        .iter()
        .map(|value| field2d::colormap_color((*value / scale.max(1e-12)).clamp(-1.0, 1.0), true))
        .collect();
    egui::ColorImage {
        size: [n, n],
        pixels,
        source_size: egui::Vec2::splat(n as f32),
    }
}

fn is_sha256(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn project_sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn benchmark_csv(benchmark: &engine::BenchResult) -> String {
    let mut csv = String::from("seed,provenance_stream,provenance_status,overlaps_reserved_stream");
    for horizon in &benchmark.horizons {
        csv += &format!(",rel_{horizon}x,persist_{horizon}x,improvement_{horizon}x");
    }
    csv.push('\n');
    for (seed_index, seed) in benchmark.seeds.iter().enumerate() {
        let record = benchmark
            .provenance
            .benchmark_seeds
            .iter()
            .find(|record| record.seed == *seed);
        let stream = record
            .map(|record| record.stream.as_str())
            .unwrap_or("unknown");
        let overlap = record.map(|record| record.overlap).unwrap_or(false);
        csv += &format!(
            "{seed},{stream},{},{}",
            benchmark.provenance.verdict, overlap
        );
        for horizon_index in 0..benchmark.horizons.len() {
            let rel = benchmark.rel[seed_index][horizon_index];
            let persist = benchmark.persist[seed_index][horizon_index];
            csv += &format!(",{rel:.6},{persist:.6},{:.6}", persist / rel.max(1e-12));
        }
        csv.push('\n');
    }
    csv
}

fn rng_stream_proposition(status: &str) -> &'static str {
    match status {
        "clean" => "no collision in checked RNG streams",
        "flagged" => "a checked RNG stream or provenance-contract conflict was found",
        _ => "RNG-stream collision status is unknown because required metadata is missing",
    }
}

fn benchmark_provenance_json(provenance: &engine::BenchProvenance) -> serde_json::Value {
    let checked_proposition = rng_stream_proposition(&provenance.verdict);
    let seeds: Vec<_> = provenance
        .benchmark_seeds
        .iter()
        .map(|record| {
            serde_json::json!({
                "seed": record.seed,
                "stream": record.stream,
                "overlap": record.overlap,
            })
        })
        .collect();
    serde_json::json!({
        "verdict": provenance.verdict,
        "checked_proposition": checked_proposition,
        "training_stream_seed": provenance.training_seed,
        "mixed_fork_stream_seed": provenance.mixed_fork_seed,
        "mixed_fork_used": provenance.mixed_fork_used,
        "validation_checkpoint_selection_stream_seed": provenance.validation_seed,
        "dataset": provenance.dataset,
        "benchmark_seeds": seeds,
        "overlap_count": provenance.overlap_count,
        "overlap_pct": provenance.overlap_pct,
        "checkpoint_epoch": provenance.epoch,
        "declared_epochs": provenance.declared_epochs,
        "checkpoint_role": provenance.checkpoint_role,
        "final_epoch_status": provenance.final_epoch_status,
        "selection_metric": provenance.selection_metric,
        "selection_stream": provenance.selection_stream,
        "source_fingerprint_present": provenance.source_fingerprint_present,
        "source_fingerprint_digest": provenance.source_fingerprint_digest,
        "legacy_unknown": provenance.legacy_unknown,
        "flags": provenance.flags,
        "validation_is_independent_test": false,
    })
}

fn benchmark_spatial_evidence_json(cell: &engine::BenchInspector) -> serde_json::Value {
    let variables: Vec<_> = InspectorVariable::ALL
        .iter()
        .filter_map(|&variable| {
            let (comparison_scale, error_scale) = cell.maps.scales(variable)?;
            let (mean, p95, max) = cell.maps.error_stats(variable)?;
            let (model_rms, reference_rms, error_rms) = cell.maps.rms(variable)?;
            Some(serde_json::json!({
                "key": variable.key(),
                "label": variable.label(),
                "symbol": variable.symbol(),
                "unit": variable.unit_key(),
                "signed": variable.signed(),
                "method": variable.method_note(),
                "sources": {
                    "model": variable.model_source(),
                    "solver_reference": variable.reference_source(),
                    "error": "DERIVED",
                },
                "comparison_scale": comparison_scale,
                "error_scale": error_scale,
                "rms": {
                    "model": model_rms,
                    "solver_reference": reference_rms,
                    "error": error_rms,
                },
                "absolute_error": {
                    "mean": mean,
                    "p95": p95,
                    "max": max,
                },
            }))
        })
        .collect();
    serde_json::json!({
        "schema": INSPECTOR_SCHEMA,
        "protocol_version": INSPECTOR_PROTOCOL_VERSION,
        "layout": INSPECTOR_LAYOUT,
        "domain": INSPECTOR_DOMAIN,
        "derivative": INSPECTOR_DERIVATIVE,
        "pressure": INSPECTOR_PRESSURE,
        "maps_embedded": false,
        "variables": variables,
    })
}

fn benchmark_report_card(
    benchmark: &engine::BenchResult,
    inspector: Option<&engine::BenchInspector>,
    run_id: &str,
    model_checkpoint_sha256: &str,
    timestamp_unix: u64,
) -> (String, String) {
    let selected_cell = inspector
        .filter(|cell| {
            benchmark.seeds.contains(&cell.seed) && benchmark.horizons.contains(&cell.horizon)
        })
        .map(|cell| {
            serde_json::json!({
                "seed": cell.seed,
                "horizon": cell.horizon,
                "seed_stream": cell.seed_stream,
                "provenance_status": cell.provenance_status,
                "provenance_checked_proposition": rng_stream_proposition(&cell.provenance_status),
                "rel_l2": cell.rel_l2,
                "persistence_rel_l2": cell.persist_rel_l2,
                "improvement_ratio": cell.improvement_ratio,
                "error_magnitude": {
                    "mean": cell.mean_abs_error,
                    "p95": cell.p95_abs_error,
                    "max": cell.max_abs_error,
                },
                "divergence_rms": {
                    "model": cell.divergence_model_rms,
                    "solver_reference": cell.divergence_truth_rms,
                    "error": cell.divergence_error_rms,
                },
                "energy_spectrum": {
                    "relative_l2": cell.spectrum_rel_l2,
                    "wavenumber": cell.spectrum_k,
                    "model": cell.spectrum_model,
                    "solver_reference": cell.spectrum_truth,
                },
                "spatial_variable_evidence": benchmark_spatial_evidence_json(cell),
            })
        });
    let warnings: Vec<_> = benchmark
        .provenance
        .flags
        .iter()
        .cloned()
        .chain(
            benchmark
                .provenance
                .legacy_unknown
                .iter()
                .map(|field| format!("UNKNOWN provenance field: {field}")),
        )
        .collect();
    let mut payload = serde_json::json!({
        "report_schema": crate::benchmark_export::REPORT_CARD_SCHEMA,
        "product": "reyn-studio",
        "version": env!("CARGO_PKG_VERSION"),
        "run_id": run_id,
        "protocol_id": "reyn.benchmark.seed-horizon.v1",
        "model": benchmark.model,
        "model_checkpoint_sha256": model_checkpoint_sha256,
        "grid": benchmark.grid,
        "epoch": benchmark.epoch,
        "timestamp_unix": timestamp_unix,
        "seeds": benchmark.seeds,
        "horizons": benchmark.horizons,
        "rel_l2": benchmark.rel,
        "persistence": benchmark.persist,
        "global_rel_l2": benchmark.global_rel,
        "runtime_s": benchmark.runtime_s,
        "protocol": "exact benchmark RNG seeds; training, mixed-fork (+10000), and validation/checkpoint-selection (+50000) streams classified from checkpoint metadata; validation is never independent test evidence",
        "provenance": benchmark_provenance_json(&benchmark.provenance),
        "selected_cell_evidence": selected_cell,
        "warnings": warnings,
        "limitations": [
            "a numerical solver output is a solver reference, not automatically physical truth",
            "recovered pressure is density-normalized and is not physical Cp without a recorded reference state",
            "RNG-stream non-collision is not field-space or trajectory non-overlap",
            "consistency, provenance, integrity, authenticity, and independent accuracy/validation are separate claims",
        ],
        "integrity_algorithm": "SHA-256",
        "authenticity": {
            "status": "UNSIGNED",
            "signature": null,
            "reason": "no organization-key signature was produced",
        },
        "verification": {
            "canonical_payload": "remove only integrity_sha256, then serialize compact UTF-8 JSON with object keys in lexical order",
            "integrity": "compute SHA-256 over the canonical payload and compare with integrity_sha256",
            "authenticity": "UNSIGNED; no organization identity is asserted by the JSON, PNG, or PDF",
        },
        "canonicalization": "SHA-256 over compact UTF-8 JSON for this object before integrity_sha256 is inserted; object keys use serde_json lexical order",
    });
    // Normalize f32-backed JSON numbers through the exact UTF-8 representation
    // a verifier will parse. Hashing the pre-serialization Value can otherwise
    // differ after a selected-cell payload is written and read as JSON.
    payload = serde_json::from_slice(
        &serde_json::to_vec(&payload).expect("report payload is JSON-serializable"),
    )
    .expect("serialized report payload parses");
    // Normalize through JSON once so a verifier that parses the exported
    // pretty JSON reconstructs byte-identical canonical numbers.
    let first_pass = serde_json::to_vec(&payload).expect("report payload is JSON-serializable");
    payload = serde_json::from_slice(&first_pass).expect("serialized report payload is valid JSON");
    let canonical = serde_json::to_vec(&payload).expect("report payload is JSON-serializable");
    use sha2::Digest;
    let digest = sha2::Sha256::digest(&canonical);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    payload
        .as_object_mut()
        .expect("report payload is an object")
        .insert(
            "integrity_sha256".into(),
            serde_json::Value::String(hex.clone()),
        );
    let mut report =
        serde_json::to_string_pretty(&payload).expect("report payload is JSON-serializable");
    report.push('\n');
    (report, hex)
}

fn stream_label(stream: &str) -> &'static str {
    match stream {
        "training" => "TRAINING",
        "mixed_fork" => "MIXED FORK",
        "validation_selection" => "VALIDATION / SELECTION",
        "fresh_test" => "FRESH TEST",
        _ => "UNKNOWN",
    }
}

fn stream_color(stream: &str) -> Color32 {
    match stream {
        "fresh_test" => SUCCESS,
        "training" | "mixed_fork" | "validation_selection" => DATA_RED,
        _ => GOLD,
    }
}

fn provenance_color(status: &str) -> Color32 {
    match status {
        "clean" => SUCCESS,
        "flagged" => DATA_RED,
        _ => GOLD,
    }
}

fn paint_bench_provenance(p: &egui::Painter, rect: Rect, provenance: &engine::BenchProvenance) {
    p.rect_filled(rect, CornerRadius::same(3), SURFACE);
    p.rect_stroke(
        rect,
        CornerRadius::same(3),
        Stroke::new(1.0, OUTLINE_VARIANT),
        egui::StrokeKind::Inside,
    );
    let x = rect.min.x + 14.0;
    let mut y = rect.min.y + 13.0;
    p.text(
        egui::pos2(x, y),
        Align2::LEFT_TOP,
        "LEAK & PROVENANCE",
        FontId::monospace(9.5),
        TEXT_MUTE,
    );
    let mark = match provenance.verdict.as_str() {
        "clean" => "✓ RNG STREAMS CLEAN",
        "flagged" => "! FLAGGED",
        _ => "? UNKNOWN",
    };
    p.text(
        egui::pos2(rect.max.x - 14.0, y),
        Align2::RIGHT_TOP,
        mark,
        FontId::monospace(10.0),
        provenance_color(&provenance.verdict),
    );
    y += 28.0;

    let mut row = |label: &str, value: String, color: Color32| {
        p.text(
            egui::pos2(x, y),
            Align2::LEFT_TOP,
            label,
            FontId::proportional(9.5),
            TEXT_MUTE,
        );
        p.text(
            egui::pos2(rect.max.x - 14.0, y),
            Align2::RIGHT_TOP,
            value,
            FontId::monospace(9.5),
            color,
        );
        y += 22.0;
    };
    let seed = |value: Option<i64>| {
        value
            .map(|number| number.to_string())
            .unwrap_or_else(|| "unknown".into())
    };
    row("training stream", seed(provenance.training_seed), TEXT_DIM);
    row(
        "mixed-fork stream",
        format!(
            "{} · {}",
            seed(provenance.mixed_fork_seed),
            if provenance.mixed_fork_used {
                "used"
            } else {
                "reserved"
            }
        ),
        if provenance.mixed_fork_used {
            TEXT_DIM
        } else {
            TEXT_MUTE
        },
    );
    row(
        "validation / selection",
        seed(provenance.validation_seed),
        GOLD,
    );
    let fresh = provenance
        .benchmark_seeds
        .iter()
        .filter(|record| record.stream == "fresh_test")
        .count();
    row(
        "benchmark seeds",
        format!(
            "{fresh} fresh · {} overlap ({:.0}%)",
            provenance.overlap_count, provenance.overlap_pct
        ),
        if provenance.overlap_count == 0 {
            SUCCESS
        } else {
            DATA_RED
        },
    );
    let epoch = match (provenance.epoch, provenance.declared_epochs) {
        (Some(epoch), Some(declared)) => format!("{epoch}/{declared}"),
        (Some(epoch), None) => format!("{epoch}/?"),
        _ => "unknown".into(),
    };
    row(
        "final epoch",
        format!(
            "{epoch} · {}",
            provenance.final_epoch_status.replace('_', " ")
        ),
        if provenance.final_epoch_status == "fixed_final" {
            SUCCESS
        } else {
            GOLD
        },
    );
    let source = provenance
        .source_fingerprint_digest
        .as_deref()
        .map(|digest| {
            let short: String = digest.chars().take(10).collect();
            format!("present · {short}…")
        })
        .unwrap_or_else(|| "absent · legacy unknown".into());
    row(
        "source fingerprint",
        source,
        if provenance.source_fingerprint_present {
            SUCCESS
        } else {
            GOLD
        },
    );
    row(
        "checkpoint selection",
        format!(
            "{} · {}",
            provenance.selection_stream, provenance.checkpoint_role
        ),
        if provenance.selection_stream == "validation" {
            GOLD
        } else {
            TEXT_DIM
        },
    );
    p.text(
        egui::pos2(x, y + 1.0),
        Align2::LEFT_TOP,
        "Validation selects checkpoints; it is not independent testing.",
        FontId::proportional(9.0),
        GOLD,
    );
    y += 25.0;
    let finding = if !provenance.flags.is_empty() {
        format!("! {} protocol flag(s)", provenance.flags.len())
    } else if !provenance.legacy_unknown.is_empty() {
        format!(
            "? {} legacy metadata gap(s)",
            provenance.legacy_unknown.len()
        )
    } else {
        "✓ no collision in checked RNG streams".into()
    };
    p.text(
        egui::pos2(x, y),
        Align2::LEFT_TOP,
        finding,
        FontId::monospace(9.0),
        provenance_color(&provenance.verdict),
    );
}

fn paint_evidence_state(p: &egui::Painter, rect: Rect, title: &str, detail: &str, color: Color32) {
    p.rect_filled(rect, CornerRadius::same(3), SURFACE);
    p.rect_stroke(
        rect,
        CornerRadius::same(3),
        Stroke::new(1.0, OUTLINE_VARIANT),
        egui::StrokeKind::Inside,
    );
    p.text(
        rect.left_center() + Vec2::new(16.0, -12.0),
        Align2::LEFT_CENTER,
        title,
        FontId::monospace(10.0),
        color,
    );
    p.text(
        rect.left_center() + Vec2::new(16.0, 13.0),
        Align2::LEFT_CENTER,
        detail,
        FontId::proportional(10.5),
        TEXT_MUTE,
    );
}

fn paint_energy_spectrum(p: &egui::Painter, rect: Rect, cell: &engine::BenchInspector) {
    p.rect_filled(rect, CornerRadius::same(2), SURFACE_LOWEST);
    p.rect_stroke(
        rect,
        CornerRadius::same(2),
        Stroke::new(1.0, OUTLINE_VARIANT),
        egui::StrokeKind::Inside,
    );
    if cell.spectrum_k.len() < 2 {
        p.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "no spectral bins",
            FontId::monospace(9.0),
            TEXT_MUTE,
        );
        return;
    }
    let plot = Rect::from_min_max(
        rect.min + Vec2::new(34.0, 13.0),
        rect.max - Vec2::new(9.0, 23.0),
    );
    let k_min = cell.spectrum_k[0].max(1e-6).log10();
    let k_max = cell
        .spectrum_k
        .last()
        .copied()
        .unwrap_or(1.0)
        .max(1e-6)
        .log10();
    let (mut e_min, mut e_max) = (f32::INFINITY, f32::NEG_INFINITY);
    for energy in cell.spectrum_model.iter().chain(&cell.spectrum_truth) {
        let value = energy.max(1e-20).log10();
        e_min = e_min.min(value);
        e_max = e_max.max(value);
    }
    if !e_min.is_finite() || !e_max.is_finite() {
        return;
    }
    if (e_max - e_min).abs() < 1e-6 {
        e_min -= 1.0;
        e_max += 1.0;
    }
    let point = |k: f32, energy: f32| {
        let tx = (k.max(1e-6).log10() - k_min) / (k_max - k_min).max(1e-6);
        let ty = (energy.max(1e-20).log10() - e_min) / (e_max - e_min);
        egui::pos2(
            egui::lerp(plot.x_range(), tx.clamp(0.0, 1.0)),
            egui::lerp(plot.y_range(), 1.0 - ty.clamp(0.0, 1.0)),
        )
    };
    for fraction in [0.0, 0.5, 1.0] {
        let gy = egui::lerp(plot.y_range(), fraction);
        p.line_segment(
            [egui::pos2(plot.min.x, gy), egui::pos2(plot.max.x, gy)],
            Stroke::new(1.0, OUTLINE_VARIANT.gamma_multiply(0.45)),
        );
    }
    let model: Vec<_> = cell
        .spectrum_k
        .iter()
        .zip(&cell.spectrum_model)
        .map(|(&k, &energy)| point(k, energy))
        .collect();
    let truth: Vec<_> = cell
        .spectrum_k
        .iter()
        .zip(&cell.spectrum_truth)
        .map(|(&k, &energy)| point(k, energy))
        .collect();
    p.add(egui::Shape::line(model, Stroke::new(1.5, EMBER)));
    p.add(egui::Shape::line(truth.clone(), Stroke::new(1.5, TERTIARY)));
    for marker in truth.iter().step_by((truth.len() / 6).max(1)) {
        p.circle_stroke(*marker, 2.0, Stroke::new(1.0, TERTIARY));
    }
    p.text(
        rect.min + Vec2::new(8.0, 8.0),
        Align2::LEFT_TOP,
        "MODEL —",
        FontId::monospace(7.5),
        EMBER,
    );
    p.text(
        rect.min + Vec2::new(70.0, 8.0),
        Align2::LEFT_TOP,
        "SOLVER REF ○",
        FontId::monospace(7.5),
        TERTIARY,
    );
    p.text(
        egui::pos2(plot.center().x, rect.max.y - 4.0),
        Align2::CENTER_BOTTOM,
        format!(
            "wavenumber k · 1 → {:.0}",
            cell.spectrum_k.last().unwrap_or(&1.0)
        ),
        FontId::monospace(7.5),
        TEXT_MUTE,
    );
    p.text(
        egui::pos2(rect.min.x + 5.0, plot.center().y),
        Align2::LEFT_CENTER,
        "log E",
        FontId::monospace(7.5),
        TEXT_MUTE,
    );
}

// -- field instrument layer (insights · probe · legend) -----------------------

/// DESIGN.md reserves gold / blue / green / red for data semantics — this is
/// that vocabulary. Ember (the action color) marks the model-error hotspot.
fn insight_color(kind: field2d::InsightKind) -> Color32 {
    match kind {
        field2d::InsightKind::PeakPressure => DATA_RED,
        field2d::InsightKind::SuctionPeak => TERTIARY,
        field2d::InsightKind::MaxVorticity => GOLD,
        field2d::InsightKind::MaxSpeed => SUCCESS,
        field2d::InsightKind::MaxError => EMBER,
    }
}

/// Same vocabulary in 3D: gold rotation, green speed, blue for the vortex core
/// (Q-max ≈ the low-pressure core — the same semantic as 2D's suction blue).
fn insight3d_color(kind: flow::Insight3DKind) -> Color32 {
    match kind {
        flow::Insight3DKind::VortexCore => TERTIARY,
        flow::Insight3DKind::MaxVorticity => GOLD,
        flow::Insight3DKind::MaxSpeed => SUCCESS,
        flow::Insight3DKind::SurfLoad => DATA_RED, // stagnation: where the flow pushes
        flow::Insight3DKind::SurfSuction => TERTIARY, // suction peak: the weak point
    }
}

/// One pinned 2D critical point: grid cell → panel position, then the shared
/// instrument marker (`viewport::insight_marker`).
fn draw_insight(
    p: &egui::Painter,
    panel: Rect,
    n: usize,
    ins: &field2d::Insight,
    chips: &mut Vec<Rect>,
) {
    let c = egui::pos2(
        panel.min.x + (ins.j as f32 + 0.5) / n as f32 * panel.width(),
        panel.min.y + (ins.i as f32 + 0.5) / n as f32 * panel.height(),
    );
    viewport::insight_marker(
        p,
        panel,
        c,
        insight_color(ins.kind),
        &format!("{} {:.3}", ins.kind.glyph(), ins.value),
        chips,
    );
}

fn blend_color(base: Color32, overlay: Color32, amount: f32) -> Color32 {
    let amount = amount.clamp(0.0, 1.0);
    let channel = |base: u8, overlay: u8| {
        (base as f32 + (overlay as f32 - base as f32) * amount).round() as u8
    };
    Color32::from_rgb(
        channel(base.r(), overlay.r()),
        channel(base.g(), overlay.g()),
        channel(base.b(), overlay.b()),
    )
}

fn engineering_section_image(section: &engineering_section::SectionPlane) -> egui::ColorImage {
    let n = section.n;
    let is_boundary = |row: usize, column: usize| {
        let solid = section.mask_value(row, column) >= 0.5;
        [
            row.checked_sub(1).map(|neighbor| (neighbor, column)),
            (row + 1 < n).then_some((row + 1, column)),
            column.checked_sub(1).map(|neighbor| (row, neighbor)),
            (column + 1 < n).then_some((row, column + 1)),
        ]
        .into_iter()
        .flatten()
        .any(|(neighbor_row, neighbor_column)| {
            (section.mask_value(neighbor_row, neighbor_column) >= 0.5) != solid
        })
    };
    let lic = section.in_plane.as_ref().map(|uv| {
        engineering_section::line_integral_convolution(n, uv, 12)
    });
    let mut pixels = Vec::with_capacity(n * n);
    for row in 0..n {
        for column in 0..n {
            let mask = section.mask_value(row, column).clamp(0.0, 1.0);
            let mut base = field2d::colormap_color(
                section.scale.normalize(section.value(row, column)),
                section.quantity.signed(),
            );
            if let Some(lic) = lic.as_ref() {
                // Modulate speed colormap with LIC texture for Fluent-style planar flow.
                let t = lic[row * n + column];
                let shade = 0.45 + 0.70 * t;
                base = Color32::from_rgba_unmultiplied(
                    ((base.r() as f32) * shade).min(255.0) as u8,
                    ((base.g() as f32) * shade).min(255.0) as u8,
                    ((base.b() as f32) * shade).min(255.0) as u8,
                    base.a(),
                );
            }
            let color = if is_boundary(row, column) {
                BRAND
            } else if mask >= 0.5 {
                SURFACE_LOWEST
            } else if mask > 0.01 {
                blend_color(base, SURFACE_LOWEST, mask * 0.7)
            } else {
                base
            };
            pixels.push(color);
        }
    }
    egui::ColorImage {
        size: [n, n],
        pixels,
        source_size: Vec2::new(n as f32, n as f32),
    }
}

fn format_section_value(value: f32, units: &str) -> String {
    let magnitude = value.abs();
    if magnitude >= 10_000.0 || (magnitude > 0.0 && magnitude < 0.001) {
        format!("{value:.4e} {units}")
    } else {
        format!("{value:.4} {units}")
    }
}

fn engineering_section_legend(
    painter: &egui::Painter,
    panel: Rect,
    section: &engineering_section::SectionPlane,
) {
    let bar = Rect::from_min_size(
        egui::pos2(panel.min.x, panel.max.y + 36.0),
        Vec2::new(panel.width(), 8.0),
    );
    let signed = section.quantity.signed();
    let strips = 64;
    for strip in 0..strips {
        let fraction = (strip as f32 + 0.5) / strips as f32;
        let normalized = if signed {
            fraction * 2.0 - 1.0
        } else {
            fraction
        };
        let x0 = bar.min.x + bar.width() * strip as f32 / strips as f32;
        let x1 = bar.min.x + bar.width() * (strip + 1) as f32 / strips as f32;
        painter.rect_filled(
            Rect::from_min_max(egui::pos2(x0, bar.min.y), egui::pos2(x1, bar.max.y)),
            CornerRadius::ZERO,
            field2d::colormap_color(normalized, signed),
        );
    }
    painter.rect_stroke(
        bar,
        CornerRadius::ZERO,
        Stroke::new(1.0, OUTLINE_VARIANT),
        egui::StrokeKind::Outside,
    );
    let font = FontId::monospace(9.5);
    let label_y = bar.max.y + 4.0;
    painter.text(
        egui::pos2(bar.min.x, label_y),
        Align2::LEFT_TOP,
        format_section_value(section.scale.legend_minimum(), section.quantity.units()),
        font.clone(),
        TEXT_MUTE,
    );
    if let Some(center) = section.scale.center {
        painter.text(
            egui::pos2(bar.center().x, label_y),
            Align2::CENTER_TOP,
            format_section_value(center, section.quantity.units()),
            font.clone(),
            TEXT_MUTE,
        );
    }
    painter.text(
        egui::pos2(bar.max.x, label_y),
        Align2::RIGHT_TOP,
        format_section_value(section.scale.legend_maximum(), section.quantity.units()),
        font,
        TEXT_MUTE,
    );
}

fn draw_engineering_section_probe(
    painter: &egui::Painter,
    panel: Rect,
    clip: Rect,
    section: &engineering_section::SectionPlane,
    position: egui::Pos2,
) {
    let column = (((position.x - panel.min.x) / panel.width() * section.n as f32) as usize)
        .min(section.n - 1);
    let row = (((position.y - panel.min.y) / panel.height() * section.n as f32) as usize)
        .min(section.n - 1);
    let center = egui::pos2(
        panel.min.x + (column as f32 + 0.5) / section.n as f32 * panel.width(),
        panel.min.y + (row as f32 + 0.5) / section.n as f32 * panel.height(),
    );
    let crosshair = Stroke::new(1.0, OUTLINE.gamma_multiply(0.55));
    painter.line_segment(
        [
            egui::pos2(panel.min.x, center.y),
            egui::pos2(panel.max.x, center.y),
        ],
        crosshair,
    );
    painter.line_segment(
        [
            egui::pos2(center.x, panel.min.y),
            egui::pos2(center.x, panel.max.y),
        ],
        crosshair,
    );
    painter.circle_stroke(center, 4.0, Stroke::new(1.0, TEXT_DIM));

    let mask = section.mask_value(row, column);
    let lines = [
        (
            format!(
                "{} cell {} · pixel {},{}",
                section.axis.label(),
                section.index,
                column,
                row
            ),
            TEXT_MUTE,
        ),
        (
            format_section_value(section.value(row, column), section.quantity.units()),
            TEXT,
        ),
        (
            if mask >= 0.5 {
                format!("stored mask {mask:.3} · SOLID GEOMETRY")
            } else {
                format!("stored mask {mask:.3} · FLUID")
            },
            if mask >= 0.5 { BRAND } else { TEXT_DIM },
        ),
    ];
    let font = FontId::monospace(10.0);
    let galleys = lines
        .iter()
        .map(|(text, color)| painter.layout_no_wrap(text.clone(), font.clone(), *color))
        .collect::<Vec<_>>();
    let width = galleys
        .iter()
        .map(|galley| galley.size().x)
        .fold(0.0, f32::max)
        + 16.0;
    let line_height = galleys[0].size().y + 3.0;
    let size = Vec2::new(width, line_height * galleys.len() as f32 + 10.0);
    let mut origin = position + Vec2::new(14.0, 14.0);
    if origin.x + size.x > clip.max.x - 8.0 {
        origin.x = position.x - size.x - 14.0;
    }
    if origin.y + size.y > clip.max.y - 8.0 {
        origin.y = position.y - size.y - 14.0;
    }
    let chip = Rect::from_min_size(origin, size);
    painter.rect_filled(chip, CornerRadius::same(2), SURFACE);
    painter.rect_stroke(
        chip,
        CornerRadius::same(2),
        Stroke::new(1.0, OUTLINE_VARIANT),
        egui::StrokeKind::Inside,
    );
    for (line, galley) in galleys.into_iter().enumerate() {
        painter.galley(
            origin + Vec2::new(8.0, 5.0 + line as f32 * line_height),
            galley,
            lines[line].1,
        );
    }
}

/// Calibrated colormap legend: gradient bar + labeled range (shared
/// model/reference scale, so the two panels read on the same instrument).
fn legend_bar(p: &egui::Painter, panel: Rect, scale: f32, signed: bool) {
    let bar = Rect::from_min_size(
        egui::pos2(panel.min.x, panel.max.y + 24.0),
        Vec2::new(panel.width(), 8.0),
    );
    let strips = 48;
    for k in 0..strips {
        let t = (k as f32 + 0.5) / strips as f32;
        let val = if signed { t * 2.0 - 1.0 } else { t };
        let x0 = bar.min.x + bar.width() * k as f32 / strips as f32;
        let x1 = bar.min.x + bar.width() * (k + 1) as f32 / strips as f32;
        p.rect_filled(
            Rect::from_min_max(egui::pos2(x0, bar.min.y), egui::pos2(x1, bar.max.y)),
            CornerRadius::ZERO,
            field2d::colormap_color(val, signed),
        );
    }
    p.rect_stroke(
        bar,
        CornerRadius::ZERO,
        Stroke::new(1.0, OUTLINE_VARIANT),
        egui::StrokeKind::Outside,
    );
    let y = bar.max.y + 4.0;
    let font = FontId::monospace(10.0);
    if signed {
        p.text(
            egui::pos2(bar.min.x, y),
            Align2::LEFT_TOP,
            format!("-{scale:.2}"),
            font.clone(),
            TEXT_MUTE,
        );
        p.text(
            egui::pos2(bar.center().x, y),
            Align2::CENTER_TOP,
            "0",
            font.clone(),
            TEXT_MUTE,
        );
        p.text(
            egui::pos2(bar.max.x, y),
            Align2::RIGHT_TOP,
            format!("+{scale:.2}"),
            font,
            TEXT_MUTE,
        );
    } else {
        p.text(
            egui::pos2(bar.min.x, y),
            Align2::LEFT_TOP,
            "0",
            font.clone(),
            TEXT_MUTE,
        );
        p.text(
            egui::pos2(bar.max.x, y),
            Align2::RIGHT_TOP,
            format!("{scale:.2}"),
            font,
            TEXT_MUTE,
        );
    }
}

/// Live hover probe: crosshair at the cell + a full readout chip (u, v, |v|,
/// ω, recovered p/ρ) beside the cursor. Native-side sampling — zero latency, no engine
/// round-trip.
fn draw_probe(p: &egui::Painter, panel: Rect, clip: Rect, n: usize, src: &[f32], pos: egui::Pos2) {
    let j = (((pos.x - panel.min.x) / panel.width() * n as f32) as usize).min(n - 1);
    let i = (((pos.y - panel.min.y) / panel.height() * n as f32) as usize).min(n - 1);
    let cx = panel.min.x + (j as f32 + 0.5) / n as f32 * panel.width();
    let cy = panel.min.y + (i as f32 + 0.5) / n as f32 * panel.height();
    let cross = Stroke::new(1.0, OUTLINE.gamma_multiply(0.45));
    p.line_segment(
        [egui::pos2(panel.min.x, cy), egui::pos2(panel.max.x, cy)],
        cross,
    );
    p.line_segment(
        [egui::pos2(cx, panel.min.y), egui::pos2(cx, panel.max.y)],
        cross,
    );
    p.circle_stroke(egui::pos2(cx, cy), 4.0, Stroke::new(1.0, TEXT_DIM));

    let s = field2d::probe(src, n, i, j);
    let font = FontId::monospace(10.5);
    let lines = [
        (format!("cell {j},{i}"), TEXT_MUTE),
        (format!("u   {:+.3}", s.u), TEXT_DIM),
        (format!("v   {:+.3}", s.v), TEXT_DIM),
        (format!("|v| {:.3}", s.speed), TEXT),
        (format!("ω   {:+.3}", s.omega), TEXT),
        (format!("p/ρ {:+.3}", s.p), TEXT),
    ];
    let galleys: Vec<_> = lines
        .iter()
        .map(|(t, c)| p.layout_no_wrap(t.clone(), font.clone(), *c))
        .collect();
    let w = galleys.iter().map(|g| g.size().x).fold(0.0, f32::max) + 16.0;
    let line_h = galleys[0].size().y + 2.0;
    let h = line_h * galleys.len() as f32 + 10.0;
    let mut anchor = pos + Vec2::new(16.0, 12.0);
    if anchor.x + w > clip.max.x - 4.0 {
        anchor.x = pos.x - 16.0 - w;
    }
    if anchor.y + h > clip.max.y - 4.0 {
        anchor.y = pos.y - 12.0 - h;
    }
    let chip = Rect::from_min_size(anchor, Vec2::new(w, h));
    p.rect_filled(chip, CornerRadius::same(3), SURFACE);
    p.rect_stroke(
        chip,
        CornerRadius::same(3),
        Stroke::new(1.0, OUTLINE),
        egui::StrokeKind::Inside,
    );
    let colors = [TEXT_MUTE, TEXT_DIM, TEXT_DIM, TEXT, TEXT, TEXT];
    for (k, g) in galleys.into_iter().enumerate() {
        p.galley(
            chip.min + Vec2::new(8.0, 5.0 + k as f32 * line_h),
            g,
            colors[k],
        );
    }
}

// -- reusable widgets --------------------------------------------------------
fn with_project_extension(mut path: std::path::PathBuf) -> std::path::PathBuf {
    let is_project = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("reynproj"));
    if !is_project {
        path.set_extension("reynproj");
    }
    path
}

fn project_fact(ui: &mut egui::Ui, label: &str, value: &str) {
    Frame::NONE
        .fill(SURFACE_LOW)
        .corner_radius(CornerRadius::same(2))
        .inner_margin(Margin::symmetric(9, 6))
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!("{label}  {value}"))
                    .text_style(mono_s())
                    .color(TEXT_DIM),
            );
        });
}

fn seg(ui: &mut egui::Ui, label: &str, active: bool) -> bool {
    // Selection is tonal (bg-4 thumb + primary text), not accent-filled —
    // ember stays reserved for the screen's one primary action.
    let (fill, color) = if active {
        (SURFACE_HIGHEST, TEXT)
    } else {
        (Color32::TRANSPARENT, TEXT_DIM)
    };
    ui.add(
        egui::Button::new(RichText::new(label).text_style(body_strong()).color(color))
            .fill(fill)
            .corner_radius(CornerRadius::same(R1))
            .stroke(Stroke::NONE),
    )
    .clicked()
}

fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

fn short_hash(value: &str) -> String {
    let prefix: String = value.chars().take(12).collect();
    if value.chars().count() > 12 {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn format_stages(stages: &[project::DependencyStage]) -> String {
    stages
        .iter()
        .map(|stage| match stage {
            project::DependencyStage::Contract => "contract",
            project::DependencyStage::Discretization => "discretization",
            project::DependencyStage::Run => "run",
            project::DependencyStage::Evidence => "evidence",
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn source_kind_label(kind: project::SourceKind) -> &'static str {
    match kind {
        project::SourceKind::Geometry => "GEOMETRY",
        project::SourceKind::Model => "MODEL",
        project::SourceKind::Reference => "REFERENCE",
    }
}

fn dependency_kind_label(kind: project_lifecycle::DependencyKind) -> &'static str {
    match kind {
        project_lifecycle::DependencyKind::Engine => "ENGINE",
        project_lifecycle::DependencyKind::Model => "MODEL",
        project_lifecycle::DependencyKind::Source => "SOURCE",
        project_lifecycle::DependencyKind::Artifact => "ARTIFACT",
        project_lifecycle::DependencyKind::Integrity => "INTEGRITY",
    }
}

fn content_state_presentation(state: project::ContentState) -> (&'static str, Color32) {
    match state {
        project::ContentState::Available => ("BUNDLED · VERIFIED", SUCCESS),
        project::ContentState::Missing => ("BUNDLE OBJECT MISSING", WARN),
        project::ContentState::Corrupt => ("BUNDLE OBJECT CORRUPT", DATA_RED),
    }
}

fn determinism_status_label(status: project::DeterminismStatus) -> &'static str {
    match status {
        project::DeterminismStatus::WithinTolerance => "WITHIN DECLARED TOLERANCE",
        project::DeterminismStatus::Difference => "DIFFERENCE EXPOSED",
        project::DeterminismStatus::NotComparable => "NOT COMPARABLE",
    }
}

fn determinism_status_color(status: project::DeterminismStatus) -> Color32 {
    match status {
        project::DeterminismStatus::WithinTolerance => SUCCESS,
        project::DeterminismStatus::Difference => DATA_RED,
        project::DeterminismStatus::NotComparable => WARN,
    }
}

fn optional_scalar(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.6e}"))
        .unwrap_or_else(|| "missing".into())
}

/// Lifecycle state glyph for a workflow stage: shape + color always travel
/// together (never color-only, SCI honesty rules).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StageGlyph {
    Complete,
    Partial,
    Empty,
}

impl StageGlyph {
    fn symbol(self) -> &'static str {
        match self {
            Self::Complete => "●",
            Self::Partial => "◐",
            Self::Empty => "○",
        }
    }

    fn color(self) -> Color32 {
        match self {
            Self::Complete => SUCCESS,
            // WARN is the status hue (§3.3); GOLD stays reserved for data.
            Self::Partial => WARN,
            Self::Empty => TEXT_MUTE,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct StageState {
    glyph: StageGlyph,
    /// `Some(reason)` disables the stage; the reason renders inline (A9) and
    /// the click never fires — no silent redirect (A5).
    blocked: Option<&'static str>,
}

/// Pure lifecycle model for the workflow rail (§4.1). Order: Project, Case
/// Setup, Results, Evidence. Results is the only gated stage — it requires a
/// completed immutable run in the open case.
fn workflow_stage_states(
    project_has_content: bool,
    case_open: bool,
    has_result: bool,
    has_runs: bool,
) -> [StageState; 4] {
    let case_glyph = if has_result {
        StageGlyph::Complete
    } else if case_open {
        StageGlyph::Partial
    } else {
        StageGlyph::Empty
    };
    [
        StageState {
            glyph: if project_has_content {
                StageGlyph::Complete
            } else {
                StageGlyph::Empty
            },
            blocked: None,
        },
        StageState {
            glyph: case_glyph,
            blocked: None,
        },
        StageState {
            glyph: if has_result {
                StageGlyph::Complete
            } else {
                StageGlyph::Empty
            },
            blocked: (!has_result).then_some("needs a completed run"),
        },
        StageState {
            glyph: if has_runs {
                StageGlyph::Complete
            } else {
                StageGlyph::Empty
            },
            blocked: None,
        },
    ]
}

/// Workflow-stage nav row: icon + label + right-aligned state glyph, an
/// inline blocked-reason line when gated, animated hover, ember edge marker
/// when active, focus ring for keyboard navigation.
fn stage_row(ui: &mut egui::Ui, icon: &str, label: &str, active: bool, state: StageState) -> bool {
    let blocked = state.blocked.is_some();
    // §3.2 rhythm: nav rows are 32px; a blocked row adds one caption line.
    let height = if blocked { 48.0 } else { 32.0 };
    // Blocked rows stay focusable (Sense::click) so the inline reason is
    // reachable by keyboard; activation is ignored below (QA C11).
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    let hover = motion_t(
        ui.ctx(),
        resp.id.with("hover"),
        resp.hovered() && !active && !blocked,
        0.14,
    );
    let label_font = body_strong().resolve(ui.style());
    let small_font = egui::TextStyle::Small.resolve(ui.style());
    let painter = ui.painter();
    let fill = if active {
        SURFACE_HIGH
    } else {
        SURFACE.gamma_multiply(hover)
    };
    painter.rect_filled(rect, CornerRadius::same(R1), fill);
    if active {
        // 2px ember edge marker — ember is a mark, never a nav fill (§3.3).
        painter.rect_filled(
            Rect::from_min_size(rect.min + Vec2::new(0.0, 6.0), Vec2::new(2.0, 32.0 - 12.0)),
            CornerRadius::same(1),
            EMBER,
        );
    }
    if resp.has_focus() {
        painter.rect_stroke(
            rect.expand(1.0),
            CornerRadius::same(R1),
            focus_stroke(),
            egui::StrokeKind::Outside,
        );
    }
    let fg = if blocked {
        TEXT_MUTE
    } else if active || resp.hovered() {
        TEXT
    } else {
        TEXT_DIM
    };
    let row_y = rect.min.y + 16.0;
    painter.text(
        egui::pos2(rect.min.x + 12.0, row_y),
        Align2::LEFT_CENTER,
        icon,
        FontId::proportional(15.0),
        fg,
    );
    painter.text(
        egui::pos2(rect.min.x + 36.0, row_y),
        Align2::LEFT_CENTER,
        label,
        label_font,
        fg,
    );
    painter.text(
        egui::pos2(rect.max.x - 12.0, row_y),
        Align2::RIGHT_CENTER,
        state.glyph.symbol(),
        FontId::proportional(11.0),
        state.glyph.color(),
    );
    if let Some(reason) = state.blocked {
        painter.text(
            egui::pos2(rect.min.x + 36.0, rect.min.y + 36.0),
            Align2::LEFT_CENTER,
            format!("○ {reason}"),
            small_font,
            TEXT_MUTE,
        );
    }
    !blocked && resp.clicked()
}

/// Destination nav row (Model Library, Settings, sandbox tools): no lifecycle
/// glyph, same active/hover/focus grammar as [`stage_row`].
fn nav_row(ui: &mut egui::Ui, icon: &str, label: &str, active: bool) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    let hover = motion_t(
        ui.ctx(),
        resp.id.with("hover"),
        resp.hovered() && !active,
        0.14,
    );
    let label_font = body_strong().resolve(ui.style());
    let painter = ui.painter();
    let fill = if active {
        SURFACE_HIGH
    } else {
        SURFACE.gamma_multiply(hover)
    };
    painter.rect_filled(rect, CornerRadius::same(R1), fill);
    if active {
        painter.rect_filled(
            Rect::from_min_size(
                rect.min + Vec2::new(0.0, 6.0),
                Vec2::new(2.0, rect.height() - 12.0),
            ),
            CornerRadius::same(1),
            EMBER,
        );
    }
    if resp.has_focus() {
        painter.rect_stroke(
            rect.expand(1.0),
            CornerRadius::same(R1),
            focus_stroke(),
            egui::StrokeKind::Outside,
        );
    }
    let fg = if active || resp.hovered() {
        TEXT
    } else {
        TEXT_DIM
    };
    painter.text(
        egui::pos2(rect.min.x + 12.0, rect.center().y),
        Align2::LEFT_CENTER,
        icon,
        FontId::proportional(15.0),
        fg,
    );
    painter.text(
        egui::pos2(rect.min.x + 36.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        label_font,
        fg,
    );
    resp.clicked()
}

fn bundled_docs_path(current_exe: &std::path::Path) -> Option<std::path::PathBuf> {
    let macos = current_exe.parent()?;
    let contents = macos.parent()?;
    if macos.file_name() == Some(std::ffi::OsStr::new("MacOS"))
        && contents.file_name() == Some(std::ffi::OsStr::new("Contents"))
    {
        Some(contents.join("Resources").join("docs").join("PRD.md"))
    } else {
        None
    }
}

fn resolve_docs_path_at(
    current_exe: &std::path::Path,
    current_dir: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, String> {
    if let Some(path) = bundled_docs_path(current_exe) {
        return if path.is_file() {
            Ok(path)
        } else {
            Err("packaged documentation is missing from Contents/Resources/docs/PRD.md".into())
        };
    }

    let current_dir = current_dir.ok_or_else(|| {
        "the current directory is unavailable for development documentation discovery".to_owned()
    })?;
    for root in current_dir.ancestors().take(6) {
        let candidate = root.join("PRD.md");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Some(parent) = current_exe.parent() {
        for root in parent.ancestors().take(8) {
            let candidate = root.join("PRD.md");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(
        "development documentation was not found in current-directory or executable ancestors"
            .into(),
    )
}

fn resolve_docs_path() -> Result<std::path::PathBuf, String> {
    let current_exe = std::env::current_exe()
        .map_err(|error| format!("the application executable path is unavailable: {error}"))?;
    let current_dir = if bundled_docs_path(&current_exe).is_some() {
        None
    } else {
        Some(
            std::env::current_dir()
                .map_err(|error| format!("the current directory is unavailable: {error}"))?,
        )
    };
    resolve_docs_path_at(&current_exe, current_dir.as_deref())
}

fn open_path(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("xdg-open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(path)
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err("opening local documentation is unsupported on this platform".into());
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    result
        .map(|_| ())
        .map_err(|error| format!("failed to launch the local document opener: {error}"))
}

/// Centered icon+label button. `border` gives a ghost style. Hover and press
/// animate per §3.7 (ease-out fills, 1px press inset) and keyboard focus
/// draws the ember focus ring.
fn action_button(
    ui: &mut egui::Ui,
    icon: Option<&str>,
    label: &str,
    fill: Color32,
    fg: Color32,
    border: Option<Color32>,
    height: f32,
    width: f32,
) -> bool {
    action_button_gated(ui, icon, label, fill, fg, border, height, width, None)
}

/// [`action_button`] with disabled-with-reason support: when `disabled_reason`
/// is set, the button renders quiet/tonal, never fires, and explains itself
/// on hover (UX-AC-01 — a control is never dead without a reason).
#[allow(clippy::too_many_arguments)]
fn action_button_gated(
    ui: &mut egui::Ui,
    icon: Option<&str>,
    label: &str,
    fill: Color32,
    fg: Color32,
    border: Option<Color32>,
    height: f32,
    width: f32,
    disabled_reason: Option<&str>,
) -> bool {
    let disabled = disabled_reason.is_some();
    let (fill, fg, border) = if disabled {
        (SURFACE_HIGH, TEXT_MUTE, Some(HAIRLINE))
    } else {
        (fill, fg, border)
    };
    let sense = if disabled {
        Sense::hover()
    } else {
        Sense::click()
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), sense);
    if let Some(reason) = disabled_reason {
        let resp = resp.clone().on_hover_text(reason);
        let press = 0.0;
        let painter = ui.painter();
        let draw_rect = rect.shrink(press);
        painter.rect_filled(draw_rect, CornerRadius::same(R1), fill);
        if let Some(b) = border {
            painter.rect_stroke(
                draw_rect,
                CornerRadius::same(R1),
                Stroke::new(1.0, b),
                egui::StrokeKind::Inside,
            );
        }
        let font = body_strong().resolve(ui.style());
        let icon_w = if icon.is_some() { 22.0 } else { 0.0 };
        let galley = elided_singleline(painter, label, font, fg, draw_rect.width() - icon_w - 20.0);
        let start = draw_rect.center().x - (icon_w + galley.size().x) / 2.0;
        if let Some(glyph) = icon {
            painter.text(
                egui::pos2(start, draw_rect.center().y),
                Align2::LEFT_CENTER,
                glyph,
                FontId::proportional(14.0),
                fg,
            );
        }
        let gpos = egui::pos2(start + icon_w, draw_rect.center().y - galley.size().y / 2.0);
        painter.galley(gpos, galley, fg);
        let _ = resp;
        return false;
    }
    let hover = motion_t(ui.ctx(), resp.id.with("hover"), resp.hovered(), 0.12);
    let press = motion_t(
        ui.ctx(),
        resp.id.with("press"),
        resp.is_pointer_button_down_on(),
        0.08,
    );
    let bg = fill.lerp_to_gamma(fill.gamma_multiply(1.14), hover);
    let font = body_strong().resolve(ui.style());
    let draw_rect = rect.shrink(press);
    let painter = ui.painter();
    painter.rect_filled(draw_rect, CornerRadius::same(R1), bg);
    if let Some(b) = border {
        painter.rect_stroke(
            draw_rect,
            CornerRadius::same(R1),
            Stroke::new(1.0, b),
            egui::StrokeKind::Inside,
        );
    }
    if resp.has_focus() {
        painter.rect_stroke(
            rect.expand(1.0),
            CornerRadius::same(R1),
            focus_stroke(),
            egui::StrokeKind::Outside,
        );
    }
    let icon_w = if icon.is_some() { 22.0 } else { 0.0 };
    let galley = elided_singleline(painter, label, font, fg, draw_rect.width() - icon_w - 20.0);
    let start = draw_rect.center().x - (icon_w + galley.size().x) / 2.0;
    if let Some(glyph) = icon {
        painter.text(
            egui::pos2(start, draw_rect.center().y),
            Align2::LEFT_CENTER,
            glyph,
            FontId::proportional(14.0),
            fg,
        );
    }
    let gpos = egui::pos2(start + icon_w, draw_rect.center().y - galley.size().y / 2.0);
    painter.galley(gpos, galley, fg);
    resp.clicked()
}

/// Single-line galley that elides with `…` instead of spilling outside its
/// button rect (QA X3: `layout_no_wrap` let long labels overflow).
fn elided_singleline(
    painter: &egui::Painter,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap.max_width = max_width.max(0.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    painter.layout_job(job)
}

fn internal_flow_reference_card(ui: &mut egui::Ui) {
    let contract = engineering::InternalFlowContract::default();
    let exact = contract.exact_contract();
    card(ui, |ui| {
        ui.label(
            RichText::new("Internal / HVAC · reference contract")
                .text_style(title())
                .color(TEXT),
        );
        ui.label(
            RichText::new(&contract.intent)
                .text_style(caption())
                .color(TEXT_MUTE),
        );
        ui.add_space(8.0);
        diag(
            ui,
            "Contract",
            exact["contract_kind"]
                .as_str()
                .unwrap_or(engineering::INTERNAL_FLOW_CONTRACT),
            TEXT_DIM,
        );
        ui.label(caps("Required future assignments"));
        ui.label(
            RichText::new(contract.required_assignments.join(" · "))
                .text_style(caption())
                .color(TEXT_DIM),
        );
        ui.add_space(8.0);
        for blocker in contract.execution_blockers() {
            ui.label(
                RichText::new(format!("EXECUTION BLOCKED · {blocker}"))
                    .text_style(mono_s())
                    .color(WARN),
            );
        }
        ui.add_space(8.0);
        // CS8: a plain status line, not a button-shaped non-action.
        ui.label(
            RichText::new("No compatible solver/model is available for this contract.")
                .text_style(caption())
                .color(TEXT_MUTE),
        );
    });
}

fn geometry_import_steps(
    format: cad::GeometryFormat,
    imported: &cad::GeometryImport,
    analyzed_mesh_sha256: &str,
    diagnostics: &cad::MeshDiagnostics,
    vm: &cad::VoxelMask,
) -> Vec<engineering::GeometryImportStep> {
    let extents = Some(diagnostics.extents.map(f64::from));
    let volume = Some(diagnostics.signed_volume);
    let mesh = Some(analyzed_mesh_sha256.to_owned());
    let mut steps = Vec::new();
    steps.push(engineering::GeometryImportStep {
        evidence_class: "translate".into(),
        operation: match format {
            cad::GeometryFormat::Stl => "parse_stl",
            cad::GeometryFormat::Step => "parse_step_brep",
            cad::GeometryFormat::ThreeMf => "parse_3mf",
        }
        .into(),
        parameters: vec![
            (
                "translator".into(),
                format!("{} {}", imported.translator, imported.translator_version),
            ),
            ("source_shells".into(), imported.source_shells.to_string()),
        ],
        input_mesh_sha256: None,
        output_mesh_sha256: if matches!(format, cad::GeometryFormat::Step) {
            None
        } else {
            mesh.clone()
        },
        source_signed_volume: if matches!(format, cad::GeometryFormat::Step) {
            None
        } else {
            volume
        },
        source_extents: if matches!(format, cad::GeometryFormat::Step) {
            None
        } else {
            extents
        },
    });
    if matches!(format, cad::GeometryFormat::Step) {
        steps.push(engineering::GeometryImportStep {
            evidence_class: "tessellate".into(),
            operation: "truck_meshalgo_tessellate".into(),
            parameters: vec![
                (
                    "relative_chord_tolerance".into(),
                    format!("{:.8}", crate::cad_step::RELATIVE_CHORD_TOLERANCE),
                ),
                (
                    "absolute_chord_tolerance_source_units".into(),
                    imported
                        .tessellation_tolerance_source_units
                        .map(|value| format!("{value:.8}"))
                        .unwrap_or_else(|| "missing".into()),
                ),
            ],
            input_mesh_sha256: None,
            output_mesh_sha256: None,
            source_signed_volume: None,
            source_extents: None,
        });
        steps.push(engineering::GeometryImportStep {
            evidence_class: "weld".into(),
            operation: "face_boundary_vertex_weld".into(),
            parameters: vec![(
                "relative_tolerance".into(),
                imported
                    .vertex_weld_relative_tolerance
                    .map(|value| format!("{value:.8}"))
                    .unwrap_or_else(|| "missing".into()),
            )],
            input_mesh_sha256: None,
            output_mesh_sha256: mesh.clone(),
            source_signed_volume: volume,
            source_extents: extents,
        });
    }
    steps.push(engineering::GeometryImportStep {
        evidence_class: "diagnose".into(),
        operation: "topology_diagnostics".into(),
        parameters: vec![
            ("triangles".into(), diagnostics.triangles.to_string()),
            ("components".into(), diagnostics.components.to_string()),
            (
                "boundary_edges".into(),
                diagnostics.boundary_edges.to_string(),
            ),
            (
                "non_manifold_edges".into(),
                diagnostics.non_manifold_edges.to_string(),
            ),
            (
                "degenerate_triangles".into(),
                diagnostics.degenerate_triangles.to_string(),
            ),
        ],
        input_mesh_sha256: mesh.clone(),
        output_mesh_sha256: mesh.clone(),
        source_signed_volume: volume,
        source_extents: extents,
    });
    steps.push(engineering::GeometryImportStep {
        evidence_class: "voxelize".into(),
        operation: "three_axis_majority_occupancy".into(),
        parameters: vec![
            ("target_grid".into(), vm.n.to_string()),
            ("solid_voxels".into(), vm.solid_voxels.to_string()),
            (
                "classification_version".into(),
                vm.classification_version.to_string(),
            ),
            (
                "axis_disagreement_fraction".into(),
                format!("{:.8}", vm.axis_disagreement_fraction),
            ),
        ],
        input_mesh_sha256: mesh,
        output_mesh_sha256: None,
        source_signed_volume: volume,
        source_extents: extents,
    });
    steps
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) {
    let response = Frame::NONE
        .fill(SURFACE)
        .corner_radius(CornerRadius::same(R2))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    // §3.4 level 1: raised by tone, not outline — no outer border; a 1px
    // inner top-edge light (white @ 4%) reads as machined elevation.
    let rect = response.response.rect;
    let y = rect.top() + 0.5;
    ui.painter().line_segment(
        [
            egui::pos2(rect.left() + R2 as f32, y),
            egui::pos2(rect.right() - R2 as f32, y),
        ],
        Stroke::new(1.0, Color32::from_white_alpha(10)),
    );
}

/// The clipboard results summary: a tab-separated table (label, value, unit,
/// source class) carrying named coefficients, physical loads in the display
/// system, reference values, and the provenance identifiers. Requires a
/// completed result; the caller only offers the button when one exists.
fn results_summary_tsv(
    case: &engineering::ExternalFlowCase,
    run_id: Option<&str>,
    system: units::UnitSystem,
    format: units::ValueFormat,
) -> String {
    use units::Quantity;
    let mut out = String::from("label\tvalue\tunit\tsource\n");
    let mut row = |label: &str, value: String, unit: &str, source: &str| {
        out.push_str(&format!("{label}\t{value}\t{unit}\t{source}\n"));
    };
    let fmt = |value: f64| units::format_value(value, format);
    row("case", case.name.clone(), "–", "PROVENANCE");
    row(
        "run",
        run_id.unwrap_or("draft (no immutable run)").into(),
        "–",
        "PROVENANCE",
    );
    row("model", case.model_id.clone(), "–", "PROVENANCE");
    row(
        "source_sha256",
        case.preflight.source_sha256.clone(),
        "–",
        "PROVENANCE",
    );
    row(
        "body_orientation",
        case.preflight.body_orientation_summary(),
        "deg",
        "PROVENANCE",
    );
    row(
        "coefficient_frame",
        engineering::COEFFICIENT_REFERENCE_FRAME.into(),
        "–",
        "PROVENANCE",
    );
    let Some(result) = case.result.as_ref() else {
        return out;
    };
    row(
        "Cd (+X drag)",
        fmt(result.force_coefficients[0]),
        "1",
        "DERIVED",
    );
    row(
        "Cs (+Y side)",
        fmt(result.force_coefficients[1]),
        "1",
        "DERIVED",
    );
    row(
        "Cl (+Z vertical)",
        fmt(result.force_coefficients[2]),
        "1",
        "DERIVED",
    );
    let axes = ["x", "y", "z"];
    for (axis, label) in axes.iter().enumerate() {
        let (value, unit) =
            units::display_value(Quantity::Force, result.force_newtons[axis], system);
        row(&format!("F{label}"), fmt(value), unit, "DERIVED");
    }
    for (axis, label) in axes.iter().enumerate() {
        row(
            &format!("Cm{label}"),
            fmt(result.moment_coefficients[axis]),
            "1",
            "DERIVED",
        );
    }
    for (axis, label) in axes.iter().enumerate() {
        let (value, unit) =
            units::display_value(Quantity::Moment, result.moment_newton_meters[axis], system);
        row(&format!("M{label}"), fmt(value), unit, "DERIVED");
    }
    row("Cp_min", fmt(result.cp_min), "1", "DERIVED");
    row("Cp_max", fmt(result.cp_max), "1", "DERIVED");
    let (area, area_unit) = units::display_value(Quantity::Area, result.surface_area_m2, system);
    row("diffuse_surface_area", fmt(area), area_unit, "DERIVED");
    let operating = &case.operating;
    let (speed, speed_unit) = units::display_value(Quantity::Velocity, operating.velocity, system);
    row("V_inf", fmt(speed), speed_unit, "REFERENCE");
    let (density, density_unit) =
        units::display_value(Quantity::Density, operating.density, system);
    row("rho_inf", fmt(density), density_unit, "REFERENCE");
    if let Some(q) = operating.dynamic_pressure() {
        let (value, unit) = units::display_value(Quantity::Pressure, q, system);
        row("q_inf", fmt(value), unit, "REFERENCE");
    }
    row(
        "L_ref",
        fmt(operating.reference_length),
        operating.length_unit.symbol(),
        "REFERENCE",
    );
    if let Some(reynolds) = operating.reynolds() {
        row("Re", fmt(reynolds), "1", "REFERENCE");
    }
    out
}

/// Own the compositor image on a short-lived worker and wake egui only when the
/// PNG has finished (or failed).
fn spawn_screenshot_write(
    result_tx: std::sync::mpsc::Sender<ScreenshotWriteResult>,
    repaint_context: egui::Context,
    image: std::sync::Arc<egui::ColorImage>,
    crop: Option<(Rect, f32)>,
    path: std::path::PathBuf,
    kind: ScreenshotWriteKind,
    provenance: ViewportShotProvenance,
) {
    std::thread::Builder::new()
        .name("reyn-screenshot-write".into())
        .spawn(move || {
            let result = match crop {
                Some((rect, pixels_per_point)) => {
                    let cropped = image.region(&rect, Some(pixels_per_point));
                    color_image_png_bytes_with_footer(&cropped, 0, &provenance.footer_lines)
                }
                None => {
                    color_image_png_bytes_with_footer(image.as_ref(), 0, &provenance.footer_lines)
                }
            }
            .and_then(|bytes| std::fs::write(&path, bytes).map_err(|error| error.to_string()));
            let _ = result_tx.send(ScreenshotWriteResult { kind, path, result });
            repaint_context.request_repaint();
        })
        .expect("screenshot worker thread should start");
}

fn color_image_png_bytes_with_footer(
    image: &egui::ColorImage,
    min_edge: usize,
    footer_lines: &[String],
) -> Result<Vec<u8>, String> {
    if footer_lines.is_empty() {
        return color_image_png_bytes(image, min_edge);
    }
    let [width, height] = image.size;
    if width == 0 || height == 0 {
        return Err("the image is empty".into());
    }
    let factor = if min_edge == 0 {
        1
    } else {
        min_edge.div_ceil(width.max(height)).max(1)
    };
    let (out_width, body_height) = (width * factor, height * factor);
    let line_height = 18usize;
    let footer_height = 12 + footer_lines.len() * line_height + 10;
    let out_height = body_height + footer_height;
    let mut rgba = Vec::with_capacity(out_width * out_height * 4);
    for row in 0..body_height {
        for column in 0..out_width {
            let pixel = image.pixels[(row / factor) * width + (column / factor)];
            rgba.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
        }
    }
    // Warm-dark instrument footer: near-black strip with light text baked as
    // solid pixels (no font rasterizer dependency in the screenshot path).
    for row in 0..footer_height {
        for _column in 0..out_width {
            let edge = row < 1;
            if edge {
                rgba.extend_from_slice(&[90, 72, 60, 255]);
            } else {
                rgba.extend_from_slice(&[28, 22, 18, 255]);
            }
        }
    }
    // Stamp a readable monochrome bitmap for each footer line using a 5x7
    // pattern for ASCII — enough for provenance hashes and labels.
    for (line_index, line) in footer_lines.iter().enumerate() {
        let y0 = body_height + 8 + line_index * line_height;
        stamp_footer_text(&mut rgba, out_width, out_height, 10, y0, line);
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, out_width as u32, out_height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
        writer
            .write_image_data(&rgba)
            .map_err(|error| error.to_string())?;
    }
    Ok(bytes)
}

fn stamp_footer_text(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    x0: usize,
    y0: usize,
    text: &str,
) {
    // Compact 5×7 glyphs for the provenance digits/letters we actually emit.
    fn glyph(ch: char) -> Option<[u8; 7]> {
        Some(match ch {
            '0' => [
                0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
            ],
            '1' => [
                0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
            ],
            '2' => [
                0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
            ],
            '3' => [
                0b01110, 0b10001, 0b00001, 0b00110, 0b00001, 0b10001, 0b01110,
            ],
            '4' => [
                0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
            ],
            '5' => [
                0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
            ],
            '6' => [
                0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
            ],
            '7' => [
                0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
            ],
            '8' => [
                0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
            ],
            '9' => [
                0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
            ],
            'a'..='z' => return glyph(ch.to_ascii_uppercase()),
            'A' => [
                0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
            ],
            'B' => [
                0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
            ],
            'C' => [
                0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
            ],
            'D' => [
                0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
            ],
            'E' => [
                0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
            ],
            'F' => [
                0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
            ],
            'G' => [
                0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
            ],
            'H' => [
                0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
            ],
            'I' => [
                0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
            ],
            'J' => [
                0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100,
            ],
            'K' => [
                0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
            ],
            'L' => [
                0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
            ],
            'M' => [
                0b10001, 0b11011, 0b10101, 0b10001, 0b10001, 0b10001, 0b10001,
            ],
            'N' => [
                0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
            ],
            'O' => [
                0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
            ],
            'P' => [
                0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
            ],
            'Q' => [
                0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
            ],
            'R' => [
                0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
            ],
            'S' => [
                0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
            ],
            'T' => [
                0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
            ],
            'U' => [
                0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
            ],
            'V' => [
                0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
            ],
            'W' => [
                0b10001, 0b10001, 0b10001, 0b10001, 0b10101, 0b11011, 0b10001,
            ],
            'X' => [
                0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
            ],
            'Y' => [
                0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
            ],
            'Z' => [
                0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
            ],
            ' ' => [0; 7],
            '.' => [0, 0, 0, 0, 0, 0b00100, 0b00100],
            '-' => [0, 0, 0, 0b01110, 0, 0, 0],
            ':' => [0, 0b00100, 0, 0, 0b00100, 0, 0],
            '/' => [0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0, 0],
            '[' => [
                0b01110, 0b01000, 0b01000, 0b01000, 0b01000, 0b01000, 0b01110,
            ],
            ']' => [
                0b01110, 0b00010, 0b00010, 0b00010, 0b00010, 0b00010, 0b01110,
            ],
            '·' | '•' => [0, 0, 0b00100, 0, 0, 0, 0],
            _ => return None,
        })
    }
    let mut x = x0;
    for ch in text.chars().take(96) {
        let Some(rows) = glyph(ch) else {
            x += 6;
            continue;
        };
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..5 {
                if bits & (1 << (4 - col)) != 0 {
                    let px = x + col;
                    let py = y0 + row;
                    if px < width && py < height {
                        let index = (py * width + px) * 4;
                        rgba[index..index + 4].copy_from_slice(&[230, 214, 200, 255]);
                    }
                }
            }
        }
        x += 6;
    }
}

/// Human labels for CAD engine stage ids (`preparing` / `developing` / …).
fn cad_progress_stage_label(stage: &str) -> &'static str {
    match stage {
        "preparing" => "preparing mask",
        "developing" => "developing flow",
        "predicting" => "model prediction",
        "recovering" => "pressure & loads",
        _ => "engine stage",
    }
}

/// Display labels for force-coefficient scalar keys used in variant plots.
fn force_coefficient_plot_label(key: &str) -> Option<&'static str> {
    match key {
        "force_coefficient_x" => Some("Cd"),
        "force_coefficient_y" => Some("Cs"),
        "force_coefficient_z" => Some("Cl"),
        _ => None,
    }
}

/// Parent/current force-coefficient pairs for the variant comparison plot.
fn force_variant_series(rows: &[(String, f64, f64, String)]) -> Vec<(&'static str, f64, f64)> {
    let mut series = Vec::with_capacity(3);
    for key in [
        "force_coefficient_x",
        "force_coefficient_y",
        "force_coefficient_z",
    ] {
        let Some(label) = force_coefficient_plot_label(key) else {
            continue;
        };
        if let Some((_, parent, current, _)) = rows.iter().find(|(row_key, _, _, _)| row_key == key)
        {
            series.push((label, *parent, *current));
        }
    }
    series
}

/// Grouped bars: gold = parent, ember = current, for Cd / Cs / Cl.
fn draw_force_variant_plot(ui: &mut egui::Ui, series: &[(&'static str, f64, f64)]) {
    if series.is_empty() {
        return;
    }
    let height = 148.0;
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(180.0), height),
        Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(3), SURFACE);
    painter.rect_stroke(
        rect,
        CornerRadius::same(3),
        Stroke::new(1.0, OUTLINE_VARIANT),
        egui::StrokeKind::Inside,
    );
    let chart = Rect::from_min_max(
        rect.min + Vec2::new(36.0, 18.0),
        rect.max - Vec2::new(12.0, 28.0),
    );
    let mut lo = series
        .iter()
        .flat_map(|(_, parent, current)| [*parent, *current])
        .fold(f64::INFINITY, f64::min);
    let mut hi = series
        .iter()
        .flat_map(|(_, parent, current)| [*parent, *current])
        .fold(f64::NEG_INFINITY, f64::max);
    if !lo.is_finite() || !hi.is_finite() {
        return;
    }
    // Keep zero in-frame when signs straddle, and pad tiny ranges.
    if lo > 0.0 {
        lo = 0.0;
    }
    if hi < 0.0 {
        hi = 0.0;
    }
    if (hi - lo).abs() < 1e-9 {
        lo -= 0.1;
        hi += 0.1;
    }
    let zero_y = chart.max.y - ((0.0 - lo) / (hi - lo)) as f32 * chart.height();
    painter.line_segment(
        [
            egui::pos2(chart.min.x, zero_y),
            egui::pos2(chart.max.x, zero_y),
        ],
        Stroke::new(1.0, OUTLINE_VARIANT),
    );
    painter.text(
        egui::pos2(chart.min.x, chart.min.y),
        Align2::LEFT_BOTTOM,
        format!("{hi:.3}"),
        FontId::monospace(9.0),
        TEXT_DIM,
    );
    painter.text(
        egui::pos2(chart.min.x, chart.max.y),
        Align2::LEFT_TOP,
        format!("{lo:.3}"),
        FontId::monospace(9.0),
        TEXT_DIM,
    );
    let group_width = chart.width() / series.len().max(1) as f32;
    let bar_width = (group_width * 0.28).clamp(8.0, 22.0);
    let gap = 3.0;
    for (index, (label, parent, current)) in series.iter().enumerate() {
        let center_x = chart.min.x + (index as f32 + 0.5) * group_width;
        let map_y = |value: f64| chart.max.y - ((value - lo) / (hi - lo)) as f32 * chart.height();
        let parent_top = map_y(*parent);
        let current_top = map_y(*current);
        let parent_rect = Rect::from_min_max(
            egui::pos2(center_x - bar_width - gap * 0.5, parent_top.min(zero_y)),
            egui::pos2(center_x - gap * 0.5, parent_top.max(zero_y)),
        );
        let current_rect = Rect::from_min_max(
            egui::pos2(center_x + gap * 0.5, current_top.min(zero_y)),
            egui::pos2(center_x + bar_width + gap * 0.5, current_top.max(zero_y)),
        );
        painter.rect_filled(parent_rect, CornerRadius::same(1), GOLD);
        painter.rect_filled(current_rect, CornerRadius::same(1), BRAND);
        painter.text(
            egui::pos2(center_x, chart.max.y + 4.0),
            Align2::CENTER_TOP,
            format!("{label} Δ{:+.3}", current - parent),
            FontId::monospace(9.0),
            TEXT_MUTE,
        );
    }
}

/// Mid-line Cp (or active quantity) profile under the section image.
fn draw_section_cp_profile(
    painter: &egui::Painter,
    viewport: Rect,
    panel: Rect,
    section: &engineering_section::SectionPlane,
) {
    let plot = Rect::from_min_max(
        egui::pos2(viewport.min.x + 46.0, panel.max.y + 28.0),
        egui::pos2(viewport.max.x - 46.0, viewport.max.y - 24.0),
    );
    if plot.height() < 48.0 || plot.width() < 160.0 {
        return;
    }
    painter.rect_filled(plot, CornerRadius::same(3), SURFACE);
    painter.rect_stroke(
        plot,
        CornerRadius::same(3),
        Stroke::new(1.0, OUTLINE_VARIANT),
        egui::StrokeKind::Inside,
    );
    painter.text(
        plot.min + Vec2::new(10.0, 6.0),
        Align2::LEFT_TOP,
        format!(
            "{} mid-line · {}",
            section.quantity.label(),
            section.quantity.source()
        ),
        FontId::monospace(10.0),
        GOLD,
    );
    let n = section.n.max(2);
    let mid = n / 2;
    let mut series = Vec::with_capacity(n);
    for i in 0..n {
        let index = mid * n + i;
        if section.mask.get(index).copied().unwrap_or(0.0) > 0.5 {
            series.push((i as f32 / (n - 1) as f32, section.values[index]));
        }
    }
    if series.len() < 2 {
        painter.text(
            plot.center(),
            Align2::CENTER_CENTER,
            "No fluid samples on the mid-line",
            FontId::proportional(11.0),
            TEXT_MUTE,
        );
        return;
    }
    let mut lo = series.iter().map(|(_, v)| *v).fold(f32::INFINITY, f32::min);
    let mut hi = series
        .iter()
        .map(|(_, v)| *v)
        .fold(f32::NEG_INFINITY, f32::max);
    if !lo.is_finite() || !hi.is_finite() || (hi - lo).abs() < 1e-6 {
        lo -= 1.0;
        hi += 1.0;
    }
    let chart = Rect::from_min_max(
        plot.min + Vec2::new(36.0, 24.0),
        plot.max - Vec2::new(12.0, 14.0),
    );
    let mut points = Vec::with_capacity(series.len());
    for (t, value) in &series {
        let x = chart.min.x + t * chart.width();
        let y = chart.max.y - ((*value - lo) / (hi - lo)) * chart.height();
        points.push(egui::pos2(x, y));
    }
    for window in points.windows(2) {
        painter.line_segment([window[0], window[1]], Stroke::new(1.5, BRAND));
    }
    painter.text(
        egui::pos2(chart.min.x, chart.min.y),
        Align2::LEFT_BOTTOM,
        format!("{hi:.2}"),
        FontId::monospace(9.0),
        TEXT_DIM,
    );
    painter.text(
        egui::pos2(chart.min.x, chart.max.y),
        Align2::LEFT_TOP,
        format!("{lo:.2}"),
        FontId::monospace(9.0),
        TEXT_DIM,
    );
}

/// Encode an egui `ColorImage` as RGBA PNG bytes, nearest-neighbor upscaling
/// small images until the long edge reaches `min_edge` (0 = never upscale) so
/// exported grid sections stay inspectable at native data resolution.
fn color_image_png_bytes(image: &egui::ColorImage, min_edge: usize) -> Result<Vec<u8>, String> {
    let [width, height] = image.size;
    if width == 0 || height == 0 {
        return Err("the image is empty".into());
    }
    let factor = if min_edge == 0 {
        1
    } else {
        min_edge.div_ceil(width.max(height)).max(1)
    };
    let (out_width, out_height) = (width * factor, height * factor);
    let mut rgba = Vec::with_capacity(out_width * out_height * 4);
    for row in 0..out_height {
        for column in 0..out_width {
            let pixel = image.pixels[(row / factor) * width + (column / factor)];
            rgba.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
        }
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, out_width as u32, out_height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
        writer
            .write_image_data(&rgba)
            .map_err(|error| error.to_string())?;
    }
    Ok(bytes)
}

/// Unit-aware SI-backed numeric input: a DragValue in the chosen display unit
/// plus a unit selector. Storage stays SI; switching the unit only changes
/// presentation. Returns the numeric response so case history can coalesce an
/// active DragValue interaction into one undo transaction.
fn unit_value_input<U: units::InputUnit>(
    ui: &mut egui::Ui,
    id: &str,
    si_value: &mut f64,
    unit: &mut U,
    si_speed: f64,
    si_range: std::ops::RangeInclusive<f64>,
) -> egui::Response {
    ui.horizontal(|ui| {
        let active = *unit;
        let mut display = active.unit_from_si(*si_value);
        let response = ui.add(
            egui::DragValue::new(&mut display)
                .speed(si_speed * active.unit_from_si(1.0))
                .range(
                    active.unit_from_si(*si_range.start())..=active.unit_from_si(*si_range.end()),
                ),
        );
        if response.changed() {
            *si_value = active
                .unit_to_si(display)
                .clamp(*si_range.start(), *si_range.end());
        }
        egui::ComboBox::from_id_salt(id)
            .selected_text(active.unit_symbol())
            .width(84.0)
            .show_ui(ui, |ui| {
                for &candidate in U::all() {
                    ui.selectable_value(unit, candidate, candidate.unit_symbol());
                }
            });
        // Honest storage: whenever the entry unit is not SI, the exact stored
        // SI value stays visible beside the field.
        if (active.unit_to_si(1.0) - 1.0).abs() > 1e-12 {
            ui.label(
                RichText::new(format!("= {si_value:.6} SI"))
                    .text_style(mono_s())
                    .color(TEXT_MUTE),
            );
        }
        response
    })
    .inner
}

fn track_case_edit_response(
    response: &egui::Response,
    transaction: CaseEditTransaction,
    changed: &mut bool,
    changed_transaction: &mut Option<CaseEditTransaction>,
    active_transaction: &mut Option<CaseEditTransaction>,
) {
    if response.has_focus() || response.dragged() {
        *active_transaction = Some(transaction);
    }
    if response.changed() {
        *changed = true;
        *changed_transaction = Some(transaction);
    }
}

fn diag(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(TEXT_DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // Overflow guard: the value elides into the remaining width
            // instead of overprinting the label; full value on hover.
            ui.add(egui::Label::new(mono(value, color)).truncate())
                .on_hover_text(RichText::new(value).monospace());
        });
    });
    ui.add_space(7.0);
}

/// §4.4 measurement-table row: label in body text, value in mono
/// right-aligned, a shared unit column, and a source-class chip
/// (`MODEL` / `RECOVERED` / `DERIVED`, N5X-EV-02) on every row. Returns the row
/// response so callers can attach method notes on hover.
fn measure_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    unit: &str,
    source: &str,
    source_color: Color32,
) -> egui::Response {
    let response = if measurement_row_stacks(ui.available_width()) {
        ui.vertical(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // Keep the source class visible; the descriptive label yields
                // and elides into whatever width remains on the left.
                ui.label(
                    RichText::new(source)
                        .text_style(mono_chip())
                        .color(source_color),
                );
                ui.add(egui::Label::new(RichText::new(label).color(TEXT_DIM)).truncate())
                    .on_hover_text(label);
            });
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(unit).text_style(mono_s()).color(TEXT_MUTE));
                    ui.add(egui::Label::new(mono(value, TEXT)).truncate())
                        .on_hover_text(RichText::new(value).monospace());
                });
            });
        })
        .response
    } else {
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).color(TEXT_DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(source)
                        .text_style(mono_chip())
                        .color(source_color),
                );
                ui.add_space(4.0);
                // Fixed-width unit column keeps the value edges ragged-left,
                // aligned-right — tabular reading order.
                let unit_galley = ui.painter().layout_no_wrap(
                    unit.to_owned(),
                    mono_s().resolve(ui.style()),
                    TEXT_MUTE,
                );
                let (unit_rect, _) =
                    ui.allocate_exact_size(Vec2::new(28.0, unit_galley.size().y), Sense::hover());
                ui.painter().galley(
                    egui::pos2(
                        unit_rect.min.x,
                        unit_rect.center().y - unit_galley.size().y / 2.0,
                    ),
                    unit_galley,
                    TEXT_MUTE,
                );
                ui.add(egui::Label::new(mono(value, TEXT)).truncate())
                    .on_hover_text(RichText::new(value).monospace());
            });
        })
        .response
    };
    ui.add_space(7.0);
    response
}

/// §4.2 landing drop zone: a level-1 card with a dashed hairline that
/// brightens while files hover. Actual drop handling lives in
/// `project_view` (dropped_files), so this stays purely presentational.
fn drop_target(ui: &mut egui::Ui, width: f32) {
    let hovering_files = ui.input(|input| !input.raw.hovered_files.is_empty());
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 96.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(R2),
        if hovering_files {
            SURFACE_HIGH
        } else {
            SURFACE_LOW
        },
    );
    let border = if hovering_files { EMBER } else { OUTLINE };
    for [start, end] in [
        [rect.left_top(), rect.right_top()],
        [rect.right_top(), rect.right_bottom()],
        [rect.right_bottom(), rect.left_bottom()],
        [rect.left_bottom(), rect.left_top()],
    ] {
        painter.extend(egui::Shape::dashed_line(
            &[start, end],
            Stroke::new(1.0, border),
            5.0,
            4.0,
        ));
    }
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        "Drop an STL, STEP, or .reynproj file here",
        caption().resolve(ui.style()),
        TEXT_MUTE,
    );
}

/// §4.5 ledger row: level-0 (no box) — label left, value right in `mono-s`,
/// hairline underneath. Long values truncate to a 12-char prefix with the
/// full value on hover and a copy affordance; absent values state their
/// honest word (`unknown` / `not completed`) in WARN, never blank.
fn ledger_row(
    ui: &mut egui::Ui,
    label: &str,
    value: Option<&str>,
    accent: Color32,
    absent_word: &str,
) {
    ui.horizontal(|ui| {
        ui.set_min_height(28.0);
        ui.label(RichText::new(label).color(TEXT_DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| match value {
            Some(full) => {
                if ui
                    .add(
                        egui::Button::new(RichText::new(ph::COPY).size(12.0).color(TEXT_MUTE))
                            .frame(false),
                    )
                    .on_hover_text("Copy full value")
                    .clicked()
                {
                    ui.ctx().copy_text(full.to_owned());
                }
                ui.label(
                    RichText::new(short_hash(full))
                        .text_style(mono_s())
                        .color(accent),
                )
                .on_hover_text(full);
            }
            None => {
                ui.label(RichText::new(absent_word).text_style(mono_s()).color(WARN));
            }
        });
    });
    let y = ui.cursor().min.y;
    ui.painter()
        .hline(ui.max_rect().x_range(), y, Stroke::new(1.0, HAIRLINE));
    ui.add_space(4.0);
}

/// §4.5 run-ledger row: run ID + created (UTC) + lifecycle state as
/// glyph+word chip. The whole row is clickable and deep-links to the
/// immutable run's evidence. Returns true when clicked.
fn run_ledger_row(
    ui: &mut egui::Ui,
    run_id: &str,
    created_utc_unix: u64,
    state: project::LifecycleState,
    active: bool,
) -> bool {
    use project::LifecycleState as LS;
    let (state_word, state_color) = match state {
        LS::Succeeded => ("SUCCEEDED", OK),
        LS::Failed => ("FAILED", DANGER),
        LS::Cancelled => ("CANCELLED", WARN),
        LS::Stale => ("STALE", WARN),
        LS::Pending => ("PENDING", EMBER),
        LS::Running => ("RUNNING", EMBER),
        LS::EvidenceLocked => ("EVIDENCE-LOCKED", TEXT_DIM),
        LS::Ready => ("READY", TEXT_DIM),
        LS::Draft => ("DRAFT", TEXT_MUTE),
    };
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    let hover = motion_t(ui.ctx(), resp.id.with("hover"), resp.hovered(), 0.14);
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(R1), SURFACE.gamma_multiply(hover));
    if active {
        painter.rect_filled(
            Rect::from_min_size(
                rect.min + Vec2::new(0.0, 6.0),
                Vec2::new(2.0, rect.height() - 12.0),
            ),
            CornerRadius::same(1),
            EMBER,
        );
    }
    let mono_font = mono_s().resolve(ui.style());
    painter.text(
        egui::pos2(rect.min.x + 12.0, rect.center().y),
        Align2::LEFT_CENTER,
        short_id(run_id),
        mono_font.clone(),
        if active { TEXT } else { TEXT_DIM },
    );
    painter.text(
        egui::pos2(rect.min.x + 110.0, rect.center().y),
        Align2::LEFT_CENTER,
        format_utc(created_utc_unix),
        mono_font.clone(),
        TEXT_MUTE,
    );
    painter.text(
        egui::pos2(rect.max.x - 12.0, rect.center().y),
        Align2::RIGHT_CENTER,
        state_word,
        mono_font,
        state_color,
    );
    let resp = resp.on_hover_text(format!("{run_id} — open this run's evidence"));
    painter.hline(rect.x_range(), rect.max.y + 1.0, Stroke::new(1.0, HAIRLINE));
    resp.clicked()
}

/// Unix seconds → "YYYY-MM-DD HH:MM UTC" without a date dependency
/// (civil-from-days, Hinnant's algorithm).
pub(crate) fn format_utc(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    let (hh, mm) = (secs / 3600, (secs % 3600) / 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02} {hh:02}:{mm:02} UTC")
}

/// §4.4 inspector accordion group: an overline header row with a caret;
/// the body keeps the quiet control styling. State persists per id.
fn inspector_group(
    ui: &mut egui::Ui,
    id_salt: &str,
    title: &str,
    default_open: bool,
    body: impl FnOnce(&mut egui::Ui),
) {
    let id = ui.make_persistent_id(("inspector", id_salt));
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        id,
        default_open,
    );
    let open = state.is_open();
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::click());
    if resp.clicked() {
        state.toggle(ui);
    }
    let hover = motion_t(ui.ctx(), resp.id.with("hover"), resp.hovered(), 0.14);
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(R1), SURFACE.gamma_multiply(hover));
    let overline_galley = painter.layout_no_wrap(
        title.to_uppercase(),
        overline().resolve(ui.style()),
        if open { TEXT_DIM } else { TEXT_MUTE },
    );
    painter.galley(
        egui::pos2(
            rect.min.x + 4.0,
            rect.center().y - overline_galley.size().y / 2.0,
        ),
        overline_galley,
        TEXT_DIM,
    );
    painter.text(
        egui::pos2(rect.max.x - 8.0, rect.center().y),
        Align2::RIGHT_CENTER,
        if open { "▾" } else { "▸" },
        FontId::proportional(11.0),
        TEXT_MUTE,
    );
    state.show_body_unindented(ui, |ui| {
        ui.add_space(4.0);
        body(ui);
        ui.add_space(8.0);
    });
    ui.add_space(4.0);
}

/// One glyph+word status line (§ honesty: never color-only).
fn alert_line(ui: &mut egui::Ui, color: Color32, glyph: &str, message: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(RichText::new(glyph).text_style(mono_s()).color(color));
        ui.label(RichText::new(message).text_style(caption()).color(TEXT_DIM));
    });
    ui.add_space(4.0);
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let value = bytes as f64;
    if value >= GIB {
        format!("{:.2} GiB", value / GIB)
    } else if value >= MIB {
        format!("{:.1} MiB", value / MIB)
    } else {
        format!("{:.0} KiB", value / KIB)
    }
}

/// One stage in the Case Setup spine (§4.3): a 40px header row with a status
/// glyph, stage name, and one-line verdict summary; clicking anywhere on the
/// row toggles the body. The body hangs off a vertical connector line that
/// continues the glyph column, so the workflow reads top-to-bottom as one
/// spine rather than a stack of cards.
#[allow(clippy::too_many_arguments)]
fn spine_stage(
    ui: &mut egui::Ui,
    id_salt: &str,
    glyph: &str,
    glyph_color: Color32,
    title: &str,
    summary: &str,
    default_open: bool,
    body: impl FnOnce(&mut egui::Ui),
) {
    let id = ui.make_persistent_id(("case-spine", id_salt));
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        id,
        default_open,
    );
    let open = state.is_open();

    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::click());
    if resp.clicked() {
        state.toggle(ui);
    }
    let hover = motion_t(ui.ctx(), resp.id.with("hover"), resp.hovered(), 0.14);
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(R1), SURFACE.gamma_multiply(hover));
    if resp.has_focus() {
        painter.rect_stroke(
            rect.expand(1.0),
            CornerRadius::same(R1),
            focus_stroke(),
            egui::StrokeKind::Outside,
        );
    }
    let row_y = rect.center().y;
    painter.text(
        egui::pos2(rect.min.x + 9.0, row_y),
        Align2::CENTER_CENTER,
        glyph,
        FontId::proportional(13.0),
        glyph_color,
    );
    painter.text(
        egui::pos2(rect.min.x + 28.0, row_y),
        Align2::LEFT_CENTER,
        title,
        body_strong().resolve(ui.style()),
        if open || resp.hovered() {
            TEXT
        } else {
            TEXT_DIM
        },
    );
    // Caret + summary, right-aligned. The summary is the verdict at a glance;
    // the caret signals the row is expandable.
    let caret = if open { "▾" } else { "▸" };
    painter.text(
        egui::pos2(rect.max.x - 8.0, row_y),
        Align2::RIGHT_CENTER,
        caret,
        FontId::proportional(11.0),
        TEXT_MUTE,
    );
    let summary_font = mono_s().resolve(ui.style());
    let title_end = rect.min.x + 28.0 + 140.0;
    let summary_galley = elided_singleline(
        painter,
        summary,
        summary_font,
        TEXT_MUTE,
        rect.max.x - 24.0 - title_end,
    );
    let summary_x = rect.max.x - 24.0 - summary_galley.size().x;
    painter.galley(
        egui::pos2(summary_x, row_y - summary_galley.size().y / 2.0),
        summary_galley,
        TEXT_MUTE,
    );
    resp.clone().on_hover_text(summary);

    state.show_body_unindented(ui, |ui| {
        let left = ui.max_rect().min.x;
        let top = ui.cursor().min.y;
        let inner = Frame::NONE
            .inner_margin(Margin {
                left: 28,
                right: 0,
                top: 4,
                bottom: 12,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                body(ui);
            });
        // Connector line continues the glyph column through the open body.
        ui.painter().vline(
            left + 9.0,
            egui::Rangef::new(top, inner.response.rect.bottom() - 8.0),
            Stroke::new(1.0, HAIRLINE),
        );
    });
    ui.add_space(4.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_RUN_ID: &str = "61f596e7-8414-488e-b764-0a1dfe671d1a";
    const FIXTURE_MODEL_SHA256: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";

    #[test]
    fn calculation_export_embeds_provenance_and_propagates_write_failures() {
        let provenance = serde_json::json!({
            "project_id": "project-1",
            "case_id": "case-1",
            "run_id": "run-1",
            "source_revision_id": "source-1",
            "solver": {"name": "reyn-engine", "version": "0.1.1"},
        });
        let mut bytes = Vec::new();
        write_calculation_export(&mut bytes, &provenance, 8, 1.0, 2.0, 3.0, 0.2, 0.8).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("reyn_calculation_provenance_json"));
        assert!(text.contains("\"source_revision_id\":\"source-1\""));
        assert!(text.contains("derived_from_rendered_model_field"));

        struct FailingWriter;
        impl std::io::Write for FailingWriter {
            fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("simulated full disk"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(write_calculation_export(
            &mut FailingWriter,
            &provenance,
            8,
            1.0,
            2.0,
            3.0,
            0.2,
            0.8,
        )
        .unwrap_err()
        .contains("simulated full disk"));
    }

    #[test]
    fn calculation_export_keeps_run_model_after_current_model_switches() {
        let mut project_manifest = project::ProjectManifest::new("Model switch", 1);
        project_manifest
            .add_source_revision(
                project::SourceRevision {
                    source_revision_id: "source-a".into(),
                    source_kind: project::SourceKind::Geometry,
                    revision: 1,
                    imported_utc_unix: 2,
                    uri_hint: None,
                    byte_size: 4,
                    content_sha256: "a".repeat(64),
                    declared_units: Some("m".into()),
                    frame: Some("source_frame".into()),
                    transform_4x4: [
                        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0,
                        1.0,
                    ],
                    parent_revision_id: None,
                    warnings: Vec::new(),
                },
                2,
            )
            .unwrap();
        project_manifest
            .create_case(
                "case-a",
                "Case A",
                project::CaseRevision {
                    case_revision_id: "revision-a".into(),
                    parent_revision_id: None,
                    created_utc_unix: 3,
                    source_revision_ids: vec!["source-a".into()],
                    contract: serde_json::json!({"model_id": "model-a.reynmodel"}),
                    discretization: serde_json::json!({"grid": 64}),
                    outputs: serde_json::json!({"loads": true}),
                },
                3,
            )
            .unwrap();
        project_manifest
            .append_run(
                "case-a",
                project::RunRecord::new(
                    "run-a",
                    None,
                    "revision-a",
                    4,
                    5,
                    project::LifecycleState::Succeeded,
                    project::RunManifest {
                        schema_version: project::PROJECT_SCHEMA_VERSION,
                        app: project::VersionedComponent {
                            name: "Reyn Studio".into(),
                            version: "0.1.1".into(),
                            sha256: None,
                        },
                        engine: None,
                        model: Some(project::VersionedComponent {
                            name: "model-a.reynmodel".into(),
                            version: "bundle-v1".into(),
                            sha256: Some("b".repeat(64)),
                        }),
                        solver: None,
                        converter: None,
                        exact_contract: serde_json::json!({"model_id": "model-a.reynmodel"}),
                        exact_settings: serde_json::json!({"submitted_request_id": "request-a"}),
                        seeds: vec![7],
                        device: "cpu".into(),
                        runtime_ms: 1,
                        stop_reason: "completed".into(),
                        warnings: Vec::new(),
                        waivers: Vec::new(),
                        missing_dependencies: Vec::new(),
                        output_sha256: vec!["c".repeat(64)],
                        scalar_outputs: Vec::new(),
                        determinism: None,
                    },
                    Vec::new(),
                ),
                5,
            )
            .unwrap();

        let current_model_after_switch = "model-b.reynmodel";
        let provenance = calculation_export_provenance(&project_manifest, Some("run-a"));
        assert_eq!(current_model_after_switch, "model-b.reynmodel");
        assert_eq!(provenance["model_id"], "model-a.reynmodel");
        assert_eq!(provenance["model_sha256"], "b".repeat(64));
        assert_eq!(provenance["case_revision_id"], "revision-a");
        assert_eq!(provenance["sources"][0]["source_revision_id"], "source-a");
    }

    fn benchmark_fixture() -> engine::BenchResult {
        engine::BenchResult {
            model: "fixture.reynmodel".into(),
            seeds: vec![70000],
            horizons: vec![1, 4],
            rel: vec![vec![0.1, 0.2]],
            persist: vec![vec![0.4, 0.6]],
            global_rel: 0.15,
            runtime_s: 1.25,
            grid: 32,
            epoch: 20,
            dt_frame: 0.04,
            provenance: engine::BenchProvenance {
                verdict: "unknown".into(),
                training_seed: Some(0),
                mixed_fork_seed: Some(10000),
                mixed_fork_used: false,
                validation_seed: Some(50000),
                dataset: "standard".into(),
                benchmark_seeds: vec![engine::BenchSeedProvenance {
                    seed: 70000,
                    stream: "fresh_test".into(),
                    overlap: false,
                }],
                overlap_count: 0,
                overlap_pct: 0.0,
                epoch: Some(20),
                declared_epochs: Some(20),
                checkpoint_role: "legacy/unknown".into(),
                final_epoch_status: "final_epoch_role_unknown".into(),
                selection_metric: "val_multi_horizon_rel_l2".into(),
                selection_stream: "validation".into(),
                source_fingerprint_present: false,
                source_fingerprint_digest: None,
                legacy_unknown: vec![
                    "checkpoint role absent".into(),
                    "source fingerprint absent".into(),
                ],
                flags: Vec::new(),
            },
        }
    }

    fn inspector_fixture() -> engine::BenchInspector {
        let variables: Vec<String> = InspectorVariable::ALL
            .iter()
            .map(|variable| variable.key().into())
            .collect();
        let signed: Vec<bool> = InspectorVariable::ALL
            .iter()
            .map(|variable| variable.signed())
            .collect();
        let values: Vec<f32> = (0..48).map(|value| value as f32).collect();
        engine::BenchInspector {
            seed: 70000,
            horizon: 1,
            n: 2,
            maps: crate::benchmark_evidence::InspectorMaps::from_protocol(
                INSPECTOR_SCHEMA,
                2,
                &variables,
                &signed,
                &values,
            )
            .unwrap(),
            seed_stream: "fresh_test".into(),
            provenance_status: "unknown".into(),
            rel_l2: 0.1,
            persist_rel_l2: 0.4,
            improvement_ratio: 4.0,
            mean_abs_error: 1.0,
            p95_abs_error: 2.0,
            max_abs_error: 3.0,
            divergence_model_rms: 1e-3,
            divergence_truth_rms: 1e-4,
            divergence_error_rms: 1.1e-3,
            spectrum_rel_l2: 0.2,
            spectrum_k: vec![1.0],
            spectrum_model: vec![2.0],
            spectrum_truth: vec![1.8],
        }
    }

    #[test]
    fn evidence_exports_include_provenance_and_valid_canonical_hash() {
        let benchmark = benchmark_fixture();
        let csv = benchmark_csv(&benchmark);
        assert!(
            csv.starts_with("seed,provenance_stream,provenance_status,overlaps_reserved_stream")
        );
        assert!(csv.contains("70000,fresh_test,unknown,false"));

        let (report, expected_hash) =
            benchmark_report_card(&benchmark, None, FIXTURE_RUN_ID, FIXTURE_MODEL_SHA256, 42);
        let mut value: serde_json::Value = serde_json::from_str(&report).unwrap();
        assert_eq!(
            value["report_schema"],
            crate::benchmark_export::REPORT_CARD_SCHEMA
        );
        assert_eq!(value["run_id"], FIXTURE_RUN_ID);
        assert_eq!(value["model_checkpoint_sha256"], FIXTURE_MODEL_SHA256);
        assert_eq!(
            crate::benchmark_export::verify_canonical_report(&report).unwrap(),
            expected_hash
        );
        assert_eq!(value["provenance"]["validation_is_independent_test"], false);
        assert_eq!(
            value["provenance"]["benchmark_seeds"][0]["stream"],
            "fresh_test"
        );
        assert_eq!(value["integrity_algorithm"], "SHA-256");
        assert_eq!(value["authenticity"]["status"], "UNSIGNED");
        assert!(value["authenticity"]["signature"].is_null());
        let embedded = value
            .as_object_mut()
            .unwrap()
            .remove("integrity_sha256")
            .unwrap();
        assert_eq!(embedded.as_str(), Some(expected_hash.as_str()));
        use sha2::Digest;
        let actual = sha2::Sha256::digest(serde_json::to_vec(&value).unwrap());
        let actual_hex: String = actual.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(actual_hex, expected_hash);

        let mut clean = benchmark_fixture();
        clean.provenance.verdict = "clean".into();
        let (clean_report, _) =
            benchmark_report_card(&clean, None, FIXTURE_RUN_ID, FIXTURE_MODEL_SHA256, 42);
        let clean_value: serde_json::Value = serde_json::from_str(&clean_report).unwrap();
        assert_eq!(
            clean_value["provenance"]["checked_proposition"],
            "no collision in checked RNG streams"
        );
    }

    #[test]
    fn report_card_carries_spatial_variable_method_provenance() {
        let benchmark = benchmark_fixture();
        let mut inspector = inspector_fixture();
        inspector.provenance_status = "clean".into();
        let (report, _) = benchmark_report_card(
            &benchmark,
            Some(&inspector),
            FIXTURE_RUN_ID,
            FIXTURE_MODEL_SHA256,
            42,
        );
        let value: serde_json::Value = serde_json::from_str(&report).unwrap();
        let spatial = &value["selected_cell_evidence"]["spatial_variable_evidence"];

        assert_eq!(
            value["selected_cell_evidence"]["provenance_checked_proposition"],
            "no collision in checked RNG streams"
        );
        assert_eq!(spatial["schema"], INSPECTOR_SCHEMA);
        assert_eq!(
            spatial["protocol_version"].as_u64(),
            Some(INSPECTOR_PROTOCOL_VERSION)
        );
        assert_eq!(spatial["layout"], INSPECTOR_LAYOUT);
        assert_eq!(spatial["domain"], "periodic_2pi");
        assert_eq!(spatial["maps_embedded"], false);
        assert_eq!(spatial["variables"].as_array().unwrap().len(), 4);
        assert_eq!(spatial["variables"][3]["key"], "divergence");
        assert_eq!(spatial["variables"][3]["signed"], true);
        assert_eq!(spatial["variables"][2]["label"], "Recovered pressure");
        assert_eq!(
            spatial["variables"][2]["unit"],
            "solver_velocity_unit_squared"
        );
        assert_eq!(
            spatial["variables"][2]["sources"]["solver_reference"],
            "RECOVERED_FROM_SOLVER_REFERENCE"
        );
        for format in [
            crate::benchmark_export::ExportFormat::Png,
            crate::benchmark_export::ExportFormat::Pdf,
        ] {
            let artifact = crate::benchmark_export::export_report(&report, format).unwrap();
            assert_eq!(
                artifact.canonical_payload_sha256,
                crate::benchmark_export::verify_canonical_report(&report).unwrap()
            );
            artifact.verify().unwrap();
        }
    }

    #[test]
    fn signed_benchmark_maps_keep_negative_and_positive_evidence_distinct() {
        let image = benchmark_map_image(&[-2.0, 0.0, 2.0, 1.0], 2, 2.0, true);
        assert_eq!(image.size, [2, 2]);
        assert_ne!(image.pixels[0], image.pixels[2]);
        assert_eq!(image.pixels[1], field2d::colormap_color(0.0, true));
    }

    #[test]
    fn engineering_navigation_hides_research_tools_by_default() {
        for nav in [Nav::Projects, Nav::Case, Nav::Results, Nav::Evidence] {
            assert!(nav_is_available(nav, false));
        }
        for nav in [
            Nav::FlowPainter,
            Nav::Fields2D,
            Nav::Metrics,
            Nav::Benchmark,
        ] {
            assert!(!nav_is_available(nav, false));
            assert!(nav_is_available(nav, true));
        }
        assert!(!settings::AppSettings::default().developer_research_sandbox);
    }

    #[test]
    fn engineering_layout_helpers_preserve_empty_states_and_narrow_measurements() {
        assert!(!should_show_detail_rail(Nav::Models, true, true));
        assert!(!should_show_detail_rail(Nav::Results, true, false));
        assert!(should_show_detail_rail(Nav::Results, true, true));
        assert!(!should_show_detail_rail(Nav::Evidence, false, false));
        assert!(should_show_detail_rail(Nav::Evidence, true, false));

        assert!(!uses_scientific_canvas(Nav::Results, false));
        assert!(uses_scientific_canvas(Nav::Results, true));
        assert!(uses_scientific_canvas(Nav::Metrics, false));
        assert!(!uses_scientific_canvas(Nav::Evidence, true));

        assert!(measurement_row_stacks(359.0));
        assert!(!measurement_row_stacks(360.0));
    }

    /// Nav stage model (§4.1): Results is gated with an explicit reason (no
    /// silent redirect), and stage glyphs reflect the lifecycle honestly.
    #[test]
    fn workflow_stage_states_gate_results_with_a_reason() {
        // Fresh session: nothing complete, Results blocked with a reason.
        let fresh = workflow_stage_states(false, false, false, false);
        assert_eq!(fresh[0].glyph, StageGlyph::Empty);
        assert_eq!(fresh[1].glyph, StageGlyph::Empty);
        assert!(fresh[2].blocked.is_some());
        assert_eq!(fresh[3].glyph, StageGlyph::Empty);

        // Case open but no completed run: Case Setup is partial, Results
        // still blocked.
        let case_open = workflow_stage_states(true, true, false, false);
        assert_eq!(case_open[0].glyph, StageGlyph::Complete);
        assert_eq!(case_open[1].glyph, StageGlyph::Partial);
        assert!(case_open[2].blocked.is_some());

        // Completed run: everything reachable, nothing blocked.
        let complete = workflow_stage_states(true, true, true, true);
        assert_eq!(complete[1].glyph, StageGlyph::Complete);
        assert_eq!(complete[2].glyph, StageGlyph::Complete);
        assert!(complete[2].blocked.is_none());
        assert_eq!(complete[3].glyph, StageGlyph::Complete);
    }

    fn qualified_model(id: &str) -> engine::ModelCard {
        engine::ModelCard {
            id: format!("reyn_models/{id}.reynmodel"),
            name: id.into(),
            managed: true,
            size_bytes: 1,
            modified_unix: 1,
            checkpoint_sha256: "a".repeat(64),
            status: "clean".into(),
            status_detail: String::new(),
            dimension: 3,
            grid: 64,
            in_channels: 4,
            out_channels: 3,
            max_steps: 32,
            epoch: 1,
            declared_epochs: 1,
            checkpoint_role: "fixed_final".into(),
            scenario: "obstacle".into(),
            source_digest: Some("source".into()),
            physics_contract: engine::EXTERNAL_FLOW_MODEL_PHYSICS_CONTRACT.into(),
            authenticity_status: "verified".into(),
            publisher_key_id: Some("release".into()),
            publisher_key_sha256: Some("b".repeat(64)),
            release_sequence: Some(1),
            support: Vec::new(),
            limitations: Vec::new(),
            benchmark_report_hashes: Vec::new(),
            unknown_fields: Vec::new(),
            qualification_class: engine::MODEL_QUALIFICATION_PRODUCTION.into(),
        }
    }

    #[test]
    fn model_selector_states_are_explicit_and_preserve_missing_identity() {
        let first = qualified_model("first");
        let second = qualified_model("second");
        assert_eq!(
            model_selector_mode(&ModelInventoryState::Loading, &[], "", 64),
            ModelSelectorMode::Loading
        );
        assert_eq!(
            model_selector_mode(&ModelInventoryState::Failed("offline".into()), &[], "", 64,),
            ModelSelectorMode::Failed("offline".into())
        );
        assert_eq!(
            model_selector_mode(&ModelInventoryState::Ready, &[], "", 64),
            ModelSelectorMode::NoneQualified
        );
        assert_eq!(
            model_selector_mode(
                &ModelInventoryState::Ready,
                std::slice::from_ref(&first),
                "",
                64,
            ),
            ModelSelectorMode::OneQualified(first.id.clone())
        );
        assert_eq!(
            model_selector_mode(
                &ModelInventoryState::Ready,
                &[first.clone(), second],
                "",
                64,
            ),
            ModelSelectorMode::MultipleQualified
        );
        assert_eq!(
            model_selector_mode(
                &ModelInventoryState::Ready,
                std::slice::from_ref(&first),
                "reyn_models/missing.reynmodel",
                64,
            ),
            ModelSelectorMode::StoredModelMissing("reyn_models/missing.reynmodel".into())
        );
        let mut bundled_2d = first;
        bundled_2d.dimension = 2;
        assert_eq!(
            model_selector_mode(&ModelInventoryState::Ready, &[bundled_2d], "", 64),
            ModelSelectorMode::NoneQualified
        );
    }

    fn summary_case_fixture() -> engineering::ExternalFlowCase {
        engineering::ExternalFlowCase {
            stage: engineering::CaseStage::Results,
            case_id: "case-1".into(),
            name: "duct fairing".into(),
            source_name: "duct.stl".into(),
            source_revision_id: Some("src-rev".into()),
            case_revision_id: Some("case-rev".into()),
            model_id: "flow3d.reynmodel".into(),
            model_sha256: Some("a".repeat(64)),
            model_max_steps: 32,
            model_support: engineering::ModelSupport::default(),
            preflight: engineering::GeometryPreflight {
                source_sha256: "b".repeat(64),
                ..engineering::GeometryPreflight::default()
            },
            operating: engineering::OperatingPoint {
                length_unit: engineering::LengthUnit::Meter,
                reference_length: 0.2,
                velocity: 30.0,
                density: 1.225,
                viscosity: 1.81e-5,
                reference_pressure: 101_325.0,
                horizon_steps: 4,
                ..engineering::OperatingPoint::default()
            },
            review_focus: engineering::ReviewFocus::default(),
            result: Some(engineering::EngineeringResult {
                method: engineering::SURFACE_LOAD_METHOD.into(),
                cp_min: -1.5,
                cp_max: 0.9,
                force_coefficients: [0.71, 0.02, -0.04],
                moment_coefficients: [0.0, 0.01, 0.0],
                force_newtons: [4.448_221_615_260_5, 0.0, 0.0],
                moment_newton_meters: [0.0, 0.1, 0.0],
                surface_area_m2: 0.05,
                pressure_force_fraction: 0.8,
                load_hotspot: [0.0; 3],
                suction_hotspot: [0.0; 3],
                divergence_rms: 1e-3,
                wake_deficit_peak: 0.3,
                wake_deficit_mean: 0.1,
                moment_origin_mode: "diffuse_surface_centroid".into(),
                semigroup: Some(0.02),
                warnings: Vec::new(),
                ..Default::default()
            }),
            parent_run_id: None,
            named_regions: Vec::new(),
            view_state: Default::default(),
        }
    }

    /// The clipboard summary names the drag coefficient, keeps provenance
    /// identifiers, and converts physical loads into the display system.
    #[test]
    fn results_summary_tsv_carries_named_coefficients_units_and_provenance() {
        let case = summary_case_fixture();
        let format = units::ValueFormat {
            significant_digits: 4,
            notation: units::NumberNotation::Auto,
        };
        let si = results_summary_tsv(&case, Some("run-42"), units::UnitSystem::Si, format);
        assert!(si.starts_with("label\tvalue\tunit\tsource\n"));
        assert!(si.contains("Cd (+X drag)\t0.7100\t1\tDERIVED"));
        assert!(si.contains("run\trun-42\t–\tPROVENANCE"));
        assert!(si.contains(&"b".repeat(64)));
        assert!(si.contains("Fx\t4.448\tN\tDERIVED"));
        assert!(si.contains("V_inf\t30.00\tm/s\tREFERENCE"));

        // Imperial display converts the physical loads (1 lbf exactly here)
        // while coefficients stay dimensionless.
        let imperial =
            results_summary_tsv(&case, Some("run-42"), units::UnitSystem::Imperial, format);
        assert!(imperial.contains("Fx\t1.000\tlbf\tDERIVED"));
        assert!(imperial.contains("Cd (+X drag)\t0.7100\t1\tDERIVED"));

        // Draft (no immutable run) is stated honestly, never invented.
        let draft = results_summary_tsv(&case, None, units::UnitSystem::Si, format);
        assert!(draft.contains("run\tdraft (no immutable run)\t–\tPROVENANCE"));
    }

    /// A queued payload from a terminated generation is stale and can never be
    /// recorded; a preview fetch also never becomes a run.
    #[test]
    fn terminated_and_stale_cad_results_are_never_recorded() {
        // The in-flight run is the only thing that may be recorded.
        assert_eq!(
            classify_cad_result("run-b", None, Some("run-b")),
            CadResultDisposition::Record
        );
        // Interrupting clears `pending_run`; a message already queued by the
        // terminated generation is stale.
        assert_eq!(
            classify_cad_result("run-a", None, None),
            CadResultDisposition::DiscardStale
        );
        // A superseded request nobody is waiting for is stale, not cancelled.
        assert_eq!(
            classify_cad_result("run-c", None, Some("run-b")),
            CadResultDisposition::DiscardStale
        );
        // A playback fetch is display-only even while a run is pending.
        assert_eq!(
            classify_cad_result("preview-1", Some((3, "preview-1")), Some("run-b")),
            CadResultDisposition::Preview(3)
        );
        assert_eq!(
            classify_cad_result("run-a", Some((3, "preview-1")), None),
            CadResultDisposition::DiscardStale
        );
    }

    #[test]
    fn failing_uncached_preview_does_not_fail_a_new_run() {
        let preview = engine::RequestContext {
            id: "preview-a".into(),
            kind: engine::RequestKind::CadPredict,
        };
        assert_eq!(
            classify_engine_error(Some(&preview), Some((7, "preview-a")), Some("new-run"),),
            EngineErrorDisposition::Preview(7)
        );
        assert_eq!(
            classify_cad_result("new-run", None, Some("new-run")),
            CadResultDisposition::Record
        );
    }

    #[test]
    fn stale_cad_error_after_cancel_and_restart_is_discarded() {
        let stale = engine::RequestContext {
            id: "cancelled-generation".into(),
            kind: engine::RequestKind::CadPredict,
        };
        assert_eq!(
            classify_engine_error(Some(&stale), None, Some("retry-generation")),
            EngineErrorDisposition::DiscardStale
        );
    }

    #[test]
    fn unrelated_request_failure_cannot_change_current_run() {
        let list_models = engine::RequestContext {
            id: "list-models".into(),
            kind: engine::RequestKind::ListModels,
        };
        assert_eq!(
            classify_engine_error(Some(&list_models), None, Some("run-a")),
            EngineErrorDisposition::Library(engine::RequestKind::ListModels)
        );
        assert_eq!(
            classify_cad_result("run-a", None, Some("run-a")),
            CadResultDisposition::Record
        );
    }

    #[test]
    fn stale_list_error_cannot_finish_a_new_import() {
        let stale_list = engine::RequestContext {
            id: "list-before-import".into(),
            kind: engine::RequestKind::ListModels,
        };
        let current_import = engine::RequestContext {
            id: "current-import".into(),
            kind: engine::RequestKind::ImportModel,
        };
        assert!(!library_response_is_current(
            Some(&current_import),
            Some(&stale_list),
        ));
        assert!(library_response_is_current(
            Some(&current_import),
            Some(&current_import),
        ));
    }

    #[test]
    fn stale_list_response_cannot_finish_a_new_list_request() {
        let stale = engine::RequestContext {
            id: "list-old".into(),
            kind: engine::RequestKind::ListModels,
        };
        let current = engine::RequestContext {
            id: "list-new".into(),
            kind: engine::RequestKind::ListModels,
        };
        assert!(!library_response_is_current(Some(&current), Some(&stale)));
        assert!(library_response_is_current(Some(&current), Some(&current)));
        assert!(engine_error_is_stale(
            EngineErrorDisposition::Library(engine::RequestKind::ListModels),
            Some(&current),
            Some(&stale),
        ));
        assert!(!engine_error_is_stale(
            EngineErrorDisposition::Library(engine::RequestKind::ListModels),
            Some(&current),
            Some(&current),
        ));
    }

    #[test]
    fn successful_run_is_isolated_from_stale_preview_success() {
        assert_eq!(
            classify_cad_result("old-preview", Some((5, "different-preview")), Some("run-b"),),
            CadResultDisposition::DiscardStale
        );
        assert_eq!(
            classify_cad_result("run-b", Some((5, "different-preview")), Some("run-b")),
            CadResultDisposition::Record
        );
    }

    /// Horizon previews are cached for instant scrubbing, evicted from the far
    /// end first, and never displace the recorded step.
    #[test]
    fn horizon_frames_cache_and_evict_around_the_displayed_step() {
        let frame = |n: usize| HorizonFrame {
            n,
            velocity: vec![0.0; 3 * n * n * n],
            pressure: vec![0.0; n * n * n],
            cp: vec![0.0; n * n * n],
            traction: vec![0.0; 3 * n * n * n],
            mask: std::sync::Arc::new(vec![0.0; n * n * n]),
            force_coefficients: [0.5, 0.0, 0.1],
            cp_min: -1.0,
            cp_max: 0.8,
        };
        let mut playback = HorizonPlayback::default();
        for step in 1..=(HORIZON_CACHE_LIMIT as u32 + 6) {
            playback.frames.insert(step, frame(3));
        }
        playback.step = 20;
        playback.trim(4); // 4 is the recorded horizon
        assert_eq!(playback.frames.len(), HORIZON_CACHE_LIMIT);
        assert!(playback.frames.contains_key(&4), "recorded step kept");
        assert!(playback.frames.contains_key(&20), "displayed step kept");
        assert!(!playback.frames.contains_key(&1), "farthest step evicted");

        // A case serves a cached step from memory and says nothing at all for a
        // step it does not hold, rather than showing the wrong field.
        let mut case = playback_case_fixture();
        case.playback.frames.insert(7, frame(case.result_grid));
        case.playback.step = 7;
        let fields = case.display_fields().expect("cached preview is displayed");
        assert_eq!(fields.step, 7);
        assert!(!fields.recorded, "a preview is not the recorded run");
        case.playback.step = 9;
        assert!(case.display_fields().is_none(), "step 9 is not held");
        // Step 0 means "wherever the run was recorded".
        case.playback.step = 0;
        let recorded = case.display_fields().expect("recorded fields");
        assert_eq!(recorded.step, case.steps);
        assert!(recorded.recorded);
        // Any case edit drops the previews: they belong to the old contract.
        case.playback.reset();
        assert!(case.playback.frames.is_empty());
    }

    fn playback_case_fixture() -> CadCase {
        let n = 3usize;
        let cube = n * n * n;
        let workflow = summary_case_fixture();
        CadCase {
            mask: std::sync::Arc::new(vec![1.0; cube]),
            mask_bounds: Some(([-1.0; 3], [1.0; 3])),
            model: workflow.model_id.clone(),
            steps: 4,
            surf: None,
            surf_mask_source: None,
            name: workflow.name.clone(),
            velocity: vec![0.0; 3 * cube],
            pressure: vec![0.0; cube],
            cp: vec![0.0; cube],
            traction: vec![0.0; 3 * cube],
            result_grid: n,
            dt_frame: 0.04,
            workflow,
            active_run_id: Some(FIXTURE_RUN_ID.into()),
            pending: false,
            pending_request_id: None,
            pending_run: None,
            playback: HorizonPlayback::default(),
        }
    }

    #[test]
    fn fea_menu_predicate_requires_the_complete_load_field() {
        let complete = playback_case_fixture();
        assert!(has_complete_fea_load_field(&complete));

        let mut missing_traction = playback_case_fixture();
        missing_traction.traction.pop();
        assert!(!has_complete_fea_load_field(&missing_traction));

        let mut preview_only = playback_case_fixture();
        preview_only.active_run_id = None;
        assert!(!has_complete_fea_load_field(&preview_only));

        let mut running = playback_case_fixture();
        running.pending = true;
        assert!(!has_complete_fea_load_field(&running));
    }

    fn pending_orientation(
        generation: u64,
        request_id: &str,
        angles: [f64; 3],
    ) -> PendingOrientation {
        PendingOrientation {
            generation,
            request_id: request_id.into(),
            case_id: "case-1".into(),
            source_sha256: "source-sha".into(),
            angles,
            started_at: std::time::Instant::now(),
            kind: PendingOrientationKind::Draft,
        }
    }

    fn orientation_mask(angles: [f64; 3]) -> cad::VoxelMask {
        cad::VoxelMask {
            n: 3,
            mask: vec![1.0; 27],
            solid_voxels: 27,
            char_len: 0.6,
            components: 1,
            minimum_cells_across: 3,
            boundary_clearance_cells: 1,
            axis_disagreement_fraction: 0.0,
            odd_crossing_rows: [0; 3],
            classification_version: 2,
            scale: 1.0,
            orientation: cad::BodyOrientation::from_degrees(angles),
            transform_4x4: [
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
            ],
        }
    }

    fn orientation_completion(
        generation: u64,
        request_id: &str,
        angles: [f64; 3],
        result: Result<cad::VoxelMask, String>,
    ) -> OrientationWorkResult {
        OrientationWorkResult {
            generation,
            request_id: request_id.into(),
            case_id: "case-1".into(),
            source_sha256: "source-sha".into(),
            angles,
            completed_utc_unix: 200,
            result,
        }
    }

    #[test]
    fn rapid_orientation_requests_coalesce_to_newest_generation() {
        let (tx, rx) = std::sync::mpsc::channel();
        let request = |generation, angle| OrientationWorkRequest {
            generation,
            request_id: format!("orientation-{generation}"),
            case_id: "case-1".into(),
            source_sha256: "source-sha".into(),
            source_name: "body.stl".into(),
            angles: [angle, 0.0, 0.0],
            grid: 32,
            source_bytes: vec![generation as u8],
            selected_shell_entity_id: Some(10 + generation),
        };
        tx.send(request(2, 4.0)).unwrap();
        tx.send(request(3, 7.5)).unwrap();
        let newest = coalesce_orientation_requests(request(1, 1.0), &rx);
        assert_eq!(newest.generation, 3);
        assert_eq!(newest.request_id, "orientation-3");
        assert_eq!(newest.angles, [7.5, 0.0, 0.0]);
        assert_eq!(newest.source_bytes, vec![3]);
        assert_eq!(newest.selected_shell_entity_id, Some(13));
    }

    #[test]
    fn geometry_setup_status_names_transform_gate_not_preflight() {
        let mut preflight = engineering::GeometryPreflight {
            source_sha256: "a".repeat(64),
            source_bytes: 1024,
            triangles: 12,
            solid_voxels: 216,
            target_grid: 64,
            components: 1,
            source_extents: [1.0; 3],
            proposed_scale: 1.0,
            solver_characteristic_length: 0.6,
            transform_4x4: [
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
            ],
            voxel_components: 1,
            minimum_cells_across: 4,
            boundary_clearance_cells: 6,
            voxel_classification_version: 2,
            ..engineering::GeometryPreflight::default()
        };
        let waiting = geometry_setup_engine_status("cuboid_ap214", &preflight);
        assert!(waiting.contains("transform approval required"), "{waiting}");
        assert!(!waiting.contains("preflight required"), "{waiting}");
        preflight.transform_approved = true;
        let accepted = geometry_setup_engine_status("cuboid_ap214", &preflight);
        assert!(accepted.contains("geometry accepted"), "{accepted}");
        preflight.boundary_edges = 1;
        let incomplete = geometry_setup_engine_status("cuboid_ap214", &preflight);
        assert!(incomplete.contains("preflight incomplete"), "{incomplete}");
    }

    #[test]
    fn stale_orientation_completion_cannot_mutate_current_case() {
        let pending = pending_orientation(2, "orientation-2", [8.0, 1.0, 0.0]);
        let stale = orientation_completion(
            1,
            "orientation-1",
            [3.0, 0.0, 0.0],
            Ok(orientation_mask([3.0, 0.0, 0.0])),
        );
        assert_eq!(
            classify_orientation_result(&stale, Some(&pending), Some(("case-1", "source-sha"))),
            OrientationResultDisposition::DiscardStale
        );

        let current = orientation_completion(
            2,
            "orientation-2",
            [8.0, 1.0, 0.0],
            Ok(orientation_mask([8.0, 1.0, 0.0])),
        );
        assert_eq!(
            classify_orientation_result(&current, Some(&pending), Some(("case-1", "source-sha"))),
            OrientationResultDisposition::Apply
        );
        assert_eq!(
            classify_orientation_result(
                &current,
                Some(&pending),
                Some(("other-case", "source-sha"))
            ),
            OrientationResultDisposition::DiscardStale
        );
    }

    #[test]
    fn recovered_case_reconstruction_is_async_and_not_project_dirty() {
        let angles = [6.0, -1.0, 0.25];
        let pending = PendingOrientation {
            generation: 5,
            request_id: "orientation-hydrate-5".into(),
            case_id: "case-1".into(),
            source_sha256: "source-sha".into(),
            angles,
            started_at: std::time::Instant::now(),
            kind: PendingOrientationKind::Hydrate(Box::new(PendingOrientationHydration {
                workflow: summary_case_fixture(),
                selected_run_id: None,
                dt_frame: 0.0,
            })),
        };
        assert!(
            !pending.mutates_project(),
            "runtime reconstruction is not an edit"
        );
        let completed = orientation_completion(
            5,
            "orientation-hydrate-5",
            angles,
            Ok(orientation_mask(angles)),
        );
        assert_eq!(
            classify_orientation_result(&completed, Some(&pending), None),
            OrientationResultDisposition::Apply
        );
        assert_eq!(
            classify_orientation_result(&completed, Some(&pending), Some(("case-1", "source-sha"))),
            OrientationResultDisposition::DiscardStale
        );
    }

    #[test]
    fn orientation_worker_hands_failures_back_with_request_identity() {
        let worker = OrientationWorker::spawn(None).expect("worker starts");
        worker
            .request_tx
            .send(OrientationWorkRequest {
                generation: 9,
                request_id: "orientation-9".into(),
                case_id: "case-1".into(),
                source_sha256: "source-sha".into(),
                source_name: "broken.stl".into(),
                angles: [12.0, -2.0, 0.5],
                grid: 32,
                source_bytes: vec![0; 4],
                selected_shell_entity_id: None,
            })
            .unwrap();
        let completed = worker
            .result_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("failure is delivered without UI polling");
        assert_eq!(completed.generation, 9);
        assert_eq!(completed.request_id, "orientation-9");
        assert_eq!(completed.angles, [12.0, -2.0, 0.5]);
        assert!(completed
            .result
            .as_ref()
            .is_err_and(|error| error.contains("file too small")));
        let pending = pending_orientation(9, "orientation-9", completed.angles);
        assert_eq!(
            classify_orientation_result(&completed, Some(&pending), Some(("case-1", "source-sha"))),
            OrientationResultDisposition::Failed
        );
        assert_eq!(
            autosave_deadline_after_attempt(completed.completed_utc_unix, 30, false),
            completed.completed_utc_unix + 5
        );
    }

    #[test]
    fn save_and_run_gate_draft_or_in_flight_orientation_geometry() {
        let angles = [5.0, 0.0, 0.0];
        let pending = pending_orientation(4, "orientation-4", angles);
        let save = orientation_geometry_gate("Saving project drafts", Some(angles), Some(&pending))
            .expect("save is gated");
        assert!(save.contains(&short_id("orientation-4")));
        assert!(save.contains("re-voxelizing"));

        let run = orientation_geometry_gate("Starting a new run", Some(angles), None)
            .expect("run is gated");
        assert!(run.contains("not in the voxel mask"));
        assert!(orientation_geometry_gate("Saving project drafts", None, None).is_none());
    }

    #[test]
    fn static_ready_or_unavailable_state_has_no_background_poll() {
        assert_eq!(
            background_repaint_delays(false, false, false, 10, 20),
            (None, None)
        );
        assert_eq!(
            background_repaint_delays(true, false, false, 10, 20).0,
            Some(LIVE_REPAINT_INTERVAL)
        );
        assert_eq!(
            background_repaint_delays(false, true, false, 10, 20).1,
            Some(std::time::Duration::from_secs(10))
        );
        // An orientation draft or worker request is event-driven. A previously
        // scheduled autosave may wake once, but it never starts an idle poll;
        // worker completion or the next user edit wakes the UI.
        assert_eq!(
            background_repaint_delays(false, true, true, 20, 20),
            (None, None)
        );
        assert_eq!(autosave_deadline_after_attempt(40, 30, true), 70);
        assert_eq!(autosave_deadline_after_attempt(40, 30, false), 45);
    }

    #[test]
    fn case_and_orientation_drafts_drive_dirty_guards_and_recovery_wakes() {
        assert!(!has_unsaved_project_work(false, false, None, false));
        assert!(has_unsaved_project_work(true, false, None, false));
        assert!(has_unsaved_project_work(false, true, None, false));
        assert!(has_unsaved_project_work(
            false,
            false,
            Some([3.0, -1.5, 0.25]),
            false
        ));
        assert!(has_unsaved_project_work(false, false, None, true));

        let dirty = has_unsaved_project_work(false, true, None, false);
        assert_eq!(
            background_repaint_delays(false, dirty, false, 100, 130),
            (None, Some(std::time::Duration::from_secs(30)))
        );
        let orientation_dirty =
            has_unsaved_project_work(false, false, Some([3.0, -1.5, 0.25]), false);
        assert_eq!(
            background_repaint_delays(false, orientation_dirty, true, 130, 130),
            (None, None)
        );
        // A maximum-grid orientation re-voxelization may finish well after the
        // deadline that started it. The next wake is measured from completion,
        // not from the stale frame timestamp, and still uses one event deadline.
        let started = 130;
        let completed = 171;
        let next = autosave_deadline_after_attempt(completed, 30, true);
        assert_eq!(next, 201);
        assert_eq!(
            background_repaint_delays(false, true, false, completed, next),
            (None, Some(std::time::Duration::from_secs(30)))
        );
        assert_ne!(next, started + 30);
    }

    #[test]
    fn read_only_capability_rejects_project_mutations_centrally() {
        assert!(
            project_mutation_rejection(project_lifecycle::ProjectAccessMode::Full, "Run").is_none()
        );
        let rejection = project_mutation_rejection(
            project_lifecycle::ProjectAccessMode::ReadOnlyEvidence,
            "Recording a case revision",
        )
        .expect("read-only mode rejects mutation");
        assert!(rejection.contains("Recording a case revision"));
        assert!(rejection.contains("read-only evidence mode"));
        assert!(rejection.contains("runs and evidence remain unchanged"));
        let run_rejection = project_mutation_rejection(
            project_lifecycle::ProjectAccessMode::ReadOnlyEvidence,
            "Starting a new immutable run",
        )
        .expect("read-only mode rejects engine execution");
        assert!(run_rejection.contains("Starting a new immutable run"));
        let orientation_rejection = project_mutation_rejection(
            project_lifecycle::ProjectAccessMode::ReadOnlyEvidence,
            "Changing body orientation",
        )
        .expect("read-only mode rejects orientation workers");
        assert!(orientation_rejection.contains("Changing body orientation"));
        assert!(orientation_rejection.contains("unchanged and inspectable"));
    }

    #[test]
    fn optimistic_save_conflicts_map_to_unique_sibling_copies() {
        let conflict =
            project_lifecycle::LifecycleError::Project(project::ProjectError::WriteConflict {
                expected_sha256: Some("a".repeat(64)),
                actual_sha256: Some("b".repeat(64)),
            });
        assert!(is_project_write_conflict(&conflict));
        assert!(!is_project_write_conflict(
            &project_lifecycle::LifecycleError::SaveAsRequired
        ));

        let original = std::path::Path::new("/tmp/wing.reynproj");
        let first = conflict_copy_path(original, 42, "abcdef123456");
        let second = conflict_copy_path(original, 42, "9876543210");
        assert_eq!(
            first,
            std::path::PathBuf::from("/tmp/wing conflict 42 abcdef12.reynproj")
        );
        assert_ne!(first, second);
        assert_eq!(first.parent(), original.parent());
        assert_eq!(
            resolve_project_conflict_action(ProjectConflictAction::Reload, original, 42, "unused"),
            ProjectConflictResolution::Reload(original.to_path_buf())
        );
        assert_eq!(
            resolve_project_conflict_action(ProjectConflictAction::SaveAs, original, 42, "unused"),
            ProjectConflictResolution::PromptSaveAs
        );
        assert_eq!(
            resolve_project_conflict_action(
                ProjectConflictAction::ConflictCopy,
                original,
                42,
                "abcdef123456"
            ),
            ProjectConflictResolution::SaveConflictCopy(first)
        );
        assert_eq!(
            resolve_project_conflict_action(ProjectConflictAction::Dismiss, original, 42, "unused"),
            ProjectConflictResolution::Dismiss
        );
    }

    #[test]
    fn failed_persistence_drops_large_transient_results_before_ui_installation() {
        struct LargeTransient {
            values: Vec<f32>,
            dropped: std::rc::Rc<std::cell::Cell<bool>>,
        }

        impl Drop for LargeTransient {
            fn drop(&mut self) {
                self.dropped.set(true);
            }
        }

        let dropped = std::rc::Rc::new(std::cell::Cell::new(false));
        let transient = LargeTransient {
            // The engineering field payload carries nine 64³ float channels.
            values: vec![0.0; 9 * 64usize.pow(3)],
            dropped: dropped.clone(),
        };
        let mut persistence_attempts = 0;
        let result: Result<(String, LargeTransient), String> =
            retain_after_persistence(transient, |field| {
                persistence_attempts += 1;
                assert_eq!(field.values.len(), 9 * 64usize.pow(3));
                Err("injected atomic write failure".into())
            });

        assert_eq!(persistence_attempts, 1);
        assert_eq!(
            result
                .err()
                .expect("fault injection must reject persistence"),
            "injected atomic write failure".to_string()
        );
        assert!(dropped.get(), "failed transient payload must be released");
    }

    #[test]
    fn cad_volume_density_calibration_opens_thin_wake_fields() {
        let mut volume = vec![0u8; 64 * 64 * 64];
        for value in volume.iter_mut().take(400) {
            *value = 40;
        }
        for value in volume.iter_mut().skip(400).take(80) {
            *value = 200;
        }
        let (lo, hi) = calibrate_cad_volume_density(&volume);
        assert!(
            lo < 0.50,
            "CAD wakes must not inherit sandbox density_lo≈0.85 (got {lo})"
        );
        assert!(hi > lo + 0.05, "lo={lo} hi={hi}");
        let (empty_lo, empty_hi) = calibrate_cad_volume_density(&[]);
        assert!(empty_lo < empty_hi);
    }

    #[test]
    fn cad_case_keeps_precomputed_geometry_bounds() {
        let case = playback_case_fixture();
        assert_eq!(case.mask_bounds, Some(([-1.0; 3], [1.0; 3])));
    }

    /// PNG export helper: upscales small images by an integer factor and
    /// produces a decodable PNG of the right dimensions.
    #[test]
    fn color_image_png_bytes_round_trips_dimensions() {
        let image = egui::ColorImage {
            size: [2, 2],
            pixels: vec![Color32::RED, Color32::GREEN, Color32::BLUE, Color32::WHITE],
            source_size: egui::Vec2::new(2.0, 2.0),
        };
        let bytes = color_image_png_bytes(&image, 8).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let reader = decoder.read_info().unwrap();
        let info = reader.info();
        assert_eq!((info.width, info.height), (8, 8));
        // min_edge = 0 keeps native resolution.
        let native = color_image_png_bytes(&image, 0).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(native));
        let reader = decoder.read_info().unwrap();
        assert_eq!((reader.info().width, reader.info().height), (2, 2));
    }

    #[test]
    fn screenshot_worker_crops_and_writes_off_thread() {
        let image = egui::ColorImage {
            size: [4, 4],
            pixels: vec![Color32::WHITE; 16],
            source_size: egui::Vec2::new(4.0, 4.0),
        };
        let path = std::env::temp_dir().join(format!(
            "reyn-screenshot-worker-{}.png",
            uuid::Uuid::new_v4()
        ));
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_screenshot_write(
            tx,
            egui::Context::default(),
            std::sync::Arc::new(image),
            Some((
                Rect::from_min_max(egui::pos2(1.0, 1.0), egui::pos2(3.0, 3.0)),
                1.0,
            )),
            path.clone(),
            ScreenshotWriteKind::Viewport,
            ViewportShotProvenance {
                footer_lines: vec!["TEST FOOTER".into()],
            },
        );
        let completion = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("screenshot worker completion");
        completion.result.expect("screenshot write succeeds");
        let bytes = std::fs::read(&path).expect("screenshot bytes");
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let reader = decoder.read_info().unwrap();
        // Crop is 2×2; provenance footer appends a fixed strip (12 + 18·lines + 10).
        assert_eq!(
            (reader.info().width, reader.info().height),
            (2, 2 + 12 + 18 + 10)
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn docs_resolution_prefers_bundle_and_never_falls_back_when_incomplete() {
        let root = std::env::temp_dir().join(format!("reyn-docs-bundle-{}", uuid::Uuid::new_v4()));
        let executable = root.join("Reyn Studio.app/Contents/MacOS/reyn-studio");
        let bundled = root.join("Reyn Studio.app/Contents/Resources/docs/PRD.md");
        let development = root.join("PRD.md");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(bundled.parent().unwrap()).unwrap();
        std::fs::write(&bundled, "# Packaged docs\n").unwrap();
        std::fs::write(&development, "# Development docs\n").unwrap();

        assert_eq!(
            resolve_docs_path_at(&executable, Some(&root)).unwrap(),
            bundled
        );
        std::fs::remove_file(&bundled).unwrap();
        let error = resolve_docs_path_at(&executable, Some(&root)).unwrap_err();
        assert!(error.contains("Contents/Resources/docs/PRD.md"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn docs_resolution_finds_development_ancestor_without_build_path_literal() {
        let root = std::env::temp_dir().join(format!("reyn-docs-dev-{}", uuid::Uuid::new_v4()));
        let executable = root.join("target/debug/reyn-studio");
        let current_dir = root.join("nested/work");
        let docs = root.join("PRD.md");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&current_dir).unwrap();
        std::fs::write(&docs, "# Development docs\n").unwrap();

        assert_eq!(
            resolve_docs_path_at(&executable, Some(&current_dir)).unwrap(),
            docs
        );

        let source = include_str!("app.rs");
        for forbidden in [
            ["file", "://./", "PRD.md"].concat(),
            ["CARGO_", "MANIFEST_DIR"].concat(),
            ["/", "Users", "/"].concat(),
        ] {
            assert!(
                !source.contains(&forbidden),
                "OpenDocs source contains forbidden literal: {forbidden}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_scientific_and_placeholder_labels_do_not_reenter_shell() {
        let source = include_str!("app.rs");
        let forbidden = [
            ["Surface Loads (", "C", "p", ")"].concat(),
            ["Truth", " Overlay"].concat(),
            ["Solver ", "truth"].concat(),
            ["Project ", "Alpha"].concat(),
            ["Live ", "Session"].concat(),
            ["\"Sup", "port\""].concat(),
        ];
        for label in forbidden {
            assert!(
                !source.contains(&label),
                "legacy shell label returned: {label}"
            );
        }
    }

    #[test]
    fn force_variant_series_keeps_cd_cs_cl_order_and_skips_other_scalars() {
        let rows = vec![
            ("wake_deficit_peak".into(), 0.2, 0.3, "1".into()),
            ("force_coefficient_z".into(), -0.04, -0.01, "1".into()),
            ("force_coefficient_x".into(), 0.71, 0.82, "1".into()),
            ("force_coefficient_y".into(), 0.02, 0.01, "1".into()),
        ];
        let series = force_variant_series(&rows);
        assert_eq!(
            series,
            vec![("Cd", 0.71, 0.82), ("Cs", 0.02, 0.01), ("Cl", -0.04, -0.01)]
        );
        assert_eq!(
            force_coefficient_plot_label("force_coefficient_x"),
            Some("Cd")
        );
        assert!(force_coefficient_plot_label("moment_coefficient_x").is_none());
    }
}
