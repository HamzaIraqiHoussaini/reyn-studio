//! Invitation-only access gate for official preview binaries.
//!
//! Public source builds remain buildable under their source license. The gate
//! is compiled into an official binary only when `REYN_ACCESS_REQUIRED=1` and
//! `REYN_ACCESS_ENDPOINT` are set at build time. Credentials and server secrets
//! never enter the binary or repository.

use crate::{
    app::{AppBootstrap, ReynApp},
    credential_store::{NativeSessionCredentialStore, SessionCredentialStore},
    theme, updater,
};
use serde::{Deserialize, Serialize};
use std::{
    rc::Rc,
    sync::mpsc::{self, Receiver},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::{Zeroize, Zeroizing};

const TERMS_VERSION: &str = "1.0";
const PRIVACY_VERSION: &str = "1.0";
const RESPONSE_LIMIT_BYTES: u64 = 32 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SESSION_LIFETIME_SECONDS: u64 = 24 * 60 * 60;
const SESSION_REVALIDATION_SECONDS: u64 = 15 * 60;

fn access_required() -> bool {
    option_env!("REYN_ACCESS_REQUIRED") == Some("1")
}

fn configured_endpoint() -> Result<&'static str, LoginFailure> {
    let endpoint = option_env!("REYN_ACCESS_ENDPOINT").ok_or(LoginFailure::Configuration(
        "This official preview was built without an access endpoint.",
    ))?;
    validate_endpoint(endpoint)?;
    Ok(endpoint)
}

fn validate_endpoint(endpoint: &str) -> Result<(), LoginFailure> {
    if !endpoint.starts_with("https://") {
        return Err(LoginFailure::Configuration(
            "The preview access endpoint must use HTTPS.",
        ));
    }
    if endpoint.contains('@') || endpoint.contains('#') || endpoint.contains(char::is_whitespace) {
        return Err(LoginFailure::Configuration(
            "The preview access endpoint is malformed.",
        ));
    }
    Ok(())
}

pub fn print_build_contract_requested(args: impl IntoIterator<Item = String>) -> bool {
    let mut args = args.into_iter();
    let _executable = args.next();
    args.next().as_deref() == Some("--print-access-contract") && args.next().is_none()
}

pub fn build_contract_json() -> String {
    let endpoint =
        option_env!("REYN_ACCESS_ENDPOINT").filter(|endpoint| validate_endpoint(endpoint).is_ok());
    serde_json::json!({
        "schema": "com.reyn.studio.preview-access/1",
        "required": access_required(),
        "endpoint": endpoint,
        "terms_version": TERMS_VERSION,
        "privacy_version": PRIVACY_VERSION,
    })
    .to_string()
}

pub struct RootApp {
    studio: Option<ReynApp>,
    bootstrap: AppBootstrap,
    gate: Option<AccessGate>,
    updater: updater::Updater,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StudioLifecycleAction {
    Keep,
    Start,
    Drop,
}

fn studio_lifecycle_action(
    gate_required: bool,
    access_granted: bool,
    studio_running: bool,
) -> StudioLifecycleAction {
    let should_run = !gate_required || access_granted;
    match (should_run, studio_running) {
        (true, false) => StudioLifecycleAction::Start,
        (false, true) => StudioLifecycleAction::Drop,
        _ => StudioLifecycleAction::Keep,
    }
}

impl RootApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let gate_required = access_required();
        let bootstrap = AppBootstrap::prepare(cc);
        let updater = updater::Updater::new(cc.egui_ctx.clone());
        if updater::automatic_checks_enabled_from_disk() {
            updater.check();
        }
        let studio = (!gate_required).then(|| bootstrap.start_with_updater(updater.clone()));
        Self {
            studio,
            bootstrap,
            gate: gate_required.then(AccessGate::new),
            updater,
        }
    }
}

impl eframe::App for RootApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let gate_required = self.gate.is_some();
        if let Some(gate) = self.gate.as_mut() {
            let now_utc_unix = unix_now().unwrap_or(u64::MAX);
            gate.tick(ui.ctx().clone(), now_utc_unix);
            if let Some(delay) = gate.time_until_expiry(now_utc_unix) {
                ui.ctx().request_repaint_after(delay);
            }
        }
        let access_granted = self.gate.as_ref().is_none_or(AccessGate::is_granted);
        match studio_lifecycle_action(gate_required, access_granted, self.studio.is_some()) {
            StudioLifecycleAction::Start => {
                self.studio = Some(self.bootstrap.start_with_updater(self.updater.clone()))
            }
            StudioLifecycleAction::Drop => {
                // Dropping ReynApp first drops EngineHandle, interrupts an
                // in-flight sidecar request, joins its worker, and terminates
                // the child before any login UI is shown again.
                self.studio.take();
            }
            StudioLifecycleAction::Keep => {}
        }
        if let Some(studio) = self.studio.as_mut() {
            eframe::App::ui(studio, ui, frame);
            if studio.take_sign_out_request() {
                if let Some(gate) = self.gate.as_mut() {
                    gate.sign_out(ui.ctx().clone());
                }
                self.studio.take();
            }
        } else if let Some(gate) = self.gate.as_mut() {
            egui::Panel::top("access.update-banner")
                .show(ui, |ui| updater::show_compact_banner(ui, &self.updater));
            gate.ui(ui);
        }
    }
}

struct AccessGate {
    username: String,
    password: String,
    terms_accepted: bool,
    state: GateState,
    pending: Option<Receiver<PendingAccessResult>>,
    requested_initial_focus: bool,
    store_notice: Option<String>,
    store: Rc<dyn SessionCredentialStore>,
}

