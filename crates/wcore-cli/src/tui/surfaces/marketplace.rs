//! Lane F2 — the `/plugins` marketplace overlay.
//!
//! A summoned overlay (opened by the `/plugins` command, dismissed with `Esc`),
//! NOT a permanent tab. Two segments:
//!  * **Browse** — every registered marketplace's plugins (from the cached
//!    catalog), grouped by source, fuzzy-filtered as you type. `Enter` resolves
//!    the highlighted plugin (a git clone, off the event loop) and shows its
//!    `InstallPlan` as a consent surface; `Enter` again installs.
//!  * **Installed** — what's on disk, with `u` to uninstall.
//!
//! All slow work (resolve, install, uninstall, add-source) runs in a
//! `spawn_blocking` task whose result returns over a `oneshot` polled in
//! `tick()`. Surface-local state only; nothing about the marketplace lives on
//! `App`. The cheap listings (catalog, installed) are read from disk on enter
//! and after each mutation.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use tokio::sync::oneshot;

use crate::tui::app::App;
use crate::tui::surfaces::{Surface, SurfaceAction, SurfaceId};
use crate::tui::theme::Theme;
use crate::tui::widgets::{panel, spinner_frame};

use crate::plugin::{catalog, known, marketplace};

/// Which segment of the overlay is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Segment {
    Browse,
    Installed,
}

/// The overlay's current interaction mode.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    /// A scrollable list (browse or installed, per `seg`).
    List,
    /// A blocking task is running; show a spinner with this label.
    Loading(String),
    /// The consent surface for a resolved plugin.
    Consent,
    /// The add-marketplace text input.
    AddInput,
}

/// One rendered row of the browse list: a marketplace header or a plugin.
#[derive(Debug, Clone)]
enum BrowseRow {
    Header { marketplace: String, official: bool },
    Plugin(BrowsePlugin),
}

#[derive(Debug, Clone)]
struct BrowsePlugin {
    marketplace: String,
    name: String,
    version: Option<String>,
    description: Option<String>,
    installed: bool,
}

/// One installed plugin row.
#[derive(Debug, Clone)]
struct InstalledRow {
    plugin: String,
    marketplace: String,
    version: String,
    grade: String,
}

/// The async job currently in flight, if any. Exactly one at a time (the mode
/// gates further input while a job runs).
enum Job {
    Resolve(oneshot::Receiver<Result<Box<marketplace::PlannedInstall>, String>>),
    Install(oneshot::Receiver<Result<String, String>>),
    Uninstall(oneshot::Receiver<Result<bool, String>>),
    Add(oneshot::Receiver<Result<String, String>>),
}

/// The `/plugins` marketplace overlay surface.
pub struct MarketplaceSurface {
    seg: Segment,
    mode: Mode,
    filter: String,
    /// Cursor among the *selectable* rows (plugins in Browse, rows in Installed).
    selected: usize,
    add_input: String,
    /// A transient status line (last action result), with an error flag.
    status: Option<(String, bool)>,
    // Loaded data (refreshed on enter + after mutations).
    browse: Vec<BrowseRow>,
    installed: Vec<InstalledRow>,
    // The resolved plan awaiting consent.
    planned: Option<Box<marketplace::PlannedInstall>>,
    // In-flight async work.
    job: Option<Job>,
}

impl MarketplaceSurface {
    pub fn new() -> Self {
        Self {
            seg: Segment::Browse,
            mode: Mode::List,
            filter: String::new(),
            selected: 0,
            add_input: String::new(),
            status: None,
            browse: Vec::new(),
            installed: Vec::new(),
            planned: None,
            job: None,
        }
    }

    /// The plugins store root the CLI uses (`~/.genesis/plugins`).
    fn root() -> std::path::PathBuf {
        wcore_config::config::profile_home().join("plugins")
    }

    fn quarantine() -> std::path::PathBuf {
        Self::root().join(".quarantine")
    }

