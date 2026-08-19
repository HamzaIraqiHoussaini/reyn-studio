//! Off-UI-thread preparation of a completed CAD field for immutable persistence
//! and first paint. Heavy encode / hash / display work happens here; the UI
//! thread only inserts pre-digested bytes and installs the ready package.

use crate::cad;
use crate::engine;
use crate::engineering::{self, EngineeringResult};
use crate::flow;
use crate::project::DigestedBytes;

/// Pure package produced off the UI thread for one completed `predict_cad`.
pub struct CadFieldReady {
    pub request_id: String,
    pub run_id: String,
    pub case_revision_id: String,
    pub n: usize,
    pub horizon: u32,
    pub reynolds: f32,
    pub dt_frame: f32,
    pub field: DigestedBytes,
    pub result: DigestedBytes,
    pub result_json: serde_json::Value,
    pub engineering_result: EngineeringResult,
    pub velocity: Vec<f32>,
    pub pressure: Vec<f32>,
    pub mask: Vec<f32>,
    pub cp: Vec<f32>,
    pub traction: Vec<f32>,
    pub mask_bounds: Option<([f32; 3], [f32; 3])>,
    pub particles: Vec<flow::Particle>,
    pub volume_data: Option<(Vec<u8>, [u32; 3])>,
    pub insights: Vec<flow::Insight3D>,
    pub surface_mask_u8: Vec<u8>,
    pub surface_cp_u8: Vec<u8>,
}

/// Soft Brinkman occupancy peaks below 1.0; this display floor recovers a
/// filled shell so Results does not look hollow.
pub const DISPLAY_OCCUPANCY_FLOOR: f32 = 0.2;

pub fn surface_mask_u8_sample(occupancy: f32) -> u8 {
    if occupancy > DISPLAY_OCCUPANCY_FLOOR {
        255
    } else {
        0
    }
}

/// Identity needed to seal the engineering-result JSON before persistence.
pub struct CadFieldReadyIdentity {
    pub run_id: String,
    pub case_revision_id: String,
}

