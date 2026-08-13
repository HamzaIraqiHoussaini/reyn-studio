//! Offline Python runtime discovery and integrity validation.
//!
//! This module deliberately stops at local, immutable slot selection. It does
//! not download updates or treat a hash-only manifest as a signature.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const RUNTIME_MANIFEST_SCHEMA: &str = "com.reyn.runtime-manifest/1";
pub(crate) const RUNTIME_STATE_SCHEMA: &str = "com.reyn.runtime-state/1";
#[cfg(target_os = "macos")]
pub(crate) const TARGET_PLATFORM: &str = "macos";
#[cfg(target_os = "windows")]
pub(crate) const TARGET_PLATFORM: &str = "windows";
#[cfg(target_os = "linux")]
pub(crate) const TARGET_PLATFORM: &str = "linux";
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub(crate) const TARGET_PLATFORM: &str = "unsupported";
#[cfg(target_os = "macos")]
pub(crate) const TARGET_ARCHITECTURE: &str = "arm64";
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) const TARGET_ARCHITECTURE: &str = "x86_64";
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub(crate) const TARGET_ARCHITECTURE: &str = "unsupported";
pub(crate) const MINIMUM_MACOS: &str = "14.0";
pub(crate) const PYTHON_VERSION: &str = "3.14.6";
pub(crate) const TORCH_VERSION: &str = "2.13.0";
pub(crate) const NUMPY_VERSION: &str = "2.5.1";
pub(crate) const ENGINE_PROTOCOL: u32 = 1;
pub(crate) const STARTUP_FAILURE_ROLLBACK_THRESHOLD: u32 = 2;
pub(crate) const CRASH_ROLLBACK_THRESHOLD: u32 = 3;
pub(crate) const DEFAULT_SMOKE_TIMEOUT: Duration = Duration::from_secs(30);

