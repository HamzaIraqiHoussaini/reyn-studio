//! Deterministic 2D sections through persisted external-flow engineering fields.
//! This module is deliberately UI-free so orientation, physical scaling, and
//! mask linkage can be tested without an egui context.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SectionAxis {
    #[default]
    X,
    Y,
    Z,
}

impl SectionAxis {
    pub const ALL: [Self; 3] = [Self::X, Self::Y, Self::Z];

    pub fn label(self) -> &'static str {
        match self {
            Self::X => "X",
            Self::Y => "Y",
            Self::Z => "Z",
        }
    }

    pub fn horizontal_axis(self) -> &'static str {
        match self {
            Self::X => "Y",
            Self::Y | Self::Z => "X",
        }
    }

    pub fn vertical_axis(self) -> &'static str {
        match self {
            Self::X | Self::Y => "Z",
            Self::Z => "Y",
        }
    }

    pub fn id(self) -> u64 {
        match self {
            Self::X => 0,
            Self::Y => 1,
            Self::Z => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SectionQuantity {
    RecoveredPressure,
    #[default]
    PhysicalCp,
    VelocityMagnitude,
    VorticityMagnitude,
    FluidTractionMagnitude,
    WakeDeficit,
}

impl SectionQuantity {
    pub const ALL: [Self; 6] = [
        Self::RecoveredPressure,
        Self::PhysicalCp,
        Self::VelocityMagnitude,
        Self::VorticityMagnitude,
        Self::FluidTractionMagnitude,
        Self::WakeDeficit,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::RecoveredPressure => "Recovered pressure",
            Self::PhysicalCp => "Physical-reference Cp",
            Self::VelocityMagnitude => "Velocity magnitude",
            Self::VorticityMagnitude => "Vorticity magnitude",
            Self::FluidTractionMagnitude => "Fluid traction magnitude",
            Self::WakeDeficit => "Wake deficit",
        }
    }

    pub fn units(self) -> &'static str {
        match self {
            Self::RecoveredPressure | Self::FluidTractionMagnitude => "Pa",
            Self::PhysicalCp | Self::WakeDeficit => "1",
            Self::VelocityMagnitude => "m/s",
            Self::VorticityMagnitude => "1/s",
        }
    }

    pub fn source(self) -> &'static str {
        match self {
            Self::RecoveredPressure => "RECOVERED · model-predicted velocity",
            Self::PhysicalCp => "DERIVED · recovered pressure + operating point",
            Self::VelocityMagnitude => "MODEL PREDICTION · LIC texture on recorded velocity",
            Self::VorticityMagnitude => "DERIVED · model-predicted velocity",
            Self::FluidTractionMagnitude => "DERIVED FLUID LOAD · diffuse interface",
            Self::WakeDeficit => "DERIVED · model-predicted velocity",
        }
    }

    pub fn method(self) -> &'static str {
        match self {
            Self::RecoveredPressure => {
                "3D spectral Poisson recovery; recorded physical p∞, ρ∞, and V∞"
            }
            Self::PhysicalCp => "Cp=(p_recovered-p∞)/(0.5 ρ∞ V∞²)",
            Self::VelocityMagnitude => {
                "DirectFlowMap fixed-body prediction; scaled by recorded V∞; planar LIC"
            }
            Self::VorticityMagnitude => {
                "periodic central-difference curl on the 2π solver grid; physical scaling"
            }
            Self::FluidTractionMagnitude => {
                "diffuse_interface_traction.v1 vector magnitude; not structural stress"
            }
            Self::WakeDeficit => "max(0, 1-|u|/V∞)",
        }
    }

    pub fn signed(self) -> bool {
        matches!(self, Self::RecoveredPressure | Self::PhysicalCp)
    }

    pub fn id(self) -> u64 {
        match self {
            Self::RecoveredPressure => 0,
            Self::PhysicalCp => 1,
            Self::VelocityMagnitude => 2,
            Self::VorticityMagnitude => 3,
            Self::FluidTractionMagnitude => 4,
            Self::WakeDeficit => 5,
        }
    }
}

