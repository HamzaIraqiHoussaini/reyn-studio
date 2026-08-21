//! N6 persistent desktop settings. Runtime choices are explicit and local;
//! telemetry remains opt-in and is never enabled by a default migration.
//!
//! Every stored value keeps a serde default so settings files written by any
//! older build load cleanly. Display units, formatting, and viewport
//! preferences never change stored evidence — run manifests and versioned
//! exports remain SI regardless of these preferences.
use crate::engine::{
    EngineConfig, DEFAULT_2D_MODEL_ID, DEFAULT_3D_MODEL_ID, MODEL_BUNDLE_EXTENSION,
    TRUSTED_MODEL_CONVERSION_GUIDANCE,
};
use crate::engineering::OperatingPoint;
use crate::engineering_section::{SectionAxis, SectionQuantity};
use crate::field2d::FieldColormap;
use crate::signing::{PublicKeyRecord, SIGNATURE_ALGORITHM};
use crate::theme::*;
use crate::units::{self, InputUnitPrefs, NumberNotation, UnitSystem, ValueFormat};
use crate::viewport::{NavScheme, StandardView};
use egui::{Align, CornerRadius, Frame, Layout, Margin, RichText, Stroke};
use egui_phosphor::regular as ph;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ComputeDevice {
    Auto,
    Mps,
    Cpu,
}

impl ComputeDevice {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Mps, Self::Cpu];
    const WINDOWS: [Self; 2] = [Self::Auto, Self::Cpu];

    pub fn available() -> &'static [Self] {
        if cfg!(target_os = "macos") {
            &Self::ALL
        } else {
            &Self::WINDOWS
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Automatic",
            Self::Mps => "Apple Metal (MPS)",
            Self::Cpu => "CPU",
        }
    }

    pub fn engine_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Mps => "mps",
            Self::Cpu => "cpu",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    Instrument,
    HighContrast,
}

impl ThemeMode {
    pub const ALL: [Self; 2] = [Self::Instrument, Self::HighContrast];

    pub fn label(self) -> &'static str {
        match self {
            Self::Instrument => "Instrument Dark",
            Self::HighContrast => "Instrument High Contrast",
        }
    }
}

/// Colormap range behavior for signed section views (Cp).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CpRangeMode {
    /// Symmetric range from the section's own extrema.
    #[default]
    Auto,
    /// Symmetric range pinned to ±extent so sections compare across runs.
    Pinned,
}

impl CpRangeMode {
    pub const ALL: [Self; 2] = [Self::Auto, Self::Pinned];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto (per section)",
            Self::Pinned => "Pinned symmetric range",
        }
    }
}

/// Theme-sanctioned surfaces for the render viewport well.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ViewportBackground {
    /// The darkest calibrated well (`BG_VIEWPORT`) — data glows here.
    #[default]
    InstrumentWell,
    /// The app canvas (`BG_0`) — slightly lifted, matches document screens.
    Canvas,
}

impl ViewportBackground {
    pub const ALL: [Self; 2] = [Self::InstrumentWell, Self::Canvas];

    pub fn label(self) -> &'static str {
        match self {
            Self::InstrumentWell => "Instrument well (darkest)",
            Self::Canvas => "App canvas",
        }
    }

    pub fn color(self) -> egui::Color32 {
        match self {
            Self::InstrumentWell => BG_VIEWPORT,
            Self::Canvas => BG_0,
        }
    }
}

/// A named, user-savable operating point (fluid state + speed). Applying a
/// preset only fills the case's draft fields — every gate still runs.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct OperatingPointPreset {
    pub name: String,
    pub velocity_mps: f64,
    pub density_kg_m3: f64,
    pub viscosity_pa_s: f64,
    pub reference_pressure_pa: f64,
}

pub const CASE_TEMPLATE_SCHEMA_VERSION: u32 = 1;
pub const CASE_TEMPLATE_EXTENSION: &str = "reyntemplate";

/// Portable defaults for a new or existing external-flow draft.
///
/// The format deliberately excludes geometry, source/model identity, transforms,
/// waivers, runs, and evidence. Applying one still goes through the ordinary
/// readiness and staleness paths before a new immutable run can be created.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CaseTemplate {
    pub schema_version: u32,
    pub name: String,
    pub operating: CaseTemplateOperatingDefaults,
    pub preferred_view: CaseTemplateViewDefaults,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CaseTemplateOperatingDefaults {
    /// Canonical SI, regardless of the display/input units used while saving.
    pub velocity_mps: f64,
    pub density_kg_m3: f64,
    pub viscosity_pa_s: f64,
    pub reference_pressure_pa: f64,
    pub horizon_steps: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CaseTemplateViewDefaults {
    pub section_axis: SectionAxis,
    pub section_quantity: SectionQuantity,
}

impl CaseTemplate {
    pub fn from_draft(
        name: impl Into<String>,
        operating: &OperatingPoint,
        section_axis: SectionAxis,
        section_quantity: SectionQuantity,
    ) -> Self {
        Self {
            schema_version: CASE_TEMPLATE_SCHEMA_VERSION,
            name: name.into().trim().to_owned(),
            operating: CaseTemplateOperatingDefaults {
                velocity_mps: operating.velocity,
                density_kg_m3: operating.density,
                viscosity_pa_s: operating.viscosity,
                reference_pressure_pa: operating.reference_pressure,
                horizon_steps: operating.horizon_steps,
            },
            preferred_view: CaseTemplateViewDefaults {
                section_axis,
                section_quantity,
            },
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != CASE_TEMPLATE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported case-template schema {}; this build supports schema {}",
                self.schema_version, CASE_TEMPLATE_SCHEMA_VERSION
            ));
        }
        if self.name.trim().is_empty() {
            return Err("case-template name cannot be empty".into());
        }
        if !self.operating.velocity_mps.is_finite() || self.operating.velocity_mps <= 0.0 {
            return Err("case-template free-stream speed must be positive".into());
        }
        if !self.operating.density_kg_m3.is_finite() || self.operating.density_kg_m3 <= 0.0 {
            return Err("case-template density must be positive".into());
        }
        if !self.operating.viscosity_pa_s.is_finite() || self.operating.viscosity_pa_s <= 0.0 {
            return Err("case-template dynamic viscosity must be positive".into());
        }
        if !self.operating.reference_pressure_pa.is_finite() {
            return Err("case-template reference pressure must be finite".into());
        }
        if !(1..=256).contains(&self.operating.horizon_steps) {
            return Err("case-template horizon must be between 1 and 256 steps".into());
        }
        Ok(())
    }

    /// Seed only template-owned draft fields. Geometry-derived reference
    /// length/units and the fixed +X flow direction remain untouched.
    pub fn apply_to(
        &self,
        operating: &mut OperatingPoint,
        model_max_steps: u32,
    ) -> Result<bool, String> {
        self.validate()?;
        if model_max_steps == 0 {
            return Err("the selected model does not declare a supported horizon".into());
        }
        let horizon_steps = self.operating.horizon_steps.min(model_max_steps);
        let changed = operating.velocity != self.operating.velocity_mps
            || operating.density != self.operating.density_kg_m3
            || operating.viscosity != self.operating.viscosity_pa_s
            || operating.reference_pressure != self.operating.reference_pressure_pa
            || operating.horizon_steps != horizon_steps;
        operating.velocity = self.operating.velocity_mps;
        operating.density = self.operating.density_kg_m3;
        operating.viscosity = self.operating.viscosity_pa_s;
        operating.reference_pressure = self.operating.reference_pressure_pa;
        operating.horizon_steps = horizon_steps;
        Ok(changed)
    }

    fn summary(&self) -> String {
        format!(
            "{} m/s · {} kg/m³ · {:.3e} Pa·s · H{} · {} / {}",
            self.operating.velocity_mps,
            self.operating.density_kg_m3,
            self.operating.viscosity_pa_s,
            self.operating.horizon_steps,
            self.preferred_view.section_quantity.label(),
            self.preferred_view.section_axis.label(),
        )
    }
}

