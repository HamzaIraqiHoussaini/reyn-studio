//! 3D flow field for the viewport. Particles advected through an ABC/Beltrami
//! field (an exact 3D vortex structure), coloured by signed vorticity. This is a
//! real, interactive visualization now; the seam `regenerate()` is where the
//! Python engine's real predicted field will plug in later.

#[derive(Clone, Copy)]
pub struct Particle {
    pub pos: [f32; 3], // in [-1, 1]^3
    pub vort: f32,     // signed, normalized ~[-1, 1] (blue < 0 < ember)
    pub speed: f32,    // [0, 1]
}

/// ABC (Arnold–Beltrami–Childress) velocity — swirling Beltrami vortex lines.
fn abc(p: [f32; 3]) -> [f32; 3] {
    let (x, y, z) = (p[0], p[1], p[2]);
    [z.sin() + y.cos(), x.sin() + z.cos(), y.sin() + x.cos()]
}

fn curl_x(p: [f32; 3]) -> f32 {
    // ω_x = ∂w/∂y - ∂v/∂z ; for ABC this equals u_x (Beltrami), gives a smooth signed field
    let h = 0.01;
    let dwdy = (abc([p[0], p[1] + h, p[2]])[2] - abc([p[0], p[1] - h, p[2]])[2]) / (2.0 * h);
    let dvdz = (abc([p[0], p[1], p[2] + h])[1] - abc([p[0], p[1], p[2] - h])[1]) / (2.0 * h);
    dwdy - dvdz
}

pub fn generate(n: usize, seed: u64) -> Vec<Particle> {
    let mut rng = seed.wrapping_mul(0x9E3779B97F4A7C15).max(1);
    let mut rnd = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng >> 11) as f32 / (1u64 << 53) as f32
    };
    let scale = std::f32::consts::PI; // domain [-π, π] for ABC, mapped to [-1,1] at the end
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let mut p = [
            (rnd() * 2.0 - 1.0) * scale,
            (rnd() * 2.0 - 1.0) * scale,
            (rnd() * 2.0 - 1.0) * scale,
        ];
        // advect along the flow so particles collect onto vortex tubes
        let dt = 0.06;
        for _ in 0..36 {
            let v = abc(p);
            for k in 0..3 {
                p[k] += v[k] * dt;
            }
        }
        let v = abc(p);
        let speed = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt() / 3.0_f32.sqrt();
        let vort = (curl_x(p) / 2.0).clamp(-1.0, 1.0);
        // wrap into [-π,π] then normalize to [-1,1]
        let wrap = |a: f32| {
            let m = (a + scale).rem_euclid(2.0 * scale) - scale;
            m / scale
        };
        out.push(Particle {
            pos: [wrap(p[0]), wrap(p[1]), wrap(p[2])],
            vort,
            speed: speed.clamp(0.0, 1.0),
        });
    }
    out
}

