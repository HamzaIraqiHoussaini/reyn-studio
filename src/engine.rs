//! Python engine client. Spawns `reyn_engine.py` under the research venv, reads
//! `READY {port}`, connects over loopback TCP, and exchanges length-prefixed
//! frames. Runs on a worker thread; the UI talks to it via channels so inference
//! (seconds) never blocks the egui frame.
use crate::benchmark_evidence::{
    InspectorMaps, InspectorVariable, INSPECTOR_DERIVATIVE, INSPECTOR_DOMAIN, INSPECTOR_LAYOUT,
    INSPECTOR_PRESSURE, INSPECTOR_PROTOCOL_VERSION, INSPECTOR_SCHEMA,
};
use crate::runtime;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const ENGINE_ENTRYPOINT: &str = "engine/reyn_engine.py";
const ENGINE_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const ENGINE_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// CAD predicts include Brinkman warmup + one model forward + pressure/loads.
/// Keep a longer socket bound so slow CPU/MPS hosts do not fail the customer path.
const ENGINE_CAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(360);
#[cfg(test)]
const DEVELOPMENT_UNSIGNED_FIXTURE_MARKER: &str = ".development-unsigned-model-fixture";
pub const MODEL_BUNDLE_EXTENSION: &str = "reynmodel";
pub const MODEL_SIGNATURE_SUFFIX: &str = ".sig";
pub const DEFAULT_3D_MODEL_ID: &str = "reyn-flow3d-obstacle-32-v1.reynmodel";
pub const DEFAULT_2D_MODEL_ID: &str = "reyn-h64-tail-brinkman-seed0-v1.reynmodel";
pub const TRUSTED_MODEL_CONVERSION_GUIDANCE: &str =
    "Production inference requires a verified .reynmodel bundle and its adjacent \
     .reynmodel.sig publisher signature. Legacy .pth files are never opened; convert a \
     checkpoint you trust offline with convert_model_bundle.py, have an authorized publisher \
     sign it, and copy or relink both files together.";
const REQUIRED_ENGINE_RESOURCES: &[&str] = &[
    "model_bundle.py",
    "n5_inspector.py",
    "n5_overlap.py",
    "pinned_model_trust.py",
    "reyn_engine.py",
];
const REQUIRED_RESEARCH_MODULES: &[&str] = &[
    "dataset.py",
    "dataset_3d.py",
    "flow_contract.py",
    "flow_quantities.py",
    "models_3d.py",
    "occupancy_geometry_3d.py",
    "obstacle_dataset.py",
    "obstacle_solver.py",
    "obstacle_solver_3d.py",
    "pressure_channel_contract_3d.py",
    "pressure_model_contract_3d.py",
    "spectral_solver.py",
    "spectral_solver_3d.py",
    "time_moe_operator.py",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    pub research_dir: String,
    pub python_path: String,
    pub device: String,
    #[cfg(test)]
    pub engine_script: Option<String>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        let research_dir = research_dir();
        let local_python = if cfg!(target_os = "windows") {
            Path::new(&research_dir).join(".venv/Scripts/python.exe")
        } else {
            Path::new(&research_dir).join(".venv/bin/python")
        };
        let python_path = std::env::var("REYN_PYTHON").unwrap_or_else(|_| {
            if local_python.is_file() {
                local_python.to_string_lossy().into_owned()
            } else if cfg!(target_os = "windows") {
                "python".into()
            } else {
                "python3".into()
            }
        });
        Self {
            research_dir,
            python_path,
            device: std::env::var("REYN_DEVICE").unwrap_or_else(|_| "auto".into()),
            #[cfg(test)]
            engine_script: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ModelCard {
    pub id: String,
    pub name: String,
    pub managed: bool,
    pub size_bytes: u64,
    pub modified_unix: u64,
    pub checkpoint_sha256: String,
    pub status: String,
    pub status_detail: String,
    pub dimension: u32,
    pub grid: u32,
    pub in_channels: u32,
    pub out_channels: u32,
    pub max_steps: u32,
    pub epoch: u32,
    pub declared_epochs: u32,
    pub checkpoint_role: String,
    pub scenario: String,
    pub source_digest: Option<String>,
    pub physics_contract: String,
    pub authenticity_status: String,
    pub publisher_key_id: Option<String>,
    pub publisher_key_sha256: Option<String>,
    pub release_sequence: Option<u64>,
    pub support: Vec<String>,
    pub limitations: Vec<String>,
    pub benchmark_report_hashes: Vec<String>,
    pub unknown_fields: Vec<String>,
    /// Fail-closed release class for Studio execution claims.
    ///
    /// - `production`: may qualify for external-flow CAD when the contract matches
    /// - `preview`: authenticatable and inspectable; never CAD
    /// - `research`: authenticatable; CAD only for the signed 32³ 5→3 obstacle operator
    /// - `unknown`: treated as incomplete
    pub qualification_class: String,
}

pub const EXTERNAL_FLOW_MODEL_PHYSICS_CONTRACT: &str = "fixed_body_brinkman.v1";
pub const MODEL_QUALIFICATION_PRODUCTION: &str = "production";
pub const MODEL_QUALIFICATION_PREVIEW: &str = "preview";
pub const MODEL_QUALIFICATION_RESEARCH: &str = "research";
pub const MODEL_QUALIFICATION_UNKNOWN: &str = "unknown";
pub const YC_PREVIEW_MODEL_RELEASE_SCHEMA: &str = "com.reyn.yc-preview-model-release/1";

/// Local trusted-checkpoint escape hatch. When
/// `REYN_MODEL_DEVELOPMENT_UNSIGNED=1`, Studio accepts the engine's
/// `development_unsigned_override` authenticity status for external-flow CAD.
/// Signed production publishers still use `verified`. Never set this in release
/// packaging.
pub fn development_unsigned_models_allowed() -> bool {
    matches!(
        std::env::var("REYN_MODEL_DEVELOPMENT_UNSIGNED").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

pub fn model_authenticity_allows_external_flow(status: &str) -> bool {
    status == "verified"
        || (development_unsigned_models_allowed() && status == "development_unsigned_override")
}

/// Derive the release class from published identity and limitations.
/// Preview / research labels always win over a clean authenticity status so a
/// YC preview pack cannot silently become a production CAD qualifier.
pub fn derive_model_qualification_class(
    version: &str,
    limitations: &[String],
    status: &str,
) -> String {
    let version = version.to_ascii_lowercase();
    let limitation_blob = limitations
        .iter()
        .map(|line| line.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("\n");
    if version.contains("yc-preview") || version.contains("preview") {
        return MODEL_QUALIFICATION_PREVIEW.into();
    }
    if version.contains("research") {
        return MODEL_QUALIFICATION_RESEARCH.into();
    }
    if limitation_blob.contains("yc research preview")
        || limitation_blob.contains("not production-qualified")
    {
        return MODEL_QUALIFICATION_PREVIEW.into();
    }
    if limitation_blob.contains("research") || limitation_blob.contains("not production") {
        return MODEL_QUALIFICATION_RESEARCH.into();
    }
    if status == "clean"
        && !limitation_blob.contains("incomplete")
        && !limitation_blob.contains("preview")
    {
        return MODEL_QUALIFICATION_PRODUCTION.into();
    }
    MODEL_QUALIFICATION_UNKNOWN.into()
}

pub fn external_flow_research_operator_allowed(
    model: &ModelCard,
    target_grid: usize,
) -> bool {
    model.qualification_class == MODEL_QUALIFICATION_RESEARCH
        && model.dimension == 3
        && model.grid == 32
        && target_grid == 32
        && model.in_channels == 5
        && model.out_channels == 3
        && model.scenario == "obstacle"
        && model.physics_contract == EXTERNAL_FLOW_MODEL_PHYSICS_CONTRACT
}

pub fn external_flow_model_issues(model: &ModelCard, target_grid: usize) -> Vec<String> {
    let mut issues = Vec::new();
    if !is_model_bundle_id(&model.id) {
        issues.push("the artifact does not have a canonical .reynmodel identity".into());
    }
    if !model_authenticity_allows_external_flow(&model.authenticity_status) {
        issues.push("publisher authenticity is not verified".into());
    }
    let research_cad = external_flow_research_operator_allowed(model, target_grid);
    if model.qualification_class != MODEL_QUALIFICATION_PRODUCTION && !research_cad {
        issues.push(format!(
            "qualification class is {}, not production (preview packs cannot run external-flow CAD)",
            if model.qualification_class.trim().is_empty() {
                MODEL_QUALIFICATION_UNKNOWN
            } else {
                model.qualification_class.as_str()
            }
        ));
    }
    if model.status != "clean" {
        issues.push(format!(
            "qualification status is {}",
            if model.status.trim().is_empty() {
                "unknown"
            } else {
                model.status.as_str()
            }
        ));
    }
    if model.dimension != 3 {
        issues.push(format!("dimension is {}D, not 3D", model.dimension));
    }
    if model.grid as usize != target_grid {
        issues.push(format!(
            "grid is {}³, but this geometry requires {}³",
            model.grid, target_grid
        ));
    }
    if !matches!(
        (model.in_channels, model.out_channels),
        (4, 3) | (5, 3)
    ) {
        issues.push(format!(
            "channels are {}→{}, not a 4→3 or 5→3 3D obstacle contract",
            model.in_channels, model.out_channels
        ));
    }
    if model.scenario != "obstacle" {
        issues.push(format!(
            "scenario is {:?}, not fixed-body obstacle flow",
            model.scenario
        ));
    }
    if model.physics_contract != EXTERNAL_FLOW_MODEL_PHYSICS_CONTRACT {
        issues.push(format!(
            "physics contract is {:?}, not {}",
            model.physics_contract, EXTERNAL_FLOW_MODEL_PHYSICS_CONTRACT
        ));
    }
    if model.max_steps == 0 {
        issues.push("no supported prediction horizon is declared".into());
    }
    if model.checkpoint_sha256.len() != 64
        || !model
            .checkpoint_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        issues.push("checkpoint SHA-256 is missing or non-canonical".into());
    }
    issues
}

pub fn is_qualified_external_flow_model(model: &ModelCard, target_grid: usize) -> bool {
    external_flow_model_issues(model, target_grid).is_empty()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelValidationIssue {
    pub code: String,
    pub field: String,
    pub message: String,
    pub severity: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelValidation {
    pub accepted: bool,
    pub status: String,
    pub summary: String,
    pub issues: Vec<ModelValidationIssue>,
    pub candidate_name: Option<String>,
    pub candidate_sha256: Option<String>,
}

pub fn is_model_bundle_id(model: &str) -> bool {
    Path::new(model.trim())
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case(MODEL_BUNDLE_EXTENSION))
}

pub fn model_signature_path(bundle: impl AsRef<Path>) -> PathBuf {
    let bundle = bundle.as_ref();
    let mut signature_name = bundle.as_os_str().to_os_string();
    signature_name.push(MODEL_SIGNATURE_SUFFIX);
    PathBuf::from(signature_name)
}

pub fn require_model_signature(bundle: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    let signature = model_signature_path(bundle);
    if signature.is_file() {
        Ok(signature)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "[signature.missing] Required detached publisher signature not found: {}. {}",
                signature.display(),
                TRUSTED_MODEL_CONVERSION_GUIDANCE
            ),
        ))
    }
}

fn require_model_bundle_id(model: &str) -> std::io::Result<()> {
    if is_model_bundle_id(model) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            TRUSTED_MODEL_CONVERSION_GUIDANCE,
        ))
    }
}

fn model_format_rejection(path: &str) -> ModelValidation {
    ModelValidation {
        accepted: false,
        status: "rejected".into(),
        summary: TRUSTED_MODEL_CONVERSION_GUIDANCE.into(),
        issues: vec![ModelValidationIssue {
            code: "bundle.invalid_extension".into(),
            field: "path".into(),
            message: TRUSTED_MODEL_CONVERSION_GUIDANCE.into(),
            severity: "error".into(),
        }],
        candidate_name: Path::new(path)
            .file_name()
            .and_then(OsStr::to_str)
            .map(str::to_owned),
        candidate_sha256: None,
    }
}

fn model_signature_rejection(path: &str, error: &std::io::Error) -> ModelValidation {
    ModelValidation {
        accepted: false,
        status: "rejected".into(),
        summary: error.to_string(),
        issues: vec![ModelValidationIssue {
            code: "signature.missing".into(),
            field: "signature".into(),
            message: error.to_string(),
            severity: "error".into(),
        }],
        candidate_name: Path::new(path)
            .file_name()
            .and_then(OsStr::to_str)
            .map(str::to_owned),
        candidate_sha256: None,
    }
}

pub struct Field {
    pub shape: Vec<usize>,
    pub data: Vec<f32>,
    pub scenario: String,
}

/// A 2D field for the pressure-recovery view: model velocity plus recovered
/// pressure (`ai` = `[3,N,N]` u,v,p), optional solver reference in the legacy
/// protocol field `truth`, and separate self-consistency/reference metrics.
pub struct Field2D {
    pub n: usize,
    pub ai: Vec<f32>,
    pub truth: Option<Vec<f32>>,
    pub horizon: u32,
    pub dt_frame: f32,
    pub peak_p: f32,
    pub low_p: f32,
    pub semigroup: Option<f32>,
    pub rel_l2: Option<f32>,
    pub persist: Option<f32>,
    pub p_residual: f32,
    pub p_iters: u32,
    pub p_method: String,
    pub scenario: String,
}