/// Built-in reference fluids at standard conditions. These are textbook
/// values, clearly named; they fill inputs and never bypass validation.
pub fn built_in_presets() -> Vec<OperatingPointPreset> {
    vec![
        OperatingPointPreset {
            name: "Wind tunnel · air 15 °C · 30 m/s".into(),
            velocity_mps: 30.0,
            density_kg_m3: 1.225,
            viscosity_pa_s: 1.81e-5,
            reference_pressure_pa: 101_325.0,
        },
        OperatingPointPreset {
            name: "Low-speed air · 15 °C · 5 m/s".into(),
            velocity_mps: 5.0,
            density_kg_m3: 1.225,
            viscosity_pa_s: 1.81e-5,
            reference_pressure_pa: 101_325.0,
        },
        OperatingPointPreset {
            name: "Water · 20 °C · 1 m/s".into(),
            velocity_mps: 1.0,
            density_kg_m3: 998.2,
            viscosity_pa_s: 1.002e-3,
            reference_pressure_pa: 101_325.0,
        },
    ]
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct AppSettings {
    pub compute_device: ComputeDevice,
    pub python_path: String,
    pub research_dir: String,
    pub project_directory: String,
    pub autosave_interval_seconds: u32,
    pub automatic_update_checks: bool,
    pub theme: ThemeMode,
    /// Replace interface transitions with instant state changes (§3.7).
    pub reduced_motion: bool,
    pub telemetry: bool,
    /// Permanently exposes experimental prediction tools. The default product
    /// path remains the case-centered engineering workflow.
    pub developer_research_sandbox: bool,
    /// Persistent organization-key identifier only. Until the signing backend
    /// verifies a key, this reference never implies authenticity or "signed".
    pub signing_key_reference: String,
    /// Portable public verification material. Private bytes remain in the
    /// native key provider and are never serialized into settings.
    pub signing_public_key_base64: String,
    pub signing_key_fingerprint_sha256: String,
    pub revoked_signing_key_fingerprints: Vec<String>,

    // -- Units & formatting (display/input side only; storage stays SI) -----
    pub unit_system: UnitSystem,
    pub significant_digits: u8,
    pub number_notation: NumberNotation,
    pub input_units: InputUnitPrefs,

    // -- Appearance ----------------------------------------------------------
    /// Interface zoom factor (egui zoom; ⌘+/⌘− adjust it live, this persists).
    pub ui_scale: f32,
    pub colormap: FieldColormap,
    pub cp_range_mode: CpRangeMode,
    pub cp_pinned_extent: f64,
    pub default_section_axis: SectionAxis,
    pub default_section_quantity: SectionQuantity,

    // -- Viewport -------------------------------------------------------------
    /// Which mouse buttons orbit, pan, and zoom. Rival-vendor mappings ship
    /// alongside the Reyn default because orbit muscle memory is a real
    /// switching cost.
    pub navigation_scheme: NavScheme,
    pub orbit_sensitivity: f32,
    pub invert_scroll_zoom: bool,
    pub show_domain_bounds: bool,
    pub show_viewport_hints: bool,
    pub viewport_background: ViewportBackground,

    // -- Workflow defaults ----------------------------------------------------
    pub default_horizon_steps: u32,
    /// Last explicit verified-bundle selections. Aliases consume settings
    /// written by development builds that persisted the runtime field names.
    #[serde(alias = "current_model")]
    pub default_3d_model: String,
    #[serde(alias = "f2d_model")]
    pub default_2d_model: String,
    /// Empty means "use the system default / last location".
    pub default_export_directory: String,
    pub operating_presets: Vec<OperatingPointPreset>,
    /// User-owned reusable defaults. Entries are also exportable as strict,
    /// versioned `.reyntemplate` files for another machine.
    pub case_templates: Vec<CaseTemplate>,
    /// Session-only fail-closed guard. When malformed settings could not be
    /// quarantined, saving defaults must not overwrite the only recovery copy.
    #[serde(skip)]
    pub protected_malformed_settings_path: Option<PathBuf>,
}

impl Default for AppSettings {
    fn default() -> Self {
        let config = EngineConfig::default();
        Self {
            compute_device: ComputeDevice::Auto,
            python_path: config.python_path,
            research_dir: config.research_dir,
            project_directory: default_project_directory(),
            autosave_interval_seconds: 120,
            automatic_update_checks: true,
            theme: ThemeMode::Instrument,
            reduced_motion: false,
            telemetry: false,
            developer_research_sandbox: false,
            signing_key_reference: String::new(),
            signing_public_key_base64: String::new(),
            signing_key_fingerprint_sha256: String::new(),
            revoked_signing_key_fingerprints: Vec::new(),
            unit_system: UnitSystem::Si,
            significant_digits: 5,
            number_notation: NumberNotation::Auto,
            input_units: InputUnitPrefs::default(),
            ui_scale: 1.0,
            colormap: FieldColormap::Ember,
            cp_range_mode: CpRangeMode::Auto,
            cp_pinned_extent: 1.5,
            default_section_axis: SectionAxis::X,
            default_section_quantity: SectionQuantity::PhysicalCp,
            navigation_scheme: NavScheme::default(),
            orbit_sensitivity: 1.0,
            invert_scroll_zoom: false,
            show_domain_bounds: true,
            show_viewport_hints: true,
            viewport_background: ViewportBackground::InstrumentWell,
            default_horizon_steps: 4,
            default_3d_model: DEFAULT_3D_MODEL_ID.into(),
            default_2d_model: DEFAULT_2D_MODEL_ID.into(),
            default_export_directory: String::new(),
            operating_presets: Vec::new(),
            case_templates: Vec::new(),
            protected_malformed_settings_path: None,
        }
    }
}

impl AppSettings {
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = config_path() else {
            return (
                Self::default(),
                Some("settings directory unavailable; using session defaults".into()),
            );
        };
        if !path.is_file() {
            return (Self::default(), None);
        }
        Self::load_from_path(&path)
    }

    fn load_from_path(path: &Path) -> (Self, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                let mut defaults = Self::default();
                defaults.protected_malformed_settings_path = Some(path.to_owned());
                return (
                    defaults,
                    Some(format!(
                        "settings could not be read and were not overwritten; preserve or recover {} before saving: {error}",
                        path.display()
                    )),
                );
            }
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(mut settings) => {
                settings.telemetry = false;
                settings.protected_malformed_settings_path = None;
                let migrated_models = settings.migrate_legacy_model_defaults();
                settings.normalize();
                let warning = migrated_models.then(|| {
                    format!(
                        "Legacy .pth model preferences were migrated to .reynmodel identifiers. {}",
                        TRUSTED_MODEL_CONVERSION_GUIDANCE
                    )
                });
                (settings, warning)
            }
            Err(error) => match quarantine_malformed_settings(path) {
                Ok(recovery_path) => (
                    Self::default(),
                    Some(format!(
                        "malformed settings were preserved at {}; session defaults are active: {error}",
                        recovery_path.display()
                    )),
                ),
                Err(quarantine_error) => {
                    let mut defaults = Self::default();
                    defaults.protected_malformed_settings_path = Some(path.to_owned());
                    (
                        defaults,
                        Some(format!(
                            "malformed settings were not overwritten because quarantine failed ({quarantine_error}); recover {} before saving: {error}",
                            path.display()
                        )),
                    )
                }
            },
        }
    }

    /// Clamp numeric preferences into their supported ranges so a hand-edited
    /// or older settings file can never push the UI into a broken state.
    pub fn normalize(&mut self) {
        self.migrate_legacy_model_defaults();
        if !cfg!(target_os = "macos") && self.compute_device == ComputeDevice::Mps {
            self.compute_device = ComputeDevice::Cpu;
        }
        self.significant_digits = self
            .significant_digits
            .clamp(units::MIN_SIGNIFICANT_DIGITS, units::MAX_SIGNIFICANT_DIGITS);
        if !self.ui_scale.is_finite() {
            self.ui_scale = 1.0;
        }
        self.ui_scale = self.ui_scale.clamp(0.8, 1.4);
        if !self.orbit_sensitivity.is_finite() {
            self.orbit_sensitivity = 1.0;
        }
        self.orbit_sensitivity = self.orbit_sensitivity.clamp(0.4, 2.0);
        if !self.cp_pinned_extent.is_finite() || self.cp_pinned_extent <= 0.0 {
            self.cp_pinned_extent = 1.5;
        }
        self.cp_pinned_extent = self.cp_pinned_extent.clamp(0.05, 100.0);
        self.default_horizon_steps = self.default_horizon_steps.clamp(1, 256);
        self.autosave_interval_seconds = self.autosave_interval_seconds.clamp(30, 3600);
        self.operating_presets
            .retain(|preset| !preset.name.trim().is_empty());
    }

    fn migrate_legacy_model_defaults(&mut self) -> bool {
        let migrated = migrate_legacy_model_id(&mut self.default_3d_model, DEFAULT_3D_MODEL_ID)
            | migrate_legacy_model_id(&mut self.default_2d_model, DEFAULT_2D_MODEL_ID);
        if self.default_3d_model == "flow3d_obs_v1.reynmodel" {
            self.default_3d_model = DEFAULT_3D_MODEL_ID.into();
        }
        migrated
    }

    /// The active display format for numeric values.
    pub fn value_format(&self) -> ValueFormat {
        ValueFormat {
            significant_digits: self.significant_digits,
            notation: self.number_notation,
        }
    }

    pub fn engine_config(&self) -> EngineConfig {
        EngineConfig {
            research_dir: self.research_dir.clone(),
            python_path: self.python_path.clone(),
            device: self.compute_device.engine_value().into(),
            #[cfg(test)]
            engine_script: None,
        }
    }

    pub fn validate_runtime(&self) -> Result<(), String> {
        if self.python_path.trim().is_empty() {
            return Err("Python executable cannot be empty".into());
        }
        let python = Path::new(self.python_path.trim());
        if (python.is_absolute() || self.python_path.contains('/')) && !python.is_file() {
            return Err(format!(
                "Python executable was not found: {}",
                self.python_path
            ));
        }
        let research = Path::new(self.research_dir.trim());
        if !research.is_dir() {
            return Err(format!(
                "Research checkout was not found: {}",
                self.research_dir
            ));
        }
        if !research.join("time_moe_operator.py").is_file() {
            return Err("Research checkout does not contain time_moe_operator.py".into());
        }
        if !self.default_export_directory.trim().is_empty()
            && !Path::new(self.default_export_directory.trim()).is_dir()
        {
            return Err(format!(
                "Default export directory was not found: {}",
                self.default_export_directory
            ));
        }
        Ok(())
    }

    pub fn save(&self) -> Result<PathBuf, String> {
        let path = config_path().ok_or_else(|| "settings directory unavailable".to_string())?;
        save_to(self, &path)?;
        Ok(path)
    }

    pub fn upsert_case_template(&mut self, mut template: CaseTemplate) -> Result<(), String> {
        template.name = template.name.trim().to_owned();
        template.validate()?;
        self.case_templates
            .retain(|existing| !existing.name.eq_ignore_ascii_case(&template.name));
        self.case_templates.push(template);
        self.case_templates.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        });
        Ok(())
    }

    pub fn configured_signing_key(&self) -> Result<Option<PublicKeyRecord>, String> {
        let fields_empty = self.signing_key_reference.trim().is_empty()
            && self.signing_public_key_base64.trim().is_empty()
            && self.signing_key_fingerprint_sha256.trim().is_empty();
        if fields_empty {
            return Ok(None);
        }
        if self.signing_key_reference.trim().is_empty()
            || self.signing_public_key_base64.trim().is_empty()
            || self.signing_key_fingerprint_sha256.trim().is_empty()
        {
            return Err(
                "signing-key reference is incomplete; no signed claim can be produced".into(),
            );
        }
        let record = PublicKeyRecord {
            key_id: self.signing_key_reference.clone(),
            algorithm: SIGNATURE_ALGORITHM.into(),
            public_key_base64: self.signing_public_key_base64.clone(),
            key_fingerprint_sha256: self.signing_key_fingerprint_sha256.clone(),
        };
        record.validate().map_err(|error| error.to_string())?;
        if self
            .revoked_signing_key_fingerprints
            .iter()
            .any(|fingerprint| fingerprint == &record.key_fingerprint_sha256)
        {
            return Err("configured signing key is revoked".into());
        }
        Ok(Some(record))
    }

    pub fn signing_key_is_revoked(&self) -> bool {
        !self.signing_key_fingerprint_sha256.is_empty()
            && self
                .revoked_signing_key_fingerprints
                .iter()
                .any(|fingerprint| fingerprint == &self.signing_key_fingerprint_sha256)
    }

    pub fn set_signing_key(&mut self, key: &PublicKeyRecord) {
        self.signing_key_reference = key.key_id.clone();
        self.signing_public_key_base64 = key.public_key_base64.clone();
        self.signing_key_fingerprint_sha256 = key.key_fingerprint_sha256.clone();
    }

    pub fn revoke_signing_key(&mut self) {
        if !self.signing_key_fingerprint_sha256.is_empty()
            && !self
                .revoked_signing_key_fingerprints
                .contains(&self.signing_key_fingerprint_sha256)
        {
            self.revoked_signing_key_fingerprints
                .push(self.signing_key_fingerprint_sha256.clone());
            self.revoked_signing_key_fingerprints.sort();
            self.revoked_signing_key_fingerprints.dedup();
        }
    }
}

fn migrate_legacy_model_id(model: &mut String, fallback: &str) -> bool {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        *model = fallback.into();
        return false;
    }
    if trimmed != model {
        *model = trimmed.into();
    }
    let path = Path::new(model);
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pth"))
    {
        let mut migrated = path.to_path_buf();
        migrated.set_extension(MODEL_BUNDLE_EXTENSION);
        *model = migrated.to_string_lossy().into_owned();
        true
    } else {
        false
    }
}

fn project_directory_from(
    documents: Option<PathBuf>,
    profile: Option<std::ffi::OsString>,
) -> PathBuf {
    documents
        .or_else(|| {
            profile
                .map(PathBuf::from)
                .map(|home| home.join("Documents"))
        })
        .map(|documents| documents.join("Reyn Studio Projects"))
        .unwrap_or_else(|| PathBuf::from("Reyn Studio Projects"))
}

#[cfg(target_os = "windows")]
fn windows_documents_directory() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Documents, SHGetKnownFolderPath};

    let mut raw = std::ptr::null_mut();
    let result =
        unsafe { SHGetKnownFolderPath(&FOLDERID_Documents, 0, std::ptr::null_mut(), &mut raw) };
    if result < 0 || raw.is_null() {
        return None;
    }
    let mut length = 0usize;
    unsafe {
        while *raw.add(length) != 0 {
            length += 1;
        }
    }
    let path = PathBuf::from(std::ffi::OsString::from_wide(unsafe {
        std::slice::from_raw_parts(raw, length)
    }));
    unsafe {
        CoTaskMemFree(raw.cast());
    }
    (!path.as_os_str().is_empty()).then_some(path)
}

fn default_project_directory() -> String {
    #[cfg(target_os = "windows")]
    {
        return project_directory_from(
            windows_documents_directory(),
            std::env::var_os("USERPROFILE"),
        )
        .to_string_lossy()
        .into_owned();
    }
    #[cfg(not(target_os = "windows"))]
    {
        project_directory_from(
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join("Documents")),
            None,
        )
        .to_string_lossy()
        .into_owned()
    }
}

pub fn config_path() -> Option<PathBuf> {
    if let Some(override_dir) = std::env::var_os("REYN_STUDIO_CONFIG_DIR") {
        return Some(PathBuf::from(override_dir).join("settings.json"));
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home).join("Library/Application Support/Reyn Studio/settings.json")
        })
    }
    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("Reyn Studio/settings.json"));
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|path| path.join("reyn-studio/settings.json"))
    }
}

#[derive(Clone, Copy, Debug)]
pub enum SettingsAction {
    Save,
    RestoreDefaults,
    CreateSigningKey,
    RevokeSigningKey,
    ImportCaseTemplate,
    ExportCaseTemplate(usize),
}

/// Settings screen categories — a left rail selects one; only its sections
/// render, so depth never becomes a control wall (§3, progressive disclosure).
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
pub enum SettingsCategory {
    #[default]
    Compute,
    Units,
    Appearance,
    Viewport,
    Workflow,
    Shortcuts,
    Storage,
    Updates,
    Signing,
    Developer,
}

impl SettingsCategory {
    pub const ALL: [Self; 10] = [
        Self::Compute,
        Self::Units,
        Self::Appearance,
        Self::Viewport,
        Self::Workflow,
        Self::Shortcuts,
        Self::Storage,
        Self::Updates,
        Self::Signing,
        Self::Developer,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Compute => "Compute & engine",
            Self::Units => "Units & formatting",
            Self::Appearance => "Appearance",
            Self::Viewport => "Viewport & camera",
            Self::Workflow => "Workflow defaults",
            Self::Shortcuts => "Keyboard shortcuts",
            Self::Storage => "Storage & recovery",
            Self::Updates => "Updates",
            Self::Signing => "Signing & privacy",
            Self::Developer => "Developer",
        }
    }

    /// Dev/QA deep-link ids for `REYN_STUDIO_START_NAV=settings:<id>`.
    pub fn from_qa_id(id: &str) -> Option<Self> {
        Some(match id {
            "compute" => Self::Compute,
            "units" => Self::Units,
            "appearance" => Self::Appearance,
            "viewport" => Self::Viewport,
            "workflow" => Self::Workflow,
            "shortcuts" => Self::Shortcuts,
            "storage" => Self::Storage,
            "updates" => Self::Updates,
            "signing" => Self::Signing,
            "developer" => Self::Developer,
            _ => return None,
        })
    }
}

/// Per-session UI state for the Settings screen (never persisted).
pub struct SettingsUiState {
    pub category: SettingsCategory,
    pub confirm_restore_defaults: bool,
    pub revoke_signing_key_armed: bool,
    preset_delete_armed: Option<usize>,
    template_delete_armed: Option<usize>,
    qa_focus_category: Option<SettingsCategory>,
    qa_scroll_bottom: bool,
}