/// Convert a real engine velocity field `[3, N, N, N]` (row-major, C first) into
/// render particles: sample voxels, take the vorticity (curl) for signed colour,
/// speed for brightness, position normalized to [-1, 1]. This is the real model
/// output driving the viewport.
pub fn from_field(shape: &[usize], data: &[f32]) -> Vec<Particle> {
    if shape.len() != 4 || shape[0] != 3 {
        return Vec::new();
    }
    let (nx, ny, nz) = (shape[1], shape[2], shape[3]);
    if data.len() < 3 * nx * ny * nz {
        return Vec::new();
    }
    let at = |c: usize, i: usize, j: usize, k: usize| data[((c * nx + i) * ny + j) * nz + k];

    let target = 8000usize;
    let stride = (((nx * ny * nz) as f32 / target as f32).cbrt().floor() as usize).max(1);
    // per-sample jitter breaks the visible lattice so it reads as a field
    let cell = 2.0 / (nx.max(2) - 1) as f32 * stride as f32;
    let hash = |a: usize, b: usize, c: usize, salt: u32| -> f32 {
        let mut h = (a as u32).wrapping_mul(73856093)
            ^ (b as u32).wrapping_mul(19349663)
            ^ (c as u32).wrapping_mul(83492791)
            ^ salt.wrapping_mul(2654435761);
        h ^= h >> 13;
        h = h.wrapping_mul(0x5bd1e995);
        h ^= h >> 15;
        (h as f32 / u32::MAX as f32) - 0.5
    };
    let mut raw: Vec<([f32; 3], f32, f32)> = Vec::new();
    let (mut maxv, mut maxs) = (1e-6f32, 1e-6f32);
    let mut i = 1;
    while i < nx - 1 {
        let mut j = 1;
        while j < ny - 1 {
            let mut k = 1;
            while k < nz - 1 {
                let wx = (at(2, i, j + 1, k) - at(2, i, j - 1, k))
                    - (at(1, i, j, k + 1) - at(1, i, j, k - 1));
                let wy = (at(0, i, j, k + 1) - at(0, i, j, k - 1))
                    - (at(2, i + 1, j, k) - at(2, i - 1, j, k));
                let wz = (at(1, i + 1, j, k) - at(1, i - 1, j, k))
                    - (at(0, i, j + 1, k) - at(0, i, j - 1, k));
                let mag = (wx * wx + wy * wy + wz * wz).sqrt();
                let (u, v, w) = (at(0, i, j, k), at(1, i, j, k), at(2, i, j, k));
                let sp = (u * u + v * v + w * w).sqrt();
                maxv = maxv.max(mag);
                maxs = maxs.max(sp);
                let pos = [
                    (i as f32 / (nx - 1) as f32 * 2.0 - 1.0) + hash(i, j, k, 1) * 0.9 * cell,
                    (j as f32 / (ny - 1) as f32 * 2.0 - 1.0) + hash(i, j, k, 2) * 0.9 * cell,
                    (k as f32 / (nz - 1) as f32 * 2.0 - 1.0) + hash(i, j, k, 3) * 0.9 * cell,
                ];
                raw.push((pos, wx.signum() * mag, sp));
                k += stride;
            }
            j += stride;
        }
        i += stride;
    }
    raw.into_iter()
        .map(|(pos, vort, sp)| Particle {
            pos,
            vort: (vort / maxv).clamp(-1.0, 1.0),
            speed: (sp / maxs).clamp(0.0, 1.0),
        })
        .collect()
}

/// Vorticity-magnitude scalar volume from an engine velocity field `[3,N,N,N]`,
/// normalized to `[0,1]` and laid out for a wgpu 3D texture (x = i fastest,
/// then j, then k). Feeds the volume raymarch. Returns `(bytes, [nx,ny,nz])`.
pub fn vorticity_volume(shape: &[usize], data: &[f32]) -> Option<(Vec<u8>, [u32; 3])> {
    vorticity_field(shape, data).map(|volume| (volume.bytes, volume.dims))
}

