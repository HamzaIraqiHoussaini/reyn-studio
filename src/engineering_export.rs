//! Lab-sheet PNG/PDF presentation of an engineering-case report.
//!
//! Derived from the same `ReportInput` contract as the HTML report. The
//! rendered bytes carry the HTML report's content digest so a PNG/PDF can be
//! regenerated and compared without inventing numbers.

use crate::report::{engineering_report_html, ReportInput, REPORT_SCHEMA};
use crate::signing::{
    self, PublicKeyRecord, SignedEvidenceArtifact, SigningKeyProvider, SigningLineage,
};
use flate2::{write::ZlibEncoder, Compression};
use fontdue::{Font, FontSettings};
use sha2::{Digest, Sha256};
use std::io::Write;

pub const LAB_SHEET_SCHEMA: &str = "reyn_engineering_report_labsheet.v1";
const PAPER: [u8; 3] = [248, 246, 243];
const INK: [u8; 3] = [41, 29, 22];
const MUTED: [u8; 3] = [103, 82, 71];
const HAIRLINE: [u8; 3] = [205, 193, 185];
const EMBER: [u8; 3] = [194, 79, 8];
const WIDTH: u32 = 1_240;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LabSheetFormat {
    Png,
    Pdf,
}

impl LabSheetFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Pdf => "pdf",
        }
    }
}

#[derive(Clone, Debug)]
pub struct LabSheetArtifact {
    #[allow(dead_code)]
    pub format: LabSheetFormat,
    pub bytes: Vec<u8>,
    pub html_sha256: String,
    pub content_sha256: String,
    pub signature: Option<SignedEvidenceArtifact>,
}

/// Render a printable lab sheet from an engineering report input.
pub fn engineering_report_labsheet(
    input: &ReportInput<'_>,
    format: LabSheetFormat,
) -> Result<LabSheetArtifact, String> {
    let html = engineering_report_html(input)?;
    let html_sha256 = hex_sha256(html.as_bytes());
    let lines = lab_sheet_lines(input, &html_sha256)?;
    let canvas = render_lines(&lines)?;
    let bytes = match format {
        LabSheetFormat::Png => encode_png(&canvas, &html_sha256)?,
        LabSheetFormat::Pdf => encode_pdf(&canvas, &html_sha256)?,
    };
    Ok(LabSheetArtifact {
        format,
        content_sha256: hex_sha256(&bytes),
        html_sha256,
        signature: None,
        bytes,
    })
}

/// Sign the HTML report digest as engineering evidence and attach the sidecar.
pub fn engineering_report_labsheet_signed(
    input: &ReportInput<'_>,
    format: LabSheetFormat,
    provider: &dyn SigningKeyProvider,
    key: &PublicKeyRecord,
    key_is_revoked: bool,
    created_utc_unix: u64,
) -> Result<LabSheetArtifact, String> {
    let mut artifact = engineering_report_labsheet(input, format)?;
    let lineage = SigningLineage {
        run_id: input.run_id.to_owned(),
        report_schema: REPORT_SCHEMA.into(),
        canonical_report_sha256: artifact.html_sha256.clone(),
        canonical_payload_sha256: artifact.html_sha256.clone(),
        created_utc_unix,
    };
    let signed = signing::sign_canonical_payload(provider, key, key_is_revoked, &lineage)
        .map_err(|error| error.to_string())?;
    artifact.signature = Some(signed);
    Ok(artifact)
}

/// Sign the immutable engineering-result JSON digest (content-addressed SHA-256).
///
/// The result document is both report and payload: one digest is signed, matching
/// how lab-sheet HTML digests are signed. The immutable JSON bytes stay unsigned;
/// authenticity is a detached sidecar.
pub fn sign_engineering_result_digest(
    provider: &dyn SigningKeyProvider,
    key: &PublicKeyRecord,
    key_is_revoked: bool,
    run_id: &str,
    content_sha256: &str,
    created_utc_unix: u64,
) -> Result<SignedEvidenceArtifact, String> {
    let lineage = SigningLineage {
        run_id: run_id.to_owned(),
        report_schema: crate::engineering::ENGINEERING_RESULT_SCHEMA.into(),
        canonical_report_sha256: content_sha256.to_owned(),
        canonical_payload_sha256: content_sha256.to_owned(),
        created_utc_unix,
    };
    signing::sign_canonical_payload(provider, key, key_is_revoked, &lineage)
        .map_err(|error| error.to_string())
}