impl Default for SettingsUiState {
    fn default() -> Self {
        // Capture-only state hooks. They do not alter AppSettings, and ordinary
        // launches never set them. Keeping them here lets the native screenshot
        // harness exercise adverse states without app.rs ownership.
        let armed = std::env::var("REYN_STUDIO_SETTINGS_QA_ARM_DELETE").ok();
        let preset_delete_armed = armed
            .as_deref()
            .and_then(|value| value.strip_prefix("preset:"))
            .and_then(|index| index.parse().ok());
        let template_delete_armed = armed
            .as_deref()
            .and_then(|value| value.strip_prefix("template:"))
            .and_then(|index| index.parse().ok());
        let qa_focus_category = std::env::var("REYN_STUDIO_SETTINGS_QA_FOCUS_CATEGORY")
            .ok()
            .and_then(|id| SettingsCategory::from_qa_id(&id));
        let qa_scroll_bottom = std::env::var_os("REYN_STUDIO_SETTINGS_QA_SCROLL_BOTTOM").is_some();
        Self {
            category: SettingsCategory::default(),
            confirm_restore_defaults: false,
            revoke_signing_key_armed: false,
            preset_delete_armed,
            template_delete_armed,
            qa_focus_category,
            qa_scroll_bottom,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsBodyLayout {
    CategoryRail,
    CategoryPicker,
}

const CATEGORY_PICKER_BREAKPOINT: f32 = 720.0;
const CATEGORY_PICKER_HEIGHT: f32 = 54.0;

fn settings_body_layout(width: f32) -> SettingsBodyLayout {
    if width < CATEGORY_PICKER_BREAKPOINT {
        SettingsBodyLayout::CategoryPicker
    } else {
        SettingsBodyLayout::CategoryRail
    }
}

fn settings_footer_height(confirming_restore: bool) -> f32 {
    if confirming_restore {
        100.0
    } else {
        64.0
    }
}

const SAVE_DISABLED_REASON: &str = "No settings changes to save.";

fn save_disabled_reason(dirty: bool) -> Option<&'static str> {
    (!dirty).then_some(SAVE_DISABLED_REASON)
}

pub fn show_settings(
    ui: &mut egui::Ui,
    saved: &AppSettings,
    draft: &mut AppSettings,
    state: &mut SettingsUiState,
    updater: Option<&crate::updater::Updater>,
) -> Option<SettingsAction> {
    // S6/G3: the same centered content column as every other document screen
    // (980 max width, symmetric ≥34px gutters — the founder's margins fix).
    content_column(ui, CONTENT_MAX_WIDTH, |ui| {
        let mut action = None;
        ui.add_space(24.0);
        ui.label(display_text("Desktop settings"));
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Units, appearance, viewport, workflow, runtime, storage, and signing-key state. Changes stay on this machine; stored evidence remains SI.",
            )
            .text_style(caption())
            .color(TEXT_MUTE),
        );
        ui.add_space(16.0);

        // Reserve room for the action row below the scroll region so the save
        // controls can never be pushed under the status bar.
        let footer_height = settings_footer_height(state.confirm_restore_defaults);
        let body_height = (ui.available_height() - footer_height).max(120.0);
        match settings_body_layout(ui.available_width()) {
            SettingsBodyLayout::CategoryRail => {
                ui.horizontal_top(|ui| {
                    category_rail(ui, state);
                    ui.add_space(6.0);
                    // Single meeting-edge hairline between rail and content
                    // (§3.4 level 0). It stops short of the action-row rule.
                    let x = ui.cursor().min.x;
                    ui.painter().vline(
                        x,
                        egui::Rangef::new(
                            ui.cursor().min.y,
                            ui.cursor().min.y + (body_height - 14.0).max(0.0),
                        ),
                        Stroke::new(1.0, HAIRLINE),
                    );
                    ui.add_space(14.0);
                    ui.vertical(|ui| {
                        settings_category_scroll(
                            ui,
                            body_height,
                            draft,
                            state,
                            &mut action,
                            updater,
                        );
                    });
                });
            }
            SettingsBodyLayout::CategoryPicker => {
                category_picker(ui, state);
                ui.add_space(8.0);
                settings_category_scroll(
                    ui,
                    (body_height - CATEGORY_PICKER_HEIGHT).max(80.0),
                    draft,
                    state,
                    &mut action,
                    updater,
                );
            }
        }

        // Action row — always visible beneath the scroll region, above the
        // status bar, separated by a full-span hairline (§3.4 level 0).
        ui.add_space(8.0);
        let cursor_y = ui.cursor().min.y - 4.0;
        ui.painter().hline(
            ui.max_rect().x_range(),
            cursor_y,
            Stroke::new(1.0, HAIRLINE),
        );
        ui.add_space(4.0);
        if state.confirm_restore_defaults {
            ui.label(
                RichText::new(
                    "Restore every preference default? Signing keys, saved presets, and case templates are kept.",
                )
                .text_style(caption())
                .color(WARN),
            );
            ui.add_space(4.0);
        }
        ui.horizontal_wrapped(|ui| {
            let dirty = draft != saved;
            let save_fill = if dirty { EMBER } else { SURFACE_HIGH };
            let save_text = if dirty { ON_EMBER } else { TEXT_MUTE };
            let save = ui.add_enabled(
                dirty,
                egui::Button::new(
                    RichText::new(if runtime_changed(saved, draft) {
                        "Save & restart engine"
                    } else {
                        "Save settings"
                    })
                    .color(save_text),
                )
                .fill(save_fill)
                .min_size(egui::vec2(0.0, 28.0)),
            );
            let save = if let Some(reason) = save_disabled_reason(dirty) {
                save.on_disabled_hover_text(reason)
            } else {
                save
            };
            if save.clicked() {
                action = Some(SettingsAction::Save);
            }
            if state.confirm_restore_defaults {
                if ui
                    .add(egui::Button::new("Confirm reset").min_size(egui::vec2(0.0, 28.0)))
                    .clicked()
                {
                    action = Some(SettingsAction::RestoreDefaults);
                    state.confirm_restore_defaults = false;
                }
                if ui
                    .add(egui::Button::new("Cancel").min_size(egui::vec2(0.0, 28.0)))
                    .clicked()
                {
                    state.confirm_restore_defaults = false;
                }
            } else if ui
                .add(egui::Button::new("Restore defaults…").min_size(egui::vec2(0.0, 28.0)))
                .clicked()
            {
                state.confirm_restore_defaults = true;
            }
            if dirty {
                ui.label(
                    RichText::new("Unsaved changes")
                        .text_style(caption())
                        .color(WARN),
                );
            }
        });
        action
    })
}

fn category_picker(ui: &mut egui::Ui, state: &mut SettingsUiState) {
    ui.label(overline_text("Category"));
    egui::ComboBox::from_id_salt("settings.category-picker")
        .selected_text(state.category.label())
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            for category in SettingsCategory::ALL {
                ui.selectable_value(&mut state.category, category, category.label());
            }
        });
}

fn category_rail(ui: &mut egui::Ui, state: &mut SettingsUiState) {
    // Quiet list rows; the active row gets a tonal fill and edge marker. Ember
    // remains reserved for Save, but keyboard focus is always explicit.
    ui.vertical(|ui| {
        ui.set_width(188.0);
        for category in SettingsCategory::ALL {
            let active = state.category == category;
            let (_, rect) = ui.allocate_space(egui::vec2(ui.available_width(), 30.0));
            let response = ui.interact(rect, category_row_id(category), egui::Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::SelectableLabel,
                    true,
                    active,
                    category.label(),
                )
            });
            if state.qa_focus_category == Some(category) {
                response.request_focus();
                if response.has_focus() {
                    state.qa_focus_category = None;
                }
            }
            let painter = ui.painter();
            if active {
                painter.rect_filled(rect, CornerRadius::same(R1), SURFACE_HIGH);
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        rect.min + egui::vec2(0.0, 6.0),
                        egui::vec2(2.0, rect.height() - 12.0),
                    ),
                    CornerRadius::same(1),
                    OUTLINE,
                );
            } else if response.hovered() {
                painter.rect_filled(rect, CornerRadius::same(R1), SURFACE);
            }
            if response.has_focus() {
                painter.rect_stroke(
                    rect.expand(1.0),
                    CornerRadius::same(R1),
                    focus_stroke(),
                    egui::StrokeKind::Outside,
                );
            }
            painter.text(
                egui::pos2(rect.min.x + 12.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                category.label(),
                body_strong().resolve(ui.style()),
                if active { TEXT } else { TEXT_DIM },
            );
            let keyboard_activated = response.has_focus()
                && ui.input_mut(|input| {
                    input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                        || input.consume_key(egui::Modifiers::NONE, egui::Key::Space)
                });
            if response.clicked() || keyboard_activated {
                state.category = category;
            }
            ui.add_space(2.0);
        }
    });
}

fn category_row_id(category: SettingsCategory) -> egui::Id {
    egui::Id::new(("settings.category-row", category))
}

fn settings_category_scroll(
    ui: &mut egui::Ui,
    height: f32,
    draft: &mut AppSettings,
    state: &mut SettingsUiState,
    action: &mut Option<SettingsAction>,
    updater: Option<&crate::updater::Updater>,
) {
    let mut scroll = egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(height);
    if state.qa_scroll_bottom {
        // Egui propagates an infinite offset into the clip calculation and
        // renders a blank viewport; a finite sentinel clamps to the real end.
        scroll = scroll.vertical_scroll_offset(1_000_000.0);
    }
    scroll.show(ui, |ui| {
        match state.category {
            SettingsCategory::Compute => category_compute(ui, draft, action),
            SettingsCategory::Units => category_units(ui, draft),
            SettingsCategory::Appearance => category_appearance(ui, draft),
            SettingsCategory::Viewport => category_viewport(ui, draft),
            SettingsCategory::Workflow => category_workflow(ui, draft, state, action),
            SettingsCategory::Shortcuts => category_shortcuts(ui, draft),
            SettingsCategory::Storage => category_storage(ui, draft),
            SettingsCategory::Updates => category_updates(ui, draft, updater),
            SettingsCategory::Signing => category_signing(ui, draft, state, action),
            SettingsCategory::Developer => category_developer(ui, draft),
        }
        // Tall windows used to leave unexplained space under short cards.
        // Anchor a real scope note to the bottom instead.
        let footnote = settings_scope_footnote_height(ui);
        let slack = ui.available_height();
        ui.add_space(if slack > footnote + 26.0 {
            slack - footnote
        } else {
            16.0
        });
        settings_scope_footnote(ui);
    });
}

fn category_updates(
    ui: &mut egui::Ui,
    draft: &mut AppSettings,
    updater: Option<&crate::updater::Updater>,
) {
    section(ui, "Application updates", |ui| {
        crate::updater::show_settings(ui, updater, &mut draft.automatic_update_checks);
    });
}

const SCOPE_FOOTNOTE_SCOPE: &str = "Local to this machine. Stored evidence — run manifests, fields, and versioned exports — stays SI regardless of these choices.";

fn scope_footnote_location() -> String {
    let location = config_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "location unavailable on this machine".to_owned());
    format!("Preferences file · {location}")
}

/// Height the footnote needs at the current column width. Measured rather than
/// guessed: the scope line wraps to two rows in a narrow window, and a guessed
/// reserve clipped it.
fn settings_scope_footnote_height(ui: &egui::Ui) -> f32 {
    let width = ui.available_width();
    let line = |text: String, style: egui::TextStyle| {
        let font = style.resolve(ui.style());
        ui.painter().layout(text, font, TEXT_MUTE, width).size().y
    };
    // Two label rows, so two helpings of the vertical item spacing egui inserts.
    9.0 + line(scope_footnote_location(), mono_s())
        + 2.0
        + line(SCOPE_FOOTNOTE_SCOPE.to_owned(), caption())
        + ui.spacing().item_spacing.y * 2.0
}

/// Quiet closing line for the settings column: where preferences are stored and
/// what they cannot change. Anchors the bottom of the content area at any height.
fn settings_scope_footnote(ui: &mut egui::Ui) {
    ui.painter().hline(
        egui::Rangef::new(ui.cursor().min.x, ui.max_rect().max.x),
        ui.cursor().min.y,
        Stroke::new(1.0, HAIRLINE),
    );
    ui.add_space(9.0);
    ui.label(
        RichText::new(scope_footnote_location())
            .text_style(mono_s())
            .color(TEXT_MUTE),
    );
    ui.add_space(2.0);
    ui.label(
        RichText::new(SCOPE_FOOTNOTE_SCOPE)
            .text_style(caption())
            .color(TEXT_MUTE),
    );
}

// ---------------------------------------------------------------------------
// Category bodies
// ---------------------------------------------------------------------------

fn category_compute(
    ui: &mut egui::Ui,
    draft: &mut AppSettings,
    _action: &mut Option<SettingsAction>,
) {
    section(ui, "Compute & engine", |ui| {
        setting_row_reset(
            ui,
            "Compute device",
            if cfg!(target_os = "macos") {
                "Reloads the Python sidecar; Automatic prefers Metal on Apple Silicon."
            } else {
                "Reloads the bundled Python sidecar. Windows preview supports Automatic and CPU only; CUDA is not qualified."
            },
            &mut draft.compute_device,
            AppSettings::default().compute_device,
            |ui, value| {
                egui::ComboBox::from_id_salt("settings.device")
                    .selected_text(value.label())
                    .width(190.0)
                    .show_ui(ui, |ui| {
                        for &device in ComputeDevice::available() {
                            ui.selectable_value(value, device, device.label());
                        }
                    });
            },
        );
        ui.separator();
        path_setting_row(
            ui,
            "Python executable",
            "Pinned interpreter used to launch the bundled inference engine.",
            "python",
            &mut draft.python_path,
            &AppSettings::default().python_path,
            PathPick::File,
        );
        ui.separator();
        path_setting_row(
            ui,
            "Research checkout",
            "Verified model-bundle library and solver modules used by this development build.",
            "research",
            &mut draft.research_dir,
            &AppSettings::default().research_dir,
            PathPick::Folder,
        );
    });
}