/// Normalized |ω| volume plus the physical peak used for an honest colorbar.
pub fn vorticity_field(shape: &[usize], data: &[f32]) -> Option<NormalizedVolume> {
    if shape.len() != 4 || shape[0] != 3 {
        return None;
    }
    let (nx, ny, nz) = (shape[1], shape[2], shape[3]);
    if nx < 2 || ny < 2 || nz < 2 || data.len() < 3 * nx * ny * nz {
        return None;
    }
    let at = |c: usize, i: usize, j: usize, k: usize| data[((c * nx + i) * ny + j) * nz + k];
    let cl = |v: i64, n: usize| v.clamp(0, n as i64 - 1) as usize;
    let mut mag = vec![0f32; nx * ny * nz];
    let mut maxv = 1e-6f32;
    let mut sum = 0.0f32;
    let mut count = 0usize;
    for i in 0..nx {
        let (ip, im) = (cl(i as i64 + 1, nx), cl(i as i64 - 1, nx));
        for j in 0..ny {
            let (jp, jm) = (cl(j as i64 + 1, ny), cl(j as i64 - 1, ny));
            for k in 0..nz {
                let (kp, km) = (cl(k as i64 + 1, nz), cl(k as i64 - 1, nz));
                let wx = (at(2, i, jp, k) - at(2, i, jm, k)) - (at(1, i, j, kp) - at(1, i, j, km));
                let wy = (at(0, i, j, kp) - at(0, i, j, km)) - (at(2, ip, j, k) - at(2, im, j, k));
                let wz = (at(1, ip, j, k) - at(1, im, j, k)) - (at(0, i, jp, k) - at(0, i, jm, k));
                let m = (wx * wx + wy * wy + wz * wz).sqrt();
                mag[(k * ny + j) * nx + i] = m;
                if m > maxv {
                    maxv = m;
                }
                if m > 0.0 {
                    sum += m;
                    count += 1;
                }
            }
        }
    }
    let bytes = mag
        .iter()
        .map(|m| ((m / maxv).clamp(0.0, 1.0) * 255.0) as u8)
        .collect();
    Some(NormalizedVolume {
        bytes,
        dims: [nx as u32, ny as u32, nz as u32],
        physical_max: maxv,
        mean_positive: if count == 0 { 0.0 } else { sum / count as f32 },
    })
}

/// Normalized scalar volume plus the physical peak used for an honest colorbar.
#[derive(Clone, Debug)]
pub struct NormalizedVolume {
    pub bytes: Vec<u8>,
    pub dims: [u32; 3],
    pub physical_max: f32,
    pub mean_positive: f32,
}

/// Speed magnitude volume from an engine velocity field `[3,N,N,N]`.
pub fn speed_volume(shape: &[usize], data: &[f32]) -> Option<NormalizedVolume> {
    if shape.len() != 4 || shape[0] != 3 {
        return None;
    }
    let (nx, ny, nz) = (shape[1], shape[2], shape[3]);
    if nx < 2 || ny < 2 || nz < 2 || data.len() < 3 * nx * ny * nz {
        return None;
    }
    let at = |c: usize, i: usize, j: usize, k: usize| data[((c * nx + i) * ny + j) * nz + k];
    let mut mag = vec![0f32; nx * ny * nz];
    let mut maxv = 1e-6f32;
    let mut sum = 0.0f32;
    let mut count = 0usize;
    for i in 0..nx {
        for j in 0..ny {
            for k in 0..nz {
                let (u, v, w) = (at(0, i, j, k), at(1, i, j, k), at(2, i, j, k));
                let m = (u * u + v * v + w * w).sqrt();
                mag[(k * ny + j) * nx + i] = m;
                if m > maxv {
                    maxv = m;
                }
                if m > 0.0 {
                    sum += m;
                    count += 1;
                }
            }
        }
    }
    let bytes = mag
        .iter()
        .map(|m| ((m / maxv).clamp(0.0, 1.0) * 255.0) as u8)
        .collect();
    Some(NormalizedVolume {
        bytes,
        dims: [nx as u32, ny as u32, nz as u32],
        physical_max: maxv,
        mean_positive: if count == 0 { 0.0 } else { sum / count as f32 },
    })
}

/// Q-criterion scalar volume from an engine velocity field `[3,N,N,N]`.
/// Stores `max(Q, 0)` normalized to `[0,1]` for a thin iso-surface TF window.
/// Q = ½(‖Ω‖² − ‖S‖²); positive values mark rotation-dominated vortex cores.
pub fn q_criterion_volume(shape: &[usize], data: &[f32]) -> Option<(Vec<u8>, [u32; 3])> {
    q_criterion_field(shape, data).map(|volume| (volume.bytes, volume.dims))
}