enum GateState {
    Ready,
    Restoring(Session),
    CheckingCredentials,
    Validating(Session),
    Granted(Session),
    RetryValidation(Session, LoginFailure),
    SigningOut(Session),
    RetrySignOut(Session, LoginFailure),
    Failed(LoginFailure),
}

struct Session {
    token: Zeroizing<String>,
    expires_at_utc_unix: u64,
    next_validation_utc_unix: u64,
}

#[derive(Deserialize, Serialize)]
struct StoredSession {
    token: String,
    expires_at_utc_unix: u64,
    terms_version: String,
    privacy_version: String,
}

enum PendingAccessResult {
    Login(Result<Session, LoginFailure>),
    Validate(Result<ValidatedSession, LoginFailure>),
    Logout(Result<(), LoginFailure>),
}

struct ValidatedSession {
    expires_at_utc_unix: u64,
}

impl Session {
    fn is_valid_at(&self, now_utc_unix: u64) -> bool {
        !self.token.is_empty() && self.expires_at_utc_unix > now_utc_unix
    }

    fn zeroize(&mut self) {
        self.token.zeroize();
    }

    fn validation_due(&self, now_utc_unix: u64) -> bool {
        now_utc_unix >= self.next_validation_utc_unix
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoginFailure {
    Configuration(&'static str),
    InvalidCredentials,
    RateLimited,
    PolicyOutdated,
    InvalidSession,
    SessionExpired,
    Network,
    Service,
}

impl LoginFailure {
    fn message(self) -> &'static str {
        match self {
            Self::Configuration(message) => message,
            Self::InvalidCredentials => "That username or password was not accepted.",
            Self::RateLimited => "Too many attempts. Wait one minute, then try again.",
            Self::PolicyOutdated => {
                "This build uses an outdated legal version. Download the latest official build."
            }
            Self::InvalidSession => {
                "Your saved access is no longer valid. Sign in again to continue."
            }
            Self::SessionExpired => "Your preview session expired. Sign in again to continue.",
            Self::Network => {
                "Reyn could not reach the access service. Check your connection and try again."
            }
            Self::Service => "The access service is unavailable. Try again shortly.",
        }
    }
}

impl AccessGate {
    fn new() -> Self {
        Self::new_with_store(Rc::new(NativeSessionCredentialStore))
    }

    fn new_with_store(store: Rc<dyn SessionCredentialStore>) -> Self {
        let mut store_notice = None;
        let state = if let Err(error) = configured_endpoint() {
            GateState::Failed(error)
        } else {
            match load_stored_session(store.as_ref()) {
                Ok(Some(session)) => GateState::Restoring(session),
                Ok(None) => GateState::Ready,
                Err(error) => {
                    let _ = store.delete();
                    store_notice = Some(format!(
                        "Saved access could not be read securely: {error:?}"
                    ));
                    GateState::Ready
                }
            }
        };
        Self {
            username: String::new(),
            password: String::new(),
            terms_accepted: false,
            state,
            pending: None,
            requested_initial_focus: false,
            store_notice,
            store,
        }
    }

    fn is_granted(&self) -> bool {
        matches!(&self.state, GateState::Granted(session) if !session.token.is_empty())
    }

    fn tick(&mut self, context: egui::Context, now_utc_unix: u64) {
        self.poll_at(now_utc_unix);
        if self.pending.is_some() {
            return;
        }
        let state = std::mem::replace(&mut self.state, GateState::Ready);
        match state {
            GateState::Restoring(session) => self.start_validation(session, context),
            GateState::Granted(mut session) if !session.is_valid_at(now_utc_unix) => {
                session.zeroize();
                let _ = self.store.delete();
                self.reset_credentials();
                self.state = GateState::Failed(LoginFailure::SessionExpired);
            }
            GateState::Granted(session) if session.validation_due(now_utc_unix) => {
                self.start_validation(session, context)
            }
            other => self.state = other,
        }
    }

    fn time_until_expiry(&self, now_utc_unix: u64) -> Option<Duration> {
        match &self.state {
            GateState::Granted(session) if session.is_valid_at(now_utc_unix) => {
                Some(Duration::from_secs(
                    session
                        .expires_at_utc_unix
                        .min(session.next_validation_utc_unix)
                        .saturating_sub(now_utc_unix),
                ))
            }
            _ => None,
        }
    }

    fn submit(&mut self, context: egui::Context) {
        if self.pending.is_some()
            || self.username.trim().is_empty()
            || self.password.is_empty()
            || !self.terms_accepted
        {
            return;
        }

        let endpoint = match configured_endpoint() {
            Ok(endpoint) => endpoint.to_owned(),
            Err(error) => {
                self.state = GateState::Failed(error);
                return;
            }
        };
        let mut request = LoginRequest {
            username: self.username.trim().to_owned(),
            password: self.password.clone(),
            terms_version: TERMS_VERSION,
            privacy_version: PRIVACY_VERSION,
            client: ClientInfo {
                app_version: env!("CARGO_PKG_VERSION"),
                platform: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
            },
        };
        self.password.zeroize();

        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        self.state = GateState::CheckingCredentials;
        std::thread::Builder::new()
            .name("reyn-preview-access".into())
            .spawn(move || {
                let result = authenticate(&endpoint, &request);
                request.password.zeroize();
                let _ = tx.send(PendingAccessResult::Login(result));
                context.request_repaint();
            })
            .expect("preview access worker thread should start");
    }

    fn start_validation(&mut self, session: Session, context: egui::Context) {
        let endpoint = match configured_endpoint() {
            Ok(endpoint) => endpoint.to_owned(),
            Err(error) => {
                self.state = GateState::RetryValidation(session, error);
                return;
            }
        };
        let token = session.token.to_string();
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        self.state = GateState::Validating(session);
        std::thread::Builder::new()
            .name("reyn-preview-session-validation".into())
            .spawn(move || {
                let result = validate_session(&endpoint, &token);
                let mut token = token;
                token.zeroize();
                let _ = tx.send(PendingAccessResult::Validate(result));
                context.request_repaint();
            })
            .expect("preview session validation thread should start");
    }

    fn poll_at(&mut self, now_utc_unix: u64) {
        let Some(receiver) = self.pending.as_ref() else {
            return;
        };
        let Ok(result) = receiver.try_recv() else {
            return;
        };
        self.pending = None;
        self.state = match result {
            PendingAccessResult::Login(Ok(session)) if session.is_valid_at(now_utc_unix) => {
                self.username.zeroize();
                self.password.zeroize();
                self.save_session(&session);
                GateState::Granted(session)
            }
            PendingAccessResult::Login(Ok(mut session)) => {
                session.zeroize();
                GateState::Failed(LoginFailure::SessionExpired)
            }
            PendingAccessResult::Login(Err(error)) => GateState::Failed(error),
            PendingAccessResult::Validate(result) => {
                let state = std::mem::replace(&mut self.state, GateState::Ready);
                let mut session = match state {
                    GateState::Validating(session) => session,
                    other => {
                        self.state = other;
                        return;
                    }
                };
                match result {
                    Ok(validated) if validated.expires_at_utc_unix > now_utc_unix => {
                        session.expires_at_utc_unix = validated.expires_at_utc_unix;
                        session.next_validation_utc_unix =
                            now_utc_unix.saturating_add(SESSION_REVALIDATION_SECONDS);
                        self.save_session(&session);
                        GateState::Granted(session)
                    }
                    Err(error @ (LoginFailure::Network | LoginFailure::Service)) => {
                        GateState::RetryValidation(session, error)
                    }
                    _ => {
                        session.zeroize();
                        let _ = self.store.delete();
                        self.reset_credentials();
                        GateState::Failed(LoginFailure::InvalidSession)
                    }
                }
            }
            PendingAccessResult::Logout(result) => {
                let state = std::mem::replace(&mut self.state, GateState::Ready);
                let mut session = match state {
                    GateState::SigningOut(session) => session,
                    other => {
                        self.state = other;
                        return;
                    }
                };
                match result {
                    Ok(()) => {
                        session.zeroize();
                        let _ = self.store.delete();
                        self.reset_credentials();
                        GateState::Ready
                    }
                    Err(error) => GateState::RetrySignOut(session, error),
                }
            }
        };
    }

    fn save_session(&mut self, session: &Session) {
        let stored = StoredSession {
            token: session.token.to_string(),
            expires_at_utc_unix: session.expires_at_utc_unix,
            terms_version: TERMS_VERSION.into(),
            privacy_version: PRIVACY_VERSION.into(),
        };
        let mut encoded = match serde_json::to_vec(&stored) {
            Ok(encoded) => encoded,
            Err(_) => {
                self.store_notice = Some("Saved access could not be encoded.".into());
                return;
            }
        };
        if let Err(error) = self.store.save(&encoded) {
            self.store_notice = Some(format!(
                "Access is active for this run, but the native credential store failed: {error:?}"
            ));
        } else {
            self.store_notice = None;
        }
        encoded.zeroize();
    }

    fn reset_credentials(&mut self) {
        self.password.zeroize();
        self.username.zeroize();
        self.terms_accepted = false;
        self.requested_initial_focus = false;
    }

    fn retry_saved_session(&mut self, context: egui::Context) {
        let state = std::mem::replace(&mut self.state, GateState::Ready);
        match state {
            GateState::RetryValidation(session, _) => self.start_validation(session, context),
            other => self.state = other,
        }
    }

    fn sign_out(&mut self, context: egui::Context) {
        let state = std::mem::replace(&mut self.state, GateState::Ready);
        let session = match state {
            GateState::Granted(session)
            | GateState::Restoring(session)
            | GateState::Validating(session)
            | GateState::RetryValidation(session, _)
            | GateState::RetrySignOut(session, _) => session,
            _ => {
                let _ = self.store.delete();
                self.state = GateState::Ready;
                return;
            }
        };
        self.start_logout(session, context);
    }

    fn retry_sign_out(&mut self, context: egui::Context) {
        let state = std::mem::replace(&mut self.state, GateState::Ready);
        match state {
            GateState::RetrySignOut(session, _) => self.start_logout(session, context),
            other => self.state = other,
        }
    }

    fn start_logout(&mut self, session: Session, context: egui::Context) {
        let endpoint = configured_endpoint().ok().map(str::to_owned);
        let token = session.token.to_string();
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        self.state = GateState::SigningOut(session);
        std::thread::Builder::new()
            .name("reyn-preview-session-logout".into())
            .spawn(move || {
                let result = endpoint
                    .as_deref()
                    .ok_or(LoginFailure::Service)
                    .and_then(|endpoint| logout_session(endpoint, &token));
                let mut token = token;
                token.zeroize();
                let _ = tx.send(PendingAccessResult::Logout(result));
                context.request_repaint();
            })
            .expect("preview logout thread should start");
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        self.poll_at(unix_now().unwrap_or(u64::MAX));
        ui.painter()
            .rect_filled(ui.max_rect(), egui::CornerRadius::ZERO, theme::BG_0);

        let panel_width = ui.available_width().min(520.0);
        let panel_height = ui.available_height().min(610.0);
        let panel_rect = egui::Rect::from_center_size(
            ui.max_rect().center(),
            egui::vec2(panel_width, panel_height),
        );

        ui.scope_builder(egui::UiBuilder::new().max_rect(panel_rect), |ui| {
            egui::Frame::new()
                .fill(theme::BG_2)
                .stroke(egui::Stroke::new(1.0, theme::HAIRLINE))
                .corner_radius(egui::CornerRadius::same(theme::R2))
                .inner_margin(egui::Margin::same(30))
                .show(ui, |ui| {
                    ui.set_min_width((panel_width - 60.0).max(280.0));
                    ui.label(theme::overline_text("Invitation-only preview"));
                    ui.add_space(12.0);
                    ui.label(theme::display_text("Unlock Reyn Studio"));
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(
                            "Use the credentials supplied with the YC evaluation package.",
                        )
                        .text_style(theme::body())
                        .color(theme::TEXT_DIM),
                    );
                    ui.add_space(24.0);

                    if matches!(
                        self.state,
                        GateState::Restoring(_) | GateState::Validating(_)
                    ) {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new("Validating saved access…")
                                    .text_style(theme::body())
                                    .color(theme::TEXT_DIM),
                            );
                        });
                        ui.add_space(12.0);
                        ui.label(
                            egui::RichText::new(
                                "Your username and password are not stored. Reyn is checking the revocable session held by the native credential vault.",
                            )
                            .text_style(theme::caption())
                            .color(theme::TEXT_MUTE),
                        );
                        return;
                    }
                    if matches!(self.state, GateState::SigningOut(_)) {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new("Revoking saved access…")
                                    .text_style(theme::body())
                                    .color(theme::TEXT_DIM),
                            );
                        });
                        return;
                    }
                    if let GateState::RetrySignOut(_, error) = &self.state {
                        let error = *error;
                        ui.label(
                            egui::RichText::new(error.message())
                                .text_style(theme::body())
                                .color(theme::TEXT),
                        );
                        ui.add_space(12.0);
                        if ui.button("Retry sign out").clicked() {
                            self.retry_sign_out(ui.ctx().clone());
                        }
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(
                                "Reyn keeps the revocable session in the native vault until the access service confirms sign out.",
                            )
                            .text_style(theme::caption())
                            .color(theme::TEXT_MUTE),
                        );
                        return;
                    }
                    if let GateState::RetryValidation(_, error) = &self.state {
                        let error = *error;
                        ui.label(
                            egui::RichText::new(error.message())
                                .text_style(theme::body())
                                .color(theme::TEXT),
                        );
                        ui.add_space(12.0);
                        if ui.button("Retry saved access").clicked() {
                            self.retry_saved_session(ui.ctx().clone());
                        }
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(
                                "Your saved session is still held securely. Credentials are not requested for a temporary service failure.",
                            )
                            .text_style(theme::caption())
                            .color(theme::TEXT_MUTE),
                        );
                        return;
                    }

                    ui.label(
                        egui::RichText::new("Username")
                            .text_style(theme::body_strong())
                            .color(theme::TEXT),
                    );
                    let username = ui.add_sized(
                        [ui.available_width(), 38.0],
                        egui::TextEdit::singleline(&mut self.username)
                            .hint_text("Preview username")
                            .id_salt("reyn-access-username"),
                    );
                    if !self.requested_initial_focus {
                        username.request_focus();
                        self.requested_initial_focus = true;
                    }

                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new("Password")
                            .text_style(theme::body_strong())
                            .color(theme::TEXT),
                    );
                    ui.add_sized(
                        [ui.available_width(), 38.0],
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .hint_text("Preview password")
                            .id_salt("reyn-access-password"),
                    );

                    ui.add_space(16.0);
                    ui.checkbox(
                        &mut self.terms_accepted,
                        format!(
                            "I accept Terms v{TERMS_VERSION} and acknowledge Privacy v{PRIVACY_VERSION}."
                        ),
                    );
                    ui.horizontal_wrapped(|ui| {
                        ui.hyperlink_to("Read Terms", "https://reynflow.com/legal/terms/");
                        ui.label(
                            egui::RichText::new("·")
                                .text_style(theme::caption())
                                .color(theme::TEXT_MUTE),
                        );
                        ui.hyperlink_to(
                            "Read Privacy Policy",
                            "https://reynflow.com/legal/privacy/",
                        );
                    });

                    if let GateState::Failed(error) = self.state {
                        ui.add_space(16.0);
                        egui::Frame::new()
                            .fill(theme::tint_fill(theme::DANGER))
                            .stroke(egui::Stroke::new(
                                1.0,
                                theme::tint_hairline(theme::DANGER),
                            ))
                            .corner_radius(egui::CornerRadius::same(theme::R1))
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(format!("Could not unlock · {}", error.message()))
                                        .text_style(theme::body())
                                        .color(theme::TEXT),
                                );
                            });
                    }

                    ui.add_space(20.0);
                    let checking = matches!(self.state, GateState::CheckingCredentials);
                    let can_submit = !checking
                        && !self.username.trim().is_empty()
                        && !self.password.is_empty()
                        && self.terms_accepted;
                    let label = if checking {
                        "Checking access…"
                    } else {
                        "Unlock Studio"
                    };
                    let button = egui::Button::new(
                        egui::RichText::new(label)
                            .text_style(theme::body_strong())
                            .color(theme::ON_EMBER),
                    )
                    .fill(theme::EMBER)
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same(theme::R1))
                    .min_size(egui::vec2(ui.available_width(), 42.0));
                    let clicked = ui.add_enabled(can_submit, button).clicked();
                    let enter = ui.input(|input| input.key_pressed(egui::Key::Enter));
                    if clicked || (enter && can_submit) {
                        self.submit(ui.ctx().clone());
                    }

                    if checking {
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new("Contacting the encrypted access service")
                                    .text_style(theme::caption())
                                    .color(theme::TEXT_MUTE),
                            );
                        });
                    }
                    if let Some(notice) = &self.store_notice {
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(notice)
                                .text_style(theme::caption())
                                .color(theme::WARN),
                        );
                    }

                    ui.add_space(18.0);
                    ui.separator();
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new(
                            "Credentials are checked once over HTTPS. Only a revocable session token is kept in Keychain or Credential Manager.",
                        )
                        .text_style(theme::caption())
                        .color(theme::TEXT_MUTE),
                    );
                });
        });
    }
}

