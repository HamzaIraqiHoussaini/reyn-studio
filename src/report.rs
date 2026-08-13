//! Self-contained HTML engineering report for one immutable external-flow run.
//!
//! The report is a review artifact derived from already-persisted run data —
//! it never fabricates values, always carries the full provenance chain
//! (source hash → case revision → run → model hash), and closes with an
//! explicit limitations block. Everything is inlined (CSS + base64 figures)
//! so the single file can be archived or e-mailed as-is.

use crate::engineering::{self, ExternalFlowCase};
use crate::units::{self, Quantity, UnitSystem, ValueFormat};
use base64::Engine as _;

pub const REPORT_SCHEMA: &str = "reyn_engineering_report_html.v1";

/// Optional embedded section figure (already rendered from the stored field).
pub struct SectionFigure {
    pub png_base64: String,
    pub caption: String,
}

pub struct ReportInput<'a> {
    pub case: &'a ExternalFlowCase,
    pub run_id: &'a str,
    pub run_created_utc_unix: u64,
    pub generated_utc_unix: u64,
    pub app_version: &'a str,
    pub unit_system: UnitSystem,
    pub format: ValueFormat,
    pub section_figure: Option<SectionFigure>,
}

/// Escape a string for HTML text/attribute contexts.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn row(label: &str, value: &str) -> String {
    format!(
        "<tr><th>{}</th><td>{}</td></tr>\n",
        escape(label),
        escape(value)
    )
}

fn mono_row(label: &str, value: &str) -> String {
    format!(
        "<tr><th>{}</th><td class=\"mono\">{}</td></tr>\n",
        escape(label),
        escape(value)
    )
}

fn source_row(label: &str, value: &str, source: &str) -> String {
    format!(
        "<tr><th>{}</th><td>{}</td><td class=\"source\">{}</td></tr>\n",
        escape(label),
        escape(value),
        escape(source)
    )
}

fn validate_required_text(label: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("Engineering report requires a persisted {label}."));
    }
    if value.chars().any(|character| character == '\0') {
        return Err(format!(
            "Engineering report {label} contains an unsupported null character."
        ));
    }
    Ok(())
}

fn validate_sha256(label: &str, value: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(format!(
            "Engineering report requires a canonical lowercase {label} SHA-256."
        ))
    }
}

fn validate_finite(label: &str, values: impl IntoIterator<Item = f64>) -> Result<(), String> {
    if let Some((index, _)) = values
        .into_iter()
        .enumerate()
        .find(|(_, value)| !value.is_finite())
    {
        Err(format!(
            "Engineering report rejected non-finite {label} value at index {index}."
        ))
    } else {
        Ok(())
    }
}

fn validate_section_figure(figure: &SectionFigure) -> Result<(), String> {
    validate_required_text("section figure caption", &figure.caption)?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&figure.png_base64)
        .map_err(|error| {
            format!("Engineering report section figure is not valid base64: {error}")
        })?;
    if !decoded.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("Engineering report section figure must contain a valid PNG signature.".into());
    }
    Ok(())
}

