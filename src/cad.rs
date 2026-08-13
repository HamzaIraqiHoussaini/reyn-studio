//! Geometry import: STL (binary + ASCII) or single-part STEP B-rep → voxel
//! obstacle mask for the immersed-mask pipeline, plus recovered-pressure surface analysis. Maxima identify
//! stagnation/load points; minima identify suction/low-pressure points.
//!
//! Voxelization uses three independent ray-parity classifications (+X, +Y, +Z)
//! and a majority vote. Their disagreement is retained as engineering evidence;
//! a single-axis fill can silently lose double-digit fractions of a leaky,
//! nested, or intersecting body. The mesh is auto-fit to the solver's
//! training envelope: centred in the tunnel with a cross-stream size inside the
//! band the 3D obstacle models were trained on.

use crate::flow::{Insight3D, Insight3DKind};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};

#[derive(Clone, Debug)]
pub struct Mesh {
    pub tris: Vec<[[f32; 3]; 3]>,
}

/// Canonical SHA-256 of the analyzed triangle mesh (source-space, post-weld).
///
/// Encoding `reyn.analyzed-mesh.v1` + LE triangle count + each triangle as nine
/// IEEE-754 f32 bit patterns in triangle/vertex/component order. Distinct from
/// the source-bytes SHA-256: STEP B-rep identity is not mesh identity.
pub fn analyzed_mesh_sha256(mesh: &Mesh) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"reyn.analyzed-mesh.v1\0");
    hasher.update((mesh.tris.len() as u64).to_le_bytes());
    for triangle in &mesh.tris {
        for vertex in triangle {
            for component in vertex {
                hasher.update(component.to_bits().to_le_bytes());
            }
        }
    }
    format!("{:x}", hasher.finalize())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryFormat {
    Stl,
    Step,
    ThreeMf,
}

impl GeometryFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stl => "STL",
            Self::Step => "STEP",
            Self::ThreeMf => "3MF",
        }
    }

    pub fn media_type(self) -> &'static str {
        match self {
            Self::Stl => "model/stl",
            Self::Step => "model/step",
            Self::ThreeMf => "model/3mf",
        }
    }
}

