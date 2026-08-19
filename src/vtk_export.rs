//! Neutral, ParaView-readable export of immutable external-flow field evidence.
//!
//! The legacy VTK `STRUCTURED_GRID` writer is intentionally streaming: a 64³
//! field is written point-by-point and array-by-array instead of first becoming
//! one large formatted `String`. Grid points and vector components are mapped
//! from solver axes into the approved imported-source frame.

use crate::engineering::{self, EngineeringFieldBlob, EXTERNAL_FLOW_CONTRACT};
use crate::project::LifecycleState;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const VTK_FIELD_SCHEMA: &str = "reyn.engineering_field.legacy_vtk_structured_grid.v1";
pub const COORDINATE_FRAME: &str = "approved_imported_source_frame";
pub const VECTOR_FRAME: &str = "approved_imported_source_frame_cartesian";
pub const VELOCITY_METHOD: &str = "DirectFlowMap fixed-body model prediction";
pub const PRESSURE_METHOD: &str = "pressure recovered from model-predicted velocity";
pub const CP_METHOD: &str = "Cp=(p_recovered-p_inf)/(0.5*rho_inf*V_inf^2)";
pub const OCCUPANCY_METHOD: &str = "three-axis majority-vote voxel occupancy v2";

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VtkFieldProvenance {
    pub source_revision_id: String,
    pub case_revision_id: String,
    pub run_id: String,
    pub model_sha256: String,
    pub contract_kind: String,
    pub field_sha256: String,
    pub traction_method: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VtkFieldExport {
    pub field: EngineeringFieldBlob,
    /// Approved column-major source-to-solver affine transform.
    pub source_to_solver_transform_4x4: [f64; 16],
    pub meters_per_source_unit: f64,
    pub transform_approved: bool,
    pub run_state: LifecycleState,
    pub provenance: VtkFieldProvenance,
}

#[derive(Clone, Copy, Debug)]
struct ValidatedGrid {
    n: usize,
    points: usize,
    frame: SourceFrameMap,
}

/// Affine solver-point mapping plus rotation-only vector mapping, both derived
/// from the same approved inverse transform used by the engineering workflow.
#[derive(Clone, Copy, Debug)]
struct SourceFrameMap {
    origin_m: [f64; 3],
    solver_basis_m: [[f64; 3]; 3],
    solver_basis_direction: [[f64; 3]; 3],
}

impl SourceFrameMap {
    fn from_approved_transform(
        transform: [f64; 16],
        meters_per_source_unit: f64,
    ) -> Result<Self, String> {
        if transform[3].abs() > 1e-12
            || transform[7].abs() > 1e-12
            || transform[11].abs() > 1e-12
            || (transform[15] - 1.0).abs() > 1e-12
        {
            return Err(
                "VTK export requires an affine source-to-solver transform with [0,0,0,1] homogeneous row."
                    .into(),
            );
        }
        let origin_m =
            engineering::solver_point_to_source_m([0.0; 3], transform, meters_per_source_unit)?;
        let mut solver_basis_m = [[0.0; 3]; 3];
        let mut lengths = [0.0; 3];
        for solver_axis in 0..3 {
            let mut unit = [0.0; 3];
            unit[solver_axis] = 1.0;
            let mapped =
                engineering::solver_point_to_source_m(unit, transform, meters_per_source_unit)?;
            solver_basis_m[solver_axis] = std::array::from_fn(|axis| mapped[axis] - origin_m[axis]);
            lengths[solver_axis] = solver_basis_m[solver_axis]
                .iter()
                .map(|component| component * component)
                .sum::<f64>()
                .sqrt();
        }
        if lengths
            .iter()
            .any(|length| !length.is_finite() || *length <= 1e-18)
        {
            return Err("VTK export requires a finite, invertible source transform.".into());
        }
        let mean_length = lengths.iter().sum::<f64>() / 3.0;
        if lengths
            .iter()
            .any(|length| (length - mean_length).abs() > mean_length * 1e-8)
        {
            return Err(
                "VTK export requires the approved source transform to use isotropic scale.".into(),
            );
        }
        let solver_basis_direction = std::array::from_fn(|solver_axis| {
            solver_basis_m[solver_axis].map(|component| component / lengths[solver_axis])
        });
        for left in 0..3 {
            for right in left + 1..3 {
                let dot = (0..3)
                    .map(|axis| {
                        solver_basis_direction[left][axis] * solver_basis_direction[right][axis]
                    })
                    .sum::<f64>();
                if dot.abs() > 1e-8 {
                    return Err(
                        "VTK export requires an orthogonal rotation in the approved source transform."
                            .into(),
                    );
                }
            }
        }
        let determinant = solver_basis_direction[0][0]
            * (solver_basis_direction[1][1] * solver_basis_direction[2][2]
                - solver_basis_direction[1][2] * solver_basis_direction[2][1])
            - solver_basis_direction[0][1]
                * (solver_basis_direction[1][0] * solver_basis_direction[2][2]
                    - solver_basis_direction[1][2] * solver_basis_direction[2][0])
            + solver_basis_direction[0][2]
                * (solver_basis_direction[1][0] * solver_basis_direction[2][1]
                    - solver_basis_direction[1][1] * solver_basis_direction[2][0]);
        if (determinant - 1.0).abs() > 1e-8 {
            return Err(
                "VTK export requires a right-handed rotation; reflected source transforms are unsupported."
                    .into(),
            );
        }
        Ok(Self {
            origin_m,
            solver_basis_m,
            solver_basis_direction,
        })
    }

    fn point_m(self, solver_point: [f64; 3]) -> [f64; 3] {
        std::array::from_fn(|source_axis| {
            self.origin_m[source_axis]
                + (0..3)
                    .map(|solver_axis| {
                        self.solver_basis_m[solver_axis][source_axis] * solver_point[solver_axis]
                    })
                    .sum::<f64>()
        })
    }

    fn vector(self, solver_vector: [f64; 3]) -> [f64; 3] {
        std::array::from_fn(|source_axis| {
            (0..3)
                .map(|solver_axis| {
                    self.solver_basis_direction[solver_axis][source_axis]
                        * solver_vector[solver_axis]
                })
                .sum()
        })
    }
}

impl VtkFieldExport {
    fn validate(&self) -> Result<ValidatedGrid, String> {
        if !matches!(
            self.run_state,
            LifecycleState::Succeeded | LifecycleState::EvidenceLocked
        ) {
            return Err(
                "VTK evidence export requires a completed persisted run; draft, running, stale, and failed runs are rejected."
                    .into(),
            );
        }
        if !self.transform_approved {
            return Err("VTK evidence export requires the approved source transform.".into());
        }
        if self.provenance.contract_kind != EXTERNAL_FLOW_CONTRACT {
            return Err("VTK evidence export supports only the external-flow contract.".into());
        }
        for (label, value) in [
            (
                "source revision",
                self.provenance.source_revision_id.as_str(),
            ),
            ("case revision", self.provenance.case_revision_id.as_str()),
            ("run", self.provenance.run_id.as_str()),
            ("contract", self.provenance.contract_kind.as_str()),
            ("traction method", self.provenance.traction_method.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(format!("VTK evidence export requires a persisted {label}."));
            }
        }
        validate_sha256("model", &self.provenance.model_sha256)?;
        validate_sha256("field", &self.provenance.field_sha256)?;
        if !self.meters_per_source_unit.is_finite() || self.meters_per_source_unit <= 0.0 {
            return Err(
                "VTK evidence export requires a finite positive source-unit conversion.".into(),
            );
        }
        let n = self.field.n;
        let points = n
            .checked_mul(n)
            .and_then(|value| value.checked_mul(n))
            .ok_or_else(|| "VTK field dimensions overflow.".to_string())?;
        let vector_values = points
            .checked_mul(3)
            .ok_or_else(|| "VTK vector array dimensions overflow.".to_string())?;
        if n < 3 {
            return Err(format!(
                "VTK field grid dimension {n} is invalid; each axis requires at least 3 points."
            ));
        }
        for (label, actual, expected) in [
            ("velocity", self.field.velocity.len(), vector_values),
            ("recovered pressure", self.field.pressure_pa.len(), points),
            ("Cp", self.field.cp.len(), points),
            ("traction", self.field.traction_pa.len(), vector_values),
            ("solid occupancy", self.field.mask.len(), points),
        ] {
            if actual != expected {
                return Err(format!(
                    "VTK {label} array length {actual} does not match expected length {expected} for a {n}³ grid."
                ));
            }
        }
        let actual_field_sha256 = validate_and_hash_field(&self.field, points)?;
        if actual_field_sha256 != self.provenance.field_sha256 {
            return Err(format!(
                "VTK persisted field SHA-256 does not match the selected field payload (expected {}, computed {}). Reload the immutable run evidence before exporting.",
                self.provenance.field_sha256, actual_field_sha256
            ));
        }
        let frame = SourceFrameMap::from_approved_transform(
            self.source_to_solver_transform_4x4,
            self.meters_per_source_unit,
        )?;
        Ok(ValidatedGrid { n, points, frame })
    }
}

fn validate_sha256(label: &str, digest: &str) -> Result<(), String> {
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(format!(
            "VTK evidence export requires a canonical lowercase {label} SHA-256."
        ))
    }
}