    /// Reload the cheap listings from disk. Safe to call synchronously.
    fn reload(&mut self) {
        let root = Self::root();
        let installed_provs = marketplace::list_marketplace_installed(&root).unwrap_or_default();

        self.installed = installed_provs
            .iter()
            .map(|p| InstalledRow {
                plugin: p.plugin.clone(),
                marketplace: p.marketplace.clone(),
                version: p.version.clone(),
                grade: p.grade.clone(),
            })
            .collect();
        self.installed.sort_by(|a, b| a.plugin.cmp(&b.plugin));

        let is_installed = |market: &str, name: &str| {
            installed_provs
                .iter()
                .any(|p| p.marketplace == market && p.plugin == name)
        };

        let mut browse = Vec::new();
        let mut markets = known::list_marketplaces(&root).unwrap_or_default();
        markets.sort_by(|a, b| a.name.cmp(&b.name));
        for m in &markets {
            let entries = catalog::load_catalog(&root, &m.name);
            browse.push(BrowseRow::Header {
                marketplace: m.name.clone(),
                official: m.official,
            });
            for e in entries {
                browse.push(BrowseRow::Plugin(BrowsePlugin {
                    marketplace: m.name.clone(),
                    installed: is_installed(&m.name, &e.name),
                    name: e.name,
                    version: e.version,
                    description: e.description,
                }));
            }
        }
        self.browse = browse;
        self.clamp_cursor();
    }

    /// The browse rows after the current filter is applied. A header survives
    /// only if at least one of its plugins matches.
    fn filtered_browse(&self) -> Vec<BrowseRow> {
        if self.filter.is_empty() {
            return self.browse.clone();
        }
        let needle = self.filter.to_lowercase();
        let matches = |p: &BrowsePlugin| {
            p.name.to_lowercase().contains(&needle)
                || p.description
                    .as_deref()
                    .is_some_and(|d| d.to_lowercase().contains(&needle))
        };
        let mut out: Vec<BrowseRow> = Vec::new();
        let mut pending_header: Option<BrowseRow> = None;
        let mut header_has_match = false;
        for row in &self.browse {
            match row {
                BrowseRow::Header { .. } => {
                    pending_header = Some(row.clone());
                    header_has_match = false;
                }
                BrowseRow::Plugin(p) if matches(p) => {
                    if !header_has_match {
                        if let Some(h) = pending_header.take() {
                            out.push(h);
                        }
                        header_has_match = true;
                    }
                    out.push(row.clone());
                }
                BrowseRow::Plugin(_) => {}
            }
        }
        out
    }

    /// The selectable plugins in the (filtered) browse list, in display order.
    fn browse_plugins(&self) -> Vec<BrowsePlugin> {
        self.filtered_browse()
            .into_iter()
            .filter_map(|r| match r {
                BrowseRow::Plugin(p) => Some(p),
                BrowseRow::Header { .. } => None,
            })
            .collect()
    }

    /// Number of selectable rows in the current segment.
    fn selectable_len(&self) -> usize {
        match self.seg {
            Segment::Browse => self.browse_plugins().len(),
            Segment::Installed => self.installed.len(),
        }
    }