#[derive(Clone, Debug)]
pub struct GeometryImport {
    pub mesh: Mesh,
    pub format: GeometryFormat,
    pub declared_unit: Option<String>,
    pub translator: String,
    pub translator_version: String,
    pub tessellation_tolerance_source_units: Option<f64>,
    pub vertex_weld_relative_tolerance: Option<f64>,
    pub source_shells: usize,
    /// STEP pick-one shell entity id. None for STL/3MF and single-shell STEP.
    pub selected_shell_entity_id: Option<u64>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum GeometryParseError {
    Message(String),
    ChooseShell(crate::cad_step::ShellChoiceRequired),
}

impl From<GeometryParseError> for String {
    fn from(value: GeometryParseError) -> Self {
        match value {
            GeometryParseError::Message(message) => message,
            GeometryParseError::ChooseShell(choice) => {
                crate::cad_step::StepParseError::ChooseShell(choice).into()
            }
        }
    }
}

/// Parse a supported geometry source based on its filename extension.
///
/// STEP is intentionally restricted to resolved single-part B-rep sources.
/// Assemblies are rejected by `cad_step` until occurrence transforms and stable
/// region identity can be preserved in the evidence contract. Multi-shell STEP
/// files without an assembly graph require an explicit shell selection.
#[allow(dead_code)] // binary crate; Studio import always goes through selecting
pub fn parse_geometry(source_name: &str, bytes: &[u8]) -> Result<GeometryImport, String> {
    parse_geometry_selecting(source_name, bytes, None).map_err(Into::into)
}

/// Like [`parse_geometry`], with an optional STEP shell entity id for pick-one.
pub fn parse_geometry_selecting(
    source_name: &str,
    bytes: &[u8],
    selected_shell_entity_id: Option<u64>,
) -> Result<GeometryImport, GeometryParseError> {
    let extension = std::path::Path::new(source_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("stl") => {
            if selected_shell_entity_id.is_some() {
                return Err(GeometryParseError::Message(
                    "STL sources are single-body meshes; shell selection does not apply".into(),
                ));
            }
            Ok(GeometryImport {
                mesh: parse_stl(bytes).map_err(GeometryParseError::Message)?,
                format: GeometryFormat::Stl,
                declared_unit: None,
                translator: "reyn-stl".into(),
                translator_version: env!("CARGO_PKG_VERSION").into(),
                tessellation_tolerance_source_units: None,
                vertex_weld_relative_tolerance: None,
                source_shells: 1,
                selected_shell_entity_id: None,
                warnings: Vec::new(),
            })
        }
        Some("stp" | "step") => {
            let imported =
                match crate::cad_step::parse_step_selecting(bytes, selected_shell_entity_id) {
                    Ok(imported) => imported,
                    Err(crate::cad_step::StepParseError::ChooseShell(choice)) => {
                        return Err(GeometryParseError::ChooseShell(choice));
                    }
                    Err(crate::cad_step::StepParseError::Message(message)) => {
                        return Err(GeometryParseError::Message(message));
                    }
                };
            Ok(GeometryImport {
                mesh: imported.mesh,
                format: GeometryFormat::Step,
                declared_unit: imported.declared_unit,
                translator: crate::cad_step::TRANSLATOR.into(),
                translator_version: crate::cad_step::TRANSLATOR_VERSION.into(),
                tessellation_tolerance_source_units: Some(
                    imported.tessellation_tolerance_source_units,
                ),
                vertex_weld_relative_tolerance: Some(imported.vertex_weld_relative_tolerance),
                source_shells: imported.shell_count,
                selected_shell_entity_id: imported.selected_shell_entity_id,
                warnings: imported.warnings,
            })
        }
        Some("3mf") => {
            if selected_shell_entity_id.is_some() {
                return Err(GeometryParseError::Message(
                    "3MF pick-one is limited to the primary model body in this release".into(),
                ));
            }
            let imported = crate::cad_3mf::parse_3mf(bytes).map_err(GeometryParseError::Message)?;
            Ok(GeometryImport {
                mesh: imported.mesh,
                format: GeometryFormat::ThreeMf,
                declared_unit: Some(imported.declared_unit),
                translator: crate::cad_3mf::TRANSLATOR.into(),
                translator_version: crate::cad_3mf::TRANSLATOR_VERSION.into(),
                tessellation_tolerance_source_units: None,
                vertex_weld_relative_tolerance: None,
                source_shells: 1,
                selected_shell_entity_id: None,
                warnings: imported.warnings,
            })
        }
        Some(extension) => Err(GeometryParseError::Message(format!(
            "unsupported geometry extension .{extension}; choose STL, STP, STEP, or 3MF"
        ))),
        None => Err(GeometryParseError::Message(
            "geometry source has no extension; choose STL, STP, STEP, or 3MF".into(),
        )),
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MeshDiagnostics {
    pub triangles: usize,
    pub components: usize,
    pub degenerate_triangles: usize,
    pub boundary_edges: usize,
    pub non_manifold_edges: usize,
    pub inconsistent_winding_edges: usize,
    pub self_intersection_pairs: usize,
    /// Signed source-space volume from the triangle winding. The current load
    /// method derives normals from the occupancy-mask gradient, so a negative
    /// value is recorded but does not reverse reported loads.
    pub signed_volume: f64,
    pub extents: [f32; 3],
}

/// Topology and scale checks performed before solver preprocessing. Extents are
/// intentionally reported in source units; format declarations are recorded
/// separately and never silently rescale the customer's geometry.
pub fn diagnose_mesh(mesh: &Mesh) -> MeshDiagnostics {
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for triangle in &mesh.tris {
        for vertex in triangle {
            for axis in 0..3 {
                lo[axis] = lo[axis].min(vertex[axis]);
                hi[axis] = hi[axis].max(vertex[axis]);
            }
        }
    }
    let extents = if mesh.tris.is_empty() {
        [0.0; 3]
    } else {
        [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]]
    };
    let diagonal = (extents[0] * extents[0] + extents[1] * extents[1] + extents[2] * extents[2])
        .sqrt()
        .max(1.0);
    let tolerance = diagonal * 1e-6;
    let key = |vertex: [f32; 3]| -> [i64; 3] {
        [
            (vertex[0] / tolerance).round() as i64,
            (vertex[1] / tolerance).round() as i64,
            (vertex[2] / tolerance).round() as i64,
        ]
    };
    let mut degenerate_triangles = 0usize;
    let mut signed_volume = 0.0f64;
    let mut edge_uses: HashMap<([i64; 3], [i64; 3]), Vec<(usize, bool)>> = HashMap::new();
    for (triangle_index, triangle) in mesh.tris.iter().enumerate() {
        let [a, b, c] = *triangle;
        signed_volume += (a[0] as f64 * (b[1] as f64 * c[2] as f64 - b[2] as f64 * c[1] as f64)
            + a[1] as f64 * (b[2] as f64 * c[0] as f64 - b[0] as f64 * c[2] as f64)
            + a[2] as f64 * (b[0] as f64 * c[1] as f64 - b[1] as f64 * c[0] as f64))
            / 6.0;
        let ab = [
            triangle[1][0] - triangle[0][0],
            triangle[1][1] - triangle[0][1],
            triangle[1][2] - triangle[0][2],
        ];
        let ac = [
            triangle[2][0] - triangle[0][0],
            triangle[2][1] - triangle[0][1],
            triangle[2][2] - triangle[0][2],
        ];
        let cross = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        if cross.iter().map(|value| value * value).sum::<f32>().sqrt() <= tolerance * tolerance {
            degenerate_triangles += 1;
        }
        let vertices = [key(triangle[0]), key(triangle[1]), key(triangle[2])];
        for (start, end) in [(0, 1), (1, 2), (2, 0)] {
            let a = vertices[start];
            let b = vertices[end];
            let (canonical, forward) = if a <= b {
                ((a, b), true)
            } else {
                ((b, a), false)
            };
            edge_uses
                .entry(canonical)
                .or_default()
                .push((triangle_index, forward));
        }
    }
    let boundary_edges = edge_uses.values().filter(|uses| uses.len() == 1).count();
    let non_manifold_edges = edge_uses.values().filter(|uses| uses.len() > 2).count();
    let inconsistent_winding_edges = edge_uses
        .values()
        .filter(|uses| uses.len() == 2 && uses[0].1 == uses[1].1)
        .count();
    let self_intersection_pairs = count_self_intersections(mesh, tolerance);

    let mut parent: Vec<usize> = (0..mesh.tris.len()).collect();
    fn root(parent: &mut [usize], mut index: usize) -> usize {
        while parent[index] != index {
            parent[index] = parent[parent[index]];
            index = parent[index];
        }
        index
    }
    for uses in edge_uses.values() {
        if let Some((first, _)) = uses.first() {
            for (other, _) in uses.iter().skip(1) {
                let first_root = root(&mut parent, *first);
                let other_root = root(&mut parent, *other);
                parent[other_root] = first_root;
            }
        }
    }
    let mut component_roots = std::collections::HashSet::new();
    for triangle_index in 0..mesh.tris.len() {
        component_roots.insert(root(&mut parent, triangle_index));
    }
    MeshDiagnostics {
        triangles: mesh.tris.len(),
        components: component_roots.len(),
        degenerate_triangles,
        boundary_edges,
        non_manifold_edges,
        inconsistent_winding_edges,
        self_intersection_pairs,
        signed_volume,
        extents,
    }
}

/// Count non-adjacent triangle pairs that intersect. A sweep on the first AABB
/// axis avoids the quadratic all-pairs cost on ordinary tessellated surfaces.
/// Coplanar overlaps are deliberately not inferred from numerical noise; the
/// topology and multi-axis gates still protect the current fixture set.
fn count_self_intersections(mesh: &Mesh, tolerance: f32) -> usize {
    #[derive(Clone)]
    struct Bounds {
        triangle: usize,
        lo: [f32; 3],
        hi: [f32; 3],
    }
    let mut bounds: Vec<Bounds> = mesh
        .tris
        .iter()
        .enumerate()
        .map(|(triangle, vertices)| {
            let mut lo = [f32::MAX; 3];
            let mut hi = [f32::MIN; 3];
            for vertex in vertices {
                for axis in 0..3 {
                    lo[axis] = lo[axis].min(vertex[axis]);
                    hi[axis] = hi[axis].max(vertex[axis]);
                }
            }
            Bounds { triangle, lo, hi }
        })
        .collect();
    bounds.sort_by(|a, b| a.lo[0].partial_cmp(&b.lo[0]).unwrap());
    let same_vertex =
        |a: [f32; 3], b: [f32; 3]| (0..3).all(|axis| (a[axis] - b[axis]).abs() <= tolerance);
    let mut intersections = 0usize;
    for left in 0..bounds.len() {
        let a_bounds = &bounds[left];
        for b_bounds in bounds.iter().skip(left + 1) {
            if b_bounds.lo[0] > a_bounds.hi[0] + tolerance {
                break;
            }
            if (1..3).any(|axis| {
                b_bounds.lo[axis] > a_bounds.hi[axis] + tolerance
                    || a_bounds.lo[axis] > b_bounds.hi[axis] + tolerance
            }) {
                continue;
            }
            let a = &mesh.tris[a_bounds.triangle];
            let b = &mesh.tris[b_bounds.triangle];
            if a.iter().any(|av| b.iter().any(|bv| same_vertex(*av, *bv))) {
                continue;
            }
            if triangles_intersect(a, b, tolerance) {
                intersections += 1;
            }
        }
    }
    intersections
}

fn triangles_intersect(a: &[[f32; 3]; 3], b: &[[f32; 3]; 3], tolerance: f32) -> bool {
    [(a[0], a[1]), (a[1], a[2]), (a[2], a[0])]
        .into_iter()
        .any(|(start, end)| segment_hits_triangle(start, end, b, tolerance))
        || [(b[0], b[1]), (b[1], b[2]), (b[2], b[0])]
            .into_iter()
            .any(|(start, end)| segment_hits_triangle(start, end, a, tolerance))
}

fn segment_hits_triangle(
    start: [f32; 3],
    end: [f32; 3],
    triangle: &[[f32; 3]; 3],
    tolerance: f32,
) -> bool {
    let sub = |a: [f32; 3], b: [f32; 3]| std::array::from_fn(|i| a[i] - b[i]);
    let cross = |a: [f32; 3], b: [f32; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let dot = |a: [f32; 3], b: [f32; 3]| (0..3).map(|i| a[i] * b[i]).sum::<f32>();
    let direction = sub(end, start);
    let edge1 = sub(triangle[1], triangle[0]);
    let edge2 = sub(triangle[2], triangle[0]);
    let h = cross(direction, edge2);
    let determinant = dot(edge1, h);
    if determinant.abs() <= tolerance {
        return false;
    }
    let inverse = 1.0 / determinant;
    let s = sub(start, triangle[0]);
    let u = inverse * dot(s, h);
    if u < -tolerance || u > 1.0 + tolerance {
        return false;
    }
    let q = cross(s, edge1);
    let v = inverse * dot(direction, q);
    if v < -tolerance || u + v > 1.0 + tolerance {
        return false;
    }
    let along_segment = inverse * dot(edge2, q);
    along_segment > tolerance && along_segment < 1.0 - tolerance
}

/// Parse an STL file (auto-detects binary vs ASCII).
pub fn parse_stl(bytes: &[u8]) -> Result<Mesh, String> {
    if bytes.len() < 15 {
        return Err("file too small to be an STL".into());
    }
    // binary layout: 80-byte header | u32 count | count × 50 bytes
    if bytes.len() >= 84 {
        let count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
        if bytes.len() == 84 + count * 50 {
            let mut tris = Vec::with_capacity(count);
            for t in 0..count {
                let base = 84 + t * 50 + 12; // skip the normal
                let f = |o: usize| {
                    f32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
                };
                let mut tri = [[0f32; 3]; 3];
                for (v, vert) in tri.iter_mut().enumerate() {
                    for (c, x) in vert.iter_mut().enumerate() {
                        *x = f(base + v * 12 + c * 4);
                    }
                }
                tris.push(tri);
            }
            return Ok(Mesh { tris });
        }
    }
    // ASCII: "solid ... facet normal ... vertex x y z"
    let text = std::str::from_utf8(bytes).map_err(|_| "neither binary nor ASCII STL")?;
    let mut tris = Vec::new();
    let mut verts: Vec<[f32; 3]> = Vec::with_capacity(3);
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("vertex") {
            let mut it = rest
                .split_whitespace()
                .filter_map(|s| s.parse::<f32>().ok());
            let v = [it.next(), it.next(), it.next()];
            if let (Some(x), Some(y), Some(z)) = (v[0], v[1], v[2]) {
                verts.push([x, y, z]);
                if verts.len() == 3 {
                    tris.push([verts[0], verts[1], verts[2]]);
                    verts.clear();
                }
            }
        }
    }
    if tris.is_empty() {
        return Err("no triangles found in STL".into());
    }
    Ok(Mesh { tris })
}

pub struct VoxelMask {
    pub n: usize,
    pub mask: Vec<f32>, // [n³], layout [i*n*n + j*n + k] matching engine fields
    pub solid_voxels: usize,
    pub char_len: f32, // cross-stream extent in solver units (Re scaling)
    pub components: usize,
    pub minimum_cells_across: usize,
    pub boundary_clearance_cells: usize,
    /// Fraction of cells in the union of the three axis classifications on
    /// which +X/+Y/+Z do not all agree. Zero means axis-independent occupancy.
    pub axis_disagreement_fraction: f64,
    /// Scanlines with an odd number of crossings for +X, +Y, and +Z.
    pub odd_crossing_rows: [usize; 3],
    /// Version 2 introduced independent three-axis voting. Persisting the
    /// version prevents old single-axis projects from looking newly validated.
    pub classification_version: u32,
    /// Isotropic source-unit → solver-unit scale, separate from
    /// `transform_4x4` because that matrix also carries the body rotation.
    pub scale: f64,
    pub orientation: BodyOrientation,
    pub transform_4x4: [f64; 16],
}

/// Body orientation relative to the fixed `+X` free stream, in degrees. The
/// model's stream direction cannot be changed, so an angle of attack is applied
/// by rotating the *geometry* before voxelization — which is exactly what is
/// computed, and what the UI says.
///
/// * `angle_of_attack` — about `+Y` (the side axis); positive pitches the nose
///   up toward `+Z`.
/// * `yaw` — about `+Z` (vertical); positive swings the nose toward `+Y`.
/// * `roll` — about `+X` (streamwise).
///
/// Applied roll → angle of attack → yaw, so the angles compose the way an
/// aerodynamicist reads them off a body.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BodyOrientation {
    pub angle_of_attack_deg: f64,
    pub yaw_deg: f64,
    pub roll_deg: f64,
}

impl BodyOrientation {
    pub fn from_degrees(values: [f64; 3]) -> Self {
        Self {
            angle_of_attack_deg: values[0],
            yaw_deg: values[1],
            roll_deg: values[2],
        }
    }