fn lab_sheet_lines(input: &ReportInput<'_>, html_sha256: &str) -> Result<Vec<SheetLine>, String> {
    let case = input.case;
    let result = case
        .result
        .as_ref()
        .ok_or_else(|| "A completed run result is required for a report.".to_string())?;
    let mut lines = Vec::new();
    lines.push(SheetLine::Title(
        "Reyn Studio · Engineering case report".into(),
    ));
    lines.push(SheetLine::Muted(format!(
        "{LAB_SHEET_SCHEMA} · app {} · generated {}",
        input.app_version,
        crate::app::format_utc(input.generated_utc_unix)
    )));
    lines.push(SheetLine::Rule);
    lines.push(SheetLine::Heading("Provenance".into()));
    lines.push(SheetLine::Kv(
        "Case".into(),
        format!("{} · {}", case.name, case.case_id),
    ));
    lines.push(SheetLine::Kv("Run".into(), input.run_id.to_owned()));
    lines.push(SheetLine::Kv(
        "Source SHA-256".into(),
        case.preflight.source_sha256.clone(),
    ));
    if !case.preflight.analyzed_mesh_sha256.is_empty() {
        lines.push(SheetLine::Kv(
            "Analyzed mesh SHA-256".into(),
            case.preflight.analyzed_mesh_sha256.clone(),
        ));
    }
    for step in &case.preflight.import_steps {
        let parameters = step
            .parameters
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(" · ");
        lines.push(SheetLine::Kv(
            "Import derivation".into(),
            if parameters.is_empty() {
                step.operation.clone()
            } else {
                format!("{} · {parameters}", step.operation)
            },
        ));
    }
    lines.push(SheetLine::Kv(
        "Model SHA-256".into(),
        case.model_sha256
            .clone()
            .unwrap_or_else(|| "UNKNOWN".into()),
    ));
    lines.push(SheetLine::Kv("HTML digest".into(), html_sha256.to_owned()));
    lines.push(SheetLine::Heading("Operating point".into()));
    lines.push(SheetLine::Kv(
        "Velocity".into(),
        format!("{:.6} m/s", case.operating.velocity),
    ));
    lines.push(SheetLine::Kv(
        "Density".into(),
        format!("{:.6} kg/m³", case.operating.density),
    ));
    lines.push(SheetLine::Kv(
        "Viscosity".into(),
        format!("{:.6e} Pa·s", case.operating.viscosity),
    ));
    lines.push(SheetLine::Kv(
        "Reynolds".into(),
        case.operating
            .reynolds()
            .map(|value| format!("{value:.3}"))
            .unwrap_or_else(|| "UNKNOWN".into()),
    ));
    lines.push(SheetLine::Kv(
        "Horizon".into(),
        format!("{} steps", case.operating.horizon_steps),
    ));
    lines.push(SheetLine::Heading("Loads".into()));
    lines.push(SheetLine::Kv(
        "Force [N]".into(),
        format!(
            "[{:.6}, {:.6}, {:.6}] · RECOVERED",
            result.force_newtons[0], result.force_newtons[1], result.force_newtons[2]
        ),
    ));
    lines.push(SheetLine::Kv(
        "Moment [N·m]".into(),
        format!(
            "[{:.6}, {:.6}, {:.6}] · RECOVERED",
            result.moment_newton_meters[0],
            result.moment_newton_meters[1],
            result.moment_newton_meters[2]
        ),
    ));
    lines.push(SheetLine::Kv(
        "Cp range".into(),
        format!("[{:.4}, {:.4}] · RECOVERED", result.cp_min, result.cp_max),
    ));
    lines.push(SheetLine::Kv("Method".into(), result.method.clone()));
    if !result.warnings.is_empty() {
        lines.push(SheetLine::Heading("Warnings".into()));
        for warning in &result.warnings {
            lines.push(SheetLine::Body(format!("• {warning}")));
        }
    }
    lines.push(SheetLine::Heading("Limitations".into()));
    lines.push(SheetLine::Body(
        "Fixed-body external incompressible flow · +X free stream · model horizon only · recovered pressure / physical Cp are recovered, not independently measured."
            .into(),
    ));
    lines.push(SheetLine::Muted(
        "UNSIGNED presentation artifact · SI values are authoritative.".into(),
    ));
    Ok(lines)
}