    fn clamp_cursor(&mut self) {
        let len = self.selectable_len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    fn move_cursor(&mut self, delta: i32) {
        let len = self.selectable_len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected.min(len - 1) as i32;
        self.selected = (cur + delta).rem_euclid(len as i32) as usize;
    }

    fn selected_browse_plugin(&self) -> Option<BrowsePlugin> {
        self.browse_plugins().into_iter().nth(self.selected)
    }

    fn selected_installed(&self) -> Option<InstalledRow> {
        self.installed.get(self.selected).cloned()
    }

    // ── async kickoffs ─────────────────────────────────────────────────

    /// Resolve the highlighted browse plugin off-thread → Consent on success.
    fn start_resolve(&mut self) {
        let Some(p) = self.selected_browse_plugin() else {
            return;
        };
        let (tx, rx) = oneshot::channel();
        let (root, quarantine) = (Self::root(), Self::quarantine());
        let (market, name) = (p.marketplace.clone(), p.name.clone());
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                marketplace::resolve_and_plan(&root, &quarantine, &market, &name)
                    .map(Box::new)
                    .map_err(|e| e.to_string())
            })
            .await
            .unwrap_or_else(|_| Err("resolve task panicked".to_string()));
            let _ = tx.send(result);
        });
        self.job = Some(Job::Resolve(rx));
        self.mode = Mode::Loading(format!("Resolving {}…", p.name));
        self.status = None;
    }

    /// Commit the resolved plan off-thread.
    fn start_install(&mut self) {
        let Some(planned) = self.planned.take() else {
            return;
        };
        let label = format!("Installing {}…", planned.draft.name);
        let (tx, rx) = oneshot::channel();
        let root = Self::root();
        let installed_at = now_rfc3339();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                marketplace::commit_install(&root, &planned, installed_at)
                    .map(|dir| dir.display().to_string())
                    .map_err(|e| e.to_string())
            })
            .await
            .unwrap_or_else(|_| Err("install task panicked".to_string()));
            let _ = tx.send(result);
        });
        self.job = Some(Job::Install(rx));
        self.mode = Mode::Loading(label);
    }

    /// Uninstall the highlighted installed plugin off-thread.
    fn start_uninstall(&mut self) {
        let Some(row) = self.selected_installed() else {
            return;
        };
        let (tx, rx) = oneshot::channel();
        let root = Self::root();
        let (plugin, market) = (row.plugin.clone(), row.marketplace.clone());
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                marketplace::remove_marketplace_plugin(&root, &plugin, &market)
                    .map_err(|e| e.to_string())
            })
            .await
            .unwrap_or_else(|_| Err("uninstall task panicked".to_string()));
            let _ = tx.send(result);
        });
        self.job = Some(Job::Uninstall(rx));
        self.mode = Mode::Loading(format!("Uninstalling {}…", row.plugin));
    }

    /// Add a marketplace source off-thread (git clone + catalog cache).
    fn start_add(&mut self) {
        let source = self.add_input.trim().to_string();
        if source.is_empty() {
            self.mode = Mode::List;
            return;
        }
        let (tx, rx) = oneshot::channel();
        let (root, quarantine) = (Self::root(), Self::quarantine());
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                marketplace::add_marketplace_source(&root, &quarantine, &source)
                    .map(|m| m.name)
                    .map_err(|e| e.to_string())
            })
            .await
            .unwrap_or_else(|_| Err("add task panicked".to_string()));
            let _ = tx.send(result);
        });
        self.job = Some(Job::Add(rx));
        self.mode = Mode::Loading(format!("Adding {}…", self.add_input.trim()));
        self.add_input.clear();
    }
}

impl Default for MarketplaceSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl Surface for MarketplaceSurface {
    fn id(&self) -> SurfaceId {
        SurfaceId::Plugins
    }

    fn on_enter(&mut self, _app: &mut App) {
        self.reload();
    }

    fn tick(&mut self, _app: &mut App) -> SurfaceAction {
        // Poll the in-flight job, if any.
        let done = match &mut self.job {
            Some(Job::Resolve(rx)) => rx.try_recv().ok().map(JobResult::Resolve),
            Some(Job::Install(rx)) => rx.try_recv().ok().map(JobResult::Install),
            Some(Job::Uninstall(rx)) => rx.try_recv().ok().map(JobResult::Uninstall),
            Some(Job::Add(rx)) => rx.try_recv().ok().map(JobResult::Add),
            None => None,
        };
        if let Some(result) = done {
            self.job = None;
            self.apply_job_result(result);
        }
        SurfaceAction::None
    }

    fn handle_key(&mut self, key: KeyEvent, _app: &mut App) -> SurfaceAction {
        // While a job runs, swallow input except Esc (which only clears the
        // transient view, never the running task).
        if matches!(self.mode, Mode::Loading(_)) {
            return SurfaceAction::None;
        }

        match &self.mode {
            Mode::AddInput => self.handle_add_key(key),
            Mode::Consent => match key.code {
                KeyCode::Enter => {
                    self.start_install();
                    SurfaceAction::None
                }
                KeyCode::Esc => {
                    self.planned = None;
                    self.mode = Mode::List;
                    SurfaceAction::None
                }
                _ => SurfaceAction::None,
            },
            Mode::List => self.handle_list_key(key),
            Mode::Loading(_) => SurfaceAction::None,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        // Cover whatever's behind (the active surface) with a clean dim fill so
        // the overlay reads as a focused panel summoned over the session.
        frame.render_widget(Clear, area);
        frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), area);

