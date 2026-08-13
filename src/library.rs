//! N6 model-library UI — "instrument inventory" (§4.6). Model-bundle facts come
//! from the Python engine; this module only presents them and emits explicit
//! user actions. The screen owns everything: header (title + active chip +
//! the single ember action), toolbar (search · dimension · health filter
//! chips), an inline import-flow feedback area, and a reflowing card grid.
use crate::engine::{
    is_model_bundle_id, ModelCard, ModelValidation, TRUSTED_MODEL_CONVERSION_GUIDANCE,
};
use crate::theme::*;
use egui::{Align, Color32, CornerRadius, Frame, Layout, Margin, RichText, Sense, Stroke, Vec2};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DimensionFilter {
    #[default]
    All,
    TwoD,
    ThreeD,
}

impl DimensionFilter {
    const ALL: [Self; 3] = [Self::All, Self::TwoD, Self::ThreeD];

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::TwoD => "2D",
            Self::ThreeD => "3D",
        }
    }
}

/// Health filter — the old passive counters as actionable chips (§4.6, A19).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HealthFilter {
    #[default]
    All,
    NeedsReview,
    Rejected,
}

#[derive(Default)]
pub struct LibraryState {
    pub search: String,
    pub dimension: DimensionFilter,
    pub health: HealthFilter,
    pub pending_delete: Option<String>,
    pub busy: bool,
    pub notice: Option<(String, bool)>,
    pub validation: Option<ModelValidation>,
}

pub enum LibraryAction {
    Activate(String),
    Delete(String),
    Import,
    Refresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Health {
    Ready,
    NeedsReview,
    Rejected,
}

fn is_unknown(value: &str) -> bool {
    let value = value.trim();
    value.is_empty()
        || value.eq_ignore_ascii_case("unknown")
        || value.eq_ignore_ascii_case("legacy/unknown")
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(u64::MAX)
}

fn modified_timestamp_known(unix: u64, now: u64) -> bool {
    unix > 0 && unix <= now.saturating_add(300)
}

/// Metadata required for a card to read as complete. A validated contract may
/// still have provenance/applicability gaps; those remain review state rather
/// than inheriting the contract's green status.
fn metadata_gaps(model: &ModelCard) -> Vec<&'static str> {
    let mut gaps = Vec::new();
    if model.size_bytes == 0 {
        gaps.push("file size");
    }
    if !modified_timestamp_known(model.modified_unix, unix_now()) {
        gaps.push("modified timestamp");
    }
    if !is_sha256(&model.checkpoint_sha256) {
        gaps.push("bundle SHA-256");
    }
    if !matches!(model.dimension, 2 | 3) {
        gaps.push("field dimension");
    }
    if model.grid == 0 {
        gaps.push("grid");
    }
    if model.in_channels == 0 || model.out_channels == 0 {
        gaps.push("channel contract");
    }
    if model.max_steps == 0 {
        gaps.push("prediction horizon");
    }
    if model.epoch == 0 || model.declared_epochs == 0 || model.epoch > model.declared_epochs {
        gaps.push("training epoch");
    }
    if is_unknown(&model.checkpoint_role) {
        gaps.push("checkpoint role");
    }
    if is_unknown(&model.scenario) {
        gaps.push("regime");
    }
    if model.source_digest.as_deref().is_none_or(is_unknown) {
        gaps.push("source digest");
    }
    if is_unknown(&model.physics_contract) {
        gaps.push("physics contract");
    }
    if model.authenticity_status != "verified" {
        gaps.push("publisher authenticity");
    }
    if model.publisher_key_id.as_deref().is_none_or(is_unknown) {
        gaps.push("publisher key id");
    }
    if !model.publisher_key_sha256.as_deref().is_some_and(is_sha256) {
        gaps.push("publisher key fingerprint");
    }
    if model.release_sequence.is_none() {
        gaps.push("release sequence");
    }
    if model.support.is_empty() || model.support.iter().any(|value| is_unknown(value)) {
        gaps.push("support envelope");
    }
    if model.limitations.is_empty() || model.limitations.iter().any(|value| is_unknown(value)) {
        gaps.push("limitations");
    }
    if model
        .benchmark_report_hashes
        .iter()
        .any(|digest| !is_sha256(digest))
    {
        gaps.push("benchmark report hash");
    }
    if !model.unknown_fields.is_empty() {
        gaps.push("legacy fields");
    }
    gaps
}

fn model_health(model: &ModelCard) -> Health {
    if !is_model_bundle_id(&model.id)
        || !is_model_bundle_id(&model.name)
        || model.authenticity_status != "verified"
    {
        return Health::Rejected;
    }
    match model.status.as_str() {
        "invalid" => Health::Rejected,
        "clean" if metadata_gaps(model).is_empty() => Health::Ready,
        _ => Health::NeedsReview,
    }
}

fn status_color(model: &ModelCard) -> Color32 {
    match model_health(model) {
        Health::Ready => OK,
        Health::Rejected => DANGER,
        Health::NeedsReview => WARN,
    }
}

/// Scientific-state token (mono-chip caps); never color-only — the dot glyph
/// and the word travel together.
fn status_label(model: &ModelCard) -> &'static str {
    match model_health(model) {
        Health::Ready => "CONTRACT OK",
        Health::Rejected => "REJECTED",
        Health::NeedsReview if model.status == "clean" => "METADATA GAPS",
        Health::NeedsReview => "REVIEW REQUIRED",
    }
}

fn matches_dimension(model: &ModelCard, dimension: DimensionFilter) -> bool {
    match dimension {
        DimensionFilter::All => true,
        DimensionFilter::TwoD => model.dimension == 2,
        DimensionFilter::ThreeD => model.dimension == 3,
    }
}