/// Full-field Q (positive part) with the physical peak for a labeled iso value.
pub fn q_criterion_field(shape: &[usize], data: &[f32]) -> Option<NormalizedVolume> {
    if shape.len() != 4 || shape[0] != 3 {
        return None;
    }
    let (nx, ny, nz) = (shape[1], shape[2], shape[3]);
    if nx < 3 || ny < 3 || nz < 3 || data.len() < 3 * nx * ny * nz {
        return None;
    }
    let at = |c: usize, i: usize, j: usize, k: usize| data[((c * nx + i) * ny + j) * nz + k];
    let cl = |v: i64, n: usize| v.clamp(0, n as i64 - 1) as usize;
    let mut q_field = vec![0f32; nx * ny * nz];
    let mut maxq = 1e-6f32;
    let mut sum = 0.0f32;
    let mut count = 0usize;
    for i in 0..nx {
        let (ip, im) = (cl(i as i64 + 1, nx), cl(i as i64 - 1, nx));
        for j in 0..ny {
            let (jp, jm) = (cl(j as i64 + 1, ny), cl(j as i64 - 1, ny));
            for k in 0..nz {
                let (kp, km) = (cl(k as i64 + 1, nz), cl(k as i64 - 1, nz));
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
                let q = (0.5 * (oo - ss)).max(0.0);
                q_field[(k * ny + j) * nx + i] = q;
                if q > maxq {
                    maxq = q;
                }
                if q > 0.0 {
                    sum += q;
                    count += 1;
                }
            }
        }
    }
    let bytes = q_field
        .iter()
        .map(|q| ((q / maxq).clamp(0.0, 1.0) * 255.0) as u8)
        .collect();
    Some(NormalizedVolume {
        bytes,
        dims: [nx as u32, ny as u32, nz as u32],
        physical_max: maxq,
        mean_positive: if count == 0 { 0.0 } else { sum / count as f32 },
    })
}

/// The 3D counterparts of the 2D Field Insights, found in one gradient pass over
/// an engine field: strongest rotation (max |ω|), fastest flow (max |v|), and
/// the **Q-criterion maximum** — the standard vortex-core detector
/// (Q = ½(‖Ω‖² − ‖S‖²) > 0 where rotation beats strain).
#[derive(Clone, Copy, PartialEq)]
pub enum Insight3DKind {
    VortexCore, // max Q
    MaxVorticity,
    MaxSpeed,
    SurfLoad,    // max surface pressure — stagnation / load point (CAD)
    SurfSuction, // min surface pressure — suction peak / weak point (CAD)
}

