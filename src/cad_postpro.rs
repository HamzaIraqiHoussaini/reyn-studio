//! Honest post-processing of a one-jump 32³ model field.
//!
//! Glyphs, iso windows, and line probes are derived from stored velocity and
//! occupancy. They are not solver residuals, wall laws, or time histories.

use crate::cad_field_ready::DISPLAY_OCCUPANCY_FLOOR;
use crate::engineering_section::SectionAxis;

/// Every 2nd cell on 32³ → 16³ = 4,096 candidate arrows before solids.
pub const GLYPH_STRIDE: usize = 2;
/// Glyph length in the render domain `[-1, 1]³` at unit nondimensional speed.
pub const GLYPH_LENGTH: f32 = 0.085;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IsoScalar {
    #[default]
    Q,
    Speed,
}

impl IsoScalar {
    pub const ALL: [Self; 2] = [Self::Q, Self::Speed];

    pub fn label(self) -> &'static str {
        match self {
            Self::Q => "Q",
            Self::Speed => "|U|",
        }
    }

    pub fn units(self) -> &'static str {
        match self {
            Self::Q => "1/s²",
            Self::Speed => "nondim",
        }
    }

    pub fn source(self) -> &'static str {
        "recovered from model velocity · not a production iso"
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProbeQuantity {
    #[default]
    PhysicalCp,
    Speed,
    Q,
}

impl ProbeQuantity {
    pub const ALL: [Self; 3] = [Self::PhysicalCp, Self::Speed, Self::Q];