        let card = centered(area, 90, 90);
        let block = panel(" Plugins ", theme);
        let inner = block.inner(card);
        frame.render_widget(Clear, card);
        frame.render_widget(block, card);
        if inner.height < 3 || inner.width < 10 {
            return;
        }

        match &self.mode {
            Mode::Loading(label) => render_loading(frame, inner, label, app.frame_tick, theme),
            Mode::Consent => render_consent(frame, inner, self.planned.as_deref(), theme),
            Mode::AddInput => render_add(frame, inner, &self.add_input, theme),
            Mode::List => self.render_list(frame, inner, theme),
        }
    }
}

/// The outcome of a finished job, normalized for `apply_job_result`.
enum JobResult {
    Resolve(Result<Box<marketplace::PlannedInstall>, String>),
    Install(Result<String, String>),
    Uninstall(Result<bool, String>),
    Add(Result<String, String>),
}

impl MarketplaceSurface {
    fn apply_job_result(&mut self, result: JobResult) {
        match result {
            JobResult::Resolve(Ok(planned)) => {
                self.planned = Some(planned);
                self.mode = Mode::Consent;
            }
            JobResult::Resolve(Err(e)) => {
                self.status = Some((format!("resolve failed: {e}"), true));
                self.mode = Mode::List;
            }
            JobResult::Install(Ok(_dir)) => {
                self.status = Some(("installed".to_string(), false));
                self.mode = Mode::List;
                self.reload();
            }
            JobResult::Install(Err(e)) => {
                self.status = Some((format!("install failed: {e}"), true));
                self.mode = Mode::List;
            }
            JobResult::Uninstall(Ok(_)) => {
                self.status = Some(("uninstalled".to_string(), false));
                self.mode = Mode::List;
                self.reload();
            }
            JobResult::Uninstall(Err(e)) => {
                self.status = Some((format!("uninstall failed: {e}"), true));
                self.mode = Mode::List;
            }
            JobResult::Add(Ok(name)) => {
                self.status = Some((format!("added marketplace '{name}'"), false));
                self.mode = Mode::List;
                self.seg = Segment::Browse;
                self.reload();
            }
            JobResult::Add(Err(e)) => {
                self.status = Some((format!("add failed: {e}"), true));
                self.mode = Mode::List;
            }
        }
    }

    fn handle_list_key(&mut self, key: KeyEvent) -> SurfaceAction {
        match key.code {
            KeyCode::Esc => {
                if self.filter.is_empty() {
                    SurfaceAction::CloseOverlay
                } else {
                    self.filter.clear();
                    self.clamp_cursor();
                    SurfaceAction::None
                }
            }
            KeyCode::Down => {
                self.move_cursor(1);
                SurfaceAction::None
            }
            KeyCode::Up => {
                self.move_cursor(-1);
                SurfaceAction::None
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.seg = match self.seg {
                    Segment::Browse => Segment::Installed,
                    Segment::Installed => Segment::Browse,
                };
                self.selected = 0;
                self.filter.clear();
                SurfaceAction::None
            }
            KeyCode::Enter => {
                if self.seg == Segment::Browse {
                    self.start_resolve();
                }
                SurfaceAction::None
            }
            KeyCode::Char('u') if self.seg == Segment::Installed => {
                self.start_uninstall();
                SurfaceAction::None
            }
            KeyCode::Char('a') => {
                self.add_input.clear();
                self.mode = Mode::AddInput;
                SurfaceAction::None
            }
            KeyCode::Char('r') => {
                self.reload();
                self.status = Some(("refreshed".to_string(), false));
                SurfaceAction::None
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.clamp_cursor();
                SurfaceAction::None
            }
            // Type to filter (Browse only — Installed lists are short).
            KeyCode::Char(c) if self.seg == Segment::Browse => {
                self.filter.push(c);
                self.selected = 0;
                SurfaceAction::None
            }
            _ => SurfaceAction::None,
        }
    }