#[derive(Serialize)]
struct LoginRequest {
    username: String,
    password: String,
    terms_version: &'static str,
    privacy_version: &'static str,
    client: ClientInfo,
}

#[derive(Serialize)]
struct ClientInfo {
    app_version: &'static str,
    platform: &'static str,
    architecture: &'static str,
}

#[derive(Deserialize)]
struct LoginResponse {
    ok: bool,
    session_token: Option<String>,
    expires_at_utc_unix: Option<u64>,
    terms_version: String,
    privacy_version: String,
}

#[derive(Deserialize)]
struct SessionValidationResponse {
    ok: bool,
    expires_at_utc_unix: Option<u64>,
    terms_version: String,
    privacy_version: String,
}

fn unix_now() -> Result<u64, LoginFailure> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| LoginFailure::Service)
}

fn load_stored_session(
    store: &(impl SessionCredentialStore + ?Sized),
) -> Result<Option<Session>, crate::credential_store::StoreError> {
    let Some(mut encoded) = store.load()? else {
        return Ok(None);
    };
    let parsed = serde_json::from_slice::<StoredSession>(&encoded);
    encoded.zeroize();
    let Ok(mut stored) = parsed else {
        let _ = store.delete();
        return Ok(None);
    };
    let now_utc_unix = unix_now().unwrap_or(u64::MAX);
    let token_valid = (32..=256).contains(&stored.token.len())
        && stored
            .token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    let policy_valid =
        stored.terms_version == TERMS_VERSION && stored.privacy_version == PRIVACY_VERSION;
    let expiry_valid = stored.expires_at_utc_unix > now_utc_unix
        && stored.expires_at_utc_unix.saturating_sub(now_utc_unix) <= MAX_SESSION_LIFETIME_SECONDS;
    if !token_valid || !policy_valid || !expiry_valid {
        stored.token.zeroize();
        let _ = store.delete();
        return Ok(None);
    }
    Ok(Some(Session {
        token: Zeroizing::new(stored.token),
        expires_at_utc_unix: stored.expires_at_utc_unix,
        next_validation_utc_unix: now_utc_unix,
    }))
}

