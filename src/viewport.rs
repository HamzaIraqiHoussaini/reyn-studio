//! Interactive 3D flow viewport. The camera orbits, pans, zooms to the cursor,
//! frames the geometry, and snaps to named stations, under whichever mouse
//! scheme the operator picked; every 3D control (opacity, density, slice) drives
//! the render. Particles are projected here on the CPU, then handed to the
//! native wgpu bloom renderer (`gpu.rs`, N2) which lights the vortex cores with
//! a real HDR and bloom pass. A CPU halo+core fallback keeps the viewport
//! working if wgpu is ever unavailable.
use crate::cad_postpro::{self, Glyph};
use crate::flow::Particle;
use crate::gpu::{self, GpuInstance, SegInstance};
use crate::theme::*;
use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};

/// Closest and furthest orbit radius, in solver-domain units. The domain is
/// `[-1,1]³`, so 1.2 frames a small body tightly and 14 pulls the whole tunnel
/// back into view.
pub const DIST_RANGE: std::ops::RangeInclusive<f32> = 1.2..=14.0;
/// Orbit elevation limit. One degree short of the pole keeps the world-up
/// reference (`+Z`) well conditioned, so the basis never flips mid-drag.
pub const PITCH_LIMIT: f32 = 1.553_343; // 89°
/// Standard-view interpolation duration (§3.7 motion budget).
const SNAP_SECONDS: f32 = 0.28;

/// Camera pose: orbit angles about the physics frame plus the look-at target,
/// which pan moves. `+X` is the free stream, `+Y` is the side axis, and `+Z`
/// is vertical — the same frame the coefficients are reported in, so "side"
/// and "top" mean the same thing in the viewport and in the results table.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub yaw: f32,
    pub pitch: f32,
    pub dist: f32,
    pub target: [f32; 3],
}

/// A named camera station. Labels are in flow terms because the stream
/// direction is physically fixed: an axis letter alone would not say whether
/// you are looking at the nose or the wake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StandardView {
    Upstream,
    Downstream,
    SideLeft,
    SideRight,
    Top,
    Bottom,
    Iso,
}

impl StandardView {
    pub const ALL: [Self; 7] = [
        Self::Iso,
        Self::Upstream,
        Self::Downstream,
        Self::SideLeft,
        Self::SideRight,
        Self::Top,
        Self::Bottom,
    ];

    /// Flow-frame name plus the axis it looks along.
    pub fn label(self) -> &'static str {
        match self {
            Self::Upstream => "Upstream (−X)",
            Self::Downstream => "Downstream (+X)",
            Self::SideLeft => "Side (−Y)",
            Self::SideRight => "Side (+Y)",
            Self::Top => "Top (+Z)",
            Self::Bottom => "Bottom (−Z)",
            Self::Iso => "Iso",
        }
    }

    /// Compact label for the viewport control strip.
    pub fn short(self) -> &'static str {
        match self {
            Self::Upstream => "Up",
            Self::Downstream => "Down",
            Self::SideLeft => "−Y",
            Self::SideRight => "+Y",
            Self::Top => "Top",
            Self::Bottom => "Bot",
            Self::Iso => "Iso",
        }
    }

    /// What the engineer is actually looking at, for hover disclosure.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Upstream => {
                "Camera upstream of the body, looking downstream: the frontal / windward view."
            }
            Self::Downstream => "Camera downstream, looking upstream: the base and near-wake view.",
            Self::SideLeft => "Side elevation from −Y; the free stream runs left to right.",
            Self::SideRight => "Side elevation from +Y; the free stream runs right to left.",
            Self::Top => "Plan view from above (+Z), stream left to right.",
            Self::Bottom => "Plan view from below (−Z).",
            Self::Iso => "Three-quarter view from upstream, above, and −Y.",
        }
    }

    /// Orbit angles for this station. Top/bottom stop one degree short of the
    /// pole so the up-reference stays conditioned.
    pub fn angles(self) -> (f32, f32) {
        match self {
            Self::Downstream => (0.0, 0.0),
            Self::Upstream => (std::f32::consts::PI, 0.0),
            Self::SideRight => (std::f32::consts::FRAC_PI_2, 0.0),
            Self::SideLeft => (-std::f32::consts::FRAC_PI_2, 0.0),
            Self::Top => (-std::f32::consts::FRAC_PI_2, PITCH_LIMIT),
            Self::Bottom => (-std::f32::consts::FRAC_PI_2, -PITCH_LIMIT),
            Self::Iso => (-2.53, 0.5),
        }
    }
}

/// Which mouse buttons orbit, pan, and zoom. Orbit muscle memory is a real
/// switching cost, so Reyn ships the rival vendors' mappings alongside its own
/// (audit gap #1: every major CAD tool does this).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NavScheme {
    #[default]
    Reyn,
    SolidWorks,
    Fusion,
    ParaView,
}

impl NavScheme {
    pub const ALL: [Self; 4] = [Self::Reyn, Self::SolidWorks, Self::Fusion, Self::ParaView];

    pub fn label(self) -> &'static str {
        match self {
            Self::Reyn => "Reyn default",
            Self::SolidWorks => "SolidWorks-style",
            Self::Fusion => "Fusion-style",
            Self::ParaView => "ParaView-style",
        }
    }

    /// Documented mapping, shown verbatim in Settings and in the shortcut
    /// reference so the binding is discoverable without experiment.
    pub fn mapping(self) -> [(&'static str, &'static str); 3] {
        match self {
            Self::Reyn => [
                ("Orbit", "Left-drag"),
                ("Pan", "Middle-drag, or ⇧ + left-drag"),
                ("Zoom", "Scroll (toward the cursor)"),
            ],
            Self::SolidWorks => [
                ("Orbit", "Middle-drag"),
                ("Pan", "Ctrl + middle-drag, or ⇧ + left-drag"),
                ("Zoom", "Scroll (toward the cursor)"),
            ],
            Self::Fusion => [
                ("Orbit", "⇧ + middle-drag"),
                ("Pan", "Middle-drag"),
                ("Zoom", "Scroll (toward the cursor)"),
            ],
            Self::ParaView => [
                ("Orbit", "Left-drag"),
                ("Pan", "Middle-drag"),
                ("Zoom", "Right-drag, or scroll"),
            ],
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Self::Reyn => {
                "Left-drag orbits, so the primary button does the thing you do most. Matches Onshape and most web viewers."
            }
            Self::SolidWorks => {
                "SolidWorks reserves the left button for selection: the middle button orbits, and Ctrl + middle pans."
            }
            Self::Fusion => {
                "Fusion 360 pans with the middle button and orbits with ⇧ + middle."
            }
            Self::ParaView => {
                "ParaView orbits with the left button, pans with the middle button, and zooms with right-drag or scroll."
            }
        }
    }
}

/// Which navigation gesture a pointer drag means under the active scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gesture {
    None,
    Orbit,
    Pan,
}

fn gesture_for(scheme: NavScheme, primary: bool, middle: bool, shift: bool, ctrl: bool) -> Gesture {
    match scheme {
        NavScheme::Reyn => {
            if middle || (primary && shift) {
                Gesture::Pan
            } else if primary {
                Gesture::Orbit
            } else {
                Gesture::None
            }
        }
        NavScheme::SolidWorks => {
            if (middle && ctrl) || (primary && shift) {
                Gesture::Pan
            } else if middle {
                Gesture::Orbit
            } else {
                Gesture::None
            }
        }
        NavScheme::Fusion => {
            if middle && shift {
                Gesture::Orbit
            } else if middle || (primary && shift) {
                Gesture::Pan
            } else {
                Gesture::None
            }
        }
        NavScheme::ParaView => {
            if middle {
                Gesture::Pan
            } else if primary {
                Gesture::Orbit
            } else {
                Gesture::None
            }
        }
    }
}

/// Orbit camera with a movable target. Fields stay public for the status chip;
/// the interpolation state is private so poses can only change through the
/// methods that clamp them.
pub struct Camera {
    pub yaw: f32,
    pub pitch: f32,
    pub dist: f32,
    pub target: [f32; 3],
    anim: Option<(Pose, Pose, f32)>,
}

impl Default for Camera {
    fn default() -> Self {
        let (yaw, pitch) = StandardView::Iso.angles();
        Self {
            yaw,
            pitch,
            dist: 4.4,
            target: [0.0; 3],
            anim: None,
        }
    }
}

fn norm(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-9);
    [v[0] / l, v[1] / l, v[2] / l]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// How vertical a forward vector may be before the world-up reference has to
/// swing off `+Z`. It sits above `sin(PITCH_LIMIT)` on purpose: orbiting and the
/// top/bottom stations both stop at 89°, and swinging the reference there would
/// silently roll the view a quarter turn, so the plan view would no longer show
/// the stream running left to right the way its label promises.
const UP_REFERENCE_LIMIT: f32 = 0.99995; // sin(89.43°)