fn validate_and_hash_field(field: &EngineeringFieldBlob, points: usize) -> Result<String, String> {
    let n = u32::try_from(field.n)
        .map_err(|_| "VTK field grid dimension cannot be represented by its persisted schema.")?;
    let values = points
        .checked_mul(9)
        .ok_or_else(|| "VTK persisted field value count overflows.".to_string())?;
    let values = u32::try_from(values)
        .map_err(|_| "VTK field payload is too large for its persisted schema.")?;
    let mut hasher = Sha256::new();
    hasher.update(b"REYNENG1");
    hasher.update(n.to_le_bytes());
    hasher.update(values.to_le_bytes());
    for (label, values) in [
        ("velocity", field.velocity.as_slice()),
        ("recovered pressure", field.pressure_pa.as_slice()),
        ("solid occupancy", field.mask.as_slice()),
        ("Cp", field.cp.as_slice()),
        ("traction", field.traction_pa.as_slice()),
    ] {
        for (index, value) in values.iter().enumerate() {
            if !value.is_finite() {
                return Err(format!(
                    "VTK {label} array contains a non-finite value at persisted index {index}."
                ));
            }
            if label == "solid occupancy" && !(0.0..=1.0).contains(value) {
                return Err(format!(
                    "VTK solid occupancy at persisted index {index} is {value}; values must lie in [0,1]."
                ));
            }
            hasher.update(value.to_le_bytes());
        }
    }
    let mut hex = String::with_capacity(64);
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in hasher.finalize() {
        hex.push(DIGITS[(byte >> 4) as usize] as char);
        hex.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    Ok(hex)
}

/// Stable, provenance-bearing default name for the selected immutable run.
pub fn default_file_name(case_name: &str, run_id: &str, model_sha256: &str) -> String {
    let case = file_token(case_name, "case");
    let run = file_token(run_id, "run");
    let model = model_sha256
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase();
    let model = if model.is_empty() {
        "unknown".to_owned()
    } else {
        model
    };
    format!("{case}__{run}__model-{model}__fields.vtk")
}

fn file_token(value: &str, fallback: &str) -> String {
    let mut token = String::new();
    let mut previous_separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
            token.push(character);
            previous_separator = false;
        } else if !previous_separator && !token.is_empty() {
            token.push('_');
            previous_separator = true;
        }
    }
    while token.ends_with('_') {
        token.pop();
    }
    if token.is_empty() {
        fallback.to_owned()
    } else {
        token
    }
}