fn matches_search(model: &ModelCard, search: &str) -> bool {
    let needle = search.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }

    let scalar_fields = [
        model.id.as_str(),
        model.name.as_str(),
        model.status.as_str(),
        model.status_detail.as_str(),
        model.scenario.as_str(),
        model.checkpoint_role.as_str(),
        model.physics_contract.as_str(),
        model.checkpoint_sha256.as_str(),
        model.authenticity_status.as_str(),
    ];
    let scalar_match = scalar_fields
        .iter()
        .any(|value| value.to_lowercase().contains(&needle))
        || model
            .source_digest
            .as_deref()
            .is_some_and(|value| value.to_lowercase().contains(&needle))
        || model
            .publisher_key_id
            .as_deref()
            .is_some_and(|value| value.to_lowercase().contains(&needle))
        || model
            .publisher_key_sha256
            .as_deref()
            .is_some_and(|value| value.to_lowercase().contains(&needle));
    let list_match = model
        .support
        .iter()
        .chain(&model.limitations)
        .chain(&model.benchmark_report_hashes)
        .chain(&model.unknown_fields)
        .any(|value| value.to_lowercase().contains(&needle));
    let gaps = metadata_gaps(model);
    scalar_match
        || list_match
        || gaps.iter().any(|gap| gap.contains(&needle))
        || (!gaps.is_empty() && "unknown".starts_with(&needle))
}

fn matches_scope(model: &ModelCard, state: &LibraryState) -> bool {
    matches_dimension(model, state.dimension) && matches_search(model, &state.search)
}

fn visible(model: &ModelCard, state: &LibraryState) -> bool {
    let health_matches = match state.health {
        HealthFilter::All => true,
        HealthFilter::NeedsReview => model_health(model) == Health::NeedsReview,
        HealthFilter::Rejected => model_health(model) == Health::Rejected,
    };
    matches_scope(model, state) && health_matches
}

/// Counts are scoped by search and dimension so chip numbers describe the
/// inventory the user is currently filtering, not hidden global totals.
fn health_counts(models: &[ModelCard], state: &LibraryState) -> (usize, usize, usize) {
    models
        .iter()
        .filter(|model| matches_scope(model, state))
        .fold(
            (0, 0, 0),
            |(total, review, rejected), model| match model_health(model) {
                Health::Ready => (total + 1, review, rejected),
                Health::NeedsReview => (total + 1, review + 1, rejected),
                Health::Rejected => (total + 1, review, rejected + 1),
            },
        )
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    if bytes == 0 {
        "UNKNOWN".into()
    } else if bytes as f64 >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB)
    } else if bytes as f64 >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else if bytes as f64 >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn format_modified(unix: u64) -> String {
    format_modified_at(unix, unix_now())
}

fn format_modified_at(unix: u64, now: u64) -> String {
    if unix == 0 {
        return "UNKNOWN".into();
    }
    if !modified_timestamp_known(unix, now) {
        return "future timestamp".into();
    }
    let days = now.saturating_sub(unix) / 86_400;
    match days {
        0 => "today".into(),
        1 => "1 day ago".into(),
        value if value < 365 => format!("{value} days ago"),
        value => format!("{:.1} years ago", value as f32 / 365.25),
    }
}

fn compact_role(role: &str) -> &str {
    match role {
        "fixed_final" => "fixed final",
        "best_validation" => "validation selected",
        "legacy/unknown" => "UNKNOWN",
        other => other,
    }
}

fn known_u32(value: u32) -> String {
    if value == 0 {
        "UNKNOWN".into()
    } else {
        value.to_string()
    }
}

fn known_text(value: &str) -> &str {
    if is_unknown(value) {
        "UNKNOWN"
    } else {
        value
    }
}

fn epoch_text(model: &ModelCard) -> String {
    match (model.epoch, model.declared_epochs) {
        (0, 0) => "UNKNOWN".into(),
        (epoch, 0) => format!("{epoch} / UNKNOWN"),
        (0, declared) => format!("UNKNOWN / {declared}"),
        (epoch, declared) => format!("{epoch} / {declared}"),
    }
}

fn support_summary(model: &ModelCard) -> String {
    if !matches!(model.dimension, 2 | 3) {
        return "UNKNOWN contract".into();
    }
    let grid = if model.grid == 0 {
        "UNKNOWN grid".into()
    } else if model.dimension == 3 {
        format!("{}³ grid", model.grid)
    } else {
        format!("{}² grid", model.grid)
    };
    let horizon = if model.max_steps == 0 {
        // Matches `contract_line`'s wording — "hUNKNOWN" read as a token.
        "horizon UNKNOWN".to_owned()
    } else {
        format!("h{}", model.max_steps)
    };
    format!(
        "{}D · {grid} · {} → {} ch · {horizon}",
        model.dimension,
        known_u32(model.in_channels),
        known_u32(model.out_channels),
    )
}

/// Plain-language contract line (§4.6 card row 2). Same data as
/// `support_summary`, human-ordered; UNKNOWN stays UNKNOWN.
fn contract_line(model: &ModelCard) -> String {
    if model.dimension == 0 {
        return "Contract UNKNOWN — no field dimension declared".into();
    }
    if !matches!(model.dimension, 2 | 3) {
        return format!(
            "Contract unsupported — {}D field dimension is not recognized",
            model.dimension
        );
    }
    let grid = if model.grid == 0 {
        "UNKNOWN grid".into()
    } else if model.dimension == 3 {
        format!("{}³ grid", model.grid)
    } else {
        format!("{}² grid", model.grid)
    };
    format!(
        "{}D velocity field · {grid} · {} → {} channels · horizon {}",
        model.dimension,
        known_u32(model.in_channels),
        known_u32(model.out_channels),
        known_u32(model.max_steps)
    )
}

/// Filenames are data, not titles (§3.1): the card title is humanized while a
/// middle-elided mono value preserves both identifying ends; hover keeps the
/// complete filename available.
fn humanize_name(name: &str) -> String {
    let stem = name.strip_suffix(".reynmodel").unwrap_or(name);
    let spaced: String = stem
        .chars()
        .map(|character| match character {
            '_' | '-' => ' ',
            other => other,
        })
        .collect();
    let mut characters = spaced.chars();
    let humanized = match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => spaced,
    };
    humanized
}

fn compact_name(name: &str) -> String {
    compact_middle(name, 42)
}

fn compact_middle(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.into();
    }
    let kept = max_chars.saturating_sub(1);
    let start_len = kept * 2 / 3;
    let end_len = kept.saturating_sub(start_len);
    let start: String = value.chars().take(start_len).collect();
    let end: String = value
        .chars()
        .rev()
        .take(end_len)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{start}…{end}")
}

fn compact_digest(value: &str) -> String {
    if is_unknown(value) {
        "UNKNOWN".into()
    } else {
        compact_middle(value, 29)
    }
}