/// Camera basis from a forward vector, with the physics-frame world up (`+Z`).
/// Straight down the pole the reference swings to `+X` so the basis never
/// degenerates — `fs_volume` in `gpu.rs` runs the identical fallback, so CPU
/// annotations and the raymarched image agree pixel for pixel.
pub fn basis_from_forward(fwd: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    let up_ref = if fwd[2].abs() > UP_REFERENCE_LIMIT {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 0.0, 1.0]
    };
    let right = norm(cross(fwd, up_ref));
    (right, cross(right, fwd))
}

impl Camera {
    pub fn pose(&self) -> Pose {
        Pose {
            yaw: self.yaw,
            pitch: self.pitch,
            dist: self.dist,
            target: self.target,
        }
    }

    /// Apply a pose immediately, clamping distance and elevation.
    pub fn set_pose(&mut self, pose: Pose) {
        self.yaw = pose.yaw;
        self.pitch = pose.pitch.clamp(-PITCH_LIMIT, PITCH_LIMIT);
        self.dist = pose.dist.clamp(*DIST_RANGE.start(), *DIST_RANGE.end());
        self.target = pose.target.map(|c| c.clamp(-2.0, 2.0));
        self.anim = None;
    }

    /// Interpolate to a pose over [`SNAP_SECONDS`]; instant under reduced
    /// motion. Yaw takes the short way round.
    pub fn glide_to(&mut self, mut pose: Pose, reduced_motion: bool) {
        let turns = ((pose.yaw - self.yaw) / std::f32::consts::TAU).round();
        pose.yaw -= turns * std::f32::consts::TAU;
        if reduced_motion {
            self.set_pose(pose);
            return;
        }
        let from = self.pose();
        self.set_pose(pose);
        let to = self.pose();
        self.anim = Some((from, to, 0.0));
    }

    /// Advance an in-flight snap. Returns true while animating, so the caller
    /// keeps requesting repaints.
    pub fn advance(&mut self, dt: f32) -> bool {
        let Some((from, to, t)) = self.anim.as_mut() else {
            return false;
        };
        *t = (*t + dt / SNAP_SECONDS).min(1.0);
        let k = egui::emath::easing::cubic_out(*t);
        let (from, to, done) = (*from, *to, *t >= 1.0);
        let mix = |a: f32, b: f32| a + (b - a) * k;
        self.yaw = mix(from.yaw, to.yaw);
        self.pitch = mix(from.pitch, to.pitch);
        self.dist = mix(from.dist, to.dist);
        self.target = std::array::from_fn(|axis| mix(from.target[axis], to.target[axis]));
        if done {
            self.anim = None;
        }
        !done
    }

    /// True while a Fit / station glide is still interpolating.
    pub fn is_animating(&self) -> bool {
        self.anim.is_some()
    }

    /// Eye position in solver-domain coordinates.
    pub fn eye(&self) -> [f32; 3] {
        let (cp, sp) = (self.pitch.cos(), self.pitch.sin());
        [
            self.target[0] + self.dist * cp * self.yaw.cos(),
            self.target[1] + self.dist * cp * self.yaw.sin(),
            self.target[2] + self.dist * sp,
        ]
    }

    /// (forward, right, up) unit vectors.
    pub fn basis(&self) -> ([f32; 3], [f32; 3], [f32; 3]) {
        let eye = self.eye();
        let fwd = norm([
            self.target[0] - eye[0],
            self.target[1] - eye[1],
            self.target[2] - eye[2],
        ]);
        let (right, up) = basis_from_forward(fwd);
        (fwd, right, up)
    }

    /// Pixels per solver unit at the target plane, matching `fs_volume`'s
    /// vertical field of view.
    fn pixels_per_unit(&self, rect: Rect) -> f32 {
        0.5 * rect.height() / (TAN_HALF_FOV * self.dist.max(1e-3))
    }

    /// Project a domain point to screen with the render camera. `None` behind
    /// the near plane. Also returns eye-space depth for fading and sorting.
    pub fn project(&self, rect: Rect, p: [f32; 3]) -> Option<(Pos2, f32)> {
        let eye = self.eye();
        let (fwd, right, up) = self.basis();
        let v = [p[0] - eye[0], p[1] - eye[1], p[2] - eye[2]];
        let z = dot(v, fwd);
        if z < 0.05 {
            return None;
        }
        let k = 0.5 * rect.height() / TAN_HALF_FOV / z;
        Some((
            Pos2::new(
                rect.center().x + dot(v, right) * k,
                rect.center().y - dot(v, up) * k,
            ),
            z,
        ))
    }

    /// World ray through a screen point (origin at the eye, unit direction) —
    /// the inverse of [`Camera::project`], used by the 3D surface probe.
    pub fn ray(&self, rect: Rect, screen: Pos2) -> ([f32; 3], [f32; 3]) {
        let (fwd, right, up) = self.basis();
        let ndc_x = (screen.x - rect.center().x) / (0.5 * rect.height()) * TAN_HALF_FOV;
        let ndc_y = (rect.center().y - screen.y) / (0.5 * rect.height()) * TAN_HALF_FOV;
        let dir = norm(std::array::from_fn(|axis| {
            fwd[axis] + right[axis] * ndc_x + up[axis] * ndc_y
        }));
        (self.eye(), dir)
    }

    pub fn orbit(&mut self, delta: Vec2, rate: f32) {
        self.anim = None;
        self.yaw -= delta.x * rate;
        self.pitch = (self.pitch + delta.y * rate).clamp(-PITCH_LIMIT, PITCH_LIMIT);
    }

    /// Drag the target across the view plane so the grabbed point tracks the
    /// cursor one-to-one at the target depth.
    pub fn pan(&mut self, delta: Vec2, rect: Rect) {
        self.anim = None;
        let (_, right, up) = self.basis();
        let scale = 1.0 / self.pixels_per_unit(rect).max(1e-6);
        for axis in 0..3 {
            self.target[axis] = (self.target[axis]
                - (right[axis] * delta.x - up[axis] * delta.y) * scale)
                .clamp(-2.0, 2.0);
        }
    }

    /// Scroll zoom. With a cursor position the point under the pointer stays
    /// put (zoom-to-cursor); `invert` honors Settings › Viewport.
    pub fn zoom(&mut self, scroll: f32, cursor: Option<Pos2>, rect: Rect, invert: bool) {
        if scroll == 0.0 {
            return;
        }
        self.anim = None;
        let signed = if invert { -scroll } else { scroll };
        let before = self.dist;
        let dist = (before * (1.0 - signed * 0.0018)).clamp(*DIST_RANGE.start(), *DIST_RANGE.end());
        self.dist = dist;
        // Zoom-to-cursor: the point under the pointer (measured on the target
        // plane) stays put, because the world offset per pixel is proportional
        // to distance — so shifting the target by the difference cancels it.
        if let Some(cursor) = cursor.filter(|c| rect.contains(*c)) {
            let (_, right, up) = self.basis();
            let offset = cursor - rect.center();
            let per_pixel = TAN_HALF_FOV * (before - dist) / (0.5 * rect.height());
            for axis in 0..3 {
                self.target[axis] = (self.target[axis]
                    + (right[axis] * offset.x - up[axis] * offset.y) * per_pixel)
                    .clamp(-2.0, 2.0);
            }
        }
    }

    /// Pose that frames `bounds` (domain coordinates) at the current
    /// orientation, with a 12% margin. Falls back to the whole solver domain
    /// when nothing is loaded.
    pub fn fit_pose(&self, bounds: Option<([f32; 3], [f32; 3])>, rect: Rect) -> Pose {
        let (lo, hi) = bounds.unwrap_or(([-1.0; 3], [1.0; 3]));
        let center = std::array::from_fn(|axis| 0.5 * (lo[axis] + hi[axis]));
        let radius = (0..3)
            .map(|axis| 0.5 * (hi[axis] - lo[axis]))
            .fold(0.0f32, |sum, half| sum + half * half)
            .sqrt()
            .max(1e-3);
        let aspect = (rect.width() / rect.height().max(1.0)).max(1e-3);
        let tan_half = TAN_HALF_FOV * aspect.min(1.0);
        Pose {
            yaw: self.yaw,
            pitch: self.pitch,
            dist: (radius * 1.12 / tan_half).clamp(*DIST_RANGE.start(), *DIST_RANGE.end()),
            target: center,
        }
    }
}