pub enum Cmd {
    WithContext {
        request: RequestContext,
        command: Box<Cmd>,
    },
    ListModels,
    ImportModel {
        path: String,
    },
    DeleteModel {
        model: String,
    },
    Predict {
        model: String,
        seed: u64,
    },
    Predict2D {
        model: String,
        steps: u32,
        seed: u64,
        want_truth: bool,
        method: String,   // "spectral" | "fd"
        tolerance: f32,   // FD stop tolerance
        boundary: String, // "periodic" | "dirichlet"
    },
    /// Flow Painter: advance a user-painted `[2,N,N]` velocity IC. Stateless —
    /// the IC rides with every request (a 128² IC is ~130 KB over loopback).
    PredictIC {
        model: String,
        steps: u32,
        ic: std::sync::Arc<Vec<f32>>,
    },
    /// CAD flow analysis: a voxelized `[N³]` geometry mask becomes a wind-tunnel
    /// case (engine caches the solver warmup per mask, so horizon changes are
    /// one model pass).
    CadPredict {
        request_id: String,
        model: String,
        steps: u32,
        mask: std::sync::Arc<Vec<f32>>,
        reynolds: f32,
        characteristic_length_solver: f32,
        reference_length_m: f32,
        velocity_mps: f32,
        density_kg_m3: f32,
        reference_pressure_pa: f32,
        /// `diffuse_surface_centroid` or `source_frame_point`.
        moment_origin_mode: String,
        /// Solver-frame origin used when mode is `source_frame_point`.
        moment_origin_solver: Option<[f32; 3]>,
        /// Optional aero reference area in m² for coefficient reporting.
        reference_area_m2: Option<f32>,
    },
    /// N5 — run the benchmark suite (seeds × horizons) on a 2D checkpoint.
    RunBenchmark {
        model: String,
        seeds: Vec<u32>,
        horizons: Vec<u32>,
    },
    /// N5.2 — fetch field/error/spectral evidence only for the selected cell.
    InspectBenchmarkCell {
        model: String,
        seed: u32,
        horizon: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestKind {
    ListModels,
    ImportModel,
    DeleteModel,
    Predict,
    Predict2D,
    PredictIc,
    CadPredict,
    RunBenchmark,
    InspectBenchmarkCell,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestContext {
    pub id: String,
    pub kind: RequestKind,
}

impl Cmd {
    fn request_context(&self) -> RequestContext {
        let (kind, existing_id) = match self {
            Self::WithContext { request, .. } => return request.clone(),
            Self::ListModels => (RequestKind::ListModels, None),
            Self::ImportModel { .. } => (RequestKind::ImportModel, None),
            Self::DeleteModel { .. } => (RequestKind::DeleteModel, None),
            Self::Predict { .. } => (RequestKind::Predict, None),
            Self::Predict2D { .. } => (RequestKind::Predict2D, None),
            Self::PredictIC { .. } => (RequestKind::PredictIc, None),
            Self::CadPredict { request_id, .. } => {
                (RequestKind::CadPredict, Some(request_id.clone()))
            }
            Self::RunBenchmark { .. } => (RequestKind::RunBenchmark, None),
            Self::InspectBenchmarkCell { .. } => (RequestKind::InspectBenchmarkCell, None),
        };
        RequestContext {
            id: existing_id.unwrap_or_else(|| format!("engine-{}", uuid::Uuid::new_v4())),
            kind,
        }
    }

    fn into_request(self) -> (RequestContext, Cmd) {
        match self {
            Self::WithContext { request, command } => (request, *command),
            command => {
                let request = command.request_context();
                (request, command)
            }
        }
    }
}

/// CAD prediction: model velocity + recovered pressure + the smoothed mask.
pub struct CadField {
    pub request_id: String,
    pub n: usize,
    pub vel: Vec<f32>,      // [3,N,N,N]
    pub pressure: Vec<f32>, // physical Pa, [N,N,N]
    pub mask: Vec<f32>,     // [N,N,N]
    pub cp: Vec<f32>,       // physical pressure coefficient, [N,N,N]
    pub traction: Vec<f32>, // [3,N,N,N], physical Pa on diffuse surface
    pub horizon: u32,
    pub reynolds: f32,
    pub characteristic_length_solver: f32,
    pub solver_dt: f32,
    pub solver_stride: u32,
    pub warmup_steps: u32,
    pub dt_frame: f32,
    pub force_coefficients: [f32; 3],
    pub moment_coefficients: [f32; 3],
    pub force_newtons: [f32; 3],
    pub moment_newton_meters: [f32; 3],
    pub surface_area_m2: f32,
    pub pressure_force_fraction: f32,
    pub load_hotspot: [f32; 3],
    pub suction_hotspot: [f32; 3],
    pub divergence_rms: f32,
    pub wake_deficit_peak: f32,
    pub wake_deficit_mean: f32,
    pub moment_origin_mode: String,
    pub moment_origin_solver: [f32; 3],
    pub surface_centroid_solver: [f32; 3],
    pub coefficient_reference: String,
    pub reference_area_m2: Option<f32>,
    /// Unused on the engineering CAD path (sandbox-only concept). Kept optional
    /// so older engine metadata still parses.
    pub semigroup: Option<f32>,
    pub load_method: String,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BenchSeedProvenance {
    pub seed: u32,
    pub stream: String,
    pub overlap: bool,
}

/// Metadata-backed leak/provenance findings. `verdict` is `clean`, `flagged`,
/// or `unknown`; missing legacy metadata never silently becomes clean.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BenchProvenance {
    pub verdict: String,
    pub training_seed: Option<i64>,
    pub mixed_fork_seed: Option<i64>,
    pub mixed_fork_used: bool,
    pub validation_seed: Option<i64>,
    pub dataset: String,
    pub benchmark_seeds: Vec<BenchSeedProvenance>,
    pub overlap_count: u32,
    pub overlap_pct: f32,
    pub epoch: Option<u32>,
    pub declared_epochs: Option<u32>,
    pub checkpoint_role: String,
    pub final_epoch_status: String,
    pub selection_metric: String,
    pub selection_stream: String,
    pub source_fingerprint_present: bool,
    pub source_fingerprint_digest: Option<String>,
    pub legacy_unknown: Vec<String>,
    pub flags: Vec<String>,
}

/// N5 suite result: RelL2 + persistence per exact (seed, horizon), plus the
/// checkpoint-derived provenance verdict for those seeds.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BenchResult {
    pub model: String,
    pub seeds: Vec<u32>,
    pub horizons: Vec<u32>,
    pub rel: Vec<Vec<f32>>,
    pub persist: Vec<Vec<f32>>,
    pub global_rel: f32,
    pub runtime_s: f32,
    pub grid: u32,
    pub epoch: u32,
    pub dt_frame: f32,
    pub provenance: BenchProvenance,
}

/// On-demand evidence for one selected benchmark cell. `maps` carries
/// velocity/vorticity/recovered-pressure/divergence model, solver-reference,
/// and spatial-error maps.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BenchInspector {
    pub seed: u32,
    pub horizon: u32,
    pub n: usize,
    pub maps: InspectorMaps,
    pub seed_stream: String,
    pub provenance_status: String,
    pub rel_l2: f32,
    pub persist_rel_l2: f32,
    pub improvement_ratio: f32,
    pub mean_abs_error: f32,
    pub p95_abs_error: f32,
    pub max_abs_error: f32,
    pub divergence_model_rms: f32,
    pub divergence_truth_rms: f32,
    pub divergence_error_rms: f32,
    pub spectrum_rel_l2: f32,
    pub spectrum_k: Vec<f32>,
    pub spectrum_model: Vec<f32>,
    pub spectrum_truth: Vec<f32>,
}

/// Honest stage progress for an in-flight `predict_cad` request.
///
/// `fraction` is stage-based (ordinal + within-stage local progress), not a
/// wall-clock percentage. UI copy must say so.
#[derive(Clone, Debug, PartialEq)]
pub struct CadProgress {
    pub request_id: String,
    pub stage: String,
    pub detail: String,
    pub stage_index: u32,
    pub stage_count: u32,
    pub fraction: f32,
}

pub enum Msg {
    Correlated {
        request: RequestContext,
        response: Box<Msg>,
    },
    Status(String),
    Models(Vec<ModelCard>),
    ModelImported {
        model: ModelCard,
        models: Vec<ModelCard>,
    },
    ModelImportRejected(ModelValidation),
    ModelDeleted {
        model: String,
        models: Vec<ModelCard>,
    },
    Field(Field),
    Field2D(Field2D),
    CadField(CadField),
    CadProgress(CadProgress),
    Benchmark(BenchResult),
    BenchmarkInspector(BenchInspector),
    Error(String),
}

fn guard_model_request(
    research_dir: &Path,
    model: &str,
    request: impl FnOnce() -> std::io::Result<Msg>,
) -> std::io::Result<Msg> {
    if let Err(error) = require_model_bundle_id(model) {
        return Ok(Msg::Error(error.to_string()));
    }
    // Local trusted-checkpoint escape hatch. The Python sidecar still enforces
    // development_allow_unsigned itself when the request reaches it.
    if development_unsigned_models_allowed() {
        return request();
    }
    #[cfg(test)]
    if research_dir
        .join(DEVELOPMENT_UNSIGNED_FIXTURE_MARKER)
        .is_file()
    {
        return request();
    }
    let bundle = Path::new(model.trim());
    let bundle = if bundle.is_absolute() {
        bundle.to_path_buf()
    } else {
        research_dir.join(bundle)
    };
    match require_model_signature(&bundle).map(drop) {
        Ok(()) => request(),
        Err(error) => Ok(Msg::Error(error.to_string())),
    }
}

fn guard_model_format(
    model: &str,
    request: impl FnOnce() -> std::io::Result<Msg>,
) -> std::io::Result<Msg> {
    match require_model_bundle_id(model) {
        Ok(()) => request(),
        Err(error) => Ok(Msg::Error(error.to_string())),
    }
}

fn configure_request_timeouts(stream: &TcpStream, timeout: Duration) -> std::io::Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))
}

fn cancel_engine_stream(stream: &Arc<Mutex<Option<TcpStream>>>) {
    if let Ok(mut slot) = stream.lock() {
        if let Some(stream) = slot.take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

pub struct EngineHandle {
    pub tx: Sender<Cmd>,
    pub rx: Receiver<Msg>,
    worker: Option<thread::JoinHandle<()>>,
    shutdown_stream: Arc<Mutex<Option<TcpStream>>>,
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.terminate();
    }
}

impl EngineHandle {
    /// Queue a request with an identity allocated before dispatch so callers
    /// can match the eventual response to the exact pending UI operation.
    pub fn send(&self, command: Cmd) -> Result<RequestContext, std::sync::mpsc::SendError<Cmd>> {
        let request = command.request_context();
        self.tx.send(Cmd::WithContext {
            request: request.clone(),
            command: Box::new(command),
        })?;
        Ok(request)
    }

    /// Interrupt any blocking request, join the worker, and terminate the
    /// sidecar process before returning. Safe to call more than once.
    pub fn terminate(&mut self) {
        cancel_engine_stream(&self.shutdown_stream);
        let (replacement, _) = std::sync::mpsc::channel();
        drop(std::mem::replace(&mut self.tx, replacement));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn research_dir() -> String {
    if let Some(path) = nonempty_env("REYN_RESEARCH_DIR") {
        return path.to_string_lossy().into_owned();
    }
    let current_exe = std::env::current_exe().unwrap_or_default();
    let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let resources_override = nonempty_env("REYN_RESOURCES_DIR");
    default_research_dir_at(&current_exe, &current_dir, resources_override.as_deref())
        .to_string_lossy()
        .into_owned()
}

fn nonempty_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn absolute_from(path: &Path, current_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    }
}

fn bundle_resources(current_exe: &Path) -> Option<PathBuf> {
    bundle_resources_for(current_exe, std::env::consts::OS)
}

fn bundle_resources_for(current_exe: &Path, platform: &str) -> Option<PathBuf> {
    if matches!(
        platform.to_ascii_lowercase().as_str(),
        "windows" | "win32" | "win64" | "linux"
    ) {
        let directory = current_exe.parent()?;
        let resources = directory.join("resources");
        if resources.is_dir() || directory.join("release-manifest.json").is_file() {
            return Some(resources);
        }
    }
    let macos = current_exe.parent()?;
    let contents = macos.parent()?;
    if macos.file_name() == Some(OsStr::new("MacOS"))
        && contents.file_name() == Some(OsStr::new("Contents"))
    {
        Some(contents.join("Resources"))
    } else {
        None
    }
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn development_roots(current_exe: &Path, current_dir: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in current_dir.ancestors().take(5) {
        push_unique(&mut roots, root.to_path_buf());
    }
    if let Some(parent) = current_exe.parent() {
        for root in parent.ancestors().take(6) {
            push_unique(&mut roots, root.to_path_buf());
        }
    }
    roots
}

fn resource_root_at(
    current_exe: &Path,
    current_dir: &Path,
    resources_override: Option<&Path>,
) -> Option<PathBuf> {
    resources_override
        .map(|path| absolute_from(path, current_dir))
        .or_else(|| bundle_resources(current_exe))
}

fn default_research_dir_at(
    current_exe: &Path,
    current_dir: &Path,
    resources_override: Option<&Path>,
) -> PathBuf {
    if let Some(resources) = resource_root_at(current_exe, current_dir, resources_override) {
        return resources.join("research");
    }
    for root in development_roots(current_exe, current_dir) {
        let candidate = root.join("reyn-research");
        if candidate.is_dir() {
            return candidate;
        }
    }
    current_dir
        .parent()
        .unwrap_or(current_dir)
        .join("reyn-research")
}

fn resolve_engine_script_at(
    current_exe: &Path,
    current_dir: &Path,
    resources_override: Option<&Path>,
    engine_override: Option<&Path>,
) -> std::io::Result<PathBuf> {
    if let Some(override_path) = engine_override {
        let candidate = absolute_from(override_path, current_dir);
        return if candidate.is_file() {
            Ok(candidate)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "REYN_ENGINE_SCRIPT points to a missing sidecar: {}",
                    candidate.display()
                ),
            ))
        };
    }

    let mut candidates = Vec::new();
    if let Some(resources) = resource_root_at(current_exe, current_dir, resources_override) {
        push_unique(&mut candidates, resources.join(ENGINE_ENTRYPOINT));
    }
    // A bundle must either use its own Resources directory or an explicit
    // override. Never hide an incomplete package by finding a developer checkout.
    if bundle_resources(current_exe).is_none() && resources_override.is_none() {
        for root in development_roots(current_exe, current_dir) {
            push_unique(&mut candidates, root.join(ENGINE_ENTRYPOINT));
        }
    }
    if let Some(path) = candidates.iter().find(|path| path.is_file()) {
        return Ok(path.clone());
    }
    let searched = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!(
            "Python sidecar is missing; set REYN_ENGINE_SCRIPT or REYN_RESOURCES_DIR. \
             Searched: {}",
            if searched.is_empty() {
                "(no candidate paths)".to_string()
            } else {
                searched
            }
        ),
    ))
}