fn metadata_gap_summary(model: &ModelCard) -> Option<String> {
    let gaps = metadata_gaps(model);
    if gaps.is_empty() {
        return None;
    }
    let visible = gaps.iter().take(4).copied().collect::<Vec<_>>().join(" · ");
    let remaining = gaps.len().saturating_sub(4);
    Some(if remaining == 0 {
        format!("Metadata review: {visible}")
    } else {
        format!("Metadata review: {visible} · +{remaining} more")
    })
}

fn counted_filter_label(label: &str, count: usize) -> String {
    format!("{label} · {count}")
}

fn card_columns(width: f32) -> usize {
    ((width + 12.0) / 312.0).floor().clamp(1.0, 3.0) as usize
}

/// Toolbar filter chip: label + optional count dot color; tonal when active.
fn filter_chip(ui: &mut egui::Ui, label: &str, active: bool, accent: Option<Color32>) -> bool {
    let visible_label = if accent.is_some() {
        format!("● {label}")
    } else {
        label.into()
    };
    let response = ui.add(
        egui::Button::new(
            RichText::new(visible_label)
                .text_style(body_strong())
                .color(if active {
                    TEXT
                } else {
                    accent.unwrap_or(TEXT_DIM)
                }),
        )
        .selected(active)
        .fill(if active { SURFACE_HIGHEST } else { SURFACE })
        .stroke(Stroke::new(1.0, if active { OUTLINE } else { HAIRLINE }))
        .corner_radius(CornerRadius::same(R1))
        .min_size(Vec2::new(0.0, 26.0)),
    );
    if response.has_focus() {
        ui.painter().rect_stroke(
            response.rect.expand(1.0),
            CornerRadius::same(R1),
            focus_stroke(),
            egui::StrokeKind::Outside,
        );
    }
    response.clicked()
}

fn import_button(ui: &mut egui::Ui, state: &LibraryState) -> bool {
    let label = if state.busy {
        "Working…"
    } else {
        "Import model bundle…"
    };
    ui.add_enabled(
        !state.busy,
        egui::Button::new(RichText::new(label).color(ON_EMBER))
            .fill(EMBER)
            .min_size(Vec2::new(150.0, 32.0)),
    )
    .on_disabled_hover_text("Wait for the current model-bundle operation to finish")
    .clicked()
}

fn active_checkpoint_chip(
    ui: &mut egui::Ui,
    models: &[ModelCard],
    current_model: &str,
    busy: bool,
) {
    let active = models.iter().find(|model| model.id == current_model);
    let rejected = active.is_some_and(|model| model_health(model) == Health::Rejected);
    let legacy = current_model.to_ascii_lowercase().ends_with(".pth");
    let pending = busy && models.is_empty() && !current_model.trim().is_empty();
    let unavailable = active.is_none() && !pending && !current_model.trim().is_empty();
    let label = if rejected {
        "! ACTIVE MODEL REJECTED".into()
    } else if let Some(model) = active {
        format!("◆ ACTIVE · {}", compact_name(&model.name))
    } else if legacy {
        "! LEGACY MODEL NEEDS CONVERSION".into()
    } else if pending {
        "◐ ACTIVE MODEL PENDING".into()
    } else if unavailable {
        "! ACTIVE MODEL UNAVAILABLE".into()
    } else {
        "○ NO ACTIVE MODEL".into()
    };
    let hue = if unavailable || legacy || rejected {
        WARN
    } else {
        TEXT_DIM
    };
    let fill = if unavailable || legacy || rejected {
        tint_fill(WARN)
    } else {
        SURFACE
    };
    let stroke = if unavailable || legacy || rejected {
        tint_hairline(WARN)
    } else {
        HAIRLINE
    };
    let response = Frame::NONE
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(CornerRadius::same(R1))
        .inner_margin(Margin::symmetric(9, 5))
        .show(ui, |ui| {
            ui.label(RichText::new(label).text_style(mono_s()).color(hue));
        })
        .response;
    if rejected {
        let model = active.expect("rejected active model");
        response.on_hover_text(format!(
            "{}\n{}",
            model.status_detail, TRUSTED_MODEL_CONVERSION_GUIDANCE
        ));
    } else if let Some(model) = active {
        response.on_hover_text(format!(
            "Active model bundle\n{}\n{}\nauthenticity {}\npublisher {}\nsha256 {}",
            model.name,
            support_summary(model),
            known_text(&model.authenticity_status),
            model.publisher_key_id.as_deref().unwrap_or("UNKNOWN"),
            known_text(&model.checkpoint_sha256)
        ));
    } else if legacy {
        response.on_hover_text(TRUSTED_MODEL_CONVERSION_GUIDANCE);
    } else if pending {
        response.on_hover_text("The inventory is loading; active selection is not yet resolved.");
    } else if unavailable {
        response.on_hover_text(format!(
            "The active model id is not present in the current inventory.\n{}",
            current_model
        ));
    } else {
        response.on_hover_text("Select a compatible verified model bundle from the library.");
    }
}

fn search_control(ui: &mut egui::Ui, state: &mut LibraryState, desired_width: f32) {
    ui.horizontal(|ui| {
        let clear_width = if state.search.is_empty() { 0.0 } else { 52.0 };
        let response = ui
            .add(
                egui::TextEdit::singleline(&mut state.search)
                    .hint_text("Search name, regime, contract, hash…")
                    .desired_width((desired_width - clear_width).max(120.0)),
            )
            .on_hover_text(
                "Searches model-bundle name, id, regime, contract, provenance, support, limitations, and report hashes",
            );
        if response.has_focus() && ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            state.search.clear();
            response.request_focus();
        }
        if !state.search.is_empty() && ui.small_button("Clear").clicked() {
            state.search.clear();
            response.request_focus();
        }
    });
}