#[derive(Clone, Debug)]
enum SheetLine {
    Title(String),
    Heading(String),
    Kv(String, String),
    Body(String),
    Muted(String),
    Rule,
}

struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Self {
        let mut pixels = vec![0; width as usize * height as usize * 3];
        for pixel in pixels.chunks_exact_mut(3) {
            pixel.copy_from_slice(&PAPER);
        }
        Self {
            width,
            height,
            pixels,
        }
    }

    fn fill_rect(&mut self, x: i32, y: i32, width: i32, height: i32, color: [u8; 3]) {
        let x0 = x.max(0).min(self.width as i32);
        let y0 = y.max(0).min(self.height as i32);
        let x1 = (x + width).max(0).min(self.width as i32);
        let y1 = (y + height).max(0).min(self.height as i32);
        for row in y0..y1 {
            for column in x0..x1 {
                let index = (row as usize * self.width as usize + column as usize) * 3;
                self.pixels[index..index + 3].copy_from_slice(&color);
            }
        }
    }

    fn text(&mut self, font: &Font, value: &str, size: f32, x: i32, y: i32, color: [u8; 3]) {
        let mut pen_x = x as f32;
        let baseline = y as f32 + size;
        for character in value.chars() {
            let (metrics, bitmap) = font.rasterize(character, size);
            let glyph_x = pen_x.round() as i32 + metrics.xmin;
            let glyph_y = baseline.round() as i32 - metrics.height as i32 - metrics.ymin;
            for row in 0..metrics.height {
                let target_y = glyph_y + row as i32;
                if !(0..self.height as i32).contains(&target_y) {
                    continue;
                }
                for column in 0..metrics.width {
                    let target_x = glyph_x + column as i32;
                    if !(0..self.width as i32).contains(&target_x) {
                        continue;
                    }
                    let alpha = bitmap[row * metrics.width + column] as u32;
                    if alpha == 0 {
                        continue;
                    }
                    let index = (target_y as usize * self.width as usize + target_x as usize) * 3;
                    for channel in 0..3 {
                        let background = self.pixels[index + channel] as u32;
                        let foreground = color[channel] as u32;
                        self.pixels[index + channel] =
                            ((foreground * alpha + background * (255 - alpha)) / 255) as u8;
                    }
                }
            }
            pen_x += metrics.advance_width;
        }
    }
}