pub fn prepare_cad_field_ready(
    field: engine::CadField,
    identity: CadFieldReadyIdentity,
) -> Result<CadFieldReady, String> {
    let field_bytes = engineering::encode_engineering_field_slices(
        field.n,
        &field.vel,
        &field.pressure,
        &field.mask,
        &field.cp,
        &field.traction,
    )?;
    let field_digested = DigestedBytes::new(field_bytes);
    let field_sha256 = field_digested.digest().to_owned();

    let cp_min = field.cp.iter().copied().fold(f32::INFINITY, f32::min);
    let cp_max = field.cp.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let result_json = serde_json::json!({
        "schema": engineering::ENGINEERING_RESULT_SCHEMA,
        "field_schema": engineering::ENGINEERING_FIELD_SCHEMA,
        "field_sha256": field_sha256,
        "submitted_request_id": field.request_id,
        "run_id": identity.run_id,
        "case_revision_id": identity.case_revision_id,
        "method": field.load_method,
        "grid": field.n,
        "horizon": field.horizon,
        "reynolds": field.reynolds,
        "solver_characteristic_length": field.characteristic_length_solver,
        "solver_dt": field.solver_dt,
        "solver_stride": field.solver_stride,
        "warmup_steps": field.warmup_steps,
        "dt_frame": field.dt_frame,
        "cp": {
            "minimum": cp_min,
            "maximum": cp_max,
            "source": "derived_from_recovered_pressure",
        },
        "force_coefficients": field.force_coefficients,
        "moment_coefficients": field.moment_coefficients,
        "force_newtons": field.force_newtons,
        "moment_newton_meters": field.moment_newton_meters,
        "moment_reference": field.moment_origin_mode,
        "moment_origin_mode": field.moment_origin_mode,
        "moment_origin_solver": field.moment_origin_solver,
        "surface_centroid_solver": field.surface_centroid_solver,
        "coefficient_reference": field.coefficient_reference,
        "reference_area_m2": field.reference_area_m2,
        "surface_area_m2": field.surface_area_m2,
        "pressure_force_fraction": field.pressure_force_fraction,
        "load_hotspot_m": field.load_hotspot,
        "suction_hotspot_m": field.suction_hotspot,
        "divergence_rms": field.divergence_rms,
        "wake_deficit_peak": field.wake_deficit_peak,
        "wake_deficit_mean": field.wake_deficit_mean,
        "warnings": field.warnings,
    });
    let result_bytes =
        serde_json::to_vec_pretty(&result_json).map_err(|error| error.to_string())?;
    let result_digested = DigestedBytes::new(result_bytes);

    let shape = [3usize, field.n, field.n, field.n];
    let particles = flow::from_field(&shape, &field.vel);
    let volume_data = flow::vorticity_volume(&shape, &field.vel);
    let mut insights = flow::insights3d(&shape, &field.vel);
    insights.extend(cad::surface_insights(&field.mask, &field.cp, field.n));
    let mask_bounds = cad::mask_bounds(&field.mask, field.n);
    let cp_scale = field
        .cp
        .iter()
        .fold(0.0f32, |scale, value| scale.max(value.abs()))
        .max(1e-6);
    let n = field.n;
    let mut surface_mask_u8 = vec![0u8; n * n * n];
    let mut surface_cp_u8 = vec![128u8; n * n * n];
    for i in 0..n {
        for j in 0..n {
            for k in 0..n {
                let src = i * n * n + j * n + k;
                let dst = (k * n + j) * n + i;
                surface_mask_u8[dst] = surface_mask_u8_sample(field.mask[src]);
                let t = (field.cp[src] / cp_scale) * 0.5 + 0.5;
                surface_cp_u8[dst] = (t.clamp(0.0, 1.0) * 255.0) as u8;
            }
        }
    }

    let engineering_result = EngineeringResult {
        method: field.load_method,
        cp_min: cp_min as f64,
        cp_max: cp_max as f64,
        force_coefficients: field.force_coefficients.map(f64::from),
        moment_coefficients: field.moment_coefficients.map(f64::from),
        force_newtons: field.force_newtons.map(f64::from),
        moment_newton_meters: field.moment_newton_meters.map(f64::from),
        surface_area_m2: field.surface_area_m2 as f64,
        pressure_force_fraction: field.pressure_force_fraction as f64,
        load_hotspot: field.load_hotspot.map(f64::from),
        suction_hotspot: field.suction_hotspot.map(f64::from),
        divergence_rms: field.divergence_rms as f64,
        wake_deficit_peak: field.wake_deficit_peak as f64,
        wake_deficit_mean: field.wake_deficit_mean as f64,
        moment_origin_mode: field.moment_origin_mode,
        moment_origin_solver: field.moment_origin_solver.map(f64::from),
        surface_centroid_solver: field.surface_centroid_solver.map(f64::from),
        coefficient_reference: field.coefficient_reference,
        reference_area_m2: field.reference_area_m2.map(f64::from),
        semigroup: field.semigroup.map(f64::from),
        warnings: field.warnings,
    };

    Ok(CadFieldReady {
        request_id: field.request_id,
        run_id: identity.run_id,
        case_revision_id: identity.case_revision_id,
        n: field.n,
        horizon: field.horizon,
        reynolds: field.reynolds,
        dt_frame: field.dt_frame,
        field: field_digested,
        result: result_digested,
        result_json,
        engineering_result,
        velocity: field.vel,
        pressure: field.pressure,
        mask: field.mask,
        cp: field.cp,
        traction: field.traction,
        mask_bounds,
        particles,
        volume_data,
        insights,
        surface_mask_u8,
        surface_cp_u8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_field(n: usize) -> engine::CadField {
        let cube = n * n * n;
        let mut mask = vec![0f32; cube];
        for i in n / 4..n / 2 {
            for j in n / 3..2 * n / 3 {
                for k in n / 3..2 * n / 3 {
                    mask[i * n * n + j * n + k] = 1.0;
                }
            }
        }
        engine::CadField {
            request_id: "req-ready".into(),
            n,
            vel: vec![1.0; 3 * cube],
            pressure: vec![101_325.0; cube],
            mask,
            cp: vec![0.1; cube],
            traction: vec![0.0; 3 * cube],
            horizon: 4,
            reynolds: 150.0,
            characteristic_length_solver: 0.6,
            solver_dt: 0.01,
            solver_stride: 1,
            warmup_steps: 0,
            dt_frame: 0.01,
            force_coefficients: [0.5, 0.0, 0.0],
            moment_coefficients: [0.0; 3],
            force_newtons: [1.0, 0.0, 0.0],
            moment_newton_meters: [0.0; 3],
            surface_area_m2: 1.0,
            pressure_force_fraction: 0.8,
            load_hotspot: [0.0; 3],
            suction_hotspot: [0.0; 3],
            divergence_rms: 1e-3,
            wake_deficit_peak: 0.1,
            wake_deficit_mean: 0.05,
            moment_origin_mode: "diffuse_surface_centroid".into(),
            moment_origin_solver: [0.0; 3],
            surface_centroid_solver: [0.0; 3],
            coefficient_reference: "q_inf * L_ref^2 ; q_inf * L_ref^3".into(),
            reference_area_m2: None,
            semigroup: None,
            load_method: engineering::SURFACE_LOAD_METHOD.into(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn prepare_packages_digests_without_cloning_through_blob() {
        let ready = prepare_cad_field_ready(
            synthetic_field(8),
            CadFieldReadyIdentity {
                run_id: "run-1".into(),
                case_revision_id: "case-rev-1".into(),
            },
        )
        .unwrap();
        assert_eq!(ready.field.digest().len(), 64);
        assert_eq!(ready.result.digest().len(), 64);
        assert_eq!(ready.result.len(), ready.result.bytes().len());
        assert_eq!(
            ready.result_json["field_sha256"].as_str(),
            Some(ready.field.digest())
        );
        assert_eq!(ready.surface_mask_u8.len(), 8 * 8 * 8);
        assert_eq!(
            ready.engineering_result.moment_origin_mode,
            "diffuse_surface_centroid"
        );
    }

    #[test]
    fn display_occupancy_floor_fills_soft_brinkman_shell() {
        assert_eq!(surface_mask_u8_sample(0.0), 0);
        assert_eq!(surface_mask_u8_sample(DISPLAY_OCCUPANCY_FLOOR), 0);
        assert_eq!(surface_mask_u8_sample(DISPLAY_OCCUPANCY_FLOOR + 0.01), 255);
        assert_eq!(surface_mask_u8_sample(1.0), 255);
        // Linear 0–255 encoding would leave a 0.21 occupancy almost black.
        assert_ne!(
            (0.21f32.clamp(0.0, 1.0) * 255.0) as u8,
            surface_mask_u8_sample(0.21)
        );
    }
}