pub struct SectionInput<'a> {
    pub n: usize,
    /// Stored model velocity, nondimensionalized by the recorded free-stream speed.
    pub velocity: &'a [f32],
    pub pressure_pa: &'a [f32],
    pub mask: &'a [f32],
    pub cp: &'a [f32],
    pub traction_pa: &'a [f32],
    pub free_stream_mps: f32,
    pub reference_pressure_pa: f32,
    pub reference_length_m: f32,
    pub solver_characteristic_length: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SectionScale {
    pub minimum: f32,
    pub maximum: f32,
    pub center: Option<f32>,
    extent: f32,
}

impl SectionScale {
    /// Replace a signed (centered) scale's symmetric range with a pinned
    /// ±extent so sections compare across runs. No-op for magnitude scales.
    pub fn pinned(mut self, extent: f32) -> Self {
        if self.center.is_some() && extent > 0.0 && extent.is_finite() {
            self.center = Some(0.0);
            self.extent = extent;
            self.minimum = -extent;
            self.maximum = extent;
        }
        self
    }

    pub fn normalize(self, value: f32) -> f32 {
        if let Some(center) = self.center {
            ((value - center) / self.extent).clamp(-1.0, 1.0)
        } else {
            (value / self.extent).clamp(0.0, 1.0)
        }
    }

    pub fn legend_minimum(self) -> f32 {
        self.center.map_or(0.0, |center| center - self.extent)
    }

    pub fn legend_maximum(self) -> f32 {
        self.center
            .map_or(self.extent, |center| center + self.extent)
    }
}

#[derive(Debug, PartialEq)]
pub struct SectionPlane {
    pub n: usize,
    pub axis: SectionAxis,
    pub index: usize,
    /// Snapped location in the stored grid's normalized [0, 1] domain.
    pub location: f32,
    pub quantity: SectionQuantity,
    /// Top-to-bottom pixels have decreasing vertical-axis coordinate.
    pub values: Vec<f32>,
    /// The stored diffuse geometry mask in exactly the same orientation as `values`.
    pub mask: Vec<f32>,
    pub scale: SectionScale,
    /// In-plane velocity `(u_horizontal, u_vertical)` in section pixel axes for
    /// velocity LIC. Present only for [`SectionQuantity::VelocityMagnitude`].
    pub in_plane: Option<Vec<[f32; 2]>>,
}

impl SectionPlane {
    pub fn value(&self, row: usize, column: usize) -> f32 {
        self.values[row * self.n + column]
    }

    pub fn mask_value(&self, row: usize, column: usize) -> f32 {
        self.mask[row * self.n + column]
    }
}

pub fn section_index(n: usize, location: f32) -> Result<usize, String> {
    if n < 3 {
        return Err("Engineering section requires a grid of at least 3³.".into());
    }
    if !location.is_finite() {
        return Err("Engineering section location must be finite.".into());
    }
    Ok((location.clamp(0.0, 1.0) * (n - 1) as f32).round() as usize)
}