    pub fn to_degrees(self) -> [f64; 3] {
        [self.angle_of_attack_deg, self.yaw_deg, self.roll_deg]
    }

    pub fn is_identity(self) -> bool {
        self.to_degrees()
            .iter()
            .all(|angle| angle.abs() < f64::EPSILON)
    }

    pub fn is_finite(self) -> bool {
        self.to_degrees().iter().all(|angle| angle.is_finite())
    }

    /// Rotate the body so its longest source extent lies along the fixed `+X`
    /// free stream. Cross-stream (y/z) fit then uses the remaining face, which
    /// keeps flat bricks and capsules above the critical-thickness gate without
    /// leaving the trained characteristic-length band.
    pub fn align_longest_extent_to_stream(extents: [f32; 3]) -> Self {
        let mut longest = 0usize;
        for axis in 1..3 {
            if extents[axis] > extents[longest] {
                longest = axis;
            }
        }
        // Near-cube: leave the imported attitude alone.
        let shortest = extents
            .iter()
            .copied()
            .fold(f32::MAX, f32::min)
            .max(1e-12);
        if extents[longest] / shortest < 1.05 {
            return Self::default();
        }
        match longest {
            // +Y → +X via yaw −90°.
            1 => Self {
                yaw_deg: -90.0,
                ..Self::default()
            },
            // +Z → +X via angle of attack −90°.
            2 => Self {
                angle_of_attack_deg: -90.0,
                ..Self::default()
            },
            _ => Self::default(),
        }
    }