fn validate_report_input(
    input: &ReportInput<'_>,
    result: &engineering::EngineeringResult,
) -> Result<(), String> {
    let case = input.case;
    if !matches!(
        case.stage,
        engineering::CaseStage::Results | engineering::CaseStage::Evidence
    ) {
        return Err(
            "Engineering report requires a completed case in Results or Evidence state.".into(),
        );
    }
    for (label, value) in [
        ("case name", case.name.as_str()),
        ("source file name", case.source_name.as_str()),
        ("run identity", input.run_id),
        ("model identity", case.model_id.as_str()),
        ("application version", input.app_version),
        ("surface-load method", result.method.as_str()),
    ] {
        validate_required_text(label, value)?;
    }
    let source_revision = case
        .source_revision_id
        .as_deref()
        .ok_or_else(|| "Engineering report requires a persisted source revision.".to_string())?;
    validate_required_text("source revision", source_revision)?;
    let case_revision = case
        .case_revision_id
        .as_deref()
        .ok_or_else(|| "Engineering report requires a persisted case revision.".to_string())?;
    validate_required_text("case revision", case_revision)?;
    validate_sha256("source", &case.preflight.source_sha256)?;
    if !case.preflight.import_steps.is_empty() {
        validate_sha256("analyzed mesh", &case.preflight.analyzed_mesh_sha256)?;
    }
    let model_sha256 = case
        .model_sha256
        .as_deref()
        .ok_or_else(|| "Engineering report requires a persisted model SHA-256.".to_string())?;
    validate_sha256("model", model_sha256)?;
    if input.generated_utc_unix < input.run_created_utc_unix {
        return Err(
            "Engineering report generation time cannot precede the immutable run time.".into(),
        );
    }
    if case.operating.length_unit.meters_per_unit().is_none() {
        return Err("Engineering report requires confirmed geometry units.".into());
    }
    for (label, value) in [
        ("reference length", case.operating.reference_length),
        ("free-stream speed", case.operating.velocity),
        ("density", case.operating.density),
        ("dynamic viscosity", case.operating.viscosity),
    ] {
        if !value.is_finite() || value <= 0.0 {
            return Err(format!(
                "Engineering report requires a finite positive {label}."
            ));
        }
    }
    validate_finite(
        "operating point",
        std::iter::once(case.operating.reference_pressure).chain(case.operating.flow_direction),
    )?;
    if case.operating.horizon_steps == 0 {
        return Err("Engineering report requires a nonzero prediction horizon.".into());
    }
    validate_finite(
        "geometry preflight",
        [
            case.preflight.source_signed_volume,
            case.preflight.proposed_scale,
            case.preflight.solver_characteristic_length,
            case.preflight.angle_of_attack_deg,
            case.preflight.yaw_deg,
            case.preflight.roll_deg,
            case.preflight.voxel_axis_disagreement_fraction,
        ]
        .into_iter()
        .chain(case.preflight.source_extents)
        .chain(case.preflight.transform_4x4),
    )?;
    validate_finite(
        "engineering result",
        [
            result.cp_min,
            result.cp_max,
            result.surface_area_m2,
            result.pressure_force_fraction,
            result.divergence_rms,
            result.wake_deficit_peak,
            result.wake_deficit_mean,
        ]
        .into_iter()
        .chain(result.force_coefficients)
        .chain(result.moment_coefficients)
        .chain(result.force_newtons)
        .chain(result.moment_newton_meters)
        .chain(result.load_hotspot)
        .chain(result.suction_hotspot),
    )?;
    if result.cp_min > result.cp_max {
        return Err("Engineering report Cp range has minimum greater than maximum.".into());
    }
    if result.surface_area_m2 < 0.0 {
        return Err("Engineering report surface area cannot be negative.".into());
    }
    if !(0.0..=1.0).contains(&result.pressure_force_fraction) {
        return Err("Engineering report pressure-force fraction must lie in [0,1].".into());
    }
    if result.divergence_rms < 0.0 {
        return Err("Engineering report divergence RMS cannot be negative.".into());
    }
    if let Some(figure) = &input.section_figure {
        validate_section_figure(figure)?;
    }
    Ok(())
}

/// Format an SI value in the report's display system, with the SI value in
/// parentheses when the display system is not SI (both are always readable).
fn dual(quantity: Quantity, si: f64, system: UnitSystem, format: ValueFormat) -> String {
    let display = units::format_quantity(quantity, si, system, format);
    if system == UnitSystem::Si {
        display
    } else {
        format!(
            "{display} ({})",
            units::format_quantity(quantity, si, UnitSystem::Si, format)
        )
    }
}

