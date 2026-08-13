//! Reyn Studio — fully-native neural-CFD workbench (egui + wgpu), linked to a
//! Python inference engine. Entry point: sets up the window and theme.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod access;
mod app;
mod benchmark_evidence;
mod benchmark_export;
mod cad;
mod cad_3mf;
mod cad_field_ready;
mod cad_step;
mod credential_store;
mod engine;
mod engineering;
mod engineering_export;
mod engineering_section;
mod field2d;
mod flow;
mod fonts;
mod gpu;
mod library;
#[cfg(target_os = "macos")]
mod menubar;
mod painter;
pub mod project;
mod project_lifecycle;
mod report;
mod runtime;
mod settings;
mod signing;
mod theme;
mod units;
mod updater;
mod viewport;
mod vtk_export;

fn main() -> eframe::Result<()> {
    if access::print_build_contract_requested(std::env::args()) {
        println!("{}", access::build_contract_json());
        return Ok(());
    }

    if let Some(request) = signing::parse_verify_cli(std::env::args()) {
        let exit_code = match request.and_then(run_signature_verification) {
            Ok(outcome) => {
                println!(
                    "{:?} · {} · key {} · fingerprint {}",
                    outcome.status,
                    outcome.detail,
                    outcome.key_id.as_deref().unwrap_or("UNKNOWN"),
                    outcome
                        .key_fingerprint_sha256
                        .as_deref()
                        .unwrap_or("UNKNOWN")
                );
                if outcome.status.is_cryptographically_valid() {
                    0
                } else {
                    2
                }
            }
            Err(error) => {
                eprintln!("signature verification failed: {error}");
                2
            }
        };
        std::process::exit(exit_code);
    }

    // Dev/QA affordance (matches REYN_STUDIO_START_NAV): launch at an exact
    // window size, e.g. REYN_STUDIO_WINDOW=1100x700 for min-window audits.
    let forced_size = std::env::var("REYN_STUDIO_WINDOW").ok().and_then(|spec| {
        let (w, h) = spec.split_once('x')?;
        Some([w.parse().ok()?, h.parse().ok()?])
    });
    let inner_size = forced_size.unwrap_or([1440.0, 900.0]);

    let viewport = egui::ViewportBuilder::default()
        .with_inner_size(inner_size)
        .with_min_inner_size([1100.0, 700.0])
        .with_title("Reyn Studio");
    #[cfg(target_os = "macos")]
    let viewport = viewport
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);

    let options = eframe::NativeOptions {
        // A forced QA size must win over persisted window geometry. eframe
        // restores the stored "window" entry regardless of `persist_window`
        // (that flag only gates saving), so QA runs get a scratch storage
        // path — which also keeps audits from clobbering real window state.
        persist_window: forced_size.is_none(),
        persistence_path: forced_size
            .is_some()
            .then(|| std::env::temp_dir().join("reyn-studio-qa-storage.ron")),
        // macOS keeps the incumbent full-size content treatment. Windows uses
        // standard system decorations so move, resize, minimize, and close
        // behavior remain native and reachable.
        viewport,
        // wgpu backend = native Metal on macOS, Vulkan on Linux, DX12 on Windows.
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "Reyn Studio",
        options,
        Box::new(|cc| {
            fonts::install(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(access::RootApp::new(cc)))
        }),
    )
}

fn run_signature_verification(
    request: signing::VerifyCliRequest,
) -> Result<signing::VerificationOutcome, String> {
    let report = std::fs::read_to_string(&request.report)
        .map_err(|error| format!("could not read {}: {error}", request.report.display()))?;
    let signature = std::fs::read_to_string(&request.signature)
        .map_err(|error| format!("could not read {}: {error}", request.signature.display()))?;
    let artifact = signing::SignedEvidenceArtifact::from_json(&signature)
        .map_err(|error| error.to_string())?;
    let policy = signing::VerificationPolicy::new(
        request.trusted_fingerprints,
        request.revoked_fingerprints,
    );
    Ok(benchmark_export::verify_report_signature(
        &report, &artifact, &policy,
    ))
}