/// Write an ASCII legacy VTK StructuredGrid without buffering the formatted
/// dataset in memory. VTK point order is x-fastest; source arrays are reordered
/// from Reyn's z-fastest storage with the same index applied to every quantity.
#[cfg_attr(not(test), allow(dead_code))]
pub fn write_legacy_vtk<W: Write>(writer: &mut W, export: &VtkFieldExport) -> Result<(), String> {
    let grid = export.validate()?;
    write_validated(writer, export, grid)
        .map_err(|error| format!("VTK field export write failed: {error}"))
}

fn write_validated<W: Write>(
    writer: &mut W,
    export: &VtkFieldExport,
    grid: ValidatedGrid,
) -> std::io::Result<()> {
    writeln!(writer, "# vtk DataFile Version 3.0")?;
    writeln!(writer, "Reyn Studio immutable engineering field evidence")?;
    writeln!(writer, "ASCII")?;
    writeln!(writer, "DATASET STRUCTURED_GRID")?;
    writeln!(writer, "DIMENSIONS {} {} {}", grid.n, grid.n, grid.n)?;
    writeln!(writer, "POINTS {} double", grid.points)?;
    let dx_solver = std::f64::consts::TAU / grid.n as f64;
    for k in 0..grid.n {
        for j in 0..grid.n {
            for i in 0..grid.n {
                let source = grid.frame.point_m([
                    (i as f64 + 0.5) * dx_solver,
                    (j as f64 + 0.5) * dx_solver,
                    (k as f64 + 0.5) * dx_solver,
                ]);
                writeln!(
                    writer,
                    "{:.17e} {:.17e} {:.17e}",
                    source[0], source[1], source[2]
                )?;
            }
        }
    }

    let text_fields = [
        ("reyn_schema", VTK_FIELD_SCHEMA),
        (
            "source_revision_id",
            export.provenance.source_revision_id.as_str(),
        ),
        (
            "case_revision_id",
            export.provenance.case_revision_id.as_str(),
        ),
        ("run_id", export.provenance.run_id.as_str()),
        ("model_sha256", export.provenance.model_sha256.as_str()),
        ("contract_kind", export.provenance.contract_kind.as_str()),
        ("field_sha256", export.provenance.field_sha256.as_str()),
        ("run_state", run_state_name(export.run_state)),
        ("coordinate_frame", COORDINATE_FRAME),
        ("coordinate_units", "m"),
        ("vector_frame", VECTOR_FRAME),
        (
            "point_sampling",
            "solver_cell_centers_mapped_through_approved_inverse_transform",
        ),
        ("vtk_point_order", "x_fastest_then_y_then_z"),
        ("reyn_scalar_storage_order", "z_fastest_then_y_then_x"),
        (
            "reyn_vector_storage_order",
            "component_major_then_z_fastest_then_y_then_x",
        ),
        ("velocity_units", "m/s"),
        ("velocity_source_class", "model_prediction"),
        ("velocity_method", VELOCITY_METHOD),
        ("recovered_pressure_units", "Pa"),
        ("recovered_pressure_source_class", "recovered"),
        ("recovered_pressure_method", PRESSURE_METHOD),
        ("cp_units", "1"),
        ("cp_source_class", "derived"),
        ("cp_method", CP_METHOD),
        ("traction_units", "Pa"),
        ("traction_source_class", "derived"),
        (
            "traction_method",
            export.provenance.traction_method.as_str(),
        ),
        ("solid_occupancy_units", "1"),
        ("solid_occupancy_source_class", "derived"),
        ("solid_occupancy_method", OCCUPANCY_METHOD),
    ];
    writeln!(writer, "FIELD FieldData {}", text_fields.len() + 3)?;
    for (name, value) in text_fields {
        write_byte_field(writer, name, value)?;
    }
    writeln!(writer, "grid_dimensions 3 1 int")?;
    writeln!(writer, "{} {} {}", grid.n, grid.n, grid.n)?;
    writeln!(writer, "meters_per_source_unit 1 1 double")?;
    writeln!(writer, "{:.17e}", export.meters_per_source_unit)?;
    writeln!(writer, "source_to_solver_transform_4x4 16 1 double")?;
    for (index, value) in export.source_to_solver_transform_4x4.iter().enumerate() {
        write!(writer, "{value:.17e}")?;
        if index == 15 {
            writeln!(writer)?;
        } else {
            write!(writer, " ")?;
        }
    }

    writeln!(writer, "POINT_DATA {}", grid.points)?;
    writeln!(writer, "VECTORS velocity_m_per_s double")?;
    write_vector_array(writer, grid, &export.field.velocity)?;
    writeln!(writer, "SCALARS recovered_pressure_pa float 1")?;
    writeln!(writer, "LOOKUP_TABLE default")?;
    write_scalar_array(writer, grid, &export.field.pressure_pa)?;
    writeln!(writer, "SCALARS cp_dimensionless float 1")?;
    writeln!(writer, "LOOKUP_TABLE default")?;
    write_scalar_array(writer, grid, &export.field.cp)?;
    writeln!(writer, "VECTORS traction_pa double")?;
    write_vector_array(writer, grid, &export.field.traction_pa)?;
    writeln!(writer, "SCALARS solid_occupancy float 1")?;
    writeln!(writer, "LOOKUP_TABLE default")?;
    write_scalar_array(writer, grid, &export.field.mask)?;
    Ok(())
}