    fn handle_add_key(&mut self, key: KeyEvent) -> SurfaceAction {
        match key.code {
            KeyCode::Esc => {
                self.add_input.clear();
                self.mode = Mode::List;
                SurfaceAction::None
            }
            KeyCode::Enter => {
                self.start_add();
                SurfaceAction::None
            }
            KeyCode::Backspace => {
                self.add_input.pop();
                SurfaceAction::None
            }
            KeyCode::Char(c) => {
                self.add_input.push(c);
                SurfaceAction::None
            }
            _ => SurfaceAction::None,
        }
    }

    fn render_list(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let [seg_area, body_area, hint_area] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);

        // Segment + filter row.
        let bg = Style::default().bg(theme.bg);
        let seg = |label: &str, on: bool| {
            if on {
                Span::styled(
                    format!(" {label} "),
                    bg.fg(theme.text).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(format!(" {label} "), bg.fg(theme.text_muted))
            }
        };
        let mut top = vec![
            seg("Browse", self.seg == Segment::Browse),
            Span::styled("·", bg.fg(theme.text_muted)),
            seg("Installed", self.seg == Segment::Installed),
        ];
        if self.seg == Segment::Browse {
            top.push(Span::styled("    ", bg));
            top.push(Span::styled("⌕ ", bg.fg(theme.text_muted)));
            top.push(Span::styled(
                if self.filter.is_empty() {
                    "type to filter".to_string()
                } else {
                    self.filter.clone()
                },
                bg.fg(if self.filter.is_empty() {
                    theme.text_muted
                } else {
                    theme.text
                }),
            ));
        }
        let mut seg_lines = vec![Line::from(top)];
        if let Some((msg, is_err)) = &self.status {
            seg_lines.push(Line::from(Span::styled(
                format!(" {msg}"),
                bg.fg(if *is_err { theme.error } else { theme.success }),
            )));
        } else {
            seg_lines.push(Line::from(""));
        }
        frame.render_widget(Paragraph::new(seg_lines).style(bg), seg_area);

        // Body list.
        match self.seg {
            Segment::Browse => self.render_browse(frame, body_area, theme),
            Segment::Installed => self.render_installed(frame, body_area, theme),
        }

        // Hint row.
        let hint = match self.seg {
            Segment::Browse => vec![
                Span::styled(" ↑↓ ", bg.fg(theme.orange)),
                Span::styled("move   ", bg.fg(theme.text_muted)),
                Span::styled("⏎ ", bg.fg(theme.orange)),
                Span::styled("inspect   ", bg.fg(theme.text_muted)),
                Span::styled("a ", bg.fg(theme.orange)),
                Span::styled("add   ", bg.fg(theme.text_muted)),
                Span::styled("⇥ ", bg.fg(theme.orange)),
                Span::styled("installed   ", bg.fg(theme.text_muted)),
                Span::styled("esc ", bg.fg(theme.orange)),
                Span::styled("close", bg.fg(theme.text_muted)),
            ],
            Segment::Installed => vec![
                Span::styled(" ↑↓ ", bg.fg(theme.orange)),
                Span::styled("move   ", bg.fg(theme.text_muted)),
                Span::styled("u ", bg.fg(theme.orange)),
                Span::styled("uninstall   ", bg.fg(theme.text_muted)),
                Span::styled("⇥ ", bg.fg(theme.orange)),
                Span::styled("browse   ", bg.fg(theme.text_muted)),
                Span::styled("esc ", bg.fg(theme.orange)),
                Span::styled("close", bg.fg(theme.text_muted)),
            ],
        };
        frame.render_widget(Paragraph::new(Line::from(hint)).style(bg), hint_area);
    }