fn resolve_existing_dir(path: &Path, current_dir: &Path, source: &str) -> std::io::Result<PathBuf> {
    let candidate = absolute_from(path, current_dir);
    if candidate.is_dir() {
        Ok(candidate)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "{source} points to a missing directory: {}",
                candidate.display()
            ),
        ))
    }
}

fn resolve_python_at(program: &Path, current_dir: &Path) -> std::io::Result<PathBuf> {
    let has_path_component = program.is_absolute() || program.components().count() > 1;
    if has_path_component {
        let candidate = absolute_from(program, current_dir);
        return if candidate.is_file() {
            Ok(candidate)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "[runtime.missing] Python interpreter is missing: {}. \
                     set REYN_PYTHON to a compatible interpreter.",
                    candidate.display()
                ),
            ))
        };
    }
    if let Some(path) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(program))
            .find(|candidate| candidate.is_file())
    }) {
        return Ok(path);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!(
            "[runtime.missing] Python interpreter '{}' was not found on PATH. \
             set REYN_PYTHON to a compatible interpreter.",
            program.display()
        ),
    ))
}

fn missing_research_modules(research_dir: &Path) -> Vec<&'static str> {
    REQUIRED_RESEARCH_MODULES
        .iter()
        .copied()
        .filter(|name| !research_dir.join(name).is_file())
        .collect()
}

fn model_bundle_count(research_dir: &Path) -> usize {
    [research_dir.to_path_buf(), research_dir.join("reyn_models")]
        .into_iter()
        .filter_map(|directory| fs::read_dir(directory).ok())
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter(|entry| {
            entry.path().is_file()
                && entry.path().extension().and_then(OsStr::to_str) == Some(MODEL_BUNDLE_EXTENSION)
                && model_signature_path(entry.path()).is_file()
        })
        .count()
}

fn validate_python_dependencies(python: &Path) -> std::io::Result<()> {
    let result = Command::new(python)
        .args([
            "-B",
            "-c",
            "import numpy, torch; print(numpy.__version__); print(torch.__version__)",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "failed to launch Python interpreter {}: {error}",
                    python.display()
                ),
            )
        })?;
    if result.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&result.stderr).trim().to_string();
    Err(std::io::Error::other(format!(
        "[runtime.dependencies] Python interpreter {} cannot import required dependencies \
         numpy and torch: {}. Set REYN_PYTHON to a compatible interpreter.",
        python.display(),
        if detail.is_empty() {
            "dependency probe failed"
        } else {
            &detail
        }
    )))
}

#[derive(Debug)]
struct RuntimePaths {
    python: PathBuf,
    script: PathBuf,
    research_dir: PathBuf,
    model_bundle_count: usize,
    runtime_status: String,
    health: Option<runtime::RuntimeHealthContext>,
}

fn resolve_runtime(config: &EngineConfig) -> std::io::Result<RuntimePaths> {
    let current_exe = std::env::current_exe()?;
    let current_dir = std::env::current_dir()?;
    let resources_override = nonempty_env("REYN_RESOURCES_DIR");
    let environment_engine_override = nonempty_env("REYN_ENGINE_SCRIPT");
    #[cfg(test)]
    let configured_engine_override = config.engine_script.as_deref().map(Path::new);
    #[cfg(not(test))]
    let configured_engine_override: Option<&Path> = None;
    let engine_override = configured_engine_override.or(environment_engine_override.as_deref());
    let script = resolve_engine_script_at(
        &current_exe,
        &current_dir,
        resources_override.as_deref(),
        engine_override,
    )?;

    let research_override = nonempty_env("REYN_RESEARCH_DIR");
    let configured_research = PathBuf::from(&config.research_dir);
    let resource_research = resources_override
        .as_deref()
        .map(|path| absolute_from(path, &current_dir).join("research"));
    let selected_research = research_override
        .as_deref()
        .or(resource_research.as_deref())
        .unwrap_or(configured_research.as_path());
    let research_dir = resolve_existing_dir(
        selected_research,
        &current_dir,
        if research_override.is_some() {
            "REYN_RESEARCH_DIR"
        } else if resources_override.is_some() {
            "REYN_RESOURCES_DIR research runtime"
        } else {
            "configured research directory"
        },
    )?;
    let missing = missing_research_modules(&research_dir);
    if !missing.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "[runtime.dependencies] research runtime {} is missing required modules: {}. \
                 Set REYN_RESEARCH_DIR to a compatible runtime directory.",
                research_dir.display(),
                missing.join(", ")
            ),
        ));
    }

    let python_override = nonempty_env("REYN_PYTHON");
    let configured_python = PathBuf::from(&config.python_path);
    let factory_runtime = runtime::factory_runtime_root(&current_exe);
    let managed_runtime_eligible = factory_runtime.is_some()
        && python_override.is_none()
        && resources_override.is_none()
        && engine_override.is_none()
        && research_override.is_none();
    let (python, runtime_status, health) = if managed_runtime_eligible {
        let engine_dir = script.parent().ok_or_else(|| {
            std::io::Error::other("[runtime.dependencies] Python sidecar has no resource directory")
        })?;
        let mut closure_entries = REQUIRED_ENGINE_RESOURCES
            .iter()
            .map(|name| (format!("engine/{name}"), engine_dir.join(name)))
            .collect::<Vec<_>>();
        closure_entries.extend(
            REQUIRED_RESEARCH_MODULES
                .iter()
                .map(|name| (format!("research/{name}"), research_dir.join(name))),
        );
        let research_closure =
            runtime::research_closure_sha256(&closure_entries).map_err(std::io::Error::other)?;
        let managed_root = runtime::default_managed_runtime_root();
        let host = runtime::HostCompatibility::current();
        let discovery = runtime::discover_runtime(runtime::RuntimeDiscoveryRequest {
            factory_root: factory_runtime.as_deref(),
            managed_root: managed_root.as_deref(),
            host: &host,
            expected_research_closure_sha256: &research_closure,
        });
        let selected = discovery.selected.ok_or_else(|| {
            let detail = discovery
                .diagnostics
                .iter()
                .map(|diagnostic| format!("[{}] {}", diagnostic.code, diagnostic.detail))
                .collect::<Vec<_>>()
                .join("; ");
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                if detail.is_empty() {
                    "[runtime.missing] no managed or factory Python runtime was found".into()
                } else {
                    detail
                },
            )
        })?;
        debug_assert!(selected.python.starts_with(&selected.root));
        let health = if selected.source == runtime::RuntimeSource::ManagedActive {
            managed_root
                .as_ref()
                .map(|managed_root| runtime::RuntimeHealthContext {
                    managed_root: managed_root.clone(),
                    runtime_id: selected.manifest.runtime_id.clone(),
                })
        } else {
            None
        };
        if let Err(error) =
            runtime::smoke_verified_runtime(&selected, runtime::DEFAULT_SMOKE_TIMEOUT)
        {
            if let Some(health) = health.as_ref() {
                let _ = runtime::record_runtime_failure(
                    health,
                    runtime::RuntimeHealthFailureKind::Startup,
                    &error.to_string(),
                    runtime::current_epoch(),
                );
            }
            return Err(std::io::Error::other(error));
        }
        let fallback = discovery
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code.starts_with("runtime.fallback_"))
            .map(|diagnostic| format!(" · {}", diagnostic.code))
            .unwrap_or_default();
        let status = format!(
            "{} {} · Python {} · PyTorch {} · NumPy {}{}",
            selected.source.label(),
            selected.manifest.runtime_id,
            selected.manifest.python,
            selected.manifest.torch,
            selected.manifest.numpy,
            fallback
        );
        (selected.python, status, health)
    } else {
        let python = resolve_python_at(
            python_override
                .as_deref()
                .unwrap_or(configured_python.as_path()),
            &current_dir,
        )?;
        validate_python_dependencies(&python)?;
        let source = if python_override.is_some() {
            "REYN_PYTHON developer override"
        } else {
            "local development Python"
        };
        (python, source.into(), None)
    };
    Ok(RuntimePaths {
        python,
        script,
        model_bundle_count: model_bundle_count(&research_dir),
        research_dir,
        runtime_status,
        health,
    })
}

impl EngineHandle {
    /// Spawn the engine + worker thread. Returns immediately; readiness/errors
    /// arrive as `Msg::Status` / `Msg::Error`.
    #[allow(dead_code)] // retained for engine integration tests and embedders
    pub fn spawn() -> EngineHandle {
        Self::spawn_with_config(EngineConfig::default())
    }

    pub fn spawn_with_config(config: EngineConfig) -> EngineHandle {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
        let (msg_tx, msg_rx) = std::sync::mpsc::channel::<Msg>();
        let shutdown_stream = Arc::new(Mutex::new(None));
        let worker_shutdown_stream = Arc::clone(&shutdown_stream);
        let worker = thread::spawn(move || worker(cmd_rx, msg_tx, config, worker_shutdown_stream));
        EngineHandle {
            tx: cmd_tx,
            rx: msg_rx,
            worker: Some(worker),
            shutdown_stream,
        }
    }
}

/// Human-readable compute device label for status chips ("mps" → "Apple GPU (MPS)").
fn device_label(device: &str) -> String {
    match device {
        "mps" => "Apple GPU (MPS)".into(),
        "cuda" => "NVIDIA GPU (CUDA)".into(),
        "cpu" => "CPU".into(),
        other => other.into(),
    }
}