fn category_units(ui: &mut egui::Ui, draft: &mut AppSettings) {
    section(ui, "Displayed results", |ui| {
        setting_row_reset(
            ui,
            "Unit system",
            "Applies to Results, Case Setup summaries, and reports. Run manifests, evidence, and the versioned FEA CSV stay SI.",
            &mut draft.unit_system,
            AppSettings::default().unit_system,
            |ui, value| {
                egui::ComboBox::from_id_salt("settings.units")
                    .selected_text(value.label())
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        for system in UnitSystem::ALL {
                            ui.selectable_value(value, system, system.label());
                        }
                    });
            },
        );
        ui.separator();
        setting_row_reset(
            ui,
            "Significant digits",
            "Displayed precision for measured and derived values (3–8).",
            &mut draft.significant_digits,
            AppSettings::default().significant_digits,
            |ui, value| {
                ui.add(
                    egui::DragValue::new(value)
                        .range(units::MIN_SIGNIFICANT_DIGITS..=units::MAX_SIGNIFICANT_DIGITS),
                );
            },
        );
        ui.separator();
        setting_row_reset(
            ui,
            "Notation",
            "Automatic keeps values fixed inside 10⁻³…10⁵ and scientific outside.",
            &mut draft.number_notation,
            AppSettings::default().number_notation,
            |ui, value| {
                egui::ComboBox::from_id_salt("settings.notation")
                    .selected_text(value.label())
                    .width(190.0)
                    .show_ui(ui, |ui| {
                        for notation in NumberNotation::ALL {
                            ui.selectable_value(value, notation, notation.label());
                        }
                    });
            },
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!(
                "Preview · {} · {}",
                units::format_quantity(
                    units::Quantity::Velocity,
                    30.0,
                    draft.unit_system,
                    draft.value_format()
                ),
                units::format_quantity(
                    units::Quantity::Pressure,
                    101_325.0,
                    draft.unit_system,
                    draft.value_format()
                ),
            ))
            .text_style(mono_s())
            .color(TEXT_MUTE),
        );
    });

    ui.add_space(12.0);
    section(ui, "Case input units", |ui| {
        ui.label(
            RichText::new(
                "Default entry units for the operating point. Values convert to SI on entry; \
                 the unit can also be switched per field in Case Setup.",
            )
            .text_style(caption())
            .color(TEXT_MUTE),
        );
        ui.add_space(6.0);
        setting_row_reset(
            ui,
            "Free-stream speed",
            "m/s, km/h, mph, ft/s, or knots.",
            &mut draft.input_units.velocity,
            InputUnitPrefs::default().velocity,
            |ui, value| {
                egui::ComboBox::from_id_salt("settings.input.velocity")
                    .selected_text(value.symbol())
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for unit in units::VelocityUnit::ALL {
                            ui.selectable_value(value, unit, unit.symbol());
                        }
                    });
            },
        );
        ui.separator();
        setting_row_reset(
            ui,
            "Pressure",
            "Pa, kPa, psi, or atm.",
            &mut draft.input_units.pressure,
            InputUnitPrefs::default().pressure,
            |ui, value| {
                egui::ComboBox::from_id_salt("settings.input.pressure")
                    .selected_text(value.symbol())
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for unit in units::PressureUnit::ALL {
                            ui.selectable_value(value, unit, unit.symbol());
                        }
                    });
            },
        );
        ui.separator();
        setting_row_reset(
            ui,
            "Density",
            "kg/m³ or lbm/ft³.",
            &mut draft.input_units.density,
            InputUnitPrefs::default().density,
            |ui, value| {
                egui::ComboBox::from_id_salt("settings.input.density")
                    .selected_text(value.symbol())
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for unit in units::DensityUnit::ALL {
                            ui.selectable_value(value, unit, unit.symbol());
                        }
                    });
            },
        );
        ui.separator();
        setting_row_reset(
            ui,
            "Dynamic viscosity",
            "Pa·s or mPa·s (centipoise).",
            &mut draft.input_units.viscosity,
            InputUnitPrefs::default().viscosity,
            |ui, value| {
                egui::ComboBox::from_id_salt("settings.input.viscosity")
                    .selected_text(value.symbol())
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for unit in units::ViscosityUnit::ALL {
                            ui.selectable_value(value, unit, unit.symbol());
                        }
                    });
            },
        );
    });
}

fn category_appearance(ui: &mut egui::Ui, draft: &mut AppSettings) {
    section(ui, "Interface", |ui| {
        setting_row_reset(
            ui,
            "Theme",
            "Both modes preserve the calibrated ember data vocabulary.",
            &mut draft.theme,
            AppSettings::default().theme,
            |ui, value| {
                ui.horizontal(|ui| {
                    theme_preview(ui, *value);
                    egui::ComboBox::from_id_salt("settings.theme")
                        .selected_text(value.label())
                        .width(190.0)
                        .show_ui(ui, |ui| {
                            for theme in ThemeMode::ALL {
                                ui.selectable_value(value, theme, theme.label());
                            }
                        });
                });
            },
        );
        ui.separator();
        setting_row_reset(
            ui,
            "Interface scale",
            "Zoom for all interface text and controls. ⌘+ / ⌘− adjust it live; this value persists.",
            &mut draft.ui_scale,
            AppSettings::default().ui_scale,
            |ui, value| {
                ui.add(
                    egui::Slider::new(value, 0.8..=1.4)
                        .step_by(0.05)
                        .custom_formatter(|scale, _| format!("{:.0}%", scale * 100.0)),
                );
            },
        );
        ui.separator();
        setting_row_reset(
            ui,
            "Reduce motion",
            "Replaces interface transitions (hover fades, screen crossfades, shimmer) with instant state changes.",
            &mut draft.reduced_motion,
            AppSettings::default().reduced_motion,
            |ui, value| {
                ui.checkbox(value, "Enable");
            },
        );
    });

    ui.add_space(12.0);
    section(ui, "Field views", |ui| {
        setting_row_reset(
            ui,
            "Colormap",
            "Applies to interactive section and 2D field views. Deterministic evidence exports keep the calibrated instrument map.",
            &mut draft.colormap,
            AppSettings::default().colormap,
            |ui, value| {
                egui::ComboBox::from_id_salt("settings.colormap")
                    .selected_text(value.label())
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        for map in FieldColormap::ALL {
                            ui.selectable_value(value, map, map.label());
                        }
                    });
            },
        );
        ui.separator();
        setting_row_reset(
            ui,
            "Cp color range",
            "Auto scales each section to its own extrema; pinned keeps a fixed ±range so sections compare across runs.",
            &mut draft.cp_range_mode,
            AppSettings::default().cp_range_mode,
            |ui, value| {
                egui::ComboBox::from_id_salt("settings.cp-range")
                    .selected_text(value.label())
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        for mode in CpRangeMode::ALL {
                            ui.selectable_value(value, mode, mode.label());
                        }
                    });
            },
        );
        if draft.cp_range_mode == CpRangeMode::Pinned {
            ui.separator();
            setting_row_reset(
                ui,
                "Pinned Cp extent",
                "Sections render as ±extent around Cp = 0.",
                &mut draft.cp_pinned_extent,
                AppSettings::default().cp_pinned_extent,
                |ui, value| {
                    ui.add(
                        egui::DragValue::new(value)
                            .speed(0.05)
                            .range(0.05..=100.0)
                            .prefix("± "),
                    );
                },
            );
        }
        ui.separator();
        // Two-part control on one row; the tuple lens gives the pair a single
        // reset affordance and writes back both halves.
        let mut section_default = (draft.default_section_axis, draft.default_section_quantity);
        setting_row_reset(
            ui,
            "Default section view",
            "The section plane and quantity a new engineering result opens with.",
            &mut section_default,
            (
                AppSettings::default().default_section_axis,
                AppSettings::default().default_section_quantity,
            ),
            |ui, value| {
                egui::ComboBox::from_id_salt("settings.section-quantity")
                    .selected_text(value.1.label())
                    .width(190.0)
                    .show_ui(ui, |ui| {
                        for quantity in SectionQuantity::ALL {
                            ui.selectable_value(&mut value.1, quantity, quantity.label());
                        }
                    });
                egui::ComboBox::from_id_salt("settings.section-axis")
                    .selected_text(value.0.label())
                    .width(52.0)
                    .show_ui(ui, |ui| {
                        for axis in SectionAxis::ALL {
                            ui.selectable_value(&mut value.0, axis, axis.label());
                        }
                    });
            },
        );
        draft.default_section_axis = section_default.0;
        draft.default_section_quantity = section_default.1;
    });
}

fn category_viewport(ui: &mut egui::Ui, draft: &mut AppSettings) {
    section(ui, "Navigation scheme", |ui| {
        setting_row_reset(
            ui,
            "Mouse mapping",
            "Which buttons orbit, pan, and zoom. Pick the tool your hands already know — the camera behaviour is identical, only the buttons move.",
            &mut draft.navigation_scheme,
            AppSettings::default().navigation_scheme,
            |ui, value| {
                egui::ComboBox::from_id_salt("settings.nav-scheme")
                    .selected_text(value.label())
                    .width(190.0)
                    .show_ui(ui, |ui| {
                        for scheme in NavScheme::ALL {
                            ui.selectable_value(value, scheme, scheme.label())
                                .on_hover_text(scheme.detail());
                        }
                    });
            },
        );
        ui.add_space(6.0);
        // The active mapping is printed rather than described, so the binding is
        // learnable without experimenting in the viewport.
        for (gesture, binding) in draft.navigation_scheme.mapping() {
            ui.horizontal(|ui| {
                ui.allocate_ui(egui::vec2(96.0, 18.0), |ui| {
                    ui.label(
                        RichText::new(gesture)
                            .text_style(body_strong())
                            .color(TEXT_DIM),
                    );
                });
                ui.label(RichText::new(binding).text_style(mono_s()).color(TEXT));
            });
            ui.add_space(2.0);
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new(draft.navigation_scheme.detail())
                .text_style(caption())
                .color(TEXT_MUTE),
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "F frames the geometry. 1–7 jump to the standard stations; the free stream always runs along +X, so the views are named for the flow.",
            )
            .text_style(caption())
            .color(TEXT_MUTE),
        );
    });
    ui.add_space(12.0);
    section(ui, "Camera", |ui| {
        setting_row_reset(
            ui,
            "Orbit sensitivity",
            "Degrees of rotation per pixel of drag, as a multiplier of the calibrated default.",
            &mut draft.orbit_sensitivity,
            AppSettings::default().orbit_sensitivity,
            |ui, value| {
                ui.add(
                    egui::Slider::new(value, 0.4..=2.0)
                        .step_by(0.1)
                        .custom_formatter(|factor, _| format!("{factor:.1}×")),
                );
            },
        );
        ui.separator();
        setting_row_reset(
            ui,
            "Invert scroll zoom",
            "Scroll up moves the camera away instead of closer.",
            &mut draft.invert_scroll_zoom,
            AppSettings::default().invert_scroll_zoom,
            |ui, value| {
                ui.checkbox(value, "Invert");
            },
        );
    });
    ui.add_space(12.0);
    section(ui, "Overlays & background", |ui| {
        setting_row_reset(
            ui,
            "Domain bounds",
            "Draw the [-1, 1]³ solver-domain wireframe in the 3D viewport.",
            &mut draft.show_domain_bounds,
            AppSettings::default().show_domain_bounds,
            |ui, value| {
                ui.checkbox(value, "Show");
            },
        );
        ui.separator();
        setting_row_reset(
            ui,
            "Interaction hints",
            "The one-line orbit/zoom hint at the bottom of render viewports.",
            &mut draft.show_viewport_hints,
            AppSettings::default().show_viewport_hints,
            |ui, value| {
                ui.checkbox(value, "Show");
            },
        );
        ui.separator();
        setting_row_reset(
            ui,
            "Viewport background",
            "Theme-sanctioned surfaces only; the calibrated data vocabulary is unchanged.",
            &mut draft.viewport_background,
            AppSettings::default().viewport_background,
            |ui, value| {
                egui::ComboBox::from_id_salt("settings.viewport-bg")
                    .selected_text(value.label())
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        for background in ViewportBackground::ALL {
                            ui.selectable_value(value, background, background.label());
                        }
                    });
            },
        );
    });
}