pub struct ViewOpts {
    pub opacity: f32,
    pub density_lo: f32,
    pub density_hi: f32,
    pub slice: [Option<f32>; 3], // clip plane per axis when enabled
    pub streamlines: bool,
    pub shadows: bool,
    pub mode2d: bool,
    pub gpu: bool,         // route particles through the native wgpu bloom pass
    pub volume_mode: bool, // GPU volume raymarch instead of point sprites
    pub volume: Option<gpu::VolumeData>, // the |ω| scalar field for the raymarch
    pub surface: Option<gpu::SurfaceData>, // CAD body + surface-load layer
    pub markers: Vec<Marker3D>, // billboarded critical-point annotations
    /// Settings › Viewport: multiplier on the calibrated orbit drag rate.
    pub orbit_sensitivity: f32,
    /// Settings › Viewport: scroll up moves the camera away instead of closer.
    pub invert_scroll_zoom: bool,
    /// Settings › Viewport: draw the [-1, 1]³ solver-domain wireframe.
    pub show_domain_bounds: bool,
    /// Settings › Viewport: which buttons orbit/pan.
    pub nav_scheme: NavScheme,
    /// Solid-geometry bounds in domain coordinates, for zoom-to-fit.
    pub fit_bounds: Option<([f32; 3], [f32; 3])>,
    /// Snap the camera to this station on this frame (viewport control, key,
    /// or menu item).
    pub snap_to: Option<StandardView>,
    /// Frame the geometry on this frame (F, or the fit control).
    pub fit_now: bool,
    /// Replace instrument motion with instant state changes (§3.7).
    pub reduced_motion: bool,
    /// True only on the research-sandbox screens. The analytic streamline
    /// overlay is quarantined behind this flag (see [`analytic_streamlines`]).
    pub research_sandbox: bool,
    /// Model velocity for engineering streamlines. When present and
    /// `streamlines` is on, ribbons advect this field instead of the ABC demo.
    pub model_velocity: Option<ModelVelocityField>,
    /// Decimated velocity arrows from the stored 32³ field. Empty when off.
    pub glyphs: Vec<Glyph>,
}

/// HAZARD GATE. [`streamline_polys`] advects an **analytic ABC demo field**,
/// not the model's predicted velocity, so the ribbons it draws are decoration
/// with no relationship to the case being solved. Rendering them anywhere near
/// an engineering result would violate the honesty contract (PRD §2: no fake
/// state, every rendered quantity traceable to a source), so they are hard
/// quarantined to the research sandbox and labeled there.
///
/// [`model_streamline_polys`] is the honest engineering path: it interpolates
/// the stored model velocity field and must be labeled MODEL.
pub fn analytic_streamlines(streamlines_on: bool, research_sandbox: bool) -> bool {
    streamlines_on && research_sandbox
}

/// Engineering-path gate: streamlines only when a model velocity volume is present.
pub fn model_streamlines(streamlines_on: bool, has_model_velocity: bool) -> bool {
    streamlines_on && has_model_velocity
}

/// The label the sandbox paints whenever the analytic overlay is live.
pub const ANALYTIC_STREAMLINE_LABEL: &str =
    "ANALYTIC DEMO FIELD · not model velocity · research sandbox only";

/// Label for model-field streamlines in the engineering viewport.
pub const MODEL_STREAMLINE_LABEL: &str = "MODEL · streamlines from predicted velocity";

/// Dense velocity volume for engineering streamlines: `vel` is `[3,N,N,N]` in
/// domain coordinates, same layout as `engine::CadField.vel`
/// (`((c*N+i)*N+j)*N+k`). Optional `mask` is the stored solid occupancy on the
/// same `(i,j,k)` grid — used to terminate ribbons at the body so Brinkman
/// leakage cannot draw air through the solid.
#[derive(Clone)]
pub struct ModelVelocityField {
    pub n: usize,
    pub vel: std::sync::Arc<[f32]>,
    pub mask: Option<std::sync::Arc<[f32]>>,
}

/// A billboarded critical-point annotation: fixed screen-size ring + value chip
/// at a 3D domain position (the 3D counterpart of the 2D Field Insights).
#[derive(Clone)]
pub struct Marker3D {
    pub pos: [f32; 3], // [-1,1]³ domain coords
    pub color: Color32,
    pub text: String, // "glyph value", mono
}

/// Must match `fs_volume`'s `tanH` (gpu.rs) so CPU-projected annotations land on
/// the raymarched image.
pub const TAN_HALF_FOV: f32 = 0.55;

/// Screen-space instrument marker: ring + dot at `center`, 1px leader to a
/// collision-avoiding mono value chip clamped inside `bounds`. Shared by the 2D
/// Field Insights and the 3D billboards — one visual vocabulary.
pub fn insight_marker(
    p: &egui::Painter,
    bounds: Rect,
    center: Pos2,
    col: Color32,
    text: &str,
    chips: &mut Vec<Rect>,
) {
    p.circle_stroke(center, 6.0, Stroke::new(1.5, col));
    p.circle_filled(center, 1.6, col);

    let galley = p.layout_no_wrap(text.to_owned(), egui::FontId::monospace(10.5), TEXT);
    let size = galley.size() + egui::Vec2::new(14.0, 9.0);
    let mut anchor = Pos2::new(center.x + 12.0, center.y - 22.0);
    if anchor.x + size.x > bounds.max.x - 2.0 {
        anchor.x = center.x - 12.0 - size.x;
    }
    if anchor.y < bounds.min.y + 2.0 {
        anchor.y = center.y + 12.0;
    }
    let mut chip = Rect::from_min_size(anchor, size);
    for _ in 0..8 {
        if !chips.iter().any(|c| c.intersects(chip.expand(2.0))) {
            break;
        }
        chip = chip.translate(egui::Vec2::new(0.0, size.y + 3.0));
    }
    chip = Rect::from_min_size(
        Pos2::new(
            chip.min.x.clamp(bounds.min.x, bounds.max.x - size.x),
            chip.min.y.clamp(bounds.min.y, bounds.max.y - size.y),
        ),
        size,
    );

    let to = if chip.center().x >= center.x {
        Pos2::new(chip.min.x, chip.center().y)
    } else {
        Pos2::new(chip.max.x, chip.center().y)
    };
    let dir = (to - center).normalized();
    p.line_segment(
        [center + dir * 6.0, to],
        Stroke::new(1.0, col.gamma_multiply(0.7)),
    );

    p.rect_filled(chip, egui::CornerRadius::same(2), SURFACE);
    p.rect_stroke(
        chip,
        egui::CornerRadius::same(2),
        Stroke::new(1.0, col),
        egui::StrokeKind::Inside,
    );
    p.galley(chip.min + egui::Vec2::new(7.0, 4.5), galley, TEXT);
    chips.push(chip);
}

/// Draw the billboarded critical-point markers with a mode-appropriate
/// projection; respects the same slice-plane clipping as the renderers.
fn draw_markers(
    ui: &egui::Ui,
    rect: Rect,
    cam: &Camera,
    opts: &ViewOpts,
    project_pt: Option<&dyn Fn([f32; 3]) -> (Pos2, f32)>,
) {
    if opts.markers.is_empty() {
        return;
    }
    let p = ui.painter_at(rect);
    let mut chips: Vec<Rect> = Vec::new();
    'markers: for m in &opts.markers {
        for a in 0..3 {
            if let Some(pos) = opts.slice[a] {
                if m.pos[a] < pos * 2.0 - 1.0 {
                    continue 'markers;
                }
            }
        }
        let screen = match project_pt {
            Some(f) => {
                let (s, depth) = f(m.pos);
                if !opts.mode2d && depth < 0.2 {
                    continue;
                }
                s
            }
            None => match cam.project(rect, m.pos) {
                Some((s, _)) => s,
                None => continue,
            },
        };
        if !rect.contains(screen) {
            continue;
        }
        insight_marker(&p, rect, screen, m.color, &m.text, &mut chips);
    }
}

/// Screen-space axis triad in the lower-left of the viewport well.
///
/// World axes are +X (free stream / ember), +Y (side / blue), +Z (up / green),
/// matching the solver frame and the View menu station labels. Drawn as a
/// compact instrument overlay so it shares the camera-chip vocabulary rather
/// than introducing a second chrome language.
pub fn draw_axis_triad(painter: &egui::Painter, rect: Rect, cam: &Camera) {
    const SIZE: f32 = 54.0;
    const MARGIN: f32 = 18.0;
    let origin = Pos2::new(
        rect.min.x + MARGIN + SIZE * 0.5,
        rect.max.y - MARGIN - SIZE * 0.5,
    );
    if !rect.contains(origin) {
        return;
    }
    let pad = Rect::from_center_size(origin, Vec2::splat(SIZE + 16.0));
    painter.rect_filled(
        pad,
        egui::CornerRadius::same(3),
        Color32::from_rgba_unmultiplied(28, 22, 18, 210),
    );
    painter.rect_stroke(
        pad,
        egui::CornerRadius::same(3),
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(90, 72, 60, 180)),
        egui::StrokeKind::Inside,
    );

    let (_, right, up) = cam.basis();
    // Project world axes into the view plane (ignore depth — this is a compass,
    // not a perspective gizmo). Flip Y because screen Y grows downward.
    let project_axis = |world: [f32; 3]| -> Vec2 {
        let x = world[0] * right[0] + world[1] * right[1] + world[2] * right[2];
        let y = world[0] * up[0] + world[1] * up[1] + world[2] * up[2];
        Vec2::new(x, -y)
    };
    let axes = [
        ([1.0, 0.0, 0.0], Color32::from_rgb(214, 98, 42), "X"), // stream
        ([0.0, 1.0, 0.0], Color32::from_rgb(72, 132, 188), "Y"),
        ([0.0, 0.0, 1.0], Color32::from_rgb(72, 148, 96), "Z"),
    ];
    let tip_len = SIZE * 0.42;
    // Draw shorter axes first so the longest (most foreshortened last) sits on top.
    let mut order: Vec<(usize, f32)> = axes
        .iter()
        .enumerate()
        .map(|(i, (w, _, _))| (i, project_axis(*w).length()))
        .collect();
    order.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    for (i, _) in order {
        let (world, color, label) = axes[i];
        let dir = project_axis(world);
        let len = dir.length().max(1e-4);
        let tip = origin + dir * (tip_len / len);
        painter.line_segment([origin, tip], Stroke::new(2.0, color));
        painter.circle_filled(tip, 2.2, color);
        painter.text(
            tip + dir * (8.0 / len),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::monospace(10.0),
            color,
        );
    }
    painter.circle_filled(origin, 2.5, Color32::from_rgb(230, 214, 200));
}