fn worker(
    cmd_rx: Receiver<Cmd>,
    msg_tx: Sender<Msg>,
    config: EngineConfig,
    shutdown_stream: Arc<Mutex<Option<TcpStream>>>,
) {
    let mut conn = match start(&config) {
        Ok((stream, child, device, model_bundles, runtime_status, health)) => {
            if let Ok(cancel_stream) = stream.try_clone() {
                if let Ok(mut slot) = shutdown_stream.lock() {
                    *slot = Some(cancel_stream);
                }
            }
            let model_status = if model_bundles == 0 {
                " · [runtime.models_missing] no .reynmodel + .sig pairs found"
            } else {
                ""
            };
            let _ = msg_tx.send(Msg::Status(format!(
                "● Engine ready · {} · {}{}",
                device_label(&device),
                runtime_status,
                model_status,
            )));
            if let Some(health) = health.as_ref() {
                let _ = runtime::record_runtime_startup_success(health, runtime::current_epoch());
            }
            (stream, health, child)
        }
        Err(e) => {
            let _ = msg_tx.send(Msg::Error(format!("engine unavailable: {e}")));
            return;
        }
    };
    let (ref mut conn, ref health, ref mut child) = conn;
    let mut recorded_runtime_success = false;
    while let Ok(cmd) = cmd_rx.recv() {
        let (request_context, cmd) = cmd.into_request();
        let res = match cmd {
            Cmd::WithContext { .. } => Err(std::io::Error::other("nested engine request context")),
            Cmd::ListModels => request(conn, r#"{"op":"list_models"}"#.into(), &[])
                .map(|(j, _)| Msg::Models(parse_model_cards(&j["models"]))),
            Cmd::ImportModel { path } => {
                if !is_model_bundle_id(&path) {
                    let validation = model_format_rejection(&path);
                    Ok(Msg::ModelImportRejected(validation))
                } else if let Err(error) = require_model_signature(&path) {
                    let validation = model_signature_rejection(&path, &error);
                    Ok(Msg::ModelImportRejected(validation))
                } else {
                    let req = serde_json::json!({"op": "import_model", "path": path}).to_string();
                    request(conn, req, &[]).map(|(j, _)| {
                        if !j["ok"].as_bool().unwrap_or(false) {
                            if let Some(validation) = parse_model_validation(&j["validation"]) {
                                return Msg::ModelImportRejected(validation);
                            }
                            return Msg::Error(
                                j["error"].as_str().unwrap_or("model import failed").into(),
                            );
                        }
                        let Some(model) = parse_model_card(&j["imported"]) else {
                            return Msg::Error("model import returned malformed metadata".into());
                        };
                        Msg::ModelImported {
                            model,
                            models: parse_model_cards(&j["models"]),
                        }
                    })
                }
            }
            Cmd::DeleteModel { model } => {
                let req = serde_json::json!({"op": "delete_model", "model": model}).to_string();
                guard_model_format(&model, || {
                    request(conn, req, &[]).map(|(j, _)| {
                        if !j["ok"].as_bool().unwrap_or(false) {
                            return Msg::Error(
                                j["error"].as_str().unwrap_or("model delete failed").into(),
                            );
                        }
                        Msg::ModelDeleted {
                            model: j["deleted"].as_str().unwrap_or("").into(),
                            models: parse_model_cards(&j["models"]),
                        }
                    })
                })
            }
            Cmd::Predict { model, seed } => {
                let req = serde_json::json!({
                    "op": "predict_field",
                    "model": model,
                    "seed": seed,
                })
                .to_string();
                guard_model_request(Path::new(&config.research_dir), &model, || {
                    request(conn, req, &[]).map(|(j, payload)| {
                        if !j["ok"].as_bool().unwrap_or(false) {
                            return Msg::Error(
                                j["error"].as_str().unwrap_or("predict failed").into(),
                            );
                        }
                        let shape: Vec<usize> = j["shape"]
                            .as_array()
                            .unwrap_or(&vec![])
                            .iter()
                            .filter_map(|v| v.as_u64().map(|n| n as usize))
                            .collect();
                        let data: Vec<f32> = payload
                            .chunks_exact(4)
                            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                            .collect();
                        Msg::Field(Field {
                            shape,
                            data,
                            scenario: j["scenario"].as_str().unwrap_or("").into(),
                        })
                    })
                })
            }
            Cmd::Predict2D {
                model,
                steps,
                seed,
                want_truth,
                method,
                tolerance,
                boundary,
            } => {
                let req = serde_json::json!({
                    "op": "predict2d",
                    "model": model,
                    "steps": steps,
                    "seed": seed,
                    "want_truth": want_truth,
                    "method": method,
                    "tolerance": tolerance,
                    "boundary": boundary,
                    "max_iter": 600,
                })
                .to_string();
                guard_model_request(Path::new(&config.research_dir), &model, || {
                    request(conn, req, &[]).map(|(j, payload)| parse_field2d(&j, &payload))
                })
            }
            Cmd::PredictIC { model, steps, ic } => {
                let req = serde_json::json!({"op": "predict_ic", "model": model, "steps": steps})
                    .to_string();
                let bytes: Vec<u8> = ic.iter().flat_map(|v| v.to_le_bytes()).collect();
                guard_model_request(Path::new(&config.research_dir), &model, || {
                    request(conn, req, &bytes).map(|(j, payload)| parse_field2d(&j, &payload))
                })
            }
            Cmd::CadPredict {
                request_id,
                model,
                steps,
                mask,
                reynolds,
                characteristic_length_solver,
                reference_length_m,
                velocity_mps,
                density_kg_m3,
                reference_pressure_pa,
                moment_origin_mode,
                moment_origin_solver,
                reference_area_m2,
            } => {
                let mut req = serde_json::json!({
                    "op": "predict_cad",
                    "request_id": request_id,
                    "model": model,
                    "steps": steps,
                    "reynolds": reynolds,
                    "char_len": characteristic_length_solver,
                    "reference_length_m": reference_length_m,
                    "velocity_mps": velocity_mps,
                    "density_kg_m3": density_kg_m3,
                    "reference_pressure_pa": reference_pressure_pa,
                    "moment_origin_mode": moment_origin_mode,
                });
                if let Some(origin) = moment_origin_solver {
                    req["moment_origin_solver"] =
                        serde_json::json!([origin[0], origin[1], origin[2]]);
                }
                if let Some(area) = reference_area_m2 {
                    req["reference_area_m2"] = serde_json::json!(area);
                }
                let req = req.to_string();
                let bytes: Vec<u8> = mask.iter().flat_map(|v| v.to_le_bytes()).collect();
                let progress_tx = msg_tx.clone();
                let progress_request = request_context.clone();
                guard_model_request(Path::new(&config.research_dir), &model, || {
                    // Widen only for this CAD exchange; restore the default bound after.
                    configure_request_timeouts(conn, ENGINE_CAD_REQUEST_TIMEOUT)?;
                    let outcome = request_with_progress(conn, req, &bytes, |progress| {
                        let _ = progress_tx.send(Msg::Correlated {
                            request: progress_request.clone(),
                            response: Box::new(Msg::CadProgress(progress)),
                        });
                    })
                    .map(|(j, payload)| parse_cad_field(&j, &payload));
                    let _ = configure_request_timeouts(conn, ENGINE_REQUEST_TIMEOUT);
                    outcome
                })
            }
            Cmd::RunBenchmark {
                model,
                seeds,
                horizons,
            } => {
                let req = serde_json::json!({
                    "op": "run_benchmark",
                    "model": model,
                    "seeds": seeds,
                    "horizons": horizons,
                })
                .to_string();
                guard_model_request(Path::new(&config.research_dir), &model, || {
                    request(conn, req, &[]).map(|(j, _)| parse_benchmark(&j, model.clone()))
                })
            }
            Cmd::InspectBenchmarkCell {
                model,
                seed,
                horizon,
            } => {
                let req = serde_json::json!({
                    "op": "inspect_benchmark_cell",
                    "model": model,
                    "seed": seed,
                    "horizon": horizon,
                    "evidence_schema": INSPECTOR_SCHEMA,
                })
                .to_string();
                guard_model_request(Path::new(&config.research_dir), &model, || {
                    request(conn, req, &[])
                        .map(|(j, payload)| parse_benchmark_inspector(&j, &payload))
                })
            }
        };
        if res.is_ok() && !recorded_runtime_success {
            if let Some(health) = health.as_ref() {
                recorded_runtime_success =
                    runtime::record_runtime_request_success(health, runtime::current_epoch())
                        .is_ok();
            }
        }
        let crash = res.as_ref().err().map(ToString::to_string);
        let response = res.unwrap_or_else(|error| {
            let prefix = if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                "engine timeout"
            } else {
                "engine io"
            };
            Msg::Error(format!("{prefix}: {error}"))
        });
        let _ = msg_tx.send(Msg::Correlated {
            request: request_context,
            response: Box::new(response),
        });
        if let Some(detail) = crash {
            if let Some(health) = health.as_ref() {
                let _ = runtime::record_runtime_failure(
                    health,
                    runtime::RuntimeHealthFailureKind::Crash,
                    &detail,
                    runtime::current_epoch(),
                );
            }
            break;
        }
    }
    let _ = conn.shutdown(std::net::Shutdown::Both);
    cancel_engine_stream(&shutdown_stream);
    for _ in 0..20 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn parse_model_card(value: &serde_json::Value) -> Option<ModelCard> {
    let mut card = ModelCard {
        id: value["id"].as_str()?.into(),
        name: value["name"].as_str()?.into(),
        managed: value["managed"].as_bool().unwrap_or(false),
        size_bytes: value["size_bytes"].as_u64().unwrap_or(0),
        modified_unix: value["modified_unix"].as_u64().unwrap_or(0),
        checkpoint_sha256: value["checkpoint_sha256"].as_str().unwrap_or("").into(),
        status: value["status"].as_str().unwrap_or("invalid").into(),
        status_detail: value["status_detail"].as_str().unwrap_or("").into(),
        dimension: value["dimension"].as_u64().unwrap_or(0) as u32,
        grid: value["grid"].as_u64().unwrap_or(0) as u32,
        in_channels: value["in_channels"].as_u64().unwrap_or(0) as u32,
        out_channels: value["out_channels"].as_u64().unwrap_or(0) as u32,
        max_steps: value["max_steps"].as_u64().unwrap_or(0) as u32,
        epoch: value["epoch"].as_u64().unwrap_or(0) as u32,
        declared_epochs: value["declared_epochs"].as_u64().unwrap_or(0) as u32,
        checkpoint_role: value["checkpoint_role"]
            .as_str()
            .unwrap_or("unknown")
            .into(),
        scenario: value["scenario"].as_str().unwrap_or("unknown").into(),
        source_digest: value["source_digest"].as_str().map(str::to_owned),
        physics_contract: value["physics_contract"]
            .as_str()
            .unwrap_or("unknown")
            .into(),
        authenticity_status: value["authenticity_status"]
            .as_str()
            .unwrap_or("unverified")
            .into(),
        publisher_key_id: value["publisher_key_id"].as_str().map(str::to_owned),
        publisher_key_sha256: value["publisher_key_sha256"].as_str().map(str::to_owned),
        release_sequence: value["release_sequence"].as_u64(),
        support: json_strings(&value["support"]),
        limitations: json_strings(&value["limitations"]),
        benchmark_report_hashes: json_strings(&value["benchmark_report_hashes"]),
        unknown_fields: json_strings(&value["unknown_fields"]),
        qualification_class: String::new(),
    };
    card.qualification_class = value["qualification_class"]
        .as_str()
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            derive_model_qualification_class(
                // Prefer the human-facing version token when present in name/id.
                value["version"]
                    .as_str()
                    .or_else(|| value["model_version"].as_str())
                    .unwrap_or(card.name.as_str()),
                &card.limitations,
                &card.status,
            )
        });
    if !is_model_bundle_id(&card.id) || !is_model_bundle_id(&card.name) {
        card.status = "invalid".into();
        card.status_detail = TRUSTED_MODEL_CONVERSION_GUIDANCE.into();
        card.qualification_class = MODEL_QUALIFICATION_UNKNOWN.into();
    }
    Some(card)
}

/// Validate the packaged YC preview release manifest schema (fail closed).
pub fn validate_yc_preview_model_release(manifest: &serde_json::Value) -> Result<(), String> {
    let schema = manifest
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if schema != YC_PREVIEW_MODEL_RELEASE_SCHEMA {
        return Err(format!(
            "preview model release schema is {schema:?}, expected {YC_PREVIEW_MODEL_RELEASE_SCHEMA}"
        ));
    }
    let boundary = manifest
        .get("qualification_boundary")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim();
    if boundary.is_empty() {
        return Err("preview model release omits qualification_boundary".into());
    }
    if !boundary.to_ascii_lowercase().contains("incomplete")
        && !boundary.to_ascii_lowercase().contains("preview")
    {
        return Err(
            "preview model qualification_boundary must state incomplete/preview status".into(),
        );
    }
    let version = manifest
        .pointer("/model/version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if derive_model_qualification_class(version, &[], "clean") != MODEL_QUALIFICATION_PREVIEW {
        return Err(format!(
            "preview model version {version:?} is not classified as preview"
        ));
    }
    for key in ["bundle_sha256", "weights_sha256", "tuf_root_sha256"] {
        let digest = manifest
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(format!(
                "preview model release {key} is missing or non-canonical"
            ));
        }
    }
    Ok(())
}

fn parse_model_validation(value: &serde_json::Value) -> Option<ModelValidation> {
    let issues = value["issues"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|issue| {
                    Some(ModelValidationIssue {
                        code: issue["code"].as_str()?.into(),
                        field: issue["field"].as_str().unwrap_or("checkpoint").into(),
                        message: issue["message"].as_str()?.into(),
                        severity: issue["severity"].as_str().unwrap_or("error").into(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ModelValidation {
        accepted: value["accepted"].as_bool()?,
        status: value["status"].as_str()?.into(),
        summary: value["summary"].as_str()?.into(),
        issues,
        candidate_name: value["candidate"]["name"].as_str().map(str::to_owned),
        candidate_sha256: value["candidate"]["checkpoint_sha256"]
            .as_str()
            .map(str::to_owned),
    })
}

fn parse_model_cards(value: &serde_json::Value) -> Vec<ModelCard> {
    value
        .as_array()
        .map(|cards| cards.iter().filter_map(parse_model_card).collect())
        .unwrap_or_default()
}

/// Parse a `[3,N,N]` (+ optional truth) field response into `Msg::Field2D` —
/// shared by `predict2d` and `predict_ic` (identical response shape).
fn parse_field2d(j: &serde_json::Value, payload: &[u8]) -> Msg {
    if !j["ok"].as_bool().unwrap_or(false) {
        return Msg::Error(j["error"].as_str().unwrap_or("predict failed").into());
    }
    let n = j["shape"][1].as_u64().unwrap_or(0) as usize;
    let all: Vec<f32> = payload
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let plane = n * n;
    let ai = all
        .get(..3 * plane)
        .map(<[f32]>::to_vec)
        .unwrap_or_default();
    let has_truth = j["has_truth"].as_bool().unwrap_or(false);
    let truth = if has_truth && all.len() >= 6 * plane {
        Some(all[3 * plane..6 * plane].to_vec())
    } else {
        None
    };
    let f = |k: &str| j[k].as_f64().map(|v| v as f32);
    Msg::Field2D(Field2D {
        n,
        ai,
        truth,
        horizon: j["horizon"].as_u64().unwrap_or(0) as u32,
        dt_frame: f("dt_frame").unwrap_or(0.04),
        peak_p: f("peak_p").unwrap_or(0.0),
        low_p: f("low_p").unwrap_or(0.0),
        semigroup: f("semigroup"),
        rel_l2: f("rel_l2"),
        persist: f("persist"),
        p_residual: f("p_residual").unwrap_or(0.0),
        p_iters: j["p_iters"].as_u64().unwrap_or(0) as u32,
        p_method: j["method"].as_str().unwrap_or("spectral").into(),
        scenario: j["scenario"].as_str().unwrap_or("").into(),
    })
}

fn json_u32s(value: &serde_json::Value) -> Vec<u32> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_u64().map(|number| number as u32))
                .collect()
        })
        .unwrap_or_default()
}

fn json_f32s(value: &serde_json::Value) -> Vec<f32> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_f64().map(|number| number as f32))
                .collect()
        })
        .unwrap_or_default()
}