fn write_byte_field<W: Write>(writer: &mut W, name: &str, value: &str) -> std::io::Result<()> {
    writeln!(writer, "{name} 1 {} unsigned_char", value.len())?;
    for (index, byte) in value.as_bytes().iter().enumerate() {
        write!(writer, "{byte}")?;
        if index + 1 == value.len() || (index + 1) % 32 == 0 {
            writeln!(writer)?;
        } else {
            write!(writer, " ")?;
        }
    }
    Ok(())
}

fn write_vector_array<W: Write>(
    writer: &mut W,
    grid: ValidatedGrid,
    values: &[f32],
) -> std::io::Result<()> {
    for k in 0..grid.n {
        for j in 0..grid.n {
            for i in 0..grid.n {
                let cell = reyn_index(grid.n, i, j, k);
                let source = grid.frame.vector([
                    values[cell] as f64,
                    values[grid.points + cell] as f64,
                    values[2 * grid.points + cell] as f64,
                ]);
                writeln!(
                    writer,
                    "{:.17e} {:.17e} {:.17e}",
                    source[0], source[1], source[2]
                )?;
            }
        }
    }
    Ok(())
}

fn write_scalar_array<W: Write>(
    writer: &mut W,
    grid: ValidatedGrid,
    values: &[f32],
) -> std::io::Result<()> {
    for k in 0..grid.n {
        for j in 0..grid.n {
            for i in 0..grid.n {
                writeln!(writer, "{:.9e}", values[reyn_index(grid.n, i, j, k)])?;
            }
        }
    }
    Ok(())
}