fn session_from_response(body: LoginResponse, now_utc_unix: u64) -> Result<Session, LoginFailure> {
    if !body.ok {
        return Err(LoginFailure::Service);
    }
    if body.terms_version != TERMS_VERSION || body.privacy_version != PRIVACY_VERSION {
        return Err(LoginFailure::PolicyOutdated);
    }
    let token = Zeroizing::new(body.session_token.ok_or(LoginFailure::Service)?);
    let expires_at_utc_unix = body.expires_at_utc_unix.ok_or(LoginFailure::Service)?;
    if !(32..=256).contains(&token.len())
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        || expires_at_utc_unix.saturating_sub(now_utc_unix) > MAX_SESSION_LIFETIME_SECONDS
    {
        return Err(LoginFailure::Service);
    }
    if expires_at_utc_unix <= now_utc_unix {
        return Err(LoginFailure::SessionExpired);
    }
    Ok(Session {
        token,
        expires_at_utc_unix,
        next_validation_utc_unix: now_utc_unix.saturating_add(SESSION_REVALIDATION_SECONDS),
    })
}

fn authenticate(endpoint: &str, request: &LoginRequest) -> Result<Session, LoginFailure> {
    validate_endpoint(endpoint)?;
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(REQUEST_TIMEOUT))
        .http_status_as_error(false)
        .user_agent(format!(
            "ReynStudio/{} ({}; {})",
            request.client.app_version, request.client.platform, request.client.architecture
        ))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .build(),
        )
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut response = agent
        .post(endpoint)
        .header("Accept", "application/json")
        .send_json(request)
        .map_err(|_| LoginFailure::Network)?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(match status {
            401 => LoginFailure::InvalidCredentials,
            409 | 426 => LoginFailure::PolicyOutdated,
            429 => LoginFailure::RateLimited,
            500..=599 => LoginFailure::Service,
            _ => LoginFailure::Service,
        });
    }

    let body = response
        .body_mut()
        .with_config()
        .limit(RESPONSE_LIMIT_BYTES)
        .read_json::<LoginResponse>()
        .map_err(|_| LoginFailure::Service)?;
    session_from_response(body, unix_now()?)
}