const MANIFEST_NAME: &str = "runtime-manifest.cjson";
const SIGNATURE_NAME: &str = "runtime-manifest.sig";
const SBOM_NAME: &str = "runtime-sbom.cdx.json";
const NOTICES_NAME: &str = "THIRD_PARTY_NOTICES.html";
const STATE_NAME: &str = "state.json";
const SMOKE_SCHEMA: &str = "com.reyn.runtime-smoke/1";
const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn vec_is_empty<T>(value: &[T]) -> bool {
    value.is_empty()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeManifest {
    pub schema: String,
    pub runtime_id: String,
    pub platform: String,
    pub architecture: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_macos: Option<String>,
    pub python: String,
    pub torch: String,
    pub numpy: String,
    pub engine_protocol: u32,
    pub research_closure_sha256: String,
    pub source_revision: String,
    pub build_epoch: u64,
    pub files: Vec<RuntimeFile>,
    pub sbom_sha256: String,
    pub notices_sha256: String,
}

#[derive(Serialize)]
struct RuntimeManifestIdentity<'a> {
    schema: &'a str,
    platform: &'a str,
    architecture: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum_macos: Option<&'a str>,
    python: &'a str,
    torch: &'a str,
    numpy: &'a str,
    engine_protocol: u32,
    research_closure_sha256: &'a str,
    source_revision: &'a str,
    build_epoch: u64,
    files: &'a [RuntimeFile],
    sbom_sha256: &'a str,
    notices_sha256: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimePlatformSpec {
    pub platform: &'static str,
    pub architecture: &'static str,
    pub minimum_os: Option<&'static str>,
    pub python_relative_path: &'static str,
}

const MACOS_ARM64_SPEC: RuntimePlatformSpec = RuntimePlatformSpec {
    platform: "macos",
    architecture: "arm64",
    minimum_os: Some(MINIMUM_MACOS),
    python_relative_path: "bin/python3.14",
};

const WINDOWS_X64_SPEC: RuntimePlatformSpec = RuntimePlatformSpec {
    platform: "windows",
    architecture: "x86_64",
    minimum_os: None,
    python_relative_path: "python.exe",
};

const LINUX_X64_SPEC: RuntimePlatformSpec = RuntimePlatformSpec {
    platform: "linux",
    architecture: "x86_64",
    minimum_os: None,
    python_relative_path: "bin/python3.14",
};

fn target_platform_spec() -> &'static RuntimePlatformSpec {
    if cfg!(target_os = "windows") {
        &WINDOWS_X64_SPEC
    } else if cfg!(target_os = "linux") {
        &LINUX_X64_SPEC
    } else {
        &MACOS_ARM64_SPEC
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeSelfTest {
    pub runtime_id: String,
    pub passed: bool,
    pub completed_epoch: u64,
    pub result_code: String,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub duration_ms: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeFailureRecord {
    pub runtime_id: String,
    pub kind: String,
    pub count: u32,
    pub occurred_epoch: u64,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeRollbackRecord {
    pub failed_runtime_id: String,
    pub selected_runtime_id: Option<String>,
    pub reason: String,
    pub occurred_epoch: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeState {
    pub schema: String,
    pub generation: u64,
    pub active: Option<String>,
    pub previous: Option<String>,
    pub factory_runtime_id: Option<String>,
    pub last_known_good: Option<String>,
    pub activation_epoch: u64,
    pub app_version: String,
    pub last_self_test: Option<RuntimeSelfTest>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub last_known_good_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_runtime_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub consecutive_startup_failures: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub consecutive_crashes: u32,
    #[serde(default, skip_serializing_if = "vec_is_empty")]
    pub disabled_runtime_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<RuntimeFailureRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rollback: Option<RuntimeRollbackRecord>,
}

impl RuntimeState {
    #[allow(dead_code)] // Used by the future installer after candidate verification.
    pub(crate) fn empty(app_version: impl Into<String>) -> Self {
        Self {
            schema: RUNTIME_STATE_SCHEMA.into(),
            generation: 0,
            active: None,
            previous: None,
            factory_runtime_id: None,
            last_known_good: None,
            activation_epoch: 0,
            app_version: app_version.into(),
            last_self_test: None,
            last_known_good_epoch: 0,
            health_runtime_id: None,
            consecutive_startup_failures: 0,
            consecutive_crashes: 0,
            disabled_runtime_ids: Vec::new(),
            last_failure: None,
            last_rollback: None,
        }
    }

    #[allow(dead_code)] // Used by the future installer after candidate verification.
    pub(crate) fn activate(
        mut self,
        runtime_id: impl Into<String>,
        factory_runtime_id: Option<String>,
        app_version: impl Into<String>,
        activation_epoch: u64,
    ) -> Result<Self, RuntimeValidationError> {
        let runtime_id = runtime_id.into();
        validate_runtime_id(&runtime_id)?;
        if let Some(factory) = factory_runtime_id.as_deref() {
            validate_runtime_id(factory)?;
        }
        if self.disabled_runtime_ids.contains(&runtime_id) {
            return Err(RuntimeValidationError::state(format!(
                "runtime {runtime_id} is disabled after a prior automatic rollback"
            )));
        }
        if self.active.as_deref() != Some(runtime_id.as_str()) {
            self.previous = self.active.take();
            self.active = Some(runtime_id);
        }
        self.factory_runtime_id = factory_runtime_id;
        self.activation_epoch = activation_epoch;
        self.app_version = app_version.into();
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| RuntimeValidationError::state("runtime state generation overflow"))?;
        self.last_self_test = None;
        self.health_runtime_id = self.active.clone();
        self.consecutive_startup_failures = 0;
        self.consecutive_crashes = 0;
        Ok(self)
    }

    #[allow(dead_code)] // Used by the future rollback controller.
    pub(crate) fn rollback_target(&self) -> Option<&str> {
        [
            self.previous.as_deref(),
            self.last_known_good.as_deref(),
            self.factory_runtime_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .find(|runtime_id| {
            self.active.as_deref() != Some(*runtime_id)
                && !self
                    .disabled_runtime_ids
                    .iter()
                    .any(|disabled| disabled == runtime_id)
        })
    }
}

#[allow(dead_code)] // Activation diagnostics are used by the offline installer API below.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeFailureKind {
    MissingRuntime,
    MissingDependencies,
    Integrity,
    Platform,
    State,
    SmokeTimeout,
    SmokeFailed,
    Activation,
}

impl RuntimeFailureKind {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::MissingRuntime => "runtime.missing",
            Self::MissingDependencies => "runtime.dependencies",
            Self::Integrity => "runtime.integrity",
            Self::Platform => "runtime.platform",
            Self::State => "runtime.state",
            Self::SmokeTimeout => "runtime.smoke_timeout",
            Self::SmokeFailed => "runtime.smoke_failed",
            Self::Activation => "runtime.activation",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeValidationError {
    pub kind: RuntimeFailureKind,
    pub detail: String,
}

impl RuntimeValidationError {
    fn new(kind: RuntimeFailureKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    fn missing(detail: impl Into<String>) -> Self {
        Self::new(RuntimeFailureKind::MissingRuntime, detail)
    }

    fn dependencies(detail: impl Into<String>) -> Self {
        Self::new(RuntimeFailureKind::MissingDependencies, detail)
    }

    fn integrity(detail: impl Into<String>) -> Self {
        Self::new(RuntimeFailureKind::Integrity, detail)
    }

    fn platform(detail: impl Into<String>) -> Self {
        Self::new(RuntimeFailureKind::Platform, detail)
    }

    fn state(detail: impl Into<String>) -> Self {
        Self::new(RuntimeFailureKind::State, detail)
    }

    fn smoke_timeout(detail: impl Into<String>) -> Self {
        Self::new(RuntimeFailureKind::SmokeTimeout, detail)
    }

    fn smoke_failed(detail: impl Into<String>) -> Self {
        Self::new(RuntimeFailureKind::SmokeFailed, detail)
    }

    #[allow(dead_code)] // Used by the offline activation API.
    fn activation(detail: impl Into<String>) -> Self {
        Self::new(RuntimeFailureKind::Activation, detail)
    }
}

impl std::fmt::Display for RuntimeValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "[{}] {}", self.kind.code(), self.detail)
    }
}

impl std::error::Error for RuntimeValidationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeDiagnostic {
    pub code: &'static str,
    pub detail: String,
}

impl From<RuntimeValidationError> for RuntimeDiagnostic {
    fn from(error: RuntimeValidationError) -> Self {
        Self {
            code: error.kind.code(),
            detail: error.detail,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HostCompatibility {
    pub platform: String,
    pub architecture: String,
    pub macos_version: Option<String>,
}

impl HostCompatibility {
    pub(crate) fn current() -> Self {
        let platform = normalize_platform(std::env::consts::OS);
        let architecture = normalize_architecture(std::env::consts::ARCH);
        Self {
            platform,
            architecture,
            macos_version: current_macos_version(),
        }
    }
}

fn normalize_platform(platform: &str) -> String {
    match platform.to_ascii_lowercase().as_str() {
        "darwin" | "macos" => "macos".into(),
        "win32" | "win64" | "windows" => "windows".into(),
        "linux" => "linux".into(),
        other => other.into(),
    }
}

fn normalize_architecture(architecture: &str) -> String {
    match architecture.to_ascii_lowercase().as_str() {
        "aarch64" | "arm64" => "arm64".into(),
        "amd64" | "x64" | "x86_64" => "x86_64".into(),
        other => other.into(),
    }
}

#[cfg(target_os = "macos")]
fn current_macos_version() -> Option<String> {
    let output = std::process::Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|version| !version.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn current_macos_version() -> Option<String> {
    None
}

#[allow(dead_code)] // ManagedCandidate is produced only by explicit staging.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeSource {
    ManagedActive,
    ManagedRollback,
    ManagedCandidate,
    Factory,
}

impl RuntimeSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ManagedActive => "managed runtime",
            Self::ManagedRollback => "managed rollback runtime",
            Self::ManagedCandidate => "staged managed runtime",
            Self::Factory => "factory runtime",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VerifiedRuntime {
    pub root: PathBuf,
    pub python: PathBuf,
    pub manifest: RuntimeManifest,
    pub source: RuntimeSource,
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeDiscovery {
    pub selected: Option<VerifiedRuntime>,
    pub diagnostics: Vec<RuntimeDiagnostic>,
}

pub(crate) struct RuntimeDiscoveryRequest<'a> {
    pub factory_root: Option<&'a Path>,
    pub managed_root: Option<&'a Path>,
    pub host: &'a HostCompatibility,
    pub expected_research_closure_sha256: &'a str,
}

pub(crate) fn factory_runtime_root(current_exe: &Path) -> Option<PathBuf> {
    factory_runtime_root_for(current_exe, TARGET_PLATFORM)
}

fn factory_runtime_root_for(current_exe: &Path, platform: &str) -> Option<PathBuf> {
    match normalize_platform(platform).as_str() {
        "windows" | "linux" => current_exe
            .parent()
            .map(|directory| directory.join("ReynPython")),
        _ => {
            let macos = current_exe.parent()?;
            let contents = macos.parent()?;
            (macos.file_name()?.to_str()? == "MacOS"
                && contents.file_name()?.to_str()? == "Contents")
                .then(|| contents.join("Resources/ReynPython"))
        }
    }
}

pub(crate) fn default_managed_runtime_root() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA").map(|root| PathBuf::from(root).join("Reyn Studio/Runtime"))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/Reyn Studio/Runtime"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .map(|root| root.join("reyn-studio/runtime"))
    }
}

pub(crate) fn discover_runtime(request: RuntimeDiscoveryRequest<'_>) -> RuntimeDiscovery {
    let mut diagnostics = Vec::new();
    if let Err(error) = validate_host(request.host) {
        diagnostics.push(error.into());
        diagnostics.push(RuntimeDiagnostic {
            code: RuntimeFailureKind::MissingRuntime.code(),
            detail: "no supported managed Python runtime is available on this host".into(),
        });
        return RuntimeDiscovery {
            selected: None,
            diagnostics,
        };
    }

    let mut managed_candidates = Vec::new();
    if let Some(managed_root) = request.managed_root {
        match load_runtime_state(managed_root) {
            Ok(Some(state)) => {
                let mut seen = HashSet::new();
                let disabled = state
                    .disabled_runtime_ids
                    .iter()
                    .map(String::as_str)
                    .collect::<HashSet<_>>();
                for (runtime_id, source) in [
                    (state.active.as_deref(), RuntimeSource::ManagedActive),
                    (state.previous.as_deref(), RuntimeSource::ManagedRollback),
                    (
                        state.last_known_good.as_deref(),
                        RuntimeSource::ManagedRollback,
                    ),
                ] {
                    if let Some(runtime_id) = runtime_id {
                        if disabled.contains(runtime_id) {
                            diagnostics.push(RuntimeDiagnostic {
                                code: "runtime.rollback_loop_prevented",
                                detail: format!(
                                    "runtime {runtime_id} remains disabled after automatic rollback"
                                ),
                            });
                            continue;
                        }
                        if seen.insert(runtime_id.to_owned()) {
                            managed_candidates.push((runtime_id.to_owned(), source));
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(error) => diagnostics.push(error.into()),
        }

        for (runtime_id, source) in managed_candidates {
            let root = match managed_slot_root(managed_root, &runtime_id) {
                Ok(root) => root,
                Err(error) => {
                    diagnostics.push(error.into());
                    continue;
                }
            };
            match validate_runtime_root(
                &root,
                source,
                request.host,
                request.expected_research_closure_sha256,
                Some(&runtime_id),
            ) {
                Ok(runtime) => {
                    if source == RuntimeSource::ManagedRollback {
                        diagnostics.push(RuntimeDiagnostic {
                            code: "runtime.fallback_previous",
                            detail: format!(
                                "active runtime was unavailable; selected rollback slot {}",
                                runtime.manifest.runtime_id
                            ),
                        });
                    }
                    return RuntimeDiscovery {
                        selected: Some(runtime),
                        diagnostics,
                    };
                }
                Err(error) => diagnostics.push(error.into()),
            }
        }
    }

    if let Some(factory_root) = request.factory_root {
        match validate_runtime_root(
            factory_root,
            RuntimeSource::Factory,
            request.host,
            request.expected_research_closure_sha256,
            None,
        ) {
            Ok(runtime) => {
                if !diagnostics.is_empty() {
                    diagnostics.push(RuntimeDiagnostic {
                        code: "runtime.fallback_factory",
                        detail: format!(
                            "managed runtime was unavailable; selected factory slot {}",
                            runtime.manifest.runtime_id
                        ),
                    });
                }
                return RuntimeDiscovery {
                    selected: Some(runtime),
                    diagnostics,
                };
            }
            Err(error) => diagnostics.push(error.into()),
        }
    }

    diagnostics.push(RuntimeDiagnostic {
        code: RuntimeFailureKind::MissingRuntime.code(),
        detail: "no complete, compatible factory or managed Python runtime was found".into(),
    });
    RuntimeDiscovery {
        selected: None,
        diagnostics,
    }
}

fn managed_slot_root(
    managed_root: &Path,
    runtime_id: &str,
) -> Result<PathBuf, RuntimeValidationError> {
    validate_runtime_id(runtime_id)?;
    let slots = managed_root.join("slots");
    let portable = slots
        .join(runtime_slot_name(runtime_id)?)
        .join("ReynPython");
    let legacy = slots.join(runtime_id).join("ReynPython");
    Ok(if !portable.is_dir() && legacy.is_dir() {
        legacy
    } else {
        portable
    })
}

fn runtime_slot_name(runtime_id: &str) -> Result<&str, RuntimeValidationError> {
    validate_runtime_id(runtime_id)?;
    Ok(runtime_id
        .strip_prefix("sha256:")
        .expect("validated runtime IDs use sha256"))
}

fn runtime_id_from_slot_name(name: &str) -> Option<String> {
    if validate_runtime_id(name).is_ok() {
        return Some(name.to_owned());
    }
    (name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| format!("sha256:{name}"))
}

fn validate_runtime_root(
    root: &Path,
    source: RuntimeSource,
    host: &HostCompatibility,
    expected_research_closure_sha256: &str,
    expected_runtime_id: Option<&str>,
) -> Result<VerifiedRuntime, RuntimeValidationError> {
    if !root.is_dir() {
        return Err(RuntimeValidationError::missing(format!(
            "{} slot is missing: {}",
            source.label(),
            root.display()
        )));
    }
    let manifest_path = root.join(MANIFEST_NAME);
    let bytes = fs::read(&manifest_path).map_err(|error| {
        RuntimeValidationError::missing(format!(
            "{} has no readable {}: {error}",
            root.display(),
            MANIFEST_NAME
        ))
    })?;
    let manifest: RuntimeManifest = serde_json::from_slice(&bytes).map_err(|error| {
        RuntimeValidationError::integrity(format!(
            "{} is not a valid strict runtime manifest: {error}",
            manifest_path.display()
        ))
    })?;
    let canonical = canonical_manifest_bytes(&manifest)?;
    if bytes != canonical {
        return Err(RuntimeValidationError::integrity(format!(
            "{} is not in deterministic canonical form",
            manifest_path.display()
        )));
    }
    validate_manifest_metadata(
        &manifest,
        host,
        expected_research_closure_sha256,
        expected_runtime_id,
    )?;
    validate_manifest_files(root, &manifest)?;
    let python = root.join(python_relative_path(&manifest.python, &manifest.platform)?);
    if !python.is_file() {
        return Err(RuntimeValidationError::integrity(format!(
            "validated runtime has no interpreter at {}",
            python.display()
        )));
    }
    Ok(VerifiedRuntime {
        root: root.to_path_buf(),
        python,
        manifest,
        source,
    })
}

fn validate_manifest_metadata(
    manifest: &RuntimeManifest,
    host: &HostCompatibility,
    expected_research_closure_sha256: &str,
    expected_slot_id: Option<&str>,
) -> Result<(), RuntimeValidationError> {
    validate_manifest_metadata_for_spec(
        manifest,
        host,
        target_platform_spec(),
        expected_research_closure_sha256,
        expected_slot_id,
    )
}

fn validate_manifest_metadata_for_spec(
    manifest: &RuntimeManifest,
    host: &HostCompatibility,
    spec: &RuntimePlatformSpec,
    expected_research_closure_sha256: &str,
    expected_slot_id: Option<&str>,
) -> Result<(), RuntimeValidationError> {
    if manifest.schema != RUNTIME_MANIFEST_SCHEMA {
        return Err(RuntimeValidationError::integrity(format!(
            "unsupported runtime manifest schema '{}'",
            manifest.schema
        )));
    }
    validate_runtime_id(&manifest.runtime_id)
        .map_err(|error| RuntimeValidationError::integrity(error.detail))?;
    let calculated_id = manifest_runtime_id(manifest)?;
    if manifest.runtime_id != calculated_id {
        return Err(RuntimeValidationError::integrity(format!(
            "runtime identity mismatch: manifest declares {}, calculated {calculated_id}",
            manifest.runtime_id
        )));
    }
    if let Some(expected_slot_id) = expected_slot_id {
        if manifest.runtime_id != expected_slot_id {
            return Err(RuntimeValidationError::integrity(format!(
                "managed slot pointer {expected_slot_id} contains runtime {}",
                manifest.runtime_id
            )));
        }
    }
    if normalize_platform(&manifest.platform) != spec.platform
        || normalize_architecture(&manifest.architecture) != spec.architecture
        || manifest.minimum_macos.as_deref() != spec.minimum_os
    {
        return Err(RuntimeValidationError::platform(format!(
            "runtime targets {} {} with minimum OS {:?}, expected {} {} with minimum OS {:?}",
            manifest.platform,
            manifest.architecture,
            manifest.minimum_macos,
            spec.platform,
            spec.architecture,
            spec.minimum_os
        )));
    }
    validate_host_for_spec(host, spec)?;
    if manifest.python != PYTHON_VERSION
        || manifest.torch != TORCH_VERSION
        || manifest.numpy != NUMPY_VERSION
        || manifest.engine_protocol != ENGINE_PROTOCOL
    {
        return Err(RuntimeValidationError::dependencies(format!(
            "runtime dependency versions are Python {}, PyTorch {}, NumPy {}, protocol {}; \
             expected Python {}, PyTorch {}, NumPy {}, protocol {}",
            manifest.python,
            manifest.torch,
            manifest.numpy,
            manifest.engine_protocol,
            PYTHON_VERSION,
            TORCH_VERSION,
            NUMPY_VERSION,
            ENGINE_PROTOCOL
        )));
    }
    validate_sha256("research_closure_sha256", &manifest.research_closure_sha256)?;
    if manifest.research_closure_sha256 != expected_research_closure_sha256 {
        return Err(RuntimeValidationError::dependencies(format!(
            "runtime research closure {} does not match app closure {}",
            manifest.research_closure_sha256, expected_research_closure_sha256
        )));
    }
    validate_sha256("sbom_sha256", &manifest.sbom_sha256)?;
    validate_sha256("notices_sha256", &manifest.notices_sha256)?;
    if manifest.source_revision.trim().is_empty() {
        return Err(RuntimeValidationError::integrity(
            "runtime source_revision must not be empty",
        ));
    }
    Ok(())
}

fn validate_host(host: &HostCompatibility) -> Result<(), RuntimeValidationError> {
    validate_host_for_spec(host, target_platform_spec())
}

fn validate_host_for_spec(
    host: &HostCompatibility,
    spec: &RuntimePlatformSpec,
) -> Result<(), RuntimeValidationError> {
    if normalize_platform(&host.platform) != spec.platform
        || normalize_architecture(&host.architecture) != spec.architecture
    {
        return Err(RuntimeValidationError::platform(format!(
            "compute runtime requires {} {}; host is {} {}",
            spec.platform, spec.architecture, host.platform, host.architecture
        )));
    }
    let Some(minimum_os) = spec.minimum_os else {
        return Ok(());
    };
    let version = host.macos_version.as_deref().ok_or_else(|| {
        RuntimeValidationError::platform("could not determine the host macOS version")
    })?;
    if compare_numeric_versions(version, minimum_os)
        .ok_or_else(|| {
            RuntimeValidationError::platform(format!(
                "could not parse host macOS version '{version}'"
            ))
        })?
        .is_lt()
    {
        return Err(RuntimeValidationError::platform(format!(
            "compute runtime requires macOS {minimum_os} or later; host is {version}"
        )));
    }
    Ok(())
}

fn compare_numeric_versions(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let parse = |value: &str| {
        value
            .split('.')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .ok()
    };
    let mut left = parse(left)?;
    let mut right = parse(right)?;
    let length = left.len().max(right.len());
    left.resize(length, 0);
    right.resize(length, 0);
    Some(left.cmp(&right))
}

fn validate_manifest_files(
    root: &Path,
    manifest: &RuntimeManifest,
) -> Result<(), RuntimeValidationError> {
    if manifest.files.is_empty() {
        return Err(RuntimeValidationError::integrity(
            "runtime manifest contains no files",
        ));
    }
    let mut previous: Option<&str> = None;
    let mut declared = BTreeMap::new();
    #[cfg(unix)]
    let mut regular_inodes = HashSet::new();
    for file in &manifest.files {
        validate_relative_manifest_path(&file.path)?;
        validate_sha256(&format!("files[{}].sha256", file.path), &file.sha256)?;
        if previous.is_some_and(|path| path >= file.path.as_str()) {
            return Err(RuntimeValidationError::integrity(
                "runtime manifest file paths must be unique and strictly sorted",
            ));
        }
        previous = Some(&file.path);
        declared.insert(file.path.clone(), file.sha256.clone());

        let path = root.join(&file.path);
        let link_metadata = fs::symlink_metadata(&path).map_err(|error| {
            RuntimeValidationError::integrity(format!(
                "runtime file {} is missing: {error}",
                path.display()
            ))
        })?;
        if !(link_metadata.is_file() || link_metadata.file_type().is_symlink()) {
            return Err(RuntimeValidationError::integrity(format!(
                "runtime manifest path is not a file: {}",
                path.display()
            )));
        }
        if link_metadata.file_type().is_symlink() {
            validate_internal_symlink(root, &path)?;
        }
        let metadata = fs::metadata(&path).map_err(|error| {
            RuntimeValidationError::integrity(format!(
                "runtime file {} cannot be resolved: {error}",
                path.display()
            ))
        })?;
        if metadata.len() != file.size {
            return Err(RuntimeValidationError::integrity(format!(
                "runtime file size mismatch for {}: expected {}, found {}",
                file.path,
                file.size,
                metadata.len()
            )));
        }
        #[cfg(unix)]
        if !link_metadata.file_type().is_symlink() {
            use std::os::unix::fs::MetadataExt;
            if !regular_inodes.insert((metadata.dev(), metadata.ino())) {
                return Err(RuntimeValidationError::integrity(format!(
                    "runtime contains a hard-linked manifest file: {}",
                    file.path
                )));
            }
        }
        let digest = sha256_file(&path)?;
        if digest != file.sha256 {
            return Err(RuntimeValidationError::integrity(format!(
                "runtime file hash mismatch for {}: expected {}, found {digest}",
                file.path, file.sha256
            )));
        }
    }

    let actual = collect_payload_paths(root)?;
    let declared_paths: BTreeSet<_> = declared.keys().cloned().collect();
    if actual != declared_paths {
        let missing = declared_paths
            .difference(&actual)
            .cloned()
            .collect::<Vec<_>>();
        let extra = actual
            .difference(&declared_paths)
            .cloned()
            .collect::<Vec<_>>();
        return Err(RuntimeValidationError::integrity(format!(
            "runtime file inventory mismatch; missing: [{}]; extra: [{}]",
            missing.join(", "),
            extra.join(", ")
        )));
    }
    if declared.get(SBOM_NAME) != Some(&manifest.sbom_sha256) {
        return Err(RuntimeValidationError::integrity(format!(
            "{SBOM_NAME} hash does not match sbom_sha256"
        )));
    }
    if declared.get(NOTICES_NAME) != Some(&manifest.notices_sha256) {
        return Err(RuntimeValidationError::integrity(format!(
            "{NOTICES_NAME} hash does not match notices_sha256"
        )));
    }
    let python = python_relative_path(&manifest.python, &manifest.platform)?;
    if !declared.contains_key(python.to_str().unwrap_or_default()) {
        return Err(RuntimeValidationError::integrity(format!(
            "runtime interpreter {} is absent from the file manifest",
            python.display()
        )));
    }
    Ok(())
}

fn validate_internal_symlink(root: &Path, path: &Path) -> Result<(), RuntimeValidationError> {
    let target = fs::read_link(path).map_err(|error| {
        RuntimeValidationError::integrity(format!(
            "could not read runtime symlink {}: {error}",
            path.display()
        ))
    })?;
    if target.is_absolute()
        || target
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(RuntimeValidationError::integrity(format!(
            "runtime symlink has an unsafe target: {} -> {}",
            path.display(),
            target.display()
        )));
    }
    let canonical_root = fs::canonicalize(root).map_err(|error| {
        RuntimeValidationError::integrity(format!(
            "could not canonicalize runtime root {}: {error}",
            root.display()
        ))
    })?;
    let resolved = fs::canonicalize(path).map_err(|error| {
        RuntimeValidationError::integrity(format!(
            "runtime symlink cannot be resolved {}: {error}",
            path.display()
        ))
    })?;
    if !resolved.starts_with(&canonical_root) {
        return Err(RuntimeValidationError::integrity(format!(
            "runtime symlink escapes its slot: {}",
            path.display()
        )));
    }
    Ok(())
}

fn collect_payload_paths(root: &Path) -> Result<BTreeSet<String>, RuntimeValidationError> {
    fn visit(
        root: &Path,
        directory: &Path,
        paths: &mut BTreeSet<String>,
    ) -> Result<(), RuntimeValidationError> {
        let entries = fs::read_dir(directory).map_err(|error| {
            RuntimeValidationError::integrity(format!(
                "could not inventory runtime directory {}: {error}",
                directory.display()
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                RuntimeValidationError::integrity(format!(
                    "could not inventory runtime directory entry: {error}"
                ))
            })?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                RuntimeValidationError::integrity(format!(
                    "could not inspect runtime path {}: {error}",
                    path.display()
                ))
            })?;
            if metadata.is_dir() {
                visit(root, &path, paths)?;
            } else if metadata.is_file() || metadata.file_type().is_symlink() {
                let relative = path.strip_prefix(root).map_err(|_| {
                    RuntimeValidationError::integrity(format!(
                        "runtime path escaped inventory root: {}",
                        path.display()
                    ))
                })?;
                let relative = slash_path(relative)?;
                if relative != MANIFEST_NAME && relative != SIGNATURE_NAME {
                    paths.insert(relative);
                }
            } else {
                return Err(RuntimeValidationError::integrity(format!(
                    "unsupported runtime filesystem entry: {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    let mut paths = BTreeSet::new();
    visit(root, root, &mut paths)?;
    Ok(paths)
}

fn slash_path(path: &Path) -> Result<String, RuntimeValidationError> {
    let parts = path
        .components()
        .map(|component| match component {
            Component::Normal(value) => value.to_str().map(str::to_owned).ok_or_else(|| {
                RuntimeValidationError::integrity("runtime path is not valid UTF-8")
            }),
            _ => Err(RuntimeValidationError::integrity(
                "runtime path is not a safe relative path",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(parts.join("/"))
}

fn validate_relative_manifest_path(path: &str) -> Result<(), RuntimeValidationError> {
    if path.is_empty() || path.contains('\\') || path.starts_with('/') || path.contains('\0') {
        return Err(RuntimeValidationError::integrity(format!(
            "unsafe runtime manifest path '{path}'"
        )));
    }
    let parsed = Path::new(path);
    if parsed
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(RuntimeValidationError::integrity(format!(
            "unsafe runtime manifest path '{path}'"
        )));
    }
    Ok(())
}

fn validate_runtime_id(runtime_id: &str) -> Result<(), RuntimeValidationError> {
    let Some(digest) = runtime_id.strip_prefix("sha256:") else {
        return Err(RuntimeValidationError::state(format!(
            "runtime ID must use sha256: identity: {runtime_id}"
        )));
    };
    validate_sha256("runtime_id", digest).map_err(|error| {
        RuntimeValidationError::state(format!("invalid runtime ID {runtime_id}: {}", error.detail))
    })
}

fn validate_sha256(field: &str, digest: &str) -> Result<(), RuntimeValidationError> {
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(RuntimeValidationError::integrity(format!(
            "{field} must be a lowercase 64-character SHA-256 digest"
        )))
    }
}

fn python_relative_path(version: &str, platform: &str) -> Result<PathBuf, RuntimeValidationError> {
    let mut parts = version.split('.');
    let major = parts.next().unwrap_or_default();
    let minor = parts.next().unwrap_or_default();
    if major.is_empty()
        || minor.is_empty()
        || !major.bytes().all(|byte| byte.is_ascii_digit())
        || !minor.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(RuntimeValidationError::dependencies(format!(
            "invalid Python version '{version}'"
        )));
    }
    if normalize_platform(platform) == "windows" {
        Ok(PathBuf::from(WINDOWS_X64_SPEC.python_relative_path))
    } else {
        Ok(PathBuf::from(format!("bin/python{major}.{minor}")))
    }
}

fn canonical_manifest_bytes(manifest: &RuntimeManifest) -> Result<Vec<u8>, RuntimeValidationError> {
    canonical_json_bytes(manifest)
}

fn manifest_runtime_id(manifest: &RuntimeManifest) -> Result<String, RuntimeValidationError> {
    let identity = RuntimeManifestIdentity {
        schema: &manifest.schema,
        platform: &manifest.platform,
        architecture: &manifest.architecture,
        minimum_macos: manifest.minimum_macos.as_deref(),
        python: &manifest.python,
        torch: &manifest.torch,
        numpy: &manifest.numpy,
        engine_protocol: manifest.engine_protocol,
        research_closure_sha256: &manifest.research_closure_sha256,
        source_revision: &manifest.source_revision,
        build_epoch: manifest.build_epoch,
        files: &manifest.files,
        sbom_sha256: &manifest.sbom_sha256,
        notices_sha256: &manifest.notices_sha256,
    };
    let bytes = canonical_json_bytes(&identity)?;
    Ok(format!("sha256:{}", sha256_bytes(&bytes)))
}

fn canonical_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, RuntimeValidationError> {
    fn write_value(
        value: &serde_json::Value,
        output: &mut Vec<u8>,
    ) -> Result<(), RuntimeValidationError> {
        match value {
            serde_json::Value::Null => output.extend_from_slice(b"null"),
            serde_json::Value::Bool(boolean) => {
                output.extend_from_slice(if *boolean { b"true" } else { b"false" })
            }
            serde_json::Value::Number(number) => {
                output.extend_from_slice(number.to_string().as_bytes())
            }
            serde_json::Value::String(string) => {
                serde_json::to_writer(output, string).map_err(|error| {
                    RuntimeValidationError::integrity(format!(
                        "could not canonicalize JSON string: {error}"
                    ))
                })?
            }
            serde_json::Value::Array(values) => {
                output.push(b'[');
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        output.push(b',');
                    }
                    write_value(value, output)?;
                }
                output.push(b']');
            }
            serde_json::Value::Object(object) => {
                output.push(b'{');
                let mut keys = object.keys().collect::<Vec<_>>();
                // These schemas use ASCII object keys, for which UTF-8 and
                // RFC 8785's UTF-16 key ordering are identical.
                keys.sort_unstable();
                for (index, key) in keys.into_iter().enumerate() {
                    if index != 0 {
                        output.push(b',');
                    }
                    serde_json::to_writer(&mut *output, key).map_err(|error| {
                        RuntimeValidationError::integrity(format!(
                            "could not canonicalize JSON key: {error}"
                        ))
                    })?;
                    output.push(b':');
                    write_value(&object[key], output)?;
                }
                output.push(b'}');
            }
        }
        Ok(())
    }

    let value = serde_json::to_value(value).map_err(|error| {
        RuntimeValidationError::integrity(format!(
            "could not serialize canonical JSON value: {error}"
        ))
    })?;
    let mut output = Vec::new();
    write_value(&value, &mut output)?;
    Ok(output)
}

fn sha256_file(path: &Path) -> Result<String, RuntimeValidationError> {
    let mut file = File::open(path).map_err(|error| {
        RuntimeValidationError::integrity(format!(
            "could not open runtime file {}: {error}",
            path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            RuntimeValidationError::integrity(format!(
                "could not hash runtime file {}: {error}",
                path.display()
            ))
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(hasher.finalize().as_slice()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(hasher.finalize().as_slice())
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

#[derive(Debug)]
struct BoundedProcessOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    timed_out: bool,
    duration: Duration,
}

fn read_diagnostic_stream(mut stream: impl Read) -> std::io::Result<String> {
    let mut captured = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = MAX_DIAGNOSTIC_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(String::from_utf8_lossy(&captured).trim().to_owned())
}

fn run_bounded_subprocess(
    command: &mut Command,
    timeout: Duration,
) -> Result<BoundedProcessOutput, RuntimeValidationError> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let started = Instant::now();
    let mut child = command.spawn().map_err(|error| {
        RuntimeValidationError::smoke_failed(format!(
            "could not launch runtime smoke subprocess: {error}"
        ))
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RuntimeValidationError::smoke_failed("smoke stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| RuntimeValidationError::smoke_failed("smoke stderr was not captured"))?;
    let (stdout_sender, stdout_receiver) = std::sync::mpsc::sync_channel(1);
    let (stderr_sender, stderr_receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = stdout_sender.send(read_diagnostic_stream(stdout));
    });
    std::thread::spawn(move || {
        let _ = stderr_sender.send(read_diagnostic_stream(stderr));
    });
    let process_group = child.id();
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            RuntimeValidationError::smoke_failed(format!(
                "could not poll runtime smoke subprocess: {error}"
            ))
        })? {
            break (status, false);
        }
        if started.elapsed() >= timeout {
            terminate_smoke_process_group(process_group);
            let _ = child.kill();
            let status = child.wait().map_err(|error| {
                RuntimeValidationError::smoke_failed(format!(
                    "could not reap timed-out runtime smoke subprocess: {error}"
                ))
            })?;
            break (status, true);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = receive_diagnostic_stream(stdout_receiver, process_group, "stdout")?;
    let stderr = receive_diagnostic_stream(stderr_receiver, process_group, "stderr")?;
    Ok(BoundedProcessOutput {
        status,
        stdout,
        stderr,
        timed_out,
        duration: started.elapsed(),
    })
}

fn receive_diagnostic_stream(
    receiver: std::sync::mpsc::Receiver<std::io::Result<String>>,
    process_group: u32,
    label: &str,
) -> Result<String, RuntimeValidationError> {
    let receive = match receiver.recv_timeout(Duration::from_millis(250)) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            terminate_smoke_process_group(process_group);
            receiver
                .recv_timeout(Duration::from_millis(250))
                .map_err(|error| {
                    RuntimeValidationError::smoke_failed(format!(
                        "runtime smoke {label} did not close after process termination: {error}"
                    ))
                })?
        }
        Err(error) => {
            return Err(RuntimeValidationError::smoke_failed(format!(
                "runtime smoke {label} reader disconnected: {error}"
            )));
        }
    };
    receive.map_err(|error| {
        RuntimeValidationError::smoke_failed(format!(
            "could not read runtime smoke {label}: {error}"
        ))
    })
}

#[cfg(unix)]
fn terminate_smoke_process_group(process_group: u32) {
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &format!("-{process_group}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn terminate_smoke_process_group(_process_group: u32) {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeSmokeProtocol {
    schema: String,
    python: String,
    torch: String,
    numpy: String,
    machine: String,
    platform: String,
    executable: String,
    torch_file: String,
    numpy_file: String,
}

pub(crate) fn smoke_verified_runtime(
    runtime: &VerifiedRuntime,
    timeout: Duration,
) -> Result<RuntimeSelfTest, RuntimeValidationError> {
    let probe = format!(
        r#"import json, platform, sys
import numpy, torch
print("REYN_RUNTIME_SMOKE " + json.dumps({{"schema":"{SMOKE_SCHEMA}","python":platform.python_version(),"torch":torch.__version__.split("+",1)[0],"numpy":numpy.__version__.split("+",1)[0],"machine":platform.machine(),"platform":sys.platform,"executable":sys.executable,"torch_file":torch.__file__,"numpy_file":numpy.__file__}}, sort_keys=True, separators=(",", ":")))"#
    );
    let output = run_bounded_subprocess(
        Command::new(&runtime.python).args(["-B", "-I", "-c", &probe]),
        timeout,
    )?;
    let duration_ms = output.duration.as_millis().min(u32::MAX as u128) as u32;
    if output.timed_out {
        return Err(RuntimeValidationError::smoke_timeout(format!(
            "runtime {} smoke timed out after {} ms; stdout: {}; stderr: {}",
            runtime.manifest.runtime_id,
            duration_ms,
            diagnostic_or_empty(&output.stdout),
            diagnostic_or_empty(&output.stderr)
        )));
    }
    if !output.status.success() {
        return Err(RuntimeValidationError::smoke_failed(format!(
            "runtime {} smoke exited with {}; stdout: {}; stderr: {}",
            runtime.manifest.runtime_id,
            output.status,
            diagnostic_or_empty(&output.stdout),
            diagnostic_or_empty(&output.stderr)
        )));
    }
    let line = output
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("REYN_RUNTIME_SMOKE "))
        .ok_or_else(|| {
            RuntimeValidationError::smoke_failed(format!(
                "runtime smoke omitted protocol line; stdout: {}; stderr: {}",
                diagnostic_or_empty(&output.stdout),
                diagnostic_or_empty(&output.stderr)
            ))
        })?;
    let protocol: RuntimeSmokeProtocol = serde_json::from_str(line).map_err(|error| {
        RuntimeValidationError::smoke_failed(format!(
            "runtime smoke returned invalid protocol metadata: {error}; stdout: {}",
            diagnostic_or_empty(&output.stdout)
        ))
    })?;
    validate_smoke_protocol(runtime, &protocol)?;
    Ok(RuntimeSelfTest {
        runtime_id: runtime.manifest.runtime_id.clone(),
        passed: true,
        completed_epoch: current_epoch(),
        result_code: "runtime.smoke_passed".into(),
        duration_ms,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn diagnostic_or_empty(value: &str) -> &str {
    if value.is_empty() {
        "(empty)"
    } else {
        value
    }
}

fn validate_smoke_protocol(
    runtime: &VerifiedRuntime,
    protocol: &RuntimeSmokeProtocol,
) -> Result<(), RuntimeValidationError> {
    validate_smoke_protocol_metadata(&runtime.manifest.platform, protocol)?;
    let canonical_root = fs::canonicalize(&runtime.root).map_err(|error| {
        RuntimeValidationError::integrity(format!(
            "could not canonicalize runtime {}: {error}",
            runtime.root.display()
        ))
    })?;
    for (field, path) in [
        ("executable", protocol.executable.as_str()),
        ("torch_file", protocol.torch_file.as_str()),
        ("numpy_file", protocol.numpy_file.as_str()),
    ] {
        let canonical = fs::canonicalize(path).map_err(|error| {
            RuntimeValidationError::smoke_failed(format!(
                "runtime smoke path {field} cannot be resolved: {error}"
            ))
        })?;
        if !canonical.starts_with(&canonical_root) {
            return Err(RuntimeValidationError::integrity(format!(
                "runtime smoke resolved {field} outside the verified slot"
            )));
        }
    }
    Ok(())
}

fn validate_smoke_protocol_metadata(
    manifest_platform: &str,
    protocol: &RuntimeSmokeProtocol,
) -> Result<(), RuntimeValidationError> {
    let spec = if normalize_platform(manifest_platform) == "windows" {
        &WINDOWS_X64_SPEC
    } else {
        &MACOS_ARM64_SPEC
    };
    for (field, found, expected) in [
        ("schema", protocol.schema.as_str(), SMOKE_SCHEMA),
        ("python", protocol.python.as_str(), PYTHON_VERSION),
        ("torch", protocol.torch.as_str(), TORCH_VERSION),
        ("numpy", protocol.numpy.as_str(), NUMPY_VERSION),
    ] {
        if found != expected {
            return Err(RuntimeValidationError::smoke_failed(format!(
                "runtime smoke reported {field}={found}, expected {expected}"
            )));
        }
    }
    let architecture = normalize_architecture(&protocol.machine);
    if architecture != "arm64" && architecture != "x86_64" {
        return Err(RuntimeValidationError::smoke_failed(format!(
            "runtime smoke reported unknown machine={}",
            protocol.machine
        )));
    }
    if architecture != spec.architecture {
        return Err(RuntimeValidationError::smoke_failed(format!(
            "runtime smoke reported machine={}, normalized to {architecture}, expected {}",
            protocol.machine, spec.architecture
        )));
    }
    let platform = normalize_platform(&protocol.platform);
    if platform != "macos" && platform != "windows" {
        return Err(RuntimeValidationError::smoke_failed(format!(
            "runtime smoke reported unknown platform={}",
            protocol.platform
        )));
    }
    if platform != spec.platform {
        return Err(RuntimeValidationError::smoke_failed(format!(
            "runtime smoke reported platform={}, normalized to {platform}, expected {}",
            protocol.platform, spec.platform
        )));
    }
    Ok(())
}

pub(crate) fn current_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[allow(dead_code)] // Called by the future offline-media installer.
#[derive(Debug)]
pub(crate) struct ValidatedCandidate {
    runtime: VerifiedRuntime,
    self_test: RuntimeSelfTest,
}

#[allow(dead_code)] // Called by the future offline-media installer.
pub(crate) struct CandidateValidationRequest<'a> {
    pub candidate_root: &'a Path,
    pub host: &'a HostCompatibility,
    pub expected_research_closure_sha256: &'a str,
    pub timeout: Duration,
}

#[allow(dead_code)] // Called by the future offline-media installer.
pub(crate) fn validate_staged_candidate(
    request: CandidateValidationRequest<'_>,
) -> Result<ValidatedCandidate, RuntimeValidationError> {
    let runtime = validate_runtime_root(
        request.candidate_root,
        RuntimeSource::ManagedCandidate,
        request.host,
        request.expected_research_closure_sha256,
        None,
    )?;
    let self_test = smoke_verified_runtime(&runtime, request.timeout)?;
    Ok(ValidatedCandidate { runtime, self_test })
}

#[allow(dead_code)] // None in production; fault variant is test-only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivationFault {
    None,
    AfterSlotPublish,
}

#[allow(dead_code)] // Called by the future offline-media installer.
pub(crate) fn activate_validated_candidate(
    managed_root: &Path,
    candidate: ValidatedCandidate,
    factory_runtime_id: Option<String>,
    app_version: &str,
    activation_epoch: u64,
) -> Result<RuntimeState, RuntimeValidationError> {
    activate_validated_candidate_impl(
        managed_root,
        candidate,
        factory_runtime_id,
        app_version,
        activation_epoch,
        ActivationFault::None,
    )
}

#[allow(dead_code)] // Reachable through the offline installer API and fault tests.
fn activate_validated_candidate_impl(
    managed_root: &Path,
    candidate: ValidatedCandidate,
    factory_runtime_id: Option<String>,
    app_version: &str,
    activation_epoch: u64,
    fault: ActivationFault,
) -> Result<RuntimeState, RuntimeValidationError> {
    let runtime_id = candidate.runtime.manifest.runtime_id.clone();
    let expected_stage = managed_root
        .join("staging")
        .join(runtime_slot_name(&runtime_id)?)
        .join("ReynPython");
    if candidate.runtime.root != expected_stage {
        return Err(RuntimeValidationError::activation(format!(
            "validated candidate is outside its deterministic staging path: {}",
            candidate.runtime.root.display()
        )));
    }
    let existing = load_runtime_state(managed_root)?
        .unwrap_or_else(|| RuntimeState::empty(app_version.to_owned()));
    let mut next = existing.activate(
        runtime_id.clone(),
        factory_runtime_id,
        app_version.to_owned(),
        activation_epoch,
    )?;
    next.last_self_test = Some(candidate.self_test);

    let slots_root = managed_root.join("slots");
    let slot_root = slots_root.join(runtime_slot_name(&runtime_id)?);
    let published_runtime = slot_root.join("ReynPython");
    if published_runtime.exists() || slot_root.exists() {
        return Err(RuntimeValidationError::activation(format!(
            "runtime slot already exists and is immutable: {}",
            slot_root.display()
        )));
    }
    fs::create_dir_all(&slot_root).map_err(|error| {
        RuntimeValidationError::activation(format!(
            "could not create runtime slot {}: {error}",
            slot_root.display()
        ))
    })?;
    publish_path_atomic(&candidate.runtime.root, &published_runtime, false).map_err(|error| {
        RuntimeValidationError::activation(format!(
            "could not atomically publish validated runtime slot: {error}"
        ))
    })?;
    sync_directory(&slot_root)?;
    sync_directory(&slots_root)?;
    if fault == ActivationFault::AfterSlotPublish {
        return Err(RuntimeValidationError::activation(
            "injected interruption after slot publication",
        ));
    }
    write_runtime_state_atomic(managed_root, &next)?;
    Ok(next)
}

#[allow(dead_code)] // Used by offline activation and garbage collection.
fn sync_directory(path: &Path) -> Result<(), RuntimeValidationError> {
    sync_directory_platform(path).map_err(|error| {
        RuntimeValidationError::activation(format!(
            "could not sync runtime directory {}: {error}",
            path.display()
        ))
    })
}

#[cfg(not(target_os = "windows"))]
fn sync_directory_platform(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(target_os = "windows")]
fn sync_directory_platform(_path: &Path) -> std::io::Result<()> {
    // Windows does not permit opening directory handles through std::fs::File.
    // Publication uses MoveFileExW(MOVEFILE_WRITE_THROUGH), which waits for the
    // directory metadata update to reach disk.
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn publish_path_atomic(source: &Path, destination: &Path, _replace: bool) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(target_os = "windows")]
fn publish_path_atomic(source: &Path, destination: &Path, replace: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeHealthContext {
    pub managed_root: PathBuf,
    pub runtime_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeHealthFailureKind {
    Startup,
    Crash,
}

impl RuntimeHealthFailureKind {
    fn label(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Crash => "crash",
        }
    }

    fn threshold(self) -> u32 {
        match self {
            Self::Startup => STARTUP_FAILURE_ROLLBACK_THRESHOLD,
            Self::Crash => CRASH_ROLLBACK_THRESHOLD,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeFailureTransition {
    pub count: u32,
    pub rolled_back: bool,
    pub selected_runtime_id: Option<String>,
}

pub(crate) fn record_runtime_startup_success(
    context: &RuntimeHealthContext,
    _completed_epoch: u64,
) -> Result<RuntimeState, RuntimeValidationError> {
    let mut state = load_runtime_state(&context.managed_root)?
        .ok_or_else(|| RuntimeValidationError::state("managed runtime state is missing"))?;
    if state.active.as_deref() != Some(context.runtime_id.as_str()) {
        return Err(RuntimeValidationError::state(format!(
            "ignored startup success for non-active runtime {}",
            context.runtime_id
        )));
    }
    let changed = state.health_runtime_id.as_deref() != Some(context.runtime_id.as_str())
        || state.consecutive_startup_failures != 0;
    state.health_runtime_id = Some(context.runtime_id.clone());
    state.consecutive_startup_failures = 0;
    if changed {
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| RuntimeValidationError::state("runtime state generation overflow"))?;
        write_runtime_state_atomic(&context.managed_root, &state)?;
    }
    Ok(state)
}

pub(crate) fn record_runtime_failure(
    context: &RuntimeHealthContext,
    kind: RuntimeHealthFailureKind,
    detail: &str,
    occurred_epoch: u64,
) -> Result<RuntimeFailureTransition, RuntimeValidationError> {
    let mut state = load_runtime_state(&context.managed_root)?
        .ok_or_else(|| RuntimeValidationError::state("managed runtime state is missing"))?;
    if state.active.as_deref() != Some(context.runtime_id.as_str()) {
        return Err(RuntimeValidationError::state(format!(
            "ignored {} failure for non-active runtime {}",
            kind.label(),
            context.runtime_id
        )));
    }
    if state.health_runtime_id.as_deref() != Some(context.runtime_id.as_str()) {
        state.health_runtime_id = Some(context.runtime_id.clone());
        state.consecutive_startup_failures = 0;
        state.consecutive_crashes = 0;
    }
    let count = match kind {
        RuntimeHealthFailureKind::Startup => {
            state.consecutive_startup_failures =
                state.consecutive_startup_failures.saturating_add(1);
            state.consecutive_startup_failures
        }
        RuntimeHealthFailureKind::Crash => {
            state.consecutive_crashes = state.consecutive_crashes.saturating_add(1);
            state.consecutive_crashes
        }
    };
    state.last_failure = Some(RuntimeFailureRecord {
        runtime_id: context.runtime_id.clone(),
        kind: kind.label().into(),
        count,
        occurred_epoch,
        detail: truncate_diagnostic(detail),
    });

    let mut selected_runtime_id = state.active.clone();
    let rolled_back = count >= kind.threshold();
    if rolled_back {
        let failed = context.runtime_id.clone();
        if !state.disabled_runtime_ids.contains(&failed) {
            state.disabled_runtime_ids.push(failed.clone());
            state.disabled_runtime_ids.sort();
        }
        let target = state.rollback_target().map(str::to_owned);
        let managed_target = target
            .as_deref()
            .filter(|target| state.factory_runtime_id.as_deref() != Some(*target))
            .map(str::to_owned);
        state.active = managed_target;
        state.previous = None;
        state.health_runtime_id = state.active.clone();
        state.consecutive_startup_failures = 0;
        state.consecutive_crashes = 0;
        state.last_rollback = Some(RuntimeRollbackRecord {
            failed_runtime_id: failed,
            selected_runtime_id: target.clone(),
            reason: format!(
                "{} failure threshold {} reached",
                kind.label(),
                kind.threshold()
            ),
            occurred_epoch,
        });
        selected_runtime_id = target;
    }
    state.generation = state
        .generation
        .checked_add(1)
        .ok_or_else(|| RuntimeValidationError::state("runtime state generation overflow"))?;
    write_runtime_state_atomic(&context.managed_root, &state)?;
    Ok(RuntimeFailureTransition {
        count,
        rolled_back,
        selected_runtime_id,
    })
}

fn truncate_diagnostic(detail: &str) -> String {
    let mut end = detail.len().min(MAX_DIAGNOSTIC_BYTES);
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail[..end].to_owned()
}

pub(crate) fn record_runtime_request_success(
    context: &RuntimeHealthContext,
    completed_epoch: u64,
) -> Result<RuntimeState, RuntimeValidationError> {
    let mut state = load_runtime_state(&context.managed_root)?
        .ok_or_else(|| RuntimeValidationError::state("managed runtime state is missing"))?;
    if state.active.as_deref() != Some(context.runtime_id.as_str()) {
        return Err(RuntimeValidationError::state(format!(
            "ignored request success for non-active runtime {}",
            context.runtime_id
        )));
    }
    let mut changed = state.health_runtime_id.as_deref() != Some(context.runtime_id.as_str())
        || state.consecutive_startup_failures != 0
        || state.consecutive_crashes != 0;
    state.health_runtime_id = Some(context.runtime_id.clone());
    state.consecutive_startup_failures = 0;
    state.consecutive_crashes = 0;
    let self_test_passed = state
        .last_self_test
        .as_ref()
        .is_some_and(|self_test| self_test.runtime_id == context.runtime_id && self_test.passed);
    if self_test_passed && state.last_known_good.as_deref() != Some(context.runtime_id.as_str()) {
        state.last_known_good = Some(context.runtime_id.clone());
        state.last_known_good_epoch = completed_epoch;
        changed = true;
    }
    if changed {
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| RuntimeValidationError::state("runtime state generation overflow"))?;
        write_runtime_state_atomic(&context.managed_root, &state)?;
    }
    Ok(state)
}

#[allow(dead_code)] // Exposed to the future offline-media maintenance UI.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct GarbageCollectionReport {
    pub deleted: Vec<String>,
    pub retained: Vec<String>,
    pub skipped: Vec<String>,
}

#[allow(dead_code)] // Called by the future offline-media maintenance UI.
pub(crate) fn garbage_collect_runtime_slots(
    managed_root: &Path,
) -> Result<GarbageCollectionReport, RuntimeValidationError> {
    let state = load_runtime_state(managed_root)?
        .unwrap_or_else(|| RuntimeState::empty(env!("CARGO_PKG_VERSION")));
    let protected = [
        state.active.as_deref(),
        state.previous.as_deref(),
        state.last_known_good.as_deref(),
        state.factory_runtime_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<HashSet<_>>();
    let slots_root = managed_root.join("slots");
    let entries = match fs::read_dir(&slots_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GarbageCollectionReport::default());
        }
        Err(error) => {
            return Err(RuntimeValidationError::state(format!(
                "could not enumerate runtime slots {}: {error}",
                slots_root.display()
            )));
        }
    };
    let mut report = GarbageCollectionReport::default();
    for entry in entries {
        let entry = entry.map_err(|error| {
            RuntimeValidationError::state(format!(
                "could not enumerate runtime slot entry: {error}"
            ))
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            RuntimeValidationError::state(format!(
                "could not inspect runtime slot {}: {error}",
                entry.path().display()
            ))
        })?;
        let Some(runtime_id) = runtime_id_from_slot_name(&name) else {
            report.skipped.push(name);
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            report.skipped.push(name);
            continue;
        }
        if protected.contains(runtime_id.as_str()) {
            report.retained.push(runtime_id);
            continue;
        }
        fs::remove_dir_all(entry.path()).map_err(|error| {
            RuntimeValidationError::state(format!(
                "could not garbage-collect runtime slot {}: {error}",
                entry.path().display()
            ))
        })?;
        report.deleted.push(runtime_id);
    }
    report.deleted.sort();
    report.retained.sort();
    report.skipped.sort();
    sync_directory(&slots_root)?;
    Ok(report)
}

pub(crate) fn research_closure_sha256(
    entries: &[(String, PathBuf)],
) -> Result<String, RuntimeValidationError> {
    let mut files = entries
        .iter()
        .map(|(path, source)| {
            validate_relative_manifest_path(path)?;
            let metadata = fs::metadata(source).map_err(|error| {
                RuntimeValidationError::dependencies(format!(
                    "runtime resource {} is missing: {error}",
                    source.display()
                ))
            })?;
            if !metadata.is_file() {
                return Err(RuntimeValidationError::dependencies(format!(
                    "runtime resource is not a file: {}",
                    source.display()
                )));
            }
            Ok(RuntimeFile {
                path: path.clone(),
                size: metadata.len(),
                sha256: sha256_file(source)?,
            })
        })
        .collect::<Result<Vec<_>, RuntimeValidationError>>()?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    if files
        .windows(2)
        .any(|window| window[0].path == window[1].path)
    {
        return Err(RuntimeValidationError::dependencies(
            "runtime research closure contains duplicate paths",
        ));
    }
    let bytes = canonical_json_bytes(&files)?;
    Ok(sha256_bytes(&bytes))
}

fn validate_runtime_state(state: &RuntimeState) -> Result<(), RuntimeValidationError> {
    if state.schema != RUNTIME_STATE_SCHEMA {
        return Err(RuntimeValidationError::state(format!(
            "unsupported runtime state schema '{}'",
            state.schema
        )));
    }
    for runtime_id in [
        state.active.as_deref(),
        state.previous.as_deref(),
        state.factory_runtime_id.as_deref(),
        state.last_known_good.as_deref(),
        state.health_runtime_id.as_deref(),
        state
            .last_self_test
            .as_ref()
            .map(|self_test| self_test.runtime_id.as_str()),
        state
            .last_failure
            .as_ref()
            .map(|failure| failure.runtime_id.as_str()),
        state
            .last_rollback
            .as_ref()
            .map(|rollback| rollback.failed_runtime_id.as_str()),
        state
            .last_rollback
            .as_ref()
            .and_then(|rollback| rollback.selected_runtime_id.as_deref()),
    ]
    .into_iter()
    .flatten()
    .chain(state.disabled_runtime_ids.iter().map(String::as_str))
    {
        validate_runtime_id(runtime_id)?;
    }
    if state
        .disabled_runtime_ids
        .windows(2)
        .any(|ids| ids[0] >= ids[1])
    {
        return Err(RuntimeValidationError::state(
            "disabled runtime IDs must be unique and strictly sorted",
        ));
    }
    if (state.consecutive_startup_failures > 0 || state.consecutive_crashes > 0)
        && state.health_runtime_id.is_none()
    {
        return Err(RuntimeValidationError::state(
            "runtime health counters require health_runtime_id",
        ));
    }
    Ok(())
}

fn load_runtime_state(managed_root: &Path) -> Result<Option<RuntimeState>, RuntimeValidationError> {
    let path = managed_root.join(STATE_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(RuntimeValidationError::state(format!(
                "could not read runtime state {}: {error}",
                path.display()
            )));
        }
    };
    let state: RuntimeState = serde_json::from_slice(&bytes).map_err(|error| {
        RuntimeValidationError::state(format!(
            "runtime state {} is invalid: {error}",
            path.display()
        ))
    })?;
    validate_runtime_state(&state)?;
    let canonical = canonical_json_bytes(&state).map_err(|error| {
        RuntimeValidationError::state(format!(
            "could not canonicalize runtime state: {}",
            error.detail
        ))
    })?;
    if bytes != canonical {
        return Err(RuntimeValidationError::state(format!(
            "runtime state {} is not in deterministic canonical form",
            path.display()
        )));
    }
    Ok(Some(state))
}

#[allow(dead_code)] // Foundation for a later signed/offline installer.
pub(crate) fn write_runtime_state_atomic(
    managed_root: &Path,
    state: &RuntimeState,
) -> Result<(), RuntimeValidationError> {
    validate_runtime_state(state)?;
    fs::create_dir_all(managed_root).map_err(|error| {
        RuntimeValidationError::state(format!(
            "could not create runtime state directory {}: {error}",
            managed_root.display()
        ))
    })?;
    let bytes = canonical_json_bytes(state).map_err(|error| {
        RuntimeValidationError::state(format!(
            "could not canonicalize runtime state: {}",
            error.detail
        ))
    })?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temporary = managed_root.join(format!(
        ".{STATE_NAME}.{}.{}.{}.tmp",
        std::process::id(),
        state.generation,
        nonce
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| {
                RuntimeValidationError::state(format!(
                    "could not create temporary runtime state {}: {error}",
                    temporary.display()
                ))
            })?;
        file.write_all(&bytes).map_err(|error| {
            RuntimeValidationError::state(format!(
                "could not write temporary runtime state {}: {error}",
                temporary.display()
            ))
        })?;
        file.sync_all().map_err(|error| {
            RuntimeValidationError::state(format!(
                "could not sync temporary runtime state {}: {error}",
                temporary.display()
            ))
        })?;
        publish_path_atomic(&temporary, &managed_root.join(STATE_NAME), true).map_err(|error| {
            RuntimeValidationError::state(format!(
                "could not atomically publish runtime state: {error}"
            ))
        })?;
        sync_directory_platform(managed_root).map_err(|error| {
            RuntimeValidationError::state(format!(
                "could not sync runtime state directory {}: {error}",
                managed_root.display()
            ))
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "reyn-runtime-{label}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&root).expect("create fixture root");
            Self { root }
        }

        fn managed_root(&self) -> PathBuf {
            self.root.join("Runtime")
        }

        fn factory_root(&self) -> PathBuf {
            self.root
                .join("Reyn Studio.app/Contents/Resources/ReynPython")
        }

        fn create_runtime(
            &self,
            root: &Path,
            architecture: &str,
            python: &str,
            closure: &str,
        ) -> RuntimeManifest {
            self.create_runtime_with_python(
                root,
                architecture,
                python,
                closure,
                b"python executable",
            )
        }

        fn create_runtime_with_python(
            &self,
            root: &Path,
            architecture: &str,
            python: &str,
            closure: &str,
            python_payload: &[u8],
        ) -> RuntimeManifest {
            let payloads = [
                ("THIRD_PARTY_NOTICES.html", b"notices".as_slice()),
                (target_platform_spec().python_relative_path, python_payload),
                ("lib/runtime.bin", b"native runtime".as_slice()),
                ("runtime-sbom.cdx.json", br#"{"bomFormat":"CycloneDX"}"#),
            ];
            for (relative, bytes) in payloads {
                let path = root.join(relative);
                fs::create_dir_all(path.parent().expect("payload parent")).unwrap();
                fs::write(path, bytes).unwrap();
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(
                    root.join("bin/python3.14"),
                    fs::Permissions::from_mode(0o755),
                )
                .unwrap();
            }
            let mut files = payloads
                .iter()
                .map(|(relative, _)| {
                    let path = root.join(relative);
                    RuntimeFile {
                        path: (*relative).into(),
                        size: fs::metadata(&path).unwrap().len(),
                        sha256: sha256_file(&path).unwrap(),
                    }
                })
                .collect::<Vec<_>>();
            files.sort_by(|left, right| left.path.cmp(&right.path));
            let sbom_sha256 = files
                .iter()
                .find(|file| file.path == SBOM_NAME)
                .unwrap()
                .sha256
                .clone();
            let notices_sha256 = files
                .iter()
                .find(|file| file.path == NOTICES_NAME)
                .unwrap()
                .sha256
                .clone();
            let mut manifest = RuntimeManifest {
                schema: RUNTIME_MANIFEST_SCHEMA.into(),
                runtime_id: String::new(),
                platform: TARGET_PLATFORM.into(),
                architecture: architecture.into(),
                minimum_macos: target_platform_spec().minimum_os.map(str::to_owned),
                python: python.into(),
                torch: TORCH_VERSION.into(),
                numpy: NUMPY_VERSION.into(),
                engine_protocol: ENGINE_PROTOCOL,
                research_closure_sha256: closure.into(),
                source_revision: "fixture-revision".into(),
                build_epoch: 0,
                files,
                sbom_sha256,
                notices_sha256,
            };
            manifest.runtime_id = manifest_runtime_id(&manifest).unwrap();
            fs::write(
                root.join(MANIFEST_NAME),
                canonical_manifest_bytes(&manifest).unwrap(),
            )
            .unwrap();
            manifest
        }

        #[cfg(unix)]
        fn create_staged_with_script(
            &self,
            closure: &str,
            script: &[u8],
        ) -> (RuntimeManifest, PathBuf) {
            let temporary = self.root.join(format!(
                "candidate-{}",
                fs::read_dir(&self.root).unwrap().count()
            ));
            let temporary_runtime = temporary.join("ReynPython");
            let manifest = self.create_runtime_with_python(
                &temporary_runtime,
                TARGET_ARCHITECTURE,
                PYTHON_VERSION,
                closure,
                script,
            );
            let staged = self
                .managed_root()
                .join("staging")
                .join(runtime_slot_name(&manifest.runtime_id).unwrap())
                .join("ReynPython");
            fs::create_dir_all(staged.parent().unwrap()).unwrap();
            fs::rename(temporary_runtime, &staged).unwrap();
            let _ = fs::remove_dir(temporary);
            (manifest, staged)
        }

        fn create_factory(&self, closure: &str) -> RuntimeManifest {
            self.create_runtime(
                &self.factory_root(),
                TARGET_ARCHITECTURE,
                PYTHON_VERSION,
                closure,
            )
        }

        fn create_managed(
            &self,
            architecture: &str,
            python: &str,
            closure: &str,
        ) -> (RuntimeManifest, PathBuf) {
            let staging = self.root.join(format!(
                "staging-{}",
                fs::read_dir(&self.root).unwrap().count()
            ));
            let staging_runtime = staging.join("ReynPython");
            let manifest = self.create_runtime(&staging_runtime, architecture, python, closure);
            let slot = self
                .managed_root()
                .join("slots")
                .join(runtime_slot_name(&manifest.runtime_id).unwrap());
            fs::create_dir_all(&slot).unwrap();
            let runtime = slot.join("ReynPython");
            fs::rename(staging_runtime, &runtime).unwrap();
            let _ = fs::remove_dir(staging);
            (manifest, runtime)
        }

        fn write_state(&self, state: &RuntimeState) {
            write_runtime_state_atomic(&self.managed_root(), state).unwrap();
        }

        fn discover(&self, closure: &str) -> RuntimeDiscovery {
            let managed = self.managed_root();
            let factory = self.factory_root();
            discover_runtime(RuntimeDiscoveryRequest {
                factory_root: Some(&factory),
                managed_root: Some(&managed),
                host: &supported_host(),
                expected_research_closure_sha256: closure,
            })
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn supported_host() -> HostCompatibility {
        HostCompatibility {
            platform: TARGET_PLATFORM.into(),
            architecture: TARGET_ARCHITECTURE.into(),
            macos_version: Some("26.5".into()),
        }
    }

    fn digest(byte: u8) -> String {
        format!("{byte:064x}")
    }

    fn state_with_active(runtime_id: String) -> RuntimeState {
        RuntimeState {
            schema: RUNTIME_STATE_SCHEMA.into(),
            generation: 1,
            active: Some(runtime_id),
            previous: None,
            factory_runtime_id: None,
            last_known_good: None,
            activation_epoch: 1,
            app_version: "0.1.0".into(),
            last_self_test: None,
            last_known_good_epoch: 0,
            health_runtime_id: None,
            consecutive_startup_failures: 0,
            consecutive_crashes: 0,
            disabled_runtime_ids: Vec::new(),
            last_failure: None,
            last_rollback: None,
        }
    }

    #[cfg(unix)]
    fn passing_smoke_script() -> &'static [u8] {
        br#"#!/bin/sh
ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
printf 'REYN_RUNTIME_SMOKE {"schema":"com.reyn.runtime-smoke/1","python":"3.14.6","torch":"2.13.0","numpy":"2.5.1","machine":"arm64","platform":"darwin","executable":"%s/bin/python3.14","torch_file":"%s/lib/runtime.bin","numpy_file":"%s/lib/runtime.bin"}\n' "$ROOT" "$ROOT" "$ROOT"
"#
    }

    #[test]
    fn deterministic_manifest_bytes_and_identity_are_stable() {
        let fixture = Fixture::new("deterministic");
        let first_root = fixture.root.join("first");
        let second_root = fixture.root.join("second");
        let first =
            fixture.create_runtime(&first_root, TARGET_ARCHITECTURE, PYTHON_VERSION, &digest(1));
        let second = fixture.create_runtime(
            &second_root,
            TARGET_ARCHITECTURE,
            PYTHON_VERSION,
            &digest(1),
        );
        assert_eq!(first, second);
        assert_eq!(
            canonical_manifest_bytes(&first).unwrap(),
            canonical_manifest_bytes(&second).unwrap()
        );
        assert_eq!(manifest_runtime_id(&first).unwrap(), first.runtime_id);
        let canonical = canonical_manifest_bytes(&first).unwrap();
        assert!(canonical.starts_with(
            format!(
                r#"{{"architecture":"{TARGET_ARCHITECTURE}","build_epoch":0,"engine_protocol":1"#
            )
            .as_bytes()
        ));
        assert!(!canonical.contains(&b'\n'));
    }

    #[test]
    fn windows_names_and_paths_normalize_without_a_windows_host() {
        assert_eq!(normalize_platform("win32"), "windows");
        assert_eq!(normalize_platform("Windows"), "windows");
        assert_eq!(normalize_architecture("AMD64"), "x86_64");
        assert_eq!(normalize_architecture("x64"), "x86_64");
        assert_eq!(
            python_relative_path(PYTHON_VERSION, "win32").unwrap(),
            PathBuf::from("python.exe")
        );
        assert_eq!(
            factory_runtime_root_for(Path::new("/portable/Reyn Studio.exe"), "windows"),
            Some(PathBuf::from("/portable/ReynPython"))
        );
        assert_eq!(normalize_platform("linux"), "linux");
        assert_eq!(
            factory_runtime_root_for(Path::new("/portable/reyn-studio"), "linux"),
            Some(PathBuf::from("/portable/ReynPython"))
        );
        assert_eq!(
            python_relative_path(PYTHON_VERSION, "linux").unwrap(),
            PathBuf::from("bin/python3.14")
        );
        let runtime_id = format!("sha256:{}", "a".repeat(64));
        let slot_name = runtime_slot_name(&runtime_id).unwrap();
        assert_eq!(slot_name, "a".repeat(64));
        assert!(!slot_name.contains(':'));
        assert_eq!(
            runtime_id_from_slot_name(slot_name).as_deref(),
            Some(runtime_id.as_str())
        );
    }

    #[test]
    fn windows_smoke_protocol_accepts_x86_64_aliases_and_rejects_unknown_machine() {
        let mut protocol = RuntimeSmokeProtocol {
            schema: SMOKE_SCHEMA.into(),
            python: PYTHON_VERSION.into(),
            torch: TORCH_VERSION.into(),
            numpy: NUMPY_VERSION.into(),
            machine: "x86_64".into(),
            platform: "win32".into(),
            executable: "unused".into(),
            torch_file: "unused".into(),
            numpy_file: "unused".into(),
        };
        for machine in ["x86_64", "AMD64", "x64"] {
            protocol.machine = machine.into();
            validate_smoke_protocol_metadata("windows", &protocol).unwrap();
        }
        protocol.machine = "i686".into();
        let error = validate_smoke_protocol_metadata("windows", &protocol).unwrap_err();
        assert!(error.detail.contains("unknown machine=i686"));
    }

    #[test]
    fn windows_manifest_omits_macos_floor_and_accepts_amd64_host() {
        let fixture = Fixture::new("windows-manifest");
        let mut manifest = fixture.create_runtime(
            &fixture.root.join("mac-fixture"),
            TARGET_ARCHITECTURE,
            PYTHON_VERSION,
            &digest(7),
        );
        manifest.platform = "windows".into();
        manifest.architecture = "AMD64".into();
        manifest.minimum_macos = None;
        manifest.runtime_id = manifest_runtime_id(&manifest).unwrap();
        let host = HostCompatibility {
            platform: "win32".into(),
            architecture: "AMD64".into(),
            macos_version: None,
        };
        validate_manifest_metadata_for_spec(&manifest, &host, &WINDOWS_X64_SPEC, &digest(7), None)
            .unwrap();
        let canonical = canonical_manifest_bytes(&manifest).unwrap();
        assert!(!String::from_utf8(canonical)
            .unwrap()
            .contains("minimum_macos"));
    }

    #[test]
    fn valid_active_managed_slot_wins_over_factory() {
        let fixture = Fixture::new("active");
        let closure = digest(2);
        fixture.create_factory(&closure);
        let (managed, _) = fixture.create_managed(TARGET_ARCHITECTURE, PYTHON_VERSION, &closure);
        fixture.write_state(&state_with_active(managed.runtime_id.clone()));

        let discovery = fixture.discover(&closure);
        let selected = discovery.selected.expect("managed runtime");
        assert_eq!(selected.source, RuntimeSource::ManagedActive);
        assert_eq!(selected.manifest.runtime_id, managed.runtime_id);
        assert!(discovery.diagnostics.is_empty());
    }

    #[test]
    fn tampered_manifest_falls_back_to_factory() {
        let fixture = Fixture::new("manifest-tamper");
        let closure = digest(3);
        let factory = fixture.create_factory(&closure);
        let (managed, managed_root) =
            fixture.create_managed(TARGET_ARCHITECTURE, PYTHON_VERSION, &closure);
        fixture.write_state(&state_with_active(managed.runtime_id));
        let mut tampered: RuntimeManifest =
            serde_json::from_slice(&fs::read(managed_root.join(MANIFEST_NAME)).unwrap()).unwrap();
        tampered.source_revision = "attacker-revision".into();
        fs::write(
            managed_root.join(MANIFEST_NAME),
            canonical_manifest_bytes(&tampered).unwrap(),
        )
        .unwrap();

        let discovery = fixture.discover(&closure);
        assert_eq!(
            discovery.selected.unwrap().manifest.runtime_id,
            factory.runtime_id
        );
        assert!(discovery
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "runtime.integrity"));
        assert!(discovery
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "runtime.fallback_factory"));
    }

    #[test]
    fn tampered_file_and_partial_slot_never_execute() {
        for mode in ["tampered", "partial"] {
            let fixture = Fixture::new(mode);
            let closure = digest(4);
            let factory = fixture.create_factory(&closure);
            let (managed, managed_root) =
                fixture.create_managed(TARGET_ARCHITECTURE, PYTHON_VERSION, &closure);
            fixture.write_state(&state_with_active(managed.runtime_id));
            if mode == "tampered" {
                fs::write(managed_root.join("lib/runtime.bin"), b"altered").unwrap();
            } else {
                fs::remove_file(managed_root.join("lib/runtime.bin")).unwrap();
            }

            let discovery = fixture.discover(&closure);
            assert_eq!(
                discovery.selected.unwrap().manifest.runtime_id,
                factory.runtime_id
            );
            assert!(discovery
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "runtime.integrity"));
        }
    }

    #[test]
    fn wrong_architecture_and_dependency_version_fall_back() {
        let wrong_architecture = if TARGET_ARCHITECTURE == "arm64" {
            "x86_64"
        } else {
            "arm64"
        };
        for (architecture, python, expected_code) in [
            (wrong_architecture, PYTHON_VERSION, "runtime.platform"),
            (TARGET_ARCHITECTURE, "3.13.9", "runtime.dependencies"),
        ] {
            let fixture = Fixture::new(expected_code);
            let closure = digest(5);
            let factory = fixture.create_factory(&closure);
            let (managed, _) = fixture.create_managed(architecture, python, &closure);
            fixture.write_state(&state_with_active(managed.runtime_id));

            let discovery = fixture.discover(&closure);
            assert_eq!(
                discovery.selected.unwrap().manifest.runtime_id,
                factory.runtime_id
            );
            assert!(discovery
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == expected_code));
        }
    }

    #[test]
    fn stale_active_pointer_falls_back_to_factory() {
        let fixture = Fixture::new("stale-pointer");
        let closure = digest(6);
        let factory = fixture.create_factory(&closure);
        fixture.write_state(&state_with_active(format!("sha256:{}", digest(15))));

        let discovery = fixture.discover(&closure);
        assert_eq!(
            discovery.selected.unwrap().manifest.runtime_id,
            factory.runtime_id
        );
        assert!(discovery
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "runtime.missing"));
        assert!(discovery
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "runtime.fallback_factory"));
    }

    #[test]
    fn valid_previous_slot_is_a_bounded_rollback_target() {
        let fixture = Fixture::new("previous");
        let closure = digest(7);
        let (previous, _) = fixture.create_managed(TARGET_ARCHITECTURE, PYTHON_VERSION, &closure);
        let mut state = state_with_active(format!("sha256:{}", digest(14)));
        state.previous = Some(previous.runtime_id.clone());
        fixture.write_state(&state);

        let discovery = fixture.discover(&closure);
        let selected = discovery.selected.expect("rollback runtime");
        assert_eq!(selected.source, RuntimeSource::ManagedRollback);
        assert_eq!(selected.manifest.runtime_id, previous.runtime_id);
        assert!(discovery
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "runtime.fallback_previous"));
    }

    #[test]
    fn incompatible_host_does_not_attempt_factory_runtime() {
        let fixture = Fixture::new("intel");
        let closure = digest(8);
        fixture.create_factory(&closure);
        let factory = fixture.factory_root();
        let managed = fixture.managed_root();
        let incompatible_architecture = if TARGET_ARCHITECTURE == "arm64" {
            "x86_64"
        } else {
            "arm64"
        };
        let discovery = discover_runtime(RuntimeDiscoveryRequest {
            factory_root: Some(&factory),
            managed_root: Some(&managed),
            host: &HostCompatibility {
                platform: TARGET_PLATFORM.into(),
                architecture: incompatible_architecture.into(),
                macos_version: Some("26.5".into()),
            },
            expected_research_closure_sha256: &closure,
        });
        assert!(discovery.selected.is_none());
        assert_eq!(discovery.diagnostics[0].code, "runtime.platform");
        assert_eq!(discovery.diagnostics[1].code, "runtime.missing");
    }

    #[test]
    fn missing_runtime_is_distinct_from_dependency_failure() {
        let fixture = Fixture::new("diagnostics");
        let closure = digest(9);
        let missing = fixture.discover(&closure);
        assert!(missing.selected.is_none());
        assert!(missing
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "runtime.missing"));

        let (managed, _) = fixture.create_managed(TARGET_ARCHITECTURE, "3.13.9", &closure);
        fixture.write_state(&state_with_active(managed.runtime_id));
        let incompatible = fixture.discover(&closure);
        assert!(incompatible.selected.is_none());
        assert!(incompatible
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "runtime.dependencies"));
    }

    #[test]
    fn state_updates_are_atomic_and_keep_rollback_pointer() {
        let fixture = Fixture::new("state");
        let first = format!("sha256:{}", digest(10));
        let second = format!("sha256:{}", digest(11));
        let factory = format!("sha256:{}", digest(12));
        let initial = RuntimeState::empty("0.1.0")
            .activate(first.clone(), Some(factory.clone()), "0.1.0", 10)
            .unwrap();
        fixture.write_state(&initial);
        let updated = load_runtime_state(&fixture.managed_root())
            .unwrap()
            .unwrap()
            .activate(second.clone(), Some(factory), "0.1.1", 20)
            .unwrap();
        fixture.write_state(&updated);
        fs::write(
            fixture.managed_root().join(".state.json.interrupted.tmp"),
            b"partial",
        )
        .unwrap();

        let loaded = load_runtime_state(&fixture.managed_root())
            .unwrap()
            .unwrap();
        assert_eq!(loaded.active.as_deref(), Some(second.as_str()));
        assert_eq!(loaded.previous.as_deref(), Some(first.as_str()));
        assert_eq!(loaded.rollback_target(), Some(first.as_str()));
        assert_eq!(loaded.generation, 2);
    }

    #[cfg(unix)]
    #[test]
    fn staged_smoke_timeout_is_bounded_and_captures_failure() {
        let fixture = Fixture::new("smoke-timeout");
        let closure = digest(20);
        let (_, staged) = fixture.create_staged_with_script(
            &closure,
            b"#!/bin/sh\nprintf 'smoke started\\n' >&2\nwhile :; do :; done\n",
        );
        let started = Instant::now();
        let error = validate_staged_candidate(CandidateValidationRequest {
            candidate_root: &staged,
            host: &supported_host(),
            expected_research_closure_sha256: &closure,
            timeout: Duration::from_millis(200),
        })
        .unwrap_err();
        assert_eq!(error.kind, RuntimeFailureKind::SmokeTimeout);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(staged.is_dir(), "timed-out candidate must remain staged");
    }

    #[cfg(unix)]
    #[test]
    fn failed_smoke_captures_bounded_stderr() {
        let fixture = Fixture::new("smoke-stderr");
        let closure = digest(24);
        let (_, staged) = fixture.create_staged_with_script(
            &closure,
            b"#!/bin/sh\nprintf 'dependency exploded\\n' >&2\nexit 7\n",
        );
        let error = validate_staged_candidate(CandidateValidationRequest {
            candidate_root: &staged,
            host: &supported_host(),
            expected_research_closure_sha256: &closure,
            timeout: Duration::from_secs(1),
        })
        .unwrap_err();
        assert_eq!(error.kind, RuntimeFailureKind::SmokeFailed);
        assert!(error.detail.contains("dependency exploded"));
        assert!(error.detail.contains("exit status"));
    }

    #[cfg(unix)]
    #[test]
    fn invalid_candidate_never_reaches_activation() {
        let fixture = Fixture::new("invalid-candidate");
        let closure = digest(21);
        let (_, staged) = fixture.create_staged_with_script(&closure, passing_smoke_script());
        fs::write(staged.join("lib/runtime.bin"), b"tampered after staging").unwrap();

        let error = validate_staged_candidate(CandidateValidationRequest {
            candidate_root: &staged,
            host: &supported_host(),
            expected_research_closure_sha256: &closure,
            timeout: Duration::from_secs(1),
        })
        .unwrap_err();
        assert_eq!(error.kind, RuntimeFailureKind::Integrity);
        assert!(!fixture.managed_root().join("slots").exists());
    }

    #[cfg(unix)]
    #[test]
    fn activation_interruption_keeps_old_pointer_and_publishes_no_partial_state() {
        let fixture = Fixture::new("activation-interruption");
        let closure = digest(22);
        let old = format!("sha256:{}", digest(30));
        fixture.write_state(&state_with_active(old.clone()));
        let (manifest, staged) =
            fixture.create_staged_with_script(&closure, passing_smoke_script());
        let candidate = validate_staged_candidate(CandidateValidationRequest {
            candidate_root: &staged,
            host: &supported_host(),
            expected_research_closure_sha256: &closure,
            timeout: Duration::from_secs(1),
        })
        .unwrap();

        let error = activate_validated_candidate_impl(
            &fixture.managed_root(),
            candidate,
            None,
            "0.1.0",
            40,
            ActivationFault::AfterSlotPublish,
        )
        .unwrap_err();
        assert_eq!(error.kind, RuntimeFailureKind::Activation);
        let state = load_runtime_state(&fixture.managed_root())
            .unwrap()
            .unwrap();
        assert_eq!(state.active.as_deref(), Some(old.as_str()));
        assert!(fixture
            .managed_root()
            .join("slots")
            .join(runtime_slot_name(&manifest.runtime_id).unwrap())
            .join("ReynPython")
            .is_dir());
        assert!(!staged.exists());
    }

    #[cfg(unix)]
    #[test]
    fn successful_activation_promotes_only_after_a_production_success() {
        let fixture = Fixture::new("promotion");
        let closure = digest(23);
        let old = format!("sha256:{}", digest(31));
        fixture.write_state(&state_with_active(old.clone()));
        let (manifest, staged) =
            fixture.create_staged_with_script(&closure, passing_smoke_script());
        let candidate = validate_staged_candidate(CandidateValidationRequest {
            candidate_root: &staged,
            host: &supported_host(),
            expected_research_closure_sha256: &closure,
            timeout: Duration::from_secs(1),
        })
        .unwrap();
        let activated =
            activate_validated_candidate(&fixture.managed_root(), candidate, None, "0.1.1", 50)
                .unwrap();
        assert_eq!(
            activated.active.as_deref(),
            Some(manifest.runtime_id.as_str())
        );
        assert_eq!(activated.previous.as_deref(), Some(old.as_str()));
        assert_ne!(
            activated.last_known_good.as_deref(),
            Some(manifest.runtime_id.as_str())
        );

        let promoted = record_runtime_request_success(
            &RuntimeHealthContext {
                managed_root: fixture.managed_root(),
                runtime_id: manifest.runtime_id.clone(),
            },
            60,
        )
        .unwrap();
        assert_eq!(
            promoted.last_known_good.as_deref(),
            Some(manifest.runtime_id.as_str())
        );
        assert_eq!(promoted.last_known_good_epoch, 60);
    }

    #[test]
    fn crash_threshold_rolls_back_and_disables_failed_slot() {
        let fixture = Fixture::new("crash-rollback");
        let active = format!("sha256:{}", digest(40));
        let previous = format!("sha256:{}", digest(41));
        let mut state = state_with_active(active.clone());
        state.previous = Some(previous.clone());
        fixture.write_state(&state);
        let context = RuntimeHealthContext {
            managed_root: fixture.managed_root(),
            runtime_id: active.clone(),
        };

        for expected in 1..CRASH_ROLLBACK_THRESHOLD {
            let transition = record_runtime_failure(
                &context,
                RuntimeHealthFailureKind::Crash,
                "sidecar connection closed",
                70 + expected as u64,
            )
            .unwrap();
            assert_eq!(transition.count, expected);
            assert!(!transition.rolled_back);
        }
        let transition = record_runtime_failure(
            &context,
            RuntimeHealthFailureKind::Crash,
            "sidecar connection closed",
            80,
        )
        .unwrap();
        assert!(transition.rolled_back);
        assert_eq!(
            transition.selected_runtime_id.as_deref(),
            Some(previous.as_str())
        );
        let state = load_runtime_state(&fixture.managed_root())
            .unwrap()
            .unwrap();
        assert_eq!(state.active.as_deref(), Some(previous.as_str()));
        assert!(state.disabled_runtime_ids.contains(&active));
        assert_eq!(state.consecutive_crashes, 0);
    }

    #[test]
    fn startup_rollback_never_loops_to_a_disabled_previous_slot() {
        let fixture = Fixture::new("rollback-loop");
        let active = format!("sha256:{}", digest(42));
        let disabled_previous = format!("sha256:{}", digest(43));
        let factory = format!("sha256:{}", digest(44));
        let mut state = state_with_active(active.clone());
        state.previous = Some(disabled_previous.clone());
        state.factory_runtime_id = Some(factory.clone());
        state.disabled_runtime_ids = vec![disabled_previous];
        fixture.write_state(&state);
        let context = RuntimeHealthContext {
            managed_root: fixture.managed_root(),
            runtime_id: active.clone(),
        };

        for index in 1..=STARTUP_FAILURE_ROLLBACK_THRESHOLD {
            let transition = record_runtime_failure(
                &context,
                RuntimeHealthFailureKind::Startup,
                "READY timeout",
                90 + index as u64,
            )
            .unwrap();
            if index == STARTUP_FAILURE_ROLLBACK_THRESHOLD {
                assert!(transition.rolled_back);
                assert_eq!(
                    transition.selected_runtime_id.as_deref(),
                    Some(factory.as_str())
                );
            }
        }
        let state = load_runtime_state(&fixture.managed_root())
            .unwrap()
            .unwrap();
        assert!(
            state.active.is_none(),
            "factory fallback has no managed pointer"
        );
        assert!(state.disabled_runtime_ids.contains(&active));
        assert!(state.previous.is_none());
        assert!(
            state
                .clone()
                .activate(active, Some(factory), "0.1.0", 100)
                .is_err(),
            "a disabled immutable runtime ID must not be reactivated"
        );
    }

    #[test]
    fn garbage_collection_preserves_every_state_pointer() {
        let fixture = Fixture::new("gc-safety");
        let active = format!("sha256:{}", digest(50));
        let previous = format!("sha256:{}", digest(51));
        let last_known_good = format!("sha256:{}", digest(52));
        let factory = format!("sha256:{}", digest(53));
        let orphan = format!("sha256:{}", digest(54));
        let mut state = state_with_active(active.clone());
        state.previous = Some(previous.clone());
        state.last_known_good = Some(last_known_good.clone());
        state.factory_runtime_id = Some(factory.clone());
        fixture.write_state(&state);
        let slots = fixture.managed_root().join("slots");
        for runtime_id in [&active, &previous, &last_known_good, &factory, &orphan] {
            fs::create_dir_all(
                slots
                    .join(runtime_slot_name(runtime_id).unwrap())
                    .join("ReynPython"),
            )
            .unwrap();
        }
        fs::create_dir_all(slots.join("not-a-runtime-id")).unwrap();

        let report = garbage_collect_runtime_slots(&fixture.managed_root()).unwrap();
        assert_eq!(report.deleted, vec![orphan.clone()]);
        for protected in [&active, &previous, &last_known_good, &factory] {
            assert!(slots.join(runtime_slot_name(protected).unwrap()).is_dir());
            assert!(report.retained.contains(protected));
        }
        assert!(slots.join("not-a-runtime-id").is_dir());
        assert_eq!(report.skipped, vec!["not-a-runtime-id"]);
    }
}