fn json_strings(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn json_matrix(value: &serde_json::Value) -> Vec<Vec<f32>> {
    value
        .as_array()
        .map(|rows| rows.iter().map(json_f32s).collect())
        .unwrap_or_default()
}

fn parse_provenance(value: &serde_json::Value) -> BenchProvenance {
    let benchmark_seeds = value["benchmark_seeds"]
        .as_array()
        .map(|records| {
            records
                .iter()
                .map(|record| BenchSeedProvenance {
                    seed: record["seed"].as_u64().unwrap_or(0) as u32,
                    stream: record["stream"].as_str().unwrap_or("unknown").into(),
                    overlap: record["overlap"].as_bool().unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default();
    BenchProvenance {
        verdict: value["verdict"].as_str().unwrap_or("unknown").into(),
        training_seed: value["training_seed"].as_i64(),
        mixed_fork_seed: value["mixed_fork_seed"].as_i64(),
        mixed_fork_used: value["mixed_fork_used"].as_bool().unwrap_or(false),
        validation_seed: value["validation_seed"].as_i64(),
        dataset: value["dataset"].as_str().unwrap_or("legacy/unknown").into(),
        benchmark_seeds,
        overlap_count: value["overlap_count"].as_u64().unwrap_or(0) as u32,
        overlap_pct: value["overlap_pct"].as_f64().unwrap_or(0.0) as f32,
        epoch: value["epoch"].as_u64().map(|number| number as u32),
        declared_epochs: value["declared_epochs"]
            .as_u64()
            .map(|number| number as u32),
        checkpoint_role: value["checkpoint_role"]
            .as_str()
            .unwrap_or("legacy/unknown")
            .into(),
        final_epoch_status: value["final_epoch_status"]
            .as_str()
            .unwrap_or("unknown")
            .into(),
        selection_metric: value["selection_metric"]
            .as_str()
            .unwrap_or("legacy/unknown")
            .into(),
        selection_stream: value["selection_stream"]
            .as_str()
            .unwrap_or("unknown")
            .into(),
        source_fingerprint_present: value["source_fingerprint_present"]
            .as_bool()
            .unwrap_or(false),
        source_fingerprint_digest: value["source_fingerprint_digest"]
            .as_str()
            .map(str::to_owned),
        legacy_unknown: json_strings(&value["legacy_unknown"]),
        flags: json_strings(&value["flags"]),
    }
}

fn parse_benchmark(j: &serde_json::Value, model: String) -> Msg {
    if !j["ok"].as_bool().unwrap_or(false) {
        return Msg::Error(j["error"].as_str().unwrap_or("benchmark failed").into());
    }
    let seeds = json_u32s(&j["seeds"]);
    let horizons = json_u32s(&j["horizons"]);
    let rel = json_matrix(&j["rel"]);
    let persist = json_matrix(&j["persist"]);
    let valid_shape = !seeds.is_empty()
        && !horizons.is_empty()
        && rel.len() == seeds.len()
        && persist.len() == seeds.len()
        && rel.iter().all(|row| row.len() == horizons.len())
        && persist.iter().all(|row| row.len() == horizons.len());
    if !valid_shape {
        return Msg::Error("malformed benchmark matrix".into());
    }
    Msg::Benchmark(BenchResult {
        model,
        seeds,
        horizons,
        rel,
        persist,
        global_rel: j["global_rel"].as_f64().unwrap_or(0.0) as f32,
        runtime_s: j["runtime_s"].as_f64().unwrap_or(0.0) as f32,
        grid: j["grid"].as_u64().unwrap_or(0) as u32,
        epoch: j["epoch"].as_u64().unwrap_or(0) as u32,
        dt_frame: j["dt_frame"].as_f64().unwrap_or(0.0) as f32,
        provenance: parse_provenance(&j["provenance"]),
    })
}

fn parse_benchmark_inspector(j: &serde_json::Value, payload: &[u8]) -> Msg {
    if !j["ok"].as_bool().unwrap_or(false) {
        return Msg::Error(
            j["error"]
                .as_str()
                .unwrap_or("benchmark inspector failed")
                .into(),
        );
    }
    let shape: Vec<usize> = j["shape"]
        .as_array()
        .map(|dims| {
            dims.iter()
                .filter_map(|dim| dim.as_u64().map(|value| value as usize))
                .collect()
        })
        .unwrap_or_default();
    if shape.len() != 4
        || shape[0] != InspectorVariable::ALL.len()
        || shape[1] != 3
        || shape[2] == 0
        || shape[2] != shape[3]
        || !payload.len().is_multiple_of(4)
        || j["protocol_version"].as_u64() != Some(INSPECTOR_PROTOCOL_VERSION)
        || j["layout"].as_str() != Some(INSPECTOR_LAYOUT)
        || j["domain"].as_str() != Some(INSPECTOR_DOMAIN)
        || j["derivative"].as_str() != Some(INSPECTOR_DERIVATIVE)
        || j["pressure"].as_str() != Some(INSPECTOR_PRESSURE)
    {
        return Msg::Error("malformed benchmark inspector payload".into());
    }
    let n = shape[2];
    let all: Vec<f32> = payload
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .collect();
    let variables = json_strings(&j["variables"]);
    let units = json_strings(&j["units"]);
    let expected_units: Vec<_> = InspectorVariable::ALL
        .iter()
        .map(|variable| variable.unit_key().to_owned())
        .collect();
    let panel_sources: Vec<Vec<String>> = j["panel_sources"]
        .as_array()
        .map(|rows| rows.iter().map(json_strings).collect())
        .unwrap_or_default();
    let expected_panel_sources: Vec<Vec<String>> = InspectorVariable::ALL
        .iter()
        .map(|variable| {
            vec![
                variable.model_source().to_owned(),
                variable.reference_source().to_owned(),
                "DERIVED".to_owned(),
            ]
        })
        .collect();
    if units != expected_units || panel_sources != expected_panel_sources {
        return Msg::Error("malformed benchmark inspector source/unit metadata".into());
    }
    let signed: Vec<bool> = j["signed"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_bool)
                .collect()
        })
        .unwrap_or_default();
    let maps = match InspectorMaps::from_protocol(
        j["schema"].as_str().unwrap_or(""),
        n,
        &variables,
        &signed,
        &all,
    ) {
        Ok(maps) => maps,
        Err(error) => return Msg::Error(format!("malformed benchmark inspector maps: {error}")),
    };
    let spectrum_k = json_f32s(&j["spectrum_k"]);
    let spectrum_model = json_f32s(&j["spectrum_model"]);
    let spectrum_truth = json_f32s(&j["spectrum_truth"]);
    if spectrum_k.is_empty()
        || spectrum_model.len() != spectrum_k.len()
        || spectrum_truth.len() != spectrum_k.len()
    {
        return Msg::Error("malformed benchmark spectrum".into());
    }
    let f = |key: &str| j[key].as_f64().unwrap_or(0.0) as f32;
    Msg::BenchmarkInspector(BenchInspector {
        seed: j["seed"].as_u64().unwrap_or(0) as u32,
        horizon: j["horizon"].as_u64().unwrap_or(0) as u32,
        n,
        maps,
        seed_stream: j["seed_stream"].as_str().unwrap_or("unknown").into(),
        provenance_status: j["provenance_status"].as_str().unwrap_or("unknown").into(),
        rel_l2: f("rel_l2"),
        persist_rel_l2: f("persist_rel_l2"),
        improvement_ratio: f("improvement_ratio"),
        mean_abs_error: f("mean_abs_error"),
        p95_abs_error: f("p95_abs_error"),
        max_abs_error: f("max_abs_error"),
        divergence_model_rms: f("divergence_model_rms"),
        divergence_truth_rms: f("divergence_truth_rms"),
        divergence_error_rms: f("divergence_error_rms"),
        spectrum_rel_l2: f("spectrum_rel_l2"),
        spectrum_k,
        spectrum_model,
        spectrum_truth,
    })
}

fn terminate_startup(
    child: &mut Child,
    stderr: &mut impl Read,
    message: impl Into<String>,
) -> std::io::Error {
    #[cfg(unix)]
    {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", &format!("-{}", child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
    let mut detail = String::new();
    let _ = stderr.take((64 * 1024) as u64).read_to_string(&mut detail);
    let detail = detail.trim();
    std::io::Error::other(if detail.is_empty() {
        message.into()
    } else {
        format!("{}; stderr: {detail}", message.into())
    })
}

fn read_startup_line(
    stdout: impl Read + Send + 'static,
    timeout: Duration,
) -> std::io::Result<String> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = sender.send(result);
    });
    receiver.recv_timeout(timeout).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "[runtime.startup_timeout] Python sidecar did not complete READY within {} seconds: {error}",
                timeout.as_secs()
            ),
        )
    })?
}

type EngineStartup = (
    TcpStream,
    Child,
    String,
    usize,
    String,
    Option<runtime::RuntimeHealthContext>,
);

fn start(config: &EngineConfig) -> std::io::Result<EngineStartup> {
    let runtime = resolve_runtime(config)?;
    let result = start_resolved(&runtime, config);
    if let Err(error) = result.as_ref() {
        if let Some(health) = runtime.health.as_ref() {
            let _ = runtime::record_runtime_failure(
                health,
                runtime::RuntimeHealthFailureKind::Startup,
                &error.to_string(),
                runtime::current_epoch(),
            );
        }
    }
    result
}

fn start_resolved(runtime: &RuntimePaths, config: &EngineConfig) -> std::io::Result<EngineStartup> {
    let device = std::env::var("REYN_DEVICE").unwrap_or_else(|_| config.device.clone());
    let mut command = Command::new(&runtime.python);
    command
        .arg("-B")
        .arg("-u")
        .arg(&runtime.script)
        .arg("--research-dir")
        .arg(&runtime.research_dir)
        .arg("--device")
        .arg(&device);
    if let Some(directory) = managed_model_directory() {
        command.arg("--managed-model-dir").arg(directory);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "failed to start Python sidecar with {}: {error}",
                runtime.python.display()
            ),
        )
    })?;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let line = read_startup_line(stdout, ENGINE_STARTUP_TIMEOUT).map_err(|error| {
        terminate_startup(
            &mut child,
            &mut stderr,
            format!("failed to read Python sidecar readiness: {error}"),
        )
    })?;
    let Some(json) = line.trim().strip_prefix("READY ") else {
        return Err(terminate_startup(
            &mut child,
            &mut stderr,
            format!(
                "bad engine startup from {}: {}; {}",
                runtime.script.display(),
                line.trim(),
                if line.trim().is_empty() {
                    "no READY output"
                } else {
                    "expected READY JSON"
                },
            ),
        ));
    };
    let ready = serde_json::from_str::<serde_json::Value>(json).map_err(|error| {
        terminate_startup(
            &mut child,
            &mut stderr,
            format!("invalid READY metadata: {error}"),
        )
    })?;
    if let Some(error) = ready["error"].as_str() {
        return Err(terminate_startup(
            &mut child,
            &mut stderr,
            format!("Python sidecar startup failed: {error}"),
        ));
    }
    let selected_device = ready["device"].as_str().unwrap_or("unknown").to_string();
    let Some(port) = ready["port"].as_u64() else {
        return Err(terminate_startup(
            &mut child,
            &mut stderr,
            "no port in READY metadata",
        ));
    };
    let stream = TcpStream::connect(("127.0.0.1", port as u16)).map_err(|error| {
        terminate_startup(
            &mut child,
            &mut stderr,
            format!("failed to connect to Python sidecar on loopback port {port}: {error}"),
        )
    })?;
    if let Err(error) = configure_request_timeouts(&stream, ENGINE_REQUEST_TIMEOUT) {
        return Err(terminate_startup(
            &mut child,
            &mut stderr,
            format!("failed to bound Python sidecar request timeouts: {error}"),
        ));
    }
    thread::spawn(move || {
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
    });
    Ok((
        stream,
        child,
        selected_device,
        runtime.model_bundle_count,
        runtime.runtime_status.clone(),
        runtime.health.clone(),
    ))
}

fn managed_model_directory() -> Option<PathBuf> {
    if let Some(directory) = std::env::var_os("REYN_MANAGED_MODEL_DIR") {
        return Some(PathBuf::from(directory));
    }
    if cfg!(target_os = "windows") {
        return std::env::var_os("LOCALAPPDATA")
            .map(|root| PathBuf::from(root).join("Reyn Studio/Models"));
    }
    if cfg!(target_os = "macos") {
        return std::env::var_os("HOME").map(|home| {
            PathBuf::from(home).join("Library/Application Support/Reyn Studio/Models")
        });
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|root| root.join("reyn-studio/models"))
}

