//! N6 project substrate: a strict, versioned manifest with append-only runs and
//! evidence. Storage is deliberately manifest-first so the later bundle-vs-
//! directory decision does not change scientific lineage semantics.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{Read, Write};
use std::path::Path;

pub const PROJECT_SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProjectManifest {
    schema_version: u32,
    project_id: String,
    name: String,
    created_utc_unix: u64,
    modified_utc_unix: u64,
    source_revisions: Vec<SourceRevision>,
    cases: Vec<CaseRecord>,
    evidence: Vec<EvidenceArtifact>,
    events: Vec<ProjectEvent>,
    selection: ProjectSelection,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectSelection {
    pub active_case_id: Option<String>,
    pub selected_run_id: Option<String>,
    pub selected_evidence_id: Option<String>,
    pub selected_view_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SourceRevision {
    pub source_revision_id: String,
    pub source_kind: SourceKind,
    pub revision: u32,
    pub imported_utc_unix: u64,
    /// A non-authoritative filename/URI hint. Portable review uses the digest.
    pub uri_hint: Option<String>,
    pub byte_size: u64,
    pub content_sha256: String,
    pub declared_units: Option<String>,
    pub frame: Option<String>,
    pub transform_4x4: [f64; 16],
    pub parent_revision_id: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Geometry,
    Model,
    Reference,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CaseRecord {
    case_id: String,
    name: String,
    revisions: Vec<CaseRevision>,
    active_revision_id: String,
    stale_stages: Vec<DependencyStage>,
    runs: Vec<RunRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CaseRevision {
    pub case_revision_id: String,
    pub parent_revision_id: Option<String>,
    pub created_utc_unix: u64,
    pub source_revision_ids: Vec<String>,
    pub contract: serde_json::Value,
    pub discretization: serde_json::Value,
    pub outputs: serde_json::Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Pending,
    Draft,
    Ready,
    Running,
    #[serde(alias = "complete")]
    Succeeded,
    Stale,
    Failed,
    Cancelled,
    EvidenceLocked,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    run_id: String,
    parent_run_id: Option<String>,
    case_revision_id: String,
    created_utc_unix: u64,
    completed_utc_unix: u64,
    state: LifecycleState,
    manifest: RunManifest,
    calibrated_views: Vec<CalibratedView>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RunManifest {
    pub schema_version: u32,
    pub app: VersionedComponent,
    pub engine: Option<VersionedComponent>,
    pub model: Option<VersionedComponent>,
    pub solver: Option<VersionedComponent>,
    pub converter: Option<VersionedComponent>,
    pub exact_contract: serde_json::Value,
    pub exact_settings: serde_json::Value,
    pub seeds: Vec<u64>,
    pub device: String,
    pub runtime_ms: u64,
    pub stop_reason: String,
    pub warnings: Vec<String>,
    pub waivers: Vec<String>,
    pub missing_dependencies: Vec<String>,
    pub output_sha256: Vec<String>,
    pub scalar_outputs: Vec<ScalarOutput>,
    pub determinism: Option<DeterminismRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ScalarOutput {
    pub key: String,
    pub value: f64,
    pub units: String,
    pub abs_tolerance: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeterminismStatus {
    WithinTolerance,
    Difference,
    NotComparable,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ScalarDifference {
    pub key: String,
    pub parent_value: Option<f64>,
    pub current_value: Option<f64>,
    pub abs_difference: Option<f64>,
    pub abs_tolerance: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeterminismRecord {
    pub parent_run_id: String,
    pub status: DeterminismStatus,
    pub differences: Vec<ScalarDifference>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VersionedComponent {
    pub name: String,
    pub version: String,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CalibratedView {
    pub view_id: String,
    pub quantity: String,
    pub units: String,
    pub scale_min: f64,
    pub scale_max: f64,
    pub source_class: EvidenceSourceClass,
    pub method: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EvidenceArtifact {
    pub evidence_id: String,
    pub run_ids: Vec<String>,
    pub created_utc_unix: u64,
    pub source_class: EvidenceSourceClass,
    pub media_type: String,
    pub byte_size: u64,
    pub content_sha256: String,
    pub derivation_method: Option<String>,
    pub derivation_version: Option<String>,
    pub warnings: Vec<String>,
    pub metadata: serde_json::Value,
    pub calibrated_views: Vec<CalibratedView>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSourceClass {
    ModelPrediction,
    SolverReference,
    AnalyticalReference,
    ExperimentalReference,
    Recovered,
    Derived,
    Integrity,
    AuthenticitySignature,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectEvent {
    pub event_id: String,
    pub created_utc_unix: u64,
    pub event_type: String,
    pub object_id: String,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum DependencyStage {
    Contract,
    Discretization,
    Run,
    Evidence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectError {
    Io(String),
    Json(String),
    UnsupportedSchema(u32),
    IntegrityMismatch,
    WriteConflict {
        expected_sha256: Option<String>,
        actual_sha256: Option<String>,
    },
    ContentHashMismatch {
        expected: String,
        actual: String,
    },
    InvalidDigest {
        object_id: String,
    },
    DuplicateId {
        object_id: String,
    },
    DuplicateSignature {
        canonical_payload_sha256: String,
        key_fingerprint_sha256: String,
    },
    MissingObject {
        object_id: String,
    },
    InvalidLineage(String),
    NonTerminalRun(String),
}

impl fmt::Display for ProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "project I/O: {error}"),
            Self::Json(error) => write!(formatter, "project schema: {error}"),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported project schema version {version}")
            }
            Self::IntegrityMismatch => write!(formatter, "project manifest integrity mismatch"),
            Self::WriteConflict {
                expected_sha256,
                actual_sha256,
            } => {
                let expected = expected_sha256.as_deref().unwrap_or("no file");
                let actual = actual_sha256.as_deref().unwrap_or("no file");
                write!(
                    formatter,
                    "project changed on disk before save: expected {expected}, found {actual}"
                )
            }
            Self::ContentHashMismatch { expected, actual } => {
                write!(
                    formatter,
                    "content SHA-256 mismatch: expected {expected}, received {actual}"
                )
            }
            Self::InvalidDigest { object_id } => {
                write!(formatter, "{object_id} has an invalid SHA-256 digest")
            }
            Self::DuplicateId { object_id } => write!(formatter, "duplicate object ID {object_id}"),
            Self::DuplicateSignature {
                canonical_payload_sha256,
                key_fingerprint_sha256,
            } => write!(
                formatter,
                "canonical payload {canonical_payload_sha256} already has a signature from key {key_fingerprint_sha256}"
            ),
            Self::MissingObject { object_id } => write!(formatter, "missing object {object_id}"),
            Self::InvalidLineage(detail) => write!(formatter, "invalid lineage: {detail}"),
            Self::NonTerminalRun(run_id) => {
                write!(
                    formatter,
                    "run {run_id} is not a terminal immutable attempt"
                )
            }
        }
    }
}

impl std::error::Error for ProjectError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentRole {
    Source,
    RunOutput,
    Evidence,
}

impl ContentRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::RunOutput => "run output",
            Self::Evidence => "evidence",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentReference {
    pub object_id: String,
    pub role: ContentRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentDiagnosticKind {
    Missing,
    Corrupt,
    SizeMismatch,
    Duplicate,
    BundleIntegrity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentDiagnostic {
    pub kind: ContentDiagnosticKind,
    pub content_sha256: Option<String>,
    pub references: Vec<ContentReference>,
    pub detail: String,
}

impl ContentDiagnostic {
    pub fn relinkable(&self) -> bool {
        self.content_sha256.is_some()
            && matches!(
                self.kind,
                ContentDiagnosticKind::Missing
                    | ContentDiagnosticKind::Corrupt
                    | ContentDiagnosticKind::SizeMismatch
            )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentState {
    Available,
    Missing,
    Corrupt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleSummary {
    pub required_objects: usize,
    pub available_objects: usize,
    pub available_bytes: u64,
    pub diagnostics: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentInsert {
    pub content_sha256: String,
    pub deduplicated: bool,
}

/// Bytes paired with the SHA-256 computed from those same bytes.
///
/// Construct only via [`DigestedBytes::new`] so project insertion can trust the
/// digest without hashing the buffer a second time on the UI thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DigestedBytes {
    bytes: Vec<u8>,
    digest: String,
}

impl DigestedBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        let digest = sha256_hex(&bytes);
        Self { bytes, digest }
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[derive(Clone, Debug)]
struct BundledContent {
    media_type: String,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BundledContentWire {
    content_sha256: String,
    byte_size: u64,
    media_type: String,
    encoding: String,
    data_hex: String,
}

#[derive(Serialize)]
struct BundleIntegrityObject<'a> {
    content_sha256: &'a str,
    byte_size: u64,
    media_type: &'a str,
    encoding: &'a str,
}

#[derive(Serialize)]
struct BundleIntegrityPayload<'a> {
    schema_version: u32,
    manifest_integrity_sha256: &'a str,
    objects: Vec<BundleIntegrityObject<'a>>,
}

/// A portable project document. The strict scientific manifest remains the
/// lineage authority; content-addressed bytes live beside it and are deduped by
/// SHA-256. Machine-local paths never participate in content lookup.
#[derive(Clone, Debug)]
pub struct ProjectDocument {
    manifest: ProjectManifest,
    content: BTreeMap<String, BundledContent>,
    invalid_content: Vec<BundledContentWire>,
    load_diagnostics: Vec<ContentDiagnostic>,
    needs_normalization: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectEnvelope {
    schema_version: u32,
    manifest: ProjectManifest,
    integrity_sha256: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    bundled_content: Vec<BundledContentWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bundle_integrity_sha256: Option<String>,
}

/// Last shipped project schema. These wire types intentionally preserve the
/// original field order so integrity can be checked before migration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectEnvelopeV1 {
    schema_version: u32,
    manifest: ProjectManifestV1,
    integrity_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectManifestV1 {
    schema_version: u32,
    project_id: String,
    name: String,
    created_utc_unix: u64,
    modified_utc_unix: u64,
    source_revisions: Vec<SourceRevision>,
    cases: Vec<CaseRecordV1>,
    evidence: Vec<EvidenceArtifactV1>,
    events: Vec<ProjectEvent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CaseRecordV1 {
    case_id: String,
    name: String,
    revisions: Vec<CaseRevision>,
    active_revision_id: String,
    runs: Vec<RunRecordV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RunRecordV1 {
    run_id: String,
    parent_run_id: Option<String>,
    case_revision_id: String,
    created_utc_unix: u64,
    completed_utc_unix: u64,
    state: LifecycleState,
    manifest: RunManifestV1,
    calibrated_views: Vec<CalibratedView>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RunManifestV1 {
    schema_version: u32,
    app: VersionedComponent,
    engine: Option<VersionedComponent>,
    model: Option<VersionedComponent>,
    solver: Option<VersionedComponent>,
    converter: Option<VersionedComponent>,
    exact_contract: serde_json::Value,
    exact_settings: serde_json::Value,
    seeds: Vec<u64>,
    device: String,
    runtime_ms: u64,
    stop_reason: String,
    warnings: Vec<String>,
    waivers: Vec<String>,
    missing_dependencies: Vec<String>,
    output_sha256: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EvidenceArtifactV1 {
    evidence_id: String,
    run_ids: Vec<String>,
    created_utc_unix: u64,
    source_class: EvidenceSourceClass,
    media_type: String,
    byte_size: u64,
    content_sha256: String,
    derivation_method: Option<String>,
    derivation_version: Option<String>,
    warnings: Vec<String>,
    metadata: serde_json::Value,
}

impl From<ProjectManifestV1> for ProjectManifest {
    fn from(legacy: ProjectManifestV1) -> Self {
        Self {
            schema_version: PROJECT_SCHEMA_VERSION,
            project_id: legacy.project_id,
            name: legacy.name,
            created_utc_unix: legacy.created_utc_unix,
            modified_utc_unix: legacy.modified_utc_unix,
            source_revisions: legacy.source_revisions,
            cases: legacy
                .cases
                .into_iter()
                .map(|case| CaseRecord {
                    case_id: case.case_id,
                    name: case.name,
                    revisions: case.revisions,
                    active_revision_id: case.active_revision_id,
                    stale_stages: Vec::new(),
                    runs: case
                        .runs
                        .into_iter()
                        .map(|run| RunRecord {
                            run_id: run.run_id,
                            parent_run_id: run.parent_run_id,
                            case_revision_id: run.case_revision_id,
                            created_utc_unix: run.created_utc_unix,
                            completed_utc_unix: run.completed_utc_unix,
                            state: run.state,
                            manifest: RunManifest {
                                schema_version: run.manifest.schema_version,
                                app: run.manifest.app,
                                engine: run.manifest.engine,
                                model: run.manifest.model,
                                solver: run.manifest.solver,
                                converter: run.manifest.converter,
                                exact_contract: run.manifest.exact_contract,
                                exact_settings: run.manifest.exact_settings,
                                seeds: run.manifest.seeds,
                                device: run.manifest.device,
                                runtime_ms: run.manifest.runtime_ms,
                                stop_reason: run.manifest.stop_reason,
                                warnings: run.manifest.warnings,
                                waivers: run.manifest.waivers,
                                missing_dependencies: run.manifest.missing_dependencies,
                                output_sha256: run.manifest.output_sha256,
                                scalar_outputs: Vec::new(),
                                determinism: None,
                            },
                            calibrated_views: run.calibrated_views,
                        })
                        .collect(),
                })
                .collect(),
            evidence: legacy
                .evidence
                .into_iter()
                .map(|artifact| EvidenceArtifact {
                    evidence_id: artifact.evidence_id,
                    run_ids: artifact.run_ids,
                    created_utc_unix: artifact.created_utc_unix,
                    source_class: artifact.source_class,
                    media_type: artifact.media_type,
                    byte_size: artifact.byte_size,
                    content_sha256: artifact.content_sha256,
                    derivation_method: artifact.derivation_method,
                    derivation_version: artifact.derivation_version,
                    warnings: artifact.warnings,
                    metadata: artifact.metadata,
                    calibrated_views: Vec::new(),
                })
                .collect(),
            events: legacy.events,
            selection: ProjectSelection::default(),
        }
    }
}

impl ProjectDocument {
    pub fn new(manifest: ProjectManifest) -> Self {
        Self {
            manifest,
            content: BTreeMap::new(),
            invalid_content: Vec::new(),
            load_diagnostics: Vec::new(),
            needs_normalization: false,
        }
    }

    pub fn manifest(&self) -> &ProjectManifest {
        &self.manifest
    }

    pub fn replace_manifest(&mut self, manifest: ProjectManifest) {
        self.manifest = manifest;
    }

    pub fn needs_normalization(&self) -> bool {
        self.needs_normalization
    }

    pub fn mark_saved(&mut self) {
        self.needs_normalization = false;
        self.load_diagnostics.retain(|diagnostic| {
            !matches!(
                diagnostic.kind,
                ContentDiagnosticKind::Duplicate | ContentDiagnosticKind::BundleIntegrity
            )
        });
    }

    pub(crate) fn has_bundle_integrity_failure(&self) -> bool {
        self.load_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == ContentDiagnosticKind::BundleIntegrity)
    }

    pub fn content_bytes(&self, digest: &str) -> Option<&[u8]> {
        self.content
            .get(&digest.to_ascii_lowercase())
            .map(|content| content.bytes.as_slice())
    }

    pub fn content_state(&self, digest: &str) -> ContentState {
        let digest = digest.to_ascii_lowercase();
        if self.content.contains_key(&digest) {
            ContentState::Available
        } else if self.load_diagnostics.iter().any(|diagnostic| {
            diagnostic.content_sha256.as_deref() == Some(digest.as_str())
                && matches!(
                    diagnostic.kind,
                    ContentDiagnosticKind::Corrupt | ContentDiagnosticKind::SizeMismatch
                )
        }) {
            ContentState::Corrupt
        } else {
            ContentState::Missing
        }
    }

    pub fn add_content(&mut self, bytes: Vec<u8>, media_type: impl Into<String>) -> ContentInsert {
        self.add_digested_content(DigestedBytes::new(bytes), media_type)
    }

    /// Insert bytes whose SHA-256 was already computed by [`DigestedBytes::new`].
    /// Skips a second hash of the same immutable buffer on the hot path.
    pub fn add_digested_content(
        &mut self,
        digested: DigestedBytes,
        media_type: impl Into<String>,
    ) -> ContentInsert {
        let digest = digested.digest().to_owned();
        let deduplicated = self.content.contains_key(&digest);
        self.content
            .entry(digest.clone())
            .or_insert_with(|| BundledContent {
                media_type: media_type.into(),
                bytes: digested.into_bytes(),
            });
        self.invalid_content
            .retain(|wire| !wire.content_sha256.eq_ignore_ascii_case(&digest));
        self.load_diagnostics.retain(|diagnostic| {
            diagnostic.content_sha256.as_deref() != Some(digest.as_str())
                || diagnostic.kind == ContentDiagnosticKind::Duplicate
        });
        ContentInsert {
            content_sha256: digest,
            deduplicated,
        }
    }

    pub fn add_content_with_digest(
        &mut self,
        bytes: Vec<u8>,
        media_type: impl Into<String>,
        expected_digest: &str,
    ) -> Result<ContentInsert, ProjectError> {
        require_sha256("bundled content", expected_digest)?;
        let expected_digest = expected_digest.to_ascii_lowercase();
        let digested = DigestedBytes::new(bytes);
        if digested.digest() != expected_digest {
            return Err(ProjectError::ContentHashMismatch {
                expected: expected_digest,
                actual: digested.digest().to_owned(),
            });
        }
        Ok(self.add_digested_content(digested, media_type))
    }

    pub fn relink_content(
        &mut self,
        expected_digest: &str,
        path: &Path,
        media_type: impl Into<String>,
    ) -> Result<ContentInsert, ProjectError> {
        let bytes = std::fs::read(path).map_err(|error| ProjectError::Io(error.to_string()))?;
        self.add_content_with_digest(bytes, media_type, expected_digest)
    }

    pub fn diagnostics(&self) -> Vec<ContentDiagnostic> {
        let references = self.manifest.content_references();
        let mut diagnostics = self.load_diagnostics.clone();
        for diagnostic in &mut diagnostics {
            if let Some(digest) = diagnostic.content_sha256.as_deref() {
                diagnostic.references = references.get(digest).cloned().unwrap_or_default();
            }
        }
        for (digest, owners) in references {
            if self.content.contains_key(&digest)
                || diagnostics.iter().any(|diagnostic| {
                    diagnostic.content_sha256.as_deref() == Some(digest.as_str())
                        && matches!(
                            diagnostic.kind,
                            ContentDiagnosticKind::Corrupt | ContentDiagnosticKind::SizeMismatch
                        )
                })
            {
                continue;
            }
            let owner_summary = owners
                .iter()
                .map(|owner| format!("{} {}", owner.role.label(), owner.object_id))
                .collect::<Vec<_>>()
                .join(", ");
            diagnostics.push(ContentDiagnostic {
                kind: ContentDiagnosticKind::Missing,
                content_sha256: Some(digest.clone()),
                references: owners,
                detail: format!(
                    "Bundled content {digest} is missing; affected objects: {owner_summary}"
                ),
            });
        }
        diagnostics
    }

    pub fn summary(&self) -> BundleSummary {
        let required: BTreeSet<String> = self.manifest.content_references().into_keys().collect();
        let available: Vec<&BundledContent> = required
            .iter()
            .filter_map(|digest| self.content.get(digest))
            .collect();
        BundleSummary {
            required_objects: required.len(),
            available_objects: available.len(),
            available_bytes: available
                .iter()
                .map(|content| content.bytes.len() as u64)
                .sum(),
            diagnostics: self.diagnostics().len(),
        }
    }

    pub fn save_atomic(&self, path: &Path) -> Result<(), ProjectError> {
        let bytes = self.to_bytes()?;
        write_atomic_bytes(path, &bytes, WritePrecondition::Any)
            .map(|_| ())
            .map_err(Into::into)
    }

    pub(crate) fn save_atomic_checked(
        &self,
        path: &Path,
        precondition: WritePrecondition<'_>,
    ) -> Result<String, ProjectError> {
        let bytes = self.to_bytes()?;
        write_atomic_bytes(path, &bytes, precondition).map_err(Into::into)
    }

    pub fn open(path: &Path) -> Result<Self, ProjectError> {
        Self::open_with_migration(path).map(|(document, _)| document)
    }

    pub fn open_with_migration(path: &Path) -> Result<(Self, Option<u32>), ProjectError> {
        Self::open_with_migration_and_digest(path)
            .map(|(document, migrated_from, _)| (document, migrated_from))
    }

    pub(crate) fn open_with_migration_and_digest(
        path: &Path,
    ) -> Result<(Self, Option<u32>, String), ProjectError> {
        let bytes = std::fs::read(path).map_err(|error| ProjectError::Io(error.to_string()))?;
        let digest = sha256_hex(&bytes);
        Self::decode(&bytes).map(|(document, migrated_from)| (document, migrated_from, digest))
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, ProjectError> {
        self.manifest.validate_loaded()?;
        let canonical_manifest = serde_json::to_vec(&self.manifest)
            .map_err(|error| ProjectError::Json(error.to_string()))?;
        let manifest_integrity_sha256 = sha256_hex(&canonical_manifest);
        let required: BTreeSet<String> = self.manifest.content_references().into_keys().collect();
        let mut bundled_content = Vec::new();
        for digest in &required {
            if let Some(content) = self.content.get(digest) {
                bundled_content.push(BundledContentWire {
                    content_sha256: digest.clone(),
                    byte_size: content.bytes.len() as u64,
                    media_type: content.media_type.clone(),
                    encoding: "hex".into(),
                    data_hex: hex_encode(&content.bytes),
                });
            }
        }
        for invalid in &self.invalid_content {
            let digest = invalid.content_sha256.to_ascii_lowercase();
            if required.contains(&digest)
                && !self.content.contains_key(&digest)
                && !bundled_content
                    .iter()
                    .any(|wire| wire.content_sha256.eq_ignore_ascii_case(&digest))
            {
                bundled_content.push(invalid.clone());
            }
        }
        bundled_content.sort_by(|left, right| {
            left.content_sha256
                .cmp(&right.content_sha256)
                .then_with(|| left.media_type.cmp(&right.media_type))
                .then_with(|| left.byte_size.cmp(&right.byte_size))
                .then_with(|| left.encoding.cmp(&right.encoding))
                .then_with(|| left.data_hex.cmp(&right.data_hex))
        });
        let bundle_integrity_sha256 = Some(bundle_integrity(
            &manifest_integrity_sha256,
            &bundled_content,
        )?);
        let envelope = ProjectEnvelope {
            schema_version: PROJECT_SCHEMA_VERSION,
            manifest: self.manifest.clone(),
            integrity_sha256: manifest_integrity_sha256,
            bundled_content,
            bundle_integrity_sha256,
        };
        let mut bytes = serde_json::to_vec_pretty(&envelope)
            .map_err(|error| ProjectError::Json(error.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ProjectError> {
        Self::decode(bytes).map(|(document, _)| document)
    }

    fn decode(bytes: &[u8]) -> Result<(Self, Option<u32>), ProjectError> {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|error| ProjectError::Json(error.to_string()))?;
        let version = value["schema_version"].as_u64().ok_or_else(|| {
            ProjectError::Json("project envelope is missing schema_version".into())
        })? as u32;
        let (manifest, migrated_from, bundled_content, bundle_digest, mut load_diagnostics) =
            match version {
                1 => {
                    let envelope: ProjectEnvelopeV1 = serde_json::from_value(value)
                        .map_err(|error| ProjectError::Json(error.to_string()))?;
                    if envelope.manifest.schema_version != 1 {
                        return Err(ProjectError::UnsupportedSchema(
                            envelope.manifest.schema_version,
                        ));
                    }
                    verify_manifest_integrity(
                        &envelope.manifest,
                        &envelope.integrity_sha256,
                        true,
                    )?;
                    (
                        envelope.manifest.into(),
                        Some(1),
                        Vec::new(),
                        None,
                        Vec::new(),
                    )
                }
                2 => {
                    let envelope: ProjectEnvelope = serde_json::from_value(value)
                        .map_err(|error| ProjectError::Json(error.to_string()))?;
                    if envelope.manifest.schema_version != 2 {
                        return Err(ProjectError::UnsupportedSchema(
                            envelope.manifest.schema_version,
                        ));
                    }
                    let manifest_integrity = verify_manifest_integrity(
                        &envelope.manifest,
                        &envelope.integrity_sha256,
                        true,
                    )?;
                    let mut diagnostics = Vec::new();
                    if let Some(expected) = envelope.bundle_integrity_sha256.as_deref() {
                        let actual = bundle_integrity_for_schema(
                            2,
                            &manifest_integrity,
                            &envelope.bundled_content,
                        )?;
                        if actual != expected {
                            diagnostics.push(ContentDiagnostic {
                            kind: ContentDiagnosticKind::BundleIntegrity,
                            content_sha256: None,
                            references: Vec::new(),
                            detail: format!(
                                "Bundle index integrity mismatch: expected {expected}, received {actual}"
                            ),
                        });
                        }
                    }
                    let mut manifest = envelope.manifest;
                    manifest.schema_version = PROJECT_SCHEMA_VERSION;
                    (
                        manifest,
                        Some(2),
                        envelope.bundled_content,
                        None,
                        diagnostics,
                    )
                }
                PROJECT_SCHEMA_VERSION => {
                    let envelope: ProjectEnvelope = serde_json::from_value(value)
                        .map_err(|error| ProjectError::Json(error.to_string()))?;
                    if envelope.manifest.schema_version != PROJECT_SCHEMA_VERSION {
                        return Err(ProjectError::UnsupportedSchema(
                            envelope.manifest.schema_version,
                        ));
                    }
                    verify_manifest_integrity(
                        &envelope.manifest,
                        &envelope.integrity_sha256,
                        false,
                    )?;
                    (
                        envelope.manifest,
                        None,
                        envelope.bundled_content,
                        envelope.bundle_integrity_sha256,
                        Vec::new(),
                    )
                }
                unsupported => return Err(ProjectError::UnsupportedSchema(unsupported)),
            };
        manifest.validate_loaded()?;
        let manifest_integrity = sha256_hex(
            &serde_json::to_vec(&manifest)
                .map_err(|error| ProjectError::Json(error.to_string()))?,
        );
        if let Some(expected) = bundle_digest {
            let actual = bundle_integrity(&manifest_integrity, &bundled_content)?;
            if actual != expected {
                load_diagnostics.push(ContentDiagnostic {
                    kind: ContentDiagnosticKind::BundleIntegrity,
                    content_sha256: None,
                    references: Vec::new(),
                    detail: format!(
                        "Bundle index integrity mismatch: expected {expected}, received {actual}"
                    ),
                });
            }
        }
        let mut content = BTreeMap::new();
        let mut invalid_content = Vec::new();
        let mut seen = BTreeSet::new();
        let mut needs_normalization = migrated_from.is_some();
        for wire in bundled_content {
            let digest = wire.content_sha256.to_ascii_lowercase();
            if !seen.insert(digest.clone()) {
                needs_normalization = true;
                load_diagnostics.push(ContentDiagnostic {
                    kind: ContentDiagnosticKind::Duplicate,
                    content_sha256: Some(digest.clone()),
                    references: Vec::new(),
                    detail: format!(
                        "Duplicate bundled object {digest} was deduplicated without changing manifest references"
                    ),
                });
            }
            if require_sha256("bundled content", &digest).is_err() || wire.encoding != "hex" {
                invalid_content.push(wire);
                continue;
            }
            let decoded = match hex_decode(&wire.data_hex) {
                Ok(decoded) => decoded,
                Err(error) => {
                    load_diagnostics.push(ContentDiagnostic {
                        kind: ContentDiagnosticKind::Corrupt,
                        content_sha256: Some(digest),
                        references: Vec::new(),
                        detail: format!("Bundled content could not be decoded: {error}"),
                    });
                    invalid_content.push(wire);
                    continue;
                }
            };
            if decoded.len() as u64 != wire.byte_size {
                load_diagnostics.push(ContentDiagnostic {
                    kind: ContentDiagnosticKind::SizeMismatch,
                    content_sha256: Some(digest),
                    references: Vec::new(),
                    detail: format!(
                        "Bundled content declares {} bytes but contains {}",
                        wire.byte_size,
                        decoded.len()
                    ),
                });
                invalid_content.push(wire);
                continue;
            }
            let actual = sha256_hex(&decoded);
            if actual != digest {
                load_diagnostics.push(ContentDiagnostic {
                    kind: ContentDiagnosticKind::Corrupt,
                    content_sha256: Some(digest),
                    references: Vec::new(),
                    detail: format!(
                        "Bundled content hash mismatch: decoded bytes have SHA-256 {actual}"
                    ),
                });
                invalid_content.push(wire);
                continue;
            }
            content.entry(actual).or_insert(BundledContent {
                media_type: wire.media_type,
                bytes: decoded,
            });
        }
        let mut document = Self {
            manifest,
            content,
            invalid_content,
            load_diagnostics,
            needs_normalization,
        };
        if document.promote_legacy_evidence_snapshots()? {
            document.needs_normalization = true;
        }
        Ok((document, migrated_from))
    }

    fn promote_legacy_evidence_snapshots(&mut self) -> Result<bool, ProjectError> {
        let mut promoted = false;
        for artifact in &self.manifest.evidence {
            let digest = artifact.content_sha256.to_ascii_lowercase();
            if self.content.contains_key(&digest)
                || self
                    .invalid_content
                    .iter()
                    .any(|wire| wire.content_sha256.eq_ignore_ascii_case(&digest))
            {
                continue;
            }
            let Some(snapshot) = artifact.metadata.get("snapshot") else {
                continue;
            };
            let bytes = serde_json::to_vec(snapshot)
                .map_err(|error| ProjectError::Json(error.to_string()))?;
            if bytes.len() as u64 == artifact.byte_size && sha256_hex(&bytes) == digest {
                self.content.insert(
                    digest,
                    BundledContent {
                        media_type: artifact.media_type.clone(),
                        bytes,
                    },
                );
                promoted = true;
            }
        }
        Ok(promoted)
    }
}

impl ProjectManifest {
    pub fn new(name: impl Into<String>, now_utc_unix: u64) -> Self {
        Self {
            schema_version: PROJECT_SCHEMA_VERSION,
            project_id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            created_utc_unix: now_utc_unix,
            modified_utc_unix: now_utc_unix,
            source_revisions: Vec::new(),
            cases: Vec::new(),
            evidence: Vec::new(),
            events: Vec::new(),
            selection: ProjectSelection::default(),
        }
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn created_utc_unix(&self) -> u64 {
        self.created_utc_unix
    }

    pub fn modified_utc_unix(&self) -> u64 {
        self.modified_utc_unix
    }

    /// Rename project metadata without exposing mutable access to runs or
    /// evidence. An empty name retains the honest unsaved-project label.
    pub fn rename(&mut self, name: impl Into<String>, now_utc_unix: u64) {
        let name = name.into();
        self.name = if name.trim().is_empty() {
            "Unsaved project".into()
        } else {
            name.trim().into()
        };
        self.modified_utc_unix = now_utc_unix;
    }

    pub fn cases(&self) -> &[CaseRecord] {
        &self.cases
    }

    pub fn evidence(&self) -> &[EvidenceArtifact] {
        &self.evidence
    }

    pub fn events(&self) -> &[ProjectEvent] {
        &self.events
    }

    pub fn source_revisions(&self) -> &[SourceRevision] {
        &self.source_revisions
    }

    pub fn selection(&self) -> &ProjectSelection {
        &self.selection
    }

    pub fn case(&self, case_id: &str) -> Option<&CaseRecord> {
        self.cases.iter().find(|case| case.case_id == case_id)
    }

    pub fn case_by_contract_kind(&self, kind: &str) -> Option<&CaseRecord> {
        self.cases.iter().find(|case| {
            case.active_revision()
                .contract
                .get("kind")
                .and_then(serde_json::Value::as_str)
                == Some(kind)
        })
    }

    pub fn run(&self, run_id: &str) -> Option<&RunRecord> {
        self.cases
            .iter()
            .flat_map(|case| &case.runs)
            .find(|run| run.run_id == run_id)
    }

    pub fn evidence_artifact(&self, evidence_id: &str) -> Option<&EvidenceArtifact> {
        self.evidence
            .iter()
            .find(|artifact| artifact.evidence_id == evidence_id)
    }

    pub fn evidence_for_run(&self, run_id: &str) -> Vec<&EvidenceArtifact> {
        self.evidence
            .iter()
            .filter(|artifact| artifact.run_ids.iter().any(|candidate| candidate == run_id))
            .collect()
    }

    pub fn source_by_digest(&self, digest: &str) -> Option<&SourceRevision> {
        self.source_revisions
            .iter()
            .find(|source| source.content_sha256.eq_ignore_ascii_case(digest))
    }

    pub fn run_count(&self) -> usize {
        self.cases.iter().map(|case| case.runs.len()).sum()
    }

    pub fn missing_dependencies(&self) -> Vec<String> {
        let mut missing = std::collections::BTreeSet::new();
        for dependency in self
            .cases
            .iter()
            .flat_map(|case| &case.runs)
            .flat_map(|run| &run.manifest.missing_dependencies)
        {
            missing.insert(dependency.clone());
        }
        missing.into_iter().collect()
    }

    pub fn content_references(&self) -> BTreeMap<String, Vec<ContentReference>> {
        let mut references: BTreeMap<String, Vec<ContentReference>> = BTreeMap::new();
        for source in &self.source_revisions {
            references
                .entry(source.content_sha256.to_ascii_lowercase())
                .or_default()
                .push(ContentReference {
                    object_id: source.source_revision_id.clone(),
                    role: ContentRole::Source,
                });
        }
        for run in self.cases.iter().flat_map(|case| &case.runs) {
            for digest in &run.manifest.output_sha256 {
                references
                    .entry(digest.to_ascii_lowercase())
                    .or_default()
                    .push(ContentReference {
                        object_id: run.run_id.clone(),
                        role: ContentRole::RunOutput,
                    });
            }
        }
        for artifact in &self.evidence {
            references
                .entry(artifact.content_sha256.to_ascii_lowercase())
                .or_default()
                .push(ContentReference {
                    object_id: artifact.evidence_id.clone(),
                    role: ContentRole::Evidence,
                });
        }
        for owners in references.values_mut() {
            owners.sort_by(|left, right| {
                left.object_id
                    .cmp(&right.object_id)
                    .then_with(|| left.role.label().cmp(right.role.label()))
            });
            owners.dedup();
        }
        references
    }

    pub fn add_source_revision(
        &mut self,
        source: SourceRevision,
        now_utc_unix: u64,
    ) -> Result<(), ProjectError> {
        self.ensure_unique_id(&source.source_revision_id)?;
        require_sha256(&source.source_revision_id, &source.content_sha256)?;
        if let Some(parent) = &source.parent_revision_id {
            if !self
                .source_revisions
                .iter()
                .any(|candidate| candidate.source_revision_id == *parent)
            {
                return Err(ProjectError::MissingObject {
                    object_id: parent.clone(),
                });
            }
        }
        self.source_revisions.push(source);
        self.modified_utc_unix = now_utc_unix;
        Ok(())
    }

    pub fn create_case(
        &mut self,
        case_id: impl Into<String>,
        name: impl Into<String>,
        initial_revision: CaseRevision,
        now_utc_unix: u64,
    ) -> Result<(), ProjectError> {
        let case_id = case_id.into();
        self.ensure_unique_id(&case_id)?;
        self.ensure_unique_id(&initial_revision.case_revision_id)?;
        self.validate_case_revision_sources(&initial_revision)?;
        if initial_revision.parent_revision_id.is_some() {
            return Err(ProjectError::InvalidLineage(
                "an initial case revision cannot have a parent".into(),
            ));
        }
        let active_revision_id = initial_revision.case_revision_id.clone();
        self.cases.push(CaseRecord {
            case_id,
            name: name.into(),
            revisions: vec![initial_revision],
            active_revision_id,
            stale_stages: Vec::new(),
            runs: Vec::new(),
        });
        self.modified_utc_unix = now_utc_unix;
        Ok(())
    }

    pub fn append_case_revision(
        &mut self,
        case_id: &str,
        revision: CaseRevision,
        now_utc_unix: u64,
    ) -> Result<Vec<DependencyStage>, ProjectError> {
        self.ensure_unique_id(&revision.case_revision_id)?;
        self.validate_case_revision_sources(&revision)?;
        let case = self
            .cases
            .iter_mut()
            .find(|case| case.case_id == case_id)
            .ok_or_else(|| ProjectError::MissingObject {
                object_id: case_id.into(),
            })?;
        if revision.parent_revision_id.as_deref() != Some(&case.active_revision_id) {
            return Err(ProjectError::InvalidLineage(format!(
                "case revision {} must parent active revision {}",
                revision.case_revision_id, case.active_revision_id
            )));
        }
        let active = case
            .revisions
            .iter()
            .find(|candidate| candidate.case_revision_id == case.active_revision_id)
            .expect("active case revision is internal schema state");
        let stale = active.stale_stages_against(&revision);
        case.active_revision_id = revision.case_revision_id.clone();
        case.stale_stages = stale.clone();
        case.revisions.push(revision);
        self.modified_utc_unix = now_utc_unix;
        Ok(stale)
    }

    pub fn append_run(
        &mut self,
        case_id: &str,
        run: RunRecord,
        now_utc_unix: u64,
    ) -> Result<(), ProjectError> {
        self.ensure_unique_id(&run.run_id)?;
        validate_run(&run)?;
        let case = self
            .cases
            .iter_mut()
            .find(|case| case.case_id == case_id)
            .ok_or_else(|| ProjectError::MissingObject {
                object_id: case_id.into(),
            })?;
        if !case
            .revisions
            .iter()
            .any(|revision| revision.case_revision_id == run.case_revision_id)
        {
            return Err(ProjectError::MissingObject {
                object_id: run.case_revision_id,
            });
        }
        if let Some(parent) = &run.parent_run_id {
            let Some(parent_run) = case
                .runs
                .iter()
                .find(|candidate| candidate.run_id == *parent)
            else {
                return Err(ProjectError::InvalidLineage(format!(
                    "rerun parent {parent} is not in case {case_id}"
                )));
            };
            validate_determinism(&run, parent_run)?;
        } else if run.manifest.determinism.is_some() {
            return Err(ProjectError::InvalidLineage(format!(
                "run {} has determinism evidence without a parent run",
                run.run_id
            )));
        }
        if run.case_revision_id == case.active_revision_id
            && matches!(
                run.state,
                LifecycleState::Succeeded | LifecycleState::EvidenceLocked
            )
        {
            case.stale_stages.retain(|stage| {
                !matches!(
                    stage,
                    DependencyStage::Contract
                        | DependencyStage::Discretization
                        | DependencyStage::Run
                )
            });
        }
        case.runs.push(run);
        self.modified_utc_unix = now_utc_unix;
        Ok(())
    }

    /// Move a persisted pending/running attempt to one terminal state while
    /// preserving its identity, exact submitted inputs, and lineage.
    pub fn finish_run_attempt(
        &mut self,
        case_id: &str,
        run: RunRecord,
        now_utc_unix: u64,
    ) -> Result<(), ProjectError> {
        if !matches!(
            run.state,
            LifecycleState::Succeeded
                | LifecycleState::Failed
                | LifecycleState::Cancelled
                | LifecycleState::EvidenceLocked
        ) {
            return Err(ProjectError::NonTerminalRun(run.run_id));
        }
        validate_run(&run)?;
        let case = self
            .cases
            .iter_mut()
            .find(|case| case.case_id == case_id)
            .ok_or_else(|| ProjectError::MissingObject {
                object_id: case_id.into(),
            })?;
        let run_index = case
            .runs
            .iter()
            .position(|candidate| candidate.run_id == run.run_id)
            .ok_or_else(|| ProjectError::MissingObject {
                object_id: run.run_id.clone(),
            })?;
        let prior = &case.runs[run_index];
        if !matches!(
            prior.state,
            LifecycleState::Pending | LifecycleState::Running
        ) {
            return Err(ProjectError::InvalidLineage(format!(
                "run attempt {} is already terminal",
                run.run_id
            )));
        }
        if prior.parent_run_id != run.parent_run_id
            || prior.case_revision_id != run.case_revision_id
            || prior.created_utc_unix != run.created_utc_unix
            || !same_attempt_inputs(&prior.manifest, &run.manifest)
        {
            return Err(ProjectError::InvalidLineage(format!(
                "run attempt {} changed immutable submitted inputs or lineage",
                run.run_id
            )));
        }
        if let Some(parent) = &run.parent_run_id {
            let parent_run = case
                .runs
                .iter()
                .find(|candidate| candidate.run_id == *parent)
                .ok_or_else(|| {
                    ProjectError::InvalidLineage(format!(
                        "rerun parent {parent} is not in case {case_id}"
                    ))
                })?;
            validate_determinism(&run, parent_run)?;
        } else if run.manifest.determinism.is_some() {
            return Err(ProjectError::InvalidLineage(format!(
                "run {} has determinism evidence without a parent run",
                run.run_id
            )));
        }
        let clears_run_staleness = run.case_revision_id == case.active_revision_id
            && matches!(
                run.state,
                LifecycleState::Succeeded | LifecycleState::EvidenceLocked
            );
        case.runs[run_index] = run;
        if clears_run_staleness {
            case.stale_stages.retain(|stage| {
                !matches!(
                    stage,
                    DependencyStage::Contract
                        | DependencyStage::Discretization
                        | DependencyStage::Run
                )
            });
        }
        self.modified_utc_unix = now_utc_unix;
        Ok(())
    }

    pub fn append_evidence(
        &mut self,
        artifact: EvidenceArtifact,
        now_utc_unix: u64,
    ) -> Result<(), ProjectError> {
        self.ensure_unique_id(&artifact.evidence_id)?;
        require_sha256(&artifact.evidence_id, &artifact.content_sha256)?;
        validate_calibrated_views(&artifact.evidence_id, &artifact.calibrated_views)?;
        validate_signature_artifact(&artifact, &self.evidence)?;
        if artifact.run_ids.is_empty() {
            return Err(ProjectError::InvalidLineage(
                "evidence must identify at least one immutable run".into(),
            ));
        }
        let mut active_case_ids = std::collections::BTreeSet::new();
        for run_id in &artifact.run_ids {
            let Some((case_id, active)) = self.run_case(run_id) else {
                return Err(ProjectError::MissingObject {
                    object_id: run_id.clone(),
                });
            };
            if active {
                active_case_ids.insert(case_id.to_owned());
            }
        }
        self.evidence.push(artifact);
        for case in &mut self.cases {
            if active_case_ids.contains(&case.case_id) {
                case.stale_stages
                    .retain(|stage| *stage != DependencyStage::Evidence);
            }
        }
        self.modified_utc_unix = now_utc_unix;
        Ok(())
    }

    pub fn set_selection(
        &mut self,
        selection: ProjectSelection,
        now_utc_unix: u64,
    ) -> Result<(), ProjectError> {
        let selected_case = match selection.active_case_id.as_deref() {
            Some(case_id) => {
                Some(
                    self.case(case_id)
                        .ok_or_else(|| ProjectError::MissingObject {
                            object_id: case_id.into(),
                        })?,
                )
            }
            None => None,
        };
        let selected_run = match selection.selected_run_id.as_deref() {
            Some(run_id) => {
                let run = self
                    .run(run_id)
                    .ok_or_else(|| ProjectError::MissingObject {
                        object_id: run_id.into(),
                    })?;
                if selected_case.is_some_and(|case| {
                    !case.runs.iter().any(|candidate| candidate.run_id == run_id)
                }) {
                    return Err(ProjectError::InvalidLineage(format!(
                        "selected run {run_id} does not belong to the selected case"
                    )));
                }
                Some(run)
            }
            None => None,
        };
        let selected_evidence = match selection.selected_evidence_id.as_deref() {
            Some(evidence_id) => {
                let evidence = self.evidence_artifact(evidence_id).ok_or_else(|| {
                    ProjectError::MissingObject {
                        object_id: evidence_id.into(),
                    }
                })?;
                if selected_run.is_some_and(|run| {
                    !evidence
                        .run_ids
                        .iter()
                        .any(|candidate| candidate == run.run_id())
                }) {
                    return Err(ProjectError::InvalidLineage(format!(
                        "selected evidence {evidence_id} does not link the selected run"
                    )));
                }
                Some(evidence)
            }
            None => None,
        };
        if let Some(view_id) = selection.selected_view_id.as_deref() {
            let in_run = selected_run.is_some_and(|run| {
                run.calibrated_views
                    .iter()
                    .any(|view| view.view_id == view_id)
            });
            let in_evidence = selected_evidence.is_some_and(|evidence| {
                evidence
                    .calibrated_views
                    .iter()
                    .any(|view| view.view_id == view_id)
            });
            if !in_run && !in_evidence {
                return Err(ProjectError::MissingObject {
                    object_id: view_id.into(),
                });
            }
        }
        self.selection = selection;
        self.modified_utc_unix = now_utc_unix;
        Ok(())
    }

    pub fn record_event(
        &mut self,
        event: ProjectEvent,
        now_utc_unix: u64,
    ) -> Result<(), ProjectError> {
        self.ensure_unique_id(&event.event_id)?;
        self.events.push(event);
        self.modified_utc_unix = now_utc_unix;
        Ok(())
    }

    /// Atomic manifest save. Callers may use a normal path, Save As path, or a
    /// separate recovery path; engine availability is not involved.
    pub fn save_atomic(&self, path: &Path) -> Result<(), ProjectError> {
        ProjectDocument::new(self.clone()).save_atomic(path)
    }

    pub fn open(path: &Path) -> Result<Self, ProjectError> {
        ProjectDocument::open(path).map(|document| document.manifest)
    }

    pub fn open_with_migration(path: &Path) -> Result<(Self, Option<u32>), ProjectError> {
        ProjectDocument::open_with_migration(path)
            .map(|(document, migrated_from)| (document.manifest, migrated_from))
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, ProjectError> {
        ProjectDocument::new(self.clone()).to_bytes()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ProjectError> {
        ProjectDocument::from_bytes(bytes).map(|document| document.manifest)
    }

    #[cfg(test)]
    fn decode(bytes: &[u8]) -> Result<(Self, Option<u32>), ProjectError> {
        ProjectDocument::decode(bytes)
            .map(|(document, migrated_from)| (document.manifest, migrated_from))
    }

    fn validate_loaded(&self) -> Result<(), ProjectError> {
        let mut ids = std::collections::HashSet::new();
        let mut insert = |id: &str| {
            if ids.insert(id.to_owned()) {
                Ok(())
            } else {
                Err(ProjectError::DuplicateId {
                    object_id: id.into(),
                })
            }
        };
        insert(&self.project_id)?;
        for source in &self.source_revisions {
            insert(&source.source_revision_id)?;
            require_sha256(&source.source_revision_id, &source.content_sha256)?;
        }
        for source in &self.source_revisions {
            if let Some(parent) = &source.parent_revision_id {
                if !self
                    .source_revisions
                    .iter()
                    .any(|candidate| candidate.source_revision_id == *parent)
                {
                    return Err(ProjectError::MissingObject {
                        object_id: parent.clone(),
                    });
                }
            }
        }
        for case in &self.cases {
            insert(&case.case_id)?;
            let unique_stages: std::collections::BTreeSet<_> =
                case.stale_stages.iter().copied().collect();
            if unique_stages.len() != case.stale_stages.len() {
                return Err(ProjectError::InvalidLineage(format!(
                    "case {} contains duplicate stale stages",
                    case.case_id
                )));
            }
            if !case
                .revisions
                .iter()
                .any(|revision| revision.case_revision_id == case.active_revision_id)
            {
                return Err(ProjectError::MissingObject {
                    object_id: case.active_revision_id.clone(),
                });
            }
            let mut prior_revisions = std::collections::HashSet::new();
            for (index, revision) in case.revisions.iter().enumerate() {
                insert(&revision.case_revision_id)?;
                if (index == 0 && revision.parent_revision_id.is_some())
                    || (index > 0
                        && revision
                            .parent_revision_id
                            .as_ref()
                            .is_none_or(|parent| !prior_revisions.contains(parent)))
                {
                    return Err(ProjectError::InvalidLineage(format!(
                        "case revision {} does not parent an earlier revision",
                        revision.case_revision_id
                    )));
                }
                self.validate_case_revision_sources(revision)?;
                prior_revisions.insert(revision.case_revision_id.clone());
            }
            let mut prior_runs = std::collections::HashSet::new();
            for run in &case.runs {
                insert(&run.run_id)?;
                if !prior_revisions.contains(&run.case_revision_id) {
                    return Err(ProjectError::MissingObject {
                        object_id: run.case_revision_id.clone(),
                    });
                }
                if let Some(parent) = &run.parent_run_id {
                    if !prior_runs.contains(parent) {
                        return Err(ProjectError::InvalidLineage(format!(
                            "run {} does not parent an earlier run in its case",
                            run.run_id
                        )));
                    }
                    let parent_run = case
                        .runs
                        .iter()
                        .find(|candidate| candidate.run_id == *parent)
                        .expect("prior run ID was checked");
                    validate_determinism(run, parent_run)?;
                } else if run.manifest.determinism.is_some() {
                    return Err(ProjectError::InvalidLineage(format!(
                        "run {} has determinism evidence without a parent run",
                        run.run_id
                    )));
                }
                validate_run(run)?;
                prior_runs.insert(run.run_id.clone());
            }
        }
        for (artifact_index, artifact) in self.evidence.iter().enumerate() {
            insert(&artifact.evidence_id)?;
            require_sha256(&artifact.evidence_id, &artifact.content_sha256)?;
            if artifact.run_ids.is_empty() {
                return Err(ProjectError::InvalidLineage(format!(
                    "evidence {} has no source run",
                    artifact.evidence_id
                )));
            }
            for run_id in &artifact.run_ids {
                if !self.run_exists(run_id) {
                    return Err(ProjectError::MissingObject {
                        object_id: run_id.clone(),
                    });
                }
            }
            validate_calibrated_views(&artifact.evidence_id, &artifact.calibrated_views)?;
            validate_signature_artifact(artifact, &self.evidence[..artifact_index])?;
        }
        for event in &self.events {
            insert(&event.event_id)?;
        }
        let mut selection_candidate = self.clone();
        selection_candidate.set_selection(self.selection.clone(), self.modified_utc_unix)?;
        Ok(())
    }

    fn ensure_unique_id(&self, object_id: &str) -> Result<(), ProjectError> {
        let duplicate = self.project_id == object_id
            || self
                .source_revisions
                .iter()
                .any(|source| source.source_revision_id == object_id)
            || self.cases.iter().any(|case| {
                case.case_id == object_id
                    || case
                        .revisions
                        .iter()
                        .any(|revision| revision.case_revision_id == object_id)
                    || case.runs.iter().any(|run| run.run_id == object_id)
            })
            || self
                .evidence
                .iter()
                .any(|artifact| artifact.evidence_id == object_id)
            || self.events.iter().any(|event| event.event_id == object_id);
        if duplicate {
            Err(ProjectError::DuplicateId {
                object_id: object_id.into(),
            })
        } else {
            Ok(())
        }
    }

    fn validate_case_revision_sources(&self, revision: &CaseRevision) -> Result<(), ProjectError> {
        for source_id in &revision.source_revision_ids {
            if !self
                .source_revisions
                .iter()
                .any(|source| source.source_revision_id == *source_id)
            {
                return Err(ProjectError::MissingObject {
                    object_id: source_id.clone(),
                });
            }
        }
        Ok(())
    }

    fn run_exists(&self, run_id: &str) -> bool {
        self.cases
            .iter()
            .any(|case| case.runs.iter().any(|run| run.run_id == run_id))
    }

    fn run_case(&self, run_id: &str) -> Option<(&str, bool)> {
        self.cases.iter().find_map(|case| {
            case.runs
                .iter()
                .find(|run| run.run_id == run_id)
                .map(|run| {
                    (
                        case.case_id.as_str(),
                        run.case_revision_id == case.active_revision_id,
                    )
                })
        })
    }
}

impl CaseRecord {
    pub fn case_id(&self) -> &str {
        &self.case_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn active_revision_id(&self) -> &str {
        &self.active_revision_id
    }

    pub fn active_revision(&self) -> &CaseRevision {
        self.revisions
            .iter()
            .find(|revision| revision.case_revision_id == self.active_revision_id)
            .expect("validated case contains its active revision")
    }

    pub fn revisions(&self) -> &[CaseRevision] {
        &self.revisions
    }

    pub fn runs(&self) -> &[RunRecord] {
        &self.runs
    }

    pub fn stale_stages(&self) -> &[DependencyStage] {
        &self.stale_stages
    }

    pub fn stale_stages_for_run(&self, run_id: &str) -> Option<Vec<DependencyStage>> {
        let run = self.runs.iter().find(|run| run.run_id == run_id)?;
        let revision = self
            .revisions
            .iter()
            .find(|revision| revision.case_revision_id == run.case_revision_id)?;
        Some(revision.stale_stages_against(self.active_revision()))
    }

    pub fn latest_run_for_active_revision(&self) -> Option<&RunRecord> {
        self.runs
            .iter()
            .rev()
            .find(|run| run.case_revision_id == self.active_revision_id)
    }
}

impl CaseRevision {
    pub fn stale_stages_against(&self, next: &Self) -> Vec<DependencyStage> {
        let mut stale = std::collections::BTreeSet::new();
        if self.source_revision_ids != next.source_revision_ids {
            stale.extend([
                DependencyStage::Contract,
                DependencyStage::Discretization,
                DependencyStage::Run,
                DependencyStage::Evidence,
            ]);
        }
        if self.contract != next.contract {
            stale.extend([
                DependencyStage::Discretization,
                DependencyStage::Run,
                DependencyStage::Evidence,
            ]);
        }
        if self.discretization != next.discretization {
            stale.extend([DependencyStage::Run, DependencyStage::Evidence]);
        }
        if self.outputs != next.outputs {
            stale.extend([DependencyStage::Run, DependencyStage::Evidence]);
        }
        stale.into_iter().collect()
    }
}

impl RunManifest {
    pub fn compare_scalars_against(&mut self, parent: &RunRecord) {
        let parent_by_key: std::collections::BTreeMap<_, _> = parent
            .manifest
            .scalar_outputs
            .iter()
            .map(|scalar| (scalar.key.as_str(), scalar))
            .collect();
        let current_by_key: std::collections::BTreeMap<_, _> = self
            .scalar_outputs
            .iter()
            .map(|scalar| (scalar.key.as_str(), scalar))
            .collect();
        let keys: std::collections::BTreeSet<_> = parent_by_key
            .keys()
            .chain(current_by_key.keys())
            .copied()
            .collect();
        let mut comparable = !keys.is_empty();
        let mut within_tolerance = true;
        let mut differences = Vec::with_capacity(keys.len());
        for key in keys {
            let parent_scalar = parent_by_key.get(key).copied();
            let current_scalar = current_by_key.get(key).copied();
            let same_units = parent_scalar
                .zip(current_scalar)
                .is_some_and(|(left, right)| left.units == right.units);
            let (abs_difference, abs_tolerance) = match (parent_scalar, current_scalar, same_units)
            {
                (Some(left), Some(right), true) => {
                    let difference = (right.value - left.value).abs();
                    let tolerance = left.abs_tolerance.max(right.abs_tolerance);
                    if difference > tolerance {
                        within_tolerance = false;
                    }
                    (Some(difference), Some(tolerance))
                }
                _ => {
                    comparable = false;
                    (None, None)
                }
            };
            differences.push(ScalarDifference {
                key: key.into(),
                parent_value: parent_scalar.map(|scalar| scalar.value),
                current_value: current_scalar.map(|scalar| scalar.value),
                abs_difference,
                abs_tolerance,
            });
        }
        self.determinism = Some(DeterminismRecord {
            parent_run_id: parent.run_id.clone(),
            status: if !comparable {
                DeterminismStatus::NotComparable
            } else if within_tolerance {
                DeterminismStatus::WithinTolerance
            } else {
                DeterminismStatus::Difference
            },
            differences,
        });
    }
}

impl RunRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run_id: impl Into<String>,
        parent_run_id: Option<String>,
        case_revision_id: impl Into<String>,
        created_utc_unix: u64,
        completed_utc_unix: u64,
        state: LifecycleState,
        manifest: RunManifest,
        calibrated_views: Vec<CalibratedView>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            parent_run_id,
            case_revision_id: case_revision_id.into(),
            created_utc_unix,
            completed_utc_unix,
            state,
            manifest,
            calibrated_views,
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn parent_run_id(&self) -> Option<&str> {
        self.parent_run_id.as_deref()
    }

    pub fn case_revision_id(&self) -> &str {
        &self.case_revision_id
    }

    pub fn created_utc_unix(&self) -> u64 {
        self.created_utc_unix
    }

    pub fn completed_utc_unix(&self) -> u64 {
        self.completed_utc_unix
    }

    pub fn state(&self) -> LifecycleState {
        self.state
    }

    pub fn manifest(&self) -> &RunManifest {
        &self.manifest
    }

    pub fn calibrated_views(&self) -> &[CalibratedView] {
        &self.calibrated_views
    }
}

fn require_sha256(object_id: &str, digest: &str) -> Result<(), ProjectError> {
    if digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(ProjectError::InvalidDigest {
            object_id: object_id.into(),
        })
    }
}

fn same_attempt_inputs(left: &RunManifest, right: &RunManifest) -> bool {
    left.schema_version == right.schema_version
        && left.app == right.app
        && left.engine == right.engine
        && left.model == right.model
        && left.solver == right.solver
        && left.converter == right.converter
        && left.exact_contract == right.exact_contract
        && left.exact_settings == right.exact_settings
        && left.seeds == right.seeds
        && left.device == right.device
        && left.waivers == right.waivers
        && left.missing_dependencies == right.missing_dependencies
}

fn validate_run(run: &RunRecord) -> Result<(), ProjectError> {
    if run.manifest.schema_version == 0 || run.manifest.schema_version > PROJECT_SCHEMA_VERSION {
        return Err(ProjectError::UnsupportedSchema(run.manifest.schema_version));
    }
    match run.state {
        LifecycleState::Pending | LifecycleState::Running => {
            if run.completed_utc_unix != 0
                || run.manifest.runtime_ms != 0
                || !run.manifest.output_sha256.is_empty()
                || !run.manifest.scalar_outputs.is_empty()
                || run.manifest.determinism.is_some()
                || !run.calibrated_views.is_empty()
                || !matches!(run.manifest.stop_reason.as_str(), "pending" | "running")
            {
                return Err(ProjectError::InvalidLineage(format!(
                    "run {} has terminal outputs or timestamps before completion",
                    run.run_id
                )));
            }
        }
        LifecycleState::Succeeded
        | LifecycleState::Failed
        | LifecycleState::Cancelled
        | LifecycleState::EvidenceLocked => {
            if run.completed_utc_unix < run.created_utc_unix {
                return Err(ProjectError::InvalidLineage(format!(
                    "run {} completes before it starts",
                    run.run_id
                )));
            }
            if run.manifest.stop_reason.trim().is_empty() {
                return Err(ProjectError::InvalidLineage(format!(
                    "run {} has no stop reason",
                    run.run_id
                )));
            }
        }
        LifecycleState::Draft | LifecycleState::Ready | LifecycleState::Stale => {
            return Err(ProjectError::InvalidLineage(format!(
                "run {} uses a non-attempt lifecycle state",
                run.run_id
            )));
        }
    }
    for component in [
        Some(&run.manifest.app),
        run.manifest.engine.as_ref(),
        run.manifest.model.as_ref(),
        run.manifest.solver.as_ref(),
        run.manifest.converter.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(digest) = &component.sha256 {
            require_sha256(&component.name, digest)?;
        }
    }
    for digest in &run.manifest.output_sha256 {
        require_sha256(&run.run_id, digest)?;
    }
    let mut scalar_keys = std::collections::BTreeSet::new();
    for scalar in &run.manifest.scalar_outputs {
        if scalar.key.trim().is_empty()
            || scalar.units.trim().is_empty()
            || !scalar.value.is_finite()
            || !scalar.abs_tolerance.is_finite()
            || scalar.abs_tolerance < 0.0
            || !scalar_keys.insert(scalar.key.as_str())
        {
            return Err(ProjectError::InvalidLineage(format!(
                "run {} contains an invalid declared scalar {}",
                run.run_id, scalar.key
            )));
        }
    }
    validate_calibrated_views(&run.run_id, &run.calibrated_views)
}

fn validate_calibrated_views(
    object_id: &str,
    calibrated_views: &[CalibratedView],
) -> Result<(), ProjectError> {
    let mut view_ids = std::collections::BTreeSet::new();
    for view in calibrated_views {
        if !view.scale_min.is_finite()
            || !view.scale_max.is_finite()
            || view.scale_min > view.scale_max
            || view.view_id.trim().is_empty()
            || view.quantity.trim().is_empty()
            || view.units.trim().is_empty()
            || view.method.trim().is_empty()
            || !view_ids.insert(view.view_id.as_str())
        {
            return Err(ProjectError::InvalidLineage(format!(
                "{object_id} contains an invalid calibrated view {}",
                view.view_id
            )));
        }
    }
    Ok(())
}

fn validate_signature_artifact(
    artifact: &EvidenceArtifact,
    prior_evidence: &[EvidenceArtifact],
) -> Result<(), ProjectError> {
    if artifact.source_class != EvidenceSourceClass::AuthenticitySignature {
        return Ok(());
    }
    let text = |key: &str| {
        artifact
            .metadata
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                ProjectError::InvalidLineage(format!(
                    "signature evidence {} is missing metadata field {key}",
                    artifact.evidence_id
                ))
            })
    };
    let kind = text("kind")?;
    let accepts_kind = matches!(kind, "benchmark_signature" | "engineering_signature");
    if artifact.media_type != "application/vnd.reyn.evidence-signature+json"
        || artifact.derivation_method.as_deref() != Some("ed25519_canonical_payload_signature")
        || artifact.derivation_version.as_deref() != Some("1")
        || !accepts_kind
        || text("signature_schema")? != "reyn.evidence-signature.v1"
        || text("algorithm")? != "Ed25519"
        || text("verification_at_creation")? != "valid"
    {
        return Err(ProjectError::InvalidLineage(format!(
            "signature evidence {} has unsupported authenticity metadata",
            artifact.evidence_id
        )));
    }
    let parent_evidence_id = text("parent_evidence_id")?;
    let canonical_report_sha256 = text("canonical_report_sha256")?;
    let canonical_payload_sha256 = text("canonical_payload_sha256")?;
    let key_fingerprint_sha256 = text("key_fingerprint_sha256")?;
    require_sha256(&artifact.evidence_id, canonical_report_sha256)?;
    require_sha256(&artifact.evidence_id, canonical_payload_sha256)?;
    require_sha256(&artifact.evidence_id, key_fingerprint_sha256)?;
    let parent = prior_evidence
        .iter()
        .find(|candidate| candidate.evidence_id == parent_evidence_id)
        .ok_or_else(|| {
            ProjectError::InvalidLineage(format!(
                "signature evidence {} must derive from an earlier signed parent artifact",
                artifact.evidence_id
            ))
        })?;
    let parent_ok = match kind {
        "benchmark_signature" => {
            parent.source_class == EvidenceSourceClass::Derived
                && parent.content_sha256 == canonical_report_sha256
                && parent.run_ids == artifact.run_ids
                && parent
                    .metadata
                    .get("canonical_payload_sha256")
                    .and_then(serde_json::Value::as_str)
                    == Some(canonical_payload_sha256)
        }
        "engineering_signature" => {
            // The immutable engineering-result JSON is both report and payload:
            // content_sha256 is the signed digest; no nested payload hash is required.
            parent.source_class == EvidenceSourceClass::Derived
                && parent.media_type == "application/vnd.reyn.engineering-result+json"
                && parent.content_sha256 == canonical_report_sha256
                && canonical_report_sha256 == canonical_payload_sha256
                && parent.run_ids == artifact.run_ids
        }
        _ => false,
    };
    if !parent_ok {
        return Err(ProjectError::InvalidLineage(format!(
            "signature evidence {} does not match its canonical parent lineage",
            artifact.evidence_id
        )));
    }
    if prior_evidence.iter().any(|candidate| {
        candidate.source_class == EvidenceSourceClass::AuthenticitySignature
            && candidate
                .metadata
                .get("canonical_payload_sha256")
                .and_then(serde_json::Value::as_str)
                == Some(canonical_payload_sha256)
            && candidate
                .metadata
                .get("key_fingerprint_sha256")
                .and_then(serde_json::Value::as_str)
                == Some(key_fingerprint_sha256)
    }) {
        return Err(ProjectError::DuplicateSignature {
            canonical_payload_sha256: canonical_payload_sha256.into(),
            key_fingerprint_sha256: key_fingerprint_sha256.into(),
        });
    }
    Ok(())
}

fn validate_determinism(run: &RunRecord, parent: &RunRecord) -> Result<(), ProjectError> {
    let Some(record) = &run.manifest.determinism else {
        return Ok(());
    };
    if record.parent_run_id != parent.run_id
        || run.parent_run_id.as_deref() != Some(record.parent_run_id.as_str())
    {
        return Err(ProjectError::InvalidLineage(format!(
            "run {} determinism evidence does not match its parent",
            run.run_id
        )));
    }
    if record.differences.iter().any(|difference| {
        difference.key.trim().is_empty()
            || difference
                .parent_value
                .is_some_and(|value| !value.is_finite())
            || difference
                .current_value
                .is_some_and(|value| !value.is_finite())
            || difference
                .abs_difference
                .is_some_and(|value| !value.is_finite() || value < 0.0)
            || difference
                .abs_tolerance
                .is_some_and(|value| !value.is_finite() || value < 0.0)
    }) {
        return Err(ProjectError::InvalidLineage(format!(
            "run {} contains invalid determinism differences",
            run.run_id
        )));
    }
    Ok(())
}

fn bundle_integrity(
    manifest_integrity_sha256: &str,
    objects: &[BundledContentWire],
) -> Result<String, ProjectError> {
    bundle_integrity_for_schema(PROJECT_SCHEMA_VERSION, manifest_integrity_sha256, objects)
}

fn bundle_integrity_for_schema(
    schema_version: u32,
    manifest_integrity_sha256: &str,
    objects: &[BundledContentWire],
) -> Result<String, ProjectError> {
    let payload = BundleIntegrityPayload {
        schema_version,
        manifest_integrity_sha256,
        objects: objects
            .iter()
            .map(|object| BundleIntegrityObject {
                content_sha256: &object.content_sha256,
                byte_size: object.byte_size,
                media_type: &object.media_type,
                encoding: &object.encoding,
            })
            .collect(),
    };
    let canonical =
        serde_json::to_vec(&payload).map_err(|error| ProjectError::Json(error.to_string()))?;
    Ok(sha256_hex(&canonical))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WritePrecondition<'a> {
    /// Replace any existing destination. Used only for an explicit Save As or
    /// machine-local state that has no retained generation.
    Any,
    /// Publish only if no destination exists.
    Missing,
    /// Publish only if the destination still has the bytes observed on open.
    Unchanged(&'a str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AtomicWriteError {
    Io(String),
    Conflict {
        expected_sha256: Option<String>,
        actual_sha256: Option<String>,
    },
}

impl From<AtomicWriteError> for ProjectError {
    fn from(error: AtomicWriteError) -> Self {
        match error {
            AtomicWriteError::Io(error) => Self::Io(error),
            AtomicWriteError::Conflict {
                expected_sha256,
                actual_sha256,
            } => Self::WriteConflict {
                expected_sha256,
                actual_sha256,
            },
        }
    }
}

pub(crate) fn write_atomic_bytes(
    path: &Path,
    bytes: &[u8],
    precondition: WritePrecondition<'_>,
) -> Result<String, AtomicWriteError> {
    write_atomic_bytes_impl(path, bytes, precondition, false)
}

/// Publish complete bytes through a unique same-directory temporary file.
///
/// The compare immediately before rename is optimistic conflict detection, not
/// a process lock: it prevents ordinary stale writers but cannot make a
/// cross-process compare-and-swap guarantee on every filesystem.
fn write_atomic_bytes_impl(
    path: &Path,
    bytes: &[u8],
    precondition: WritePrecondition<'_>,
    interrupt_after_sync: bool,
) -> Result<String, AtomicWriteError> {
    let parent = path
        .parent()
        .ok_or_else(|| AtomicWriteError::Io("path does not have a parent directory".into()))?;
    std::fs::create_dir_all(parent).map_err(|error| AtomicWriteError::Io(error.to_string()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    let temporary = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| AtomicWriteError::Io(error.to_string()))?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&temporary);
        return Err(AtomicWriteError::Io(error.to_string()));
    }
    drop(file);

    if interrupt_after_sync {
        let _ = std::fs::remove_file(&temporary);
        return Err(AtomicWriteError::Io(
            "injected interruption after temporary-file sync".into(),
        ));
    }

    let desired_sha256 = sha256_hex(bytes);
    let actual_sha256 = file_sha256(path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        AtomicWriteError::Io(error)
    })?;
    if actual_sha256.as_deref() == Some(desired_sha256.as_str()) {
        let _ = std::fs::remove_file(&temporary);
        sync_parent_directory(parent).map_err(AtomicWriteError::Io)?;
        return Ok(desired_sha256);
    }
    let conflict = match precondition {
        WritePrecondition::Any => false,
        WritePrecondition::Missing => actual_sha256.is_some(),
        WritePrecondition::Unchanged(expected) => actual_sha256.as_deref() != Some(expected),
    };
    if conflict {
        let _ = std::fs::remove_file(&temporary);
        return Err(AtomicWriteError::Conflict {
            expected_sha256: match precondition {
                WritePrecondition::Unchanged(expected) => Some(expected.to_owned()),
                WritePrecondition::Any | WritePrecondition::Missing => None,
            },
            actual_sha256,
        });
    }

    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(AtomicWriteError::Io(error.to_string()));
    }
    sync_parent_directory(parent).map_err(AtomicWriteError::Io)?;
    let published_sha256 = file_sha256(path)
        .map_err(AtomicWriteError::Io)?
        .ok_or_else(|| AtomicWriteError::Io("published file disappeared after rename".into()))?;
    if published_sha256 != desired_sha256 {
        return Err(AtomicWriteError::Io(format!(
            "published file verification failed: expected {desired_sha256}, found {published_sha256}"
        )));
    }
    Ok(desired_sha256)
}

pub(crate) fn file_sha256(path: &Path) -> Result<Option<String>, String> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Some(
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    ))
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<(), String> {
    let directory = std::fs::File::open(parent).map_err(|error| error.to_string())?;
    match directory.sync_all() {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_atomic_bytes_interrupted_after_sync(
    path: &Path,
    bytes: &[u8],
) -> Result<String, AtomicWriteError> {
    write_atomic_bytes_impl(path, bytes, WritePrecondition::Any, true)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn hex_decode(encoded: &str) -> Result<Vec<u8>, String> {
    if !encoded.len().is_multiple_of(2) {
        return Err("hex payload has an odd number of characters".into());
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_nibble(pair[0])?;
            let low = hex_nibble(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(format!("invalid hex character {:?}", byte as char)),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn verify_manifest_integrity(
    manifest: &impl Serialize,
    expected: &str,
    allow_legacy_complete_state: bool,
) -> Result<String, ProjectError> {
    let canonical =
        serde_json::to_vec(manifest).map_err(|error| ProjectError::Json(error.to_string()))?;
    if sha256_hex(&canonical) == expected {
        return Ok(expected.to_owned());
    }
    if allow_legacy_complete_state {
        let current =
            String::from_utf8(canonical).map_err(|error| ProjectError::Json(error.to_string()))?;
        let legacy = current.replace("\"state\":\"succeeded\"", "\"state\":\"complete\"");
        if sha256_hex(legacy.as_bytes()) == expected {
            return Ok(expected.to_owned());
        }
    }
    Err(ProjectError::IntegrityMismatch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn digest(character: char) -> String {
        std::iter::repeat_n(character, 64).collect()
    }

    fn source(id: &str) -> SourceRevision {
        SourceRevision {
            source_revision_id: id.into(),
            source_kind: SourceKind::Model,
            revision: 1,
            imported_utc_unix: 10,
            uri_hint: Some("/non-authoritative/machine/path/model.pth".into()),
            byte_size: 512,
            content_sha256: digest('a'),
            declared_units: None,
            frame: Some("periodic_xy".into()),
            transform_4x4: [
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
            ],
            parent_revision_id: None,
            warnings: vec!["units unknown".into()],
        }
    }

    fn revision(id: &str, parent: Option<&str>, source_id: &str, grid: u32) -> CaseRevision {
        CaseRevision {
            case_revision_id: id.into(),
            parent_revision_id: parent.map(str::to_owned),
            created_utc_unix: 20,
            source_revision_ids: vec![source_id.into()],
            contract: json!({"physics": "incompressible_2d", "horizon": 4}),
            discretization: json!({"grid": grid}),
            outputs: json!({"quantity": "velocity"}),
        }
    }

    fn run(id: &str, parent: Option<&str>, revision_id: &str) -> RunRecord {
        RunRecord::new(
            id,
            parent.map(str::to_owned),
            revision_id,
            30,
            31,
            LifecycleState::Succeeded,
            RunManifest {
                schema_version: 1,
                app: VersionedComponent {
                    name: "Reyn Studio".into(),
                    version: "0.1.0".into(),
                    sha256: Some(digest('b')),
                },
                engine: None,
                model: Some(VersionedComponent {
                    name: "fixture.pth".into(),
                    version: "checkpoint".into(),
                    sha256: Some(digest('a')),
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
                missing_dependencies: vec![],
                output_sha256: vec![digest('c')],
                scalar_outputs: vec![ScalarOutput {
                    key: "global_relative_l2".into(),
                    value: 0.125,
                    units: "dimensionless".into(),
                    abs_tolerance: 1e-6,
                }],
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
        )
    }

    fn running_run(id: &str, parent: Option<&str>, revision_id: &str) -> RunRecord {
        let mut attempt = run(id, parent, revision_id);
        attempt.completed_utc_unix = 0;
        attempt.state = LifecycleState::Running;
        attempt.manifest.runtime_ms = 0;
        attempt.manifest.stop_reason = "running".into();
        attempt.manifest.output_sha256.clear();
        attempt.manifest.scalar_outputs.clear();
        attempt.manifest.determinism = None;
        attempt.calibrated_views.clear();
        attempt
    }

    fn project_with_run() -> ProjectManifest {
        let mut project = ProjectManifest::new("Fixture", 1);
        project.add_source_revision(source("source-1"), 10).unwrap();
        project
            .create_case(
                "case-1",
                "Painted IC",
                revision("case-rev-1", None, "source-1", 128),
                20,
            )
            .unwrap();
        project
            .append_run("case-1", run("run-1", None, "case-rev-1"), 31)
            .unwrap();
        project
    }

    #[test]
    fn round_trip_restores_immutable_run_views_and_evidence_links() {
        let mut project = project_with_run();
        project
            .append_evidence(
                EvidenceArtifact {
                    evidence_id: "evidence-1".into(),
                    run_ids: vec!["run-1".into()],
                    created_utc_unix: 32,
                    source_class: EvidenceSourceClass::Derived,
                    media_type: "application/json".into(),
                    byte_size: 128,
                    content_sha256: digest('d'),
                    derivation_method: Some("benchmark_report".into()),
                    derivation_version: Some("1".into()),
                    warnings: vec!["no independent reference".into()],
                    metadata: json!({"integrity_only": true}),
                    calibrated_views: vec![CalibratedView {
                        view_id: "evidence.pressure".into(),
                        quantity: "recovered pressure".into(),
                        units: "solver velocity² · density-normalized".into(),
                        scale_min: -1.5,
                        scale_max: 1.5,
                        source_class: EvidenceSourceClass::Recovered,
                        method: "advective_poisson_density_normalized_zero_mean".into(),
                    }],
                },
                32,
            )
            .unwrap();
        project
            .set_selection(
                ProjectSelection {
                    active_case_id: Some("case-1".into()),
                    selected_run_id: Some("run-1".into()),
                    selected_evidence_id: Some("evidence-1".into()),
                    selected_view_id: Some("evidence.pressure".into()),
                },
                33,
            )
            .unwrap();

        let restored = ProjectManifest::from_bytes(&project.to_bytes().unwrap()).unwrap();

        assert_eq!(restored.schema_version(), PROJECT_SCHEMA_VERSION);
        assert_eq!(restored.cases()[0].runs()[0].run_id(), "run-1");
        assert_eq!(
            restored.cases()[0].runs()[0].calibrated_views()[0].scale_max,
            2.0
        );
        assert_eq!(restored.evidence()[0].run_ids, vec!["run-1"]);
        assert_eq!(
            restored.evidence()[0].source_class,
            EvidenceSourceClass::Derived
        );
        assert_eq!(restored.evidence()[0].calibrated_views[0].scale_min, -1.5);
        assert_eq!(
            restored.evidence()[0].warnings,
            vec!["no independent reference"]
        );
        assert_eq!(restored.source_revisions()[0].content_sha256, digest('a'));
        assert_eq!(
            restored.cases()[0].runs()[0]
                .manifest()
                .model
                .as_ref()
                .and_then(|model| model.sha256.as_deref()),
            Some(digest('a').as_str())
        );
        assert_eq!(
            restored.selection().selected_view_id.as_deref(),
            Some("evidence.pressure")
        );
    }

    #[test]
    fn attempts_persist_at_start_finish_once_and_allow_immediate_retry() {
        let mut project = project_with_run();
        let running = running_run("run-2", Some("run-1"), "case-rev-1");
        let submitted_contract = running.manifest.exact_contract.clone();
        let submitted_settings = running.manifest.exact_settings.clone();
        project.append_run("case-1", running, 40).unwrap();

        let bytes = project.to_bytes().unwrap();
        let mut reopened = ProjectManifest::from_bytes(&bytes).unwrap();
        let persisted = reopened.run("run-2").unwrap();
        assert_eq!(persisted.state(), LifecycleState::Running);
        assert_eq!(persisted.manifest().exact_contract, submitted_contract);
        assert_eq!(persisted.manifest().exact_settings, submitted_settings);

        let mut changed = persisted.clone();
        changed.state = LifecycleState::Cancelled;
        changed.completed_utc_unix = 41;
        changed.manifest.stop_reason = "operator_cancelled".into();
        changed.manifest.exact_settings = json!({"changed": true});
        assert!(reopened
            .finish_run_attempt("case-1", changed, 41)
            .unwrap_err()
            .to_string()
            .contains("immutable submitted inputs"));
        assert_eq!(
            reopened.run("run-2").unwrap().state(),
            LifecycleState::Running
        );

        let mut cancelled = reopened.run("run-2").unwrap().clone();
        cancelled.state = LifecycleState::Cancelled;
        cancelled.completed_utc_unix = 42;
        cancelled.manifest.runtime_ms = 12;
        cancelled.manifest.stop_reason = "operator_cancelled".into();
        reopened
            .finish_run_attempt("case-1", cancelled, 42)
            .unwrap();

        let retry = running_run("run-3", Some("run-1"), "case-rev-1");
        reopened.append_run("case-1", retry, 43).unwrap();
        let mut failed = reopened.run("run-3").unwrap().clone();
        failed.state = LifecycleState::Failed;
        failed.completed_utc_unix = 44;
        failed.manifest.runtime_ms = 8;
        failed.manifest.stop_reason = "timeout: deterministic fixture".into();
        reopened.finish_run_attempt("case-1", failed, 44).unwrap();

        let saved = ProjectManifest::from_bytes(&reopened.to_bytes().unwrap()).unwrap();
        assert_eq!(
            saved.run("run-2").unwrap().state(),
            LifecycleState::Cancelled
        );
        assert_eq!(
            saved.run("run-2").unwrap().manifest().stop_reason,
            "operator_cancelled"
        );
        assert_eq!(saved.run("run-3").unwrap().state(), LifecycleState::Failed);
        assert!(saved
            .run("run-3")
            .unwrap()
            .manifest()
            .stop_reason
            .starts_with("timeout:"));
    }

    #[test]
    fn rerun_appends_new_id_and_parent_without_mutating_completed_run() {
        let mut project = project_with_run();
        let original = project.cases()[0].runs()[0].clone();
        project
            .append_run("case-1", run("run-2", Some("run-1"), "case-rev-1"), 40)
            .unwrap();

        let runs = project.cases()[0].runs();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[1].parent_run_id(), Some("run-1"));
        assert_eq!(runs[0], original);
        assert!(matches!(
            project.append_run("case-1", run("run-1", Some("run-1"), "case-rev-1"), 41),
            Err(ProjectError::DuplicateId { .. })
        ));
    }

    #[test]
    fn case_revision_stales_only_dependent_stages_and_preserves_runs() {
        let mut project = project_with_run();
        let original = project.cases()[0].runs()[0].clone();
        let stale = project
            .append_case_revision(
                "case-1",
                revision("case-rev-2", Some("case-rev-1"), "source-1", 256),
                50,
            )
            .unwrap();

        assert_eq!(stale, vec![DependencyStage::Run, DependencyStage::Evidence]);
        assert_eq!(project.cases()[0].runs()[0], original);
        assert_eq!(project.cases()[0].active_revision_id(), "case-rev-2");
    }

    #[test]
    fn source_transform_and_contract_changes_stale_only_downstream_objects() {
        let mut project = project_with_run();
        let original = project.cases()[0].runs()[0].clone();
        let mut changed_source = source("source-2");
        changed_source.revision = 2;
        changed_source.content_sha256 = digest('e');
        changed_source.parent_revision_id = Some("source-1".into());
        changed_source.transform_4x4[12] = 0.25;
        project.add_source_revision(changed_source, 40).unwrap();

        let source_stale = project
            .append_case_revision(
                "case-1",
                revision("case-rev-2", Some("case-rev-1"), "source-2", 128),
                41,
            )
            .unwrap();
        assert_eq!(
            source_stale,
            vec![
                DependencyStage::Contract,
                DependencyStage::Discretization,
                DependencyStage::Run,
                DependencyStage::Evidence,
            ]
        );
        assert_eq!(project.cases()[0].runs()[0], original);
        assert_eq!(
            project.cases()[0].stale_stages_for_run("run-1").unwrap(),
            source_stale
        );

        project
            .append_run("case-1", run("run-2", None, "case-rev-2"), 42)
            .unwrap();
        assert_eq!(
            project.cases()[0].stale_stages(),
            &[DependencyStage::Evidence]
        );

        let mut contract_change = revision("case-rev-3", Some("case-rev-2"), "source-2", 128);
        contract_change.contract["horizon"] = json!(8);
        let contract_stale = project
            .append_case_revision("case-1", contract_change, 43)
            .unwrap();
        assert_eq!(
            contract_stale,
            vec![
                DependencyStage::Discretization,
                DependencyStage::Run,
                DependencyStage::Evidence,
            ]
        );
        assert_eq!(project.cases()[0].runs()[0], original);
        assert_eq!(
            project.cases()[0].runs()[1].state(),
            LifecycleState::Succeeded
        );
    }

    #[test]
    fn deterministic_reruns_expose_within_tolerance_and_difference_without_mutation() {
        let mut project = project_with_run();
        let original = project.cases()[0].runs()[0].clone();

        let mut within = run("run-2", Some("run-1"), "case-rev-1");
        within.manifest.scalar_outputs[0].value += 0.5e-6;
        within.manifest.compare_scalars_against(&original);
        project.append_run("case-1", within, 40).unwrap();
        assert_eq!(
            project.cases()[0].runs()[1]
                .manifest()
                .determinism
                .as_ref()
                .unwrap()
                .status,
            DeterminismStatus::WithinTolerance
        );

        let parent = project.cases()[0].runs()[1].clone();
        let mut different = run("run-3", Some("run-2"), "case-rev-1");
        different.manifest.scalar_outputs[0].value = 0.2;
        different.manifest.compare_scalars_against(&parent);
        project.append_run("case-1", different, 41).unwrap();
        let difference = project.cases()[0].runs()[2]
            .manifest()
            .determinism
            .as_ref()
            .unwrap();
        assert_eq!(difference.status, DeterminismStatus::Difference);
        assert!(difference.differences[0].abs_difference.unwrap() > 1e-6);
        assert_eq!(project.cases()[0].runs()[0], original);
    }

    #[test]
    fn schema_v1_migration_preserves_run_and_evidence_bytes_semantics() {
        let legacy_source = source("source-1");
        let legacy_revision = revision("case-rev-1", None, "source-1", 128);
        let legacy_view = CalibratedView {
            view_id: "legacy.velocity".into(),
            quantity: "velocity magnitude".into(),
            units: "normalized velocity".into(),
            scale_min: 0.0,
            scale_max: 2.0,
            source_class: EvidenceSourceClass::ModelPrediction,
            method: "direct_flow_map".into(),
        };
        let legacy_manifest = ProjectManifestV1 {
            schema_version: 1,
            project_id: "legacy-project".into(),
            name: "Legacy evidence".into(),
            created_utc_unix: 1,
            modified_utc_unix: 6,
            source_revisions: vec![legacy_source],
            cases: vec![CaseRecordV1 {
                case_id: "case-1".into(),
                name: "Legacy case".into(),
                revisions: vec![legacy_revision],
                active_revision_id: "case-rev-1".into(),
                runs: vec![RunRecordV1 {
                    run_id: "run-1".into(),
                    parent_run_id: None,
                    case_revision_id: "case-rev-1".into(),
                    created_utc_unix: 4,
                    completed_utc_unix: 5,
                    state: LifecycleState::Succeeded,
                    manifest: RunManifestV1 {
                        schema_version: 1,
                        app: VersionedComponent {
                            name: "Reyn Studio".into(),
                            version: "0.1.0".into(),
                            sha256: None,
                        },
                        engine: None,
                        model: Some(VersionedComponent {
                            name: "legacy.pth".into(),
                            version: "checkpoint".into(),
                            sha256: Some(digest('a')),
                        }),
                        solver: None,
                        converter: None,
                        exact_contract: json!({"physics": "incompressible_2d"}),
                        exact_settings: json!({"horizon": 4}),
                        seeds: vec![70000],
                        device: "cpu".into(),
                        runtime_ms: 12,
                        stop_reason: "complete".into(),
                        warnings: vec!["legacy warning".into()],
                        waivers: vec![],
                        missing_dependencies: vec!["legacy model unavailable".into()],
                        output_sha256: vec![digest('c')],
                    },
                    calibrated_views: vec![legacy_view],
                }],
            }],
            evidence: vec![EvidenceArtifactV1 {
                evidence_id: "evidence-1".into(),
                run_ids: vec!["run-1".into()],
                created_utc_unix: 6,
                source_class: EvidenceSourceClass::Derived,
                media_type: "application/json".into(),
                byte_size: 42,
                content_sha256: digest('d'),
                derivation_method: Some("legacy_report".into()),
                derivation_version: Some("1".into()),
                warnings: vec!["evidence warning".into()],
                metadata: json!({"kept": ["exact", "metadata"]}),
            }],
            events: vec![],
        };
        let canonical = serde_json::to_vec(&legacy_manifest).unwrap();
        let envelope = ProjectEnvelopeV1 {
            schema_version: 1,
            integrity_sha256: sha256_hex(&canonical),
            manifest: legacy_manifest,
        };
        let bytes = serde_json::to_vec_pretty(&envelope).unwrap();

        let (migrated, from) = ProjectManifest::decode(&bytes).unwrap();

        assert_eq!(from, Some(1));
        assert_eq!(migrated.schema_version(), PROJECT_SCHEMA_VERSION);
        assert_eq!(migrated.cases()[0].runs()[0].manifest().schema_version, 1);
        assert_eq!(
            migrated.cases()[0].runs()[0].manifest().warnings,
            vec!["legacy warning"]
        );
        assert_eq!(
            migrated.evidence()[0].metadata,
            json!({"kept": ["exact", "metadata"]})
        );
        assert_eq!(migrated.evidence()[0].warnings, vec!["evidence warning"]);
        assert_eq!(migrated.evidence()[0].content_sha256, digest('d'));
        assert!(migrated.evidence()[0].calibrated_views.is_empty());
        assert!(migrated.selection().selected_run_id.is_none());
        assert_eq!(
            ProjectManifest::from_bytes(&migrated.to_bytes().unwrap())
                .unwrap()
                .evidence(),
            migrated.evidence()
        );
    }

    #[test]
    fn schema_v2_migration_preserves_legacy_complete_runs_and_bundle_integrity() {
        let mut envelope: ProjectEnvelope =
            serde_json::from_slice(&project_with_run().to_bytes().unwrap()).unwrap();
        envelope.schema_version = 2;
        envelope.manifest.schema_version = 2;
        let canonical = serde_json::to_string(&envelope.manifest)
            .unwrap()
            .replace("\"state\":\"succeeded\"", "\"state\":\"complete\"");
        let manifest_digest = sha256_hex(canonical.as_bytes());
        envelope.integrity_sha256 = manifest_digest.clone();
        envelope.bundle_integrity_sha256 =
            Some(bundle_integrity_for_schema(2, &manifest_digest, &[]).unwrap());
        let mut value = serde_json::to_value(envelope).unwrap();
        value["manifest"]["cases"][0]["runs"][0]["state"] = json!("complete");

        let (migrated, from) =
            ProjectManifest::decode(&serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        assert_eq!(from, Some(2));
        assert_eq!(migrated.schema_version(), PROJECT_SCHEMA_VERSION);
        assert_eq!(
            migrated.cases()[0].runs()[0].state(),
            LifecycleState::Succeeded
        );
        assert_eq!(
            ProjectManifest::from_bytes(&migrated.to_bytes().unwrap())
                .unwrap()
                .cases()[0]
                .runs()[0]
                .state(),
            LifecycleState::Succeeded
        );
    }

    #[test]
    fn atomic_save_reopens_without_engine_and_tampering_is_rejected() {
        let project = project_with_run();
        let root = std::env::temp_dir().join(format!(
            "reyn-project-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let path = root.join("fixture.reynproj");
        project.save_atomic(&path).unwrap();
        let restored = ProjectManifest::open(&path).unwrap();
        assert_eq!(restored.name(), "Fixture");

        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["manifest"]["name"] = json!("Tampered");
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        assert!(matches!(
            ProjectManifest::open(&path),
            Err(ProjectError::IntegrityMismatch)
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_atomic_save_preserves_the_last_valid_document() {
        let root = std::env::temp_dir().join(format!(
            "reyn-project-atomic-failure-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let path = root.join("fixture.reynproj");
        let project = project_with_run();
        project.save_atomic(&path).unwrap();
        let valid_bytes = std::fs::read(&path).unwrap();

        let mut invalid = project;
        invalid.cases[0].runs[0].calibrated_views[0].scale_max = f64::NAN;
        assert!(invalid.save_atomic(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), valid_bytes);
        assert_eq!(ProjectManifest::open(&path).unwrap().name(), "Fixture");
        assert!(!std::fs::read_dir(&root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn interruption_after_temp_sync_preserves_last_valid_document() {
        let root = std::env::temp_dir().join(format!(
            "reyn-project-interrupted-write-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let path = root.join("fixture.reynproj");
        let project = project_with_run();
        project.save_atomic(&path).unwrap();
        let valid_bytes = std::fs::read(&path).unwrap();

        let mut replacement = project;
        replacement.rename("Interrupted replacement", 80);
        let replacement_bytes = replacement.to_bytes().unwrap();
        assert!(write_atomic_bytes_interrupted_after_sync(&path, &replacement_bytes).is_err());

        assert_eq!(std::fs::read(&path).unwrap(), valid_bytes);
        assert_eq!(ProjectManifest::open(&path).unwrap().name(), "Fixture");
        assert!(!std::fs::read_dir(&root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn future_schema_and_unknown_evidence_fields_never_silently_drop() {
        let mut value: serde_json::Value =
            serde_json::from_slice(&project_with_run().to_bytes().unwrap()).unwrap();
        value["schema_version"] = json!(99);
        assert!(matches!(
            ProjectManifest::from_bytes(&serde_json::to_vec(&value).unwrap()),
            Err(ProjectError::UnsupportedSchema(99))
        ));

        let mut project = project_with_run();
        project
            .append_evidence(
                EvidenceArtifact {
                    evidence_id: "evidence-1".into(),
                    run_ids: vec!["run-1".into()],
                    created_utc_unix: 40,
                    source_class: EvidenceSourceClass::Integrity,
                    media_type: "application/json".into(),
                    byte_size: 10,
                    content_sha256: digest('d'),
                    derivation_method: None,
                    derivation_version: None,
                    warnings: vec![],
                    metadata: json!({}),
                    calibrated_views: vec![],
                },
                40,
            )
            .unwrap();
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&project.to_bytes().unwrap()).unwrap();
        envelope["manifest"]["evidence"][0]["unversioned_field"] = json!("must not vanish");
        let canonical = serde_json::to_vec(&envelope["manifest"]).unwrap();
        envelope["integrity_sha256"] = json!(sha256_hex(&canonical));
        assert!(matches!(
            ProjectManifest::from_bytes(&serde_json::to_vec(&envelope).unwrap()),
            Err(ProjectError::Json(_))
        ));
    }

    fn portable_document() -> (ProjectDocument, Vec<u8>, Vec<u8>) {
        let source_bytes = b"portable model checkpoint bytes".to_vec();
        let artifact_value = json!({"stored": "field evidence", "scale": [0.0, 2.0]});
        let artifact_bytes = serde_json::to_vec(&artifact_value).unwrap();
        let source_digest = sha256_hex(&source_bytes);
        let artifact_digest = sha256_hex(&artifact_bytes);
        let mut project = project_with_run();
        project.source_revisions[0].content_sha256 = source_digest.clone();
        project.source_revisions[0].byte_size = source_bytes.len() as u64;
        project.source_revisions[0].uri_hint =
            Some("/missing-machine/checkpoints/model.pth".into());
        project.cases[0].runs[0]
            .manifest
            .model
            .as_mut()
            .unwrap()
            .sha256 = Some(source_digest.clone());
        project.cases[0].runs[0].manifest.output_sha256 = vec![artifact_digest.clone()];
        project
            .append_evidence(
                EvidenceArtifact {
                    evidence_id: "portable-evidence".into(),
                    run_ids: vec!["run-1".into()],
                    created_utc_unix: 32,
                    source_class: EvidenceSourceClass::Derived,
                    media_type: "application/json".into(),
                    byte_size: artifact_bytes.len() as u64,
                    content_sha256: artifact_digest,
                    derivation_method: Some("fixture".into()),
                    derivation_version: Some("1".into()),
                    warnings: vec![],
                    metadata: json!({"snapshot": artifact_value}),
                    calibrated_views: vec![],
                },
                32,
            )
            .unwrap();
        let mut document = ProjectDocument::new(project);
        document.add_content(source_bytes.clone(), "application/x-pytorch");
        document.add_content(artifact_bytes.clone(), "application/json");
        (document, source_bytes, artifact_bytes)
    }

    #[test]
    fn project_serialization_is_deterministic_across_content_insertion_order() {
        let (document, source_bytes, artifact_bytes) = portable_document();
        let expected = document.to_bytes().unwrap();
        assert_eq!(document.to_bytes().unwrap(), expected);

        let mut reverse = ProjectDocument::new(document.manifest().clone());
        reverse.add_content(artifact_bytes, "application/json");
        reverse.add_content(source_bytes, "application/x-pytorch");
        assert_eq!(reverse.to_bytes().unwrap(), expected);

        let reopened = ProjectDocument::from_bytes(&expected).unwrap();
        assert_eq!(reopened.to_bytes().unwrap(), expected);
    }

    #[test]
    fn portable_bundle_reopens_after_project_directory_moves_without_path_dependency() {
        let (document, source_bytes, artifact_bytes) = portable_document();
        let root = std::env::temp_dir().join(format!(
            "reyn-portable-move-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let original_directory = root.join("original");
        let moved_directory = root.join("moved");
        std::fs::create_dir_all(&original_directory).unwrap();
        let original_path = original_directory.join("study.reynproj");
        document.save_atomic(&original_path).unwrap();
        std::fs::rename(&original_directory, &moved_directory).unwrap();

        let reopened = ProjectDocument::open(&moved_directory.join("study.reynproj")).unwrap();
        let source_digest = sha256_hex(&source_bytes);
        let artifact_digest = sha256_hex(&artifact_bytes);
        assert_eq!(
            reopened.content_bytes(&source_digest),
            Some(source_bytes.as_slice())
        );
        assert_eq!(
            reopened.content_bytes(&artifact_digest),
            Some(artifact_bytes.as_slice())
        );
        assert!(reopened.diagnostics().is_empty());
        assert_eq!(
            reopened.manifest().source_revisions()[0]
                .uri_hint
                .as_deref(),
            Some("/missing-machine/checkpoints/model.pth")
        );
        assert_eq!(reopened.manifest().run_count(), 1);
        assert_eq!(reopened.manifest().evidence().len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_content_hash_opens_with_precise_diagnostic_and_preserved_evidence() {
        let (document, _, artifact_bytes) = portable_document();
        let mut envelope: ProjectEnvelope =
            serde_json::from_slice(&document.to_bytes().unwrap()).unwrap();
        let artifact_digest = sha256_hex(&artifact_bytes);
        let wire = envelope
            .bundled_content
            .iter_mut()
            .find(|wire| wire.content_sha256 == artifact_digest)
            .unwrap();
        let replacement = if wire.data_hex.starts_with('0') {
            "1"
        } else {
            "0"
        };
        wire.data_hex.replace_range(..1, replacement);
        let reopened =
            ProjectDocument::from_bytes(&serde_json::to_vec(&envelope).unwrap()).unwrap();

        assert_eq!(
            reopened.content_state(&artifact_digest),
            ContentState::Corrupt
        );
        assert!(reopened.diagnostics().iter().any(|diagnostic| {
            diagnostic.kind == ContentDiagnosticKind::Corrupt
                && diagnostic.content_sha256.as_deref() == Some(artifact_digest.as_str())
                && diagnostic
                    .references
                    .iter()
                    .any(|reference| reference.object_id == "portable-evidence")
        }));
        assert_eq!(reopened.manifest().evidence().len(), 1);
        assert_eq!(reopened.manifest().run_count(), 1);
    }

    #[test]
    fn duplicate_bundle_objects_are_deduplicated_deterministically() {
        let (document, source_bytes, _) = portable_document();
        let mut envelope: ProjectEnvelope =
            serde_json::from_slice(&document.to_bytes().unwrap()).unwrap();
        let duplicate = envelope.bundled_content[0].clone();
        envelope.bundled_content.push(duplicate);
        envelope.bundle_integrity_sha256 =
            Some(bundle_integrity(&envelope.integrity_sha256, &envelope.bundled_content).unwrap());
        let reopened =
            ProjectDocument::from_bytes(&serde_json::to_vec(&envelope).unwrap()).unwrap();

        assert!(reopened
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.kind == ContentDiagnosticKind::Duplicate));
        assert_eq!(
            reopened.content_bytes(&sha256_hex(&source_bytes)),
            Some(source_bytes.as_slice())
        );
        let normalized: ProjectEnvelope =
            serde_json::from_slice(&reopened.to_bytes().unwrap()).unwrap();
        let unique: BTreeSet<_> = normalized
            .bundled_content
            .iter()
            .map(|wire| &wire.content_sha256)
            .collect();
        assert_eq!(unique.len(), normalized.bundled_content.len());
    }

    #[test]
    fn relinking_missing_content_never_mutates_immutable_runs() {
        let (document, source_bytes, _) = portable_document();
        let manifest = document.manifest().clone();
        let original_run = manifest.cases()[0].runs()[0].clone();
        let source_digest = sha256_hex(&source_bytes);
        let mut missing = ProjectDocument::new(manifest);
        assert_eq!(missing.content_state(&source_digest), ContentState::Missing);
        let root = std::env::temp_dir().join(format!(
            "reyn-relink-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let candidate = root.join("checkpoint.pth");
        std::fs::write(&candidate, &source_bytes).unwrap();

        missing
            .relink_content(&source_digest, &candidate, "application/x-pytorch")
            .unwrap();

        assert_eq!(
            missing.content_state(&source_digest),
            ContentState::Available
        );
        assert_eq!(missing.manifest().cases()[0].runs()[0], original_run);
        assert!(
            missing
                .diagnostics()
                .iter()
                .all(|diagnostic| diagnostic.content_sha256.as_deref()
                    != Some(source_digest.as_str()))
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn relink_rejects_wrong_hash_without_changing_bundle() {
        let (document, source_bytes, _) = portable_document();
        let source_digest = sha256_hex(&source_bytes);
        let mut missing = ProjectDocument::new(document.manifest().clone());
        let root = std::env::temp_dir().join(format!(
            "reyn-relink-wrong-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let candidate = root.join("wrong.pth");
        std::fs::write(&candidate, b"different bytes").unwrap();

        assert!(matches!(
            missing.relink_content(&source_digest, &candidate, "application/x-pytorch"),
            Err(ProjectError::ContentHashMismatch { .. })
        ));
        assert_eq!(missing.content_state(&source_digest), ContentState::Missing);
        let _ = std::fs::remove_dir_all(root);
    }

    fn signed_project_document() -> (
        ProjectDocument,
        String,
        String,
        crate::signing::PublicKeyRecord,
    ) {
        const RUN_ID: &str = "61f596e7-8414-488e-b764-0a1dfe671d1a";
        let mut project = project_with_run();
        project.cases[0].runs[0].run_id = RUN_ID.into();
        let canonical_report = b"{\"fixture\":\"immutable canonical report\"}\n".to_vec();
        let canonical_report_sha256 = sha256_hex(&canonical_report);
        let canonical_payload_sha256 = sha256_hex(b"canonical payload fixture");
        let provider = crate::signing::DeterministicTestProvider::new("portable-project");
        let key = provider.public_key_record();
        let signature = crate::signing::sign_canonical_payload(
            &provider,
            &key,
            false,
            &crate::signing::SigningLineage {
                run_id: RUN_ID.into(),
                report_schema: "reyn.benchmark-report-card.v1".into(),
                canonical_report_sha256: canonical_report_sha256.clone(),
                canonical_payload_sha256: canonical_payload_sha256.clone(),
                created_utc_unix: 42,
            },
        )
        .unwrap();
        let signature_json = signature.to_json().unwrap();
        let signature_sha256 = sha256_hex(signature_json.as_bytes());
        let report_evidence_id = format!("benchmark-report-{canonical_report_sha256}");
        project
            .append_evidence(
                EvidenceArtifact {
                    evidence_id: report_evidence_id.clone(),
                    run_ids: vec![RUN_ID.into()],
                    created_utc_unix: 42,
                    source_class: EvidenceSourceClass::Derived,
                    media_type: "application/vnd.reyn.benchmark-report+json".into(),
                    byte_size: canonical_report.len() as u64,
                    content_sha256: canonical_report_sha256.clone(),
                    derivation_method: Some("canonical_benchmark_report".into()),
                    derivation_version: Some("1".into()),
                    warnings: vec![],
                    metadata: json!({
                        "kind": "canonical_benchmark_report",
                        "canonical_payload_sha256": canonical_payload_sha256.clone(),
                    }),
                    calibrated_views: vec![],
                },
                42,
            )
            .unwrap();
        project
            .append_evidence(
                EvidenceArtifact {
                    evidence_id: format!(
                        "benchmark-signature-{}-{}",
                        key.key_fingerprint_sha256, canonical_payload_sha256
                    ),
                    run_ids: vec![RUN_ID.into()],
                    created_utc_unix: 42,
                    source_class: EvidenceSourceClass::AuthenticitySignature,
                    media_type: "application/vnd.reyn.evidence-signature+json".into(),
                    byte_size: signature_json.len() as u64,
                    content_sha256: signature_sha256.clone(),
                    derivation_method: Some("ed25519_canonical_payload_signature".into()),
                    derivation_version: Some("1".into()),
                    warnings: vec![],
                    metadata: json!({
                        "kind": "benchmark_signature",
                        "signature_schema": crate::signing::SIGNATURE_SCHEMA,
                        "parent_evidence_id": report_evidence_id,
                        "canonical_report_sha256": canonical_report_sha256,
                        "canonical_payload_sha256": canonical_payload_sha256.clone(),
                        "algorithm": crate::signing::SIGNATURE_ALGORITHM,
                        "key_id": key.key_id.clone(),
                        "key_fingerprint_sha256": key.key_fingerprint_sha256.clone(),
                        "verification_at_creation": "valid",
                    }),
                    calibrated_views: vec![],
                },
                42,
            )
            .unwrap();
        let mut document = ProjectDocument::new(project);
        document.add_content(
            canonical_report,
            "application/vnd.reyn.benchmark-report+json",
        );
        document.add_content(
            signature_json.as_bytes().to_vec(),
            "application/vnd.reyn.evidence-signature+json",
        );
        (document, signature_sha256, signature_json, key)
    }

    #[test]
    fn duplicate_signing_is_rejected_without_mutating_prior_evidence() {
        let (document, _, _, _) = signed_project_document();
        let mut manifest = document.manifest().clone();
        let original = manifest.evidence().to_vec();
        let mut duplicate = original.last().unwrap().clone();
        duplicate.evidence_id.push_str("-duplicate");
        assert!(matches!(
            manifest.append_evidence(duplicate, 43),
            Err(ProjectError::DuplicateSignature { .. })
        ));
        assert_eq!(manifest.evidence(), original);
    }

    #[test]
    fn engineering_result_signature_appends_and_rejects_duplicates() {
        const RUN_ID: &str = "61f596e7-8414-488e-b764-0a1dfe671d1a";
        let mut project = project_with_run();
        project.cases[0].runs[0].run_id = RUN_ID.into();
        let result_bytes = br#"{"schema":"engineering_result.v1","fixture":true}"#.to_vec();
        let result_sha256 = sha256_hex(&result_bytes);
        let parent_id = format!("evidence-eng-{result_sha256}");
        project
            .append_evidence(
                EvidenceArtifact {
                    evidence_id: parent_id.clone(),
                    run_ids: vec![RUN_ID.into()],
                    created_utc_unix: 40,
                    source_class: EvidenceSourceClass::Derived,
                    media_type: "application/vnd.reyn.engineering-result+json".into(),
                    byte_size: result_bytes.len() as u64,
                    content_sha256: result_sha256.clone(),
                    derivation_method: Some(crate::engineering::SURFACE_LOAD_METHOD.into()),
                    derivation_version: Some("1".into()),
                    warnings: vec![],
                    metadata: json!({"schema": "engineering_result.v1", "fixture": true}),
                    calibrated_views: vec![],
                },
                40,
            )
            .unwrap();
        let provider = crate::signing::DeterministicTestProvider::new("eng-project");
        let key = provider.public_key_record();
        let signature = crate::engineering_export::sign_engineering_result_digest(
            &provider,
            &key,
            false,
            RUN_ID,
            &result_sha256,
            42,
        )
        .unwrap();
        let signature_json = signature.to_json().unwrap();
        let signature_sha256 = sha256_hex(signature_json.as_bytes());
        let signature_evidence = EvidenceArtifact {
            evidence_id: format!(
                "engineering-signature-{}-{}",
                key.key_fingerprint_sha256, result_sha256
            ),
            run_ids: vec![RUN_ID.into()],
            created_utc_unix: 42,
            source_class: EvidenceSourceClass::AuthenticitySignature,
            media_type: "application/vnd.reyn.evidence-signature+json".into(),
            byte_size: signature_json.len() as u64,
            content_sha256: signature_sha256,
            derivation_method: Some("ed25519_canonical_payload_signature".into()),
            derivation_version: Some("1".into()),
            warnings: vec![],
            metadata: json!({
                "kind": "engineering_signature",
                "signature_schema": crate::signing::SIGNATURE_SCHEMA,
                "parent_evidence_id": parent_id,
                "canonical_report_sha256": result_sha256,
                "canonical_payload_sha256": result_sha256,
                "algorithm": crate::signing::SIGNATURE_ALGORITHM,
                "key_id": key.key_id,
                "key_fingerprint_sha256": key.key_fingerprint_sha256,
                "verification_at_creation": "valid",
            }),
            calibrated_views: vec![],
        };
        project
            .append_evidence(signature_evidence.clone(), 42)
            .unwrap();
        let mut duplicate = signature_evidence;
        duplicate.evidence_id.push_str("-dup");
        assert!(matches!(
            project.append_evidence(duplicate, 43),
            Err(ProjectError::DuplicateSignature { .. })
        ));
    }

    #[test]
    fn signed_sidecar_moves_and_reopens_without_private_key_or_path_dependency() {
        use base64::Engine as _;
        use sha2::Digest as _;

        let (document, signature_sha256, signature_json, key) = signed_project_document();
        let root = std::env::temp_dir().join(format!(
            "reyn-signed-portable-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let original_directory = root.join("original");
        let moved_directory = root.join("moved");
        std::fs::create_dir_all(&original_directory).unwrap();
        let original_path = original_directory.join("signed.reynproj");
        document.save_atomic(&original_path).unwrap();
        std::fs::rename(&original_directory, &moved_directory).unwrap();

        let reopened = ProjectDocument::open(&moved_directory.join("signed.reynproj")).unwrap();
        let sidecar_bytes = reopened.content_bytes(&signature_sha256).unwrap();
        assert_eq!(sidecar_bytes, signature_json.as_bytes());
        let sidecar = crate::signing::SignedEvidenceArtifact::from_json(
            std::str::from_utf8(sidecar_bytes).unwrap(),
        )
        .unwrap();
        let policy = crate::signing::VerificationPolicy::new(
            [key.key_fingerprint_sha256.clone()],
            std::iter::empty::<String>(),
        );
        assert_eq!(
            crate::signing::verify_signed_hash(
                &sidecar.source.canonical_payload_sha256,
                &sidecar.source.canonical_report_sha256,
                &sidecar,
                &policy,
            )
            .status,
            crate::signing::VerificationStatus::VerifiedTrustedKey
        );

        let serialized = reopened.to_bytes().unwrap();
        let test_seed: [u8; 32] =
            Sha256::digest(b"reyn.test-only.ed25519.v1\0portable-project").into();
        let secret_hex = hex_encode(&test_seed);
        let secret_base64 = base64::engine::general_purpose::STANDARD.encode(test_seed);
        assert!(!serialized
            .windows(test_seed.len())
            .any(|window| window == test_seed));
        assert!(!String::from_utf8_lossy(&serialized).contains(&secret_hex));
        assert!(!String::from_utf8_lossy(&serialized).contains(&secret_base64));
        assert_eq!(
            sidecar.authenticity.public_key_base64,
            key.public_key_base64
        );
        assert!(String::from_utf8_lossy(&serialized)
            .contains(&hex_encode(key.public_key_base64.as_bytes())));
        let _ = std::fs::remove_dir_all(root);
    }
}
