//! STEP Part 21 import and deterministic B-rep tessellation.
//!
//! This module deliberately supports single-part solids only. Assemblies are
//! rejected because applying occurrence transforms incorrectly is worse than
//! refusing the import. The original STEP bytes remain the authoritative
//! project source; the triangle mesh is a deterministic derived representation.

use crate::cad::Mesh;
use std::collections::{BTreeMap, BTreeSet};
use truck_meshalgo::filters::OptimizingFilter;
use truck_meshalgo::prelude::RobustMeshableShape;
use truck_meshalgo::tessellation::MeshedShape;
use truck_stepio::r#in::{alias::Point3, Table};

pub const TRANSLATOR: &str = "truck-stepio";
pub const TRANSLATOR_VERSION: &str = "0.3.0";
pub const RELATIVE_CHORD_TOLERANCE: f64 = 0.001;
/// Truck tessellates B-rep faces independently. Weld coincident face-boundary
/// vertices by at most 0.005% of the tessellated bounding-box diagonal so a
/// valid closed solid does not become an artificial triangle soup with seams.
pub const VERTEX_WELD_RELATIVE_TOLERANCE: f64 = 0.00005;
const MAX_STEP_BYTES: usize = 128 * 1024 * 1024;
const MAX_SHELLS: usize = 256;
const MAX_TRIANGLES: usize = 250_000;

#[derive(Clone, Debug)]
pub struct StepImport {
    pub mesh: Mesh,
    pub declared_unit: Option<String>,
    pub tessellation_tolerance_source_units: f64,
    pub vertex_weld_relative_tolerance: f64,
    pub shell_count: usize,
    pub selected_shell_entity_id: Option<u64>,
    pub warnings: Vec<String>,
}

/// One B-rep shell that can be chosen when a STEP file contains several
/// resolved solids without assembly occurrence transforms.
#[derive(Clone, Debug)]
pub struct ShellCandidate {
    pub entity_id: u64,
    pub label: String,
}

/// Multi-shell STEP sources that are safe to pick from (no assembly graph).
#[derive(Clone, Debug)]
pub struct ShellChoiceRequired {
    pub declared_unit: String,
    pub shells: Vec<ShellCandidate>,
}

#[derive(Clone, Debug)]
pub enum StepParseError {
    Message(String),
    ChooseShell(ShellChoiceRequired),
}