pub fn extract_section(
    input: &SectionInput<'_>,
    axis: SectionAxis,
    location: f32,
    quantity: SectionQuantity,
) -> Result<SectionPlane, String> {
    let n = input.n;
    let cube = n
        .checked_mul(n)
        .and_then(|value| value.checked_mul(n))
        .ok_or_else(|| "Engineering section grid dimensions overflow.".to_string())?;
    if n < 3 || input.mask.len() != cube {
        return Err("Stored engineering mask does not match its cubic grid.".into());
    }
    let scalar_len = match quantity {
        SectionQuantity::RecoveredPressure => input.pressure_pa.len(),
        SectionQuantity::PhysicalCp => input.cp.len(),
        SectionQuantity::VelocityMagnitude
        | SectionQuantity::VorticityMagnitude
        | SectionQuantity::WakeDeficit => input.velocity.len() / 3,
        SectionQuantity::FluidTractionMagnitude => input.traction_pa.len() / 3,
    };
    if scalar_len != cube {
        return Err(format!(
            "Stored {} field does not match its cubic grid.",
            quantity.label()
        ));
    }
    if matches!(
        quantity,
        SectionQuantity::VelocityMagnitude
            | SectionQuantity::VorticityMagnitude
            | SectionQuantity::WakeDeficit
    ) && input.velocity.len() != 3 * cube
    {
        return Err("Stored engineering velocity does not have three components.".into());
    }
    if quantity == SectionQuantity::FluidTractionMagnitude && input.traction_pa.len() != 3 * cube {
        return Err("Stored engineering fluid traction does not have three components.".into());
    }
    if input
        .mask
        .iter()
        .chain(input.pressure_pa)
        .chain(input.cp)
        .chain(input.velocity)
        .chain(input.traction_pa)
        .any(|value| !value.is_finite())
    {
        return Err("Stored engineering section fields contain a non-finite value.".into());
    }
    if !input.free_stream_mps.is_finite() || input.free_stream_mps <= 0.0 {
        return Err("Engineering section requires a positive recorded free-stream speed.".into());
    }
    if quantity == SectionQuantity::VorticityMagnitude
        && (!input.reference_length_m.is_finite()
            || input.reference_length_m <= 0.0
            || !input.solver_characteristic_length.is_finite()
            || input.solver_characteristic_length <= 0.0)
    {
        return Err(
            "Vorticity section requires positive recorded physical and solver reference lengths."
                .into(),
        );
    }

    let index = section_index(n, location)?;
    let snapped_location = index as f32 / (n - 1) as f32;
    let flat = |x: usize, y: usize, z: usize| (x * n + y) * n + z;
    let component =
        |values: &[f32], component: usize, voxel: usize| values[component * cube + voxel];
    let coordinate = |row: usize, column: usize| {
        let vertical = n - 1 - row;
        match axis {
            SectionAxis::X => [index, column, vertical],
            SectionAxis::Y => [column, index, vertical],
            SectionAxis::Z => [column, vertical, index],
        }
    };
    let clamp = |value: isize| value.rem_euclid(n as isize) as usize;
    let dx = std::f32::consts::TAU / n as f32;
    let curl_magnitude = |x: usize, y: usize, z: usize| {
        let at = |component_index: usize, x: isize, y: isize, z: isize| {
            component(
                input.velocity,
                component_index,
                flat(clamp(x), clamp(y), clamp(z)),
            )
        };
        let x = x as isize;
        let y = y as isize;
        let z = z as isize;
        let derivative = |component_index: usize, derivative_axis: usize| {
            let mut plus = [x, y, z];
            let mut minus = [x, y, z];
            plus[derivative_axis] += 1;
            minus[derivative_axis] -= 1;
            (at(component_index, plus[0], plus[1], plus[2])
                - at(component_index, minus[0], minus[1], minus[2]))
                / (2.0 * dx)
        };
        let wx = derivative(2, 1) - derivative(1, 2);
        let wy = derivative(0, 2) - derivative(2, 0);
        let wz = derivative(1, 0) - derivative(0, 1);
        let nondimensional = (wx * wx + wy * wy + wz * wz).sqrt();
        nondimensional * input.free_stream_mps * input.solver_characteristic_length
            / input.reference_length_m
    };

    let mut values = Vec::with_capacity(n * n);
    let mut mask = Vec::with_capacity(n * n);
    let mut in_plane = if quantity == SectionQuantity::VelocityMagnitude {
        Some(Vec::with_capacity(n * n))
    } else {
        None
    };
    for row in 0..n {
        for column in 0..n {
            let [x, y, z] = coordinate(row, column);
            let voxel = flat(x, y, z);
            let value = match quantity {
                SectionQuantity::RecoveredPressure => input.pressure_pa[voxel],
                SectionQuantity::PhysicalCp => input.cp[voxel],
                SectionQuantity::VelocityMagnitude | SectionQuantity::WakeDeficit => {
                    let ux = component(input.velocity, 0, voxel);
                    let uy = component(input.velocity, 1, voxel);
                    let uz = component(input.velocity, 2, voxel);
                    let speed = (ux * ux + uy * uy + uz * uz).sqrt();
                    if quantity == SectionQuantity::VelocityMagnitude {
                        if let Some(plane) = in_plane.as_mut() {
                            // Section image: +column = horizontal axis, +row =
                            // downward (decreasing vertical-axis coordinate).
                            let (uh, uv) = match axis {
                                SectionAxis::X => (uy, -uz),
                                SectionAxis::Y => (ux, -uz),
                                SectionAxis::Z => (ux, -uy),
                            };
                            plane.push([uh, uv]);
                        }
                        speed * input.free_stream_mps
                    } else {
                        (1.0 - speed).max(0.0)
                    }
                }
                SectionQuantity::VorticityMagnitude => curl_magnitude(x, y, z),
                SectionQuantity::FluidTractionMagnitude => (0..3)
                    .map(|component_index| {
                        component(input.traction_pa, component_index, voxel).powi(2)
                    })
                    .sum::<f32>()
                    .sqrt(),
            };
            values.push(value);
            mask.push(input.mask[voxel]);
        }
    }

    let scale_values = values
        .iter()
        .zip(&mask)
        .filter_map(|(&value, &mask_value)| {
            let visible = if quantity == SectionQuantity::FluidTractionMagnitude {
                mask_value > 0.01 && mask_value < 0.99
            } else {
                mask_value < 0.5
            };
            visible.then_some(value)
        })
        .collect::<Vec<_>>();
    let scale_source: &[f32] = if scale_values.is_empty() {
        &values
    } else {
        &scale_values
    };
    let minimum = scale_source.iter().copied().fold(f32::INFINITY, f32::min);
    let maximum = scale_source
        .iter()
        .copied()
        .fold(f32::NEG_INFINITY, f32::max);
    let center = match quantity {
        SectionQuantity::RecoveredPressure => Some(input.reference_pressure_pa),
        SectionQuantity::PhysicalCp => Some(0.0),
        _ => None,
    };
    let extent = center
        .map(|center| {
            (minimum - center)
                .abs()
                .max((maximum - center).abs())
                .max(1e-12)
        })
        .unwrap_or_else(|| maximum.max(1e-12));

    Ok(SectionPlane {
        n,
        axis,
        index,
        location: snapped_location,
        quantity,
        values,
        mask,
        scale: SectionScale {
            minimum,
            maximum,
            center,
            extent,
        },
        in_plane,
    })
}

