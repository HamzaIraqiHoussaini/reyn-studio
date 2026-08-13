//! Application-level N6 project lifecycle.
//!
//! Project documents remain strict schema-v3 manifests inside self-contained,
//! content-addressed bundles. Machine-local recent paths and recovery snapshots
//! live beside settings and never become authoritative project dependencies.
use crate::project::{
    file_sha256, write_atomic_bytes, AtomicWriteError, BundleSummary, ContentDiagnosticKind,
    ContentInsert, ContentRole, LifecycleState, ProjectDocument, ProjectError, ProjectManifest,
    RunRecord, SourceKind, WritePrecondition,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

const STATE_SCHEMA_VERSION: u32 = 1;
const MAX_RECENT_PROJECTS: usize = 8;
const RECENT_FILE_NAME: &str = "recent-projects.json";
const RECOVERY_DIRECTORY: &str = "recovery";
const RECOVERY_EXTENSION: &str = "reynrecovery";
const AUTOSAVE_RETRY_SECONDS: u64 = 5;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecentProject {
    pub path: PathBuf,
    pub project_id: String,
    pub name: String,
    pub last_opened_utc_unix: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryEntry {
    pub project_id: String,
    pub name: String,
    pub source_path: Option<PathBuf>,
    pub saved_utc_unix: u64,
    recovery_path: PathBuf,
    storage_sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectSummary {
    pub cases: usize,
    pub runs: usize,
    pub evidence: usize,
    pub sources: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectAccessMode {
    Full,
    ReadOnlyEvidence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyKind {
    Engine,
    Model,
    Source,
    Artifact,
    Integrity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyIssue {
    pub kind: DependencyKind,
    pub object_id: String,
    pub content_sha256: Option<String>,
    pub detail: String,
    pub relinkable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectAvailability {
    pub access_mode: ProjectAccessMode,
    pub issues: Vec<DependencyIssue>,
    pub bundle: BundleSummary,
}

impl ProjectAvailability {
    fn empty() -> Self {
        Self {
            access_mode: ProjectAccessMode::Full,
            issues: Vec::new(),
            bundle: BundleSummary {
                required_objects: 0,
                available_objects: 0,
                available_bytes: 0,
                diagnostics: 0,
            },
        }
    }

    pub fn is_read_only_evidence(&self) -> bool {
        self.access_mode == ProjectAccessMode::ReadOnlyEvidence
    }
}

#[derive(Debug)]
pub enum LifecycleError {
    Project(ProjectError),
    Io(String),
    StateJson(String),
    UnsupportedStateSchema {
        kind: &'static str,
        version: u32,
    },
    SaveAsRequired,
    IntegrityRepairRequiresSaveAs,
    RecoveryNotFound(String),
    InvalidRecovery(String),
    RelinkTargetNotRequired(String),
    StateWriteConflict {
        expected_sha256: Option<String>,
        actual_sha256: Option<String>,
    },
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Project(error) => write!(formatter, "{error}"),
            Self::Io(error) => write!(formatter, "project state I/O: {error}"),
            Self::StateJson(error) => write!(formatter, "project state schema: {error}"),
            Self::UnsupportedStateSchema { kind, version } => {
                write!(formatter, "unsupported {kind} schema version {version}")
            }
            Self::SaveAsRequired => write!(formatter, "project needs a Save As location"),
            Self::IntegrityRepairRequiresSaveAs => write!(
                formatter,
                "bundle index integrity failed; preserve the original and repair with Save As"
            ),
            Self::RecoveryNotFound(project_id) => {
                write!(
                    formatter,
                    "recovery snapshot for {project_id} was not found"
                )
            }
            Self::InvalidRecovery(detail) => write!(formatter, "invalid recovery: {detail}"),
            Self::RelinkTargetNotRequired(digest) => {
                write!(formatter, "{digest} is not required by this project")
            }
            Self::StateWriteConflict {
                expected_sha256,
                actual_sha256,
            } => {
                let expected = expected_sha256.as_deref().unwrap_or("no file");
                let actual = actual_sha256.as_deref().unwrap_or("no file");
                write!(
                    formatter,
                    "project state changed before write: expected {expected}, found {actual}"
                )
            }
        }
    }
}

impl std::error::Error for LifecycleError {}

impl From<ProjectError> for LifecycleError {
    fn from(error: ProjectError) -> Self {
        Self::Project(error)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecentDocument {
    schema_version: u32,
    projects: Vec<RecentProject>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecoveryDocument {
    schema_version: u32,
    project_id: String,
    project_name: String,
    source_path: Option<PathBuf>,
    saved_utc_unix: u64,
    project_document: serde_json::Value,
}

#[derive(Clone, Debug)]
struct OwnedRecovery {
    project_id: String,
    storage_sha256: String,
}

/// Active project plus machine-local lifecycle state. No method in this type
/// starts or depends on the Python engine.
pub struct ProjectLifecycle {
    document: ProjectDocument,
    path: Option<PathBuf>,
    dirty: bool,
    recovered: bool,
    state_directory: PathBuf,
    recent_projects: Vec<RecentProject>,
    recovery_entries: Vec<RecoveryEntry>,
    last_autosave_attempt_utc_unix: u64,
    last_autosave_success_utc_unix: u64,
    on_disk_sha256: Option<String>,
    owned_recovery: Option<OwnedRecovery>,
    availability: ProjectAvailability,
}

impl ProjectLifecycle {
    /// Load local recent/recovery state while always returning a usable blank
    /// project. Malformed local state is reported and isolated.
    pub fn load(state_directory: impl Into<PathBuf>, now_utc_unix: u64) -> (Self, Vec<String>) {
        let state_directory = state_directory.into();
        let mut warnings = Vec::new();
        let recent_projects = match load_recent_projects(&state_directory) {
            Ok(projects) => projects,
            Err(error) => {
                warnings.push(format!("Recent projects were not loaded: {error}"));
                Vec::new()
            }
        };
        let recovery_entries = match load_recovery_entries(&state_directory) {
            Ok((entries, recovery_warnings)) => {
                warnings.extend(recovery_warnings);
                entries
            }
            Err(error) => {
                warnings.push(format!("Recovery snapshots were not loaded: {error}"));
                Vec::new()
            }
        };
        (
            Self {
                document: ProjectDocument::new(ProjectManifest::new(
                    "Unsaved project",
                    now_utc_unix,
                )),
                path: None,
                dirty: false,
                recovered: false,
                state_directory,
                recent_projects,
                recovery_entries,
                last_autosave_attempt_utc_unix: now_utc_unix,
                last_autosave_success_utc_unix: now_utc_unix,
                on_disk_sha256: None,
                owned_recovery: None,
                availability: ProjectAvailability::empty(),
            },
            warnings,
        )
    }

    pub fn manifest(&self) -> &ProjectManifest {
        self.document.manifest()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn is_recovered(&self) -> bool {
        self.recovered
    }

    pub fn display_name(&self) -> &str {
        self.document.manifest().name()
    }

    pub fn recent_projects(&self) -> &[RecentProject] {
        &self.recent_projects
    }

    pub fn recovery_entries(&self) -> &[RecoveryEntry] {
        &self.recovery_entries
    }

    pub fn summary(&self) -> ProjectSummary {
        ProjectSummary {
            cases: self.document.manifest().cases().len(),
            runs: self.document.manifest().run_count(),
            evidence: self.document.manifest().evidence().len(),
            sources: self.document.manifest().source_revisions().len(),
        }
    }

    pub fn availability(&self) -> &ProjectAvailability {
        &self.availability
    }

    pub fn content_bytes(&self, digest: &str) -> Option<&[u8]> {
        self.document.content_bytes(digest)
    }

    pub fn content_state(&self, digest: &str) -> crate::project::ContentState {
        self.document.content_state(digest)
    }

    pub fn add_content_with_digest(
        &mut self,
        bytes: Vec<u8>,
        media_type: impl Into<String>,
        expected_digest: &str,
    ) -> Result<ContentInsert, LifecycleError> {
        let insert = self
            .document
            .add_content_with_digest(bytes, media_type, expected_digest)?;
        if !insert.deduplicated {
            self.dirty = true;
        }
        Ok(insert)
    }

    pub fn add_digested_content(
        &mut self,
        digested: crate::project::DigestedBytes,
        media_type: impl Into<String>,
    ) -> ContentInsert {
        let insert = self.document.add_digested_content(digested, media_type);
        if !insert.deduplicated {
            self.dirty = true;
        }
        insert
    }

    pub fn relink_content(
        &mut self,
        expected_digest: &str,
        path: &Path,
    ) -> Result<ContentInsert, LifecycleError> {
        if !self
            .document
            .manifest()
            .content_references()
            .contains_key(&expected_digest.to_ascii_lowercase())
        {
            return Err(LifecycleError::RelinkTargetNotRequired(
                expected_digest.into(),
            ));
        }
        let insert =
            self.document
                .relink_content(expected_digest, path, "application/octet-stream")?;
        self.dirty = true;
        Ok(insert)
    }

    pub fn new_project(&mut self, now_utc_unix: u64) {
        self.document = ProjectDocument::new(ProjectManifest::new("Unsaved project", now_utc_unix));
        self.path = None;
        self.dirty = true;
        self.recovered = false;
        self.last_autosave_attempt_utc_unix = now_utc_unix;
        self.last_autosave_success_utc_unix = now_utc_unix;
        self.on_disk_sha256 = None;
        self.owned_recovery = None;
        self.availability = ProjectAvailability::empty();
    }

    /// Project naming is metadata-only. The clone-before-commit pattern keeps
    /// immutable run/evidence objects untouched.
    pub fn rename_project(&mut self, name: impl Into<String>, now_utc_unix: u64) {
        let mut candidate = self.document.manifest().clone();
        candidate.rename(name, now_utc_unix);
        self.document.replace_manifest(candidate);
        self.dirty = true;
    }

    /// Apply a domain edit to a cloned manifest and commit it only after every
    /// lineage and integrity precondition in the edit succeeds.
    pub fn transact<T>(
        &mut self,
        now_utc_unix: u64,
        edit: impl FnOnce(&mut ProjectManifest) -> Result<T, ProjectError>,
    ) -> Result<T, LifecycleError> {
        let mut candidate = self.document.manifest().clone();
        let result = edit(&mut candidate)?;
        self.document.replace_manifest(candidate);
        self.dirty = true;
        self.last_autosave_attempt_utc_unix = self.last_autosave_attempt_utc_unix.min(now_utc_unix);
        Ok(result)
    }

    /// Recompute transient execution availability from the current engine and
    /// validated model inventory. This never edits the manifest, immutable
    /// runs, evidence, or dirty state.
    pub fn reconcile_dependencies<'a>(
        &mut self,
        engine_available: bool,
        available_model_sha256: impl IntoIterator<Item = &'a str>,
    ) {
        let manifest = self.document.manifest();
        let has_reviewable_history = manifest.run_count() > 0 || !manifest.evidence().is_empty();
        let available_models: BTreeSet<String> = available_model_sha256
            .into_iter()
            .filter(|digest| is_sha256(digest))
            .map(str::to_ascii_lowercase)
            .collect();
        let mut issues = Vec::new();
        if has_reviewable_history && !engine_available {
            issues.push(DependencyIssue {
                kind: DependencyKind::Engine,
                object_id: "python-engine".into(),
                content_sha256: None,
                detail: "Compute engine unavailable; stored runs and evidence remain reviewable, but new engine-backed runs are blocked.".into(),
                relinkable: false,
            });
        }

        let mut required_models = BTreeSet::new();
        for case in manifest.cases() {
            for source_id in &case.active_revision().source_revision_ids {
                if let Some(source) = manifest
                    .source_revisions()
                    .iter()
                    .find(|source| source.source_revision_id == *source_id)
                {
                    if source.source_kind == SourceKind::Model {
                        required_models.insert((
                            source.content_sha256.to_ascii_lowercase(),
                            source.source_revision_id.clone(),
                        ));
                    }
                }
            }
        }
        if engine_available {
            for (digest, source_id) in required_models {
                if !available_models.contains(&digest) {
                    issues.push(DependencyIssue {
                        kind: DependencyKind::Model,
                        object_id: source_id,
                        content_sha256: Some(digest.clone()),
                        detail: format!(
                            "Model {} is not in the validated engine inventory. Its bundled bytes remain available for provenance, but rerun is blocked.",
                            short_hash(&digest)
                        ),
                        relinkable: false,
                    });
                }
            }
        }

        for diagnostic in self.document.diagnostics() {
            let kind = match diagnostic.kind {
                ContentDiagnosticKind::BundleIntegrity => DependencyKind::Integrity,
                ContentDiagnosticKind::Duplicate => DependencyKind::Integrity,
                ContentDiagnosticKind::Missing
                | ContentDiagnosticKind::Corrupt
                | ContentDiagnosticKind::SizeMismatch => {
                    if diagnostic
                        .references
                        .iter()
                        .any(|reference| reference.role == ContentRole::Source)
                    {
                        DependencyKind::Source
                    } else {
                        DependencyKind::Artifact
                    }
                }
            };
            let object_id = diagnostic
                .references
                .first()
                .map(|reference| reference.object_id.clone())
                .unwrap_or_else(|| "bundle-index".into());
            let relinkable = diagnostic.relinkable();
            issues.push(DependencyIssue {
                kind,
                object_id,
                content_sha256: diagnostic.content_sha256.clone(),
                detail: diagnostic.detail,
                relinkable,
            });
        }
        issues.sort_by(|left, right| {
            dependency_order(left.kind)
                .cmp(&dependency_order(right.kind))
                .then_with(|| left.object_id.cmp(&right.object_id))
                .then_with(|| left.content_sha256.cmp(&right.content_sha256))
        });
        let blocking = issues.iter().any(|issue| {
            matches!(
                issue.kind,
                DependencyKind::Engine
                    | DependencyKind::Model
                    | DependencyKind::Source
                    | DependencyKind::Artifact
            )
        });
        let bundle_integrity_failure = self.document.has_bundle_integrity_failure();
        self.availability = ProjectAvailability {
            access_mode: if bundle_integrity_failure || (has_reviewable_history && blocking) {
                ProjectAccessMode::ReadOnlyEvidence
            } else {
                ProjectAccessMode::Full
            },
            issues,
            bundle: self.document.summary(),
        };
    }

    /// Open validates the complete strict manifest before changing any active
    /// state, so malformed/future documents cannot displace current work.
    pub fn open(
        &mut self,
        path: &Path,
        now_utc_unix: u64,
    ) -> Result<Option<String>, LifecycleError> {
        let (document, migrated_from, on_disk_sha256) =
            ProjectDocument::open_with_migration_and_digest(path)?;
        let path = normalized_path(path);
        let needs_normalization = document.needs_normalization();
        self.document = document;
        self.path = Some(path.clone());
        self.dirty = migrated_from.is_some() || needs_normalization;
        self.recovered = false;
        self.last_autosave_attempt_utc_unix = now_utc_unix;
        self.last_autosave_success_utc_unix = now_utc_unix;
        self.on_disk_sha256 = Some(on_disk_sha256);
        self.owned_recovery = None;
        let mut warnings = Vec::new();
        if let Some(version) = migrated_from {
            warnings.push(format!(
                "Project schema v{version} was migrated in memory to v{}; save to persist the migration.",
                self.document.manifest().schema_version()
            ));
        }
        if needs_normalization && migrated_from.is_none() {
            warnings.push(
                "Portable content was deduplicated or recovered from legacy embedded evidence; save to persist the normalized bundle."
                    .into(),
            );
        }
        let diagnostics = self.document.diagnostics();
        if !diagnostics.is_empty() {
            warnings.push(format!(
                "Project opened in evidence-safe mode with {} content diagnostic{}; inspect Dependency & integrity details.",
                diagnostics.len(),
                if diagnostics.len() == 1 { "" } else { "s" }
            ));
        }
        if let Err(error) = self.record_recent(path, now_utc_unix) {
            warnings.push(format!(
                "Project opened, but the recent list was not updated: {error}"
            ));
        }
        Ok((!warnings.is_empty()).then(|| warnings.join(" ")))
    }

    pub fn save(&mut self, now_utc_unix: u64) -> Result<Option<String>, LifecycleError> {
        let path = self.path.clone().ok_or(LifecycleError::SaveAsRequired)?;
        if self.document.has_bundle_integrity_failure() {
            return Err(LifecycleError::IntegrityRepairRequiresSaveAs);
        }
        let precondition = self
            .on_disk_sha256
            .as_deref()
            .map(WritePrecondition::Unchanged)
            .unwrap_or(WritePrecondition::Missing);
        let saved_sha256 = self.document.save_atomic_checked(&path, precondition)?;
        self.on_disk_sha256 = Some(saved_sha256);
        self.finish_successful_save(path, now_utc_unix)
    }

    /// Save As writes a cloned candidate first. The active path, name, dirty
    /// state, runs, and evidence change only after the atomic write succeeds.
    pub fn save_as(
        &mut self,
        path: &Path,
        now_utc_unix: u64,
    ) -> Result<Option<String>, LifecycleError> {
        let path = normalized_path(path);
        let mut candidate = self.document.clone();
        if candidate.manifest().name() == "Unsaved project" {
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                let mut renamed = candidate.manifest().clone();
                renamed.rename(stem, now_utc_unix);
                candidate.replace_manifest(renamed);
            }
        }
        let same_path = self.path.as_ref() == Some(&path);
        if same_path && candidate.has_bundle_integrity_failure() {
            return Err(LifecycleError::IntegrityRepairRequiresSaveAs);
        }
        let precondition = if same_path {
            self.on_disk_sha256
                .as_deref()
                .map(WritePrecondition::Unchanged)
                .unwrap_or(WritePrecondition::Missing)
        } else {
            WritePrecondition::Any
        };
        let saved_sha256 = candidate.save_atomic_checked(&path, precondition)?;
        self.document = candidate;
        self.path = Some(path.clone());
        self.on_disk_sha256 = Some(saved_sha256);
        self.finish_successful_save(path, now_utc_unix)
    }

    /// Write a separate recovery document only when unsaved work is due.
    /// Caller-provided UTC makes interval behavior deterministic in tests.
    pub fn autosave_if_due(
        &mut self,
        now_utc_unix: u64,
        interval_seconds: u64,
    ) -> Result<bool, LifecycleError> {
        if !self.dirty {
            return Ok(false);
        }
        let interval_seconds = interval_seconds.max(1);
        if now_utc_unix.saturating_sub(self.last_autosave_success_utc_unix) < interval_seconds {
            return Ok(false);
        }
        if self.last_autosave_attempt_utc_unix > self.last_autosave_success_utc_unix
            && now_utc_unix.saturating_sub(self.last_autosave_attempt_utc_unix)
                < AUTOSAVE_RETRY_SECONDS
        {
            return Ok(false);
        }
        self.last_autosave_attempt_utc_unix = now_utc_unix;
        self.write_recovery_snapshot(now_utc_unix)
    }

    /// Force an atomic recovery checkpoint for a newly started or terminal run
    /// attempt. Execution must not begin until this succeeds.
    pub fn checkpoint_recovery(&mut self, now_utc_unix: u64) -> Result<bool, LifecycleError> {
        if !self.dirty {
            return Ok(false);
        }
        self.last_autosave_attempt_utc_unix = now_utc_unix;
        self.write_recovery_snapshot(now_utc_unix)
    }

    /// Record a Running attempt and make its recovery snapshot durable before
    /// execution. If checkpointing fails, the in-memory attempt is immediately
    /// terminalized so a later Save can never preserve a false Running state.
    pub fn start_run_attempt_checkpointed(
        &mut self,
        case_id: &str,
        attempt: RunRecord,
        now_utc_unix: u64,
    ) -> Result<(), LifecycleError> {
        self.transact(now_utc_unix, |manifest| {
            manifest.append_run(case_id, attempt.clone(), now_utc_unix)
        })?;
        if let Err(checkpoint_error) = self.checkpoint_recovery(now_utc_unix) {
            let mut terminal_manifest = attempt.manifest().clone();
            terminal_manifest.runtime_ms = 0;
            terminal_manifest.stop_reason =
                format!("recovery_checkpoint_failed: {checkpoint_error}");
            let interrupted = RunRecord::new(
                attempt.run_id(),
                attempt.parent_run_id().map(str::to_owned),
                attempt.case_revision_id(),
                attempt.created_utc_unix(),
                now_utc_unix,
                LifecycleState::Failed,
                terminal_manifest,
                Vec::new(),
            );
            self.transact(now_utc_unix, |manifest| {
                manifest.finish_run_attempt(case_id, interrupted, now_utc_unix)
            })?;
            // Best effort only: the original path may still be unavailable.
            // The dirty terminal state remains available to normal Save/Save As.
            let _ = self.checkpoint_recovery(now_utc_unix);
            return Err(checkpoint_error);
        }
        Ok(())
    }

    fn write_recovery_snapshot(&mut self, now_utc_unix: u64) -> Result<bool, LifecycleError> {
        let recovery_path =
            recovery_path(&self.state_directory, self.document.manifest().project_id());
        let project_document: serde_json::Value =
            serde_json::from_slice(&self.document.to_bytes()?)
                .map_err(|error| LifecycleError::StateJson(error.to_string()))?;
        let document = RecoveryDocument {
            schema_version: STATE_SCHEMA_VERSION,
            project_id: self.document.manifest().project_id().into(),
            project_name: self.document.manifest().name().into(),
            source_path: self.path.clone(),
            saved_utc_unix: now_utc_unix,
            project_document,
        };
        let mut bytes = serde_json::to_vec_pretty(&document)
            .map_err(|error| LifecycleError::StateJson(error.to_string()))?;
        bytes.push(b'\n');
        let precondition = self
            .owned_recovery
            .as_ref()
            .filter(|owned| owned.project_id == document.project_id)
            .map(|owned| WritePrecondition::Unchanged(owned.storage_sha256.as_str()))
            .unwrap_or(WritePrecondition::Missing);
        let storage_sha256 = write_atomic(&recovery_path, &bytes, precondition)?;
        self.last_autosave_success_utc_unix = now_utc_unix;
        self.owned_recovery = Some(OwnedRecovery {
            project_id: document.project_id.clone(),
            storage_sha256: storage_sha256.clone(),
        });
        let entry = RecoveryEntry {
            project_id: document.project_id,
            name: document.project_name,
            source_path: document.source_path,
            saved_utc_unix: document.saved_utc_unix,
            recovery_path,
            storage_sha256,
        };
        self.recovery_entries
            .retain(|candidate| candidate.project_id != entry.project_id);
        self.recovery_entries.push(entry);
        sort_recoveries(&mut self.recovery_entries);
        Ok(true)
    }

    /// Recover into an unsaved active state. The original save path is retained
    /// as a target hint, but is never overwritten until the user chooses Save.
    pub fn recover(&mut self, project_id: &str, now_utc_unix: u64) -> Result<(), LifecycleError> {
        let entry = self
            .recovery_entries
            .iter()
            .find(|entry| entry.project_id == project_id)
            .cloned()
            .ok_or_else(|| LifecycleError::RecoveryNotFound(project_id.into()))?;
        let (document, storage_sha256) = read_recovery_document(&entry.recovery_path)
            .map_err(|error| LifecycleError::InvalidRecovery(error.to_string()))?;
        if storage_sha256 != entry.storage_sha256 {
            return Err(LifecycleError::InvalidRecovery(
                "snapshot changed after the recovery list was loaded".into(),
            ));
        }
        let project_document = project_document_from_recovery(&document)
            .map_err(|error| LifecycleError::InvalidRecovery(error.to_string()))?;
        if project_document.manifest().project_id() != project_id
            || document.project_id != project_id
            || project_document.manifest().name() != document.project_name
        {
            return Err(LifecycleError::InvalidRecovery(format!(
                "snapshot metadata does not match requested project {project_id} (found {})",
                project_document.manifest().project_id()
            )));
        }
        self.document = project_document;
        self.path = document.source_path.map(|path| normalized_path(&path));
        self.dirty = true;
        self.recovered = true;
        self.last_autosave_attempt_utc_unix = now_utc_unix;
        self.last_autosave_success_utc_unix = now_utc_unix;
        self.on_disk_sha256 = self
            .path
            .as_deref()
            .and_then(|path| file_sha256(path).ok().flatten());
        self.owned_recovery = Some(OwnedRecovery {
            project_id: project_id.into(),
            storage_sha256,
        });
        Ok(())
    }

    pub fn discard_active_recovery(&mut self) -> Result<(), LifecycleError> {
        let project_id = self.document.manifest().project_id().to_owned();
        self.discard_recovery(&project_id)
    }

    pub fn discard_recovery(&mut self, project_id: &str) -> Result<(), LifecycleError> {
        let Some(entry) = self
            .recovery_entries
            .iter()
            .find(|entry| entry.project_id == project_id)
            .cloned()
        else {
            return Ok(());
        };
        match std::fs::remove_file(&entry.recovery_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(LifecycleError::Io(error.to_string())),
        }
        self.recovery_entries
            .retain(|candidate| candidate.project_id != project_id);
        if self
            .owned_recovery
            .as_ref()
            .is_some_and(|owned| owned.project_id == project_id)
        {
            self.owned_recovery = None;
        }
        Ok(())
    }

    pub fn remove_recent(&mut self, path: &Path) -> Result<(), LifecycleError> {
        let path = normalized_path(path);
        let mut candidate = self.recent_projects.clone();
        candidate.retain(|recent| recent.path != path);
        persist_recent_projects(&self.state_directory, &candidate)?;
        self.recent_projects = candidate;
        Ok(())
    }

    fn finish_successful_save(
        &mut self,
        path: PathBuf,
        now_utc_unix: u64,
    ) -> Result<Option<String>, LifecycleError> {
        self.path = Some(path.clone());
        self.dirty = false;
        self.recovered = false;
        self.document.mark_saved();
        self.last_autosave_attempt_utc_unix = now_utc_unix;
        self.last_autosave_success_utc_unix = now_utc_unix;
        let mut warnings = Vec::new();
        if let Err(error) = self.discard_owned_active_recovery() {
            warnings.push(format!("the older recovery snapshot remains: {error}"));
        }
        if let Err(error) = self.record_recent(path, now_utc_unix) {
            warnings.push(format!("the recent list was not updated: {error}"));
        }
        Ok((!warnings.is_empty()).then(|| warnings.join("; ")))
    }

    fn discard_owned_active_recovery(&mut self) -> Result<(), LifecycleError> {
        let project_id = self.document.manifest().project_id();
        let Some(owned) = self
            .owned_recovery
            .as_ref()
            .filter(|owned| owned.project_id == project_id)
            .cloned()
        else {
            return Ok(());
        };
        let path = recovery_path(&self.state_directory, project_id);
        let actual_sha256 = file_sha256(&path).map_err(LifecycleError::Io)?;
        if actual_sha256.as_deref() != Some(owned.storage_sha256.as_str()) {
            return Err(LifecycleError::StateWriteConflict {
                expected_sha256: Some(owned.storage_sha256),
                actual_sha256,
            });
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(LifecycleError::Io(error.to_string())),
        }
        self.recovery_entries
            .retain(|candidate| candidate.project_id != project_id);
        self.owned_recovery = None;
        Ok(())
    }

    fn record_recent(&mut self, path: PathBuf, now_utc_unix: u64) -> Result<(), LifecycleError> {
        let path = normalized_path(&path);
        let mut candidate = self.recent_projects.clone();
        candidate.retain(|recent| recent.path != path);
        candidate.insert(
            0,
            RecentProject {
                path,
                project_id: self.document.manifest().project_id().into(),
                name: self.document.manifest().name().into(),
                last_opened_utc_unix: now_utc_unix,
            },
        );
        candidate.truncate(MAX_RECENT_PROJECTS);
        persist_recent_projects(&self.state_directory, &candidate)?;
        self.recent_projects = candidate;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeferredProjectAction {
    New,
    Open(PathBuf),
    Recover(String),
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsavedDecision {
    Save,
    Discard,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardRequest {
    Execute(DeferredProjectAction),
    ConfirmationRequired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardResolution {
    SaveThen(DeferredProjectAction),
    Execute(DeferredProjectAction),
    Cancelled,
    Idle,
}

#[derive(Default)]
pub struct UnsavedChangesGuard {
    pending: Option<DeferredProjectAction>,
}

impl UnsavedChangesGuard {
    pub fn request(
        &mut self,
        action: DeferredProjectAction,
        has_unsaved_changes: bool,
    ) -> GuardRequest {
        if has_unsaved_changes {
            self.pending = Some(action);
            GuardRequest::ConfirmationRequired
        } else {
            GuardRequest::Execute(action)
        }
    }

    pub fn pending(&self) -> Option<&DeferredProjectAction> {
        self.pending.as_ref()
    }

    pub fn resolve(&mut self, decision: UnsavedDecision) -> GuardResolution {
        let Some(action) = self.pending.take() else {
            return GuardResolution::Idle;
        };
        match decision {
            UnsavedDecision::Save => GuardResolution::SaveThen(action),
            UnsavedDecision::Discard => GuardResolution::Execute(action),
            UnsavedDecision::Cancel => GuardResolution::Cancelled,
        }
    }
}

fn dependency_order(kind: DependencyKind) -> u8 {
    match kind {
        DependencyKind::Engine => 0,
        DependencyKind::Model => 1,
        DependencyKind::Source => 2,
        DependencyKind::Artifact => 3,
        DependencyKind::Integrity => 4,
    }
}

fn is_sha256(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn short_hash(digest: &str) -> String {
    digest.get(..12).unwrap_or(digest).to_owned()
}

fn recent_path(state_directory: &Path) -> PathBuf {
    state_directory.join(RECENT_FILE_NAME)
}

fn recovery_path(state_directory: &Path, project_id: &str) -> PathBuf {
    let digest: String = Sha256::digest(project_id.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    state_directory
        .join(RECOVERY_DIRECTORY)
        .join(format!("{digest}.{RECOVERY_EXTENSION}"))
}

fn normalized_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn load_recent_projects(state_directory: &Path) -> Result<Vec<RecentProject>, LifecycleError> {
    let path = recent_path(state_directory);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(LifecycleError::Io(error.to_string())),
    };
    let document: RecentDocument = serde_json::from_slice(&bytes)
        .map_err(|error| LifecycleError::StateJson(error.to_string()))?;
    if document.schema_version != STATE_SCHEMA_VERSION {
        return Err(LifecycleError::UnsupportedStateSchema {
            kind: "recent-project list",
            version: document.schema_version,
        });
    }
    let mut deduplicated = Vec::new();
    for mut recent in document.projects {
        recent.path = normalized_path(&recent.path);
        if deduplicated
            .iter()
            .any(|candidate: &RecentProject| candidate.path == recent.path)
        {
            continue;
        }
        deduplicated.push(recent);
        if deduplicated.len() == MAX_RECENT_PROJECTS {
            break;
        }
    }
    Ok(deduplicated)
}

fn persist_recent_projects(
    state_directory: &Path,
    projects: &[RecentProject],
) -> Result<(), LifecycleError> {
    let document = RecentDocument {
        schema_version: STATE_SCHEMA_VERSION,
        projects: projects.to_vec(),
    };
    let mut bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| LifecycleError::StateJson(error.to_string()))?;
    bytes.push(b'\n');
    write_atomic(
        &recent_path(state_directory),
        &bytes,
        WritePrecondition::Any,
    )
    .map(|_| ())
}

fn load_recovery_entries(
    state_directory: &Path,
) -> Result<(Vec<RecoveryEntry>, Vec<String>), LifecycleError> {
    let directory = state_directory.join(RECOVERY_DIRECTORY);
    let read_dir = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(error) => return Err(LifecycleError::Io(error.to_string())),
    };
    let mut paths: Vec<PathBuf> = read_dir
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().and_then(|extension| extension.to_str()) == Some(RECOVERY_EXTENSION)
        })
        .collect();
    paths.sort();
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for path in paths {
        let (document, storage_sha256) = match read_recovery_document(&path) {
            Ok(document) => document,
            Err(error) => {
                warnings.push(format!(
                    "{} was ignored: {error}",
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("recovery snapshot")
                ));
                continue;
            }
        };
        if path != recovery_path(state_directory, &document.project_id) {
            warnings.push(format!(
                "{} was ignored because its filename does not match its project ID",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("recovery snapshot")
            ));
            continue;
        }
        let project_document = match project_document_from_recovery(&document) {
            Ok(project_document) => project_document,
            Err(error) => {
                warnings.push(format!(
                    "{} was ignored: {error}",
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("recovery snapshot")
                ));
                continue;
            }
        };
        if project_document.manifest().project_id() != document.project_id
            || project_document.manifest().name() != document.project_name
        {
            warnings.push(format!(
                "{} was ignored because its recovery metadata does not match its project",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("recovery snapshot")
            ));
            continue;
        }
        // A save can succeed even if deleting its old recovery file is denied.
        // Suppress that stale snapshot when the saved document is byte-equivalent.
        if let Some(source_path) = &document.source_path {
            let saved_matches = ProjectDocument::open(source_path)
                .and_then(|saved| Ok(saved.to_bytes()? == project_document.to_bytes()?))
                .unwrap_or(false);
            if saved_matches {
                let _ = std::fs::remove_file(&path);
                continue;
            }
        }
        entries.push(RecoveryEntry {
            project_id: document.project_id,
            name: document.project_name,
            source_path: document.source_path,
            saved_utc_unix: document.saved_utc_unix,
            recovery_path: path,
            storage_sha256,
        });
    }
    sort_recoveries(&mut entries);
    Ok((entries, warnings))
}

fn sort_recoveries(entries: &mut [RecoveryEntry]) {
    entries.sort_by(|left, right| {
        right
            .saved_utc_unix
            .cmp(&left.saved_utc_unix)
            .then_with(|| left.project_id.cmp(&right.project_id))
    });
}

fn read_recovery_document(path: &Path) -> Result<(RecoveryDocument, String), LifecycleError> {
    let bytes = std::fs::read(path).map_err(|error| LifecycleError::Io(error.to_string()))?;
    let document: RecoveryDocument = serde_json::from_slice(&bytes)
        .map_err(|error| LifecycleError::StateJson(error.to_string()))?;
    if document.schema_version != STATE_SCHEMA_VERSION {
        return Err(LifecycleError::UnsupportedStateSchema {
            kind: "recovery",
            version: document.schema_version,
        });
    }
    Ok((document, state_bytes_digest(&bytes)))
}

fn project_document_from_recovery(
    document: &RecoveryDocument,
) -> Result<ProjectDocument, LifecycleError> {
    let bytes = serde_json::to_vec(&document.project_document)
        .map_err(|error| LifecycleError::StateJson(error.to_string()))?;
    ProjectDocument::from_bytes(&bytes).map_err(Into::into)
}

fn state_bytes_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write_atomic(
    path: &Path,
    bytes: &[u8],
    precondition: WritePrecondition<'_>,
) -> Result<String, LifecycleError> {
    write_atomic_bytes(path, bytes, precondition).map_err(|error| match error {
        AtomicWriteError::Io(error) => LifecycleError::Io(error),
        AtomicWriteError::Conflict {
            expected_sha256,
            actual_sha256,
        } => LifecycleError::StateWriteConflict {
            expected_sha256,
            actual_sha256,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{
        CalibratedView, CaseRevision, EvidenceArtifact, EvidenceSourceClass, LifecycleState,
        RunManifest, RunRecord, SourceKind, SourceRevision, VersionedComponent,
        PROJECT_SCHEMA_VERSION,
    };
    use serde_json::json;

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "reyn-project-lifecycle-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    fn bytes_digest(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn fixture_source_bytes() -> Vec<u8> {
        b"fixture checkpoint bytes".to_vec()
    }

    fn fixture_evidence_bytes() -> Vec<u8> {
        br#"{"fixture":"stored evidence"}"#.to_vec()
    }

    fn fixture_project() -> ProjectManifest {
        let source_digest = bytes_digest(&fixture_source_bytes());
        let evidence_digest = bytes_digest(&fixture_evidence_bytes());
        let mut project = ProjectManifest::new("Evidence fixture", 1);
        project
            .add_source_revision(
                SourceRevision {
                    source_revision_id: "source-1".into(),
                    source_kind: SourceKind::Model,
                    revision: 1,
                    imported_utc_unix: 2,
                    uri_hint: Some("/machine-only/model.pth".into()),
                    byte_size: fixture_source_bytes().len() as u64,
                    content_sha256: source_digest.clone(),
                    declared_units: None,
                    frame: Some("periodic_xy".into()),
                    transform_4x4: [
                        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0,
                        1.0,
                    ],
                    parent_revision_id: None,
                    warnings: vec!["units unknown".into()],
                },
                2,
            )
            .unwrap();
        project
            .create_case(
                "case-1",
                "Painted IC",
                CaseRevision {
                    case_revision_id: "revision-1".into(),
                    parent_revision_id: None,
                    created_utc_unix: 3,
                    source_revision_ids: vec!["source-1".into()],
                    contract: json!({"physics": "incompressible_2d"}),
                    discretization: json!({"grid": 128}),
                    outputs: json!({"quantity": "velocity"}),
                },
                3,
            )
            .unwrap();
        project
            .append_run(
                "case-1",
                RunRecord::new(
                    "run-1",
                    None,
                    "revision-1",
                    4,
                    5,
                    LifecycleState::Succeeded,
                    RunManifest {
                        schema_version: PROJECT_SCHEMA_VERSION,
                        app: VersionedComponent {
                            name: "Reyn Studio".into(),
                            version: "0.1.0".into(),
                            sha256: None,
                        },
                        engine: None,
                        model: Some(VersionedComponent {
                            name: "model.pth".into(),
                            version: "checkpoint".into(),
                            sha256: Some(source_digest),
                        }),
                        solver: None,
                        converter: None,
                        exact_contract: json!({"physics": "incompressible_2d"}),
                        exact_settings: json!({"horizon": 4}),
                        seeds: vec![70000],
                        device: "cpu".into(),
                        runtime_ms: 12,
                        stop_reason: "complete".into(),
                        warnings: vec![],
                        waivers: vec![],
                        missing_dependencies: vec!["model checkpoint aaaa…".into()],
                        output_sha256: vec![evidence_digest.clone()],
                        scalar_outputs: vec![],
                        determinism: None,
                    },
                    vec![CalibratedView {
                        view_id: "velocity".into(),
                        quantity: "velocity magnitude".into(),
                        units: "normalized velocity".into(),
                        scale_min: 0.0,
                        scale_max: 2.0,
                        source_class: EvidenceSourceClass::ModelPrediction,
                        method: "direct_flow_map".into(),
                    }],
                ),
                5,
            )
            .unwrap();
        project
            .append_evidence(
                EvidenceArtifact {
                    evidence_id: "evidence-1".into(),
                    run_ids: vec!["run-1".into()],
                    created_utc_unix: 6,
                    source_class: EvidenceSourceClass::Derived,
                    media_type: "application/json".into(),
                    byte_size: fixture_evidence_bytes().len() as u64,
                    content_sha256: evidence_digest,
                    derivation_method: Some("benchmark_report".into()),
                    derivation_version: Some("1".into()),
                    warnings: vec!["no independent reference".into()],
                    metadata: json!({"integrity_only": true}),
                    calibrated_views: vec![],
                },
                6,
            )
            .unwrap();
        project
    }

    #[test]
    fn cancel_keeps_unsaved_project_and_clears_pending_transition() {
        let root = test_root("cancel");
        let (mut lifecycle, _) = ProjectLifecycle::load(&root, 10);
        lifecycle.new_project(11);
        let active_id = lifecycle.manifest().project_id().to_owned();
        let mut guard = UnsavedChangesGuard::default();
        assert_eq!(
            guard.request(DeferredProjectAction::New, lifecycle.is_dirty()),
            GuardRequest::ConfirmationRequired
        );
        assert_eq!(
            guard.resolve(UnsavedDecision::Cancel),
            GuardResolution::Cancelled
        );
        assert!(guard.pending().is_none());
        assert_eq!(lifecycle.manifest().project_id(), active_id);
        assert!(lifecycle.is_dirty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_and_future_open_leave_active_unsaved_work_untouched() {
        let root = test_root("invalid-open");
        std::fs::create_dir_all(&root).unwrap();
        let (mut lifecycle, _) = ProjectLifecycle::load(root.join("state"), 20);
        lifecycle.new_project(21);
        let active_id = lifecycle.manifest().project_id().to_owned();

        let malformed = root.join("malformed.reynproj");
        std::fs::write(&malformed, b"{not json").unwrap();
        assert!(matches!(
            lifecycle.open(&malformed, 22),
            Err(LifecycleError::Project(ProjectError::Json(_)))
        ));
        assert_eq!(lifecycle.manifest().project_id(), active_id);
        assert!(lifecycle.path().is_none());
        assert!(lifecycle.is_dirty());

        let mut future: serde_json::Value =
            serde_json::from_slice(&fixture_project().to_bytes().unwrap()).unwrap();
        future["schema_version"] = json!(99);
        let future_path = root.join("future.reynproj");
        std::fs::write(&future_path, serde_json::to_vec(&future).unwrap()).unwrap();
        assert!(matches!(
            lifecycle.open(&future_path, 23),
            Err(LifecycleError::Project(ProjectError::UnsupportedSchema(99)))
        ));
        assert_eq!(lifecycle.manifest().project_id(), active_id);
        assert!(lifecycle.path().is_none());
        assert!(lifecycle.is_dirty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn save_as_is_atomic_and_failure_does_not_retarget_or_rename() {
        let root = test_root("atomic");
        std::fs::create_dir_all(&root).unwrap();
        let (mut lifecycle, _) = ProjectLifecycle::load(root.join("state"), 30);
        lifecycle.new_project(31);
        let target = root.join("wing-study.reynproj");
        std::fs::write(&target, b"old incomplete bytes").unwrap();
        lifecycle.save_as(&target, 32).unwrap();
        assert_eq!(
            ProjectManifest::open(&target).unwrap().project_id(),
            lifecycle.manifest().project_id()
        );
        assert_eq!(lifecycle.display_name(), "wing-study");
        assert!(!lifecycle.is_dirty());
        assert!(!std::fs::read_dir(&root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));

        lifecycle.new_project(33);
        let unsaved_id = lifecycle.manifest().project_id().to_owned();
        let invalid_target = root.join("destination-is-a-directory");
        std::fs::create_dir_all(&invalid_target).unwrap();
        assert!(lifecycle.save_as(&invalid_target, 34).is_err());
        assert_eq!(lifecycle.manifest().project_id(), unsaved_id);
        assert_eq!(lifecycle.display_name(), "Unsaved project");
        assert!(lifecycle.path().is_none());
        assert!(lifecycle.is_dirty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn recent_list_persists_successful_projects_in_recency_order() {
        let root = test_root("recent");
        let state = root.join("state");
        let (mut lifecycle, _) = ProjectLifecycle::load(&state, 40);
        lifecycle.new_project(41);
        let first = root.join("first.reynproj");
        lifecycle.save_as(&first, 42).unwrap();
        lifecycle.new_project(43);
        let second = root.join("second.reynproj");
        lifecycle.save_as(&second, 44).unwrap();

        let (reloaded, warnings) = ProjectLifecycle::load(&state, 45);
        assert!(warnings.is_empty());
        assert_eq!(reloaded.recent_projects().len(), 2);
        assert_eq!(reloaded.recent_projects()[0].path, normalized_path(&second));
        assert_eq!(reloaded.recent_projects()[1].path, normalized_path(&first));
        assert!(reloaded.recent_projects()[0].last_opened_utc_unix > 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn autosave_recovery_preserves_evidence_and_clears_after_explicit_save() {
        let root = test_root("recovery");
        std::fs::create_dir_all(&root).unwrap();
        let state = root.join("state");
        let source = root.join("evidence.reynproj");
        let fixture = fixture_project();
        let immutable_evidence = fixture.evidence().to_vec();
        fixture.save_atomic(&source).unwrap();

        let (mut lifecycle, _) = ProjectLifecycle::load(&state, 50);
        lifecycle.open(&source, 51).unwrap();
        lifecycle.rename_project("Recovered evidence", 52);
        assert!(!lifecycle.autosave_if_due(55, 10).unwrap());
        assert!(lifecycle.autosave_if_due(62, 10).unwrap());
        assert_eq!(lifecycle.manifest().evidence(), immutable_evidence);

        let (mut restarted, warnings) = ProjectLifecycle::load(&state, 63);
        assert!(warnings.is_empty());
        assert_eq!(restarted.recovery_entries().len(), 1);
        let recovery_id = restarted.recovery_entries()[0].project_id.clone();
        restarted.recover(&recovery_id, 64).unwrap();
        assert!(restarted.is_dirty());
        assert!(restarted.is_recovered());
        assert_eq!(restarted.path(), Some(normalized_path(&source).as_path()));
        assert_eq!(restarted.display_name(), "Recovered evidence");
        assert_eq!(restarted.manifest().evidence(), immutable_evidence);
        restarted.save(65).unwrap();
        assert!(!restarted.is_dirty());
        assert!(restarted.recovery_entries().is_empty());
        assert_eq!(
            ProjectManifest::open(&source).unwrap().evidence(),
            immutable_evidence
        );

        let (after_save, warnings) = ProjectLifecycle::load(&state, 66);
        assert!(warnings.is_empty());
        assert!(after_save.recovery_entries().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_start_checkpoint_terminalizes_attempt_before_save_and_reopen() {
        let root = test_root("attempt-checkpoint-failure");
        std::fs::create_dir_all(&root).unwrap();
        let state = root.join("blocked-state");
        let source = root.join("attempt.reynproj");
        fixture_project().save_atomic(&source).unwrap();

        let (mut lifecycle, _) = ProjectLifecycle::load(&state, 70);
        lifecycle.open(&source, 71).unwrap();
        let prior = lifecycle.manifest().run("run-1").unwrap();
        let mut manifest = prior.manifest().clone();
        manifest.runtime_ms = 0;
        manifest.stop_reason = "running".into();
        manifest.output_sha256.clear();
        manifest.scalar_outputs.clear();
        manifest.determinism = None;
        let attempt = RunRecord::new(
            "run-checkpoint-failed",
            Some("run-1".into()),
            prior.case_revision_id(),
            72,
            0,
            LifecycleState::Running,
            manifest,
            Vec::new(),
        );

        std::fs::remove_dir_all(&state).unwrap();
        std::fs::write(&state, b"blocks recovery directory creation").unwrap();
        let error = lifecycle
            .start_run_attempt_checkpointed("case-1", attempt, 72)
            .unwrap_err();
        assert!(error.to_string().contains("project state I/O"));
        let terminal = lifecycle.manifest().run("run-checkpoint-failed").unwrap();
        assert_eq!(terminal.state(), LifecycleState::Failed);
        assert!(terminal
            .manifest()
            .stop_reason
            .starts_with("recovery_checkpoint_failed:"));

        lifecycle.save(73).unwrap();
        let (mut reopened, warnings) = ProjectLifecycle::load(root.join("fresh-state"), 74);
        assert!(warnings.is_empty());
        reopened.open(&source, 75).unwrap();
        let persisted = reopened.manifest().run("run-checkpoint-failed").unwrap();
        assert_eq!(persisted.state(), LifecycleState::Failed);
        assert_eq!(persisted.completed_utc_unix(), 72);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn engine_and_model_dependencies_reconcile_without_mutating_stored_evidence() {
        let root = test_root("missing");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("review.reynproj");
        fixture_project().save_atomic(&path).unwrap();
        let (mut lifecycle, _) = ProjectLifecycle::load(root.join("state"), 70);
        lifecycle.open(&path, 71).unwrap();
        let evidence = lifecycle.manifest().evidence().to_vec();
        let model_digest = lifecycle.manifest().source_revisions()[0]
            .content_sha256
            .clone();

        lifecycle.reconcile_dependencies(false, std::iter::empty());
        assert!(lifecycle.availability().is_read_only_evidence());
        assert!(lifecycle
            .availability()
            .issues
            .iter()
            .any(|issue| issue.kind == DependencyKind::Engine));

        lifecycle.reconcile_dependencies(true, std::iter::empty());
        assert!(lifecycle
            .availability()
            .issues
            .iter()
            .any(|issue| issue.kind == DependencyKind::Model));
        lifecycle.reconcile_dependencies(true, [model_digest.as_str()]);
        assert!(!lifecycle
            .availability()
            .issues
            .iter()
            .any(|issue| issue.kind == DependencyKind::Model));
        lifecycle.reconcile_dependencies(true, std::iter::empty());
        assert!(lifecycle
            .availability()
            .issues
            .iter()
            .any(|issue| issue.kind == DependencyKind::Model));

        assert_eq!(lifecycle.summary().runs, 1);
        assert_eq!(lifecycle.summary().evidence, 1);
        assert_eq!(lifecycle.manifest().evidence(), evidence);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_sources_and_artifacts_relink_safely_without_run_mutation() {
        let root = test_root("relink");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("review.reynproj");
        fixture_project().save_atomic(&path).unwrap();
        let (mut lifecycle, _) = ProjectLifecycle::load(root.join("state"), 80);
        lifecycle.open(&path, 81).unwrap();
        let original_run = lifecycle.manifest().cases()[0].runs()[0].clone();
        let original_evidence = lifecycle.manifest().evidence().to_vec();
        let source_digest = bytes_digest(&fixture_source_bytes());
        let evidence_digest = bytes_digest(&fixture_evidence_bytes());
        lifecycle.reconcile_dependencies(true, [source_digest.as_str()]);
        assert!(lifecycle
            .availability()
            .issues
            .iter()
            .any(|issue| issue.kind == DependencyKind::Source && issue.relinkable));
        assert!(lifecycle
            .availability()
            .issues
            .iter()
            .any(|issue| issue.kind == DependencyKind::Artifact && issue.relinkable));

        let source = root.join("model.pth");
        let artifact = root.join("evidence.json");
        std::fs::write(&source, fixture_source_bytes()).unwrap();
        std::fs::write(&artifact, fixture_evidence_bytes()).unwrap();
        lifecycle.relink_content(&source_digest, &source).unwrap();
        lifecycle
            .relink_content(&evidence_digest, &artifact)
            .unwrap();
        lifecycle.reconcile_dependencies(false, std::iter::empty());
        assert!(lifecycle.availability().is_read_only_evidence());
        assert_eq!(
            lifecycle.content_bytes(&evidence_digest),
            Some(fixture_evidence_bytes().as_slice())
        );
        lifecycle.reconcile_dependencies(true, [source_digest.as_str()]);

        assert!(!lifecycle.availability().is_read_only_evidence());
        assert!(lifecycle.availability().issues.is_empty());
        assert_eq!(lifecycle.manifest().cases()[0].runs()[0], original_run);
        assert_eq!(lifecycle.manifest().evidence(), original_evidence);
        assert_eq!(
            lifecycle.content_bytes(&evidence_digest),
            Some(fixture_evidence_bytes().as_slice())
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn recovery_snapshot_retains_portable_content_objects() {
        let root = test_root("bundle-recovery");
        std::fs::create_dir_all(&root).unwrap();
        let state = root.join("state");
        let path = root.join("review.reynproj");
        fixture_project().save_atomic(&path).unwrap();
        let (mut lifecycle, _) = ProjectLifecycle::load(&state, 90);
        lifecycle.open(&path, 91).unwrap();
        let source_digest = bytes_digest(&fixture_source_bytes());
        let evidence_digest = bytes_digest(&fixture_evidence_bytes());
        let source = root.join("model.pth");
        let artifact = root.join("evidence.json");
        std::fs::write(&source, fixture_source_bytes()).unwrap();
        std::fs::write(&artifact, fixture_evidence_bytes()).unwrap();
        lifecycle.relink_content(&source_digest, &source).unwrap();
        lifecycle
            .relink_content(&evidence_digest, &artifact)
            .unwrap();
        lifecycle.rename_project("Recovered portable evidence", 92);
        assert!(lifecycle.autosave_if_due(103, 10).unwrap());

        let (mut restarted, warnings) = ProjectLifecycle::load(&state, 104);
        assert!(warnings.is_empty());
        let recovery_id = restarted.recovery_entries()[0].project_id.clone();
        restarted.recover(&recovery_id, 105).unwrap();
        assert_eq!(
            restarted.content_bytes(&source_digest),
            Some(fixture_source_bytes().as_slice())
        );
        assert_eq!(
            restarted.content_bytes(&evidence_digest),
            Some(fixture_evidence_bytes().as_slice())
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_project_saves_refuse_stale_writer_without_overwriting() {
        let root = test_root("project-conflict");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("shared.reynproj");
        fixture_project().save_atomic(&path).unwrap();

        let (mut first, _) = ProjectLifecycle::load(root.join("state-first"), 100);
        let (mut second, _) = ProjectLifecycle::load(root.join("state-second"), 100);
        first.open(&path, 101).unwrap();
        second.open(&path, 101).unwrap();

        first.rename_project("First writer", 102);
        first.save(103).unwrap();
        second.rename_project("Stale second writer", 104);
        assert!(matches!(
            second.save(105),
            Err(LifecycleError::Project(ProjectError::WriteConflict {
                expected_sha256: Some(_),
                actual_sha256: Some(_),
            }))
        ));
        assert!(second.is_dirty());
        assert_eq!(ProjectManifest::open(&path).unwrap().name(), "First writer");

        let conflict_copy = root.join("second-writer-conflict-copy.reynproj");
        second.save_as(&conflict_copy, 106).unwrap();
        assert_eq!(
            ProjectManifest::open(&conflict_copy).unwrap().name(),
            "Stale second writer"
        );
        assert_eq!(ProjectManifest::open(&path).unwrap().name(), "First writer");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_autosaves_preserve_first_snapshot_and_save_does_not_delete_it() {
        let root = test_root("recovery-conflict");
        std::fs::create_dir_all(&root).unwrap();
        let state = root.join("shared-state");
        let path = root.join("shared.reynproj");
        fixture_project().save_atomic(&path).unwrap();

        let (mut first, _) = ProjectLifecycle::load(&state, 110);
        let (mut second, _) = ProjectLifecycle::load(&state, 110);
        first.open(&path, 111).unwrap();
        second.open(&path, 111).unwrap();

        first.rename_project("First recovery", 112);
        assert!(first.autosave_if_due(121, 10).unwrap());
        let recovery = recovery_path(&state, first.manifest().project_id());
        let first_snapshot = std::fs::read(&recovery).unwrap();

        second.rename_project("Second recovery", 113);
        assert!(matches!(
            second.autosave_if_due(122, 10),
            Err(LifecycleError::StateWriteConflict {
                expected_sha256: None,
                actual_sha256: Some(_),
            })
        ));
        assert_eq!(std::fs::read(&recovery).unwrap(), first_snapshot);

        second.save(123).unwrap();
        assert!(recovery.exists());
        let (restarted, warnings) = ProjectLifecycle::load(&state, 124);
        assert!(warnings.is_empty());
        assert_eq!(restarted.recovery_entries().len(), 1);
        assert_eq!(restarted.recovery_entries()[0].name, "First recovery");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_and_changed_recovery_snapshots_are_isolated_without_displacing_work() {
        let root = test_root("invalid-recovery");
        let state = root.join("state");
        let (mut lifecycle, _) = ProjectLifecycle::load(&state, 130);
        lifecycle.new_project(131);
        lifecycle.rename_project("Valid recovery", 132);
        assert!(lifecycle.autosave_if_due(142, 10).unwrap());
        let project_id = lifecycle.manifest().project_id().to_owned();
        let recovery = recovery_path(&state, &project_id);
        let recovery_directory = recovery.parent().unwrap();
        std::fs::write(
            recovery_directory.join("malformed.reynrecovery"),
            b"{not json",
        )
        .unwrap();
        std::fs::write(
            recovery_directory.join(".ignored-interrupted.reynrecovery.tmp"),
            b"{partial",
        )
        .unwrap();

        let (mut restarted, warnings) = ProjectLifecycle::load(&state, 143);
        assert_eq!(restarted.recovery_entries().len(), 1);
        assert_eq!(warnings.len(), 1);
        let active_id = restarted.manifest().project_id().to_owned();

        std::fs::write(&recovery, b"{changed after scan").unwrap();
        assert!(restarted.recover(&project_id, 144).is_err());
        assert_eq!(restarted.manifest().project_id(), active_id);
        assert!(!restarted.is_dirty());
        assert!(recovery.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_autosave_retries_on_bounded_schedule() {
        let root = test_root("autosave-retry");
        std::fs::create_dir_all(&root).unwrap();
        let state = root.join("state-blocker");
        std::fs::write(&state, b"not a directory").unwrap();
        let (mut lifecycle, _) = ProjectLifecycle::load(&state, 150);
        lifecycle.new_project(151);

        assert!(lifecycle.autosave_if_due(161, 10).is_err());
        assert!(!lifecycle.autosave_if_due(162, 10).unwrap());
        std::fs::remove_file(&state).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        assert!(lifecycle.autosave_if_due(166, 10).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bundle_index_integrity_failure_requires_repair_to_new_path() {
        let root = test_root("integrity-repair");
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("integrity-failed.reynproj");
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&fixture_project().to_bytes().unwrap()).unwrap();
        envelope["bundle_integrity_sha256"] = json!("0".repeat(64));
        std::fs::write(&source, serde_json::to_vec_pretty(&envelope).unwrap()).unwrap();
        let original = std::fs::read(&source).unwrap();

        let (mut lifecycle, _) = ProjectLifecycle::load(root.join("state"), 170);
        assert!(lifecycle.open(&source, 171).unwrap().is_some());
        lifecycle.reconcile_dependencies(true, std::iter::empty());
        assert!(lifecycle.availability().is_read_only_evidence());
        assert!(matches!(
            lifecycle.save(172),
            Err(LifecycleError::IntegrityRepairRequiresSaveAs)
        ));
        assert_eq!(std::fs::read(&source).unwrap(), original);

        let repaired = root.join("repaired.reynproj");
        lifecycle.save_as(&repaired, 173).unwrap();
        let reopened = ProjectDocument::open(&repaired).unwrap();
        assert!(!reopened.has_bundle_integrity_failure());
        assert_eq!(std::fs::read(&source).unwrap(), original);
        let _ = std::fs::remove_dir_all(root);
    }
}