fn render_lines(lines: &[SheetLine]) -> Result<Canvas, String> {
    let regular = Font::from_bytes(
        include_bytes!("../assets/Inter-Regular.ttf") as &[u8],
        FontSettings::default(),
    )
    .map_err(|error| format!("could not load report font: {error}"))?;
    let mono = Font::from_bytes(
        include_bytes!("../assets/JetBrainsMono-Regular.ttf") as &[u8],
        FontSettings::default(),
    )
    .map_err(|error| format!("could not load mono font: {error}"))?;

    let mut height = 64i32;
    for line in lines {
        height += match line {
            SheetLine::Title(_) => 42,
            SheetLine::Heading(_) => 36,
            SheetLine::Kv(_, _) => 28,
            SheetLine::Body(_) => 24,
            SheetLine::Muted(_) => 22,
            SheetLine::Rule => 18,
        };
    }
    height += 48;
    let mut canvas = Canvas::new(WIDTH, height.max(640) as u32);
    canvas.fill_rect(40, 28, 8, 28, EMBER);
    let mut y = 36i32;
    for line in lines {
        match line {
            SheetLine::Title(text) => {
                canvas.text(&regular, text, 28.0, 60, y, INK);
                y += 42;
            }
            SheetLine::Heading(text) => {
                y += 8;
                canvas.text(&regular, text, 16.0, 48, y, EMBER);
                y += 28;
            }
            SheetLine::Kv(label, value) => {
                canvas.text(&mono, label, 12.0, 48, y, MUTED);
                canvas.text(&mono, value, 12.0, 220, y, INK);
                y += 28;
            }
            SheetLine::Body(text) => {
                canvas.text(&regular, text, 13.0, 48, y, INK);
                y += 24;
            }
            SheetLine::Muted(text) => {
                canvas.text(&mono, text, 11.0, 48, y, MUTED);
                y += 22;
            }
            SheetLine::Rule => {
                canvas.fill_rect(48, y + 6, (WIDTH as i32) - 96, 1, HAIRLINE);
                y += 18;
            }
        }
    }
    Ok(canvas)
}

fn encode_png(canvas: &Canvas, html_sha256: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, canvas.width, canvas.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .add_text_chunk("ReynEngineeringHtmlSHA256".into(), html_sha256.into())
            .map_err(|error| error.to_string())?;
        encoder
            .add_text_chunk("ReynAuthenticityStatus".into(), "UNSIGNED".into())
            .map_err(|error| error.to_string())?;
        encoder
            .add_text_chunk("ReynReportSchema".into(), LAB_SHEET_SCHEMA.into())
            .map_err(|error| error.to_string())?;
        let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
        writer
            .write_image_data(&canvas.pixels)
            .map_err(|error| error.to_string())?;
    }
    Ok(bytes)
}