fn validate_session(endpoint: &str, token: &str) -> Result<ValidatedSession, LoginFailure> {
    validate_endpoint(endpoint)?;
    let agent = access_agent();
    let mut response = agent
        .post(&format!("{endpoint}/validate"))
        .header("Accept", "application/json")
        .header("Authorization", &format!("Bearer {token}"))
        .send_empty()
        .map_err(|_| LoginFailure::Network)?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(match status {
            401 => LoginFailure::InvalidSession,
            409 | 426 => LoginFailure::PolicyOutdated,
            500..=599 => LoginFailure::Service,
            _ => LoginFailure::Service,
        });
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(RESPONSE_LIMIT_BYTES)
        .read_json::<SessionValidationResponse>()
        .map_err(|_| LoginFailure::Service)?;
    if !body.ok || body.terms_version != TERMS_VERSION || body.privacy_version != PRIVACY_VERSION {
        return Err(LoginFailure::PolicyOutdated);
    }
    let expires_at_utc_unix = body.expires_at_utc_unix.ok_or(LoginFailure::Service)?;
    let now_utc_unix = unix_now()?;
    if expires_at_utc_unix <= now_utc_unix {
        return Err(LoginFailure::SessionExpired);
    }
    if expires_at_utc_unix.saturating_sub(now_utc_unix) > MAX_SESSION_LIFETIME_SECONDS {
        return Err(LoginFailure::Service);
    }
    Ok(ValidatedSession {
        expires_at_utc_unix,
    })
}