fn category_workflow(
    ui: &mut egui::Ui,
    draft: &mut AppSettings,
    state: &mut SettingsUiState,
    action: &mut Option<SettingsAction>,
) {
    section(ui, "New-case defaults", |ui| {
        setting_row_reset(
            ui,
            "Default prediction horizon",
            "Initial horizon for a newly imported case, clamped to the selected model's support.",
            &mut draft.default_horizon_steps,
            AppSettings::default().default_horizon_steps,
            |ui, value| {
                ui.add(egui::DragValue::new(value).range(1..=256).suffix(" steps"));
            },
        );
        ui.separator();
        path_setting_row(
            ui,
            "Default export directory",
            "Starting location for CSV, PNG, and report export dialogs. Empty uses the system default.",
            "export-dir",
            &mut draft.default_export_directory,
            &AppSettings::default().default_export_directory,
            PathPick::Folder,
        );
    });
    ui.add_space(12.0);
    section(ui, "Operating-point presets", |ui| {
        ui.label(
            RichText::new(
                "Save the current operating point from Case Setup (\"Save as preset…\"). \
                 Presets fill draft inputs only — every readiness gate still runs.",
            )
            .text_style(caption())
            .color(TEXT_MUTE),
        );
        ui.add_space(8.0);
        ui.label(overline_text("Built-in references"));
        ui.add_space(3.0);
        for preset in built_in_presets() {
            collection_item_row(
                ui,
                &preset.name,
                &preset_summary(&preset),
                TEXT_MUTE,
                |ui| {
                    ui.label(
                        RichText::new("Built-in")
                            .text_style(caption())
                            .color(TEXT_MUTE),
                    );
                },
            );
        }
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(overline_text("Custom presets"));
        ui.add_space(3.0);
        let mut remove_index = None;
        for (index, preset) in draft.operating_presets.iter().enumerate() {
            let armed = state.preset_delete_armed == Some(index);
            collection_item_row(ui, &preset.name, &preset_summary(preset), TEXT_MUTE, |ui| {
                if armed {
                    if ui
                        .small_button("Cancel")
                        .on_hover_text("Keep this preset")
                        .clicked()
                    {
                        state.preset_delete_armed = None;
                    }
                    if ui
                        .small_button("Confirm remove")
                        .on_hover_text("Remove this saved preset after settings are saved")
                        .clicked()
                    {
                        remove_index = Some(index);
                    }
                } else if ui
                    .small_button("Remove…")
                    .on_hover_text(format!("Remove preset “{}”", preset.name))
                    .clicked()
                {
                    state.preset_delete_armed = Some(index);
                }
            });
        }
        if let Some(index) = remove_index {
            draft.operating_presets.remove(index);
            state.preset_delete_armed = None;
        }
        if draft.operating_presets.is_empty() {
            state.preset_delete_armed = None;
            empty_collection_state(
                ui,
                "No custom presets",
                "Create one from Case Setup › Operating point. Built-in references remain available.",
            );
        }
    });
    ui.add_space(12.0);
    section(ui, "Portable case templates", |ui| {
        ui.label(
            RichText::new(
                "Versioned defaults for operating conditions and the preferred section view. \
                 Templates never include geometry, models, transforms, waivers, runs, or evidence; \
                 applying one still runs every readiness gate.",
            )
            .text_style(caption())
            .color(TEXT_MUTE),
        );
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Import template…").clicked() {
                *action = Some(SettingsAction::ImportCaseTemplate);
            }
            ui.label(
                RichText::new(format!(
                    ".{} · schema {} · defaults only",
                    CASE_TEMPLATE_EXTENSION, CASE_TEMPLATE_SCHEMA_VERSION
                ))
                .text_style(mono_s())
                .color(TEXT_MUTE),
            );
        });
        if draft.case_templates.is_empty() {
            state.template_delete_armed = None;
            ui.add_space(8.0);
            empty_collection_state(
                ui,
                "No saved case templates",
                "Import a .reyntemplate file here, or save defaults from Case Setup.",
            );
            return;
        }
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);
        let mut remove_index = None;
        for (index, template) in draft.case_templates.iter().enumerate() {
            let validation = template.validate();
            let (detail, color) = match &validation {
                Ok(()) => (template.summary(), TEXT_MUTE),
                Err(error) => (format!("! Unavailable · {error}"), DANGER),
            };
            let armed = state.template_delete_armed == Some(index);
            collection_item_row(ui, &template.name, &detail, color, |ui| {
                if armed {
                    if ui
                        .small_button("Cancel")
                        .on_hover_text("Keep this case template")
                        .clicked()
                    {
                        state.template_delete_armed = None;
                    }
                    if ui
                        .small_button("Confirm remove")
                        .on_hover_text("Remove this template after settings are saved")
                        .clicked()
                    {
                        remove_index = Some(index);
                    }
                } else {
                    if ui
                        .small_button("Remove…")
                        .on_hover_text(format!("Remove template “{}”", template.name))
                        .clicked()
                    {
                        state.template_delete_armed = Some(index);
                    }
                    let export = ui
                        .add_enabled(validation.is_ok(), egui::Button::new("Export…"))
                        .on_disabled_hover_text(
                            validation
                                .as_ref()
                                .err()
                                .map_or("Template is unavailable", String::as_str),
                        );
                    if export.clicked() {
                        *action = Some(SettingsAction::ExportCaseTemplate(index));
                    }
                }
            });
        }
        if let Some(index) = remove_index {
            draft.case_templates.remove(index);
            state.template_delete_armed = None;
        }
    });
}

fn collection_item_row(
    ui: &mut egui::Ui,
    name: &str,
    detail: &str,
    detail_color: egui::Color32,
    actions: impl FnOnce(&mut egui::Ui),
) {
    ui.add_space(5.0);
    let identity = |ui: &mut egui::Ui| {
        ui.add(
            egui::Label::new(RichText::new(name).text_style(body_strong()).color(TEXT)).truncate(),
        )
        .on_hover_text(name);
        ui.add(
            egui::Label::new(
                RichText::new(detail)
                    .text_style(mono_s())
                    .color(detail_color),
            )
            .truncate(),
        )
        .on_hover_text(detail);
    };
    if collection_row_stacks(ui.available_width()) {
        identity(ui);
        ui.add_space(3.0);
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), actions);
        });
    } else {
        let available = ui.available_width();
        let action_width = 190.0_f32.min(available * 0.38);
        let gap = ui.spacing().item_spacing.x;
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2((available - action_width - gap).max(120.0), 0.0),
                Layout::top_down(Align::Min),
                identity,
            );
            ui.allocate_ui_with_layout(
                egui::vec2(action_width, 0.0),
                Layout::right_to_left(Align::Center),
                actions,
            );
        });
    }
    ui.add_space(5.0);
}

fn empty_collection_state(ui: &mut egui::Ui, title: &str, detail: &str) {
    Frame::NONE
        .fill(BG_0)
        .stroke(Stroke::new(1.0, HAIRLINE))
        .corner_radius(CornerRadius::same(R1))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.label(
                    RichText::new("○")
                        .text_style(body_strong())
                        .color(TEXT_MUTE),
                );
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(title)
                            .text_style(body_strong())
                            .color(TEXT_DIM),
                    );
                    ui.label(RichText::new(detail).text_style(caption()).color(TEXT_MUTE));
                });
            });
        });
}

fn preset_summary(preset: &OperatingPointPreset) -> String {
    format!(
        "{} m/s · {} kg/m³ · {:.3e} Pa·s",
        preset.velocity_mps, preset.density_kg_m3, preset.viscosity_pa_s
    )
}

fn shortcut_labels(
    is_macos: bool,
) -> (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
) {
    if is_macos {
        ("⌘Z", "⇧⌘Z", "⌘", "⇧⌘", "⌘W / ⌘Q")
    } else {
        (
            "Ctrl+Z",
            "Ctrl+Shift+Z / Ctrl+Y",
            "Ctrl+",
            "Ctrl+Shift+",
            "Ctrl+W / Alt+F4",
        )
    }
}

fn category_shortcuts(ui: &mut egui::Ui, draft: &mut AppSettings) {
    section(ui, "Keyboard reference", |ui| {
        ui.label(
            RichText::new("Bindings are fixed in this build; rebinding is planned.")
                .text_style(caption())
                .color(TEXT_MUTE),
        );
        ui.add_space(8.0);
        let (undo_keys, redo_keys, primary, shift_primary, close_keys) =
            shortcut_labels(cfg!(target_os = "macos"));
        let mut rows: Vec<(String, String)> = vec![
            (
                format!("{primary}K"),
                "Command palette — navigate or act".into(),
            ),
            (
                undo_keys.into(),
                "Undo the last safe Case Setup draft edit (immutable identity excluded)".into(),
            ),
            (
                redo_keys.into(),
                "Redo the last safe Case Setup draft edit".into(),
            ),
            (
                format!("{primary}N"),
                "New project (guarded by unsaved changes)".into(),
            ),
            (format!("{primary}O"), "Open project…".into()),
            (format!("{primary}S"), "Save project".into()),
            (format!("{shift_primary}S"), "Save project as…".into()),
            (
                close_keys.into(),
                "Close / quit through the unsaved-changes guard".into(),
            ),
            (
                format!("{primary}+ / {primary}− / {primary}0"),
                "Interface zoom in / out / reset (live)".into(),
            ),
        ];
        if draft.developer_research_sandbox {
            rows.push((
                "G".into(),
                "Regenerate procedural flow (research sandbox)".into(),
            ));
            rows.push((
                "← ↑ → ↓".into(),
                "Move the selected benchmark cell (Benchmark Lab)".into(),
            ));
        }
        shortcut_rows(ui, &rows);
    });
    ui.add_space(12.0);
    // The viewport bindings live here as well as in Viewport & camera: this is
    // where an engineer looks for "how do I pan", and it follows the scheme they
    // actually have selected instead of describing a default they changed.
    section(ui, "3D viewport", |ui| {
        ui.label(
            RichText::new(format!(
                "Mouse mapping follows Settings › Viewport & camera — currently {}.",
                draft.navigation_scheme.label()
            ))
            .text_style(caption())
            .color(TEXT_MUTE),
        );
        ui.add_space(8.0);
        let mut rows: Vec<(String, String)> = draft
            .navigation_scheme
            .mapping()
            .into_iter()
            .map(|(gesture, binding)| (binding.to_owned(), format!("{gesture} the 3D viewport")))
            .collect();
        rows.push((
            "F".into(),
            "Frame the geometry (zoom to fit; the solver domain when nothing is loaded)".into(),
        ));
        for (index, view) in StandardView::ALL.iter().enumerate() {
            rows.push((
                (index + 1).to_string(),
                format!("{} — {}", view.label(), view.detail()),
            ));
        }
        rows.push((
            "Click".into(),
            "Probe the surface under the pointer for local Cp, pressure, and traction".into(),
        ));
        rows.push((
            "Space".into(),
            "Play or pause horizon playback on a completed result".into(),
        ));
        rows.push((
            ", / .".into(),
            "Step the horizon back / forward by one model step".into(),
        ));
        shortcut_rows(ui, &rows);
    });
}

/// Key/description reference rows. The key column is fixed-width so the
/// descriptions align into a readable second column.
fn shortcut_rows(ui: &mut egui::Ui, rows: &[(String, String)]) {
    for (keys, description) in rows {
        ui.horizontal_top(|ui| {
            ui.allocate_ui(egui::vec2(148.0, 18.0), |ui| {
                ui.label(RichText::new(keys).text_style(mono_s()).color(TEXT));
            });
            ui.label(
                RichText::new(description)
                    .text_style(caption())
                    .color(TEXT_DIM),
            );
        });
        ui.add_space(3.0);
    }
}

fn category_storage(ui: &mut egui::Ui, draft: &mut AppSettings) {
    section(ui, "Storage & recovery", |ui| {
        path_setting_row(
            ui,
            "Project directory",
            "Default location for local project bundles; portable content hashes remain authoritative.",
            "project-dir",
            &mut draft.project_directory,
            &AppSettings::default().project_directory,
            PathPick::Folder,
        );
        ui.separator();
        setting_row_reset(
            ui,
            "Autosave interval",
            "Recovery snapshots are separate from explicit project saves.",
            &mut draft.autosave_interval_seconds,
            AppSettings::default().autosave_interval_seconds,
            |ui, value| {
                ui.add(egui::DragValue::new(value).range(30..=3600).suffix(" s"));
            },
        );
    });
}