fn encode_pdf(canvas: &Canvas, html_sha256: &str) -> Result<Vec<u8>, String> {
    let mut compressed = ZlibEncoder::new(Vec::new(), Compression::best());
    compressed
        .write_all(&canvas.pixels)
        .map_err(|error| error.to_string())?;
    let compressed = compressed.finish().map_err(|error| error.to_string())?;
    const PAGE_W: f64 = 595.0;
    const PAGE_H: f64 = 842.0;
    let source_ratio = canvas.width as f64 / canvas.height as f64;
    let page_ratio = PAGE_W / PAGE_H;
    let (draw_w, draw_h) = if source_ratio > page_ratio {
        (PAGE_W, PAGE_W / source_ratio)
    } else {
        (PAGE_H * source_ratio, PAGE_H)
    };
    let draw_x = (PAGE_W - draw_w) / 2.0;
    let draw_y = (PAGE_H - draw_h) / 2.0;
    let content =
        format!("q\n{draw_w:.4} 0 0 {draw_h:.4} {draw_x:.4} {draw_y:.4} cm\n/Report Do\nQ\n");
    let info = format!(
        "<< /Title (Reyn Studio Engineering Case Report) /Creator (Reyn Studio) /Subject (HTML digest SHA-256: {html_sha256}; Authenticity: UNSIGNED) >>"
    );
    let stream = |header: String, data: &[u8]| {
        let mut out = header.into_bytes();
        out.extend_from_slice(b"\nstream\n");
        out.extend_from_slice(data);
        out.extend_from_slice(b"\nendstream");
        out
    };
    let objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W:.0} {PAGE_H:.0}] /Resources << /XObject << /Report 4 0 R >> >> /Contents 5 0 R >>"
        )
        .into_bytes(),
        stream(
            format!(
                "<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode /Length {} >>",
                canvas.width,
                canvas.height,
                compressed.len()
            ),
            &compressed,
        ),
        stream(format!("<< /Length {} >>", content.len()), content.as_bytes()),
        info.into_bytes(),
    ];
    let mut pdf = format!(
        "%PDF-1.4\n% ReynEngineeringHtmlSHA256: {html_sha256}\n% ReynAuthenticityStatus: UNSIGNED\n"
    )
    .into_bytes();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        write!(&mut pdf, "{} 0 obj\n", index + 1).map_err(|error| error.to_string())?;
        pdf.extend_from_slice(object);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    write!(
        &mut pdf,
        "xref\n0 {}\n0000000000 65535 f \n",
        objects.len() + 1
    )
    .map_err(|error| error.to_string())?;
    for offset in offsets {
        writeln!(&mut pdf, "{offset:010} 00000 n ").map_err(|error| error.to_string())?;
    }
    let file_id = hex_sha256(html_sha256.as_bytes());
    write!(
        &mut pdf,
        "trailer\n<< /Size {} /Root 1 0 R /Info 6 0 R /ID [<{file_id}><{file_id}>] >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    )
    .map_err(|error| error.to_string())?;
    Ok(pdf)
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engineering::{
        CaseViewState, EngineeringResult, ExternalFlowCase, GeometryPreflight, LengthUnit,
        ModelSupport, OperatingPoint,
    };
    use crate::units::{NumberNotation, UnitSystem, ValueFormat};

    fn fixture_case() -> ExternalFlowCase {
        ExternalFlowCase {
            stage: crate::engineering::CaseStage::Results,
            case_id: "case-1".into(),
            name: "bracket".into(),
            source_name: "bracket.stl".into(),
            source_revision_id: Some("source-rev-1".into()),
            case_revision_id: Some("case-rev-1".into()),
            model_id: "flow3d_obs_v1.pth".into(),
            model_sha256: Some("c".repeat(64)),
            model_max_steps: 64,
            model_support: ModelSupport::default(),
            preflight: GeometryPreflight {
                source_sha256: "d".repeat(64),
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
            view_state: CaseViewState::default(),
        }
    }

    fn input(case: &ExternalFlowCase) -> ReportInput<'_> {
        ReportInput {
            case,
            run_id: "11111111-1111-1111-1111-111111111111",
            run_created_utc_unix: 1_784_000_000,
            generated_utc_unix: 1_784_000_100,
            app_version: "0.2.0",
            unit_system: UnitSystem::Si,
            format: ValueFormat {
                significant_digits: 4,
                notation: NumberNotation::Auto,
            },
            section_figure: None,
        }
    }

    #[test]
    fn labsheet_png_and_pdf_signatures() {
        let case = fixture_case();
        let png = engineering_report_labsheet(&input(&case), LabSheetFormat::Png).expect("png");
        assert!(png.bytes.starts_with(&[0x89, b'P', b'N', b'G']));
        let pdf = engineering_report_labsheet(&input(&case), LabSheetFormat::Pdf).expect("pdf");
        assert!(pdf.bytes.starts_with(b"%PDF"));
        assert!(pdf
            .bytes
            .windows(b"ReynAuthenticityStatus: UNSIGNED".len())
            .any(|window| window == b"ReynAuthenticityStatus: UNSIGNED"));
    }

    #[test]
    fn engineering_result_digest_signs_identical_report_and_payload_hashes() {
        let provider = crate::signing::DeterministicTestProvider::new("eng-result");
        let key = provider.public_key_record();
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let signed = sign_engineering_result_digest(
            &provider,
            &key,
            false,
            "61f596e7-8414-488e-b764-0a1dfe671d1a",
            digest,
            42,
        )
        .expect("sign");
        assert_eq!(
            signed.source.report_schema,
            crate::engineering::ENGINEERING_RESULT_SCHEMA
        );
        assert_eq!(signed.source.canonical_report_sha256, digest);
        assert_eq!(signed.source.canonical_payload_sha256, digest);
        let policy = crate::signing::VerificationPolicy::new(
            [key.key_fingerprint_sha256.clone()],
            std::iter::empty::<String>(),
        );
        assert_eq!(
            crate::signing::verify_signed_hash(digest, digest, &signed, &policy).status,
            crate::signing::VerificationStatus::VerifiedTrustedKey
        );
    }
}
