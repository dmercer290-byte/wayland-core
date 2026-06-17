//! Paste-to-detect modal — state machine + view-model.
//!
//! The user pastes an API key, token, or credential; the modal fingerprints it,
//! validates it live against the guessed provider, and reveals what connected —
//! the headline "you hand it a key and it configures itself" moment.
//!
//! This module is the **logic** half: a pure state machine plus a string
//! view-model. It deliberately holds no ratatui or `App` types so the
//! transitions and the exact lines that will be drawn are unit-testable without
//! a terminal. The `Surface` implementation (draw, key routing, the async
//! `detect_paste` spawn, storage write + live rebind) is a thin wrapper layered
//! on top — every string it renders comes from [`PasteModal::ladder_lines`] /
//! [`PasteModal::reveal_lines`] here, so testing this module tests what the user
//! sees.
//!
//! Async is handled the way the rest of the TUI does it: `handle_key` stays
//! synchronous and, on Enter, returns [`PasteModalAction::Detect`]; the host
//! spawns `detect_paste` and hands the modal a result channel via
//! [`PasteModal::start_detecting`]; [`PasteModal::poll`] (called each tick)
//! pulls the [`DetectionResult`] in without ever blocking the render loop.

use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use tokio::sync::oneshot;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::{Surface, SurfaceAction, SurfaceId};
use crate::tui::app::App;
use crate::tui::theme::Theme;
use wcore_providers::fingerprint::{CredentialKind, fingerprint_key};
use wcore_providers::paste_detect::{DetectionResult, detect_paste};

/// What the host (the `Surface`/router) should do after a key press. Keeps the
/// modal ignorant of `App`, async runtimes, and storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PasteModalAction {
    /// Nothing to do; the modal absorbed the key.
    None,
    /// Begin detecting the pasted credential. The host spawns `detect_paste`
    /// and calls [`PasteModal::start_detecting`] with the result channel.
    Detect(String),
    /// Persist this provider's key (already validated) and set it as default,
    /// then trigger a live rebind. Carries the provider slug.
    Save { provider: String },
    /// Close the modal without saving.
    Close,
}

/// The modal's current phase.
#[derive(Debug)]
pub(crate) enum PasteState {
    /// Awaiting input; the user is typing/pasting.
    Editing,
    /// A detection is in flight (async). The ladder animates.
    Detecting,
    /// A provider connected and the key authenticated.
    Connected {
        provider: String,
        model_count: usize,
        /// The flagship (first/most-capable) live model id, if any. Only real
        /// data from the live catalog — never an invented capability.
        flagship: Option<String>,
    },
    /// The credential needs a guided wizard (AWS secret+region, GCP project,
    /// Azure endpoint, JWT routing) rather than a single validating request.
    Guided {
        kind: CredentialKind,
        provider: Option<String>,
    },
    /// Nothing authenticated; show the failures and offer the picker.
    Unresolved {
        best_guess: Option<String>,
        failures: Vec<String>,
    },
}

/// The paste-to-detect modal.
pub(crate) struct PasteModal {
    input: Input,
    state: PasteState,
    /// Result channel for an in-flight detection (set by `start_detecting`).
    pending: Option<oneshot::Receiver<DetectionResult>>,
}

impl Default for PasteModal {
    fn default() -> Self {
        Self::new()
    }
}

impl PasteModal {
    pub(crate) fn new() -> Self {
        Self {
            input: Input::default(),
            state: PasteState::Editing,
            pending: None,
        }
    }

    /// The raw text the user has entered so far.
    pub(crate) fn value(&self) -> &str {
        self.input.value()
    }

    pub(crate) fn state(&self) -> &PasteState {
        &self.state
    }

    /// Masked echo of the input — credentials are never shown in clear text.
    pub(crate) fn masked_input(&self) -> String {
        mask(self.input.value())
    }