fn category_signing(
    ui: &mut egui::Ui,
    draft: &mut AppSettings,
    state: &mut SettingsUiState,
    action: &mut Option<SettingsAction>,
) {
    if !cfg!(target_os = "macos") {
        section(ui, "Signing & integrity", |ui| {
            ui.label(
                RichText::new(
                    "Evidence signing is unavailable in the Windows preview. Reyn does not store private keys in an unprotected file or present macOS Keychain controls on Windows.",
                )
                .text_style(body())
                .color(TEXT_DIM),
            );
        });
        return;
    }
    section(ui, "Signing & integrity", |ui| {
        let key_state = if draft.signing_key_is_revoked() {
            ("REVOKED", DATA_RED)
        } else {
            match draft.configured_signing_key() {
                Ok(Some(_)) => ("READY · ED25519", SUCCESS),
                Ok(None) => ("NOT CONFIGURED", WARN),
                Err(_) => ("INVALID REFERENCE", DATA_RED),
            }
        };
        setting_row(
            ui,
            "Organization key",
            "Private bytes stay in the non-synchronizing macOS Keychain. Projects and reports receive only the public key and fingerprint.",
            |ui| {
                if draft.signing_key_reference.trim().is_empty() {
                    if ui.button("Create local key…").clicked() {
                        *action = Some(SettingsAction::CreateSigningKey);
                    }
                } else {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(&draft.signing_key_reference)
                                .text_style(mono_s())
                                .color(TEXT_DIM),
                        );
                        ui.label(
                            RichText::new(short_fingerprint(
                                &draft.signing_key_fingerprint_sha256,
                            ))
                            .text_style(mono_s())
                            .color(TEXT_MUTE),
                        );
                    });
                }
            },
        );
        ui.separator();
        setting_row(
            ui,
            "Authenticity state",
            "A valid signature proves possession of this key. Recipients must compare its fingerprint through an independent channel.",
            |ui| {
                ui.label(
                    chip_text(key_state.0)
                        .color(key_state.1),
                );
            },
        );
        if !draft.signing_key_reference.trim().is_empty() && !draft.signing_key_is_revoked() {
            ui.separator();
            if state.revoke_signing_key_armed {
                ui.label(
                    RichText::new(
                        "Revocation is permanent for this local fingerprint. Existing signatures remain cryptographically valid but must verify as REVOKED when this list is supplied.",
                    )
                    .text_style(caption())
                    .color(DATA_RED),
                );
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        state.revoke_signing_key_armed = false;
                    }
                    if ui.button("Confirm revoke").clicked() {
                        *action = Some(SettingsAction::RevokeSigningKey);
                        state.revoke_signing_key_armed = false;
                    }
                });
            } else if ui.button("Revoke key…").clicked() {
                state.revoke_signing_key_armed = true;
            }
        }
    });
    ui.add_space(12.0);
    section(ui, "Privacy", |ui| {
        setting_row(
            ui,
            "Anonymous telemetry",
            "No analytics endpoint is bundled. Model paths and fields never leave the machine.",
            |ui| {
                draft.telemetry = false;
                ui.label(chip_text("OFF").color(SUCCESS));
            },
        );
    });
}

fn category_developer(ui: &mut egui::Ui, draft: &mut AppSettings) {
    section(ui, "Developer", |ui| {
        setting_row_reset(
            ui,
            "Research Sandbox",
            "Expose procedural 3D prediction, Flow Painter, standalone 2D fields, and Benchmark Lab. These tools are experimental and remain outside engineering case evidence.",
            &mut draft.developer_research_sandbox,
            AppSettings::default().developer_research_sandbox,
            |ui, value| {
                ui.checkbox(value, "Enable");
            },
        );
        if draft.developer_research_sandbox {
            ui.add_space(8.0);
            ui.label(
                RichText::new("Developer mode · sandbox outputs are not case results")
                    .text_style(caption())
                    .color(WARN),
            );
        }
    });
}

/// Honest engine-state color for the runtime rail: derived from the real
/// `engine_ok` flag, never assumed healthy (PRD: no fake state). The status
/// string carries the glyph + words; this pairs the matching semantic hue.
pub fn engine_state_color(engine_ok: bool) -> egui::Color32 {
    if engine_ok {
        SUCCESS
    } else {
        WARN
    }
}

pub fn show_controls(
    ui: &mut egui::Ui,
    saved: &AppSettings,
    engine_status: &str,
    engine_ok: bool,
    notice: Option<&(String, bool)>,
) {
    ui.label(title_text("Runtime"));
    ui.label(
        RichText::new("local engine configuration")
            .size(11.5)
            .color(TEXT_MUTE),
    );
    ui.add_space(16.0);

    if let Some((message, is_error)) = notice {
        // Calm alert recipe (§3.3): tint fill + same-hue hairline; full color
        // only on the leading glyph.
        let hue = if *is_error { DANGER } else { OK };
        let glyph = if *is_error { "!" } else { "✓" };
        Frame::NONE
            .fill(tint_fill(hue))
            .stroke(Stroke::new(1.0, tint_hairline(hue)))
            .corner_radius(CornerRadius::same(R1))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.label(RichText::new(glyph).size(11.0).strong().color(hue));
                    ui.label(RichText::new(message).size(11.0).color(TEXT_DIM));
                });
            });
        ui.add_space(12.0);
    }

    runtime_fact(ui, "ENGINE", engine_status, engine_state_color(engine_ok));
    runtime_fact(ui, "DEVICE POLICY", saved.compute_device.label(), TEXT_DIM);
    runtime_fact(ui, "THEME", saved.theme.label(), TEXT_DIM);
    runtime_fact(
        ui,
        "UNITS",
        match saved.unit_system {
            UnitSystem::Si => "SI",
            UnitSystem::Imperial => "Imperial (display only)",
        },
        TEXT_DIM,
    );
    runtime_fact(ui, "TELEMETRY", "Off", SUCCESS);
    runtime_fact(
        ui,
        "SIGNING KEY",
        if saved.signing_key_is_revoked() {
            "Revoked"
        } else if saved
            .configured_signing_key()
            .is_ok_and(|key| key.is_some())
        {
            "Ready · Ed25519"
        } else if saved.signing_key_reference.trim().is_empty() {
            "Not configured"
        } else {
            "Invalid reference"
        },
        if saved.signing_key_is_revoked() || saved.configured_signing_key().is_err() {
            DATA_RED
        } else if saved
            .configured_signing_key()
            .is_ok_and(|key| key.is_some())
        {
            SUCCESS
        } else {
            WARN
        },
    );
    ui.add_space(14.0);

    Frame::NONE
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, HAIRLINE))
        .corner_radius(CornerRadius::same(R2))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.label(overline_text("Settings file"));
            let path_text = config_path()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "unavailable".into());
            // S8: elide instead of wrapping mid-path; full path on hover.
            let path = ui.add(
                egui::Label::new(
                    RichText::new(&path_text)
                        .text_style(mono_s())
                        .color(TEXT_DIM),
                )
                .truncate()
                .sense(egui::Sense::click()),
            );
            if path.clicked() {
                ui.ctx().copy_text(path_text.clone());
            }
            path.on_hover_text(
                RichText::new(format!("{path_text}\n\nClick to copy full path.")).monospace(),
            );
        });
}

pub fn runtime_changed(saved: &AppSettings, draft: &AppSettings) -> bool {
    saved.compute_device != draft.compute_device
        || saved.python_path != draft.python_path
        || saved.research_dir != draft.research_dir
}

fn section(ui: &mut egui::Ui, section_title: &str, add: impl FnOnce(&mut egui::Ui)) {
    Frame::NONE
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, HAIRLINE))
        .corner_radius(CornerRadius::same(R2))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(title_text(section_title));
            ui.add_space(10.0);
            add(ui);
        });
}

/// Below this row width the label + control pair cannot share a line without
/// overlapping, so rows stack the control under the label instead.
const ROW_STACK_BREAKPOINT: f32 = 400.0;
const PATH_CONTROL_STACK_BREAKPOINT: f32 = 360.0;
const COLLECTION_ROW_STACK_BREAKPOINT: f32 = 440.0;

fn setting_row_stacks(width: f32) -> bool {
    width < ROW_STACK_BREAKPOINT
}

fn path_controls_stack(width: f32) -> bool {
    width < PATH_CONTROL_STACK_BREAKPOINT
}

fn collection_row_stacks(width: f32) -> bool {
    width < COLLECTION_ROW_STACK_BREAKPOINT
}

fn setting_row(ui: &mut egui::Ui, label: &str, detail: &str, control: impl FnOnce(&mut egui::Ui)) {
    // S3: the label column yields to the control at narrow widths instead of
    // overlapping it (min 200px reserved for the control), with row padding.
    ui.add_space(4.0);
    if setting_row_stacks(ui.available_width()) {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.label(RichText::new(label).text_style(body_strong()).color(TEXT));
            ui.label(RichText::new(detail).text_style(caption()).color(TEXT_MUTE));
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), control);
            });
        });
        ui.add_space(4.0);
        return;
    }
    ui.horizontal(|ui| {
        let label_width = (ui.available_width() - 216.0).clamp(140.0, 360.0);
        ui.vertical(|ui| {
            ui.set_max_width(label_width);
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.label(RichText::new(label).text_style(body_strong()).color(TEXT));
            ui.label(RichText::new(detail).text_style(caption()).color(TEXT_MUTE));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), control);
    });
    ui.add_space(4.0);
}

/// A setting row with a per-setting "reset to default" affordance: the ↺
/// appears only while the draft value differs from the shipped default, in a
/// fixed slot so controls stay aligned.
fn setting_row_reset<T: PartialEq>(
    ui: &mut egui::Ui,
    label: &str,
    detail: &str,
    value: &mut T,
    default: T,
    control: impl FnOnce(&mut egui::Ui, &mut T),
) {
    ui.add_space(4.0);
    let control_line = |ui: &mut egui::Ui, value: &mut T| {
        let modified = *value != default;
        let reset = reset_button(ui, modified, label);
        if reset {
            *value = default;
            return;
        }
        control(ui, value);
    };
    if setting_row_stacks(ui.available_width()) {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.label(RichText::new(label).text_style(body_strong()).color(TEXT));
            ui.label(RichText::new(detail).text_style(caption()).color(TEXT_MUTE));
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    control_line(ui, value);
                });
            });
        });
        ui.add_space(4.0);
        return;
    }
    ui.horizontal(|ui| {
        let label_width = (ui.available_width() - 240.0).clamp(140.0, 360.0);
        ui.vertical(|ui| {
            ui.set_max_width(label_width);
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.label(RichText::new(label).text_style(body_strong()).color(TEXT));
            ui.label(RichText::new(detail).text_style(caption()).color(TEXT_MUTE));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            control_line(ui, value);
        });
    });
    ui.add_space(4.0);
}

fn reset_button(ui: &mut egui::Ui, visible: bool, label: &str) -> bool {
    let accessible_label = format!("Reset {label} to default");
    let response = ui.add_visible(
        visible,
        egui::Button::new(
            RichText::new(ph::ARROW_COUNTER_CLOCKWISE)
                .size(13.0)
                .color(TEXT_MUTE),
        )
        .frame(false),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, accessible_label.clone())
    });
    visible && response.on_hover_text(accessible_label).clicked()
}

#[derive(Clone, Copy)]
enum PathPick {
    File,
    Folder,
}

/// A path setting: label and detail on their own line, then the path itself
/// across the full card width. Paths are long and the tail is the informative
/// end, so they get the whole row instead of the ~200px control gutter the
/// two-column rows reserve (QA S8).
fn path_setting_row(
    ui: &mut egui::Ui,
    label: &str,
    detail: &str,
    salt: &str,
    value: &mut String,
    default: &str,
    pick: PathPick,
) {
    ui.add_space(4.0);
    ui.label(RichText::new(label).text_style(body_strong()).color(TEXT));
    ui.add_space(2.0);
    ui.label(RichText::new(detail).text_style(caption()).color(TEXT_MUTE));
    ui.add_space(6.0);
    let stacked = path_controls_stack(ui.available_width());
    if stacked {
        path_field(ui, value, label, salt, ui.available_width().max(80.0));
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                browse_path_button(ui, value, pick);
                ui.add_space(4.0);
                if reset_button(ui, value != default, label) {
                    *value = default.to_owned();
                }
            });
        });
    } else {
        ui.horizontal(|ui| {
            // Browse… is placed first in a right-to-left pass so the field can
            // claim everything left over without a fixed path-width cap.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                browse_path_button(ui, value, pick);
                ui.add_space(4.0);
                if reset_button(ui, value != default, label) {
                    *value = default.to_owned();
                }
                ui.add_space(8.0);
                let width = ui.available_width().max(80.0);
                path_field(ui, value, label, salt, width);
            });
        });
    }
    ui.add_space(6.0);
}

fn browse_path_button(ui: &mut egui::Ui, value: &mut String, pick: PathPick) {
    if ui.button("Browse…").clicked() {
        let dialog = rfd::FileDialog::new();
        let picked = match pick {
            PathPick::File => dialog.pick_file(),
            PathPick::Folder => dialog.pick_folder(),
        };
        if let Some(path) = picked {
            *value = path.display().to_string();
        }
    }
}

/// Click-to-edit path field. Unfocused it paints the value middle-elided with a
/// visible ellipsis (and the full value on hover) so a long path never appears
/// silently cut mid-word; focused it is an ordinary text field.
fn path_field(ui: &mut egui::Ui, value: &mut String, label: &str, salt: &str, width: f32) {
    let edit_id = ui.id().with(salt).with("path-edit");
    let editing = ui.memory(|memory| memory.has_focus(edit_id));
    if editing {
        let response = ui.add_sized(
            egui::vec2(width, 24.0),
            egui::TextEdit::singleline(value)
                .id(edit_id)
                .font(egui::TextStyle::Monospace),
        );
        response.widget_info(|| {
            let mut info = egui::WidgetInfo::text_edit(true, value.as_str(), value.as_str(), "");
            info.label = Some(label.to_owned());
            info
        });
        return;
    }
    let (_, rect) = ui.allocate_space(egui::vec2(width, 24.0));
    let response = ui.interact(rect, edit_id, egui::Sense::click());
    response.widget_info(|| {
        let mut info = egui::WidgetInfo::text_edit(true, value.as_str(), value.as_str(), "");
        info.label = Some(label.to_owned());
        info
    });
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(R1), BG_0);
    painter.rect_stroke(
        rect,
        CornerRadius::same(R1),
        Stroke::new(
            1.0,
            if response.hovered() {
                OUTLINE
            } else {
                OUTLINE_VARIANT
            },
        ),
        egui::StrokeKind::Inside,
    );
    if response.has_focus() {
        painter.rect_stroke(
            rect.expand(1.0),
            CornerRadius::same(R1),
            focus_stroke(),
            egui::StrokeKind::Outside,
        );
    }
    let font = mono_s().resolve(ui.style());
    let inner = rect.width() - 20.0;
    let (shown, color) = if value.trim().is_empty() {
        ("not set".to_owned(), TEXT_MUTE)
    } else {
        (elide_middle(ui, value, inner, font.clone()), TEXT)
    };
    ui.painter().text(
        egui::pos2(rect.min.x + 10.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        shown,
        font,
        color,
    );
    if response.clicked() {
        ui.memory_mut(|memory| memory.request_focus(edit_id));
    }
    let hover = if value.trim().is_empty() {
        "No location set — the system default is used. Click to type a path, or Browse…".to_owned()
    } else {
        value.clone()
    };
    response.on_hover_text(RichText::new(hover).monospace());
}