fn dimension_picker(ui: &mut egui::Ui, state: &mut LibraryState) {
    Frame::NONE
        .fill(SURFACE)
        .corner_radius(CornerRadius::same(R2))
        .stroke(Stroke::new(1.0, HAIRLINE))
        .inner_margin(2)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for dimension in DimensionFilter::ALL {
                    let selected = state.dimension == dimension;
                    let (fill, color) = if selected {
                        (SURFACE_HIGHEST, TEXT)
                    } else {
                        (Color32::TRANSPARENT, TEXT_DIM)
                    };
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(dimension.label())
                                    .text_style(body_strong())
                                    .color(color),
                            )
                            .selected(selected)
                            .fill(fill)
                            .corner_radius(CornerRadius::same(R1))
                            .stroke(Stroke::NONE),
                        )
                        .clicked()
                    {
                        state.dimension = dimension;
                    }
                }
            });
        });
}

fn refresh_button(ui: &mut egui::Ui, busy: bool) -> bool {
    ui.add_enabled(
        !busy,
        egui::Button::new(
            RichText::new(format!(
                "{}  Refresh",
                egui_phosphor::regular::ARROWS_CLOCKWISE
            ))
            .text_style(body_strong()),
        ),
    )
    .on_disabled_hover_text("Wait for the current model-bundle operation to finish")
    .clicked()
}

/// One calm feedback area for the import flow (notice + structured
/// validation), rendered inline under the toolbar — never a floating console.
fn import_feedback(ui: &mut egui::Ui, state: &mut LibraryState) {
    let mut clear_notice = false;
    if let Some((message, is_error)) = &state.notice {
        let hue = if *is_error { DANGER } else { OK };
        let glyph = if *is_error { "!" } else { "✓" };
        Frame::NONE
            .fill(tint_fill(hue))
            .stroke(Stroke::new(1.0, tint_hairline(hue)))
            .corner_radius(CornerRadius::same(R1))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(glyph).text_style(caption()).color(hue));
                    ui.add(
                        egui::Label::new(
                            RichText::new(message).text_style(caption()).color(TEXT_DIM),
                        )
                        .wrap(),
                    );
                    if ui.small_button("Dismiss").clicked() {
                        clear_notice = true;
                    }
                });
            });
        ui.add_space(10.0);
    }
    if clear_notice {
        state.notice = None;
    }
    let mut clear_validation = false;
    if let Some(validation) = &state.validation {
        // Single owner of the import rejection: plain-language line first,
        // verbatim structured codes beneath (SCI semantics preserved).
        Frame::NONE
            .fill(tint_fill(DANGER))
            .stroke(Stroke::new(1.0, tint_hairline(DANGER)))
            .corner_radius(CornerRadius::same(R1))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("×").text_style(caption()).color(DANGER));
                    ui.add(
                        egui::Label::new(
                            RichText::new("Import rejected — the active model is unchanged.")
                                .text_style(body_strong())
                                .color(TEXT),
                        )
                        .wrap(),
                    );
                    if ui.small_button("Dismiss").clicked() {
                        clear_validation = true;
                    }
                });
                if validation.issues.is_empty() {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&validation.summary)
                                .text_style(caption())
                                .color(TEXT_DIM),
                        )
                        .wrap(),
                    );
                } else {
                    ui.add_space(4.0);
                    ui.label(overline_text("Structured validation"));
                    for issue in &validation.issues {
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("{} · {}", issue.code, issue.message))
                                    .text_style(mono_s())
                                    .color(TEXT_DIM),
                            )
                            .wrap(),
                        )
                        .on_hover_text(format!("Field: {} · {}", issue.field, issue.severity));
                    }
                }
            });
        ui.add_space(10.0);
    }
    if clear_validation {
        state.validation = None;
    }
}

/// Skeleton card rows while the engine inventory loads (§3.7 shimmer).
fn skeleton_grid(ui: &mut egui::Ui, columns: usize) {
    let time = ui.input(|input| input.time);
    let animate = !reduced_motion(ui.ctx());
    for row in 0..2 {
        ui.columns(columns, |cells| {
            for (column, cell) in cells.iter_mut().enumerate() {
                let phase = (row * columns + column) as f64 * 0.35;
                let pulse = if animate {
                    (((time * std::f64::consts::TAU / 1.1 + phase).sin() * 0.5 + 0.5) * 0.16) as f32
                } else {
                    0.08
                };
                Frame::NONE
                    .fill(SURFACE)
                    .corner_radius(CornerRadius::same(R2))
                    .inner_margin(Margin::same(16))
                    .show(cell, |ui| {
                        ui.set_width(ui.available_width());
                        for width_fraction in [0.55, 0.85, 0.7, 0.4] {
                            let (rect, _) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width() * width_fraction, 13.0),
                                Sense::hover(),
                            );
                            ui.painter().rect_filled(
                                rect,
                                CornerRadius::same(3),
                                Color32::from_white_alpha((10.0 + pulse * 255.0 * 0.12) as u8),
                            );
                            ui.add_space(9.0);
                        }
                    });
            }
        });
        ui.add_space(12.0);
    }
    if animate {
        ui.ctx().request_repaint();
    }
}