    /// Handle a key. Pure: returns a [`PasteModalAction`] for the host to act on
    /// and never performs I/O.
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PasteModalAction {
        match (&self.state, key.code) {
            // Cancel from anywhere.
            (_, KeyCode::Esc) => PasteModalAction::Close,

            // Editing: Enter kicks off detection (when there is something to detect).
            (PasteState::Editing, KeyCode::Enter) => {
                let value = self.input.value().trim().to_string();
                if value.is_empty() {
                    PasteModalAction::None
                } else {
                    self.state = PasteState::Detecting;
                    PasteModalAction::Detect(value)
                }
            }
            (PasteState::Editing, _) => {
                self.input.handle_event(&Event::Key(key));
                PasteModalAction::None
            }

            // Connected: Enter saves + sets default.
            (PasteState::Connected { provider, .. }, KeyCode::Enter) => PasteModalAction::Save {
                provider: provider.clone(),
            },

            // Unresolved: Enter returns to editing for another paste.
            (PasteState::Unresolved { .. }, KeyCode::Enter) => {
                self.reset_to_editing();
                PasteModalAction::None
            }

            // Detecting / Guided / other keys: absorbed (no-op).
            _ => PasteModalAction::None,
        }
    }

    /// Register the result channel for an in-flight detection. Moves the modal
    /// into [`PasteState::Detecting`].
    pub(crate) fn start_detecting(&mut self, rx: oneshot::Receiver<DetectionResult>) {
        self.state = PasteState::Detecting;
        self.pending = Some(rx);
    }

    /// Poll the in-flight detection (call each tick). Returns `true` when a
    /// result arrived and the state changed.
    pub(crate) fn poll(&mut self) -> bool {
        let Some(rx) = self.pending.as_mut() else {
            return false;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.pending = None;
                self.apply_result(result);
                true
            }
            // Sender dropped without a value — treat as a failed detection.
            Err(oneshot::error::TryRecvError::Closed) => {
                self.pending = None;
                self.state = PasteState::Unresolved {
                    best_guess: None,
                    failures: vec!["detection was cancelled".to_string()],
                };
                true
            }
            Err(oneshot::error::TryRecvError::Empty) => false,
        }
    }

    /// Fold a [`DetectionResult`] into the modal state. Public to the crate so
    /// the host can apply a result it obtained synchronously (and so tests can
    /// drive transitions directly).
    pub(crate) fn apply_result(&mut self, result: DetectionResult) {
        self.state = match result {
            DetectionResult::Connected { provider, models } => PasteState::Connected {
                provider,
                model_count: models.len(),
                flagship: models.first().map(|m| m.id.clone()),
            },
            DetectionResult::NeedsGuidedSetup { kind, provider } => {
                PasteState::Guided { kind, provider }
            }
            DetectionResult::Unresolved {
                best_guess,
                attempts,
            } => PasteState::Unresolved {
                best_guess,
                failures: attempts
                    .into_iter()
                    .filter_map(|a| a.failure.map(|f| format!("{}: {f}", a.provider)))
                    .collect(),
            },
        };
    }

    fn reset_to_editing(&mut self) {
        self.input = Input::default();
        self.state = PasteState::Editing;
        self.pending = None;
    }

    /// The animated detection ladder, as drawn lines. `tick` drives the spinner
    /// glyph on the active rung. Only shown while [`PasteState::Detecting`].
    pub(crate) fn ladder_lines(&self, tick: usize) -> Vec<String> {
        const SPINNER: [char; 4] = ['⠋', '⠙', '⠹', '⠸'];
        let s = SPINNER[tick % SPINNER.len()];
        vec![
            format!("{s} Detecting provider…"),
            format!("{s} Validating key…"),
            format!("{s} Fetching catalog…"),
        ]
    }

    /// The result lines the modal shows once detection settles. Every line is
    /// backed by real data (provider, live model count, flagship id, or the
    /// failure reason) — no invented capabilities.
    pub(crate) fn reveal_lines(&self) -> Vec<String> {
        match &self.state {
            PasteState::Editing => vec![
                "Paste an API key, token, or credential".to_string(),
                "stored in your keychain · only ever sent to the provider".to_string(),
            ],
            PasteState::Detecting => self.ladder_lines(0),
            PasteState::Connected {
                provider,
                model_count,
                flagship,
            } => {
                let mut lines = vec![format!("✓ {provider} connected — {model_count} models")];
                if let Some(flag) = flagship {
                    lines.push(format!("  ready: {flag}"));
                }
                lines.push("Make this my default? [Enter]   Cancel [Esc]".to_string());
                lines
            }
            PasteState::Guided { kind, provider } => {
                let what = guided_hint(*kind, provider.as_deref());
                vec![
                    what,
                    "Press [Esc] to set it up, or paste a different key".to_string(),
                ]
            }
            PasteState::Unresolved {
                best_guess,
                failures,
            } => {
                let mut lines = vec!["✗ Couldn't connect with that credential".to_string()];
                lines.extend(failures.iter().map(|f| format!("  {f}")));
                if let Some(guess) = best_guess {
                    lines.push(format!("  looked most like {guess}"));
                }
                lines.push("[Enter] try another key   [Esc] close".to_string());
                lines
            }
        }
    }
}