/// blue (neg) -> dark -> ember -> gold (pos), returned as linear-ish rgb 0..1.
fn colormap_rgb(vort: f32) -> [f32; 3] {
    let t = vort.clamp(-1.0, 1.0);
    if t < 0.0 {
        let k = -t;
        [
            0.10 + 0.44 * (1.0 - k),
            0.40 + 0.4 * (1.0 - k),
            0.55 + 0.45 * k,
        ]
    } else {
        [0.55 + 0.42 * t, 0.28 + 0.18 * t, 0.05 + 0.02 * t]
    }
}

/// A projected particle carrying everything both the GPU and CPU paths need.
struct Proj {
    screen: Pos2,
    ndc: [f32; 2],
    depth: f32,
    r_pts: f32,
    base: [f32; 3],
    weight: f32,
    gain: f32,
}

/// What this frame's pointer activity means to the caller.
#[derive(Clone, Copy, Debug, Default)]
pub struct Interaction {
    /// A completed click that did not orbit or pan: the 3D probe pick point.
    pub picked: Option<Pos2>,
}

pub fn show(
    ui: &mut egui::Ui,
    rect: Rect,
    cam: &mut Camera,
    opts: &ViewOpts,
    particles: &[Particle],
) -> Interaction {
    let resp = ui.interact(rect, ui.id().with("viewport3d"), Sense::click_and_drag());
    // A click is a probe pick only when it stayed put; a drag is navigation.
    let interaction = Interaction {
        picked: resp
            .clicked()
            .then(|| resp.interact_pointer_pos())
            .flatten()
            .filter(|_| !opts.mode2d),
    };
    // Standard views and zoom-to-fit arrive as one-frame requests from the
    // viewport control strip, the F key, or the View menu.
    if let Some(view) = opts.snap_to {
        let (yaw, pitch) = view.angles();
        let pose = Pose {
            yaw,
            pitch,
            ..cam.pose()
        };
        cam.glide_to(pose, opts.reduced_motion);
    }
    if opts.fit_now {
        let pose = cam.fit_pose(opts.fit_bounds, rect);
        cam.glide_to(pose, opts.reduced_motion);
    }
    if cam.advance(ui.input(|i| i.stable_dt).min(0.05)) {
        ui.ctx().request_repaint();
    }
    let orbit_rate = 0.008 * opts.orbit_sensitivity.clamp(0.4, 2.0);
    if resp.dragged() && !opts.mode2d {
        let (primary, middle, shift, ctrl) = ui.input(|i| {
            (
                i.pointer.button_down(egui::PointerButton::Primary),
                i.pointer.button_down(egui::PointerButton::Middle),
                i.modifiers.shift,
                i.modifiers.command || i.modifiers.ctrl,
            )
        });
        match gesture_for(opts.nav_scheme, primary, middle, shift, ctrl) {
            Gesture::Orbit => cam.orbit(resp.drag_delta(), orbit_rate),
            Gesture::Pan => cam.pan(resp.drag_delta(), rect),
            Gesture::None => {}
        }
    }
    if resp.hovered() && !opts.mode2d {
        let (scroll, cursor) = ui.input(|i| (i.smooth_scroll_delta.y, i.pointer.hover_pos()));
        cam.zoom(scroll, cursor, rect, opts.invert_scroll_zoom);
    } else if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        cam.zoom(scroll, None, rect, opts.invert_scroll_zoom);
    }

    // Volume raymarch mode: body/surface (+ optional volume) on the GPU, then
    // composite honest model streamlines and markers on top. Early-returning
    // here used to drop engineering streamlines entirely on Results.
    if opts.gpu && opts.volume_mode && !opts.mode2d {
        let eye = cam.eye();
        let slice_c = [
            opts.slice[0].map(|p| p * 2.0 - 1.0).unwrap_or(-2.0),
            opts.slice[1].map(|p| p * 2.0 - 1.0).unwrap_or(-2.0),
            opts.slice[2].map(|p| p * 2.0 - 1.0).unwrap_or(-2.0),
        ];
        let use_model = model_streamlines(
            opts.streamlines,
            opts.model_velocity.is_some() && !opts.research_sandbox,
        );
        let ppp = ui.ctx().pixels_per_point();
        let project = |v: [f32; 3]| -> (Pos2, f32) {
            match cam.project(rect, v) {
                Some((screen, depth)) => (screen, depth),
                None => (Pos2::new(f32::MAX, f32::MAX), -1.0),
            }
        };
        let mut segments = if use_model {
            if let Some(field) = opts.model_velocity.as_ref() {
                model_streamline_segments(&project, field, opts.fit_bounds, eye, rect, ppp)
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        segments.extend(glyph_segments(&project, &opts.glyphs, rect, ppp));
        gpu::add_volume(
            ui,
            rect,
            eye,
            cam.target,
            TAN_HALF_FOV,
            opts.density_lo,
            opts.density_hi,
            slice_c,
            opts.shadows,
            opts.volume.clone(),
            opts.surface.clone(),
            segments,
        );
        if use_model {
            let p = ui.painter_at(rect);
            p.text(
                rect.left_top() + egui::vec2(16.0, rect.height() - 44.0),
                egui::Align2::LEFT_TOP,
                MODEL_STREAMLINE_LABEL,
                mono_s().resolve(ui.style()),
                GOLD,
            );
        }
        if !opts.glyphs.is_empty() {
            let p = ui.painter_at(rect);
            p.text(
                rect.left_top() + egui::vec2(16.0, rect.height() - 28.0),
                egui::Align2::LEFT_TOP,
                "MODEL · velocity glyphs (every 2nd fluid cell)",
                mono_s().resolve(ui.style()),
                GOLD,
            );
        }
        draw_markers(ui, rect, cam, opts, None); // billboards over the raymarch
        return interaction;
    }

    let p = ui.painter_at(rect);
    let center = rect.center();
    let scale = rect.height().min(rect.width()) * 0.44;
    let zoom = Camera::default().dist / cam.dist;
    let mode2d = opts.mode2d;
    // One projector for particles, streamlines, the domain box, and the
    // billboards — identical to the raymarch camera, so switching layers never
    // shifts the picture. Behind the near plane the point is pushed far
    // off-screen instead of dropped, keeping the closure infallible.
    let project = |v: [f32; 3]| -> (Pos2, f32) {
        if mode2d {
            return (
                Pos2::new(
                    center.x + v[0] * scale * zoom,
                    center.y - v[1] * scale * zoom,
                ),
                1.0 - v[2],
            );
        }
        match cam.project(rect, v) {
            Some((screen, depth)) => (screen, depth),
            None => (Pos2::new(f32::MAX, f32::MAX), -1.0),
        }
    };

    // domain bounding box (3D only; Settings › Viewport can hide it)
    if !mode2d && opts.show_domain_bounds {
        let cs = [
            [-1., -1., -1.],
            [1., -1., -1.],
            [1., 1., -1.],
            [-1., 1., -1.],
            [-1., -1., 1.],
            [1., -1., 1.],
            [1., 1., 1.],
            [-1., 1., 1.],
        ];
        let edges = [
            (0, 1),
            (1, 2),
            (2, 3),
            (3, 0),
            (4, 5),
            (5, 6),
            (6, 7),
            (7, 4),
            (0, 4),
            (1, 5),
            (2, 6),
            (3, 7),
        ];
        let box_stroke = Stroke::new(1.0, OUTLINE_VARIANT.gamma_multiply(0.9));
        for (a, b) in edges {
            p.line_segment([project(cs[a]).0, project(cs[b]).0], box_stroke);
        }
    }

    // project + cull + slice-clip every particle once
    let thr = (opts.density_lo - 0.5).max(0.0); // density -> |vort| threshold
    let mut proj: Vec<Proj> = Vec::with_capacity(particles.len());
    for pt in particles {
        if pt.vort.abs() < thr {
            continue;
        }
        let mut clipped = false;
        for a in 0..3 {
            if let Some(pos) = opts.slice[a] {
                if pt.pos[a] < pos * 2.0 - 1.0 {
                    clipped = true;
                    break;
                }
            }
        }
        if clipped {
            continue;
        }
        let (s, depth) = project(pt.pos);
        if !rect.expand(60.0).contains(s) {
            continue;
        }
        let fade = if mode2d {
            1.0
        } else {
            (2.2 / depth).clamp(0.15, 1.0)
        };
        let shadow = if opts.shadows && !mode2d {
            0.45 + 0.55 * fade
        } else {
            1.0
        };
        let weight = opts.opacity * fade * (0.35 + 0.65 * pt.speed) * shadow;
        let r_pts = if mode2d {
            (3.0 * zoom).clamp(1.5, 4.0)
        } else {
            (fade * 3.0).clamp(1.2, 3.4)
        };
        // HDR gain: push the fast, high-vorticity cores above 1.0 so they bloom
        let hot = (pt.speed * (0.45 + 0.55 * pt.vort.abs())).clamp(0.0, 1.4);
        let gain = 0.75 + 2.4 * hot;
        let ndc = [
            (s.x - rect.min.x) / rect.width() * 2.0 - 1.0,
            1.0 - (s.y - rect.min.y) / rect.height() * 2.0,
        ];
        proj.push(Proj {
            screen: s,
            ndc,
            depth,
            r_pts,
            base: colormap_rgb(pt.vort),
            weight,
            gain,
        });
    }

    let use_model = model_streamlines(
        opts.streamlines,
        opts.model_velocity.is_some() && !opts.research_sandbox,
    );
    let use_analytic = analytic_streamlines(opts.streamlines, opts.research_sandbox);
    if opts.gpu {
        let ppp = ui.ctx().pixels_per_point();
        let instances: Vec<GpuInstance> = proj
            .iter()
            .map(|q| GpuInstance {
                pos: q.ndc,
                radius_px: q.r_pts * ppp,
                weight: q.weight,
                color: [
                    q.base[0] * q.gain,
                    q.base[1] * q.gain,
                    q.base[2] * q.gain,
                    1.0,
                ],
            })
            .collect();
        let segments = if use_analytic {
            streamline_segments(&project, particles, rect, ppp)
        } else if use_model {
            if let Some(field) = opts.model_velocity.as_ref() {
                model_streamline_segments(&project, field, opts.fit_bounds, cam.eye(), rect, ppp)
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        let mut segments = segments;
        segments.extend(glyph_segments(&project, &opts.glyphs, rect, ppp));
        gpu::add_flow(ui, rect, instances, segments);
    } else {
        // CPU fallback: depth-sorted faint halo + bright core (soft glow, no GPU)
        proj.sort_by(|a, b| {
            b.depth
                .partial_cmp(&a.depth)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for q in &proj {
            let col = Color32::from_rgba_unmultiplied(
                (q.base[0] * 255.0) as u8,
                (q.base[1] * 255.0) as u8,
                (q.base[2] * 255.0) as u8,
                (q.weight.clamp(0.0, 1.0) * 255.0) as u8,
            );
            let halo = Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), col.a() / 3);
            p.circle_filled(q.screen, q.r_pts * 2.1, halo);
            p.circle_filled(q.screen, q.r_pts, col);
        }
        if use_analytic {
            for poly in streamline_polys(&project, particles) {
                p.line(poly, Stroke::new(1.0, GOLD.gamma_multiply(0.5)));
            }
        } else if use_model {
            if let Some(field) = opts.model_velocity.as_ref() {
                for (poly, _) in model_streamline_polys(&project, field, opts.fit_bounds) {
                    p.line(poly, Stroke::new(1.2, BRAND.gamma_multiply(0.75)));
                }
            }
        }
    }

    if use_analytic {
        p.text(
            rect.left_top() + egui::vec2(16.0, rect.height() - 44.0),
            egui::Align2::LEFT_TOP,
            ANALYTIC_STREAMLINE_LABEL,
            mono_s().resolve(ui.style()),
            WARN,
        );
    } else if use_model {
        p.text(
            rect.left_top() + egui::vec2(16.0, rect.height() - 44.0),
            egui::Align2::LEFT_TOP,
            MODEL_STREAMLINE_LABEL,
            mono_s().resolve(ui.style()),
            GOLD,
        );
    }

    // billboarded critical points, projected with this mode's own camera
    draw_markers(ui, rect, cam, opts, Some(&project));
    interaction
}

fn sample_model_velocity(field: &ModelVelocityField, pos: [f32; 3]) -> [f32; 3] {
    let n = field.n.max(2);
    let to_index =
        |coordinate: f32| ((coordinate + 1.0) * 0.5 * (n - 1) as f32).clamp(0.0, (n - 1) as f32);
    let ix = to_index(pos[0]) as usize;
    let iy = to_index(pos[1]) as usize;
    let iz = to_index(pos[2]) as usize;
    // CadField / flow.rs layout: ((c * N + i) * N + j) * N + k
    let at = |component: usize, x: usize, y: usize, z: usize| {
        field
            .vel
            .get(((component * n + x) * n + y) * n + z)
            .copied()
            .unwrap_or(0.0)
    };
    [at(0, ix, iy, iz), at(1, ix, iy, iz), at(2, ix, iy, iz)]
}

/// Solid occupancy in `[0,1]` at a domain point. Missing mask → always fluid.
fn sample_model_mask(field: &ModelVelocityField, pos: [f32; 3]) -> f32 {
    let Some(mask) = field.mask.as_ref() else {
        return 0.0;
    };
    let n = field.n.max(2);
    let to_index =
        |coordinate: f32| ((coordinate + 1.0) * 0.5 * (n - 1) as f32).clamp(0.0, (n - 1) as f32);
    let ix = to_index(pos[0]) as usize;
    let iy = to_index(pos[1]) as usize;
    let iz = to_index(pos[2]) as usize;
    mask.get((ix * n + iy) * n + iz).copied().unwrap_or(0.0)
}

/// Display-solid threshold: matches the Cp surface shell floor so streamlines
/// stop where the raymarch draws the body, not deeper in the Brinkman core.
const STREAMLINE_SOLID: f32 = 0.20;

/// Upstream rake sized from the body AABB when available; otherwise a fixed
/// free-stream half-domain rake. Seeds sit slightly ahead of the body so tubes
/// wrap the solid rather than starting inside it.
fn model_streamline_seeds(fit_bounds: Option<([f32; 3], [f32; 3])>) -> Vec<[f32; 3]> {
    let (x0, y0, y1, z0, z1) = if let Some((lo, hi)) = fit_bounds {
        let cy = 0.5 * (lo[1] + hi[1]);
        let cz = 0.5 * (lo[2] + hi[2]);
        // Slightly oversized frontal rake so edge streamlines clear the sides.
        let hy = ((hi[1] - lo[1]) * 0.95).max(0.28);
        let hz = ((hi[2] - lo[2]) * 0.95).max(0.28);
        let x = (lo[0] - 0.32).clamp(-0.95, -0.30);
        (x, cy - hy, cy + hy, cz - hz, cz + hz)
    } else {
        (-0.85, -0.70, 0.70, -0.70, 0.70)
    };
    let mut seeds = Vec::with_capacity(64);
    for j in 0..8 {
        for k in 0..8 {
            let ty = j as f32 / 7.0;
            let tz = k as f32 / 7.0;
            seeds.push([x0, y0 + (y1 - y0) * ty, z0 + (z1 - z0) * tz]);
        }
    }
    seeds
}

fn speed_rgb(speed: f32) -> [f32; 3] {
    // Ember sequential on nondimensional |u|/V∞ (≈1 freestream).
    let t = (speed * 0.55).clamp(0.0, 1.0);
    [0.28 + 0.62 * t, 0.14 + 0.32 * t, 0.05 + 0.10 * t]
}

/// True when the camera ray from `eye` to `point` hits the solid before the
/// sample — used so screen-space tubes do not paint through the Cp body.
fn occluded_by_solid(field: &ModelVelocityField, eye: [f32; 3], point: [f32; 3]) -> bool {
    if field.mask.is_none() {
        return false;
    }
    let delta = [point[0] - eye[0], point[1] - eye[1], point[2] - eye[2]];
    let dist = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2])
        .sqrt()
        .max(1e-4);
    let steps = ((dist / 0.06).ceil() as usize).clamp(8, 28);
    for i in 1..steps {
        let t = i as f32 / steps as f32 * 0.92;
        let p = [
            eye[0] + delta[0] * t,
            eye[1] + delta[1] * t,
            eye[2] + delta[2] * t,
        ];
        if sample_model_mask(field, p) >= STREAMLINE_SOLID {
            return true;
        }
    }
    false
}

/// Integrate model velocity with RK2 arc-length steps in domain coordinates.
/// Ribbons terminate on the solid mask so Brinkman leakage cannot draw
/// through-body flow.
fn model_streamline_traces(
    field: &ModelVelocityField,
    fit_bounds: Option<([f32; 3], [f32; 3])>,
) -> Vec<(Vec<[f32; 3]>, Vec<f32>)> {
    let seeds = model_streamline_seeds(fit_bounds);
    let h = 0.032;
    let steps = 48usize;
    let mut traces = Vec::with_capacity(seeds.len());
    for seed in seeds {
        if sample_model_mask(field, seed) >= STREAMLINE_SOLID {
            continue;
        }
        let mut pos = seed;
        let mut trace = Vec::with_capacity(steps);
        let mut speeds = Vec::with_capacity(steps);
        for _ in 0..steps {
            if sample_model_mask(field, pos) >= STREAMLINE_SOLID {
                break;
            }
            let v0 = sample_model_velocity(field, pos);
            let sp0 = (v0[0] * v0[0] + v0[1] * v0[1] + v0[2] * v0[2])
                .sqrt()
                .max(1e-4);
            if sp0 < 0.04 {
                trace.push(pos);
                speeds.push(sp0);
                break;
            }
            trace.push(pos);
            speeds.push(sp0);
            let mut mid = [0.0f32; 3];
            for a in 0..3 {
                mid[a] = (pos[a] + v0[a] / sp0 * (h * 0.5)).clamp(-1.0, 1.0);
            }
            if sample_model_mask(field, mid) >= STREAMLINE_SOLID {
                break;
            }
            let v1 = sample_model_velocity(field, mid);
            let sp1 = (v1[0] * v1[0] + v1[1] * v1[1] + v1[2] * v1[2])
                .sqrt()
                .max(1e-4);
            let mut next = [0.0f32; 3];
            for a in 0..3 {
                next[a] = (pos[a] + v1[a] / sp1 * h).clamp(-1.0, 1.0);
            }
            if sample_model_mask(field, next) >= STREAMLINE_SOLID {
                break;
            }
            pos = next;
        }
        if trace.len() >= 2 {
            traces.push((trace, speeds));
        }
    }
    traces
}

fn model_streamline_polys(
    project: &impl Fn([f32; 3]) -> (Pos2, f32),
    field: &ModelVelocityField,
    fit_bounds: Option<([f32; 3], [f32; 3])>,
) -> Vec<(Vec<Pos2>, Vec<f32>)> {
    model_streamline_traces(field, fit_bounds)
        .into_iter()
        .map(|(trace, speeds)| (trace.into_iter().map(|p| project(p).0).collect(), speeds))
        .collect()
}

fn model_streamline_segments(
    project: &impl Fn([f32; 3]) -> (Pos2, f32),
    field: &ModelVelocityField,
    fit_bounds: Option<([f32; 3], [f32; 3])>,
    eye: [f32; 3],
    rect: Rect,
    ppp: f32,
) -> Vec<SegInstance> {
    let to_ndc = |s: Pos2| {
        [
            (s.x - rect.min.x) / rect.width() * 2.0 - 1.0,
            1.0 - (s.y - rect.min.y) / rect.height() * 2.0,
        ]
    };
    let mut segments = Vec::new();
    for (trace, speeds) in model_streamline_traces(field, fit_bounds) {
        for i in 0..trace.len().saturating_sub(1) {
            let mid = [
                0.5 * (trace[i][0] + trace[i + 1][0]),
                0.5 * (trace[i][1] + trace[i + 1][1]),
                0.5 * (trace[i][2] + trace[i + 1][2]),
            ];
            if occluded_by_solid(field, eye, mid) {
                continue;
            }
            let (s0, _) = project(trace[i]);
            let (s1, _) = project(trace[i + 1]);
            if s0.x > 1.0e20 || s1.x > 1.0e20 {
                continue;
            }
            let rgb = speed_rgb(0.5 * (speeds[i] + speeds[i + 1]));
            segments.push(SegInstance {
                p0: to_ndc(s0),
                p1: to_ndc(s1),
                width_px: 3.0 * ppp,
                _pad: 0.0,
                color: [rgb[0], rgb[1], rgb[2], 1.0],
            });
        }
    }
    segments
}

/// Project a few streamlines to screen polylines, seeded from strong-vorticity
/// particles and advected through a **closed-form ABC field** — deliberately
/// not the model's velocity. Callers must gate on [`analytic_streamlines`].
fn streamline_polys(
    project: &impl Fn([f32; 3]) -> (Pos2, f32),
    particles: &[Particle],
) -> Vec<Vec<Pos2>> {
    let seeds: Vec<[f32; 3]> = particles
        .iter()
        .filter(|q| q.vort.abs() > 0.6)
        .take(40)
        .map(|q| q.pos)
        .collect();
    let mut polys = Vec::with_capacity(seeds.len());
    for s in seeds {
        let mut pos = s;
        let mut poly = Vec::with_capacity(24);
        for _ in 0..24 {
            poly.push(project(pos).0);
            let (x, y, z) = (
                pos[0] * std::f32::consts::PI,
                pos[1] * std::f32::consts::PI,
                pos[2] * std::f32::consts::PI,
            );
            let v = [
                (z.sin() + y.cos()),
                (x.sin() + z.cos()),
                (y.sin() + x.cos()),
            ];
            for k in 0..3 {
                pos[k] = (pos[k] + v[k] * 0.02).clamp(-1.0, 1.0);
            }
        }
        polys.push(poly);
    }
    polys
}

/// The same streamlines as GPU ribbon segments (NDC endpoints, HDR gold so they
/// bloom into glowing tubes).
fn streamline_segments(
    project: &impl Fn([f32; 3]) -> (Pos2, f32),
    particles: &[Particle],
    rect: Rect,
    ppp: f32,
) -> Vec<SegInstance> {
    let to_ndc = |s: Pos2| {
        [
            (s.x - rect.min.x) / rect.width() * 2.0 - 1.0,
            1.0 - (s.y - rect.min.y) / rect.height() * 2.0,
        ]
    };
    let mut segs = Vec::new();
    for poly in streamline_polys(project, particles) {
        for w in poly.windows(2) {
            segs.push(SegInstance {
                p0: to_ndc(w[0]),
                p1: to_ndc(w[1]),
                width_px: 1.6 * ppp,
                _pad: 0.0,
                color: [1.5, 1.0, 0.35, 1.0], // gold, HDR
            });
        }
    }
    segs
}

fn glyph_segments(
    project: &impl Fn([f32; 3]) -> (Pos2, f32),
    glyphs: &[Glyph],
    rect: Rect,
    ppp: f32,
) -> Vec<SegInstance> {
    let to_ndc = |s: Pos2| {
        [
            (s.x - rect.min.x) / rect.width() * 2.0 - 1.0,
            1.0 - (s.y - rect.min.y) / rect.height() * 2.0,
        ]
    };
    let max_speed = glyphs
        .iter()
        .map(|glyph| glyph.speed)
        .fold(1e-6f32, f32::max);
    let mut segs = Vec::with_capacity(glyphs.len());
    for glyph in glyphs {
        let length = cad_postpro::GLYPH_LENGTH * (glyph.speed / max_speed).clamp(0.15, 1.5);
        let tip = [
            glyph.pos[0] + glyph.dir[0] * length,
            glyph.pos[1] + glyph.dir[1] * length,
            glyph.pos[2] + glyph.dir[2] * length,
        ];
        let (s0, _) = project(glyph.pos);
        let (s1, _) = project(tip);
        if s0.x > 1.0e10 || s1.x > 1.0e10 {
            continue;
        }
        let t = (glyph.speed / max_speed).clamp(0.0, 1.0);
        segs.push(SegInstance {
            p0: to_ndc(s0),
            p1: to_ndc(s1),
            width_px: 1.35 * ppp,
            _pad: 0.0,
            color: [0.35 + 1.1 * t, 0.55 + 0.35 * t, 0.95 - 0.4 * t, 1.0],
        });
    }
    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect::from_min_size(Pos2::new(0.0, 0.0), egui::vec2(1600.0, 900.0))
    }

    #[test]
    fn default_camera_is_the_iso_station() {
        let cam = Camera::default();
        let (yaw, pitch) = StandardView::Iso.angles();
        assert_eq!((cam.yaw, cam.pitch), (yaw, pitch));
        assert_eq!(cam.target, [0.0; 3]);
        // Upstream of the body, above it, and off to −Y: the three-quarter view.
        let eye = cam.eye();
        assert!(eye[0] < 0.0 && eye[1] < 0.0 && eye[2] > 0.0);
    }

    #[test]
    fn standard_views_look_along_their_named_axis() {
        let cases = [
            (StandardView::Upstream, 0usize, -1.0f32),
            (StandardView::Downstream, 0, 1.0),
            (StandardView::SideRight, 1, 1.0),
            (StandardView::SideLeft, 1, -1.0),
            (StandardView::Top, 2, 1.0),
            (StandardView::Bottom, 2, -1.0),
        ];
        for (view, axis, sign) in cases {
            let mut cam = Camera::default();
            let (yaw, pitch) = view.angles();
            cam.set_pose(Pose {
                yaw,
                pitch,
                ..cam.pose()
            });
            let eye = cam.eye();
            // The eye sits on the named side of the target…
            assert!(
                eye[axis] * sign > 0.9 * cam.dist,
                "{} eye {eye:?}",
                view.label()
            );
            // …and the vertical axis is up on screen for every non-polar view.
            let (_, _, up) = cam.basis();
            if !matches!(view, StandardView::Top | StandardView::Bottom) {
                assert!(up[2] > 0.99, "{} up {up:?}", view.label());
            }
        }
    }

    #[test]
    fn side_view_puts_the_stream_left_to_right() {
        let mut cam = Camera::default();
        let (yaw, pitch) = StandardView::SideLeft.angles();
        cam.set_pose(Pose {
            yaw,
            pitch,
            ..cam.pose()
        });
        let (_, right, _) = cam.basis();
        assert!(
            right[0] > 0.99,
            "expected +X to screen right, got {right:?}"
        );
        let nose = cam.project(rect(), [0.5, 0.0, 0.0]).expect("in front");
        let tail = cam.project(rect(), [-0.5, 0.0, 0.0]).expect("in front");
        assert!(nose.0.x > tail.0.x);
    }

    #[test]
    fn project_and_ray_are_inverses() {
        let cam = Camera::default();
        let rect = rect();
        for point in [[0.0; 3], [0.4, -0.2, 0.6], [-0.7, 0.5, -0.3]] {
            let (screen, _) = cam.project(rect, point).expect("visible");
            let (origin, dir) = cam.ray(rect, screen);
            // The picked ray must pass through the projected point.
            let to_point: [f32; 3] = std::array::from_fn(|a| point[a] - origin[a]);
            let along = dot(to_point, dir);
            let residual: [f32; 3] = std::array::from_fn(|a| to_point[a] - dir[a] * along);
            let error =
                (residual[0] * residual[0] + residual[1] * residual[1] + residual[2] * residual[2])
                    .sqrt();
            assert!(error < 1e-3, "ray missed {point:?} by {error}");
        }
    }

    #[test]
    fn fit_frames_the_bounds_and_centers_them() {
        let cam = Camera::default();
        let rect = rect();
        let bounds = ([0.1, -0.2, -0.05], [0.5, 0.2, 0.35]);
        let pose = cam.fit_pose(Some(bounds), rect);
        assert_eq!(pose.yaw, cam.yaw, "fit preserves orientation");
        let centre = [0.3, 0.0, 0.15];
        for axis in 0..3 {
            assert!(
                (pose.target[axis] - centre[axis]).abs() < 1e-5,
                "{:?} is not centred on {centre:?}",
                pose.target
            );
        }
        let mut fitted = Camera::default();
        fitted.set_pose(pose);
        // Every corner of the box lands inside the viewport…
        for corner in 0..8 {
            let point = [
                if corner & 1 == 0 {
                    bounds.0[0]
                } else {
                    bounds.1[0]
                },
                if corner & 2 == 0 {
                    bounds.0[1]
                } else {
                    bounds.1[1]
                },
                if corner & 4 == 0 {
                    bounds.0[2]
                } else {
                    bounds.1[2]
                },
            ];
            let (screen, _) = fitted.project(rect, point).expect("visible");
            assert!(rect.contains(screen), "corner {point:?} at {screen:?}");
        }
        // …and the framing is tight: a body this size must not be left tiny.
        let (near, _) = fitted.project(rect, bounds.0).expect("visible");
        let (far, _) = fitted.project(rect, bounds.1).expect("visible");
        assert!((near - far).length() > 0.25 * rect.height());
        // An empty scene falls back to the whole solver domain, not zero.
        let domain = cam.fit_pose(None, rect);
        assert_eq!(domain.target, [0.0; 3]);
        assert!(domain.dist > 2.0);
    }

    #[test]
    fn zoom_to_cursor_keeps_the_pointed_point_under_the_cursor() {
        let rect = rect();
        let mut cam = Camera::default();
        let cursor = Pos2::new(rect.center().x + 220.0, rect.center().y - 130.0);
        // Whatever sits under the cursor on the target plane…
        let (origin, dir) = cam.ray(rect, cursor);
        let point: [f32; 3] = std::array::from_fn(|a| origin[a] + dir[a] * cam.dist);
        cam.zoom(120.0, Some(cursor), rect, false);
        assert!(cam.dist < 4.4, "scroll up must move closer by default");
        // …stays under the cursor afterwards.
        let (moved, _) = cam.project(rect, point).expect("visible");
        assert!((moved - cursor).length() < 2.0, "{moved:?} vs {cursor:?}");
    }

    #[test]
    fn invert_scroll_zoom_reverses_the_direction_and_clamps() {
        let rect = rect();
        let mut cam = Camera::default();
        cam.zoom(120.0, None, rect, true);
        assert!(cam.dist > 4.4);
        for _ in 0..200 {
            cam.zoom(600.0, None, rect, false);
        }
        assert_eq!(cam.dist, *DIST_RANGE.start());
        for _ in 0..200 {
            cam.zoom(600.0, None, rect, true);
        }
        assert_eq!(cam.dist, *DIST_RANGE.end());
    }

    #[test]
    fn pan_tracks_the_cursor_one_to_one_at_the_target_plane() {
        let rect = rect();
        let mut cam = Camera::default();
        let before = cam.project(rect, cam.target).expect("target visible").0;
        cam.pan(egui::vec2(60.0, -25.0), rect);
        let after = cam.project(rect, [0.0; 3]).expect("origin visible").0;
        assert!((after.x - (before.x + 60.0)).abs() < 1.0);
        assert!((after.y - (before.y - 25.0)).abs() < 1.0);
    }

    #[test]
    fn orbit_respects_the_pitch_limit_and_sensitivity() {
        let mut cam = Camera::default();
        for _ in 0..500 {
            cam.orbit(egui::vec2(0.0, 40.0), 0.008);
        }
        assert_eq!(cam.pitch, PITCH_LIMIT);
        let mut slow = Camera::default();
        let mut fast = Camera::default();
        slow.orbit(egui::vec2(50.0, 0.0), 0.008 * 0.5);
        fast.orbit(egui::vec2(50.0, 0.0), 0.008 * 2.0);
        assert!(
            (fast.yaw - Camera::default().yaw).abs() > (slow.yaw - Camera::default().yaw).abs()
        );
    }

    /// The station labels make physical claims — "stream left to right", "the
    /// frontal view" — and an operator reads a pressure map against them. The
    /// up-reference fallback used to trigger at the 89° plan view and roll it a
    /// quarter turn, so the claims are pinned here rather than trusted.
    #[test]
    fn standard_views_put_the_stream_where_their_labels_promise() {
        let station = |view: StandardView| {
            let mut cam = Camera::default();
            let (yaw, pitch) = view.angles();
            cam.set_pose(Pose {
                yaw,
                pitch,
                target: [0.0; 3],
                ..cam.pose()
            });
            cam
        };
        let stream = [1.0, 0.0, 0.0];
        // Side (−Y) and the plan view both say the stream runs left to right.
        for view in [StandardView::SideLeft, StandardView::Top] {
            let (_, right, _) = station(view).basis();
            assert!(
                dot(right, stream) > 0.99,
                "{}: stream should run left to right, screen-right is {right:?}",
                view.label()
            );
        }
        // Side (+Y) is the mirror image, and says so.
        let (_, right, _) = station(StandardView::SideRight).basis();
        assert!(dot(right, stream) < -0.99, "{right:?}");
        // Plan view from above keeps +Y up the screen; from below it flips.
        let (_, _, up) = station(StandardView::Top).basis();
        assert!(dot(up, [0.0, 1.0, 0.0]) > 0.99, "{up:?}");
        let (_, _, up) = station(StandardView::Bottom).basis();
        assert!(dot(up, [0.0, -1.0, 0.0]) > 0.99, "{up:?}");
        // Upstream stands ahead of the body looking downstream; downstream is
        // the base view looking back into the wake.
        let upstream = station(StandardView::Upstream);
        assert!(upstream.eye()[0] < 0.0, "{:?}", upstream.eye());
        assert!(dot(upstream.basis().0, stream) > 0.99);
        let downstream = station(StandardView::Downstream);
        assert!(downstream.eye()[0] > 0.0, "{:?}", downstream.eye());
        assert!(dot(downstream.basis().0, stream) < -0.99);
        // Iso: upstream, above, and on −Y, as its hover text says.
        let eye = station(StandardView::Iso).eye();
        assert!(eye[0] < 0.0 && eye[1] < 0.0 && eye[2] > 0.0, "{eye:?}");
        // No station may produce a degenerate or skewed basis.
        for view in StandardView::ALL {
            let (fwd, right, up) = station(view).basis();
            let name = view.label();
            assert!(dot(right, up).abs() < 1e-4, "{name} basis is skewed");
            assert!(dot(fwd, right).abs() < 1e-4, "{name} basis is skewed");
            for axis in [fwd, right, up] {
                let length = dot(axis, axis).sqrt();
                assert!((length - 1.0).abs() < 1e-4, "{name}: {length}");
            }
        }
    }

    #[test]
    fn snaps_interpolate_and_settle_exactly() {
        let mut cam = Camera::default();
        let (yaw, pitch) = StandardView::Top.angles();
        let goal = Pose {
            yaw,
            pitch,
            ..cam.pose()
        };
        cam.glide_to(goal, false);
        assert!(cam.anim.is_some());
        assert!(cam.advance(0.1));
        assert!(cam.pitch < PITCH_LIMIT, "still interpolating");
        for _ in 0..10 {
            cam.advance(0.1);
        }
        assert!(cam.anim.is_none());
        assert_eq!(cam.pitch, PITCH_LIMIT);
        // Reduced motion snaps in one frame.
        let mut instant = Camera::default();
        instant.glide_to(goal, true);
        assert!(instant.anim.is_none());
        assert_eq!(instant.pitch, PITCH_LIMIT);
    }

    #[test]
    fn yaw_snaps_take_the_short_way_round() {
        let mut cam = Camera::default();
        cam.set_pose(Pose {
            yaw: 3.4,
            ..cam.pose()
        });
        let (yaw, pitch) = StandardView::Downstream.angles(); // 0 rad
        cam.glide_to(
            Pose {
                yaw,
                pitch,
                ..cam.pose()
            },
            false,
        );
        // 3.4 → 0 direct is 3.4 rad the wrong way round; +τ is 2.88 rad.
        let (_, to, _) = cam.anim.expect("animating");
        assert!((to.yaw - std::f32::consts::TAU).abs() < 1e-4, "{}", to.yaw);
        // Just inside half a turn the direct path is already the short one.
        let mut near = Camera::default();
        near.set_pose(Pose {
            yaw: 3.0,
            ..near.pose()
        });
        near.glide_to(
            Pose {
                yaw,
                pitch,
                ..near.pose()
            },
            false,
        );
        let (_, to, _) = near.anim.expect("animating");
        assert!(to.yaw.abs() < 1e-4, "{}", to.yaw);
    }

    #[test]
    fn navigation_schemes_match_their_documented_mappings() {
        // Reyn: left orbits, middle or shift+left pans.
        assert_eq!(
            gesture_for(NavScheme::Reyn, true, false, false, false),
            Gesture::Orbit
        );
        assert_eq!(
            gesture_for(NavScheme::Reyn, true, false, true, false),
            Gesture::Pan
        );
        assert_eq!(
            gesture_for(NavScheme::Reyn, false, true, false, false),
            Gesture::Pan
        );
        // SolidWorks: the left button never navigates on its own.
        assert_eq!(
            gesture_for(NavScheme::SolidWorks, true, false, false, false),
            Gesture::None
        );
        assert_eq!(
            gesture_for(NavScheme::SolidWorks, false, true, false, false),
            Gesture::Orbit
        );
        assert_eq!(
            gesture_for(NavScheme::SolidWorks, false, true, false, true),
            Gesture::Pan
        );
        // Fusion: middle pans, shift+middle orbits.
        assert_eq!(
            gesture_for(NavScheme::Fusion, false, true, false, false),
            Gesture::Pan
        );
        assert_eq!(
            gesture_for(NavScheme::Fusion, false, true, true, false),
            Gesture::Orbit
        );
        for scheme in NavScheme::ALL {
            assert_eq!(
                gesture_for(scheme, false, false, false, false),
                Gesture::None
            );
            let mapping = scheme.mapping();
            assert_eq!(mapping[0].0, "Orbit");
            assert!(mapping.iter().all(|(_, keys)| !keys.is_empty()));
        }
    }

    #[test]
    fn analytic_streamlines_stay_quarantined_outside_the_sandbox() {
        // The overlay advects a closed-form field, so it may never render on an
        // engineering result — only inside the research sandbox.
        assert!(!analytic_streamlines(true, false));
        assert!(!analytic_streamlines(false, true));
        assert!(analytic_streamlines(true, true));
        assert!(ANALYTIC_STREAMLINE_LABEL.contains("not model velocity"));
    }

    #[test]
    fn model_streamlines_gate_requires_velocity_and_toggle() {
        assert!(!model_streamlines(false, true));
        assert!(!model_streamlines(true, false));
        assert!(model_streamlines(true, true));
    }

    #[test]
    fn sample_model_velocity_uses_cad_field_axis_order() {
        // CadField layout is ((c*N+i)*N+j)*N+k — not z-major.
        let n = 4usize;
        let mut vel = vec![0.0f32; 3 * n * n * n];
        let mut set = |c: usize, i: usize, j: usize, k: usize, v: f32| {
            vel[((c * n + i) * n + j) * n + k] = v;
        };
        set(0, 3, 1, 2, 7.0);
        set(1, 3, 1, 2, 8.0);
        set(2, 3, 1, 2, 9.0);
        let field = ModelVelocityField {
            n,
            vel: vel.into(),
            mask: None,
        };
        // Domain point for voxel (3,1,2) on a 4³ grid.
        let pos = [
            3.0 / 3.0 * 2.0 - 1.0,
            1.0 / 3.0 * 2.0 - 1.0,
            2.0 / 3.0 * 2.0 - 1.0,
        ];
        let sample = sample_model_velocity(&field, pos);
        assert_eq!(sample, [7.0, 8.0, 9.0]);
    }

    #[test]
    fn model_streamlines_terminate_inside_the_solid_mask() {
        let n = 8usize;
        let cube = n * n * n;
        let mut vel = vec![0.0f32; 3 * cube];
        let mut mask = vec![0.0f32; cube];
        // Uniform +X freestream plus a solid brick in the middle.
        for i in 0..n {
            for j in 0..n {
                for k in 0..n {
                    vel[((0 * n + i) * n + j) * n + k] = 1.0;
                    if (3..5).contains(&i) && (3..5).contains(&j) && (3..5).contains(&k) {
                        mask[(i * n + j) * n + k] = 1.0;
                    }
                }
            }
        }
        let field = ModelVelocityField {
            n,
            vel: vel.into(),
            mask: Some(mask.into()),
        };
        let project = |p: [f32; 3]| (Pos2::new(p[0], p[1]), 0.0);
        let brick_center = [
            3.5 / 7.0 * 2.0 - 1.0,
            3.5 / 7.0 * 2.0 - 1.0,
            3.5 / 7.0 * 2.0 - 1.0,
        ];
        assert!(sample_model_mask(&field, brick_center) >= STREAMLINE_SOLID);
        assert_eq!(
            sample_model_velocity(&field, [-0.8, 0.0, 0.0]),
            [1.0, 0.0, 0.0]
        );
        assert!(occluded_by_solid(&field, [-0.9, 0.0, 0.0], [0.9, 0.0, 0.0]));
        assert!(!occluded_by_solid(
            &field,
            [-0.9, 0.8, 0.8],
            [-0.4, 0.8, 0.8]
        ));
        let polys = model_streamline_polys(
            &project,
            &field,
            Some(([-0.05, -0.02, -0.02], [0.05, 0.02, 0.02])),
        );
        assert!(!polys.is_empty());
        assert!(
            polys
                .iter()
                .any(|(poly, _)| poly.len() >= 2 && poly.len() < 48),
            "at least the seeds that strike the brick must terminate"
        );
        let brick_face = 3.0 / 7.0 * 2.0 - 1.0;
        let hitting = polys.iter().filter(|(poly, _)| poly.len() < 48);
        for (poly, _) in hitting {
            let max_x = poly.iter().map(|p| p.x).fold(f32::NEG_INFINITY, f32::max);
            assert!(
                max_x < brick_face + 0.12,
                "terminated ribbon still entered the solid (max x {max_x}, brick {brick_face})"
            );
        }
    }

    #[test]
    fn volume_mode_composites_model_streamlines_when_gated_on() {
        // Engineering Results uses volume raymarch + model tubes together.
        // The gate itself must stay independent of volume_mode so the volume
        // branch can composite streamlines after gpu::add_volume.
        assert!(model_streamlines(true, true));
        assert!(!analytic_streamlines(true, false));
        let seeds = model_streamline_seeds(Some(([-0.2, -0.1, -0.1], [0.2, 0.1, 0.1])));
        assert_eq!(seeds.len(), 64);
        assert!(seeds.iter().all(|p| p[0] < -0.2));
    }
}