/// Shorten `text` to fit `width`, dropping from the middle and keeping more of
/// the tail (the file or folder name is the part that identifies a path).
fn elide_middle(ui: &egui::Ui, text: &str, width: f32, font: egui::FontId) -> String {
    let measure = |candidate: &str| {
        ui.painter()
            .layout_no_wrap(candidate.to_owned(), font.clone(), TEXT)
            .size()
            .x
    };
    if width <= 0.0 || measure(text) <= width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let build = |keep: usize| -> String {
        let tail = (keep * 2 / 3).min(chars.len());
        let head = keep.saturating_sub(tail);
        let mut shown: String = chars[..head.min(chars.len())].iter().collect();
        shown.push('…');
        shown.extend(chars[chars.len() - tail..].iter());
        shown
    };
    // Largest keep-count that still fits.
    let (mut low, mut high) = (0usize, chars.len());
    while low < high {
        let mid = (low + high).div_ceil(2);
        if measure(&build(mid)) <= width {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    build(low)
}

/// Three-swatch preview of a theme mode: surface, text, and ember accent.
fn theme_preview(ui: &mut egui::Ui, mode: ThemeMode) {
    let text = match mode {
        ThemeMode::Instrument => TEXT,
        ThemeMode::HighContrast => egui::Color32::WHITE,
    };
    // S5: swatches sit on a darker well with visible outlines so the surface
    // swatch reads instead of vanishing into the card fill.
    let (rect, _) = ui.allocate_exact_size(egui::vec2(60.0, 22.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(4), BG_0);
    painter.rect_stroke(
        rect,
        CornerRadius::same(4),
        Stroke::new(1.0, OUTLINE),
        egui::StrokeKind::Inside,
    );
    for (index, color) in [BG_1, text, EMBER].into_iter().enumerate() {
        let swatch = egui::Rect::from_min_size(
            rect.min + egui::vec2(3.0 + index as f32 * 18.0, 3.0),
            egui::vec2(16.0, 16.0),
        );
        painter.rect_filled(swatch, CornerRadius::same(3), color);
        painter.rect_stroke(
            swatch,
            CornerRadius::same(3),
            Stroke::new(1.0, OUTLINE),
            egui::StrokeKind::Inside,
        );
    }
}

fn runtime_fact(ui: &mut egui::Ui, label: &str, value: &str, color: egui::Color32) {
    ui.label(overline_text(label));
    ui.label(RichText::new(value).text_style(mono_s()).color(color));
    ui.add_space(11.0);
}

fn short_fingerprint(fingerprint: &str) -> String {
    if fingerprint.len() >= 24 {
        format!(
            "{}…{}",
            &fingerprint[..12],
            &fingerprint[fingerprint.len() - 8..]
        )
    } else if fingerprint.is_empty() {
        "fingerprint unavailable".into()
    } else {
        fingerprint.into()
    }
}

fn quarantine_malformed_settings(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "settings path has no parent directory".to_string())?;
    let file_stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("settings");
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("json");
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    for collision in 0..1_000u16 {
        let suffix = if collision == 0 {
            String::new()
        } else {
            format!("-{collision}")
        };
        let recovery_path = parent.join(format!(
            "{file_stem}.malformed-{timestamp}{suffix}.{extension}"
        ));
        if recovery_path.exists() {
            continue;
        }
        std::fs::rename(path, &recovery_path).map_err(|error| {
            format!(
                "could not move {} to {}: {error}",
                path.display(),
                recovery_path.display()
            )
        })?;
        return Ok(recovery_path);
    }
    Err("could not allocate a unique malformed-settings recovery path".into())
}

fn save_to(settings: &AppSettings, path: &Path) -> Result<(), String> {
    if settings
        .protected_malformed_settings_path
        .as_deref()
        .is_some_and(|protected| protected == path && protected.exists())
    {
        return Err(format!(
            "refusing to overwrite malformed settings at {}; move or recover that file first",
            path.display()
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| "settings path has no parent directory".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
}

pub fn encode_case_template(template: &CaseTemplate) -> Result<Vec<u8>, String> {
    template.validate()?;
    let mut bytes = serde_json::to_vec_pretty(template).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn decode_case_template(bytes: &[u8]) -> Result<CaseTemplate, String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| format!("invalid template JSON: {error}"))?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "case template is missing schema_version".to_string())?;
    if version != CASE_TEMPLATE_SCHEMA_VERSION as u64 {
        return Err(format!(
            "unsupported case-template schema {version}; this build supports schema {}",
            CASE_TEMPLATE_SCHEMA_VERSION
        ));
    }
    let template: CaseTemplate =
        serde_json::from_value(value).map_err(|error| format!("invalid case template: {error}"))?;
    template.validate()?;
    Ok(template)
}

pub fn save_case_template(template: &CaseTemplate, path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "case-template path has no parent directory".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = path.with_extension(format!("{CASE_TEMPLATE_EXTENSION}.tmp"));
    std::fs::write(&temporary, encode_case_template(template)?)
        .map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
}

pub fn load_case_template(path: &Path) -> Result<CaseTemplate, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    decode_case_template(&bytes)
}

pub fn case_template_file_name(template: &CaseTemplate) -> String {
    let stem: String = template
        .name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | ' ') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let stem = stem.trim();
    let stem = if stem.is_empty() {
        "Reyn case template"
    } else {
        stem
    };
    format!("{stem}.{CASE_TEMPLATE_EXTENSION}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(
        not(target_os = "windows"),
        ignore = "requires the Windows Known Folder API"
    )]
    fn windows_known_folder_api_returns_a_usable_native_path() {
        #[cfg(target_os = "windows")]
        {
            let documents = windows_documents_directory()
                .expect("SHGetKnownFolderPath should return the current Documents folder");
            assert!(
                documents.is_absolute(),
                "Documents Known Folder path must be absolute: {documents:?}"
            );

            let project_directory = project_directory_from(Some(documents.clone()), None);
            assert!(
                project_directory.is_absolute(),
                "derived project directory must stay absolute: {project_directory:?}"
            );
            assert_eq!(project_directory.parent(), Some(documents.as_path()));
            assert_eq!(
                project_directory.file_name(),
                Some(std::ffi::OsStr::new("Reyn Studio Projects"))
            );
        }
    }

    #[test]
    fn windows_project_directory_prefers_known_folder_and_has_safe_fallbacks() {
        let documents = PathBuf::from(r"C:\Users\Renée Doe\Cloud Documents");
        let profile = PathBuf::from(r"C:\Users\Renée Doe");
        assert_eq!(
            project_directory_from(Some(documents.clone()), Some(r"C:\Users\ignored".into()),),
            documents.join("Reyn Studio Projects")
        );
        assert_eq!(
            project_directory_from(None, Some(profile.clone().into_os_string())),
            profile.join("Documents").join("Reyn Studio Projects")
        );
        assert_eq!(
            project_directory_from(None, None),
            PathBuf::from("Reyn Studio Projects")
        );
        assert_eq!(
            shortcut_labels(false),
            (
                "Ctrl+Z",
                "Ctrl+Shift+Z / Ctrl+Y",
                "Ctrl+",
                "Ctrl+Shift+",
                "Ctrl+W / Alt+F4",
            )
        );
    }

    #[test]
    fn settings_layout_protects_narrow_content_widths() {
        assert_eq!(
            settings_body_layout(CATEGORY_PICKER_BREAKPOINT - 1.0),
            SettingsBodyLayout::CategoryPicker
        );
        assert_eq!(
            settings_body_layout(CATEGORY_PICKER_BREAKPOINT),
            SettingsBodyLayout::CategoryRail
        );
        assert!(setting_row_stacks(ROW_STACK_BREAKPOINT - 1.0));
        assert!(!setting_row_stacks(ROW_STACK_BREAKPOINT));
        assert!(path_controls_stack(PATH_CONTROL_STACK_BREAKPOINT - 1.0));
        assert!(!path_controls_stack(PATH_CONTROL_STACK_BREAKPOINT));
        assert!(collection_row_stacks(COLLECTION_ROW_STACK_BREAKPOINT - 1.0));
        assert!(!collection_row_stacks(COLLECTION_ROW_STACK_BREAKPOINT));
    }

    #[test]
    fn restore_confirmation_reserves_a_second_footer_line() {
        assert!(settings_footer_height(true) > settings_footer_height(false));
        assert_eq!(settings_footer_height(false), 64.0);
        assert_eq!(save_disabled_reason(false), Some(SAVE_DISABLED_REASON));
        assert_eq!(save_disabled_reason(true), None);
    }

    #[test]
    fn focused_category_rows_activate_from_enter_and_space() {
        for (key, target) in [
            (egui::Key::Enter, SettingsCategory::Workflow),
            (egui::Key::Space, SettingsCategory::Signing),
        ] {
            let context = egui::Context::default();
            crate::fonts::install(&context);
            crate::theme::apply(&context);
            let mut state = SettingsUiState {
                category: SettingsCategory::Compute,
                confirm_restore_defaults: false,
                revoke_signing_key_armed: false,
                preset_delete_armed: None,
                template_delete_armed: None,
                qa_focus_category: None,
                qa_scroll_bottom: false,
            };
            let target_id = category_row_id(target);
            context.memory_mut(|memory| memory.request_focus(target_id));
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(240.0, 420.0),
                )),
                events: vec![egui::Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            };
            let _ = context.run_ui(input, |ui| {
                category_rail(ui, &mut state);
            });
            assert_eq!(state.category, target);
            assert_eq!(context.memory(|memory| memory.focused()), Some(target_id));
        }
    }

    #[test]
    fn adverse_capture_fixture_covers_populated_and_invalid_states() {
        let settings: AppSettings =
            serde_json::from_str(include_str!("../docs/qa/settings-adverse/settings.json"))
                .unwrap();
        assert!(settings.operating_presets[0].name.chars().count() > 100);
        assert!(settings.case_templates[0].name.chars().count() > 100);
        assert!(settings.case_templates[0].validate().is_ok());
        assert!(settings.case_templates[1]
            .validate()
            .unwrap_err()
            .contains("viscosity must be positive"));
    }

    #[test]
    fn populated_workflow_rows_render_narrow_and_wide_without_expanding() {
        for width in [320.0, 720.0] {
            let context = egui::Context::default();
            crate::fonts::install(&context);
            crate::theme::apply(&context);
            let mut settings: AppSettings =
                serde_json::from_str(include_str!("../docs/qa/settings-adverse/settings.json"))
                    .unwrap();
            let mut state = SettingsUiState {
                category: SettingsCategory::Workflow,
                confirm_restore_defaults: false,
                revoke_signing_key_armed: false,
                preset_delete_armed: Some(0),
                template_delete_armed: Some(0),
                qa_focus_category: None,
                qa_scroll_bottom: false,
            };
            let mut action = None;
            let mut occupied = egui::Rect::NOTHING;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 1_400.0),
                )),
                ..Default::default()
            };
            let _ = context.run_ui(input, |ui| {
                ui.set_width(width);
                category_workflow(ui, &mut settings, &mut state, &mut action);
                occupied = ui.min_rect();
            });
            assert!(
                occupied.width() <= width + 0.5,
                "workflow content expanded to {} at {width}px",
                occupied.width()
            );
            assert!(action.is_none());
            assert_eq!(state.preset_delete_armed, Some(0));
            assert_eq!(state.template_delete_armed, Some(0));
        }
    }

    /// Every settings category stays reachable through the QA deep-link
    /// (REYN_STUDIO_START_NAV=settings:<id>).
    #[test]
    fn every_settings_category_has_a_qa_deep_link_id() {
        let ids = [
            "compute",
            "units",
            "appearance",
            "viewport",
            "workflow",
            "shortcuts",
            "storage",
            "updates",
            "signing",
            "developer",
        ];
        assert_eq!(ids.len(), SettingsCategory::ALL.len());
        for (id, category) in ids.iter().zip(SettingsCategory::ALL) {
            assert_eq!(SettingsCategory::from_qa_id(id), Some(category));
        }
        assert_eq!(SettingsCategory::from_qa_id("nonsense"), None);
    }

    /// Honesty guard (PRD: no fake state): the runtime rail's engine color
    /// must track the real `engine_ok` flag — never green while down.
    #[test]
    fn engine_state_color_is_never_green_when_engine_is_down() {
        assert_eq!(engine_state_color(true), SUCCESS);
        assert_eq!(engine_state_color(false), WARN);
        assert_ne!(engine_state_color(false), SUCCESS);
    }

    #[test]
    fn settings_round_trip_keeps_telemetry_off() {
        let root = std::env::temp_dir().join(format!(
            "reyn-settings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("settings.json");
        let settings = AppSettings {
            compute_device: ComputeDevice::Cpu,
            python_path: "/usr/bin/python3".into(),
            research_dir: "/tmp/reyn-research".into(),
            project_directory: "/tmp/reyn-projects".into(),
            autosave_interval_seconds: 300,
            theme: ThemeMode::HighContrast,
            reduced_motion: true,
            telemetry: false,
            developer_research_sandbox: true,
            signing_key_reference: "org-key-2026".into(),
            unit_system: UnitSystem::Imperial,
            significant_digits: 6,
            number_notation: NumberNotation::Scientific,
            input_units: InputUnitPrefs {
                velocity: units::VelocityUnit::MilesPerHour,
                pressure: units::PressureUnit::Psi,
                density: units::DensityUnit::PoundsMassPerCubicFoot,
                viscosity: units::ViscosityUnit::MillipascalSecond,
            },
            ui_scale: 1.2,
            colormap: FieldColormap::Viridis,
            cp_range_mode: CpRangeMode::Pinned,
            cp_pinned_extent: 2.0,
            default_section_axis: SectionAxis::Z,
            default_section_quantity: SectionQuantity::VelocityMagnitude,
            orbit_sensitivity: 1.4,
            invert_scroll_zoom: true,
            show_domain_bounds: false,
            show_viewport_hints: false,
            viewport_background: ViewportBackground::Canvas,
            default_horizon_steps: 8,
            default_export_directory: "/tmp".into(),
            operating_presets: vec![OperatingPointPreset {
                name: "Tunnel B · 22 m/s".into(),
                velocity_mps: 22.0,
                density_kg_m3: 1.2,
                viscosity_pa_s: 1.8e-5,
                reference_pressure_pa: 101_000.0,
            }],
            case_templates: vec![CaseTemplate::from_draft(
                "Tunnel B review",
                &OperatingPoint {
                    velocity: 22.0,
                    density: 1.2,
                    viscosity: 1.8e-5,
                    reference_pressure: 101_000.0,
                    horizon_steps: 8,
                    ..OperatingPoint::default()
                },
                SectionAxis::Z,
                SectionQuantity::VelocityMagnitude,
            )],
            ..AppSettings::default()
        };
        save_to(&settings, &path).unwrap();
        let loaded: AppSettings = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(loaded, settings);
        assert!(!loaded.telemetry);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_settings_are_quarantined_before_defaults_can_save() {
        let root = std::env::temp_dir().join(format!(
            "reyn-settings-malformed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("settings.json");
        std::fs::create_dir_all(&root).unwrap();
        let malformed = br#"{"theme":"instrument","unfinished":"#;
        std::fs::write(&path, malformed).unwrap();

        let (defaults, warning) = AppSettings::load_from_path(&path);
        assert!(warning
            .expect("malformed settings warning")
            .contains("preserved"));
        assert!(!path.exists());
        let recovery_paths = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|candidate| {
                candidate
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("settings.malformed-"))
            })
            .collect::<Vec<_>>();
        assert_eq!(recovery_paths.len(), 1);
        assert_eq!(std::fs::read(&recovery_paths[0]).unwrap(), malformed);

        save_to(&defaults, &path).unwrap();
        assert!(path.is_file());
        assert_eq!(std::fs::read(&recovery_paths[0]).unwrap(), malformed);

        let mut protected = AppSettings::default();
        protected.protected_malformed_settings_path = Some(path.clone());
        assert!(save_to(&protected, &path)
            .unwrap_err()
            .contains("refusing to overwrite malformed settings"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_partial_settings_receive_safe_defaults() {
        let settings: AppSettings =
            serde_json::from_str(r#"{"python_path":"python3","research_dir":"/tmp"}"#).unwrap();
        assert_eq!(settings.compute_device, ComputeDevice::Auto);
        assert_eq!(settings.theme, ThemeMode::Instrument);
        assert!(!settings.reduced_motion);
        assert!(!settings.telemetry);
        assert!(!settings.developer_research_sandbox);
        assert_eq!(settings.autosave_interval_seconds, 120);
        assert!(settings.signing_key_reference.is_empty());
        assert!(settings.signing_public_key_base64.is_empty());
        assert!(settings.signing_key_fingerprint_sha256.is_empty());
        assert!(settings.revoked_signing_key_fingerprints.is_empty());
        // New preference groups all arrive at their shipped defaults.
        assert_eq!(settings.unit_system, UnitSystem::Si);
        assert_eq!(settings.significant_digits, 5);
        assert_eq!(settings.number_notation, NumberNotation::Auto);
        assert_eq!(settings.input_units, InputUnitPrefs::default());
        assert_eq!(settings.ui_scale, 1.0);
        assert_eq!(settings.colormap, FieldColormap::Ember);
        assert_eq!(settings.cp_range_mode, CpRangeMode::Auto);
        assert_eq!(settings.default_section_axis, SectionAxis::X);
        assert_eq!(
            settings.default_section_quantity,
            SectionQuantity::PhysicalCp
        );
        assert_eq!(settings.orbit_sensitivity, 1.0);
        assert!(!settings.invert_scroll_zoom);
        assert!(settings.show_domain_bounds);
        assert!(settings.show_viewport_hints);
        assert_eq!(
            settings.viewport_background,
            ViewportBackground::InstrumentWell
        );
        assert_eq!(settings.default_horizon_steps, 4);
        assert_eq!(settings.default_3d_model, DEFAULT_3D_MODEL_ID);
        assert_eq!(settings.default_2d_model, DEFAULT_2D_MODEL_ID);
        assert!(settings.default_export_directory.is_empty());
        assert!(settings.operating_presets.is_empty());
        assert!(settings.case_templates.is_empty());
    }

    #[test]
    fn persisted_legacy_model_defaults_migrate_without_loading_pickle() {
        let root = std::env::temp_dir().join(format!(
            "reyn-settings-model-migration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("settings.json");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            &path,
            r#"{
                "python_path": "python3",
                "research_dir": "/tmp",
                "current_model": "flow3d_obs_v1.pth",
                "f2d_model": "reyn_models/obstacle_v2_shapes.PTH"
            }"#,
        )
        .unwrap();

        let (settings, warning) = AppSettings::load_from_path(&path);
        assert_eq!(settings.default_3d_model, DEFAULT_3D_MODEL_ID);
        assert_eq!(
            settings.default_2d_model,
            "reyn_models/obstacle_v2_shapes.reynmodel"
        );
        let warning = warning.expect("migration guidance");
        assert!(warning.contains("never opened"));
        assert!(warning.contains("convert_model_bundle.py"));
        assert!(warning.contains(".reynmodel.sig"));

        save_to(&settings, &path).unwrap();
        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(persisted.contains(DEFAULT_3D_MODEL_ID));
        assert!(!persisted.contains(".pth"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn normalize_clamps_out_of_range_preferences() {
        let mut settings = AppSettings {
            significant_digits: 42,
            ui_scale: 9.0,
            orbit_sensitivity: f32::NAN,
            cp_pinned_extent: -3.0,
            default_horizon_steps: 0,
            autosave_interval_seconds: 1,
            operating_presets: vec![OperatingPointPreset::default()],
            ..AppSettings::default()
        };
        settings.normalize();
        assert_eq!(settings.significant_digits, units::MAX_SIGNIFICANT_DIGITS);
        assert_eq!(settings.ui_scale, 1.4);
        assert_eq!(settings.orbit_sensitivity, 1.0);
        assert_eq!(settings.cp_pinned_extent, 1.5);
        assert_eq!(settings.default_horizon_steps, 1);
        assert_eq!(settings.autosave_interval_seconds, 30);
        assert!(
            settings.operating_presets.is_empty(),
            "nameless presets dropped"
        );
    }

    #[test]
    fn runtime_restart_is_only_requested_for_engine_changes() {
        let saved = AppSettings::default();
        let mut draft = saved.clone();
        draft.theme = ThemeMode::HighContrast;
        draft.unit_system = UnitSystem::Imperial;
        draft.ui_scale = 1.2;
        assert!(!runtime_changed(&saved, &draft));
        draft.compute_device = ComputeDevice::Cpu;
        assert!(runtime_changed(&saved, &draft));
    }

    #[test]
    fn storage_and_signing_state_persist_without_engine_restart() {
        let saved = AppSettings::default();
        let mut draft = saved.clone();
        draft.project_directory = "/tmp/portable-reyn-projects".into();
        draft.autosave_interval_seconds = 45;
        draft.signing_key_reference = "verification-key-a".into();
        draft.developer_research_sandbox = true;
        assert_ne!(draft, saved);
        assert!(!runtime_changed(&saved, &draft));

        let json = serde_json::to_string(&draft).unwrap();
        let loaded: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.project_directory, draft.project_directory);
        assert_eq!(loaded.autosave_interval_seconds, 45);
        assert_eq!(loaded.signing_key_reference, "verification-key-a");
        assert!(loaded.developer_research_sandbox);
        assert!(loaded.configured_signing_key().is_err());
    }

    #[test]
    fn public_key_settings_round_trip_and_revocation_blocks_signing_state() {
        use base64::Engine as _;
        use sha2::Digest as _;

        let provider = crate::signing::DeterministicTestProvider::new("settings-public");
        let key = provider.public_key_record();
        let mut settings = AppSettings::default();
        settings.set_signing_key(&key);
        assert_eq!(
            settings.configured_signing_key().unwrap(),
            Some(key.clone())
        );
        let json = serde_json::to_string_pretty(&settings).unwrap();
        let seed: [u8; 32] =
            sha2::Sha256::digest(b"reyn.test-only.ed25519.v1\0settings-public").into();
        assert!(!json.contains(&base64::engine::general_purpose::STANDARD.encode(seed)));
        assert!(!json.contains(
            &seed
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        assert!(json.contains(&key.public_key_base64));

        let mut restored: AppSettings = serde_json::from_str(&json).unwrap();
        restored.revoke_signing_key();
        assert!(restored.signing_key_is_revoked());
        assert!(restored.configured_signing_key().is_err());
    }

    #[test]
    fn portable_case_template_round_trips_and_seeds_only_owned_defaults() {
        let mut operating = OperatingPoint {
            length_unit: crate::engineering::LengthUnit::Millimeter,
            reference_length: 420.0,
            velocity: 12.0,
            density: 1.18,
            viscosity: 1.9e-5,
            reference_pressure: 100_800.0,
            flow_direction: [1.0, 0.0, 0.0],
            horizon_steps: 12,
            ..OperatingPoint::default()
        };
        let template = CaseTemplate::from_draft(
            "Aero review",
            &operating,
            SectionAxis::Y,
            SectionQuantity::PhysicalCp,
        );
        let bytes = encode_case_template(&template).unwrap();
        let restored = decode_case_template(&bytes).unwrap();
        assert_eq!(restored, template);
        let json = String::from_utf8(bytes).unwrap();
        for forbidden in [
            "source_sha256",
            "model_sha256",
            "transform_4x4",
            "waivers",
            "run_id",
            "evidence",
            "reference_length",
            "flow_direction",
        ] {
            assert!(
                !json.contains(forbidden),
                "{forbidden} leaked into template"
            );
        }

        operating.length_unit = crate::engineering::LengthUnit::Foot;
        operating.reference_length = 7.5;
        operating.flow_direction = [1.0, 0.0, 0.0];
        operating.velocity = 3.0;
        operating.horizon_steps = 2;
        assert!(restored.apply_to(&mut operating, 8).unwrap());
        assert_eq!(operating.velocity, 12.0);
        assert_eq!(operating.horizon_steps, 8, "model support clamps defaults");
        assert_eq!(operating.length_unit, crate::engineering::LengthUnit::Foot);
        assert_eq!(operating.reference_length, 7.5);
        assert_eq!(operating.flow_direction, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn case_template_import_rejects_future_schema_and_invalid_defaults() {
        let future = br#"{
            "schema_version": 99,
            "name": "Future",
            "operating": {
                "velocity_mps": 1.0,
                "density_kg_m3": 1.0,
                "viscosity_pa_s": 0.001,
                "reference_pressure_pa": 101325.0,
                "horizon_steps": 4
            },
            "preferred_view": {
                "section_axis": "x",
                "section_quantity": "physical_cp"
            }
        }"#;
        assert!(decode_case_template(future)
            .unwrap_err()
            .contains("unsupported case-template schema 99"));

        let mut template = CaseTemplate::from_draft(
            "Invalid",
            &OperatingPoint::default(),
            SectionAxis::X,
            SectionQuantity::PhysicalCp,
        );
        template.operating.viscosity_pa_s = 0.0;
        assert!(encode_case_template(&template)
            .unwrap_err()
            .contains("viscosity must be positive"));
    }
}