pub fn show_library(
    ui: &mut egui::Ui,
    models: &[ModelCard],
    current_model: &str,
    state: &mut LibraryState,
) -> Option<LibraryAction> {
    let mut action = None;
    content_column(ui, CONTENT_MAX_WIDTH, |ui| {
        ui.add_space(26.0);

        // Header: the ONLY "Model Library" title; right side carries the
        // active-model chip and the screen's single ember action.
        let narrow_header = ui.available_width() < 820.0;
        let heading = |ui: &mut egui::Ui| {
            ui.vertical(|ui| {
                ui.label(display_text("Model Library"));
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "Local .reynmodel bundles, declared contracts, and provenance gaps—before inference.",
                        )
                        .text_style(caption())
                        .color(TEXT_MUTE),
                    )
                    .wrap(),
                );
            });
        };
        if narrow_header {
            heading(ui);
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                if import_button(ui, state) {
                    action = Some(LibraryAction::Import);
                }
                active_checkpoint_chip(ui, models, current_model, state.busy);
            });
        } else {
            ui.horizontal(|ui| {
                heading(ui);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if import_button(ui, state) {
                        action = Some(LibraryAction::Import);
                    }
                    ui.add_space(8.0);
                    active_checkpoint_chip(ui, models, current_model, state.busy);
                });
            });
        }
        ui.add_space(16.0);

        // Toolbar: search · dimension · refresh, followed by health filters.
        ui.label(overline_text("Search & filters"));
        ui.add_space(4.0);
        let narrow_toolbar = ui.available_width() < 620.0;
        if narrow_toolbar {
            let search_width = ui.available_width();
            search_control(ui, state, search_width);
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                dimension_picker(ui, state);
                if refresh_button(ui, state.busy) {
                    action = Some(LibraryAction::Refresh);
                }
            });
        } else {
            ui.horizontal(|ui| {
                search_control(ui, state, 280.0);
                ui.add_space(6.0);
                dimension_picker(ui, state);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if refresh_button(ui, state.busy) {
                        action = Some(LibraryAction::Refresh);
                    }
                });
            });
        }

        let (total, review, rejected) = health_counts(models, state);
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(overline_text("Model health"));
            if filter_chip(
                ui,
                &counted_filter_label("All", total),
                state.health == HealthFilter::All,
                None,
            ) {
                state.health = HealthFilter::All;
            }
            if filter_chip(
                ui,
                &counted_filter_label("Needs review", review),
                state.health == HealthFilter::NeedsReview,
                Some(WARN),
            ) {
                state.health = if state.health == HealthFilter::NeedsReview {
                    HealthFilter::All
                } else {
                    HealthFilter::NeedsReview
                };
            }
            if filter_chip(
                ui,
                &counted_filter_label("Rejected", rejected),
                state.health == HealthFilter::Rejected,
                Some(DANGER),
            ) {
                state.health = if state.health == HealthFilter::Rejected {
                    HealthFilter::All
                } else {
                    HealthFilter::Rejected
                };
            }
        });
        ui.add_space(12.0);

        // Import-flow feedback lives inline, right under the toolbar.
        import_feedback(ui, state);
        if state.busy && state.notice.is_none() {
            ui.horizontal_wrapped(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new("Model-bundle operation in progress…")
                        .text_style(caption())
                        .color(TEXT_DIM),
                );
            });
            ui.add_space(10.0);
        }

        let columns = card_columns(ui.available_width());

        if state.busy && models.is_empty() {
            ui.label(
                RichText::new("Loading model inventory…")
                    .text_style(body_strong())
                    .color(TEXT_DIM),
            );
            ui.add_space(8.0);
            skeleton_grid(ui, columns);
            return;
        }
        if models.is_empty() {
            let unavailable = state.notice.as_ref().is_some_and(|(_, is_error)| *is_error);
            Frame::NONE
                .fill(SURFACE)
                .stroke(Stroke::new(
                    1.0,
                    if unavailable {
                        tint_hairline(WARN)
                    } else {
                        HAIRLINE
                    },
                ))
                .corner_radius(CornerRadius::same(R2))
                .inner_margin(Margin::same(28))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(title_text(if unavailable {
                        "Model inventory unavailable"
                    } else {
                        "No model bundles found"
                    }));
                    ui.add(
                        egui::Label::new(
                            RichText::new(if unavailable {
                                "Reyn could not load the engine inventory. Existing project evidence remains untouched; retry when the engine is available."
                            } else {
                                "Import a verified .reynmodel bundle together with its adjacent .reynmodel.sig publisher signature. Legacy .pth files are never opened; convert a checkpoint you trust offline with convert_model_bundle.py."
                            })
                            .text_style(caption())
                            .color(TEXT_MUTE),
                        )
                        .wrap(),
                    );
                    ui.add_space(10.0);
                    if unavailable {
                        if ui.button("Retry inventory").clicked() {
                            action = Some(LibraryAction::Refresh);
                        }
                    } else if ui.button("Import model bundle…").clicked() {
                        action = Some(LibraryAction::Import);
                    }
                });
            return;
        }

        let filtered: Vec<&ModelCard> = models
            .iter()
            .filter(|model| visible(model, state))
            .collect();
        if filtered.is_empty() {
            Frame::NONE
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, HAIRLINE))
                .corner_radius(CornerRadius::same(R2))
                .inner_margin(Margin::same(20))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(title_text("No matching model bundles"));
                    ui.label(
                        RichText::new(
                            "No model bundle matches the current search, dimension, and health filters.",
                        )
                        .text_style(caption())
                        .color(TEXT_MUTE),
                    );
                    ui.add_space(8.0);
                    if ui.button("Clear all filters").clicked() {
                        state.search.clear();
                        state.dimension = DimensionFilter::All;
                        state.health = HealthFilter::All;
                    }
                });
            return;
        }

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for row in filtered.chunks(columns) {
                    ui.columns(columns, |cells| {
                        for (column, model) in row.iter().enumerate() {
                            if let Some(card_action) =
                                model_card(&mut cells[column], model, current_model, state)
                            {
                                action = Some(card_action);
                            }
                        }
                    });
                    ui.add_space(12.0);
                }
                // Bottom breathing room so the last row never touches the
                // panel edge (QA L6).
                ui.add_space(24.0);
            });
    });
    action
}