    fn render_browse(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let bg = Style::default().bg(theme.bg);
        let rows = self.filtered_browse();
        if rows.is_empty() {
            let msg = if self.browse.is_empty() {
                "  No marketplaces yet — press 'a' to add one (owner/repo, URL, or path)."
            } else {
                "  Nothing matches that filter."
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(msg, bg.fg(theme.text_muted)))).style(bg),
                area,
            );
            return;
        }
        let mut lines: Vec<Line> = Vec::new();
        let mut plugin_idx = 0usize;
        for row in &rows {
            match row {
                BrowseRow::Header {
                    marketplace,
                    official,
                } => {
                    let mut spans = vec![Span::styled(
                        format!(" {marketplace} "),
                        bg.fg(theme.text_muted).add_modifier(Modifier::BOLD),
                    )];
                    if *official {
                        spans.push(Span::styled("official", bg.fg(theme.success)));
                    }
                    lines.push(Line::from(spans));
                }
                BrowseRow::Plugin(p) => {
                    let is_sel = plugin_idx == self.selected;
                    let row_bg = if is_sel {
                        Style::default().bg(theme.surface_hover)
                    } else {
                        bg
                    };
                    let cursor = if is_sel { "▸ " } else { "  " };
                    let name_style = if is_sel {
                        row_bg.fg(theme.text).add_modifier(Modifier::BOLD)
                    } else {
                        row_bg.fg(theme.text_dim)
                    };
                    let mut spans = vec![
                        Span::styled(cursor, row_bg.fg(theme.orange)),
                        Span::styled(format!("{:<22}", truncate(&p.name, 22)), name_style),
                    ];
                    if let Some(d) = &p.description {
                        spans.push(Span::styled(truncate(d, 40), row_bg.fg(theme.text_muted)));
                    }
                    if p.installed {
                        spans.push(Span::styled("  ✓ installed", row_bg.fg(theme.success)));
                    }
                    lines.push(Line::from(spans));
                    plugin_idx += 1;
                }
            }
        }
        frame.render_widget(Paragraph::new(lines).style(bg), area);
    }

    fn render_installed(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let bg = Style::default().bg(theme.bg);
        if self.installed.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "  Nothing installed yet.",
                    bg.fg(theme.text_muted),
                )))
                .style(bg),
                area,
            );
            return;
        }
        let lines: Vec<Line> = self
            .installed
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let is_sel = i == self.selected;
                let row_bg = if is_sel {
                    Style::default().bg(theme.surface_hover)
                } else {
                    bg
                };
                let cursor = if is_sel { "▸ " } else { "  " };
                let name_style = if is_sel {
                    row_bg.fg(theme.text).add_modifier(Modifier::BOLD)
                } else {
                    row_bg.fg(theme.text_dim)
                };
                Line::from(vec![
                    Span::styled(cursor, row_bg.fg(theme.orange)),
                    Span::styled(
                        format!(
                            "{:<26}",
                            truncate(&format!("{}@{}", r.plugin, r.marketplace), 26)
                        ),
                        name_style,
                    ),
                    Span::styled(format!("{:<10}", r.version), row_bg.fg(theme.text_muted)),
                    Span::styled(r.grade.clone(), row_bg.fg(theme.text_muted)),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines).style(bg), area);
    }
}

// ── free helpers ───────────────────────────────────────────────────────

fn render_loading(frame: &mut Frame, area: Rect, label: &str, tick: u64, theme: &Theme) {
    let bg = Style::default().bg(theme.bg);
    let line = Line::from(vec![
        Span::styled(format!("  {} ", spinner_frame(tick)), bg.fg(theme.orange)),
        Span::styled(label.to_string(), bg.fg(theme.text)),
    ]);
    frame.render_widget(Paragraph::new(vec![Line::from(""), line]).style(bg), area);
}