/// Cheap CPU line-integral convolution over a planar velocity field.
/// Returns grayscale LIC in `[0,1]` matching section pixel order.
pub fn line_integral_convolution(n: usize, in_plane: &[[f32; 2]], steps: usize) -> Vec<f32> {
    assert_eq!(in_plane.len(), n * n);
    let noise = |row: usize, column: usize| -> f32 {
        // Deterministic hash noise — no RNG dependency in the section module.
        let x = (row as u32).wrapping_mul(374761393) ^ (column as u32).wrapping_mul(668265263);
        let mut z = x.wrapping_add(0x9e37_79b9);
        z = (z ^ (z >> 16)).wrapping_mul(0x85eb_ca6b);
        z = (z ^ (z >> 13)).wrapping_mul(0xc2b2_ae35);
        z = z ^ (z >> 16);
        (z & 0xffff) as f32 / 65535.0
    };
    let sample_uv = |r: f32, c: f32| -> [f32; 2] {
        let r0 = r.floor().clamp(0.0, (n - 1) as f32) as usize;
        let c0 = c.floor().clamp(0.0, (n - 1) as f32) as usize;
        in_plane[r0 * n + c0]
    };
    let sample_noise = |r: f32, c: f32| -> f32 {
        let r0 = r.round().clamp(0.0, (n - 1) as f32) as usize;
        let c0 = c.round().clamp(0.0, (n - 1) as f32) as usize;
        noise(r0, c0)
    };
    let mut out = vec![0.5f32; n * n];
    let h = 0.75f32;
    for row in 0..n {
        for column in 0..n {
            let mut acc = 0.0f32;
            let mut wsum = 0.0f32;
            for dir in [-1.0f32, 1.0] {
                let mut r = row as f32;
                let mut c = column as f32;
                for step in 0..=steps {
                    let weight = 1.0 - step as f32 / (steps as f32 + 1.0);
                    acc += sample_noise(r, c) * weight;
                    wsum += weight;
                    let uv = sample_uv(r, c);
                    let mag = (uv[0] * uv[0] + uv[1] * uv[1]).sqrt().max(1e-4);
                    c = (c + dir * uv[0] / mag * h).clamp(0.0, (n - 1) as f32);
                    r = (r + dir * uv[1] / mag * h).clamp(0.0, (n - 1) as f32);
                }
            }
            out[row * n + column] = (acc / wsum).clamp(0.0, 1.0);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(n: usize) -> (Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>) {
        let cube = n * n * n;
        let mut scalar = vec![0.0; cube];
        let mut mask = vec![0.0; cube];
        for x in 0..n {
            for y in 0..n {
                for z in 0..n {
                    let voxel = (x * n + y) * n + z;
                    scalar[voxel] = (100 * x + 10 * y + z) as f32;
                    mask[voxel] = scalar[voxel] / 1000.0;
                }
            }
        }
        (
            vec![0.0; 3 * cube],
            scalar.clone(),
            mask,
            scalar,
            vec![0.0; 3 * cube],
        )
    }

    fn input<'a>(
        n: usize,
        velocity: &'a [f32],
        pressure: &'a [f32],
        mask: &'a [f32],
        cp: &'a [f32],
        traction: &'a [f32],
    ) -> SectionInput<'a> {
        SectionInput {
            n,
            velocity,
            pressure_pa: pressure,
            mask,
            cp,
            traction_pa: traction,
            free_stream_mps: 2.0,
            reference_pressure_pa: 0.0,
            reference_length_m: 2.0,
            solver_characteristic_length: 0.5,
        }
    }

    /// Pinned Cp range (Settings › Appearance): a signed scale becomes an
    /// exact symmetric ±extent; magnitude scales are untouched.
    #[test]
    fn pinned_scale_overrides_signed_ranges_only() {
        let signed = SectionScale {
            minimum: -0.4,
            maximum: 0.9,
            center: Some(0.0),
            extent: 0.9,
        };
        let pinned = signed.pinned(1.5);
        assert_eq!(pinned.legend_minimum(), -1.5);
        assert_eq!(pinned.legend_maximum(), 1.5);
        assert_eq!(pinned.normalize(1.5), 1.0);
        assert_eq!(pinned.normalize(-3.0), -1.0, "clamped beyond the pin");
        // Magnitude (uncentered) scales are unchanged by pinning.
        let magnitude = SectionScale {
            minimum: 0.0,
            maximum: 4.0,
            center: None,
            extent: 4.0,
        };
        let same = magnitude.pinned(1.5);
        assert_eq!(same.legend_maximum(), 4.0);
        // Invalid extents are ignored rather than corrupting the scale.
        let unchanged = signed.pinned(f32::NAN);
        assert_eq!(unchanged.legend_maximum(), 0.9);
    }

    #[test]
    fn axes_extract_expected_plane_orientation_and_mask() {
        let n = 3;
        let (velocity, pressure, mask, cp, traction) = fixture(n);
        let input = input(n, &velocity, &pressure, &mask, &cp, &traction);
        let x = extract_section(
            &input,
            SectionAxis::X,
            0.5,
            SectionQuantity::RecoveredPressure,
        )
        .unwrap();
        assert_eq!((x.index, x.value(0, 0), x.value(0, 2)), (1, 102.0, 122.0));
        assert_eq!((x.value(2, 0), x.value(2, 2)), (100.0, 120.0));
        assert!((x.mask_value(0, 2) - 0.122).abs() < 1e-6);

        let y = extract_section(
            &input,
            SectionAxis::Y,
            0.5,
            SectionQuantity::RecoveredPressure,
        )
        .unwrap();
        assert_eq!((y.value(0, 0), y.value(0, 2)), (12.0, 212.0));
        assert_eq!((y.value(2, 0), y.value(2, 2)), (10.0, 210.0));

        let z = extract_section(
            &input,
            SectionAxis::Z,
            0.5,
            SectionQuantity::RecoveredPressure,
        )
        .unwrap();
        assert_eq!((z.value(0, 0), z.value(0, 2)), (21.0, 221.0));
        assert_eq!((z.value(2, 0), z.value(2, 2)), (1.0, 201.0));
    }

    #[test]
    fn vector_magnitudes_and_wake_deficit_use_recorded_physical_scaling() {
        let n = 3;
        let cube = n * n * n;
        let mut velocity = vec![0.0; 3 * cube];
        let mut traction = vec![0.0; 3 * cube];
        for voxel in 0..cube {
            velocity[voxel] = 0.3;
            velocity[cube + voxel] = 0.4;
            traction[voxel] = 3.0;
            traction[cube + voxel] = 4.0;
            traction[2 * cube + voxel] = 12.0;
        }
        let pressure = vec![0.0; cube];
        let mask = vec![0.25; cube];
        let cp = vec![0.0; cube];
        let input = input(n, &velocity, &pressure, &mask, &cp, &traction);

        let speed = extract_section(
            &input,
            SectionAxis::Z,
            0.0,
            SectionQuantity::VelocityMagnitude,
        )
        .unwrap();
        assert!((speed.value(1, 1) - 1.0).abs() < 1e-6);
        let plane = speed.in_plane.expect("velocity LIC needs in-plane UV");
        assert_eq!(plane.len(), n * n);
        // Z section: (uh, uv) = (ux, -uy) = (0.3, -0.4)
        assert!((plane[n + 1][0] - 0.3).abs() < 1e-6);
        assert!((plane[n + 1][1] + 0.4).abs() < 1e-6);
        let wake =
            extract_section(&input, SectionAxis::Z, 0.0, SectionQuantity::WakeDeficit).unwrap();
        assert!((wake.value(1, 1) - 0.5).abs() < 1e-6);
        assert!(wake.in_plane.is_none());
        let traction = extract_section(
            &input,
            SectionAxis::Z,
            0.0,
            SectionQuantity::FluidTractionMagnitude,
        )
        .unwrap();
        assert!((traction.value(1, 1) - 13.0).abs() < 1e-6);
    }

    #[test]
    fn vorticity_uses_solver_to_physical_coordinate_scale() {
        let n = 5;
        let cube = n * n * n;
        let mut velocity = vec![0.0; 3 * cube];
        for x in 0..n {
            let value = (std::f32::consts::TAU * x as f32 / n as f32).sin();
            for y in 0..n {
                for z in 0..n {
                    velocity[cube + (x * n + y) * n + z] = value;
                }
            }
        }
        let pressure = vec![0.0; cube];
        let mask = vec![0.0; cube];
        let cp = vec![0.0; cube];
        let traction = vec![0.0; 3 * cube];
        let input = input(n, &velocity, &pressure, &mask, &cp, &traction);
        let section = extract_section(
            &input,
            SectionAxis::Z,
            0.0,
            SectionQuantity::VorticityMagnitude,
        )
        .unwrap();
        let numerical_derivative =
            (std::f32::consts::TAU / n as f32).sin() / (std::f32::consts::TAU / n as f32);
        let expected = numerical_derivative * 2.0 * 0.5 / 2.0;
        assert!((section.value(n - 1, 0) - expected).abs() < 1e-6);
        assert_eq!(section.scale.center, None);
    }

    #[test]
    fn recovered_pressure_scale_is_centered_on_recorded_reference() {
        let n = 3;
        let cube = n * n * n;
        let velocity = vec![0.0; 3 * cube];
        let mut pressure = vec![100.0; cube];
        pressure[0] = 90.0;
        pressure[2 * n] = 110.0;
        let mask = vec![0.0; cube];
        let cp = vec![0.0; cube];
        let traction = vec![0.0; 3 * cube];
        let mut input = input(n, &velocity, &pressure, &mask, &cp, &traction);
        input.reference_pressure_pa = 100.0;
        let section = extract_section(
            &input,
            SectionAxis::X,
            0.0,
            SectionQuantity::RecoveredPressure,
        )
        .unwrap();
        assert_eq!(section.scale.center, Some(100.0));
        assert_eq!(section.scale.normalize(90.0), -1.0);
        assert_eq!(section.scale.normalize(100.0), 0.0);
        assert_eq!(section.scale.normalize(110.0), 1.0);
    }

    #[test]
    fn lic_follows_a_uniform_horizontal_field() {
        let n = 16;
        let uv = vec![[1.0, 0.0]; n * n];
        let lic = line_integral_convolution(n, &uv, 8);
        assert_eq!(lic.len(), n * n);
        // Adjacent samples along a streamline should be more correlated than
        // across streamlines for a horizontal field.
        let mut along = 0.0f32;
        let mut across = 0.0f32;
        for row in 1..n - 1 {
            for column in 1..n - 1 {
                let here = lic[row * n + column];
                along += (here - lic[row * n + column + 1]).abs();
                across += (here - lic[(row + 1) * n + column]).abs();
            }
        }
        assert!(
            along < across,
            "LIC should correlate more along flow ({along}) than across ({across})"
        );
    }
}