fn logout_session(endpoint: &str, token: &str) -> Result<(), LoginFailure> {
    validate_endpoint(endpoint)?;
    let response = access_agent()
        .post(&format!("{endpoint}/logout"))
        .header("Accept", "application/json")
        .header("Authorization", &format!("Bearer {token}"))
        .send_empty()
        .map_err(|_| LoginFailure::Network)?;
    match response.status().as_u16() {
        200 | 401 => Ok(()),
        500..=599 => Err(LoginFailure::Service),
        _ => Err(LoginFailure::Service),
    }
}

fn access_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(REQUEST_TIMEOUT))
        .http_status_as_error(false)
        .user_agent(format!(
            "ReynStudio/{} ({}; {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .build(),
        )
        .build();
    ureq::Agent::new_with_config(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct MemorySessionStore {
        secret: RefCell<Option<Vec<u8>>>,
        deletes: RefCell<usize>,
    }

    impl SessionCredentialStore for MemorySessionStore {
        fn load(&self) -> Result<Option<Vec<u8>>, crate::credential_store::StoreError> {
            Ok(self.secret.borrow().clone())
        }

        fn save(&self, secret: &[u8]) -> Result<(), crate::credential_store::StoreError> {
            *self.secret.borrow_mut() = Some(secret.to_vec());
            Ok(())
        }

        fn delete(&self) -> Result<(), crate::credential_store::StoreError> {
            *self.secret.borrow_mut() = None;
            *self.deletes.borrow_mut() += 1;
            Ok(())
        }
    }

    fn test_session(expires_at_utc_unix: u64, next_validation_utc_unix: u64) -> Session {
        Session {
            token: Zeroizing::new("s".repeat(43)),
            expires_at_utc_unix,
            next_validation_utc_unix,
        }
    }

    fn gate_with_state(
        state: GateState,
        store: Rc<MemorySessionStore>,
        pending: Option<Receiver<PendingAccessResult>>,
    ) -> AccessGate {
        AccessGate {
            username: "invitee".to_owned(),
            password: "not-retained".to_owned(),
            terms_accepted: true,
            state,
            pending,
            requested_initial_focus: true,
            store_notice: None,
            store,
        }
    }

    #[test]
    fn secure_store_restores_only_current_valid_sessions() {
        let store = MemorySessionStore::default();
        let now = unix_now().unwrap();
        let record = StoredSession {
            token: "s".repeat(43),
            expires_at_utc_unix: now + 3600,
            terms_version: TERMS_VERSION.into(),
            privacy_version: PRIVACY_VERSION.into(),
        };
        store
            .save(&serde_json::to_vec(&record).unwrap())
            .expect("store fixture");
        let restored = load_stored_session(&store).unwrap().expect("valid session");
        assert_eq!(restored.expires_at_utc_unix, now + 3600);
        assert_eq!(restored.next_validation_utc_unix, now);

        store.save(b"not-json").unwrap();
        assert!(load_stored_session(&store).unwrap().is_none());
        assert_eq!(*store.deletes.borrow(), 1);
        assert!(store.secret.borrow().is_none());
    }

    #[test]
    fn secure_store_deletes_expired_malformed_and_legal_version_mismatches() {
        let store = MemorySessionStore::default();
        let now = unix_now().unwrap();
        for record in [
            StoredSession {
                token: "short".into(),
                expires_at_utc_unix: now + 60,
                terms_version: TERMS_VERSION.into(),
                privacy_version: PRIVACY_VERSION.into(),
            },
            StoredSession {
                token: "s".repeat(43),
                expires_at_utc_unix: now,
                terms_version: TERMS_VERSION.into(),
                privacy_version: PRIVACY_VERSION.into(),
            },
            StoredSession {
                token: "s".repeat(43),
                expires_at_utc_unix: now + 60,
                terms_version: "obsolete".into(),
                privacy_version: PRIVACY_VERSION.into(),
            },
        ] {
            store
                .save(&serde_json::to_vec(&record).unwrap())
                .expect("store invalid fixture");
            assert!(load_stored_session(&store).unwrap().is_none());
            assert!(store.secret.borrow().is_none());
        }
        assert_eq!(*store.deletes.borrow(), 3);
    }

    #[test]
    fn first_login_persists_a_session_that_restart_can_restore() {
        let now = unix_now().unwrap();
        let store = Rc::new(MemorySessionStore::default());
        let (sender, receiver) = mpsc::channel();
        sender
            .send(PendingAccessResult::Login(Ok(test_session(
                now + 3_600,
                now + SESSION_REVALIDATION_SECONDS,
            ))))
            .expect("queue login");
        let mut gate = gate_with_state(
            GateState::CheckingCredentials,
            store.clone(),
            Some(receiver),
        );

        gate.poll_at(now);

        assert!(gate.is_granted());
        assert!(gate.username.is_empty());
        assert!(gate.password.is_empty());
        assert!(store.secret.borrow().is_some());
        let restored = load_stored_session(store.as_ref())
            .expect("read saved session")
            .expect("restore saved session");
        assert_eq!(restored.expires_at_utc_unix, now + 3_600);
        assert_eq!(restored.token.len(), 43);
    }

    #[test]
    fn validation_failure_retries_without_credentials_but_revocation_deletes_session() {
        let now = 1_000;
        let store = Rc::new(MemorySessionStore::default());
        store.save(b"vault-entry").expect("store fixture");
        let (network_tx, network_rx) = mpsc::channel();
        network_tx
            .send(PendingAccessResult::Validate(Err(LoginFailure::Network)))
            .expect("queue network failure");
        let mut gate = gate_with_state(
            GateState::Validating(test_session(now + 3_600, now)),
            store.clone(),
            Some(network_rx),
        );

        gate.poll_at(now);

        assert!(matches!(
            gate.state,
            GateState::RetryValidation(_, LoginFailure::Network)
        ));
        assert!(store.secret.borrow().is_some());
        assert_eq!(*store.deletes.borrow(), 0);

        let session = match std::mem::replace(&mut gate.state, GateState::Ready) {
            GateState::RetryValidation(session, _) => session,
            _ => unreachable!("retry state"),
        };
        let (revoked_tx, revoked_rx) = mpsc::channel();
        revoked_tx
            .send(PendingAccessResult::Validate(Err(
                LoginFailure::InvalidSession,
            )))
            .expect("queue revocation");
        gate.state = GateState::Validating(session);
        gate.pending = Some(revoked_rx);
        gate.poll_at(now);

        assert!(matches!(
            gate.state,
            GateState::Failed(LoginFailure::InvalidSession)
        ));
        assert!(store.secret.borrow().is_none());
        assert_eq!(*store.deletes.borrow(), 1);
    }

    #[test]
    fn logout_keeps_the_vault_entry_until_server_revocation_succeeds() {
        let now = 1_000;
        let store = Rc::new(MemorySessionStore::default());
        store.save(b"vault-entry").expect("store fixture");
        let (failure_tx, failure_rx) = mpsc::channel();
        failure_tx
            .send(PendingAccessResult::Logout(Err(LoginFailure::Service)))
            .expect("queue logout failure");
        let mut gate = gate_with_state(
            GateState::SigningOut(test_session(now + 3_600, now + 60)),
            store.clone(),
            Some(failure_rx),
        );

        gate.poll_at(now);

        assert!(matches!(
            gate.state,
            GateState::RetrySignOut(_, LoginFailure::Service)
        ));
        assert!(store.secret.borrow().is_some());
        assert_eq!(*store.deletes.borrow(), 0);

        let session = match std::mem::replace(&mut gate.state, GateState::Ready) {
            GateState::RetrySignOut(session, _) => session,
            _ => unreachable!("retry sign-out state"),
        };
        let (success_tx, success_rx) = mpsc::channel();
        success_tx
            .send(PendingAccessResult::Logout(Ok(())))
            .expect("queue logout success");
        gate.state = GateState::SigningOut(session);
        gate.pending = Some(success_rx);
        gate.poll_at(now);

        assert!(matches!(gate.state, GateState::Ready));
        assert!(store.secret.borrow().is_none());
        assert_eq!(*store.deletes.borrow(), 1);
    }

    #[test]
    fn endpoint_requires_https_without_fragment_or_credentials() {
        assert!(validate_endpoint("https://reynflow.com/api/yc-access/v1/session").is_ok());
        assert_eq!(
            validate_endpoint("http://reynflow.com/api").unwrap_err(),
            LoginFailure::Configuration("The preview access endpoint must use HTTPS.")
        );
        assert!(validate_endpoint("https://user@reynflow.com/api").is_err());
        assert!(validate_endpoint("https://reynflow.com/api#token").is_err());
        assert!(validate_endpoint("https://reyn flow.com/api").is_err());
    }

    #[test]
    fn failures_have_user_safe_messages() {
        for failure in [
            LoginFailure::InvalidCredentials,
            LoginFailure::RateLimited,
            LoginFailure::PolicyOutdated,
            LoginFailure::InvalidSession,
            LoginFailure::SessionExpired,
            LoginFailure::Network,
            LoginFailure::Service,
        ] {
            let message = failure.message();
            assert!(!message.is_empty());
            assert!(!message.contains("token"));
            assert!(!message.contains("secret"));
        }
    }

    #[test]
    fn public_builds_are_not_access_gated_by_default() {
        if option_env!("REYN_ACCESS_REQUIRED").is_none() {
            assert!(!access_required());
        }
    }

    #[test]
    fn lifecycle_never_starts_gated_studio_before_access_and_drops_it_on_expiry() {
        assert_eq!(
            studio_lifecycle_action(true, false, false),
            StudioLifecycleAction::Keep
        );
        assert_eq!(
            studio_lifecycle_action(true, true, false),
            StudioLifecycleAction::Start
        );
        assert_eq!(
            studio_lifecycle_action(true, false, true),
            StudioLifecycleAction::Drop
        );
        assert_eq!(
            studio_lifecycle_action(false, false, false),
            StudioLifecycleAction::Start
        );
        assert_eq!(
            studio_lifecycle_action(false, false, true),
            StudioLifecycleAction::Keep
        );
    }

    #[test]
    fn access_contract_cli_requires_the_exact_single_switch() {
        assert!(print_build_contract_requested([
            "reyn-studio".into(),
            "--print-access-contract".into(),
        ]));
        assert!(!print_build_contract_requested(["reyn-studio".into()]));
        assert!(!print_build_contract_requested([
            "reyn-studio".into(),
            "--print-access-contract".into(),
            "extra".into(),
        ]));
    }

    #[test]
    fn build_contract_contains_no_credentials() {
        let contract = build_contract_json();
        assert!(contract.contains("com.reyn.studio.preview-access/1"));
        assert!(!contract.contains("password"));
        assert!(!contract.contains("username"));
    }

    fn login_response(expires_at_utc_unix: Option<u64>) -> LoginResponse {
        LoginResponse {
            ok: true,
            session_token: Some("a".repeat(32)),
            expires_at_utc_unix,
            terms_version: TERMS_VERSION.to_owned(),
            privacy_version: PRIVACY_VERSION.to_owned(),
        }
    }

    #[test]
    fn session_response_rejects_stale_malformed_and_unbounded_expiry() {
        let now = 1_000_000;
        assert!(matches!(
            session_from_response(login_response(Some(now)), now),
            Err(LoginFailure::SessionExpired)
        ));
        assert!(matches!(
            session_from_response(login_response(None), now),
            Err(LoginFailure::Service)
        ));
        assert!(matches!(
            session_from_response(
                login_response(Some(now + MAX_SESSION_LIFETIME_SECONDS + 1)),
                now,
            ),
            Err(LoginFailure::Service)
        ));
        let mut wrong_policy = login_response(Some(now + 60));
        wrong_policy.terms_version = "obsolete".to_owned();
        assert!(matches!(
            session_from_response(wrong_policy, now),
            Err(LoginFailure::PolicyOutdated)
        ));
        let malformed: LoginResponse =
            serde_json::from_str(
                r#"{"ok":true,"expires_at_utc_unix":1000010,"terms_version":"1.0","privacy_version":"1.0"}"#,
            )
            .expect("optional token field deserializes");
        assert!(matches!(
            session_from_response(malformed, now),
            Err(LoginFailure::Service)
        ));
        assert!(serde_json::from_str::<LoginResponse>(
            r#"{"ok":true,"session_token":7,"expires_at_utc_unix":"later"}"#
        )
        .is_err());
    }

    #[test]
    fn expired_session_zeroizes_inputs_and_returns_to_login() {
        let store = Rc::new(MemorySessionStore::default());
        let mut gate = gate_with_state(GateState::Granted(test_session(100, 200)), store, None);

        gate.tick(egui::Context::default(), 100);

        assert!(matches!(
            gate.state,
            GateState::Failed(LoginFailure::SessionExpired)
        ));
        assert!(gate.username.is_empty());
        assert!(gate.password.is_empty());
        assert!(!gate.terms_accepted);
        assert!(!gate.requested_initial_focus);
        assert!(!gate.is_granted());
    }

    #[test]
    fn granted_session_schedules_an_expiry_repaint() {
        let store = Rc::new(MemorySessionStore::default());
        let gate = gate_with_state(GateState::Granted(test_session(150, 200)), store, None);
        assert_eq!(gate.time_until_expiry(100), Some(Duration::from_secs(50)));
        assert_eq!(gate.time_until_expiry(150), None);
    }

    #[test]
    fn queued_response_is_rechecked_before_unlocking() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(PendingAccessResult::Login(Ok(Session {
                token: Zeroizing::new("s".repeat(32)),
                expires_at_utc_unix: 100,
                next_validation_utc_unix: 200,
            })))
            .expect("queue session");
        let store = Rc::new(MemorySessionStore::default());
        let mut gate = gate_with_state(GateState::CheckingCredentials, store, Some(receiver));

        gate.poll_at(100);

        assert!(matches!(
            gate.state,
            GateState::Failed(LoginFailure::SessionExpired)
        ));
        assert!(!gate.is_granted());
    }

    #[test]
    fn native_request_and_response_bounds_remain_finite() {
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(15));
        assert_eq!(RESPONSE_LIMIT_BYTES, 32 * 1024);
        assert_eq!(MAX_SESSION_LIFETIME_SECONDS, 24 * 60 * 60);
    }

    #[test]
    #[ignore = "requires explicit live YC credentials and network access"]
    fn live_access_service_accepts_the_native_client_contract() {
        let username = std::env::var("REYN_TEST_USERNAME").expect("REYN_TEST_USERNAME");
        let password = std::env::var("REYN_TEST_PASSWORD").expect("REYN_TEST_PASSWORD");
        let request = LoginRequest {
            username,
            password,
            terms_version: TERMS_VERSION,
            privacy_version: PRIVACY_VERSION,
            client: ClientInfo {
                app_version: env!("CARGO_PKG_VERSION"),
                platform: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
            },
        };
        let session = authenticate("https://reynflow.com/api/yc-access/v1/session", &request)
            .expect("native client should authenticate");
        assert!(!session.token.is_empty());
    }
}