impl Insight3DKind {
    pub fn glyph(self) -> &'static str {
        match self {
            Insight3DKind::VortexCore => "Q",
            Insight3DKind::MaxVorticity => "ω",
            Insight3DKind::MaxSpeed => "v",
            Insight3DKind::SurfLoad => "P▲",
            Insight3DKind::SurfSuction => "P▼",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Insight3D {
    pub kind: Insight3DKind,
    pub pos: [f32; 3], // [-1,1]³ domain coords (matches the render particles)
    pub value: f32,
}

/// Critical points of a `[3,N,N,N]` velocity field. Runs once per field arrival
/// (~10 ms at 64³ native) — not per frame.
pub fn insights3d(shape: &[usize], data: &[f32]) -> Vec<Insight3D> {
    if shape.len() != 4 || shape[0] != 3 {
        return Vec::new();
    }
    let (nx, ny, nz) = (shape[1], shape[2], shape[3]);
    if nx < 3 || ny < 3 || nz < 3 || data.len() < 3 * nx * ny * nz {
        return Vec::new();
    }
    let at = |c: usize, i: usize, j: usize, k: usize| data[((c * nx + i) * ny + j) * nz + k];
    let cl = |v: i64, n: usize| v.clamp(0, n as i64 - 1) as usize;

    let mut best_q = (f32::MIN, [0usize; 3]);
    let mut best_w = (f32::MIN, [0usize; 3]);
    let mut best_s = (f32::MIN, [0usize; 3]);
    for i in 0..nx {
        let (ip, im) = (cl(i as i64 + 1, nx), cl(i as i64 - 1, nx));
        for j in 0..ny {
            let (jp, jm) = (cl(j as i64 + 1, ny), cl(j as i64 - 1, ny));
            for k in 0..nz {
                let (kp, km) = (cl(k as i64 + 1, nz), cl(k as i64 - 1, nz));
                // g[c][a] = ∂u_c/∂x_a (central, cell units)
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
                let q = 0.5 * (oo - ss);
                let wmag = (2.0 * oo).sqrt(); // ‖Ω‖² = ½|ω|²
                let (u, v, w) = (at(0, i, j, k), at(1, i, j, k), at(2, i, j, k));
                let sp = (u * u + v * v + w * w).sqrt();
                if q > best_q.0 {
                    best_q = (q, [i, j, k]);
                }
                if wmag > best_w.0 {
                    best_w = (wmag, [i, j, k]);
                }
                if sp > best_s.0 {
                    best_s = (sp, [i, j, k]);
                }
            }
        }
    }
    let to_pos = |c: [usize; 3]| {
        [
            c[0] as f32 / (nx - 1) as f32 * 2.0 - 1.0,
            c[1] as f32 / (ny - 1) as f32 * 2.0 - 1.0,
            c[2] as f32 / (nz - 1) as f32 * 2.0 - 1.0,
        ]
    };
    vec![
        Insight3D {
            kind: Insight3DKind::VortexCore,
            pos: to_pos(best_q.1),
            value: best_q.0,
        },
        Insight3D {
            kind: Insight3DKind::MaxVorticity,
            pos: to_pos(best_w.1),
            value: best_w.0,
        },
        Insight3D {
            kind: Insight3DKind::MaxSpeed,
            pos: to_pos(best_s.1),
            value: best_s.0,
        },
    ]
}

/// Procedural |ω| volume (ABC/Beltrami, where curl u = u so |ω| = |u|) for the
/// placeholder before a model field arrives. `seed` phase-shifts it so
/// regenerate visibly changes the view.
pub fn procedural_volume(n: usize, seed: u64) -> (Vec<u8>, [u32; 3]) {
    let sc = std::f32::consts::PI;
    let ph = (seed as f32) * 0.7;
    let mut vals = vec![0f32; n * n * n];
    let mut maxv = 1e-6f32;
    let coord = |a: usize| (a as f32 / (n - 1) as f32 * 2.0 - 1.0) * sc;
    for k in 0..n {
        for j in 0..n {
            for i in 0..n {
                let v = abc([coord(i) + ph, coord(j) + ph, coord(k) + ph]);
                let m = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                vals[(k * n + j) * n + i] = m;
                if m > maxv {
                    maxv = m;
                }
            }
        }
    }
    let bytes = vals
        .iter()
        .map(|m| ((m / maxv).clamp(0.0, 1.0) * 255.0) as u8)
        .collect();
    (bytes, [n as u32, n as u32, n as u32])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quiet field with a planted fast jet and a planted solid-body vortex:
    /// the insights must land on them.
    #[test]
    fn insights3d_find_planted_structures() {
        let n = 16usize;
        let shape = [3, n, n, n];
        let mut data = vec![0.0f32; 3 * n * n * n];
        let idx = |c: usize, i: usize, j: usize, k: usize| ((c * n + i) * n + j) * n + k;

        // fast jet PLATEAU around (3,4,5): constant u inside → high speed, zero
        // interior gradients (a single hot voxel would be a delta function whose
        // shear out-vorticizes the actual vortex — the detector is that literal)
        for i in 2..=4usize {
            for j in 3..=5usize {
                for k in 4..=6usize {
                    data[idx(0, i, j, k)] = 9.0;
                }
            }
        }

        // solid-body rotation about the z-axis centred at (10,10,8):
        // u = -(y-yc)*s, v = (x-xc)*s → ω_z = 2s and Q = ½‖Ω‖² > 0 at the centre
        let (xc, yc, zc, s) = (10i64, 10i64, 8i64, 5.0f32);
        for di in -1i64..=1 {
            for dj in -1i64..=1 {
                for dk in -1i64..=1 {
                    let (i, j, k) = ((xc + di) as usize, (yc + dj) as usize, (zc + dk) as usize);
                    data[idx(0, i, j, k)] = -(dj as f32) * s;
                    data[idx(1, i, j, k)] = di as f32 * s;
                }
            }
        }

        let out = insights3d(&shape, &data);
        assert_eq!(out.len(), 3);
        let get = |k: Insight3DKind| out.iter().find(|x| x.kind == k).unwrap();

        let sp = get(Insight3DKind::MaxSpeed);
        let cell = |p: f32| ((p + 1.0) / 2.0 * (n - 1) as f32).round() as i64;
        assert!(
            (cell(sp.pos[0]) - 3).abs() <= 1
                && (cell(sp.pos[1]) - 4).abs() <= 1
                && (cell(sp.pos[2]) - 5).abs() <= 1,
            "speed max off the jet: {:?}",
            (cell(sp.pos[0]), cell(sp.pos[1]), cell(sp.pos[2]))
        );
        assert!((sp.value - 9.0).abs() < 1e-4);

        // vortex centre: pure rotation (zero strain) → Q max within the core
        let q = get(Insight3DKind::VortexCore);
        assert!(
            (cell(q.pos[0]) - xc).abs() <= 1
                && (cell(q.pos[1]) - yc).abs() <= 1
                && (cell(q.pos[2]) - zc).abs() <= 1,
            "Q core off target: {:?}",
            (cell(q.pos[0]), cell(q.pos[1]), cell(q.pos[2]))
        );
        assert!(
            q.value > 0.0,
            "vortex core must have Q > 0 (got {})",
            q.value
        );

        // max |ω| also lives in the vortex, with |ω| ≈ 2s
        let w = get(Insight3DKind::MaxVorticity);
        assert!((cell(w.pos[0]) - xc).abs() <= 2 && (cell(w.pos[1]) - yc).abs() <= 2);
        assert!(
            w.value > s,
            "|ω| at the core should exceed s (got {})",
            w.value
        );

        for ins in &out {
            for a in 0..3 {
                assert!(ins.pos[a] >= -1.0 && ins.pos[a] <= 1.0);
            }
        }
    }

    #[test]
    fn q_criterion_volume_is_nonempty_on_a_synthetic_vortex() {
        // Solid-body rotation in the XY plane: Q > 0 at the core.
        let n = 16usize;
        let mut data = vec![0f32; 3 * n * n * n];
        let at = |c: usize, i: usize, j: usize, k: usize| ((c * n + i) * n + j) * n + k;
        let s = 2.0f32;
        let (xc, yc) = (n / 2, n / 2);
        for i in 0..n {
            for j in 0..n {
                for k in 0..n {
                    let x = i as f32 - xc as f32;
                    let y = j as f32 - yc as f32;
                    data[at(0, i, j, k)] = -s * y;
                    data[at(1, i, j, k)] = s * x;
                    data[at(2, i, j, k)] = 0.0;
                }
            }
        }
        let (bytes, dims) = q_criterion_volume(&[3, n, n, n], &data).expect("q volume");
        assert_eq!(dims, [n as u32, n as u32, n as u32]);
        assert_eq!(bytes.len(), n * n * n);
        let peak = *bytes.iter().max().unwrap();
        assert!(peak > 200, "vortex core should saturate the Q TF ({peak})");
        let core = bytes[((n / 2) * n + (n / 2)) * n + (n / 2)];
        assert!(core > 100, "core voxel should be hot ({core})");
    }
}