fn render_add(frame: &mut Frame, area: Rect, input: &str, theme: &Theme) {
    let bg = Style::default().bg(theme.bg);
    let lines = vec![
        Line::from(Span::styled(
            "  Add a marketplace",
            bg.fg(theme.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "  owner/repo, a git URL, or a local path:",
            bg.fg(theme.text_muted),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  ", bg),
            Span::styled(input.to_string(), bg.fg(theme.text)),
            Span::styled("▏", bg.fg(theme.orange)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  ⏎ ", bg.fg(theme.orange)),
            Span::styled("add    ", bg.fg(theme.text_muted)),
            Span::styled("esc ", bg.fg(theme.orange)),
            Span::styled("cancel", bg.fg(theme.text_muted)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).style(bg), area);
}

fn render_consent(
    frame: &mut Frame,
    area: Rect,
    planned: Option<&marketplace::PlannedInstall>,
    theme: &Theme,
) {
    let bg = Style::default().bg(theme.bg);
    let Some(planned) = planned else {
        return;
    };
    let plan = &planned.plan;
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled(
            format!("  {}", plan.plugin),
            bg.fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  @{}", plan.marketplace), bg.fg(theme.text_muted)),
        Span::styled(format!("   {:?}", plan.grade), bg.fg(theme.warning)),
    ]));
    lines.push(Line::from(""));

    // Adds.
    let (skills, commands, agents) = {
        let s = plan.adds.iter().filter(|a| a.kind == "skill").count();
        let c = plan.adds.iter().filter(|a| a.kind == "command").count();
        let a = plan.adds.iter().filter(|a| a.kind == "agent").count();
        (s, c, a)
    };
    lines.push(kv_line(
        "adds",
        &format!("{skills} skills · {agents} agents · {commands} commands"),
        theme,
    ));

    // Spawns (env keys only, never values).
    if plan.spawns.is_empty() {
        lines.push(kv_line("spawns", "—", theme));
    } else {
        for s in &plan.spawns {
            let args = if s.args.is_empty() {
                String::new()
            } else {
                format!(" {}", s.args.join(" "))
            };
            lines.push(Line::from(vec![
                Span::styled("  spawns    ", bg.fg(theme.text_muted)),
                Span::styled(
                    format!("{} [{}]", s.name, s.transport_kind),
                    bg.fg(theme.text),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                format!(
                    "            {}{}",
                    truncate(&s.command, 48),
                    truncate(&args, 16)
                ),
                bg.fg(theme.link),
            )));
            if !s.env_keys.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!(
                        "            env: {}  (values hidden)",
                        s.env_keys.join(", ")
                    ),
                    bg.fg(theme.text_muted),
                )));
            }
        }
    }

    // Ignored.
    if !plan.ignored.is_empty() {
        let kinds: Vec<&str> = plan.ignored.iter().map(|i| i.kind.as_str()).collect();
        lines.push(kv_line("ignored", &kinds.join(", "), theme));
    }

    // Warnings (prompt-risk, unsigned-source) in colour.
    if !plan.warnings.is_empty() {
        lines.push(Line::from(""));
        for w in &plan.warnings {
            lines.push(Line::from(Span::styled(
                format!("  ⚠ {}: {}", w.kind, truncate(&w.detail, 56)),
                bg.fg(theme.warning),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ⏎ ", bg.fg(theme.orange)),
        Span::styled("install     ", bg.fg(theme.text)),
        Span::styled("esc ", bg.fg(theme.orange)),
        Span::styled("cancel", bg.fg(theme.text_muted)),
    ]));

    frame.render_widget(Paragraph::new(lines).style(bg), area);
}

fn kv_line<'a>(key: &str, value: &str, theme: &Theme) -> Line<'a> {
    let bg = Style::default().bg(theme.bg);
    Line::from(vec![
        Span::styled(format!("  {key:<8}  "), bg.fg(theme.text_muted)),
        Span::styled(value.to_string(), bg.fg(theme.text)),
    ])
}

/// Truncate to `max` chars with an ellipsis, counting by char (not byte).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let keep = max.saturating_sub(1);
        format!("{}…", s.chars().take(keep).collect::<String>())
    }
}

/// A centered sub-rect at `pw`%×`ph`% of `area`.
fn centered(area: Rect, pw: u16, ph: u16) -> Rect {
    let w = area.width * pw / 100;
    let h = area.height * ph / 100;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w.max(1),
        height: h.max(1),
    }
}