    pub fn label(self) -> &'static str {
        match self {
            Self::PhysicalCp => "Cp",
            Self::Speed => "|U|",
            Self::Q => "Q",
        }
    }

    pub fn units(self) -> &'static str {
        match self {
            Self::PhysicalCp => "1",
            Self::Speed => "nondim",
            Self::Q => "1/s²",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glyph {
    pub pos: [f32; 3],
    pub dir: [f32; 3],
    pub speed: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LineProbe {
    pub axis: SectionAxis,
    pub quantity: ProbeQuantity,
    /// Domain fraction along the probe (0 at the inlet-side face, 1 at the far face).
    pub samples: Vec<(f32, f32)>,
}

/// Decimated velocity arrows on the 32³ lattice. Solids at the display occupancy
/// floor are skipped so arrows do not sit inside the Cp shell.
pub fn velocity_glyphs(n: usize, velocity: &[f32], mask: &[f32]) -> Vec<Glyph> {
    if n < 2 {
        return Vec::new();
    }
    let cube = n * n * n;
    if velocity.len() < 3 * cube || mask.len() < cube {
        return Vec::new();
    }
    let at = |c: usize, i: usize, j: usize, k: usize| velocity[((c * n + i) * n + j) * n + k];
    let mut glyphs = Vec::new();
    let mut i = 0;
    while i < n {
        let mut j = 0;
        while j < n {
            let mut k = 0;
            while k < n {
                let occupancy = mask[i * n * n + j * n + k];
                if occupancy < DISPLAY_OCCUPANCY_FLOOR {
                    let u = at(0, i, j, k);
                    let v = at(1, i, j, k);
                    let w = at(2, i, j, k);
                    let speed = (u * u + v * v + w * w).sqrt();
                    if speed.is_finite() && speed > 1e-6 {
                        let inv = 1.0 / speed;
                        glyphs.push(Glyph {
                            pos: cell_center(i, j, k, n),
                            dir: [u * inv, v * inv, w * inv],
                            speed,
                        });
                    }
                }
                k += GLYPH_STRIDE;
            }
            j += GLYPH_STRIDE;
        }
        i += GLYPH_STRIDE;
    }
    glyphs
}

fn cell_center(i: usize, j: usize, k: usize, n: usize) -> [f32; 3] {
    let denom = (n.saturating_sub(1)).max(1) as f32;
    [
        i as f32 / denom * 2.0 - 1.0,
        j as f32 / denom * 2.0 - 1.0,
        k as f32 / denom * 2.0 - 1.0,
    ]
}

/// Map a user iso (fraction of the stored field max) to a thin raymarch window.
pub fn iso_density_window(iso_fraction: f32) -> (f32, f32) {
    let t = if iso_fraction.is_finite() {
        iso_fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let half = 0.02;
    let lo = (t - half).clamp(0.0, 1.0);
    let hi = (t + half).clamp(lo + 1e-3, 1.0);
    (lo, hi)
}

pub fn physical_iso_value(iso_fraction: f32, field_max: f32) -> f32 {
    let t = if iso_fraction.is_finite() {
        iso_fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    t * field_max.max(0.0)
}

/// Sample one curve through the stored field. A one-jump operator has no time
/// history; this is quantity versus arc length on the current field only.
pub fn sample_line_probe(
    n: usize,
    axis: SectionAxis,
    quantity: ProbeQuantity,
    velocity: &[f32],
    cp: &[f32],
    mask: &[f32],
) -> LineProbe {
    let mut samples = Vec::new();
    if n < 3 {
        return LineProbe {
            axis,
            quantity,
            samples,
        };
    }
    let cube = n * n * n;
    if velocity.len() < 3 * cube || cp.len() < cube || mask.len() < cube {
        return LineProbe {
            axis,
            quantity,
            samples,
        };
    }
    let mid = n / 2;
    let denom = (n - 1) as f32;
    for t in 0..n {
        let (i, j, k) = match axis {
            SectionAxis::X => (t, mid, mid),
            SectionAxis::Y => (mid, t, mid),
            SectionAxis::Z => (mid, mid, t),
        };
        let idx = i * n * n + j * n + k;
        if mask[idx] >= DISPLAY_OCCUPANCY_FLOOR {
            continue;
        }
        let value = match quantity {
            ProbeQuantity::PhysicalCp => cp[idx],
            ProbeQuantity::Speed => {
                let u = velocity[idx];
                let v = velocity[cube + idx];
                let w = velocity[2 * cube + idx];
                (u * u + v * v + w * w).sqrt()
            }
            ProbeQuantity::Q => q_at(n, velocity, i, j, k),
        };
        if value.is_finite() {
            samples.push((t as f32 / denom, value));
        }
    }
    LineProbe {
        axis,
        quantity,
        samples,
    }
}

fn q_at(n: usize, velocity: &[f32], i: usize, j: usize, k: usize) -> f32 {
    let cl = |v: i64| v.clamp(0, n as i64 - 1) as usize;
    let at = |c: usize, i: usize, j: usize, k: usize| velocity[((c * n + i) * n + j) * n + k];
    let (ip, im) = (cl(i as i64 + 1), cl(i as i64 - 1));
    let (jp, jm) = (cl(j as i64 + 1), cl(j as i64 - 1));
    let (kp, km) = (cl(k as i64 + 1), cl(k as i64 - 1));
    let mut g = [[0f32; 3]; 3];
    for (c, row) in g.iter_mut().enumerate() {
        row[0] = 0.5 * (at(c, ip, j, k) - at(c, im, j, k));
        row[1] = 0.5 * (at(c, i, jp, k) - at(c, i, jm, k));
        row[2] = 0.5 * (at(c, i, j, kp) - at(c, i, j, km));
    }
    let (mut oo, mut ss) = (0f32, 0f32);
    for a in 0..3 {
        for b in 0..3 {
            let om = 0.5 * (g[a][b] - g[b][a]);
            let st = 0.5 * (g[a][b] + g[b][a]);
            oo += om * om;
            ss += st * st;
        }
    }
    0.5 * (oo - ss)
}

pub fn signed_cp_range(cp: &[f32]) -> (f32, f32) {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for value in cp {
        if value.is_finite() {
            min = min.min(*value);
            max = max.max(*value);
        }
    }
    if !min.is_finite() {
        (0.0, 0.0)
    } else {
        (min, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uniform_cube(n: usize, speed: f32) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let cube = n * n * n;
        let mut vel = vec![0.0; 3 * cube];
        for i in 0..cube {
            vel[i] = speed;
        }
        (vel, vec![0.0; cube], vec![0.1; cube])
    }

    #[test]
    fn glyphs_skip_solids_and_decimate_by_two() {
        let n = 8;
        let cube = n * n * n;
        let (mut vel, mut mask, _) = uniform_cube(n, 1.0);
        for i in 0..n {
            for j in 0..n {
                for k in 0..n {
                    if i < 4 {
                        mask[i * n * n + j * n + k] = 1.0;
                        vel[i * n * n + j * n + k] = 0.0;
                    }
                }
            }
        }
        let glyphs = velocity_glyphs(n, &vel, &mask);
        assert!(!glyphs.is_empty());
        assert!(glyphs.len() <= (n / GLYPH_STRIDE).pow(3));
        assert!(glyphs.iter().all(|g| g.pos[0] > -0.05));
        assert!(glyphs.iter().all(|g| (g.speed - 1.0).abs() < 1e-5));
        let _ = cube;
    }

    #[test]
    fn iso_window_is_a_labeled_band_not_a_mystery_fog() {
        let (lo, hi) = iso_density_window(0.5);
        assert!((lo - 0.48).abs() < 1e-6);
        assert!((hi - 0.52).abs() < 1e-6);
        assert_eq!(physical_iso_value(0.5, 12.0), 6.0);
        let (lo0, hi0) = iso_density_window(0.0);
        assert_eq!(lo0, 0.0);
        assert!(hi0 > lo0);
    }

    #[test]
    fn line_probe_is_one_curve_and_skips_the_solid() {
        let n = 8;
        let cube = n * n * n;
        let mut vel = vec![0.0; 3 * cube];
        let mut cp = vec![0.0; cube];
        let mut mask = vec![0.0; cube];
        for i in 0..n {
            for j in 0..n {
                for k in 0..n {
                    let idx = i * n * n + j * n + k;
                    vel[idx] = i as f32;
                    cp[idx] = i as f32 * 0.1;
                    if i == 3 {
                        mask[idx] = 1.0;
                    }
                }
            }
        }
        let probe = sample_line_probe(
            n,
            SectionAxis::X,
            ProbeQuantity::PhysicalCp,
            &vel,
            &cp,
            &mask,
        );
        assert!(!probe
            .samples
            .iter()
            .any(|(s, _)| (*s - 3.0 / 7.0).abs() < 1e-6));
        assert!(probe.samples.len() >= 6);
        assert!((probe.samples[0].1 - 0.0).abs() < 1e-5);
    }
}