fn model_card(
    ui: &mut egui::Ui,
    model: &ModelCard,
    current_model: &str,
    state: &mut LibraryState,
) -> Option<LibraryAction> {
    let mut action = None;
    let active = model.id == current_model;
    let card_health = model_health(model);
    let rejected = card_health == Health::Rejected;
    // Rejected cards recede: dimmed text, red only on the dot + status chip.
    let (title_color, body_color, detail_color) = if rejected {
        (TEXT_DIM, TEXT_MUTE, TEXT_MUTE)
    } else {
        (TEXT, TEXT_DIM, TEXT_MUTE)
    };
    let frame_response = Frame::NONE
        .fill(if active { SURFACE_HIGH } else { SURFACE })
        .stroke(Stroke::new(1.0, if active { OUTLINE } else { HAIRLINE }))
        .corner_radius(CornerRadius::same(R2))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_min_height(200.0);
            let status_token = |ui: &mut egui::Ui| {
                let label = format!("● {}", status_label(model));
                let response = ui.label(
                    chip_text(&label).color(status_color(model)),
                );
                match card_health {
                    Health::Ready => response.on_hover_text(
                        "Declared contract validates and required card metadata is present.",
                    ),
                    Health::NeedsReview if model.status == "clean" => response.on_hover_text(
                        "The declared contract validates, but provenance or applicability metadata is incomplete.",
                    ),
                    Health::NeedsReview => response.on_hover_text(
                        "The engine did not return a clean contract status; review the stated reason.",
                    ),
                    Health::Rejected => response.on_hover_text(
                        "The model bundle failed structured contract validation and cannot be activated.",
                    ),
                };
            };
            if ui.available_width() < 260.0 {
                ui.add(
                    egui::Label::new(
                        RichText::new(humanize_name(&model.name))
                            .text_style(title())
                            .color(title_color),
                    )
                    .truncate(),
                )
                .on_hover_text(&model.name);
                ui.add_space(4.0);
                status_token(ui);
            } else {
                ui.horizontal(|ui| {
                    // Title truncates; the status chip keeps priority (QA L4).
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        status_token(ui);
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(humanize_name(&model.name))
                                        .text_style(title())
                                        .color(title_color),
                                )
                                .truncate(),
                            )
                            .on_hover_text(&model.name);
                        });
                    });
                });
            }
            ui.label(
                RichText::new(compact_name(&model.name))
                    .text_style(mono_s())
                    .color(detail_color),
            )
            .on_hover_text(&model.name);
            ui.add_space(6.0);
            ui.add(
                egui::Label::new(RichText::new(contract_line(model)).color(body_color)).wrap(),
            );
            if let Some(gaps) = metadata_gap_summary(model) {
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(gaps).text_style(caption()).color(WARN),
                    )
                    .wrap(),
                );
            }
            if !model.status_detail.trim().is_empty() && model.status != "clean" {
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(&model.status_detail)
                            .text_style(caption())
                            .color(if rejected { DANGER } else { WARN }),
                    )
                    .wrap(),
                );
            }
            ui.add_space(6.0);
            ui.collapsing("Model facts & provenance", |ui| {
                egui::Grid::new(("model.facts", &model.id))
                    .num_columns(2)
                    .spacing([12.0, 5.0])
                    .show(ui, |ui| {
                        fact(ui, "Regime", known_text(&model.scenario));
                        ui.end_row();
                        fact(ui, "Epoch", &epoch_text(model));
                        ui.end_row();
                        fact(ui, "Role", known_text(compact_role(&model.checkpoint_role)));
                        ui.end_row();
                        fact(ui, "Contract", &support_summary(model));
                        ui.end_row();
                        fact(ui, "Physics", known_text(&model.physics_contract));
                        ui.end_row();
                        fact(
                            ui,
                            "Authenticity",
                            known_text(&model.authenticity_status),
                        );
                        ui.end_row();
                        fact(
                            ui,
                            "Publisher key",
                            model.publisher_key_id.as_deref().unwrap_or("UNKNOWN"),
                        )
                        .on_hover_text(format!(
                            "Publisher key fingerprint\n{}",
                            model
                                .publisher_key_sha256
                                .as_deref()
                                .unwrap_or("UNKNOWN")
                        ));
                        ui.end_row();
                        fact(
                            ui,
                            "Release sequence",
                            &model
                                .release_sequence
                                .map(|sequence| sequence.to_string())
                                .unwrap_or_else(|| "UNKNOWN".into()),
                        );
                        ui.end_row();
                        fact(ui, "Size", &format_bytes(model.size_bytes));
                        ui.end_row();
                        fact(ui, "Modified", &format_modified(model.modified_unix));
                        ui.end_row();
                        fact(
                            ui,
                            "Bundle SHA-256",
                            &compact_digest(&model.checkpoint_sha256),
                        )
                        .on_hover_text(format!(
                            "Bundle SHA-256\n{}",
                            known_text(&model.checkpoint_sha256)
                        ));
                        ui.end_row();
                        let source_digest = model
                            .source_digest
                            .as_deref()
                            .map(compact_digest)
                            .unwrap_or_else(|| "UNKNOWN".into());
                        fact(
                            ui,
                            "Source digest",
                            &source_digest,
                        )
                        .on_hover_text(format!(
                            "Source digest\n{}",
                            model.source_digest.as_deref().unwrap_or("UNKNOWN")
                        ));
                        ui.end_row();
                        fact(
                            ui,
                            "Library copy",
                            if model.managed { "MANAGED" } else { "NOT MANAGED" },
                        );
                        ui.end_row();
                    });
                ui.add_space(5.0);
                ui.label(overline_text("Declared envelope"));
                if model.support.is_empty() {
                    ui.label(
                        RichText::new("UNKNOWN — no applicability envelope declared")
                            .text_style(caption())
                            .color(WARN),
                    );
                } else {
                    for support in &model.support {
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("• {support}"))
                                    .text_style(caption())
                                    .color(TEXT_DIM),
                            )
                            .wrap(),
                        );
                    }
                }
                if is_unknown(&model.physics_contract) {
                    ui.add(
                        egui::Label::new(
                            RichText::new("• physics contract: UNKNOWN")
                                .text_style(caption())
                                .color(WARN),
                        )
                        .wrap(),
                    );
                } else {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!(
                                "• physics contract: {}",
                                model.physics_contract
                            ))
                            .text_style(caption())
                            .color(TEXT_DIM),
                        )
                        .wrap(),
                    );
                }
                ui.add_space(5.0);
                ui.label(overline_text("Limitations"));
                if model.limitations.is_empty() {
                    ui.label(
                        RichText::new("UNKNOWN — model bundle declares no limitations")
                            .text_style(caption())
                            .color(WARN),
                    );
                } else {
                    for limitation in &model.limitations {
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("• {limitation}"))
                                    .text_style(caption())
                                    .color(TEXT_DIM),
                            )
                            .wrap(),
                        );
                    }
                }
                ui.add_space(5.0);
                ui.label(overline_text("Benchmark reports"));
                if model.benchmark_report_hashes.is_empty() {
                    ui.label(
                        RichText::new("NONE DECLARED")
                            .text_style(mono_s())
                            .color(TEXT_MUTE),
                    );
                } else {
                    for digest in &model.benchmark_report_hashes {
                        let valid = is_sha256(digest);
                        ui.label(
                            RichText::new(if valid {
                                compact_digest(digest)
                            } else {
                                format!("UNRECOGNIZED · {}", compact_digest(digest))
                            })
                            .text_style(mono_s())
                            .color(if valid { BRAND } else { WARN }),
                        )
                        .on_hover_text(format!(
                            "{}\n{}",
                            if valid {
                                "Canonical report SHA-256"
                            } else {
                                "Malformed report hash — not treated as a valid link"
                            },
                            digest
                        ));
                    }
                }
                if !model.unknown_fields.is_empty() {
                    ui.add_space(5.0);
                    ui.label(overline_text("Unknown legacy fields"));
                    ui.add(
                        egui::Label::new(
                            RichText::new(model.unknown_fields.join(" · "))
                                .text_style(mono_s())
                                .color(WARN),
                        )
                        .wrap(),
                    );
                }
            });
            // Footer flows after content (QA L2): no bottom_up inside a
            // height-unbounded grid cell, which stretched cards and let the
            // footer overlap opened disclosure content.
            ui.add_space(10.0);
            {
                if state.pending_delete.as_deref() == Some(&model.id) {
                    // Inline destructive confirmation — danger fill is one of
                    // the two sanctioned full-saturation uses (§3.3).
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(
                                !state.busy,
                                egui::Button::new(
                                    RichText::new("Delete model bundle").color(Color32::WHITE),
                                )
                                .fill(DANGER),
                            )
                            .on_disabled_hover_text(
                                "Wait for the current model-bundle operation to finish",
                            )
                            .clicked()
                        {
                            action = Some(LibraryAction::Delete(model.id.clone()));
                        }
                        if ui.button("Cancel").clicked() {
                            state.pending_delete = None;
                        }
                    });
                } else {
                    ui.horizontal_wrapped(|ui| {
                        if active {
                            // Ember budget (QA L5): the edge marker carries
                            // the accent; the chip is a quiet state token.
                            ui.label(chip_text("◆ ACTIVE").color(TEXT_DIM));
                        } else {
                            if ui
                                .add_enabled(
                                    !rejected && !state.busy,
                                    egui::Button::new("Set active"),
                                )
                                .on_disabled_hover_text(if rejected {
                                    "Rejected model bundles cannot be activated"
                                } else {
                                    "Wait for the current model-bundle operation to finish"
                                })
                                .clicked()
                            {
                                action = Some(LibraryAction::Activate(model.id.clone()));
                            }
                            if rejected {
                                // Disabled-with-reason, always inline (A9).
                                ui.label(
                                    RichText::new("Rejected model bundles can't be activated")
                                        .text_style(caption())
                                        .color(TEXT_MUTE),
                                );
                            }
                        }
                        if model.managed
                            && ui
                                .add_enabled(
                                    !active && !state.busy,
                                    egui::Button::new(RichText::new("Delete").color(TEXT_DIM)),
                                )
                                .on_disabled_hover_text(if active {
                                    "Activate another model before deleting"
                                } else {
                                    "Wait for the current model-bundle operation to finish"
                                })
                                .clicked()
                        {
                            state.pending_delete = Some(model.id.clone());
                        }
                    });
                }
            }
        });
    if active {
        // Active card: 2px ember edge marker, not a brighter fill (§4.6).
        let rect = frame_response.response.rect;
        ui.painter().rect_filled(
            egui::Rect::from_min_size(
                rect.min + Vec2::new(0.0, 10.0),
                Vec2::new(2.0, rect.height() - 20.0),
            ),
            CornerRadius::same(1),
            EMBER,
        );
    }
    action
}