/// One-line "what to do next" for a credential that needs a guided wizard.
fn guided_hint(kind: CredentialKind, provider: Option<&str>) -> String {
    match kind {
        CredentialKind::AwsAccessKeyId => {
            "AWS access key detected — Bedrock also needs the secret key + region".to_string()
        }
        CredentialKind::GcpServiceAccount => {
            "GCP service account detected — Vertex needs the project + location".to_string()
        }
        CredentialKind::GcpAccessToken => {
            "GCP token detected — it's short-lived; let's wire ADC for refresh".to_string()
        }
        CredentialKind::Jwt => "Looks like a JWT — which service issues it?".to_string(),
        _ => match provider {
            Some(p) => format!("{p} needs a bit more to finish setup"),
            None => "This credential needs a bit more to finish setup".to_string(),
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Surface — the thin TUI wrapper over `PasteModal` (S4b).
// ─────────────────────────────────────────────────────────────────────────

/// The `/connect` overlay surface: an arrow-key-free modal that hosts a
/// [`PasteModal`], spawns the async `detect_paste` probe, and — on a connected
/// provider the user accepts — writes the validated key to the credentials
/// store, makes it the default, and triggers a live engine rebind.
///
/// All rendered text comes from the modal's view-model
/// ([`PasteModal::reveal_lines`] / [`PasteModal::ladder_lines`]); this struct
/// only owns the spinner tick and the host-side effects the pure modal cannot
/// perform (async spawn, storage write, rebind signal).
pub(crate) struct PasteDetectSurface {
    modal: PasteModal,
    /// Drives the detection-ladder spinner; advanced once per `tick`.
    tick: usize,
}

impl PasteDetectSurface {
    pub(crate) fn new() -> Self {
        Self {
            modal: PasteModal::new(),
            tick: 0,
        }
    }

    /// Persist a validated, accepted credential and request a live rebind.
    ///
    /// Returns a human-readable status line for the transcript. The pasted
    /// value is re-normalized through the fingerprinter so the bytes written
    /// are exactly the bytes that authenticated (an `export NAME=…` paste is
    /// stripped to the bare key), and the key is written under the same
    /// credentials-store slot [`resolve_api_key`](wcore_config) reads — so the
    /// next rebind resolves it without a restart. On any failure the engine is
    /// left untouched (no rebind requested).
    fn save_credential(&self, app: &mut App, slug: &str) -> String {
        use wcore_config::config::{
            ProviderType, default_model_for_slug, patch_global_config, provider_type_from_slug,
            store_provider_api_key,
        };

        let Some(provider) = provider_type_from_slug(slug) else {
            return format!("Couldn't map provider \"{slug}\" — nothing saved.");
        };
        // Bedrock/Vertex/ChatGPT never reach a Connected state via paste, but
        // guard anyway: those authenticate out-of-band and have no key slot.
        if matches!(
            provider,
            ProviderType::Bedrock | ProviderType::Vertex | ProviderType::OpenAIChatGpt
        ) {
            return format!("{slug} authenticates out-of-band; nothing to store.");
        }

        let secret = fingerprint_key(self.modal.value()).normalized;
        if let Err(e) = store_provider_api_key(provider, &secret) {
            tracing::warn!(target: "wcore_cli::tui::paste_detect", "credential save failed: {e:#}");
            return format!("Couldn't save the key: {e}");
        }

        // Make the freshly-connected provider the default so the next prompt
        // runs against it. Non-destructive: only the `[default]` keys change.
        let model = default_model_for_slug(slug);
        let persist = patch_global_config(|f| {
            f.default.provider = slug.to_string();
            if !model.is_empty() {
                f.default.model = Some(model.to_string());
            }
        });
        if let Err(e) = persist {
            tracing::warn!(target: "wcore_cli::tui::paste_detect", "default-provider write failed: {e:#}");
            return format!("Key saved, but couldn't set the default: {e}");
        }

        // The router consumes this on the next tick and rebinds the live
        // engine, which re-resolves the config (new default + stored key).
        app.rebind_request = crate::tui::app::RebindRequest::Credential;
        format!("Connected {slug} — it's now your default. Applying to this session…")
    }

    /// The lines to draw for the current state: an animated ladder while
    /// detecting, the masked input while editing, otherwise the settled reveal.
    fn body_lines(&self) -> Vec<String> {
        match self.modal.state() {
            PasteState::Detecting => self.modal.ladder_lines(self.tick),
            PasteState::Editing => {
                let mut lines = self.modal.reveal_lines();
                let echo = self.modal.masked_input();
                lines.push(String::new());
                lines.push(format!("  ▸ {echo}"));
                lines
            }
            _ => self.modal.reveal_lines(),
        }
    }
}

impl Surface for PasteDetectSurface {
    fn id(&self) -> SurfaceId {
        SurfaceId::PasteDetect
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _app: &App, theme: &Theme) {
        let pane = centered_rect(area, 72, 12);
        frame.render_widget(Clear, pane);
        let block = Block::bordered()
            .title("Connect a provider")
            .border_style(Style::default().fg(theme.orange))
            .style(Style::default().bg(theme.surface_elevated).fg(theme.text));
        let inner = block.inner(pane);
        frame.render_widget(block, pane);
        if inner.height == 0 {
            return;
        }

        let lines: Vec<Line> = self
            .body_lines()
            .into_iter()
            .map(|s| {
                // Color by the line's own marker so success/failure read at a
                // glance; everything else stays in the body text color.
                let style = if s.starts_with('✓') {
                    Style::default()
                        .fg(theme.success)
                        .add_modifier(Modifier::BOLD)
                } else if s.starts_with('✗') {
                    Style::default()
                        .fg(theme.warning)
                        .add_modifier(Modifier::BOLD)
                } else if s.contains("[Enter]") || s.contains("[Esc]") || s.contains("⏎") {
                    Style::default().fg(theme.text_dim)
                } else {
                    Style::default().fg(theme.text)
                };
                Line::styled(s, style)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn handle_key(&mut self, key: KeyEvent, app: &mut App) -> SurfaceAction {
        match self.modal.handle_key(key) {
            PasteModalAction::None => SurfaceAction::None,
            PasteModalAction::Close => SurfaceAction::CloseOverlay,
            PasteModalAction::Detect(raw) => {
                // Spawn the live probe off the render loop; the result lands via
                // `tick → poll`. Requires a tokio runtime (always present under
                // the TUI loop). Without one the modal stays in Detecting and
                // the dropped sender flips it to Unresolved on the next poll.
                let (tx, rx) = oneshot::channel();
                if tokio::runtime::Handle::try_current().is_ok() {
                    tokio::spawn(async move {
                        let base = wcore_config::config::Config::resolve(
                            &wcore_config::config::CliArgs::default(),
                        )
                        .unwrap_or_default();
                        let _ = tx.send(detect_paste(&base, &raw).await);
                    });
                }
                self.modal.start_detecting(rx);
                SurfaceAction::None
            }
            PasteModalAction::Save { provider } => {
                let status = self.save_credential(app, &provider);
                push_system_line(app, status);
                SurfaceAction::CloseOverlay
            }
        }
    }

    fn tick(&mut self, _app: &mut App) -> SurfaceAction {
        self.tick = self.tick.wrapping_add(1);
        // Drain an in-flight detection result into the modal state. A redraw
        // happens every frame regardless, so no explicit invalidation needed.
        self.modal.poll();
        SurfaceAction::None
    }
}

/// Append a one-line System turn so the user sees the outcome of a save without
/// the modal having to grow a status state. Mirrors the transcript-append shape
/// used elsewhere in the router (`note_surface_panic`).
fn push_system_line(app: &mut App, msg: String) {
    use crate::tui::app::{TurnRole, TurnView};
    use crate::tui::turn_element::TurnElement;
    app.session.turns.push(TurnView {
        role: TurnRole::System,
        elements: vec![TurnElement::Markdown(msg)],
    });
    app.session.trim_history();
}

/// A centered sub-rect of `area`, clamped to fit. Self-contained so the surface
/// does not depend on `config.rs`'s private layout helpers.
fn centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// Mask a secret for on-screen echo: keep a short visible tail so the user can
/// confirm they pasted the right thing, hide the rest.
fn mask(value: &str) -> String {
    let n = value.chars().count();
    if n == 0 {
        return String::new();
    }
    let tail: String = value.chars().skip(n.saturating_sub(4)).collect();
    let dots = "•".repeat(n.saturating_sub(4).min(24));
    format!("{dots}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;
    use wcore_providers::ModelInfo;
    use wcore_providers::key_validation::ValidationOutcome;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(m: &mut PasteModal, s: &str) {
        for ch in s.chars() {
            m.handle_key(key(KeyCode::Char(ch)));
        }
    }

    #[test]
    fn typing_accumulates_into_the_buffer() {
        let mut m = PasteModal::new();
        type_str(&mut m, "sk-ant-xyz");
        assert_eq!(m.value(), "sk-ant-xyz");
        assert!(matches!(m.state(), PasteState::Editing));
    }

    #[test]
    fn enter_on_nonempty_emits_detect_and_enters_detecting() {
        let mut m = PasteModal::new();
        type_str(&mut m, "sk-ant-xyz");
        let action = m.handle_key(key(KeyCode::Enter));
        assert_eq!(action, PasteModalAction::Detect("sk-ant-xyz".to_string()));
        assert!(matches!(m.state(), PasteState::Detecting));
    }

    #[test]
    fn enter_on_empty_does_nothing() {
        let mut m = PasteModal::new();
        assert_eq!(m.handle_key(key(KeyCode::Enter)), PasteModalAction::None);
        assert!(matches!(m.state(), PasteState::Editing));
    }

    #[test]
    fn esc_always_closes() {
        let mut m = PasteModal::new();
        type_str(&mut m, "abc");
        assert_eq!(m.handle_key(key(KeyCode::Esc)), PasteModalAction::Close);
    }

    #[test]
    fn connected_result_reveals_real_data_only() {
        let mut m = PasteModal::new();
        m.apply_result(DetectionResult::Connected {
            provider: "anthropic".to_string(),
            models: vec![
                ModelInfo::from_id("claude-opus-4-8"),
                ModelInfo::from_id("claude-haiku-4-5"),
            ],
        });
        let lines = m.reveal_lines();
        assert!(lines[0].contains("anthropic connected — 2 models"));
        assert!(lines.iter().any(|l| l.contains("claude-opus-4-8")));
        // Enter on a connected modal asks the host to save that provider.
        assert_eq!(
            m.handle_key(key(KeyCode::Enter)),
            PasteModalAction::Save {
                provider: "anthropic".to_string()
            }
        );
    }

    #[test]
    fn guided_result_shows_the_next_action_not_a_dead_end() {
        let mut m = PasteModal::new();
        m.apply_result(DetectionResult::NeedsGuidedSetup {
            kind: CredentialKind::AwsAccessKeyId,
            provider: Some("bedrock".to_string()),
        });
        let lines = m.reveal_lines();
        assert!(lines[0].contains("AWS access key"));
        assert!(lines[0].contains("secret key + region"));
    }

    #[test]
    fn unresolved_lists_failures_and_offers_retry() {
        let mut m = PasteModal::new();
        m.apply_result(DetectionResult::Unresolved {
            best_guess: Some("openai".to_string()),
            attempts: vec![ValidationOutcome {
                provider: "openai".to_string(),
                reached: wcore_providers::key_validation::Rung::Detected,
                models: Vec::new(),
                failure: Some("401 Unauthorized".to_string()),
            }],
        });
        let lines = m.reveal_lines();
        assert!(lines.iter().any(|l| l.contains("Couldn't connect")));
        assert!(lines.iter().any(|l| l.contains("openai: 401 Unauthorized")));
        assert!(lines.iter().any(|l| l.contains("most like openai")));
        // Enter retries (back to editing), Esc closes.
        m.handle_key(key(KeyCode::Enter));
        assert!(matches!(m.state(), PasteState::Editing));
    }

    #[test]
    fn poll_applies_an_async_result() {
        let mut m = PasteModal::new();
        let (tx, rx) = oneshot::channel();
        m.start_detecting(rx);
        assert!(matches!(m.state(), PasteState::Detecting));
        assert!(!m.poll(), "nothing sent yet");
        tx.send(DetectionResult::Connected {
            provider: "groq".to_string(),
            models: vec![ModelInfo::from_id("llama-3.1-70b")],
        })
        .unwrap();
        assert!(m.poll(), "result should be applied");
        assert!(matches!(m.state(), PasteState::Connected { .. }));
    }

    #[test]
    fn poll_treats_dropped_sender_as_failure() {
        let mut m = PasteModal::new();
        let (tx, rx) = oneshot::channel::<DetectionResult>();
        m.start_detecting(rx);
        drop(tx);
        assert!(m.poll());
        assert!(matches!(m.state(), PasteState::Unresolved { .. }));
    }

    #[test]
    fn masking_keeps_a_short_tail() {
        let mut m = PasteModal::new();
        type_str(&mut m, "sk-ant-api03-secret1234");
        let masked = m.masked_input();
        assert!(masked.ends_with("1234"));
        assert!(masked.contains('•'));
        assert!(!masked.contains("secret"));
    }

    // ── Surface (S4b) ───────────────────────────────────────────────────

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Render the surface into an 80×24 `TestBackend` and flatten the buffer.
    fn render_surface(surface: &mut PasteDetectSurface, app: &App) -> String {
        let theme = Theme::no_color();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        terminal
            .draw(|f| surface.render(f, f.area(), app, &theme))
            .expect("render paste-detect surface");
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn surface_reports_paste_detect_id() {
        assert_eq!(PasteDetectSurface::new().id(), SurfaceId::PasteDetect);
    }

    #[test]
    fn editing_state_renders_prompt_framed_with_masked_echo() {
        let app = App::new();
        let mut surface = PasteDetectSurface::new();
        type_str(&mut surface.modal, "sk-ant-api03-bodyXY42");
        let text = render_surface(&mut surface, &app);
        assert!(text.contains("Connect a provider"), "framed title missing");
        assert!(text.contains("Paste an API key"), "editing prompt missing");
        // The echo masks everything but the last four chars: the tail shows so
        // the user can confirm the paste, the secret body never reaches screen.
        assert!(text.contains('•'), "masked dots missing from echo");
        assert!(text.contains("XY42"), "confirmation tail missing");
        assert!(!text.contains("api03"), "secret body leaked to screen");
        assert!(
            !text.contains("sk-ant-api03-bodyXY42"),
            "raw secret leaked to screen"
        );
    }

    #[test]
    fn connected_state_renders_real_data_and_default_prompt() {
        let app = App::new();
        let mut surface = PasteDetectSurface::new();
        surface.modal.apply_result(DetectionResult::Connected {
            provider: "anthropic".to_string(),
            models: vec![
                ModelInfo::from_id("claude-opus-4-8"),
                ModelInfo::from_id("claude-haiku-4-5"),
            ],
        });
        let text = render_surface(&mut surface, &app);
        assert!(
            text.contains("anthropic connected"),
            "provider line missing"
        );
        assert!(text.contains("2 models"), "live count missing");
        assert!(text.contains("claude-opus-4-8"), "flagship id missing");
        assert!(text.contains("default"), "make-default prompt missing");
    }

    #[test]
    fn detecting_state_renders_the_ladder() {
        let app = App::new();
        let mut surface = PasteDetectSurface::new();
        let (_tx, rx) = oneshot::channel();
        surface.modal.start_detecting(rx);
        let text = render_surface(&mut surface, &app);
        assert!(text.contains("Detecting provider"), "ladder not rendered");
    }

    #[test]
    fn esc_closes_the_overlay() {
        let mut app = App::new();
        let mut surface = PasteDetectSurface::new();
        type_str(&mut surface.modal, "abc");
        let action = surface.handle_key(key(KeyCode::Esc), &mut app);
        assert!(matches!(action, SurfaceAction::CloseOverlay));
    }

    #[test]
    fn tick_advances_the_spinner_and_drains_a_result() {
        let mut app = App::new();
        let mut surface = PasteDetectSurface::new();
        let (tx, rx) = oneshot::channel();
        surface.modal.start_detecting(rx);
        tx.send(DetectionResult::Connected {
            provider: "groq".to_string(),
            models: vec![ModelInfo::from_id("llama-3.1-70b")],
        })
        .unwrap();
        let before = surface.tick;
        surface.tick(&mut app);
        assert_eq!(
            surface.tick,
            before.wrapping_add(1),
            "spinner did not advance"
        );
        assert!(
            matches!(surface.modal.state(), PasteState::Connected { .. }),
            "tick did not drain the detection result into the modal"
        );
    }
}