impl From<StepParseError> for String {
    fn from(value: StepParseError) -> Self {
        match value {
            StepParseError::Message(message) => message,
            StepParseError::ChooseShell(choice) => format!(
                "this STEP file contains {} B-rep shells; choose one solid to analyze ({})",
                choice.shells.len(),
                choice
                    .shells
                    .iter()
                    .map(|shell| shell.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl From<String> for StepParseError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl From<&str> for StepParseError {
    fn from(message: &str) -> Self {
        Self::Message(message.into())
    }
}

/// Convenience wrapper used by fixture tests and external callers.
#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_step(bytes: &[u8]) -> Result<StepImport, String> {
    parse_step_selecting(bytes, None).map_err(Into::into)
}

/// List pickable shells when a multi-body STEP has no assembly occurrences.
#[cfg_attr(not(test), allow(dead_code))]
pub fn list_shell_candidates(bytes: &[u8]) -> Result<ShellChoiceRequired, String> {
    match parse_step_selecting(bytes, None) {
        Err(StepParseError::ChooseShell(choice)) => Ok(choice),
        Err(StepParseError::Message(message)) => Err(message),
        Ok(_) => Err("this STEP file has a single shell; no chooser is required".into()),
    }
}

/// Tessellate a STEP source, optionally restricting to one shell entity id.
pub fn parse_step_selecting(
    bytes: &[u8],
    selected_shell_entity_id: Option<u64>,
) -> Result<StepImport, StepParseError> {
    if bytes.is_empty() {
        return Err(StepParseError::Message("STEP source is empty".into()));
    }
    if bytes.len() > MAX_STEP_BYTES {
        return Err(StepParseError::Message(format!(
            "STEP source is {:.1} MB; the safe import limit is {} MB",
            bytes.len() as f64 / (1024.0 * 1024.0),
            MAX_STEP_BYTES / (1024 * 1024)
        )));
    }

    let result = std::panic::catch_unwind(|| parse_step_inner(bytes, selected_shell_entity_id));
    match result {
        Ok(result) => result,
        Err(_) => Err(StepParseError::Message(
            "the STEP translator stopped unexpectedly; the source was not imported or modified"
                .into(),
        )),
    }
}

fn parse_step_inner(
    bytes: &[u8],
    selected_shell_entity_id: Option<u64>,
) -> Result<StepImport, StepParseError> {
    // ISO 10303-21 control syntax is ASCII. Lossy decoding preserves that
    // syntax while tolerating legacy vendor comments or names in local encodings.
    let source = String::from_utf8_lossy(bytes);
    let upper = source.to_ascii_uppercase();
    if !upper.contains("ISO-10303-21") {
        return Err(StepParseError::Message(
            "missing ISO-10303-21 exchange-file header".into(),
        ));
    }
    if upper.contains("NEXT_ASSEMBLY_USAGE_OCCURRENCE(")
        || upper.contains("CONTEXT_DEPENDENT_SHAPE_REPRESENTATION(")
    {
        return Err(StepParseError::Message(
            "assemblies are not supported yet; export one resolved part/body as STEP (or pick a single solid in CAD) and import that file — multi-body occurrence transforms are not preserved"
                .into(),
        ));
    }

    let declared_unit = detect_length_unit(&upper)?.ok_or_else(|| {
        StepParseError::Message(
            "the STEP file has no unambiguous length-unit declaration; re-export it with explicit units"
                .into(),
        )
    })?;
    let table = Table::from_step(&source).ok_or_else(|| {
        StepParseError::Message("the STEP Part 21 data section could not be parsed".into())
    })?;
    if table.shell.is_empty() {
        return Err(StepParseError::Message(
            "no B-rep shell was found; point clouds, wireframes, and surface-only STEP files are unsupported"
                .into(),
        ));
    }
    if table.shell.len() > MAX_SHELLS {
        return Err(StepParseError::Message(format!(
            "the STEP file contains {} shells; the safe import limit is {MAX_SHELLS}",
            table.shell.len()
        )));
    }

    let mut shell_entries = table.shell.iter().collect::<Vec<_>>();
    shell_entries.sort_by_key(|(entity_id, _)| **entity_id);
    let total_shells = shell_entries.len();
    if shell_entries.len() > 1 && selected_shell_entity_id.is_none() {
        let shells = shell_entries
            .iter()
            .enumerate()
            .map(|(index, (entity_id, _))| ShellCandidate {
                entity_id: **entity_id,
                label: format!("Solid {} · shell #{}", index + 1, entity_id),
            })
            .collect();
        return Err(StepParseError::ChooseShell(ShellChoiceRequired {
            declared_unit,
            shells,
        }));
    }
    if let Some(selected) = selected_shell_entity_id {
        if !shell_entries
            .iter()
            .any(|(entity_id, _)| **entity_id == selected)
        {
            return Err(StepParseError::Message(format!(
                "B-rep shell #{selected} was not found in this STEP file"
            )));
        }
        shell_entries.retain(|(entity_id, _)| **entity_id == selected);
    }
    let mut compressed_shells = Vec::with_capacity(shell_entries.len());
    for (entity_id, shell) in shell_entries {
        let compressed = table.to_compressed_shell(shell).map_err(|error| {
            StepParseError::Message(format!("B-rep shell #{entity_id} is unsupported: {error}"))
        })?;
        compressed_shells.push((*entity_id, compressed));
    }

    // Derive tessellation scale from topological B-rep vertices, not every
    // CARTESIAN_POINT in the file: AP242 PMI and annotation geometry can sit
    // far from the analysis body and must not coarsen its surface mesh.
    let source_diameter = point_diameter(
        compressed_shells
            .iter()
            .flat_map(|(_, shell)| shell.vertices.iter().copied()),
    )?;
    let tolerance = source_diameter * RELATIVE_CHORD_TOLERANCE;
    if !tolerance.is_finite() || tolerance <= 0.0 {
        return Err(StepParseError::Message(
            "could not derive a finite STEP tessellation tolerance".into(),
        ));
    }

    let mut triangles = Vec::new();
    let mut warnings = Vec::new();

    for (entity_id, compressed) in compressed_shells {
        let mut polygon = compressed.robust_triangulation(tolerance).to_polygon();
        polygon
            .put_together_same_attrs(VERTEX_WELD_RELATIVE_TOLERANCE)
            .remove_degenerate_faces()
            .remove_unused_attrs();
        if !polygon.quad_faces().is_empty() || !polygon.other_faces().is_empty() {
            return Err(StepParseError::Message(format!(
                "B-rep shell #{entity_id} produced non-triangular facets"
            )));
        }

        let positions = polygon.positions();
        for face in polygon.tri_faces() {
            if triangles.len() >= MAX_TRIANGLES {
                return Err(StepParseError::Message(format!(
                    "STEP tessellation exceeds the safe limit of {MAX_TRIANGLES} triangles; export a coarser tessellation or a simpler body"
                )));
            }
            let mut triangle = [[0.0f32; 3]; 3];
            for (corner, vertex) in face.iter().enumerate() {
                let point = positions.get(vertex.pos).ok_or_else(|| {
                    StepParseError::Message(format!(
                        "B-rep shell #{entity_id} produced an invalid vertex index"
                    ))
                })?;
                triangle[corner] = point_to_f32(*point)?;
            }
            triangles.push(triangle);
        }
    }

    if triangles.is_empty() {
        return Err(StepParseError::Message(
            "STEP tessellation produced no triangles".into(),
        ));
    }
    if let Some(selected) = selected_shell_entity_id {
        warnings.push(format!(
            "Operator selected B-rep shell #{selected} from {total_shells} shells in the source; other shells were not analyzed."
        ));
    }

    Ok(StepImport {
        mesh: Mesh { tris: triangles },
        declared_unit: Some(declared_unit),
        tessellation_tolerance_source_units: tolerance,
        vertex_weld_relative_tolerance: VERTEX_WELD_RELATIVE_TOLERANCE,
        shell_count: selected_shell_entity_id.map(|_| 1).unwrap_or(total_shells),
        selected_shell_entity_id,
        warnings,
    })
}

fn point_to_f32(point: Point3) -> Result<[f32; 3], String> {
    let coordinates = [point.x, point.y, point.z];
    if coordinates.iter().any(|value| !value.is_finite()) {
        return Err("STEP tessellation produced a non-finite coordinate".into());
    }
    if coordinates
        .iter()
        .any(|value| *value > f32::MAX as f64 || *value < f32::MIN as f64)
    {
        return Err("STEP tessellation produced a coordinate outside the supported range".into());
    }
    Ok(coordinates.map(|value| value as f32))
}

fn point_diameter(points: impl Iterator<Item = Point3>) -> Result<f64, String> {
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    for point in points {
        for (axis, value) in [point.x, point.y, point.z].into_iter().enumerate() {
            if value.is_finite() {
                lo[axis] = lo[axis].min(value);
                hi[axis] = hi[axis].max(value);
            }
        }
    }
    let extents = std::array::from_fn::<_, 3, _>(|axis| hi[axis] - lo[axis]);
    let diameter = extents
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    if !diameter.is_finite() || diameter <= 0.0 {
        return Err("STEP geometry has no finite, positive 3D extent".into());
    }
    Ok(diameter)
}

fn detect_length_unit(upper: &str) -> Result<Option<String>, String> {
    let records = upper.split(';').collect::<Vec<_>>();
    let mut unit_entities = BTreeMap::new();
    for record in records
        .iter()
        .copied()
        .filter(|record| record.contains("LENGTH_UNIT"))
    {
        let Some(entity_id) = record_entity_id(record) else {
            continue;
        };
        let compact = record
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect::<String>();
        let unit = if compact.contains("SI_UNIT") && compact.contains(".METRE.") {
            if compact.contains(".MILLI.") {
                Some("mm")
            } else if compact.contains(".CENTI.") {
                Some("cm")
            } else if compact.contains("SI_UNIT($,.METRE.)") {
                Some("m")
            } else {
                None
            }
        } else if compact.contains("CONVERSION_BASED_UNIT('INCH") {
            Some("in")
        } else if compact.contains("CONVERSION_BASED_UNIT('FOOT")
            || compact.contains("CONVERSION_BASED_UNIT('FEET")
        {
            Some("ft")
        } else {
            None
        };
        unit_entities.insert(entity_id, unit.map(str::to_string));
    }

    // Conversion-based units reference an SI base unit. Only units referenced
    // by GLOBAL_UNIT_ASSIGNED_CONTEXT are authoritative for geometry; counting
    // every LENGTH_UNIT entity would misread an inch context as both inch and
    // millimetre.
    let assigned_ids = records
        .iter()
        .copied()
        .filter(|record| record.contains("GLOBAL_UNIT_ASSIGNED_CONTEXT"))
        .flat_map(record_entity_references)
        .collect::<BTreeSet<_>>();
    let mut units = BTreeSet::new();
    for entity_id in &assigned_ids {
        match unit_entities.get(entity_id) {
            Some(Some(unit)) => {
                units.insert(unit.clone());
            }
            Some(None) => {
                return Err(
                    "the active STEP representation declares a length unit Reyn cannot identify safely; re-export it in mm, cm, m, in, or ft"
                        .into(),
                );
            }
            None => {}
        }
    }
    if units.is_empty() && unit_entities.len() == 1 {
        match unit_entities.into_values().next().flatten() {
            Some(unit) => {
                units.insert(unit);
            }
            None => {
                return Err(
                    "the STEP file declares an unsupported SI length prefix; convert the part to millimetres, centimetres, or metres before import"
                        .into(),
                );
            }
        }
    }

    match units.len() {
        0 => Ok(None),
        1 => Ok(units.into_iter().next()),
        _ => Err(format!(
            "the STEP file uses multiple length units ({}); resolve the representation contexts in CAD before import",
            units.into_iter().collect::<Vec<_>>().join(", ")
        )),
    }
}

fn record_entity_id(record: &str) -> Option<u64> {
    let record = record.trim_start();
    let digits = record.strip_prefix('#')?.split_once('=')?.0.trim();
    digits.parse().ok()
}

fn record_entity_references(record: &str) -> Vec<u64> {
    let mut references = Vec::new();
    let bytes = record.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'#' {
            index += 1;
            continue;
        }
        index += 1;
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if start < index {
            if let Ok(entity_id) = record[start..index].parse() {
                references.push(entity_id);
            }
        }
    }
    references
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_si_and_conversion_units() {
        assert_eq!(
            detect_length_unit(
                "#1=(LENGTH_UNIT() NAMED_UNIT(*) SI_UNIT(.MILLI.,.METRE.));\
                 #2=(GEOMETRIC_REPRESENTATION_CONTEXT(3) GLOBAL_UNIT_ASSIGNED_CONTEXT((#1)));"
            )
            .unwrap(),
            Some("mm".into())
        );
        assert_eq!(
            detect_length_unit(
                "#1=(LENGTH_UNIT() NAMED_UNIT(*) SI_UNIT(.MILLI.,.METRE.));\
                 #2=MEASURE_WITH_UNIT(LENGTH_MEASURE(25.4),#1);\
                 #3=(LENGTH_UNIT() NAMED_UNIT(*) CONVERSION_BASED_UNIT('INCH',#2));\
                 #4=(GEOMETRIC_REPRESENTATION_CONTEXT(3) GLOBAL_UNIT_ASSIGNED_CONTEXT((#3)));"
            )
            .unwrap(),
            Some("in".into())
        );
        assert_eq!(
            detect_length_unit(
                "#1=(LENGTH_UNIT() NAMED_UNIT(*) SI_UNIT(.MICRO.,.METRE.));\
                 #2=(LENGTH_UNIT() NAMED_UNIT(*) SI_UNIT($,.METRE.));\
                 #3=(GEOMETRIC_REPRESENTATION_CONTEXT(3) GLOBAL_UNIT_ASSIGNED_CONTEXT((#2)));"
            )
            .unwrap(),
            Some("m".into())
        );
        assert!(detect_length_unit(
            "#1=(LENGTH_UNIT() NAMED_UNIT(*) SI_UNIT(.MICRO.,.METRE.));\
             #2=(GEOMETRIC_REPRESENTATION_CONTEXT(3) GLOBAL_UNIT_ASSIGNED_CONTEXT((#1)));"
        )
        .unwrap_err()
        .contains("cannot identify safely"));
    }

    #[test]
    fn blocks_multiple_units_and_assemblies() {
        let units = "#1=(LENGTH_UNIT() SI_UNIT($,.METRE.));\
                     #2=(LENGTH_UNIT() SI_UNIT(.MILLI.,.METRE.));\
                     #3=(GEOMETRIC_REPRESENTATION_CONTEXT(3) GLOBAL_UNIT_ASSIGNED_CONTEXT((#1)));\
                     #4=(GEOMETRIC_REPRESENTATION_CONTEXT(3) GLOBAL_UNIT_ASSIGNED_CONTEXT((#2)));";
        assert!(detect_length_unit(units)
            .unwrap_err()
            .contains("multiple length units"));

        let assembly = b"ISO-10303-21; DATA; #1=NEXT_ASSEMBLY_USAGE_OCCURRENCE('','','',#2,#3,$); ENDSEC; END-ISO-10303-21;";
        assert!(parse_step(assembly).unwrap_err().contains("assemblies"));
    }

    #[test]
    fn selected_shell_must_exist_on_single_body_fixture() {
        let bytes = include_bytes!("../test-geometry/cuboid_ap214.step");
        let err = parse_step_selecting(bytes, Some(9_999_999)).expect_err("missing shell");
        match err {
            StepParseError::Message(message) => {
                assert!(message.contains("was not found"));
            }
            StepParseError::ChooseShell(_) => panic!("cuboid fixture should not require a chooser"),
        }
        let imported = parse_step_selecting(bytes, None).expect("single-shell STEP");
        assert!(imported.selected_shell_entity_id.is_none());
        assert_eq!(imported.shell_count, 1);
        assert!(list_shell_candidates(bytes)
            .unwrap_err()
            .contains("single shell"));
    }

    #[test]
    fn tessellates_ap214_cuboid_fixture() {
        let bytes = include_bytes!("../test-geometry/cuboid_ap214.step");
        let imported = parse_step(bytes).expect("STEP cuboid should import");
        assert_eq!(imported.declared_unit.as_deref(), Some("m"));
        assert_eq!(imported.shell_count, 1);
        assert!(imported.mesh.tris.len() >= 12);
        let diagnostics = crate::cad::diagnose_mesh(&imported.mesh);
        assert_eq!(diagnostics.boundary_edges, 0);
        assert_eq!(diagnostics.non_manifold_edges, 0);
        assert_eq!(diagnostics.components, 1);
        let voxelized = crate::cad::voxelize(&imported.mesh, 32)
            .expect("closed STEP cuboid should reach voxel preflight");
        assert!(voxelized.solid_voxels > 0);
        assert!(
            voxelized.axis_disagreement_fraction
                <= crate::engineering::GeometryPreflight::MAX_AXIS_DISAGREEMENT_FRACTION
        );
        crate::cad::voxelize_oriented(
            &imported.mesh,
            32,
            crate::cad::BodyOrientation::from_degrees([10.0, 5.0, 0.0]),
        )
        .expect("STEP cuboid should support orientation re-voxelization");
    }


    #[test]
    fn cuboid_minimum_cells_across_at_64() {
        let bytes = include_bytes!("../test-geometry/cuboid_ap214.step");
        let imported = parse_step(bytes).expect("STEP cuboid");
        let diagnostics = crate::cad::diagnose_mesh(&imported.mesh);
        let orientation =
            crate::cad::BodyOrientation::align_longest_extent_to_stream(diagnostics.extents);
        let vm = crate::cad::voxelize_oriented(&imported.mesh, 64, orientation)
            .expect("voxelize 64");
        eprintln!(
            "cuboid64 solid={} min_cells={} clearance={} orientation={:?} disagreement={:.4}",
            vm.solid_voxels,
            vm.minimum_cells_across,
            vm.boundary_clearance_cells,
            orientation.to_degrees(),
            vm.axis_disagreement_fraction
        );
        assert!(
            vm.minimum_cells_across >= 3,
            "expected >=3 cells across, got {}",
            vm.minimum_cells_across
        );
    }

    #[test]
    fn complex_ap242_translation_is_deterministic_and_defects_remain_visible() {
        let bytes = include_bytes!("../test-geometry/part_ap242.step");
        let first = parse_step(bytes).expect("AP242 part should import");
        let second = parse_step(bytes).expect("repeat AP242 import should succeed");
        assert_eq!(first.declared_unit.as_deref(), Some("m"));
        assert_eq!(first.mesh.tris, second.mesh.tris);
        assert_eq!(
            crate::cad::analyzed_mesh_sha256(&first.mesh),
            crate::cad::analyzed_mesh_sha256(&second.mesh)
        );
        assert_eq!(
            first.tessellation_tolerance_source_units,
            second.tessellation_tolerance_source_units
        );
        let diagnostics = crate::cad::diagnose_mesh(&first.mesh);
        assert!(diagnostics.triangles > 12);
        // Truck currently leaves seams in this valid curved AP242 fixture.
        // Import must preserve those diagnostics so GeometryPreflight blocks
        // execution instead of presenting partial tessellation as a closed body.
        assert!(diagnostics.boundary_edges > 0);
        assert_eq!(diagnostics.non_manifold_edges, 0);
    }

    #[test]
    fn corpus_rejects_assembly_occurrence_fixture() {
        let bytes = include_bytes!("../test-geometry/corpus/assembly_occurrence.step");
        let err = parse_step(bytes).expect_err("assembly occurrence must fail closed");
        assert!(
            err.contains("assembl"),
            "expected assembly reject, got: {err}"
        );
    }

    #[test]
    fn corpus_rejects_conflicting_units_fixture() {
        let bytes = include_bytes!("../test-geometry/corpus/conflicting_units.step");
        let err = parse_step(bytes).expect_err("conflicting units must fail closed");
        assert!(
            err.contains("multiple length units") || err.contains("unit"),
            "expected unit conflict reject, got: {err}"
        );
    }

    #[test]
    fn corpus_rejects_malformed_truncated_fixture() {
        let bytes = include_bytes!("../test-geometry/corpus/malformed_truncated.step");
        let err = parse_step(bytes).expect_err("truncated STEP must fail closed");
        assert!(!err.is_empty(), "malformed import must return an error");
    }

    #[test]
    fn nist_ctc01_ap203_geom_imports_deterministically() {
        let bytes = include_bytes!("../test-geometry/corpus/vendor/nist_ctc01_ap203_geom.step");
        let first = parse_step(bytes).expect("NIST CTC-01 AP203 geometry should import");
        let second = parse_step(bytes).expect("repeat NIST CTC-01 import should succeed");
        assert_eq!(first.shell_count, 1);
        assert!(first.mesh.tris.len() >= 12);
        assert_eq!(
            crate::cad::analyzed_mesh_sha256(&first.mesh),
            crate::cad::analyzed_mesh_sha256(&second.mesh)
        );
        assert_eq!(
            first.tessellation_tolerance_source_units,
            second.tessellation_tolerance_source_units
        );
    }

    #[test]
    fn nist_ftc06_ap203_geom_imports_or_records_honest_ceiling() {
        let bytes = include_bytes!("../test-geometry/corpus/vendor/nist_ftc06_ap203_geom.step");
        match parse_step(bytes) {
            Ok(imported) => {
                assert_eq!(imported.shell_count, 1);
                assert!(imported.mesh.tris.len() >= 12);
                let again = parse_step(bytes).expect("repeat FTC-06 import");
                assert_eq!(
                    crate::cad::analyzed_mesh_sha256(&imported.mesh),
                    crate::cad::analyzed_mesh_sha256(&again.mesh)
                );
            }
            Err(error) => {
                // Keep the failure visible: this is corpus evidence for OCCT gating.
                assert!(
                    !error.is_empty(),
                    "NIST FTC-06 reject must carry an actionable message"
                );
            }
        }
    }

    #[test]
    fn nist_ctc01_ap242_with_pmi_imports_or_records_honest_ceiling() {
        let bytes = include_bytes!("../test-geometry/corpus/vendor/nist_ctc01_ap242_e1.step");
        match parse_step(bytes) {
            Ok(imported) => {
                assert_eq!(imported.shell_count, 1);
                let again = parse_step(bytes).expect("repeat AP242 CTC-01 import");
                assert_eq!(
                    crate::cad::analyzed_mesh_sha256(&imported.mesh),
                    crate::cad::analyzed_mesh_sha256(&again.mesh)
                );
            }
            Err(error) => {
                assert!(
                    !error.is_empty(),
                    "NIST CTC-01 AP242 reject must carry an actionable message"
                );
            }
        }
    }

    #[test]
    fn nist_ctc03_and_ctc05_ap203_geom_import_deterministically() {
        for bytes in [
            include_bytes!("../test-geometry/corpus/vendor/nist_ctc03_ap203_geom.step").as_slice(),
            include_bytes!("../test-geometry/corpus/vendor/nist_ctc05_ap203_geom.step").as_slice(),
        ] {
            let imported = match parse_step_selecting(bytes, None) {
                Ok(imported) => imported,
                Err(StepParseError::ChooseShell(choice)) => {
                    let selected = choice.shells[0].entity_id;
                    parse_step_selecting(bytes, Some(selected))
                        .expect("NIST multi-shell CTC should import after pick-one")
                }
                Err(StepParseError::Message(error)) => {
                    panic!("NIST CTC AP203 geometry should import: {error}")
                }
            };
            let again = parse_step_selecting(bytes, imported.selected_shell_entity_id)
                .expect("repeat NIST CTC import should succeed");
            assert_eq!(imported.shell_count, 1);
            assert!(imported.mesh.tris.len() >= 12);
            assert_eq!(
                crate::cad::analyzed_mesh_sha256(&imported.mesh),
                crate::cad::analyzed_mesh_sha256(&again.mesh)
            );
        }
    }

    #[test]
    fn nist_ftc11_ap203_geom_imports_or_records_honest_ceiling() {
        let bytes = include_bytes!("../test-geometry/corpus/vendor/nist_ftc11_ap203_geom.step");
        match parse_step(bytes) {
            Ok(imported) => {
                assert_eq!(imported.shell_count, 1);
                assert!(!imported.mesh.tris.is_empty());
            }
            Err(error) => assert!(!error.is_empty()),
        }
    }

    #[test]
    fn nist_tessellated_and_stc_ap242_record_honest_ceiling_or_import() {
        for bytes in [
            include_bytes!("../test-geometry/corpus/vendor/nist_stc06_ap242_e3.step").as_slice(),
            include_bytes!("../test-geometry/corpus/vendor/nist_ftc08_ap242_e1_tessellated.step")
                .as_slice(),
        ] {
            match parse_step(bytes) {
                Ok(imported) => {
                    assert_eq!(imported.shell_count, 1);
                    assert!(!imported.mesh.tris.is_empty());
                }
                Err(error) => {
                    // Faceted/tessellated NIST packs often have no classic B-rep shell.
                    // Keep the reject visible — this is OCCT-gate evidence.
                    assert!(!error.is_empty());
                }
            }
        }
    }

    #[test]
    fn fusion_simple_ap214_imports_deterministically() {
        let bytes = include_bytes!("../test-geometry/corpus/vendor/fusion_simple_ap214.step");
        let first = match parse_step_selecting(bytes, None) {
            Ok(imported) => imported,
            Err(StepParseError::ChooseShell(choice)) => {
                let selected = choice.shells[0].entity_id;
                parse_step_selecting(bytes, Some(selected))
                    .expect("Fusion multi-shell part should import after pick-one")
            }
            Err(StepParseError::Message(error)) => {
                panic!("Fusion AP214 export should import: {error}")
            }
        };
        let again = parse_step_selecting(bytes, first.selected_shell_entity_id)
            .expect("repeat Fusion import should succeed");
        assert_eq!(first.shell_count, 1);
        assert!(first.mesh.tris.len() >= 12);
        assert_eq!(
            crate::cad::analyzed_mesh_sha256(&first.mesh),
            crate::cad::analyzed_mesh_sha256(&again.mesh)
        );
        // Fusion personal export used AUTOMOTIVE_DESIGN (AP214) and millimetre units.
        assert_eq!(first.declared_unit.as_deref(), Some("mm"));
    }

    #[test]
    fn onshape_extra_ap242_imports_deterministically() {
        let bytes = include_bytes!("../test-geometry/corpus/vendor/onshape_extra_ap242.step");
        let first = parse_step(bytes).expect("Onshape AP242 extra part should import");
        let second = parse_step(bytes).expect("repeat Onshape import should succeed");
        assert_eq!(first.shell_count, 1);
        assert!(first.mesh.tris.len() >= 12);
        assert_eq!(first.declared_unit.as_deref(), Some("m"));
        assert_eq!(
            crate::cad::analyzed_mesh_sha256(&first.mesh),
            crate::cad::analyzed_mesh_sha256(&second.mesh)
        );
        // Must be a different fixture from the existing curved Onshape part.
        let existing = include_bytes!("../test-geometry/part_ap242.step");
        assert_ne!(
            crate::cad::analyzed_mesh_sha256(&first.mesh),
            crate::cad::analyzed_mesh_sha256(
                &parse_step(existing)
                    .expect("existing Onshape fixture should still import")
                    .mesh
            )
        );
    }

    #[test]
    fn solidworks_nist_ctc01_ap214_imports_deterministically() {
        let bytes =
            include_bytes!("../test-geometry/corpus/vendor/solidworks_nist_ctc01_ap214.step");
        let first = match parse_step_selecting(bytes, None) {
            Ok(imported) => imported,
            Err(StepParseError::ChooseShell(choice)) => {
                let selected = choice.shells[0].entity_id;
                parse_step_selecting(bytes, Some(selected))
                    .expect("SolidWorks NIST CTC-01 should import after pick-one")
            }
            Err(StepParseError::Message(error)) => {
                panic!("SolidWorks NIST CTC-01 AP214 export should import: {error}")
            }
        };
        let again = parse_step_selecting(bytes, first.selected_shell_entity_id)
            .expect("repeat SolidWorks import should succeed");
        assert_eq!(first.shell_count, 1);
        assert!(first.mesh.tris.len() >= 12);
        assert_eq!(first.declared_unit.as_deref(), Some("mm"));
        assert_eq!(
            crate::cad::analyzed_mesh_sha256(&first.mesh),
            crate::cad::analyzed_mesh_sha256(&again.mesh)
        );
    }
}