fn fact(ui: &mut egui::Ui, label: &str, value: &str) -> egui::Response {
    ui.label(RichText::new(label).text_style(caption()).color(TEXT_MUTE));
    let caution =
        value.contains("UNKNOWN") || value.contains("UNRECOGNIZED") || value == "future timestamp";
    ui.add(
        egui::Label::new(RichText::new(value).text_style(mono_s()).color(if caution {
            WARN
        } else {
            TEXT_DIM
        }))
        .wrap(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(name: &str, dimension: u32) -> ModelCard {
        ModelCard {
            id: name.into(),
            name: name.into(),
            managed: false,
            size_bytes: 10,
            modified_unix: 1,
            status: "clean".into(),
            status_detail: String::new(),
            dimension,
            grid: 128,
            in_channels: 4,
            out_channels: 2,
            max_steps: 64,
            epoch: 40,
            declared_epochs: 40,
            checkpoint_role: "fixed_final".into(),
            scenario: "obstacle".into(),
            source_digest: Some("abc".into()),
            checkpoint_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            physics_contract: "incompressible.external.fixed_body.v2".into(),
            authenticity_status: "verified".into(),
            publisher_key_id: Some("fixture-publisher".into()),
            publisher_key_sha256: Some(
                "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into(),
            ),
            release_sequence: Some(1),
            support: vec!["3D grid".into()],
            limitations: vec!["Fixed-body external flow only".into()],
            benchmark_report_hashes: vec![],
            unknown_fields: vec![],
            qualification_class: crate::engine::MODEL_QUALIFICATION_RESEARCH.into(),
        }
    }

    #[test]
    fn library_filter_combines_dimension_and_query() {
        let mut state = LibraryState {
            search: "wake".into(),
            dimension: DimensionFilter::ThreeD,
            ..Default::default()
        };
        assert!(visible(&model("wake-3d.reynmodel", 3), &state));
        assert!(!visible(&model("wake-2d.reynmodel", 2), &state));
        state.search = "free".into();
        assert!(!visible(&model("wake-3d.reynmodel", 3), &state));
    }

    #[test]
    fn health_filter_chips_partition_the_inventory() {
        let mut rejected = model("rejected.reynmodel", 3);
        rejected.status = "invalid".into();
        let mut review = model("review.reynmodel", 3);
        review.status = "review".into();
        let clean = model("clean.reynmodel", 3);

        let mut state = LibraryState {
            health: HealthFilter::Rejected,
            ..Default::default()
        };
        assert!(visible(&rejected, &state));
        assert!(!visible(&review, &state));
        assert!(!visible(&clean, &state));

        state.health = HealthFilter::NeedsReview;
        assert!(!visible(&rejected, &state));
        assert!(visible(&review, &state));
        assert!(!visible(&clean, &state));

        state.health = HealthFilter::All;
        assert!(visible(&rejected, &state));
        assert!(visible(&review, &state));
        assert!(visible(&clean, &state));

        let counts = health_counts(&[rejected, review, clean], &state);
        assert_eq!(counts, (3, 1, 1));
        assert_eq!(counted_filter_label("Needs review", 9), "Needs review · 9");
    }

    #[test]
    fn clean_contract_with_metadata_gaps_never_reads_as_healthy() {
        let mut incomplete = model("incomplete.reynmodel", 3);
        incomplete.source_digest = None;
        incomplete.limitations.clear();
        incomplete.unknown_fields = vec!["training_dataset_sha256".into()];

        assert_eq!(model_health(&incomplete), Health::NeedsReview);
        assert_eq!(status_label(&incomplete), "METADATA GAPS");
        let summary = metadata_gap_summary(&incomplete).unwrap();
        assert!(summary.contains("source digest"));
        assert!(summary.contains("limitations"));
        assert!(summary.contains("legacy fields"));

        incomplete.status = "invalid".into();
        assert_eq!(model_health(&incomplete), Health::Rejected);
        assert_eq!(status_label(&incomplete), "REJECTED");
    }

    #[test]
    fn search_includes_contract_provenance_and_declared_envelope() {
        let mut candidate = model("candidate.reynmodel", 3);
        candidate.physics_contract = "external.fixed-body.v2".into();
        candidate.support = vec!["bluff-body wake regime".into()];
        candidate.benchmark_report_hashes = vec!["b".repeat(64)];

        assert!(matches_search(&candidate, "fixed-body"));
        assert!(matches_search(&candidate, "wake regime"));
        assert!(matches_search(&candidate, &"b".repeat(16)));
        candidate.source_digest = None;
        assert!(matches_search(&candidate, "unknown"));
        assert!(matches_search(&candidate, "source digest"));
    }

    #[test]
    fn health_counts_follow_search_and_dimension_scope() {
        let mut clean_3d = model("wake-clean.reynmodel", 3);
        let mut review_3d = model("wake-review.reynmodel", 3);
        review_3d.source_digest = None;
        let rejected_2d = {
            let mut value = model("wake-rejected.reynmodel", 2);
            value.status = "invalid".into();
            value
        };
        clean_3d.scenario = "wake".into();

        let state = LibraryState {
            search: "wake".into(),
            dimension: DimensionFilter::ThreeD,
            ..Default::default()
        };
        assert_eq!(
            health_counts(&[clean_3d, review_3d, rejected_2d], &state),
            (2, 1, 0)
        );
    }

    #[test]
    fn byte_format_is_engineering_legible() {
        assert_eq!(format_bytes(0), "UNKNOWN");
        assert_eq!(format_bytes(10), "10 B");
        assert_eq!(format_bytes(2 * 1024), "2.0 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.00 GiB");
        assert!(
            compact_name("obstacle_physics_h64_mixed_delta_seed0_epoch40.reynmodel").contains('…')
        );
        assert_eq!(format_modified_at(0, 1_000), "UNKNOWN");
        assert_eq!(format_modified_at(2_000, 1_000), "future timestamp");
    }

    #[test]
    fn absent_metadata_is_rendered_as_unknown() {
        let mut incomplete = model("incomplete.reynmodel", 2);
        incomplete.grid = 0;
        incomplete.max_steps = 0;
        incomplete.epoch = 0;
        incomplete.declared_epochs = 0;
        incomplete.scenario = "unknown".into();
        assert!(support_summary(&incomplete).contains("UNKNOWN grid"));
        // QA L3: "hUNKNOWN" read as one token; UNKNOWN stays explicit but
        // legible and matches contract_line's wording.
        assert!(support_summary(&incomplete).contains("horizon UNKNOWN"));
        assert!(!support_summary(&incomplete).contains("hUNKNOWN"));
        assert!(contract_line(&incomplete).contains("UNKNOWN grid"));
        assert!(contract_line(&incomplete).contains("horizon UNKNOWN"));
        assert_eq!(epoch_text(&incomplete), "UNKNOWN");
        assert_eq!(known_text(&incomplete.scenario), "UNKNOWN");
        assert_eq!(model_health(&incomplete), Health::NeedsReview);
    }

    #[test]
    fn legacy_pickle_cards_are_explicitly_rejected() {
        let legacy = model("legacy.pth", 2);
        assert_eq!(model_health(&legacy), Health::Rejected);
        assert_eq!(status_label(&legacy), "REJECTED");
        assert!(!is_model_bundle_id(&legacy.id));
        assert!(TRUSTED_MODEL_CONVERSION_GUIDANCE.contains("never opened"));
    }

    #[test]
    fn unsigned_bundle_cards_are_explicitly_rejected() {
        let mut unsigned = model("unsigned.reynmodel", 2);
        unsigned.authenticity_status = "unverified".into();
        unsigned.status_detail = "required detached signature not found".into();
        assert_eq!(model_health(&unsigned), Health::Rejected);
        assert_eq!(status_label(&unsigned), "REJECTED");
        assert!(metadata_gaps(&unsigned).contains(&"publisher authenticity"));
    }

    #[test]
    fn card_titles_humanize_filenames_but_keep_the_exact_name_in_mono() {
        assert_eq!(
            humanize_name("direct_v1_latest.reynmodel"),
            "Direct v1 latest"
        );
        let long = "obstacle_physics_h64_mixed_delta_seed0_epoch40.reynmodel";
        assert_eq!(
            humanize_name(long),
            "Obstacle physics h64 mixed delta seed0 epoch40"
        );
        let compact = compact_name(long);
        assert!(compact.contains('…'));
        assert!(compact.ends_with(".reynmodel"));
        assert!(compact.chars().count() <= 42);
        assert_eq!(
            compact_name("direct_v1_latest.reynmodel"),
            "direct_v1_latest.reynmodel"
        );
    }

    #[test]
    fn digest_and_grid_helpers_preserve_narrow_layout_truth() {
        let digest = "abcdef0123456789".repeat(4);
        let compact = compact_digest(&digest);
        assert!(is_sha256(&digest));
        assert!(compact.contains('…'));
        assert!(compact.starts_with("abcdef"));
        assert!(compact.ends_with("6789"));
        assert_eq!(compact_digest("unknown"), "UNKNOWN");
        assert_eq!(card_columns(180.0), 1);
        assert_eq!(card_columns(640.0), 2);
        assert_eq!(card_columns(980.0), 3);
    }
}