fn reyn_index(n: usize, i: usize, j: usize, k: usize) -> usize {
    i * n * n + j * n + k
}

fn run_state_name(state: LifecycleState) -> &'static str {
    match state {
        LifecycleState::Pending => "pending",
        LifecycleState::Succeeded => "succeeded",
        LifecycleState::EvidenceLocked => "evidence_locked",
        LifecycleState::Draft => "draft",
        LifecycleState::Ready => "ready",
        LifecycleState::Running => "running",
        LifecycleState::Stale => "stale",
        LifecycleState::Failed => "failed",
        LifecycleState::Cancelled => "cancelled",
    }
}

/// Atomically replace the chosen path after a complete, flushed sibling-temp
/// write. A failed validation or write leaves the destination untouched.
pub fn write_atomic(path: &Path, export: &VtkFieldExport) -> Result<(), String> {
    let grid = export.validate()?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(format!(
            "VTK export directory does not exist: {}",
            parent.display()
        ));
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "VTK export path requires a valid file name.".to_string())?;
    let (temporary, file) = create_temporary_file(parent, file_name)?;
    let result = (|| -> Result<(), String> {
        let mut writer = BufWriter::new(file);
        write_validated(&mut writer, export, grid).map_err(|error| {
            format!(
                "VTK field export write failed for {}: {error}",
                path.display()
            )
        })?;
        writer
            .flush()
            .map_err(|error| format!("Could not flush VTK temporary file: {error}"))?;
        writer
            .get_ref()
            .sync_all()
            .map_err(|error| format!("Could not sync VTK temporary file: {error}"))?;
        drop(writer);
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("Could not atomically publish VTK export: {error}"))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn create_temporary_file(parent: &Path, file_name: &str) -> Result<(PathBuf, File), String> {
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{file_name}.tmp-{}-{sequence}",
            std::process::id()
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "Could not create VTK temporary file {}: {error}",
                    candidate.display()
                ));
            }
        }
    }
    Err("Could not reserve a unique sibling path for atomic VTK export.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> VtkFieldExport {
        let n = 3;
        let points = n * n * n;
        let field = EngineeringFieldBlob {
            n,
            velocity: [vec![1.0; points], vec![0.0; points], vec![0.0; points]].concat(),
            pressure_pa: (0..points).map(|value| value as f32 + 100_000.0).collect(),
            mask: vec![0.5; points],
            cp: (0..points).map(|value| value as f32 / 10.0).collect(),
            traction_pa: [vec![10.0; points], vec![20.0; points], vec![30.0; points]].concat(),
        };
        let field_sha256 = validate_and_hash_field(&field, points).unwrap();
        VtkFieldExport {
            field,
            source_to_solver_transform_4x4: [
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
            ],
            meters_per_source_unit: 1.0,
            transform_approved: true,
            run_state: LifecycleState::Succeeded,
            provenance: VtkFieldProvenance {
                source_revision_id: "source-revision-1".into(),
                case_revision_id: "case-revision-1".into(),
                run_id: "run-11111111-2222-3333-4444-555555555555".into(),
                model_sha256: "a".repeat(64),
                contract_kind: EXTERNAL_FLOW_CONTRACT.into(),
                field_sha256,
                traction_method: engineering::SURFACE_LOAD_METHOD.into(),
            },
        }
    }

    fn bytes(export: &VtkFieldExport) -> Vec<u8> {
        let mut output = Vec::new();
        write_legacy_vtk(&mut output, export).expect("write");
        output
    }

    fn lines_after<'a>(text: &'a str, marker: &str, count: usize) -> Vec<&'a str> {
        let mut lines = text.lines();
        assert!(lines.any(|line| line == marker), "missing marker {marker}");
        lines.take(count).collect()
    }

    fn numeric_values_between(text: &str, start: &str, end: Option<&str>) -> Vec<f64> {
        let mut lines = text.lines();
        assert!(lines.any(|line| line == start), "missing marker {start}");
        lines
            .take_while(|line| end.is_none_or(|end| *line != end))
            .flat_map(str::split_whitespace)
            .filter_map(|value| value.parse::<f64>().ok())
            .collect()
    }

    fn decode_byte_field(text: &str, name: &str) -> String {
        let lines = text.lines().collect::<Vec<_>>();
        let (header_index, header) = lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.starts_with(&format!("{name} ")))
            .expect("field header");
        let header = header.split_whitespace().collect::<Vec<_>>();
        let count = header[2].parse::<usize>().expect("tuple count");
        let bytes = lines[header_index + 1..]
            .iter()
            .flat_map(|line| line.split_whitespace())
            .take(count)
            .map(|value| value.parse::<u8>().expect("byte"))
            .collect::<Vec<_>>();
        String::from_utf8(bytes).expect("utf8")
    }

    #[test]
    fn vtk_structured_grid_has_exact_dimensions_and_array_lengths() {
        let text = String::from_utf8(bytes(&fixture())).unwrap();
        assert!(text.contains("DATASET STRUCTURED_GRID"));
        assert!(text.contains("DIMENSIONS 3 3 3"));
        assert!(text.contains("POINTS 27 double"));
        assert!(text.contains("POINT_DATA 27"));
        assert_eq!(
            numeric_values_between(&text, "POINTS 27 double", Some("FIELD FieldData 33")).len(),
            3 * 27
        );
        assert_eq!(
            numeric_values_between(
                &text,
                "VECTORS velocity_m_per_s double",
                Some("SCALARS recovered_pressure_pa float 1")
            )
            .len(),
            3 * 27
        );
        assert_eq!(
            numeric_values_between(
                &text,
                "SCALARS recovered_pressure_pa float 1",
                Some("SCALARS cp_dimensionless float 1")
            )
            .len(),
            27
        );
        let cp = numeric_values_between(
            &text,
            "SCALARS cp_dimensionless float 1",
            Some("VECTORS traction_pa double"),
        );
        assert_eq!(cp.len(), 27);
        for (actual, expected) in cp[..3].iter().zip([0.0, 0.9, 1.8]) {
            assert!((actual - expected).abs() < 1e-6);
        }
        assert_eq!(
            numeric_values_between(
                &text,
                "VECTORS traction_pa double",
                Some("SCALARS solid_occupancy float 1")
            )
            .len(),
            3 * 27
        );
        assert_eq!(
            numeric_values_between(&text, "SCALARS solid_occupancy float 1", None).len(),
            27
        );
    }

    #[test]
    fn coordinates_units_and_vectors_use_the_rotated_scaled_source_frame() {
        let mut export = fixture();
        // source→solver = translation + 2 * Rz(+90°)
        export.source_to_solver_transform_4x4 = [
            0.0, 2.0, 0.0, 0.0, -2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 2.0, 0.0, 10.0, 20.0, 30.0, 1.0,
        ];
        export.meters_per_source_unit = 1e-3;
        let text = String::from_utf8(bytes(&export)).unwrap();
        let point = lines_after(&text, "POINTS 27 double", 1)[0]
            .split_whitespace()
            .map(|value| value.parse::<f64>().unwrap())
            .collect::<Vec<_>>();
        let solver = std::f64::consts::TAU / 6.0;
        let expected = [
            (solver - 20.0) * 0.5e-3,
            -(solver - 10.0) * 0.5e-3,
            (solver - 30.0) * 0.5e-3,
        ];
        for axis in 0..3 {
            assert!((point[axis] - expected[axis]).abs() < 1e-12);
        }
        let velocity = lines_after(&text, "VECTORS velocity_m_per_s double", 1)[0]
            .split_whitespace()
            .map(|value| value.parse::<f64>().unwrap())
            .collect::<Vec<_>>();
        assert!(velocity[0].abs() < 1e-12);
        assert!((velocity[1] + 1.0).abs() < 1e-12);
        assert!(velocity[2].abs() < 1e-12);
        assert_eq!(
            decode_byte_field(&text, "coordinate_frame"),
            COORDINATE_FRAME
        );
        assert_eq!(decode_byte_field(&text, "coordinate_units"), "m");
        assert_eq!(decode_byte_field(&text, "velocity_units"), "m/s");
        assert_eq!(decode_byte_field(&text, "traction_units"), "Pa");
    }

    #[test]
    fn provenance_and_field_semantics_are_embedded() {
        let export = fixture();
        let text = String::from_utf8(bytes(&export)).unwrap();
        for (name, expected) in [
            ("reyn_schema", VTK_FIELD_SCHEMA),
            ("source_revision_id", "source-revision-1"),
            ("case_revision_id", "case-revision-1"),
            ("run_id", export.provenance.run_id.as_str()),
            ("model_sha256", export.provenance.model_sha256.as_str()),
            ("contract_kind", EXTERNAL_FLOW_CONTRACT),
            ("field_sha256", export.provenance.field_sha256.as_str()),
            ("velocity_source_class", "model_prediction"),
            ("recovered_pressure_source_class", "recovered"),
            ("cp_source_class", "derived"),
            ("traction_source_class", "derived"),
            ("solid_occupancy_source_class", "derived"),
            ("traction_method", engineering::SURFACE_LOAD_METHOD),
            ("vtk_point_order", "x_fastest_then_y_then_z"),
            ("reyn_scalar_storage_order", "z_fastest_then_y_then_x"),
            (
                "reyn_vector_storage_order",
                "component_major_then_z_fastest_then_y_then_x",
            ),
        ] {
            assert_eq!(decode_byte_field(&text, name), expected, "{name}");
        }
    }

    #[test]
    fn malformed_nonfinite_and_noncanonical_inputs_are_rejected() {
        let mut wrong_length = fixture();
        wrong_length.field.cp.pop();
        assert!(write_legacy_vtk(&mut Vec::new(), &wrong_length)
            .unwrap_err()
            .contains("Cp array length 26"));

        let mut nonfinite = fixture();
        nonfinite.field.pressure_pa[0] = f32::NAN;
        assert!(write_legacy_vtk(&mut Vec::new(), &nonfinite)
            .unwrap_err()
            .contains("non-finite value at persisted index 0"));

        let mut bad_occupancy = fixture();
        bad_occupancy.field.mask[0] = 1.1;
        assert!(write_legacy_vtk(&mut Vec::new(), &bad_occupancy)
            .unwrap_err()
            .contains("values must lie in [0,1]"));

        let mut missing_model_identity = fixture();
        missing_model_identity.provenance.model_sha256.clear();
        assert!(write_legacy_vtk(&mut Vec::new(), &missing_model_identity)
            .unwrap_err()
            .contains("canonical lowercase model SHA-256"));

        let mut nonfinite_transform = fixture();
        nonfinite_transform.source_to_solver_transform_4x4[0] = f64::NAN;
        assert!(write_legacy_vtk(&mut Vec::new(), &nonfinite_transform).is_err());

        let mut unapproved_transform = fixture();
        unapproved_transform.transform_approved = false;
        assert!(write_legacy_vtk(&mut Vec::new(), &unapproved_transform)
            .unwrap_err()
            .contains("approved source transform"));

        let mut reflected_transform = fixture();
        reflected_transform.source_to_solver_transform_4x4[0] = -1.0;
        assert!(write_legacy_vtk(&mut Vec::new(), &reflected_transform)
            .unwrap_err()
            .contains("right-handed rotation"));
    }

    #[test]
    fn field_digest_must_match_exact_persisted_payload() {
        let mut export = fixture();
        let recorded = export.provenance.field_sha256.clone();
        export.field.cp[0] = 0.125;
        let error = write_legacy_vtk(&mut Vec::new(), &export).unwrap_err();
        assert!(error.contains("does not match"));
        assert!(error.contains(&recorded));

        let mut uppercase_model = fixture();
        uppercase_model.provenance.model_sha256 = "A".repeat(64);
        assert!(write_legacy_vtk(&mut Vec::new(), &uppercase_model)
            .unwrap_err()
            .contains("canonical lowercase model SHA-256"));

        let mut uppercase_field = fixture();
        uppercase_field.provenance.field_sha256 =
            uppercase_field.provenance.field_sha256.to_ascii_uppercase();
        assert!(write_legacy_vtk(&mut Vec::new(), &uppercase_field)
            .unwrap_err()
            .contains("canonical lowercase field SHA-256"));
    }

    #[test]
    fn only_completed_persisted_run_states_can_export() {
        for state in [
            LifecycleState::Draft,
            LifecycleState::Ready,
            LifecycleState::Running,
            LifecycleState::Stale,
            LifecycleState::Failed,
        ] {
            let mut export = fixture();
            export.run_state = state;
            assert!(
                write_legacy_vtk(&mut Vec::new(), &export).is_err(),
                "{state:?}"
            );
        }
        let mut locked = fixture();
        locked.run_state = LifecycleState::EvidenceLocked;
        assert!(write_legacy_vtk(&mut Vec::new(), &locked).is_ok());
    }

    #[test]
    fn bytes_and_default_filename_are_deterministic() {
        let export = fixture();
        assert_eq!(bytes(&export), bytes(&export));
        let text = String::from_utf8(bytes(&export)).unwrap();
        assert!(text.contains("1.04719755119659763e0"));
        assert!(!text.contains("1,04719755119659763e0"));
        assert!(!text.contains("NaN"));
        assert!(!text.contains(" inf"));
        assert_eq!(
            default_file_name(
                "Wing Study / α=12°",
                &export.provenance.run_id,
                &export.provenance.model_sha256
            ),
            "Wing_Study_12__run-11111111-2222-3333-4444-555555555555__model-aaaaaaaaaaaa__fields.vtk"
        );
    }

    fn paraview_smoke_fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/qa/paraview-vtk-smoke/fixture.vtk")
    }

    #[test]
    fn golden_paraview_smoke_fixture_matches_exporter() {
        let expected = bytes(&fixture());
        let path = paraview_smoke_fixture_path();
        if std::env::var_os("REYN_WRITE_PARAVIEW_VTK_FIXTURE").is_some() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create ParaView smoke fixture directory");
            }
            std::fs::write(&path, &expected).expect("write ParaView smoke fixture");
        }
        let actual = std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "missing ParaView smoke fixture at {}: {error}; regenerate with \
                 REYN_WRITE_PARAVIEW_VTK_FIXTURE=1 cargo test --bin reyn-studio \
                 golden_paraview_smoke_fixture_matches_exporter",
                path.display()
            )
        });
        assert_eq!(
            actual, expected,
            "ParaView smoke fixture drifted from vtk_export::tests::fixture(); regenerate with \
             REYN_WRITE_PARAVIEW_VTK_FIXTURE=1"
        );
    }

    #[test]
    fn atomic_write_publishes_complete_bytes_without_temp_residue() {
        let export = fixture();
        let directory = std::env::temp_dir().join(format!(
            "reyn-vtk-export-test-{}",
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("field.vtk");
        write_atomic(&path, &export).unwrap();
        let published = std::fs::read(&path).unwrap();
        assert_eq!(published, bytes(&export));

        let mut mismatched = fixture();
        mismatched.field.cp[0] = 0.125;
        assert!(write_atomic(&path, &mismatched).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), published);
        assert_eq!(
            std::fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .count(),
            1
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