/// Build the report. Fails when the case has no completed result or no
/// durable run identity — a report is never produced from a draft.
pub fn engineering_report_html(input: &ReportInput<'_>) -> Result<String, String> {
    let case = input.case;
    let result = case
        .result
        .as_ref()
        .ok_or_else(|| "A completed run result is required for a report.".to_string())?;
    validate_report_input(input, result)?;
    let operating = &case.operating;
    let preflight = &case.preflight;
    let system = input.unit_system;
    let format = input.format;
    let format_plain = |value: f64| units::format_value(value, format);

    let mut provenance = String::new();
    provenance.push_str(&row("Source file", &case.source_name));
    provenance.push_str(&mono_row("Source SHA-256", &preflight.source_sha256));
    if !preflight.analyzed_mesh_sha256.is_empty() {
        provenance.push_str(&mono_row(
            "Analyzed mesh SHA-256",
            &preflight.analyzed_mesh_sha256,
        ));
    }
    provenance.push_str(&mono_row(
        "Source revision",
        case.source_revision_id
            .as_deref()
            .expect("source revision validated"),
    ));
    provenance.push_str(&mono_row(
        "Case revision",
        case.case_revision_id
            .as_deref()
            .expect("case revision validated"),
    ));
    provenance.push_str(&mono_row("Immutable run", input.run_id));
    provenance.push_str(&row(
        "Run created",
        &crate::app::format_utc(input.run_created_utc_unix),
    ));
    provenance.push_str(&row("Model", &case.model_id));
    provenance.push_str(&mono_row(
        "Model SHA-256",
        case.model_sha256
            .as_deref()
            .expect("model SHA-256 validated"),
    ));
    provenance.push_str(&mono_row("Contract", engineering::EXTERNAL_FLOW_CONTRACT));
    provenance.push_str(&mono_row(
        "Result schema",
        engineering::ENGINEERING_RESULT_SCHEMA,
    ));
    provenance.push_str(&mono_row("Surface-load method", &result.method));

    let length_symbol = operating.length_unit.symbol();
    let meters_per_unit = operating.length_unit.meters_per_unit();
    let reference_length_m = meters_per_unit.map(|scale| operating.reference_length * scale);
    let mut operating_rows = String::new();
    operating_rows.push_str(&row("Geometry units", length_symbol));
    operating_rows.push_str(&row(
        "Reference length",
        &match reference_length_m {
            Some(meters) => format!(
                "{} {length_symbol} ({})",
                format_plain(operating.reference_length),
                units::format_quantity(Quantity::Length, meters, system, format)
            ),
            None => format!(
                "{} {length_symbol} — units unconfirmed",
                format_plain(operating.reference_length)
            ),
        },
    ));
    operating_rows.push_str(&row(
        "Free-stream speed",
        &dual(Quantity::Velocity, operating.velocity, system, format),
    ));
    operating_rows.push_str(&row(
        "Density",
        &dual(Quantity::Density, operating.density, system, format),
    ));
    operating_rows.push_str(&row(
        "Dynamic viscosity",
        &dual(Quantity::Viscosity, operating.viscosity, system, format),
    ));
    operating_rows.push_str(&row(
        "Reference pressure",
        &dual(
            Quantity::Pressure,
            operating.reference_pressure,
            system,
            format,
        ),
    ));
    operating_rows.push_str(&row(
        "Reynolds number",
        &operating
            .reynolds()
            .map(format_plain)
            .unwrap_or_else(|| "UNKNOWN — units unconfirmed".into()),
    ));
    operating_rows.push_str(&row(
        "Dynamic pressure q∞",
        &operating
            .dynamic_pressure()
            .map(|value| dual(Quantity::Pressure, value, system, format))
            .unwrap_or_else(|| "incomplete".into()),
    ));
    operating_rows.push_str(&row(
        "Prediction horizon",
        &format!("{} steps", operating.horizon_steps),
    ));
    operating_rows.push_str(&row("Flow direction", "+X · fixed-body contract"));

    let watertight = preflight.boundary_edges == 0 && preflight.non_manifold_edges == 0;
    let mut geometry_rows = String::new();
    geometry_rows.push_str(&row("Triangles", &preflight.triangles.to_string()));
    geometry_rows.push_str(&row(
        "Surface components",
        &preflight.components.to_string(),
    ));
    geometry_rows.push_str(&row(
        "Watertight",
        if watertight {
            "yes — no open or non-manifold edges"
        } else {
            "no — open or non-manifold edges present"
        },
    ));
    geometry_rows.push_str(&row(
        "Defects",
        &format!(
            "{} degenerate · {} open boundary edges · {} non-manifold edges",
            preflight.degenerate_triangles, preflight.boundary_edges, preflight.non_manifold_edges
        ),
    ));
    geometry_rows.push_str(&row(
        "Bounding box",
        &format!(
            "{} × {} × {} {length_symbol}",
            format_plain(preflight.source_extents[0]),
            format_plain(preflight.source_extents[1]),
            format_plain(preflight.source_extents[2]),
        ),
    ));
    geometry_rows.push_str(&row(
        "Body orientation",
        &format!(
            "{} — applied to the geometry before voxelization; the free stream stays on +X",
            preflight.body_orientation_summary()
        ),
    ));
    geometry_rows.push_str(&row(
        "Source surface",
        &format!(
            "{} component(s) · {} open · {} non-manifold · {} inconsistent-winding edges · {} intersecting triangle pairs · signed volume {:+.6e} source³",
            preflight.components,
            preflight.boundary_edges,
            preflight.non_manifold_edges,
            preflight.inconsistent_winding_edges,
            preflight.self_intersection_pairs,
            preflight.source_signed_volume,
        ),
    ));
    geometry_rows.push_str(&row(
        "Voxel grid",
        &format!(
            "{}³ · {} solid voxels · {}-cell resolved core · {} cells boundary clearance",
            preflight.target_grid,
            preflight.solid_voxels,
            preflight.minimum_cells_across,
            preflight.boundary_clearance_cells
        ),
    ));
    geometry_rows.push_str(&row(
        "Occupancy validation",
        &format!(
            "classifier v{} · {:.2}% three-axis disagreement · odd scanlines X/Y/Z {} / {} / {}",
            preflight.voxel_classification_version,
            preflight.voxel_axis_disagreement_fraction * 100.0,
            preflight.voxel_odd_crossing_rows[0],
            preflight.voxel_odd_crossing_rows[1],
            preflight.voxel_odd_crossing_rows[2],
        ),
    ));
    for step in &preflight.import_steps {
        let parameters = step
            .parameters
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(" · ");
        let detail = if parameters.is_empty() {
            step.operation.clone()
        } else {
            format!("{} · {parameters}", step.operation)
        };
        geometry_rows.push_str(&row("Import derivation", &detail));
    }
    for waiver in &preflight.waivers {
        geometry_rows.push_str(&row("Named waiver", waiver));
    }

    let coefficient = |value: f64| format_plain(value);
    let axis_labels = ["+X · streamwise (drag)", "+Y · lateral", "+Z · vertical"];
    let load_source = format!("DERIVED · {}", result.method);
    let mut results_rows = String::new();
    for (axis, label) in axis_labels.iter().enumerate() {
        results_rows.push_str(&format!(
            "<tr><th>Force coefficient · {}</th><td class=\"mono\">{}</td><td class=\"mono\">{}</td><td class=\"source\">{}</td></tr>\n",
            escape(label),
            escape(&coefficient(result.force_coefficients[axis])),
            escape(&dual(Quantity::Force, result.force_newtons[axis], system, format)),
            escape(&load_source),
        ));
    }
    for (axis, label) in axis_labels.iter().enumerate() {
        results_rows.push_str(&format!(
            "<tr><th>Moment coefficient · {}</th><td class=\"mono\">{}</td><td class=\"mono\">{}</td><td class=\"source\">{}</td></tr>\n",
            escape(label),
            escape(&coefficient(result.moment_coefficients[axis])),
            escape(&dual(
                Quantity::Moment,
                result.moment_newton_meters[axis],
                system,
                format
            )),
            escape(&load_source),
        ));
    }

    let mut scalar_rows = String::new();
    scalar_rows.push_str(&source_row(
        "Cp range (dimensionless)",
        &format!(
            "{} … {}",
            format_plain(result.cp_min),
            format_plain(result.cp_max)
        ),
        "DERIVED · physical-reference Cp from recovered pressure",
    ));
    scalar_rows.push_str(&source_row(
        "Diffuse surface area",
        &dual(Quantity::Area, result.surface_area_m2, system, format),
        "DERIVED · diffuse immersed interface geometry",
    ));
    scalar_rows.push_str(&source_row(
        "Pressure share of force (component norms)",
        &format!("{} %", format_plain(result.pressure_force_fraction * 100.0)),
        "DERIVED · integrated pressure / total fluid-force norms",
    ));
    scalar_rows.push_str(&source_row(
        "Wake deficit · peak / mean",
        &format!(
            "{} / {}",
            format_plain(result.wake_deficit_peak),
            format_plain(result.wake_deficit_mean)
        ),
        "DERIVED · model-predicted velocity",
    ));
    // Divergence RMS is retained in immutable evidence JSON for developer
    // forensics; customer lab sheets stay loads/Cp/wake focused.
    let mut warnings_html = String::new();
    if !result.warnings.is_empty() {
        warnings_html.push_str("<h2>Run warnings</h2>\n<ul>\n");
        for warning in &result.warnings {
            warnings_html.push_str(&format!("<li>{}</li>\n", escape(warning)));
        }
        warnings_html.push_str("</ul>\n");
    }

    let figure_html = input
        .section_figure
        .as_ref()
        .map(|figure| {
            format!(
                "<h2>Section evidence</h2>\n<figure>\n<img alt=\"Stored engineering section\" \
                 src=\"data:image/png;base64,{}\">\n<figcaption>{}</figcaption>\n</figure>\n",
                escape(&figure.png_base64),
                escape(&figure.caption)
            )
        })
        .unwrap_or_default();

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>{title} — Reyn Studio engineering report</title>
<style>
:root {{ color-scheme: light; }}
body {{ font-family: -apple-system, "Segoe UI", Helvetica, Arial, sans-serif; color: #1d1a16;
       background: #fbfaf8; margin: 40px auto; max-width: 860px; padding: 0 24px; line-height: 1.45; }}
h1 {{ font-size: 24px; margin-bottom: 2px; }}
h2 {{ font-size: 15px; margin-top: 28px; border-bottom: 1px solid #d8d2c8; padding-bottom: 4px;
     letter-spacing: 0.04em; text-transform: uppercase; color: #6b6156; }}
.meta {{ color: #6b6156; font-size: 12.5px; margin-bottom: 20px; }}
table {{ border-collapse: collapse; width: 100%; font-size: 13px; }}
th {{ text-align: left; font-weight: 500; color: #4d453b; padding: 5px 14px 5px 0; width: 42%;
     vertical-align: top; border-bottom: 1px solid #ece7df; }}
td {{ padding: 5px 0; border-bottom: 1px solid #ece7df; vertical-align: top; }}
.mono {{ font-family: "SF Mono", "JetBrains Mono", Menlo, Consolas, monospace; font-size: 12px;
        word-break: break-all; }}
.source {{ color: #6b6156; font-size: 12px; }}
.limits {{ background: #f4efe7; border: 1px solid #ddd2c0; border-radius: 6px; padding: 14px 16px;
          font-size: 12.5px; margin-top: 10px; }}
.limits ul {{ margin: 6px 0 0 18px; padding: 0; }}
figure {{ margin: 12px 0; }}
figure img {{ max-width: 480px; width: 100%; image-rendering: pixelated; border: 1px solid #d8d2c8; }}
figcaption {{ font-size: 12px; color: #6b6156; margin-top: 6px; }}
</style>
</head>
<body>
<h1>{title}</h1>
<p class="meta">Reyn Studio engineering report · generated {generated} · app {app_version} ·
schema {schema} · model-derived fluid loads, not structural stress</p>

<h2>Provenance</h2>
<table>{provenance}</table>

<h2>Operating point</h2>
<table>{operating_rows}</table>

<h2>Geometry &amp; preflight</h2>
<table>{geometry_rows}</table>

<h2>Force &amp; moment coefficients</h2>
<p class="meta">Coefficients are area-weighted over the diffuse immersed interface;
physical values use the recorded q∞ scaling. Reference frame: {coefficient_frame}.
Any body orientation rotates the geometry, never these axes.</p>
<table>
<tr><th>Quantity</th><td><b>Coefficient (1)</b></td><td><b>Physical</b></td><td><b>Source · method</b></td></tr>
{results_rows}</table>

<h2>Derived scalars</h2>
<table><tr><th>Quantity</th><td><b>Value</b></td><td><b>Source · method</b></td></tr>
{scalar_rows}</table>
{warnings_html}{figure_html}
<h2>Limitations</h2>
<div class="limits">
This report presents <b>model-derived</b> results from a neural fixed-body surrogate.
<ul>
<li>Velocity is a model prediction; pressure is <b>recovered</b> from predicted velocity, not independently solved or measured.</li>
<li>Cp uses the recorded p∞, ρ∞, V∞ nondimensionalization; its pressure source remains recovered.</li>
<li>Surface tractions are pressure + Newtonian viscous fluid loads on a diffuse interface — they are <b>not structural stress</b> and are not independently validated loads.</li>
<li>No independent spatial error is shown without an attached solver reference.</li>
<li>The supported contract is external fixed-body flow with +X free stream inside the qualified Reynolds envelope; STL and single-part STEP import are managed tessellated preprocessing, not embedded or associative CAD.</li>
</ul>
</div>
</body>
</html>
"#,
        title = escape(&case.name),
        coefficient_frame = escape(crate::engineering::COEFFICIENT_REFERENCE_FRAME),
        generated = crate::app::format_utc(input.generated_utc_unix),
        app_version = escape(input.app_version),
        schema = REPORT_SCHEMA,
    );
    Ok(html)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engineering::{
        EngineeringResult, GeometryImportStep, GeometryPreflight, LengthUnit, ModelSupport,
        OperatingPoint,
    };
    use crate::units::NumberNotation;

    fn fixture_case() -> ExternalFlowCase {
        ExternalFlowCase {
            stage: crate::engineering::CaseStage::Results,
            case_id: "case-1".into(),
            name: "bracket <A> & fairing".into(),
            source_name: "bracket.stl".into(),
            source_revision_id: Some("source-rev-1".into()),
            case_revision_id: Some("case-rev-1".into()),
            model_id: "flow3d_obs_v1.pth".into(),
            model_sha256: Some("c".repeat(64)),
            model_max_steps: 64,
            model_support: ModelSupport::default(),
            preflight: GeometryPreflight {
                source_sha256: "d".repeat(64),
                analyzed_mesh_sha256: "e".repeat(64),
                import_steps: vec![GeometryImportStep {
                    evidence_class: "derived_geometry".into(),
                    operation: "tessellate".into(),
                    parameters: vec![("chord_tolerance".into(), "0.001".into())],
                    ..GeometryImportStep::default()
                }],
                source_bytes: 2048,
                triangles: 1234,
                components: 1,
                source_extents: [0.12, 0.05, 0.02],
                proposed_scale: 1.0,
                solver_characteristic_length: 0.6,
                target_grid: 64,
                solid_voxels: 900,
                voxel_components: 1,
                minimum_cells_across: 4,
                boundary_clearance_cells: 5,
                waivers: vec!["mesh.open_boundary: accepted for screening".into()],
                ..GeometryPreflight::default()
            },
            operating: OperatingPoint {
                length_unit: LengthUnit::Meter,
                reference_length: 0.12,
                velocity: 30.0,
                density: 1.225,
                viscosity: 1.81e-5,
                reference_pressure: 101_325.0,
                horizon_steps: 8,
                ..OperatingPoint::default()
            },
            review_focus: crate::engineering::ReviewFocus::default(),
            result: Some(EngineeringResult {
                method: "diffuse_interface_traction.v1".into(),
                cp_min: -1.62,
                cp_max: 1.01,
                force_coefficients: [0.82, 0.01, -0.03],
                moment_coefficients: [0.001, 0.02, -0.005],
                force_newtons: [4.1, 0.05, -0.15],
                moment_newton_meters: [0.002, 0.04, -0.01],
                surface_area_m2: 0.031,
                pressure_force_fraction: 0.83,
                load_hotspot: [0.0; 3],
                suction_hotspot: [0.0; 3],
                divergence_rms: 2.1e-3,
                wake_deficit_peak: 0.45,
                wake_deficit_mean: 0.12,
                moment_origin_mode: "diffuse_surface_centroid".into(),
                semigroup: Some(0.015),
                warnings: vec!["horizon near support limit".into()],
                ..Default::default()
            }),
            parent_run_id: None,
            named_regions: Vec::new(),
            view_state: crate::engineering::CaseViewState::default(),
        }
    }

    fn input(case: &ExternalFlowCase, system: UnitSystem) -> ReportInput<'_> {
        ReportInput {
            case,
            run_id: "run-abc-123",
            run_created_utc_unix: 1_784_000_000,
            generated_utc_unix: 1_784_000_100,
            app_version: "0.1.0",
            unit_system: system,
            format: ValueFormat {
                significant_digits: 4,
                notation: NumberNotation::Auto,
            },
            section_figure: None,
        }
    }

    #[test]
    fn report_carries_full_provenance_and_limitations() {
        let case = fixture_case();
        let html = engineering_report_html(&input(&case, UnitSystem::Si)).unwrap();
        assert!(html.contains(&"d".repeat(64)), "source hash");
        assert!(html.contains(&"e".repeat(64)), "analyzed mesh hash");
        assert!(html.contains(&"c".repeat(64)), "model hash");
        assert!(html.contains("tessellate · chord_tolerance=0.001"));
        assert!(html.contains("run-abc-123"));
        assert!(html.contains("case-rev-1"));
        assert!(html.contains(engineering::SURFACE_LOAD_METHOD));
        assert!(html.contains(engineering::EXTERNAL_FLOW_CONTRACT));
        assert!(html.contains("not structural stress"));
        assert!(html.contains("recovered"));
        assert!(html.contains(REPORT_SCHEMA));
        assert!(html.contains(engineering::ENGINEERING_RESULT_SCHEMA));
        assert!(html.contains("Named waiver"));
        assert!(html.contains("horizon near support limit"));
        assert!(html.contains("Source · method"));
        assert!(html.contains("DERIVED · physical-reference Cp from recovered pressure"));
    }

    #[test]
    fn report_escapes_untrusted_strings() {
        let mut case = fixture_case();
        case.name = "<script>alert('x')</script>".into();
        case.source_name = "part<&>.stl".into();
        case.result.as_mut().unwrap().method = "traction<&>.v1".into();
        case.result
            .as_mut()
            .unwrap()
            .warnings
            .push("<img src=x onerror=alert(1)>".into());
        let html = engineering_report_html(&input(&case, UnitSystem::Si)).unwrap();
        assert!(!html.contains("<script>alert"));
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("part&lt;&amp;&gt;.stl"));
        assert!(html.contains("traction&lt;&amp;&gt;.v1"));
    }

    #[test]
    fn report_converts_units_for_imperial_display() {
        let case = fixture_case();
        let html = engineering_report_html(&input(&case, UnitSystem::Imperial)).unwrap();
        // 30 m/s = 98.43 ft/s; the SI value stays readable in parentheses.
        assert!(html.contains("98.43 ft/s"), "converted velocity");
        assert!(html.contains("30.00 m/s"), "SI value preserved");
        assert!(html.contains("lbf"));
    }

    #[test]
    fn report_requires_a_completed_run() {
        let mut case = fixture_case();
        case.result = None;
        assert!(engineering_report_html(&input(&case, UnitSystem::Si)).is_err());
        let case = fixture_case();
        let mut no_run = input(&case, UnitSystem::Si);
        no_run.run_id = " ";
        assert!(engineering_report_html(&no_run).is_err());

        let mut draft_with_result = fixture_case();
        draft_with_result.stage = crate::engineering::CaseStage::Setup;
        assert!(
            engineering_report_html(&input(&draft_with_result, UnitSystem::Si))
                .unwrap_err()
                .contains("Results or Evidence")
        );
    }

    #[test]
    fn report_rejects_nonfinite_values_and_incomplete_provenance() {
        let mut case = fixture_case();
        case.result.as_mut().unwrap().force_newtons[1] = f64::NAN;
        assert!(engineering_report_html(&input(&case, UnitSystem::Si))
            .unwrap_err()
            .contains("non-finite engineering result"));

        let mut case = fixture_case();
        case.source_revision_id = None;
        assert!(engineering_report_html(&input(&case, UnitSystem::Si))
            .unwrap_err()
            .contains("source revision"));

        let mut case = fixture_case();
        case.model_sha256 = Some("C".repeat(64));
        assert!(engineering_report_html(&input(&case, UnitSystem::Si))
            .unwrap_err()
            .contains("canonical lowercase model SHA-256"));

        let case = fixture_case();
        let mut time_reversed = input(&case, UnitSystem::Si);
        time_reversed.generated_utc_unix = time_reversed.run_created_utc_unix - 1;
        assert!(engineering_report_html(&time_reversed)
            .unwrap_err()
            .contains("cannot precede"));
    }

    #[test]
    fn report_embeds_a_section_figure_when_supplied() {
        const ONE_PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let case = fixture_case();
        let mut with_figure = input(&case, UnitSystem::Si);
        with_figure.section_figure = Some(SectionFigure {
            png_base64: ONE_PIXEL_PNG.into(),
            caption: "X section · physical-reference Cp".into(),
        });
        let html = engineering_report_html(&with_figure).unwrap();
        assert!(html.contains(&format!("data:image/png;base64,{ONE_PIXEL_PNG}")));
        assert!(html.contains("X section"));

        let mut invalid_figure = input(&case, UnitSystem::Si);
        invalid_figure.section_figure = Some(SectionFigure {
            png_base64: "AAAA&quot; onerror=&quot;alert(1)".into(),
            caption: "unsafe".into(),
        });
        assert!(engineering_report_html(&invalid_figure)
            .unwrap_err()
            .contains("not valid base64"));
    }

    #[test]
    fn report_bytes_are_deterministic_and_locale_independent() {
        let case = fixture_case();
        let first = engineering_report_html(&input(&case, UnitSystem::Si)).unwrap();
        let second = engineering_report_html(&input(&case, UnitSystem::Si)).unwrap();
        assert_eq!(first.as_bytes(), second.as_bytes());
        assert!(first.contains("30.00 m/s"));
        assert!(!first.contains("30,00 m/s"));
        assert!(!first.contains("NaN"));
        assert!(!first.contains(">inf<"));
    }
}