    /// Row-major 3×3 rotation matrix `R_yaw · R_aoa · R_roll`.
    pub fn matrix(self) -> [[f64; 3]; 3] {
        let (sa, ca) = self.angle_of_attack_deg.to_radians().sin_cos();
        let (sb, cb) = self.yaw_deg.to_radians().sin_cos();
        let (sr, cr) = self.roll_deg.to_radians().sin_cos();
        // Angle of attack about +Y, nose up toward +Z.
        let aoa = [[ca, 0.0, -sa], [0.0, 1.0, 0.0], [sa, 0.0, ca]];
        // Yaw about +Z, nose toward +Y.
        let yaw = [[cb, -sb, 0.0], [sb, cb, 0.0], [0.0, 0.0, 1.0]];
        // Roll about the streamwise axis.
        let roll = [[1.0, 0.0, 0.0], [0.0, cr, -sr], [0.0, sr, cr]];
        multiply3(multiply3(yaw, aoa), roll)
    }
}

fn multiply3(a: [[f64; 3]; 3], b: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    std::array::from_fn(|row| {
        std::array::from_fn(|col| (0..3).map(|k| a[row][k] * b[k][col]).sum())
    })
}

/// Voxelize a mesh onto the solver grid `[0,2π]³` at resolution `n` with the
/// body in its imported orientation. See [`voxelize_oriented`].
pub fn voxelize(mesh: &Mesh, n: usize) -> Result<VoxelMask, String> {
    voxelize_oriented(mesh, n, BodyOrientation::default())
}

/// Voxelize a mesh onto the solver grid `[0,2π]³` at resolution `n`: the body is
/// rotated by `orientation` about its own bounding-box centre, then scaled so
/// its largest cross-stream (y/z) extent is `0.6` solver units (inside the
/// trained π/7..π/4 band) and centred at the training obstacle station
/// (x=0.65π, y=z=π). Independent ray-parity classifications along +X, +Y,
/// and +Z are majority-voted; their disagreement remains part of preflight.
///
/// The returned `transform_4x4` composes rotation, scale, and placement, so
/// `engineering::solver_point_to_source_m` still round-trips solver points back
/// into the approved source frame with the orientation applied.
pub fn voxelize_oriented(
    mesh: &Mesh,
    n: usize,
    orientation: BodyOrientation,
) -> Result<VoxelMask, String> {
    if !orientation.is_finite() {
        return Err("body orientation angles must be finite".into());
    }
    let (mut source_lo, mut source_hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for t in &mesh.tris {
        for v in t {
            for c in 0..3 {
                source_lo[c] = source_lo[c].min(v[c]);
                source_hi[c] = source_hi[c].max(v[c]);
            }
        }
    }
    if mesh.tris.is_empty() {
        return Err("mesh contains no triangles".into());
    }
    // Rotate about the source bounding-box centre, then refit: the fit has to
    // see the rotated silhouette or a pitched body would poke out of the tunnel.
    let pivot: [f32; 3] = std::array::from_fn(|c| (source_lo[c] + source_hi[c]) * 0.5);
    let rotation = orientation.matrix();
    let rotate = |v: [f32; 3]| -> [f32; 3] {
        let local = [
            (v[0] - pivot[0]) as f64,
            (v[1] - pivot[1]) as f64,
            (v[2] - pivot[2]) as f64,
        ];
        std::array::from_fn(|row| {
            (rotation[row][0] * local[0]
                + rotation[row][1] * local[1]
                + rotation[row][2] * local[2]) as f32
        })
    };
    // A level body skips the rotation entirely, so the imported-attitude path
    // stays bit-identical to what it produced before orientation existed.
    let oriented: Vec<[[f32; 3]; 3]> = if orientation.is_identity() {
        mesh.tris.clone()
    } else {
        mesh.tris
            .iter()
            .map(|t| [rotate(t[0]), rotate(t[1]), rotate(t[2])])
            .collect()
    };
    let mesh = &Mesh { tris: oriented };
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for t in &mesh.tris {
        for v in t {
            for c in 0..3 {
                lo[c] = lo[c].min(v[c]);
                hi[c] = hi[c].max(v[c]);
            }
        }
    }
    let ext = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
    let cross = ext[1].max(ext[2]).max(1e-9);
    let target_char = 0.6f32; // solver units, mid training band
    let scale = target_char / cross;
    let tau = std::f32::consts::TAU;
    let centre = [
        0.65 * std::f32::consts::PI,
        std::f32::consts::PI,
        std::f32::consts::PI,
    ];
    let mid = [
        (lo[0] + hi[0]) * 0.5,
        (lo[1] + hi[1]) * 0.5,
        (lo[2] + hi[2]) * 0.5,
    ];
    let map = |v: [f32; 3]| {
        [
            centre[0] + (v[0] - mid[0]) * scale,
            centre[1] + (v[1] - mid[1]) * scale,
            centre[2] + (v[2] - mid[2]) * scale,
        ]
    };
    let tris: Vec<[[f32; 3]; 3]> = mesh
        .tris
        .iter()
        .map(|t| [map(t[0]), map(t[1]), map(t[2])])
        .collect();

    let dx = tau / n as f32;
    let classifications = [
        parity_mask_axis(&tris, n, 0, dx),
        parity_mask_axis(&tris, n, 1, dx),
        parity_mask_axis(&tris, n, 2, dx),
    ];
    let odd_crossing_rows = [
        classifications[0].1,
        classifications[1].1,
        classifications[2].1,
    ];
    let mut disagreement_cells = 0usize;
    let mut union_cells = 0usize;
    let mut solid = 0usize;
    let mut mask = vec![0.0f32; n * n * n];
    for (index, value) in mask.iter_mut().enumerate() {
        let votes = classifications
            .iter()
            .filter(|classification| classification.0[index] != 0)
            .count();
        if votes > 0 {
            union_cells += 1;
        }
        if votes > 0 && votes < 3 {
            disagreement_cells += 1;
        }
        if votes >= 2 {
            *value = 1.0;
            solid += 1;
        }
    }
    let axis_disagreement_fraction = if union_cells == 0 {
        0.0
    } else {
        disagreement_cells as f64 / union_cells as f64
    };
    if solid == 0 {
        // Distinguish the two ways a mesh can voxelize to nothing. A thin body
        // (a wing at full span, say) is watertight and correct — the auto-fit
        // simply left it under a cell thick — and telling the operator to repair
        // their mesh would be a wrong answer, so name the real cause.
        let thinnest = ext.iter().copied().fold(f32::MAX, f32::min) * scale / dx;
        let axis = ["x", "y", "z"][ext
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(axis, _)| axis)
            .unwrap_or(0)];
        return Err(if thinnest < 1.0 {
            format!(
                "voxelization produced an empty solid: after auto-fit the body is \
                 {thinnest:.2} cells thick across {axis} on a {n}³ grid. Raise the grid \
                 resolution, or trim the span so the thin dimension is not fit against it."
            )
        } else {
            "voxelization produced an empty solid (is the mesh watertight?)".into()
        });
    }
    let (components, minimum_cells_across, boundary_clearance_cells) = voxel_diagnostics(&mask, n);
    // Column-major 4×4 for `scale · R · (v − pivot) − scale · mid + centre`:
    // the single transform that carries an imported source vertex all the way
    // to its solver cell, orientation included.
    let scale = scale as f64;
    let linear: [[f64; 3]; 3] =
        std::array::from_fn(|row| std::array::from_fn(|col| scale * rotation[row][col]));
    let translation: [f64; 3] = std::array::from_fn(|row| {
        centre[row] as f64
            - scale
                * ((0..3)
                    .map(|k| rotation[row][k] * pivot[k] as f64)
                    .sum::<f64>()
                    + mid[row] as f64)
    });
    let transform_4x4 = [
        linear[0][0],
        linear[1][0],
        linear[2][0],
        0.0,
        linear[0][1],
        linear[1][1],
        linear[2][1],
        0.0,
        linear[0][2],
        linear[1][2],
        linear[2][2],
        0.0,
        translation[0],
        translation[1],
        translation[2],
        1.0,
    ];
    Ok(VoxelMask {
        n,
        mask,
        solid_voxels: solid,
        char_len: target_char,
        components,
        minimum_cells_across,
        boundary_clearance_cells,
        axis_disagreement_fraction,
        odd_crossing_rows,
        classification_version: 2,
        scale,
        orientation,
        transform_4x4,
    })
}

/// Axis-specific parity classification with 2D triangle bins. `axis` is the
/// ray direction; the remaining coordinates index scanlines. The returned odd
/// count is evidence, not the sole validity gate: a missing cap can remove both
/// crossings and therefore retain an even count.
fn parity_mask_axis(tris: &[[[f32; 3]; 3]], n: usize, axis: usize, dx: f32) -> (Vec<u8>, usize) {
    let u_axis = (axis + 1) % 3;
    let v_axis = (axis + 2) % 3;
    let cell_of = |w: f32| ((w / dx) as i64).clamp(0, n as i64 - 1) as usize;
    let mut bins: Vec<Vec<u32>> = vec![Vec::new(); n * n];
    for (triangle_index, triangle) in tris.iter().enumerate() {
        let mut ulo = f32::MAX;
        let mut uhi = f32::MIN;
        let mut vlo = f32::MAX;
        let mut vhi = f32::MIN;
        for vertex in triangle {
            ulo = ulo.min(vertex[u_axis]);
            uhi = uhi.max(vertex[u_axis]);
            vlo = vlo.min(vertex[v_axis]);
            vhi = vhi.max(vertex[v_axis]);
        }
        for u in cell_of(ulo)..=cell_of(uhi) {
            for v in cell_of(vlo)..=cell_of(vhi) {
                bins[u * n + v].push(triangle_index as u32);
            }
        }
    }

    let jitter = [[1.3e-4, 2.7e-4], [2.1e-4, 1.1e-4], [1.7e-4, 2.3e-4]][axis];
    let mut mask = vec![0u8; n * n * n];
    let mut odd_rows = 0usize;
    for u in 0..n {
        let u_coord = (u as f32 + 0.5) * dx + jitter[0];
        for v in 0..n {
            let v_coord = (v as f32 + 0.5) * dx + jitter[1];
            let mut hits = Vec::new();
            for &triangle_index in &bins[u * n + v] {
                if let Some(hit) =
                    ray_axis_hit(&tris[triangle_index as usize], axis, u_coord, v_coord)
                {
                    hits.push(hit);
                }
            }
            hits.sort_by(|a, b| a.partial_cmp(b).unwrap());
            if hits.len() % 2 == 1 {
                odd_rows += 1;
            }
            for pair in hits.chunks_exact(2) {
                for ray_cell in 0..n {
                    let coordinate = (ray_cell as f32 + 0.5) * dx;
                    if coordinate >= pair[0] && coordinate <= pair[1] {
                        let mut cell = [0usize; 3];
                        cell[axis] = ray_cell;
                        cell[u_axis] = u;
                        cell[v_axis] = v;
                        mask[cell[0] * n * n + cell[1] * n + cell[2]] = 1;
                    }
                }
            }
        }
    }
    (mask, odd_rows)
}

fn ray_axis_hit(triangle: &[[f32; 3]; 3], axis: usize, u: f32, v: f32) -> Option<f32> {
    let u_axis = (axis + 1) % 3;
    let v_axis = (axis + 2) % 3;
    let a = triangle[0];
    let b = triangle[1];
    let c = triangle[2];
    let determinant = (b[u_axis] - a[u_axis]) * (c[v_axis] - a[v_axis])
        - (c[u_axis] - a[u_axis]) * (b[v_axis] - a[v_axis]);
    if determinant.abs() < 1e-20 {
        return None;
    }
    let alpha = ((u - a[u_axis]) * (c[v_axis] - a[v_axis])
        - (c[u_axis] - a[u_axis]) * (v - a[v_axis]))
        / determinant;
    let beta = ((b[u_axis] - a[u_axis]) * (v - a[v_axis])
        - (u - a[u_axis]) * (b[v_axis] - a[v_axis]))
        / determinant;
    if alpha < 0.0 || beta < 0.0 || alpha + beta > 1.0 {
        return None;
    }
    Some(a[axis] + alpha * (b[axis] - a[axis]) + beta * (c[axis] - a[axis]))
}

/// Bounds of the solid, in render-domain coordinates (`[-1,1]³`, the frame the
/// raymarch and the CPU projector share). `None` when nothing is solid. Voxel
/// `i` spans `[2i/n − 1, 2(i+1)/n − 1]`, matching how the shader samples the
/// mask texture, so zoom-to-fit frames exactly what is drawn.
pub fn mask_bounds(mask: &[f32], n: usize) -> Option<([f32; 3], [f32; 3])> {
    if n == 0 || mask.len() != n * n * n {
        return None;
    }
    let mut lo = [usize::MAX; 3];
    let mut hi = [0usize; 3];
    let mut found = false;
    for i in 0..n {
        for j in 0..n {
            for k in 0..n {
                if mask[i * n * n + j * n + k] > 0.5 {
                    found = true;
                    for (axis, cell) in [i, j, k].into_iter().enumerate() {
                        lo[axis] = lo[axis].min(cell);
                        hi[axis] = hi[axis].max(cell);
                    }
                }
            }
        }
    }
    if !found {
        return None;
    }
    let edge = |cell: usize| 2.0 * cell as f32 / n as f32 - 1.0;
    Some((
        std::array::from_fn(|axis| edge(lo[axis])),
        std::array::from_fn(|axis| edge(hi[axis] + 1)),
    ))
}

/// Centre of voxel `(i,j,k)` in render-domain coordinates.
pub fn voxel_center(cell: [usize; 3], n: usize) -> [f32; 3] {
    std::array::from_fn(|axis| 2.0 * (cell[axis] as f32 + 0.5) / n as f32 - 1.0)
}

/// Pick the first solid voxel a ray enters, plus the last fluid voxel in front
/// of it — the surface-adjacent cell the diffuse-interface loads are defined on.
/// `origin`/`dir` are in render-domain coordinates; `slice[a]` clips everything
/// below that coordinate exactly as the raymarch does, so the probe can only
/// hit what is visible. Returns `(solid_cell, surface_cell)`.
pub fn pick_solid_voxel(
    mask: &[f32],
    n: usize,
    origin: [f32; 3],
    dir: [f32; 3],
    slice: [Option<f32>; 3],
) -> Option<([usize; 3], [usize; 3])> {
    if n == 0 || mask.len() != n * n * n {
        return None;
    }
    // Clip the ray to the domain box (the slab method the shader uses).
    let mut t_enter = 0.0f32;
    let mut t_exit = f32::MAX;
    for axis in 0..3 {
        let lo = slice[axis]
            .map(|clip| clip.clamp(-1.0, 1.0))
            .unwrap_or(-1.0);
        if dir[axis].abs() < 1e-6 {
            if origin[axis] < lo || origin[axis] > 1.0 {
                return None;
            }
            continue;
        }
        let inverse = 1.0 / dir[axis];
        let first = (lo - origin[axis]) * inverse;
        let second = (1.0 - origin[axis]) * inverse;
        t_enter = t_enter.max(first.min(second));
        t_exit = t_exit.min(first.max(second));
    }
    if t_exit <= t_enter {
        return None;
    }
    let step = 1.0 / n as f32 * 0.4; // ~2.5 samples per cell
    let cell_of = |value: f32| (((value + 1.0) * 0.5 * n as f32) as isize).clamp(0, n as isize - 1);
    let mut previous_fluid: Option<[usize; 3]> = None;
    let mut t = t_enter + step * 0.5;
    while t < t_exit {
        let point: [f32; 3] = std::array::from_fn(|axis| origin[axis] + dir[axis] * t);
        let cell = [
            cell_of(point[0]) as usize,
            cell_of(point[1]) as usize,
            cell_of(point[2]) as usize,
        ];
        let index = cell[0] * n * n + cell[1] * n + cell[2];
        if mask[index] > 0.5 {
            return Some((cell, previous_fluid.unwrap_or(cell)));
        }
        previous_fluid = Some(cell);
        t += step;
    }
    None
}

fn voxel_diagnostics(mask: &[f32], n: usize) -> (usize, usize, usize) {
    let index = |i: usize, j: usize, k: usize| i * n * n + j * n + k;
    let mut bounds_lo = [n; 3];
    let mut bounds_hi = [0usize; 3];
    let mut occupied = false;
    for i in 0..n {
        for j in 0..n {
            for k in 0..n {
                if mask[index(i, j, k)] > 0.5 {
                    occupied = true;
                    bounds_lo[0] = bounds_lo[0].min(i);
                    bounds_lo[1] = bounds_lo[1].min(j);
                    bounds_lo[2] = bounds_lo[2].min(k);
                    bounds_hi[0] = bounds_hi[0].max(i);
                    bounds_hi[1] = bounds_hi[1].max(j);
                    bounds_hi[2] = bounds_hi[2].max(k);
                }
            }
        }
    }
    if !occupied {
        return (0, 0, 0);
    }
    // A bounding-box extent is not a thickness: a thin swept wing can span the
    // whole domain while remaining one cell thick. Measure the thickest local
    // orthogonal core instead. For each occupied cell, find the contiguous run
    // through it along each axis; the smallest of those three runs is its local
    // thickness. The maximum local value describes the best-resolved core and
    // correctly leaves a one-cell sheet at one cell.
    let mut runs = [
        vec![0u16; mask.len()],
        vec![0u16; mask.len()],
        vec![0u16; mask.len()],
    ];
    for axis in 0..3 {
        let u_axis = (axis + 1) % 3;
        let v_axis = (axis + 2) % 3;
        for u in 0..n {
            for v in 0..n {
                let mut start = None;
                for coordinate in 0..=n {
                    let occupied = if coordinate < n {
                        let mut cell = [0usize; 3];
                        cell[axis] = coordinate;
                        cell[u_axis] = u;
                        cell[v_axis] = v;
                        mask[index(cell[0], cell[1], cell[2])] > 0.5
                    } else {
                        false
                    };
                    if occupied && start.is_none() {
                        start = Some(coordinate);
                    } else if !occupied {
                        if let Some(run_start) = start.take() {
                            let run_length = (coordinate - run_start).min(u16::MAX as usize) as u16;
                            for member in run_start..coordinate {
                                let mut cell = [0usize; 3];
                                cell[axis] = member;
                                cell[u_axis] = u;
                                cell[v_axis] = v;
                                runs[axis][index(cell[0], cell[1], cell[2])] = run_length;
                            }
                        }
                    }
                }
            }
        }
    }
    let minimum_cells_across = (0..mask.len())
        .filter(|&cell| mask[cell] > 0.5)
        .map(|cell| runs[0][cell].min(runs[1][cell]).min(runs[2][cell]) as usize)
        .max()
        .unwrap_or(0);
    let boundary_clearance_cells = (0..3)
        .flat_map(|axis| [bounds_lo[axis], n - 1 - bounds_hi[axis]])
        .min()
        .unwrap_or(0);

    let mut visited = vec![false; mask.len()];
    let mut components = 0usize;
    for i in 0..n {
        for j in 0..n {
            for k in 0..n {
                let start = index(i, j, k);
                if mask[start] <= 0.5 || visited[start] {
                    continue;
                }
                components += 1;
                visited[start] = true;
                let mut queue = VecDeque::from([(i, j, k)]);
                while let Some((ci, cj, ck)) = queue.pop_front() {
                    for (di, dj, dk) in [
                        (-1isize, 0isize, 0isize),
                        (1, 0, 0),
                        (0, -1, 0),
                        (0, 1, 0),
                        (0, 0, -1),
                        (0, 0, 1),
                    ] {
                        let ni = ci as isize + di;
                        let nj = cj as isize + dj;
                        let nk = ck as isize + dk;
                        if ni < 0
                            || nj < 0
                            || nk < 0
                            || ni >= n as isize
                            || nj >= n as isize
                            || nk >= n as isize
                        {
                            continue;
                        }
                        let neighbor = index(ni as usize, nj as usize, nk as usize);
                        if mask[neighbor] > 0.5 && !visited[neighbor] {
                            visited[neighbor] = true;
                            queue.push_back((ni as usize, nj as usize, nk as usize));
                        }
                    }
                }
            }
        }
    }
    (components, minimum_cells_across, boundary_clearance_cells)
}

/// Recovered-pressure surface insights: scan fluid voxels adjacent to the solid
/// and pin the maximum (stagnation/load point) and minimum (suction/low-pressure
/// point). These density-normalized recovered values are not a physical pressure
/// coefficient.
pub fn surface_insights(mask: &[f32], p: &[f32], n: usize) -> Vec<Insight3D> {
    let at = |v: &[f32], i: usize, j: usize, k: usize| v[i * n * n + j * n + k];
    let mut hi = (f32::MIN, [0usize; 3]);
    let mut lo = (f32::MAX, [0usize; 3]);
    let mut found = false;
    for i in 1..n - 1 {
        for j in 1..n - 1 {
            for k in 1..n - 1 {
                if at(mask, i, j, k) > 0.5 {
                    continue;
                } // want FLUID cells
                let near_solid = at(mask, i + 1, j, k) > 0.5
                    || at(mask, i - 1, j, k) > 0.5
                    || at(mask, i, j + 1, k) > 0.5
                    || at(mask, i, j - 1, k) > 0.5
                    || at(mask, i, j, k + 1) > 0.5
                    || at(mask, i, j, k - 1) > 0.5;
                if !near_solid {
                    continue;
                }
                found = true;
                let pv = at(p, i, j, k);
                if pv > hi.0 {
                    hi = (pv, [i, j, k]);
                }
                if pv < lo.0 {
                    lo = (pv, [i, j, k]);
                }
            }
        }
    }
    if !found {
        return Vec::new();
    }
    let to_pos = |c: [usize; 3]| {
        [
            c[0] as f32 / (n - 1) as f32 * 2.0 - 1.0,
            c[1] as f32 / (n - 1) as f32 * 2.0 - 1.0,
            c[2] as f32 / (n - 1) as f32 * 2.0 - 1.0,
        ]
    };
    vec![
        Insight3D {
            kind: Insight3DKind::SurfLoad,
            pos: to_pos(hi.1),
            value: hi.0,
        },
        Insight3D {
            kind: Insight3DKind::SurfSuction,
            pos: to_pos(lo.1),
            value: lo.0,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A binary STL of an axis-aligned cuboid (12 triangles).
    fn cube_stl(lo: [f32; 3], hi: [f32; 3]) -> Vec<u8> {
        let v = |x: usize, y: usize, z: usize| {
            [
                if x == 0 { lo[0] } else { hi[0] },
                if y == 0 { lo[1] } else { hi[1] },
                if z == 0 { lo[2] } else { hi[2] },
            ]
        };
        // 12 triangles, two per face
        let quads = [
            [v(0, 0, 0), v(0, 1, 0), v(0, 1, 1), v(0, 0, 1)], // x-
            [v(1, 0, 0), v(1, 0, 1), v(1, 1, 1), v(1, 1, 0)], // x+
            [v(0, 0, 0), v(0, 0, 1), v(1, 0, 1), v(1, 0, 0)], // y-
            [v(0, 1, 0), v(1, 1, 0), v(1, 1, 1), v(0, 1, 1)], // y+
            [v(0, 0, 0), v(1, 0, 0), v(1, 1, 0), v(0, 1, 0)], // z-
            [v(0, 0, 1), v(0, 1, 1), v(1, 1, 1), v(1, 0, 1)], // z+
        ];
        let mut tris: Vec<[[f32; 3]; 3]> = Vec::new();
        for q in quads {
            tris.push([q[0], q[1], q[2]]);
            tris.push([q[0], q[2], q[3]]);
        }
        let mut out = vec![0u8; 80];
        out.extend_from_slice(&(tris.len() as u32).to_le_bytes());
        for t in tris {
            out.extend_from_slice(&[0u8; 12]); // normal (unused)
            for vert in t {
                for c in vert {
                    out.extend_from_slice(&c.to_le_bytes());
                }
            }
            out.extend_from_slice(&[0u8; 2]); // attribute
        }
        out
    }

    #[test]
    fn parses_binary_and_ascii() {
        let bin = cube_stl([0.0; 3], [1.0; 3]);
        let m = parse_stl(&bin).expect("binary parse");
        assert_eq!(m.tris.len(), 12);

        let ascii = "solid cube\n facet normal 0 0 0\n  outer loop\n   vertex 0 0 0\n   vertex 1 0 0\n   vertex 0 1 0\n  endloop\n endfacet\nendsolid";
        let m = parse_stl(ascii.as_bytes()).expect("ascii parse");
        assert_eq!(m.tris.len(), 1);
    }

    #[test]
    fn parse_geometry_stl_records_no_shell_selection() {
        let bin = cube_stl([0.0; 3], [1.0; 3]);
        let imported = parse_geometry("body.stl", &bin).expect("stl parse");
        assert_eq!(imported.source_shells, 1);
        assert_eq!(imported.selected_shell_entity_id, None);
    }

    #[test]
    fn parse_geometry_selecting_copies_step_pick_one_shell_id() {
        let bytes = include_bytes!("../test-geometry/corpus/vendor/nist_ctc03_ap203_geom.step");
        match parse_geometry_selecting("nist.step", bytes, None) {
            Err(GeometryParseError::ChooseShell(choice)) => {
                let selected = choice.shells[0].entity_id;
                let imported = parse_geometry_selecting("nist.step", bytes, Some(selected))
                    .expect("NIST multi-shell CTC should import after pick-one");
                assert_eq!(imported.selected_shell_entity_id, Some(selected));
            }
            Ok(imported) => assert_eq!(imported.selected_shell_entity_id, None),
            Err(GeometryParseError::Message(error)) => {
                panic!("NIST CTC AP203 geometry should import: {error}")
            }
        }
    }

    #[test]
    fn topology_preflight_distinguishes_closed_and_open_sources() {
        let cube = parse_stl(&cube_stl([0.0; 3], [1.0; 3])).unwrap();
        let closed = diagnose_mesh(&cube);
        assert_eq!(closed.triangles, 12);
        assert_eq!(closed.components, 1);
        assert_eq!(closed.boundary_edges, 0);
        assert_eq!(closed.non_manifold_edges, 0);
        assert_eq!(closed.degenerate_triangles, 0);

        let open = Mesh {
            tris: vec![[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]],
        };
        let open = diagnose_mesh(&open);
        assert_eq!(open.boundary_edges, 3);
        assert_eq!(open.components, 1);
    }

    #[test]
    fn voxelized_cube_fills_the_right_box() {
        // a tall cuboid: y/z cross-section 1×1 (the char dimension), x length 2
        let mesh = parse_stl(&cube_stl([0.0, 0.0, 0.0], [2.0, 1.0, 1.0])).unwrap();
        let n = 32;
        let vm = voxelize(&mesh, n).expect("voxelize");
        assert_eq!(vm.n, n);
        // char target 0.6 solver units at dx = 2π/32 ≈ 0.196 → ~3 cells across,
        // x is 2× the cross-section → ~6 cells; expect roughly 3*3*6 ≈ 54 solid
        assert!(
            vm.solid_voxels >= 18 && vm.solid_voxels <= 200,
            "unexpected solid count {}",
            vm.solid_voxels
        );
        // the solid must be near the training obstacle station (x≈0.65π→i≈10, y=z≈π→16)
        let mut ci = 0f32;
        let mut cj = 0f32;
        let mut ck = 0f32;
        let mut m = 0f32;
        for i in 0..n {
            for j in 0..n {
                for k in 0..n {
                    let w = vm.mask[i * n * n + j * n + k];
                    ci += w * i as f32;
                    cj += w * j as f32;
                    ck += w * k as f32;
                    m += w;
                }
            }
        }
        let (ci, cj, ck) = (ci / m, cj / m, ck / m);
        assert!((ci - 0.325 * n as f32).abs() < 2.0, "centroid x off: {ci}");
        assert!((cj - 0.5 * n as f32).abs() < 2.0, "centroid y off: {cj}");
        assert!((ck - 0.5 * n as f32).abs() < 2.0, "centroid z off: {ck}");
    }

    /// Centroid of the solid, in cell units.
    fn mask_centroid(vm: &VoxelMask) -> [f32; 3] {
        let n = vm.n;
        let mut sum = [0f32; 3];
        let mut weight = 0f32;
        for i in 0..n {
            for j in 0..n {
                for k in 0..n {
                    let w = vm.mask[i * n * n + j * n + k];
                    sum[0] += w * i as f32;
                    sum[1] += w * j as f32;
                    sum[2] += w * k as f32;
                    weight += w;
                }
            }
        }
        sum.map(|value| value / weight.max(1e-6))
    }

    #[test]
    fn angle_of_attack_rotates_the_body_against_a_fixed_stream() {
        // A slab that is long in x and thin in z: pitching it must grow its
        // vertical footprint while the stream stays on +X.
        let mesh = parse_stl(&cube_stl([0.0, 0.0, 0.0], [4.0, 1.0, 0.5])).unwrap();
        let n = 48;
        let level = voxelize(&mesh, n).expect("level");
        let pitched = voxelize_oriented(
            &mesh,
            n,
            BodyOrientation {
                angle_of_attack_deg: 30.0,
                ..Default::default()
            },
        )
        .expect("pitched");
        let extent = |vm: &VoxelMask, axis: usize| {
            let mut lo = n;
            let mut hi = 0;
            for i in 0..n {
                for j in 0..n {
                    for k in 0..n {
                        if vm.mask[i * n * n + j * n + k] > 0.5 {
                            let cell = [i, j, k][axis];
                            lo = lo.min(cell);
                            hi = hi.max(cell);
                        }
                    }
                }
            }
            hi + 1 - lo
        };
        assert!(
            extent(&pitched, 2) > extent(&level, 2),
            "pitching must deepen the vertical footprint ({} → {})",
            extent(&level, 2),
            extent(&pitched, 2)
        );
        // The body stays centred on the training station and inside the tunnel.
        let centroid = mask_centroid(&pitched);
        assert!((centroid[0] - 0.325 * n as f32).abs() < 3.0, "{centroid:?}");
        assert!((centroid[1] - 0.5 * n as f32).abs() < 2.0, "{centroid:?}");
        assert!((centroid[2] - 0.5 * n as f32).abs() < 2.0, "{centroid:?}");
        assert!(pitched.boundary_clearance_cells >= 2);
        // Zero rotation must be bit-identical to the unoriented path.
        let identity = voxelize_oriented(&mesh, n, BodyOrientation::default()).expect("identity");
        assert_eq!(identity.transform_4x4, level.transform_4x4);
        assert_eq!(identity.solid_voxels, level.solid_voxels);
    }

    #[test]
    fn orientation_matrix_and_transform_round_trip_to_the_source_frame() {
        let orientation = BodyOrientation {
            angle_of_attack_deg: 12.0,
            yaw_deg: -8.0,
            roll_deg: 4.0,
        };
        let rotation = orientation.matrix();
        // Rotations preserve length and are right-handed.
        for row in rotation {
            let norm = row.iter().map(|value| value * value).sum::<f64>().sqrt();
            assert!((norm - 1.0).abs() < 1e-12, "row norm {norm}");
        }
        // Positive angle of attack pitches the nose (+X) up toward +Z.
        let nose = BodyOrientation {
            angle_of_attack_deg: 20.0,
            ..Default::default()
        }
        .matrix();
        let tip = rotation_apply(nose, [1.0, 0.0, 0.0]);
        assert!(tip[2] > 0.3, "nose should rise, got {tip:?}");
        // Positive yaw swings the nose toward +Y.
        let swung = rotation_apply(
            BodyOrientation {
                yaw_deg: 20.0,
                ..Default::default()
            }
            .matrix(),
            [1.0, 0.0, 0.0],
        );
        assert!(swung[1] > 0.3, "nose should swing to +Y, got {swung:?}");

        // The recorded transform must invert back onto the source vertex, so
        // exported surface points remain in the approved source frame.
        let mesh = parse_stl(&cube_stl([1.0, 2.0, 3.0], [5.0, 4.0, 4.0])).unwrap();
        let vm = voxelize_oriented(&mesh, 32, orientation).expect("voxelize");
        let source = [2.0f64, 3.0, 3.5];
        let t = vm.transform_4x4;
        let solver: [f64; 3] = std::array::from_fn(|row| {
            t[row] * source[0] + t[4 + row] * source[1] + t[8 + row] * source[2] + t[12 + row]
        });
        let back =
            crate::engineering::solver_point_to_source_m(solver, t, 1.0).expect("invertible");
        for axis in 0..3 {
            assert!(
                (back[axis] - source[axis]).abs() < 1e-6,
                "axis {axis}: {back:?} vs {source:?}"
            );
        }
        assert!((vm.scale - t[0] / rotation[0][0]).abs() < 1e-9);
    }

    fn rotation_apply(rotation: [[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
        std::array::from_fn(|row| (0..3).map(|k| rotation[row][k] * v[k]).sum())
    }

    #[test]
    fn mask_bounds_cover_the_solid_in_domain_coordinates() {
        let n = 16;
        let mut mask = vec![0f32; n * n * n];
        for i in 4..8 {
            for j in 6..10 {
                for k in 2..4 {
                    mask[i * n * n + j * n + k] = 1.0;
                }
            }
        }
        let (lo, hi) = mask_bounds(&mask, n).expect("solid");
        assert!((lo[0] - (-0.5)).abs() < 1e-6, "{lo:?}");
        assert!((hi[0] - 0.0).abs() < 1e-6, "{hi:?}");
        assert!((lo[2] - (-0.75)).abs() < 1e-6, "{lo:?}");
        assert!((hi[2] - (-0.5)).abs() < 1e-6, "{hi:?}");
        assert!(mask_bounds(&vec![0f32; n * n * n], n).is_none());
        assert!(mask_bounds(&mask, n + 1).is_none());
    }

    #[test]
    fn ray_pick_reports_the_first_solid_and_its_surface_cell() {
        let n = 16;
        let mut mask = vec![0f32; n * n * n];
        for i in 6..10 {
            for j in 6..10 {
                for k in 6..10 {
                    mask[i * n * n + j * n + k] = 1.0;
                }
            }
        }
        // Shoot downstream along +X through the middle of the block.
        let center = voxel_center([0, 7, 7], n);
        let (solid, surface) = pick_solid_voxel(
            &mask,
            n,
            [-2.0, center[1], center[2]],
            [1.0, 0.0, 0.0],
            [None; 3],
        )
        .expect("hit");
        assert_eq!(solid, [6, 7, 7]);
        assert_eq!(
            surface,
            [5, 7, 7],
            "surface cell must be the fluid in front"
        );
        // A ray that misses reports nothing rather than a false surface point.
        assert!(pick_solid_voxel(&mask, n, [-2.0, 0.9, 0.9], [1.0, 0.0, 0.0], [None; 3]).is_none());
        // A clip plane in front of the body hides it, exactly as it hides it in
        // the raymarch.
        assert!(pick_solid_voxel(
            &mask,
            n,
            [-2.0, center[1], center[2]],
            [1.0, 0.0, 0.0],
            [Some(0.5), None, None]
        )
        .is_none());
    }

    #[test]
    fn surface_insights_find_load_and_suction() {
        let n = 12;
        let mut mask = vec![0f32; n * n * n];
        let mut p = vec![0f32; n * n * n];
        // a solid block in the middle
        for i in 4..8 {
            for j in 4..8 {
                for k in 4..8 {
                    mask[i * n * n + j * n + k] = 1.0;
                }
            }
        }
        // pressure: high in front of the block (i=3 face), low behind (i=8)
        p[3 * n * n + 5 * n + 5] = 2.0;
        p[8 * n * n + 6 * n + 6] = -1.5;
        let out = surface_insights(&mask, &p, n);
        assert_eq!(out.len(), 2);
        let load = out
            .iter()
            .find(|x| x.kind == Insight3DKind::SurfLoad)
            .unwrap();
        let suck = out
            .iter()
            .find(|x| x.kind == Insight3DKind::SurfSuction)
            .unwrap();
        assert!((load.value - 2.0).abs() < 1e-6);
        assert!((suck.value + 1.5).abs() < 1e-6);
    }

    fn fixture(name: &str) -> Mesh {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-geometry")
            .join(name);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        parse_stl(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn defective_fixture(name: &str) -> Mesh {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-geometry")
            .join("defective")
            .join(name);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        parse_stl(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// The real millimetre-unit fixtures in `test-geometry/` must survive the
    /// whole app-side path an operator walks: parse → topology preflight →
    /// oriented voxelization → surface pick. Synthetic cubes cannot catch a
    /// tessellated body poking out of the tunnel once it is pitched.
    #[test]
    fn shipped_test_geometry_imports_pitches_and_picks() {
        for name in [
            "cube_100mm.stl",
            "sphere_d100mm.stl",
            "cylinder_d60_l200mm.stl",
            "capsule_d80_l260mm.stl",
        ] {
            let mesh = fixture(name);
            let diagnostics = diagnose_mesh(&mesh);
            assert_eq!(diagnostics.boundary_edges, 0, "{name} should be watertight");
            assert_eq!(diagnostics.non_manifold_edges, 0, "{name}");
            assert_eq!(diagnostics.components, 1, "{name}");

            let n = 48;
            let level = voxelize(&mesh, n).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(level.solid_voxels > 0, "{name} voxelized to nothing");
            assert_eq!(level.classification_version, 2);
            eprintln!(
                "{name}: disagreement {:.3}% odd {:?} solid {} core {}",
                level.axis_disagreement_fraction * 100.0,
                level.odd_crossing_rows,
                level.solid_voxels,
                level.minimum_cells_across
            );
            assert!(
                level.axis_disagreement_fraction
                    <= crate::engineering::GeometryPreflight::MAX_AXIS_DISAGREEMENT_FRACTION,
                "{name} valid fixture disagrees by {:.3}%",
                level.axis_disagreement_fraction * 100.0
            );
            assert!(
                level.boundary_clearance_cells >= 2,
                "{name} touches the wall"
            );

            // A 10° angle of attack must keep the body inside the tunnel and on
            // the training station, and must actually change the mask.
            let pitched = voxelize_oriented(
                &mesh,
                n,
                BodyOrientation {
                    angle_of_attack_deg: 10.0,
                    ..Default::default()
                },
            )
            .unwrap_or_else(|e| panic!("{name} at 10°: {e}"));
            assert!(pitched.boundary_clearance_cells >= 2, "{name} at 10°");
            let centroid = mask_centroid(&pitched);
            assert!(
                (centroid[0] - 0.325 * n as f32).abs() < 3.0,
                "{name} {centroid:?}"
            );
            assert!(
                (centroid[1] - 0.5 * n as f32).abs() < 3.0,
                "{name} {centroid:?}"
            );
            assert!(
                (centroid[2] - 0.5 * n as f32).abs() < 3.0,
                "{name} {centroid:?}"
            );

            // A streamwise ray through the body centre must find a solid cell
            // and hand back the fluid cell in front of it — the 3D probe path.
            let (lo, hi) = mask_bounds(&level.mask, n).expect("solid bounds");
            let mid = [
                (lo[1] + hi[1]) * 0.5 + 0.5 / n as f32,
                (lo[2] + hi[2]) * 0.5 + 0.5 / n as f32,
            ];
            let (solid, surface) = pick_solid_voxel(
                &level.mask,
                n,
                [-4.0, mid[0], mid[1]],
                [1.0, 0.0, 0.0],
                [None; 3],
            )
            .unwrap_or_else(|| panic!("{name}: streamwise probe missed the body"));
            assert!(
                level.mask[solid[0] * n * n + solid[1] * n + solid[2]] > 0.5,
                "{name}"
            );
            assert!(
                level.mask[surface[0] * n * n + surface[1] * n + surface[2]] <= 0.5,
                "{name}: probe must report the fluid cell, not the solid"
            );
        }
    }

    #[test]
    fn defective_geometry_is_measured_or_structurally_blockable() {
        let small = defective_fixture("sphere_leak_small.stl");
        let large = defective_fixture("sphere_leak_large.stl");
        assert!(diagnose_mesh(&small).boundary_edges > 0);
        assert!(diagnose_mesh(&large).boundary_edges > 0);

        let nested = defective_fixture("box_double_shell.stl");
        let intersecting = defective_fixture("boxes_self_intersecting.stl");
        assert!(
            diagnose_mesh(&nested).components > 1,
            "nested shells must not look like one unambiguous body"
        );
        assert!(
            diagnose_mesh(&intersecting).components > 1,
            "interpenetrating disconnected shells must be blocked"
        );
        assert!(
            diagnose_mesh(&intersecting).self_intersection_pairs > 0,
            "interpenetrating shells must be detected geometrically, not only by component count"
        );

        let inverted = defective_fixture("box_inverted_normals.stl");
        let inverted_diagnostics = diagnose_mesh(&inverted);
        assert_eq!(inverted_diagnostics.boundary_edges, 0);
        assert_eq!(inverted_diagnostics.inconsistent_winding_edges, 0);
        assert!(
            inverted_diagnostics.signed_volume < 0.0,
            "source winding sign must remain visible as provenance"
        );

        for (name, mesh) in [
            ("small leak", small),
            ("large leak", large),
            ("nested shells", nested),
            ("intersecting shells", intersecting),
            ("inverted winding", inverted),
        ] {
            let mask = voxelize(&mesh, 64).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(mask.classification_version, 2);
            eprintln!(
                "{name}: disagreement {:.3}% odd {:?} solid {}",
                mask.axis_disagreement_fraction * 100.0,
                mask.odd_crossing_rows,
                mask.solid_voxels
            );
        }
    }

    #[test]
    #[ignore = "import-time measurement; run explicitly with --ignored --nocapture"]
    fn bench_three_axis_voxelization_128() {
        let mesh = fixture("sphere_d100mm.stl");
        let started = std::time::Instant::now();
        let mask = voxelize(&mesh, 128).expect("128³ sphere");
        eprintln!(
            "three-axis 128³ sphere: {:.3}s · {} solid · {:.3}% disagreement",
            started.elapsed().as_secs_f64(),
            mask.solid_voxels,
            mask.axis_disagreement_fraction * 100.0
        );
    }

    /// Pitching a real slender body has to change its silhouette against the
    /// fixed stream. The auto-fit renormalizes the cross-stream size, so the
    /// honest invariant is the vertical-to-streamwise ratio, which must grow.
    #[test]
    fn pitching_the_capsule_fixture_stands_it_up_against_the_stream() {
        let mesh = fixture("capsule_d80_l260mm.stl");
        let n = 64;
        let ratio = |orientation: BodyOrientation| {
            let vm = voxelize_oriented(&mesh, n, orientation).expect("voxelize");
            let (lo, hi) = mask_bounds(&vm.mask, n).expect("solid");
            (hi[2] - lo[2]) / (hi[0] - lo[0])
        };
        let level = ratio(BodyOrientation::default());
        let pitched = ratio(BodyOrientation {
            angle_of_attack_deg: 20.0,
            ..Default::default()
        });
        assert!(
            pitched > level * 1.2,
            "20° AoA should stand the capsule up ({level} → {pitched})"
        );
    }

    /// The NACA 0012 fixture is thinner than one cell once its 300 mm span is
    /// fit to the trained size band. The failure is a resolution limit, not a
    /// broken mesh, and the message must say which — telling an operator to
    /// repair a watertight body would send them down the wrong path.
    #[test]
    fn a_body_thinner_than_a_cell_reports_resolution_not_repair() {
        let mesh = fixture("naca0012_wing_c120_s300mm.stl");
        assert_eq!(diagnose_mesh(&mesh).boundary_edges, 0, "fixture is closed");
        let Err(error) = voxelize(&mesh, 64) else {
            panic!("wing should not resolve at 64³");
        };
        assert!(error.contains("cells thick"), "{error}");
        assert!(error.contains("grid resolution"), "{error}");
        assert!(
            !error.contains("watertight"),
            "must not blame the mesh: {error}"
        );
    }

    #[test]
    fn yc_demo_fixtures_match_the_real_preflight_path() {
        let asset = |name: &str| {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("demo/yc/assets")
                .join(name);
            let bytes =
                std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            parse_stl(&bytes).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        };

        let primary = asset("primary_capsule_d80_l260mm.stl");
        let primary_diagnostics = diagnose_mesh(&primary);
        assert_eq!(primary_diagnostics.triangles, 1_920);
        assert_eq!(primary_diagnostics.boundary_edges, 0);
        assert_eq!(primary_diagnostics.non_manifold_edges, 0);
        assert!(
            voxelize(&primary, 64).is_ok(),
            "the primary fixture must reach diagnostic preflight at 64³"
        );

        let cube = asset("fallback_cube_100mm.stl");
        let cube_diagnostics = diagnose_mesh(&cube);
        assert_eq!(cube_diagnostics.triangles, 12);
        assert_eq!(cube_diagnostics.boundary_edges, 0);

        let defective = asset("defective_sphere_missing_cap_r50mm.stl");
        let defective_diagnostics = diagnose_mesh(&defective);
        assert_eq!(defective_diagnostics.triangles, 1_680);
        assert_eq!(defective_diagnostics.boundary_edges, 48);
    }

    #[test]
    fn analyzed_mesh_sha256_is_stable_and_order_sensitive() {
        let mesh = Mesh {
            tris: vec![
                [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                [[0.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            ],
        };
        let again = Mesh {
            tris: mesh.tris.clone(),
        };
        let digest = analyzed_mesh_sha256(&mesh);
        assert_eq!(
            digest,
            "f1af891ccbc624026db7dddba66996d91377db6e74e6062c8eb2f71ed5c3ee57"
        );
        assert_eq!(digest, analyzed_mesh_sha256(&again));
        let swapped = Mesh {
            tris: vec![mesh.tris[1], mesh.tris[0]],
        };
        assert_ne!(digest, analyzed_mesh_sha256(&swapped));
    }
}