/// RFC3339 timestamp for the lockfile record. App-layer (not lib) so reading
/// the wall clock here is fine.
fn now_rfc3339() -> String {
    humantime::format_rfc3339(std::time::SystemTime::now()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn plugin(market: &str, name: &str, desc: &str, installed: bool) -> BrowseRow {
        BrowseRow::Plugin(BrowsePlugin {
            marketplace: market.into(),
            name: name.into(),
            version: Some("1.0.0".into()),
            description: Some(desc.into()),
            installed,
        })
    }

    fn fixture() -> MarketplaceSurface {
        let mut s = MarketplaceSurface::new();
        s.browse = vec![
            BrowseRow::Header {
                marketplace: "official".into(),
                official: true,
            },
            plugin("official", "stripe", "payments", false),
            plugin("official", "airtable", "tables", true),
            BrowseRow::Header {
                marketplace: "acme".into(),
                official: false,
            },
            plugin("acme", "paykit", "invoicing", false),
        ];
        s
    }

    fn render(s: &mut MarketplaceSurface, w: u16, h: u16) -> String {
        let theme = Theme::hearth();
        let app = App::new();
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| s.render(f, f.area(), &app, &theme)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn id_is_plugins() {
        assert_eq!(MarketplaceSurface::new().id(), SurfaceId::Plugins);
    }

    #[test]
    fn browse_lists_plugins_grouped_by_marketplace() {
        let mut s = fixture();
        let out = render(&mut s, 100, 24);
        assert!(out.contains("official"), "header missing:\n{out}");
        assert!(out.contains("stripe"), "plugin missing:\n{out}");
        assert!(
            out.contains("paykit"),
            "second-market plugin missing:\n{out}"
        );
        assert!(
            out.contains("✓ installed"),
            "installed badge missing:\n{out}"
        );
    }

    #[test]
    fn filter_narrows_to_matching_plugins_and_their_headers() {
        let mut s = fixture();
        s.filter = "pay".into();
        // Only paykit matches by name; stripe's description is "payments" → also matches.
        let plugins = s.browse_plugins();
        let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"paykit"), "paykit should match: {names:?}");
        assert!(
            names.contains(&"stripe"),
            "stripe desc should match: {names:?}"
        );
        assert!(
            !names.contains(&"airtable"),
            "airtable must not match: {names:?}"
        );
    }

    #[test]
    fn cursor_moves_among_plugins_only_and_wraps() {
        let mut s = fixture();
        assert_eq!(s.selectable_len(), 3);
        assert_eq!(s.selected, 0);
        s.move_cursor(1);
        assert_eq!(s.selected, 1);
        s.move_cursor(-1);
        assert_eq!(s.selected, 0);
        // Wrap backwards from 0 → last.
        s.move_cursor(-1);
        assert_eq!(s.selected, 2);
    }

    #[tokio::test]
    async fn enter_in_browse_starts_resolve_loading() {
        // `start_resolve` calls `tokio::spawn`, so this needs a runtime. The
        // resolve task itself may never complete in the test (it would clone a
        // real repo), but the synchronous mode transition into Loading is what
        // we assert.
        let mut s = fixture();
        let mut app = App::new();
        let _ = s.handle_key(key(KeyCode::Enter), &mut app);
        assert!(matches!(s.mode, Mode::Loading(_)), "expected loading mode");
    }

    #[test]
    fn esc_with_filter_clears_filter_then_closes() {
        let mut s = fixture();
        let mut app = App::new();
        s.filter = "x".into();
        // First Esc clears the filter (consumed).
        assert!(matches!(
            s.handle_key(key(KeyCode::Esc), &mut app),
            SurfaceAction::None
        ));
        assert!(s.filter.is_empty());
        // Second Esc closes the overlay.
        assert!(matches!(
            s.handle_key(key(KeyCode::Esc), &mut app),
            SurfaceAction::CloseOverlay
        ));
    }

    #[test]
    fn tab_toggles_segment() {
        let mut s = fixture();
        let mut app = App::new();
        assert_eq!(s.seg, Segment::Browse);
        s.handle_key(key(KeyCode::Tab), &mut app);
        assert_eq!(s.seg, Segment::Installed);
        s.handle_key(key(KeyCode::Tab), &mut app);
        assert_eq!(s.seg, Segment::Browse);
    }

    #[test]
    fn a_opens_add_input_and_typing_accumulates() {
        let mut s = fixture();
        let mut app = App::new();
        s.handle_key(key(KeyCode::Char('a')), &mut app);
        assert!(matches!(s.mode, Mode::AddInput));
        s.handle_key(key(KeyCode::Char('x')), &mut app);
        s.handle_key(key(KeyCode::Char('y')), &mut app);
        assert_eq!(s.add_input, "xy");
        // Esc cancels back to list.
        s.handle_key(key(KeyCode::Esc), &mut app);
        assert!(matches!(s.mode, Mode::List));
    }

    #[test]
    fn renders_tiny_area_without_panicking() {
        let mut s = fixture();
        let _ = render(&mut s, 1, 1);
        let _ = render(&mut s, 6, 4);
    }
}