fn read_frame(conn: &mut TcpStream) -> std::io::Result<(serde_json::Value, Vec<u8>)> {
    let mut lenb = [0u8; 4];
    conn.read_exact(&mut lenb)?;
    let total = u32::from_le_bytes(lenb) as usize;
    let mut body = vec![0u8; total];
    conn.read_exact(&mut body)?;
    let jl = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
    if jl > body.len().saturating_sub(4) {
        return Err(std::io::Error::other(
            "engine frame json length exceeds body",
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&body[4..4 + jl])
        .map_err(|e| std::io::Error::other(format!("json: {e}")))?;
    Ok((value, body[4 + jl..].to_vec()))
}

fn write_request(conn: &mut TcpStream, json: &str, payload: &[u8]) -> std::io::Result<()> {
    let jb = json.as_bytes();
    let body_len = 4 + jb.len() + payload.len();
    conn.write_all(&(body_len as u32).to_le_bytes())?;
    conn.write_all(&(jb.len() as u32).to_le_bytes())?;
    conn.write_all(jb)?;
    if !payload.is_empty() {
        conn.write_all(payload)?;
    }
    Ok(())
}

fn request(
    conn: &mut TcpStream,
    json: String,
    payload: &[u8],
) -> std::io::Result<(serde_json::Value, Vec<u8>)> {
    write_request(conn, &json, payload)?;
    read_frame(conn)
}

/// Write a request, then drain zero or more `progress: true` frames before the
/// final response. Progress callbacks must not block the engine worker.
fn request_with_progress(
    conn: &mut TcpStream,
    json: String,
    payload: &[u8],
    mut on_progress: impl FnMut(CadProgress),
) -> std::io::Result<(serde_json::Value, Vec<u8>)> {
    write_request(conn, &json, payload)?;
    loop {
        let (value, body) = read_frame(conn)?;
        if value.get("progress").and_then(|v| v.as_bool()) == Some(true) {
            if let Some(progress) = parse_cad_progress(&value) {
                on_progress(progress);
            }
            continue;
        }
        return Ok((value, body));
    }
}

pub fn parse_cad_progress(value: &serde_json::Value) -> Option<CadProgress> {
    if value.get("progress").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let stage_index = value["stage_index"].as_u64()? as u32;
    let stage_count = value["stage_count"].as_u64()? as u32;
    if stage_index == 0 || stage_count == 0 || stage_index > stage_count {
        return None;
    }
    let fraction = value["fraction"]
        .as_f64()
        .unwrap_or_else(|| (stage_index.saturating_sub(1) as f64 + 0.5) / stage_count as f64)
        as f32;
    Some(CadProgress {
        request_id: value["request_id"].as_str().unwrap_or("").into(),
        stage: value["stage"].as_str()?.into(),
        detail: value["detail"].as_str().unwrap_or("").into(),
        stage_index,
        stage_count,
        fraction: fraction.clamp(0.0, 1.0),
    })
}

fn parse_cad_field(j: &serde_json::Value, payload: &[u8]) -> Msg {
    if !j["ok"].as_bool().unwrap_or(false) {
        return Msg::Error(j["error"].as_str().unwrap_or("predict_cad failed").into());
    }
    let n = j["shape"][1].as_u64().unwrap_or(0) as usize;
    let all: Vec<f32> = payload
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let cube = n * n * n;
    if all.len() < 9 * cube {
        return Msg::Error("short CAD payload".into());
    }
    let f = |k: &str| j[k].as_f64().unwrap_or(0.0) as f32;
    let vector = |key: &str| -> [f32; 3] {
        [
            j[key][0].as_f64().unwrap_or(0.0) as f32,
            j[key][1].as_f64().unwrap_or(0.0) as f32,
            j[key][2].as_f64().unwrap_or(0.0) as f32,
        ]
    };
    Msg::CadField(CadField {
        request_id: j["request_id"].as_str().unwrap_or("").to_string(),
        n,
        vel: all[..3 * cube].to_vec(),
        pressure: all[3 * cube..4 * cube].to_vec(),
        mask: all[4 * cube..5 * cube].to_vec(),
        cp: all[5 * cube..6 * cube].to_vec(),
        traction: all[6 * cube..9 * cube].to_vec(),
        horizon: j["horizon"].as_u64().unwrap_or(0) as u32,
        reynolds: f("reynolds"),
        characteristic_length_solver: f("char_len"),
        solver_dt: f("solver_dt"),
        solver_stride: j["solver_stride"].as_u64().unwrap_or(0) as u32,
        warmup_steps: j["warmup_steps"].as_u64().unwrap_or(0) as u32,
        dt_frame: f("dt_frame"),
        force_coefficients: vector("force_coefficients"),
        moment_coefficients: vector("moment_coefficients"),
        force_newtons: vector("force_newtons"),
        moment_newton_meters: vector("moment_newton_meters"),
        surface_area_m2: f("surface_area_m2"),
        pressure_force_fraction: f("pressure_force_fraction"),
        load_hotspot: vector("load_hotspot"),
        suction_hotspot: vector("suction_hotspot"),
        divergence_rms: f("divergence_rms"),
        wake_deficit_peak: f("wake_deficit_peak"),
        wake_deficit_mean: f("wake_deficit_mean"),
        moment_origin_mode: j["moment_origin_mode"]
            .as_str()
            .unwrap_or("diffuse_surface_centroid")
            .to_string(),
        moment_origin_solver: vector("moment_origin_solver"),
        surface_centroid_solver: vector("surface_centroid_solver"),
        coefficient_reference: j["coefficient_reference"]
            .as_str()
            .unwrap_or("q_inf * L_ref^2 ; q_inf * L_ref^3")
            .to_string(),
        reference_area_m2: j["reference_area_m2"].as_f64().map(|value| value as f32),
        semigroup: j["semigroup"].as_f64().map(|value| value as f32),
        load_method: j["load_method"].as_str().unwrap_or("unknown").to_string(),
        warnings: j["warnings"]
            .as_array()
            .map(|warnings| {
                warnings
                    .iter()
                    .filter_map(|warning| warning.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    struct TempFixture {
        root: PathBuf,
    }

    impl TempFixture {
        fn new(label: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "reyn-engine-{label}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&root).expect("create fixture");
            Self { root }
        }

        fn file(&self, relative: impl AsRef<Path>) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().expect("fixture file parent"))
                .expect("create fixture parent");
            fs::write(&path, b"fixture").expect("write fixture");
            path
        }

        fn research(&self, relative: impl AsRef<Path>) -> PathBuf {
            let root = self.root.join(relative);
            fs::create_dir_all(&root).expect("create research fixture");
            for module in REQUIRED_RESEARCH_MODULES {
                fs::write(root.join(module), b"# fixture\n").expect("write research module");
            }
            root
        }
    }

    impl Drop for TempFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct VerifiedModelFixture {
        _temp: TempFixture,
        config: EngineConfig,
        model_id: String,
        grid: usize,
    }

    impl VerifiedModelFixture {
        fn new(dimension: u32) -> Self {
            assert!(matches!(dimension, 2 | 3));
            let temp = TempFixture::new(&format!("verified-{dimension}d"));
            let defaults = EngineConfig::default();
            let source_research = nonempty_env("REYN_RESEARCH_SOURCE_DIR")
                .unwrap_or_else(|| PathBuf::from(&defaults.research_dir));
            let source_python = if cfg!(target_os = "windows") {
                source_research.join(".venv/Scripts/python.exe")
            } else {
                source_research.join(".venv/bin/python")
            };
            let fixture_python = if source_python.is_file() {
                source_python.to_string_lossy().into_owned()
            } else {
                defaults.python_path.clone()
            };
            let source_engine = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("engine");
            let research = temp.root.join("research");
            let engine = temp.root.join("engine");
            fs::create_dir_all(&research).expect("create verified research fixture");
            fs::create_dir_all(&engine).expect("create verified engine fixture");
            for module in REQUIRED_ENGINE_RESOURCES {
                fs::copy(source_engine.join(module), engine.join(module))
                    .expect("copy engine module");
            }
            for entry in fs::read_dir(&source_research).expect("read research checkout") {
                let entry = entry.expect("research entry");
                let source = entry.path();
                if source.is_file() && source.extension().and_then(OsStr::to_str) == Some("py") {
                    fs::copy(&source, research.join(entry.file_name()))
                        .expect("copy research module");
                }
            }
            fs::write(
                research.join(DEVELOPMENT_UNSIGNED_FIXTURE_MARKER),
                b"test-only explicit development unsigned mode\n",
            )
            .expect("write development unsigned fixture marker");

            let model_id = format!("verified-{dimension}d.reynmodel");
            let destination = research.join(&model_id);
            let grid = 16usize;
            let script = r#"
import os
import sys
import torch
sys.path.insert(0, os.environ["REYN_FIXTURE_ENGINE_DIR"])
from model_bundle import load_model_bundle, write_model_bundle

dimension = int(os.environ["REYN_FIXTURE_DIMENSION"])
destination = os.environ["REYN_FIXTURE_DESTINATION"]
fixture_module = os.environ["REYN_FIXTURE_MODEL_BUNDLE_MODULE"]
torch.manual_seed(0)
if dimension == 2:
    from time_moe_operator import DirectFlowMap
    config = {
        "in_channels": 3,
        "out_channels": 2,
        "width": 8,
        "trunk_depth": 1,
        "time_dim": 8,
        "dt_scale": 0.01,
        "param_dim": 0,
    }
    model = DirectFlowMap(**config)
else:
    from models_3d import DirectFlowMap3D
    config = {
        "in_channels": 4,
        "out_channels": 3,
        "width": 8,
        "trunk_depth": 1,
        "time_dim": 8,
        "dt_scale": 0.01,
        "dilations": [1, 2],
        "grad_checkpoint": False,
    }
    model = DirectFlowMap3D(**config)

checkpoint = {
    "model_config": config,
    "model_state_dict": model.state_dict(),
    "train_args": {
        "dataset": "rust-verified-bundle-fixture",
        "seed": 0,
        "grid_size": 16,
        "max_steps": 8,
        "epochs": 1,
        "dt": 0.01,
        "stride": 1,
        "warmup_steps": 0,
        "nu": 0.01,
        "scenario": "obstacle",
    },
    "epoch": 1,
    "checkpoint_role": "fixed_final",
    "source_fingerprint": {
        "algorithm": "sha256",
        "digest": "b" * 64,
    },
    "limitations": ["Synthetic protocol fixture; no accuracy claim."],
}

write_model_bundle(
    checkpoint,
    destination,
    model_id=f"rust-verified-{dimension}d",
    model_version="1.0.0",
)
with open(fixture_module, "a", encoding="utf-8") as stream:
    stream.write(
        "\n# Explicit test-only unsigned loader injected by Rust VerifiedModelFixture.\n"
        "_fixture_production_load_model_bundle = load_model_bundle\n"
        "def load_model_bundle(path, *, development_allow_unsigned=False, trusted_state_dir=None):\n"
        "    return _fixture_production_load_model_bundle(\n"
        "        path,\n"
        "        development_allow_unsigned=True,\n"
        "        trusted_state_dir=trusted_state_dir,\n"
        "    )\n"
    )
loaded = load_model_bundle(destination, development_allow_unsigned=True)
assert loaded.authenticity["status"] == "development_unsigned_override"
"#;
            let output = Command::new(&fixture_python)
                .arg("-B")
                .arg("-c")
                .arg(script)
                .current_dir(&source_research)
                .env("REYN_FIXTURE_DIMENSION", dimension.to_string())
                .env("REYN_FIXTURE_DESTINATION", &destination)
                .env("REYN_FIXTURE_ENGINE_DIR", &engine)
                .env(
                    "REYN_FIXTURE_MODEL_BUNDLE_MODULE",
                    engine.join("model_bundle.py"),
                )
                .output()
                .expect("launch verified bundle builder");
            assert!(
                output.status.success(),
                "verified bundle fixture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(destination.is_file(), "bundle fixture was not written");
            assert!(
                !model_signature_path(&destination).exists(),
                "development-unsigned fixture unexpectedly produced a signature"
            );

            Self {
                _temp: temp,
                config: EngineConfig {
                    research_dir: research.to_string_lossy().into_owned(),
                    python_path: fixture_python,
                    device: "cpu".into(),
                    engine_script: Some(
                        engine.join("reyn_engine.py").to_string_lossy().into_owned(),
                    ),
                },
                model_id,
                grid,
            }
        }

        fn spawn(&self) -> EngineHandle {
            EngineHandle::spawn_with_config(self.config.clone())
        }
    }

    fn wait_for(h: &EngineHandle, pred: impl Fn(&Msg) -> bool, secs: u64) -> Option<Msg> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Ok(message) = h.rx.recv_timeout(Duration::from_millis(200)) {
                match message {
                    Msg::Correlated { response, .. } if pred(&response) => {
                        return Some(*response);
                    }
                    uncorrelated if pred(&uncorrelated) => {
                        return Some(uncorrelated);
                    }
                    _ => {}
                }
            }
        }
        None
    }

    struct DelayedEof;

    impl Read for DelayedEof {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            thread::sleep(Duration::from_millis(250));
            Ok(0)
        }
    }

    #[test]
    fn sidecar_ready_read_has_a_hard_timeout() {
        let started = Instant::now();
        let error = read_startup_line(DelayedEof, Duration::from_millis(20)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("runtime.startup_timeout"));
        assert!(started.elapsed() < Duration::from_millis(150));
    }

    #[test]
    fn sidecar_request_read_is_bounded() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(250));
        });
        let mut stream = TcpStream::connect(address).unwrap();
        configure_request_timeouts(&stream, Duration::from_millis(30)).unwrap();
        let started = Instant::now();
        let error = request(&mut stream, r#"{"op":"stall"}"#.into(), &[]).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ));
        assert!(started.elapsed() < Duration::from_millis(200));
        server.join().unwrap();
    }

    #[test]
    fn explicit_termination_interrupts_an_inflight_request_and_is_idempotent() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            let _ = stream.read_to_end(&mut bytes);
        });
        let stream = TcpStream::connect(address).unwrap();
        configure_request_timeouts(&stream, Duration::from_secs(5)).unwrap();
        let shutdown_stream = Arc::new(Mutex::new(Some(stream.try_clone().unwrap())));
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let mut stream = stream;
            let result = request(&mut stream, r#"{"op":"stall"}"#.into(), &[]);
            let _ = result_tx.send(result);
        });
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel();
        let (_msg_tx, msg_rx) = std::sync::mpsc::channel();
        let mut handle = EngineHandle {
            tx: cmd_tx,
            rx: msg_rx,
            worker: Some(worker),
            shutdown_stream,
        };
        thread::sleep(Duration::from_millis(25));
        let started = Instant::now();
        handle.terminate();
        handle.terminate();
        let result = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancelled request returns");
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(500));
        server.join().unwrap();
    }

    #[test]
    fn packaged_runtime_resolves_contents_resources() {
        let fixture = TempFixture::new("packaged");
        let executable = fixture.file("Reyn Studio.app/Contents/MacOS/reyn-studio");
        let script = fixture.file("Reyn Studio.app/Contents/Resources/engine/reyn_engine.py");
        let research = fixture.research("Reyn Studio.app/Contents/Resources/research");
        let current_dir = fixture.root.join("unrelated-working-directory");
        fs::create_dir_all(&current_dir).unwrap();

        assert_eq!(
            resolve_engine_script_at(&executable, &current_dir, None, None).unwrap(),
            script
        );
        assert_eq!(
            default_research_dir_at(&executable, &current_dir, None),
            research
        );
    }

    #[test]
    fn windows_portable_resources_resolve_beside_executable() {
        let fixture = TempFixture::new("windows-portable");
        let executable = fixture.file("portable/Reyn Studio.exe");
        let script = fixture.file("portable/resources/engine/reyn_engine.py");
        let research = fixture.research("portable/resources/research");
        let resources = bundle_resources_for(&executable, "win32").unwrap();
        assert_eq!(resources, fixture.root.join("portable/resources"));
        assert_eq!(resources.join(ENGINE_ENTRYPOINT), script);
        assert_eq!(resources.join("research"), research);
    }

    #[test]
    fn linux_portable_resources_resolve_beside_executable() {
        let fixture = TempFixture::new("linux-portable");
        let executable = fixture.file("portable/reyn-studio");
        let script = fixture.file("portable/resources/engine/reyn_engine.py");
        let research = fixture.research("portable/resources/research");
        let resources = bundle_resources_for(&executable, "linux").unwrap();
        assert_eq!(resources, fixture.root.join("portable/resources"));
        assert_eq!(resources.join(ENGINE_ENTRYPOINT), script);
        assert_eq!(resources.join("research"), research);
        assert_eq!(
            bundle_resources_for(&executable, "linux"),
            Some(fixture.root.join("portable/resources"))
        );
    }

    #[test]
    fn windows_release_marker_prevents_developer_fallback_when_resources_are_missing() {
        let fixture = TempFixture::new("windows-incomplete");
        let executable = fixture.file("portable/Reyn Studio.exe");
        fixture.file("portable/release-manifest.json");
        assert_eq!(
            bundle_resources_for(&executable, "windows"),
            Some(fixture.root.join("portable/resources"))
        );
    }

    #[test]
    fn development_runtime_resolves_without_build_tree_literal() {
        let fixture = TempFixture::new("development");
        let project = fixture.root.join("reyn-studio");
        let executable = fixture.file("reyn-studio/target/debug/reyn-studio");
        let script = fixture.file("reyn-studio/engine/reyn_engine.py");
        let research = fixture.research("reyn-research");

        assert_eq!(
            resolve_engine_script_at(&executable, &project, None, None).unwrap(),
            script
        );
        assert_eq!(
            default_research_dir_at(&executable, &project, None),
            research
        );
    }

    #[test]
    fn packaged_runtime_does_not_fall_back_to_developer_checkout() {
        let fixture = TempFixture::new("missing-packaged");
        let executable = fixture.file("Reyn Studio.app/Contents/MacOS/reyn-studio");
        let developer_dir = fixture.root.join("developer-checkout");
        fs::create_dir_all(&developer_dir).unwrap();
        fixture.file("developer-checkout/engine/reyn_engine.py");

        let error = resolve_engine_script_at(&executable, &developer_dir, None, None).unwrap_err();
        let message = error.to_string().replace('\\', "/");
        assert!(message.contains("Python sidecar is missing"));
        assert!(message.contains("Reyn Studio.app/Contents/Resources/engine/reyn_engine.py"));
        assert!(!message.contains("developer-checkout/engine/reyn_engine.py"));
    }

    #[test]
    fn explicit_resource_and_engine_overrides_are_deterministic() {
        let fixture = TempFixture::new("overrides");
        let executable = fixture.file("bin/reyn-studio");
        let current_dir = fixture.root.join("cwd");
        let resource_script = fixture.file("cwd/runtime/engine/reyn_engine.py");
        let resource_research = fixture.research("cwd/runtime/research");
        let override_script = fixture.file("cwd/custom/sidecar.py");

        assert_eq!(
            resolve_engine_script_at(&executable, &current_dir, Some(Path::new("runtime")), None,)
                .unwrap(),
            resource_script
        );
        assert_eq!(
            resolve_engine_script_at(
                &executable,
                &current_dir,
                Some(Path::new("runtime")),
                Some(Path::new("custom/sidecar.py")),
            )
            .unwrap(),
            override_script
        );
        assert_eq!(
            default_research_dir_at(&executable, &current_dir, Some(Path::new("runtime")),),
            resource_research
        );
    }

    #[test]
    fn missing_research_modules_and_model_bundles_are_reported() {
        let fixture = TempFixture::new("research-diagnostics");
        let research = fixture.root.join("research");
        fs::create_dir_all(research.join("reyn_models")).unwrap();
        fs::write(research.join("dataset.py"), b"# only one module\n").unwrap();
        fs::write(research.join("root.reynmodel"), b"bundle").unwrap();
        fs::write(research.join("root.reynmodel.sig"), b"signature").unwrap();
        fs::write(research.join("reyn_models/managed.reynmodel"), b"bundle").unwrap();
        fs::write(
            research.join("reyn_models/managed.reynmodel.sig"),
            b"signature",
        )
        .unwrap();
        fs::write(research.join("unsigned.reynmodel"), b"bundle").unwrap();
        fs::write(research.join("orphan.reynmodel.sig"), b"signature").unwrap();
        fs::write(research.join("legacy.pth"), b"legacy checkpoint").unwrap();

        let missing = missing_research_modules(&research);
        assert!(missing.contains(&"time_moe_operator.py"));
        assert!(!missing.contains(&"dataset.py"));
        assert_eq!(model_bundle_count(&research), 2);
    }

    #[test]
    fn missing_python_diagnostic_names_the_override_and_bundle_boundary() {
        let fixture = TempFixture::new("python-missing");
        let current_dir = fixture.root.join("cwd");
        fs::create_dir_all(&current_dir).unwrap();
        let error = resolve_python_at(Path::new("missing/bin/python"), &current_dir).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("missing/bin/python"));
        assert!(message.contains("[runtime.missing]"));
        assert!(message.contains("REYN_PYTHON"));
    }

    #[cfg(unix)]
    #[test]
    fn dependency_probe_preserves_python_import_diagnostic() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = TempFixture::new("python-dependencies");
        let python = fixture.root.join("fake-python");
        fs::write(
            &python,
            b"#!/bin/sh\necho \"ModuleNotFoundError: No module named torch\" >&2\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&python, fs::Permissions::from_mode(0o755)).unwrap();

        let message = validate_python_dependencies(&python)
            .unwrap_err()
            .to_string();
        assert!(message.contains("No module named torch"));
        assert!(message.contains("[runtime.dependencies]"));
        assert!(message.contains("REYN_PYTHON"));
    }

    #[test]
    fn parses_model_library_metadata() {
        let value = serde_json::json!({
            "id": "reyn_models/h64.reynmodel",
            "name": "h64.reynmodel",
            "managed": true,
            "size_bytes": 1048576,
            "modified_unix": 1234,
            "checkpoint_sha256": "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "status": "clean",
            "status_detail": "fixed final epoch · source fingerprint present",
            "dimension": 2,
            "grid": 128,
            "in_channels": 4,
            "out_channels": 2,
            "max_steps": 64,
            "epoch": 40,
            "declared_epochs": 40,
            "checkpoint_role": "fixed_final",
            "scenario": "obstacle",
            "source_digest": "abc123",
            "physics_contract": "fixed_body_v2",
            "authenticity_status": "verified",
            "publisher_key_id": "release-2026-a",
            "publisher_key_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "release_sequence": 12,
            "support": ["2D · 128^2 grid"],
            "limitations": ["Static body only"],
            "benchmark_report_hashes": ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
            "unknown_fields": []
        });
        let card = parse_model_card(&value).expect("valid model card");
        assert_eq!(card.id, "reyn_models/h64.reynmodel");
        assert_eq!(card.dimension, 2);
        assert!(card.managed);
        assert_eq!(card.source_digest.as_deref(), Some("abc123"));
        assert_eq!(card.physics_contract, "fixed_body_v2");
        assert_eq!(card.authenticity_status, "verified");
        assert_eq!(card.publisher_key_id.as_deref(), Some("release-2026-a"));
        assert_eq!(card.release_sequence, Some(12));
        assert_eq!(card.limitations, vec!["Static body only"]);

        let mut legacy = value;
        legacy["id"] = serde_json::json!("reyn_models/h64.pth");
        legacy["name"] = serde_json::json!("h64.pth");
        let legacy = parse_model_card(&legacy).expect("legacy card remains inspectable");
        assert_eq!(legacy.status, "invalid");
        assert!(legacy.status_detail.contains("never opened"));
        assert!(parse_model_card(&serde_json::json!({"name": "bad"})).is_none());
    }

    #[test]
    fn external_flow_qualification_requires_the_exact_verified_contract() {
        let qualified = ModelCard {
            id: "reyn_models/external-h64.reynmodel".into(),
            name: "External H64".into(),
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
            physics_contract: EXTERNAL_FLOW_MODEL_PHYSICS_CONTRACT.into(),
            authenticity_status: "verified".into(),
            publisher_key_id: Some("release".into()),
            publisher_key_sha256: Some("b".repeat(64)),
            release_sequence: Some(1),
            support: Vec::new(),
            limitations: Vec::new(),
            benchmark_report_hashes: Vec::new(),
            unknown_fields: Vec::new(),
            qualification_class: MODEL_QUALIFICATION_PRODUCTION.into(),
        };
        assert!(is_qualified_external_flow_model(&qualified, 64));
        for mismatch in [
            {
                let mut model = qualified.clone();
                model.authenticity_status = "unsigned".into();
                model
            },
            {
                let mut model = qualified.clone();
                model.dimension = 2;
                model
            },
            {
                let mut model = qualified.clone();
                model.grid = 32;
                model
            },
            {
                let mut model = qualified.clone();
                model.physics_contract = "generic".into();
                model
            },
            {
                let mut model = qualified.clone();
                model.qualification_class = MODEL_QUALIFICATION_PREVIEW.into();
                model
            },
        ] {
            assert!(!is_qualified_external_flow_model(&mismatch, 64));
        }
    }

    #[test]
    fn verified_research_32_five_channel_operator_can_bind_cad() {
        let research = ModelCard {
            id: "reyn_models/reyn-flow3d-obstacle-32-v1.reynmodel".into(),
            name: "Research 32".into(),
            managed: true,
            size_bytes: 1,
            modified_unix: 1,
            checkpoint_sha256: "a".repeat(64),
            status: "clean".into(),
            status_detail: String::new(),
            dimension: 3,
            grid: 32,
            in_channels: 5,
            out_channels: 3,
            max_steps: 128,
            epoch: 40,
            declared_epochs: 40,
            checkpoint_role: "fixed_final".into(),
            scenario: "obstacle".into(),
            source_digest: Some("source".into()),
            physics_contract: EXTERNAL_FLOW_MODEL_PHYSICS_CONTRACT.into(),
            authenticity_status: "verified".into(),
            publisher_key_id: Some("release".into()),
            publisher_key_sha256: Some("b".repeat(64)),
            release_sequence: Some(1),
            support: Vec::new(),
            limitations: vec!["Research 3D obstacle operator. Not production-qualified CFD.".into()],
            benchmark_report_hashes: Vec::new(),
            unknown_fields: Vec::new(),
            qualification_class: MODEL_QUALIFICATION_RESEARCH.into(),
        };
        assert!(is_qualified_external_flow_model(&research, 32));
        let mut preview = research.clone();
        preview.qualification_class = MODEL_QUALIFICATION_PREVIEW.into();
        assert!(!is_qualified_external_flow_model(&preview, 32));
        let mut wrong_grid = research.clone();
        assert!(!is_qualified_external_flow_model(&wrong_grid, 64));
        wrong_grid.grid = 64;
        assert!(!is_qualified_external_flow_model(&wrong_grid, 64));
    }

    #[test]
    fn research_version_stays_research_even_with_not_production_qualified_copy() {
        assert_eq!(
            derive_model_qualification_class(
                "1.0.0-research",
                &["Research 3D obstacle operator. Not production-qualified CFD.".into()],
                "clean"
            ),
            MODEL_QUALIFICATION_RESEARCH
        );
        assert_eq!(
            derive_model_qualification_class("1.0.0-yc-preview", &[], "clean"),
            MODEL_QUALIFICATION_PREVIEW
        );
    }

    #[test]
    fn yc_preview_release_manifest_is_fail_closed_and_non_production() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("packaging/models/yc-preview-h64/model-release-manifest.json");
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("preview release manifest"))
                .expect("preview release JSON");
        validate_yc_preview_model_release(&manifest).expect("schema validates");
        assert_eq!(
            derive_model_qualification_class(
                manifest["model"]["version"].as_str().unwrap(),
                &[],
                "clean"
            ),
            MODEL_QUALIFICATION_PREVIEW
        );
        let mut production_claim = manifest.clone();
        production_claim["model"]["version"] = serde_json::json!("1.0.0");
        production_claim["qualification_boundary"] =
            serde_json::json!("Fully production qualified.");
        assert!(validate_yc_preview_model_release(&production_claim).is_err());
    }

    #[test]
    fn production_model_format_guard_is_fail_closed_with_conversion_guidance() {
        let fixture = TempFixture::new("model-signature-guard");
        let research = fixture.root.join("research");
        fs::create_dir_all(&research).unwrap();
        fs::write(research.join("unsigned.reynmodel"), b"bundle").unwrap();

        assert!(is_model_bundle_id("reyn_models/verified.reynmodel"));
        assert!(!is_model_bundle_id("legacy.pth"));
        assert!(require_model_bundle_id("legacy.pth").is_err());
        assert!(matches!(
            guard_model_request(&research, "legacy.pth", || panic!(
                "legacy request reached transport"
            )),
            Ok(Msg::Error(message)) if message.contains("never opened")
        ));
        assert!(matches!(
            guard_model_request(&research, "unsigned.reynmodel", || panic!(
                "unsigned request reached transport"
            )),
            Ok(Msg::Error(message)) if message.contains("signature.missing")
        ));
        fs::write(research.join("unsigned.reynmodel.sig"), b"signature").unwrap();
        assert!(matches!(
            guard_model_request(&research, "unsigned.reynmodel", || Ok(Msg::Status(
                "transport reached".into()
            ))),
            Ok(Msg::Status(message)) if message == "transport reached"
        ));
        let rejection = model_format_rejection("/tmp/legacy.pth");
        assert!(!rejection.accepted);
        assert_eq!(rejection.issues[0].code, "bundle.invalid_extension");
        assert!(rejection.summary.contains("convert_model_bundle.py"));
        assert!(rejection.summary.contains(".reynmodel.sig"));
    }

    #[test]
    fn parses_structured_model_rejection() {
        let value = serde_json::json!({
            "accepted": false,
            "status": "rejected",
            "summary": "unsupported checkpoint contract",
            "issues": [{
                "code": "contract.unsupported_channels",
                "field": "model_config",
                "message": "unsupported checkpoint contract",
                "severity": "error"
            }],
            "candidate": {
                "name": "bad.reynmodel",
                "checkpoint_sha256": "abc123"
            }
        });
        let validation = parse_model_validation(&value).expect("structured validation");
        assert!(!validation.accepted);
        assert_eq!(validation.issues[0].code, "contract.unsupported_channels");
        assert_eq!(validation.candidate_name.as_deref(), Some("bad.reynmodel"));
    }

    #[test]
    fn parses_benchmark_provenance_protocol() {
        let value = serde_json::json!({
            "ok": true,
            "seeds": [70000, 50000],
            "horizons": [1, 4],
            "rel": [[0.1, 0.2], [0.3, 0.4]],
            "persist": [[0.5, 0.6], [0.7, 0.8]],
            "global_rel": 0.25,
            "runtime_s": 1.2,
            "grid": 128,
            "epoch": 30,
            "dt_frame": 0.04,
            "provenance": {
                "verdict": "flagged",
                "training_seed": 0,
                "mixed_fork_seed": 10000,
                "mixed_fork_used": false,
                "validation_seed": 50000,
                "dataset": "standard",
                "benchmark_seeds": [
                    {"seed": 70000, "stream": "fresh_test", "overlap": false},
                    {"seed": 50000, "stream": "validation_selection", "overlap": true}
                ],
                "overlap_count": 1,
                "overlap_pct": 50.0,
                "epoch": 30,
                "declared_epochs": 30,
                "checkpoint_role": "legacy/unknown",
                "final_epoch_status": "final_epoch_role_unknown",
                "selection_metric": "val_multi_horizon_rel_l2",
                "selection_stream": "validation",
                "source_fingerprint_present": false,
                "source_fingerprint_digest": null,
                "legacy_unknown": ["checkpoint role absent", "source fingerprint absent"],
                "flags": ["benchmark seeds overlap reserved streams: 50000=validation_selection"]
            }
        });
        match parse_benchmark(&value, "model.reynmodel".into()) {
            Msg::Benchmark(result) => {
                assert_eq!(result.seeds, vec![70000, 50000]);
                assert_eq!(result.provenance.verdict, "flagged");
                assert_eq!(result.provenance.overlap_count, 1);
                assert_eq!(
                    result.provenance.benchmark_seeds[1].stream,
                    "validation_selection"
                );
                assert!(!result.provenance.source_fingerprint_present);
                assert_eq!(result.dt_frame, 0.04);
            }
            _ => panic!("valid suite protocol did not parse"),
        }
    }

    #[test]
    fn parses_benchmark_inspector_protocol_and_rejects_short_payload() {
        let value = serde_json::json!({
            "ok": true,
            "schema": INSPECTOR_SCHEMA,
            "protocol_version": INSPECTOR_PROTOCOL_VERSION,
            "shape": [4, 3, 2, 2],
            "layout": INSPECTOR_LAYOUT,
            "variables": ["velocity", "vorticity", "pressure", "divergence"],
            "signed": [false, true, true, true],
            "units": [
                "solver_velocity_unit",
                "inverse_solver_time_unit",
                "solver_velocity_unit_squared",
                "inverse_solver_time_unit"
            ],
            "panel_sources": [
                ["MODEL", "SOLVER_REFERENCE", "DERIVED"],
                ["DERIVED_FROM_MODEL", "DERIVED_FROM_SOLVER_REFERENCE", "DERIVED"],
                ["RECOVERED_FROM_MODEL", "RECOVERED_FROM_SOLVER_REFERENCE", "DERIVED"],
                ["DERIVED_FROM_MODEL", "DERIVED_FROM_SOLVER_REFERENCE", "DERIVED"]
            ],
            "domain": INSPECTOR_DOMAIN,
            "derivative": INSPECTOR_DERIVATIVE,
            "pressure": INSPECTOR_PRESSURE,
            "seed": 70000,
            "horizon": 4,
            "seed_stream": "fresh_test",
            "provenance_status": "unknown",
            "rel_l2": 0.1,
            "persist_rel_l2": 0.4,
            "improvement_ratio": 4.0,
            "mean_abs_error": 0.02,
            "p95_abs_error": 0.05,
            "max_abs_error": 0.08,
            "divergence_model_rms": 0.001,
            "divergence_truth_rms": 0.0001,
            "divergence_error_rms": 0.0011,
            "spectrum_rel_l2": 0.12,
            "spectrum_k": [1.0, 2.0],
            "spectrum_model": [3.0, 1.0],
            "spectrum_truth": [2.8, 1.1]
        });
        let floats: Vec<f32> = (0..48).map(|number| number as f32).collect();
        let payload: Vec<u8> = floats
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        match parse_benchmark_inspector(&value, &payload) {
            Msg::BenchmarkInspector(result) => {
                assert_eq!(result.n, 2);
                let velocity = result
                    .maps
                    .get(InspectorVariable::Velocity)
                    .expect("velocity maps");
                assert_eq!(velocity.model, vec![0.0, 1.0, 2.0, 3.0]);
                assert_eq!(velocity.error, vec![8.0, 9.0, 10.0, 11.0]);
                let divergence = result
                    .maps
                    .get(InspectorVariable::Divergence)
                    .expect("spatial divergence maps");
                assert_eq!(divergence.model, vec![36.0, 37.0, 38.0, 39.0]);
                assert_eq!(divergence.error, vec![44.0, 45.0, 46.0, 47.0]);
                assert_eq!(result.spectrum_k, vec![1.0, 2.0]);
                assert_eq!(result.seed_stream, "fresh_test");
            }
            _ => panic!("valid inspector protocol did not parse"),
        }
        assert!(matches!(
            parse_benchmark_inspector(&value, &payload[..payload.len() - 4]),
            Msg::Error(_)
        ));
        let mut wrong_method = value;
        wrong_method["derivative"] = serde_json::json!("finite_difference");
        assert!(matches!(
            parse_benchmark_inspector(&wrong_method, &payload),
            Msg::Error(_)
        ));
    }

    /// End-to-end bridge test: constructs a verified tensor-only bundle, then
    /// verifies list_models + a predicted 3D field through the real sidecar.
    #[test]
    fn engine_round_trip() {
        let fixture = VerifiedModelFixture::new(3);
        let h = fixture.spawn();
        h.tx.send(Cmd::ListModels).unwrap();
        assert!(
            matches!(wait_for(&h, |m| matches!(m, Msg::Models(_)), 20),
            Some(Msg::Models(ref v)) if v.iter().any(|model| model.id == fixture.model_id)),
            "no models listed"
        );

        h.tx.send(Cmd::Predict {
            model: fixture.model_id.clone(),
            seed: 3,
        })
        .unwrap();
        match wait_for(&h, |m| matches!(m, Msg::Field(_) | Msg::Error(_)), 40) {
            Some(Msg::Field(f)) => {
                assert_eq!(f.shape, vec![3, fixture.grid, fixture.grid, fixture.grid]);
                assert_eq!(f.data.len(), 3 * fixture.grid.pow(3));
                assert!(
                    !crate::flow::from_field(&f.shape, &f.data).is_empty(),
                    "field produced no particles"
                );
            }
            Some(Msg::Error(e)) => panic!("engine error: {e}"),
            _ => panic!("timed out waiting for the model field"),
        }
    }

    /// N3 bridge test: a 2D prediction with a solver-reference overlay returns
    /// model/reference planes, a sane reference RelL2, and a semigroup
    /// self-consistency number.
    #[test]
    fn predict2d_round_trip() {
        let fixture = VerifiedModelFixture::new(2);
        let h = fixture.spawn();
        h.tx.send(Cmd::Predict2D {
            model: fixture.model_id.clone(),
            steps: 4,
            seed: 1,
            want_truth: true,
            method: "spectral".into(),
            tolerance: 1e-5,
            boundary: "periodic".into(),
        })
        .unwrap();
        match wait_for(&h, |m| matches!(m, Msg::Field2D(_) | Msg::Error(_)), 60) {
            Some(Msg::Field2D(f)) => {
                assert_eq!(f.n, fixture.grid);
                assert_eq!(f.ai.len(), 3 * fixture.grid * fixture.grid);
                let truth = f.truth.expect("want_truth but no truth returned");
                assert_eq!(truth.len(), 3 * fixture.grid * fixture.grid);
                let rel = f.rel_l2.expect("no rel_l2");
                assert!(rel.is_finite() && rel >= 0.0);
                assert!(f.persist.is_some_and(|value| value.is_finite()));
                assert!(
                    f.semigroup.is_some(),
                    "even horizon should yield a semigroup number"
                );
                assert!(
                    f.p_residual < 1e-3,
                    "spectral recovery residual too high: {}",
                    f.p_residual
                );
            }
            Some(Msg::Error(e)) => panic!("engine error: {e}"),
            _ => panic!("timed out waiting for the 2D field"),
        }
    }

    /// N4 bridge test: a painted, natively-projected vortex-pair IC goes through
    /// the unified model (mask=0-capable) and comes back as a finite field.
    #[test]
    fn predict_ic_round_trip() {
        let fixture = VerifiedModelFixture::new(2);
        let n = fixture.grid;
        let mut ic = vec![0.0f32; 2 * n * n];
        for y in 0..n {
            for x in 0..n {
                let phase_x = x as f32 * std::f32::consts::TAU / n as f32;
                let phase_y = y as f32 * std::f32::consts::TAU / n as f32;
                ic[y * n + x] = phase_y.sin();
                ic[n * n + y * n + x] = phase_x.sin();
            }
        }
        let h = fixture.spawn();
        h.tx.send(Cmd::PredictIC {
            model: fixture.model_id.clone(),
            steps: 4,
            ic: std::sync::Arc::new(ic),
        })
        .unwrap();
        match wait_for(&h, |m| matches!(m, Msg::Field2D(_) | Msg::Error(_)), 60) {
            Some(Msg::Field2D(f)) => {
                assert_eq!(f.n, fixture.grid);
                assert_eq!(f.scenario, "painted");
                assert!(f.truth.is_none(), "painted ICs have no solver truth");
                assert_eq!(f.ai.len(), 3 * fixture.grid * fixture.grid);
                assert!(f.ai.iter().all(|v| v.is_finite()), "non-finite prediction");
                assert!(f.semigroup.is_some(), "trust signal missing");
            }
            Some(Msg::Error(e)) => panic!("engine error: {e}"),
            _ => panic!("timed out waiting for the painted-IC prediction"),
        }
    }

    /// CAD bridge test: a voxel box mask → real Brinkman warmup → 3D model →
    /// velocity + recovered pressure + the smoothed mask, all finite, with a
    /// meaningful surface-load spread.
    #[test]
    fn predict_cad_round_trip() {
        let fixture = VerifiedModelFixture::new(3);
        let n = fixture.grid;
        let mut mask = vec![0f32; n * n * n];
        for i in n / 4..n / 2 {
            for j in n / 3..2 * n / 3 {
                for k in n / 3..2 * n / 3 {
                    mask[i * n * n + j * n + k] = 1.0;
                }
            }
        }
        let h = fixture.spawn();
        h.tx.send(Cmd::CadPredict {
            request_id: "test-cad-request".into(),
            model: fixture.model_id.clone(),
            steps: 4,
            mask: std::sync::Arc::new(mask),
            reynolds: 150.0,
            characteristic_length_solver: 0.6,
            reference_length_m: 1.0,
            velocity_mps: 1.0,
            density_kg_m3: 1.225,
            reference_pressure_pa: 101_325.0,
            moment_origin_mode: "diffuse_surface_centroid".into(),
            moment_origin_solver: None,
            reference_area_m2: None,
        })
        .unwrap();
        match wait_for(&h, |m| matches!(m, Msg::CadField(_) | Msg::Error(_)), 90) {
            Some(Msg::CadField(f)) => {
                assert_eq!(f.request_id, "test-cad-request");
                assert_eq!(f.n, n);
                assert_eq!(f.vel.len(), 3 * n * n * n);
                assert_eq!(f.pressure.len(), n * n * n);
                assert_eq!(f.mask.len(), n * n * n);
                assert!(f.vel.iter().all(|v| v.is_finite()));
                let cp_min = f.cp.iter().copied().fold(f32::INFINITY, f32::min);
                let cp_max = f.cp.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                assert!(cp_max > cp_min, "no Cp spread on the body");
                let surf = crate::cad::surface_insights(&f.mask, &f.cp, n);
                assert_eq!(surf.len(), 2, "surface load + suction pins expected");
            }
            Some(Msg::Error(e)) => panic!("engine error: {e}"),
            _ => panic!("timed out waiting for the CAD prediction"),
        }
    }

    /// N5 bridge test: a verified synthetic bundle returns the full measured
    /// protocol without making an accuracy claim for untrained fixture weights.
    #[test]
    fn benchmark_round_trip() {
        let fixture = VerifiedModelFixture::new(2);
        let h = fixture.spawn();
        h.tx.send(Cmd::RunBenchmark {
            model: fixture.model_id.clone(),
            seeds: vec![70000, 70001],
            horizons: vec![1, 4],
        })
        .unwrap();
        match wait_for(&h, |m| matches!(m, Msg::Benchmark(_) | Msg::Error(_)), 120) {
            Some(Msg::Benchmark(b)) => {
                assert_eq!(b.seeds, vec![70000, 70001]);
                assert_eq!(b.horizons, vec![1, 4]);
                assert_eq!(b.rel.len(), 2);
                assert_eq!(b.rel[0].len(), 2);
                for (row_r, row_p) in b.rel.iter().zip(&b.persist) {
                    for (r, p) in row_r.iter().zip(row_p) {
                        assert!(r.is_finite() && *r > 0.0);
                        assert!(p.is_finite() && *p > 0.0);
                    }
                }
                assert!(b.global_rel > 0.0 && b.runtime_s > 0.0);
                assert_eq!(b.provenance.overlap_count, 0);
                assert!(b
                    .provenance
                    .benchmark_seeds
                    .iter()
                    .all(|seed| seed.stream == "fresh_test" && !seed.overlap));
            }
            Some(Msg::Error(e)) => panic!("engine error: {e}"),
            _ => panic!("timed out waiting for the benchmark"),
        }

        h.tx.send(Cmd::InspectBenchmarkCell {
            model: fixture.model_id.clone(),
            seed: 70000,
            horizon: 4,
        })
        .unwrap();
        match wait_for(
            &h,
            |m| matches!(m, Msg::BenchmarkInspector(_) | Msg::Error(_)),
            60,
        ) {
            Some(Msg::BenchmarkInspector(cell)) => {
                assert_eq!((cell.seed, cell.horizon, cell.n), (70000, 4, fixture.grid));
                for variable in InspectorVariable::ALL {
                    let maps = cell.maps.get(variable).expect("all inspector modes");
                    assert_eq!(maps.model.len(), fixture.grid * fixture.grid);
                    assert_eq!(maps.reference.len(), fixture.grid * fixture.grid);
                    assert_eq!(maps.error.len(), fixture.grid * fixture.grid);
                }
                assert_eq!(cell.seed_stream, "fresh_test");
                assert!(cell.rel_l2.is_finite() && cell.rel_l2 > 0.0);
                assert!(cell.persist_rel_l2.is_finite() && cell.persist_rel_l2 > 0.0);
                assert!(cell.improvement_ratio.is_finite());
                assert!(cell.spectrum_k.len() > 2);
                assert_eq!(cell.spectrum_k.len(), cell.spectrum_model.len());
                assert_eq!(cell.spectrum_k.len(), cell.spectrum_truth.len());
                assert!(cell.spectrum_rel_l2.is_finite());
            }
            Some(Msg::Error(e)) => panic!("engine error: {e}"),
            _ => panic!("timed out waiting for benchmark inspector"),
        }
    }

    #[test]
    fn parse_cad_progress_accepts_stage_frames_and_rejects_final_payloads() {
        let progress = parse_cad_progress(&serde_json::json!({
            "ok": true,
            "progress": true,
            "request_id": "req-1",
            "stage": "developing",
            "stage_index": 2,
            "stage_count": 4,
            "detail": "Brinkman warmup 5/20",
            "fraction": 0.375,
        }))
        .expect("progress frame");
        assert_eq!(progress.request_id, "req-1");
        assert_eq!(progress.stage, "developing");
        assert_eq!((progress.stage_index, progress.stage_count), (2, 4));
        assert!((progress.fraction - 0.375).abs() < 1e-6);

        assert!(parse_cad_progress(&serde_json::json!({
            "ok": true,
            "shape": [9, 32, 32, 32],
            "request_id": "req-1",
        }))
        .is_none());
        assert!(parse_cad_progress(&serde_json::json!({
            "ok": true,
            "progress": true,
            "stage": "developing",
            "stage_index": 0,
            "stage_count": 4,
        }))
        .is_none());
    }
}
