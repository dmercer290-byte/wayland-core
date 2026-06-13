//! Workflows surface — the live drill-in monitor for ForgeFlows
//! (Dynamic Workflows) runs (ForgeFlows-Live Phase 2).
//!
//! A ForgeFlows workflow runs its stages as child agents that relay events
//! to the TUI as `SubAgentEvent { parent_call_id = "workflow:<node_id>", … }`
//! (shipped in Phase 1). Those events land flat in the SubAgents tab AND —
//! since Phase 2 — are grouped under this tab: the protocol bridge infers a
//! `WorkflowView` per workflow (MVP: one group, keyed `"workflow"`) holding
//! one `WorkflowNodeView` per stage.
//!
//! This surface is a read-only monitor with two view modes:
//!  * LIST — one row per workflow (name, node count, running/done/failed
//!    tally). `Enter` drills into the selected workflow.
//!  * DRILL — the selected workflow's nodes, each with its status, a short
//!    feed tail, and a token count. `Esc`/`Backspace` returns to LIST.
//!
//! All workflow state lives on `App::workflows`, written only by the
//! protocol bridge; the surface holds only the selection cursor + view mode.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tui::app::{App, SubAgentStatus, WorkflowNodeView, WorkflowView};
use crate::tui::surfaces::{Surface, SurfaceAction, SurfaceId};
use crate::tui::theme::Theme;

/// The number of feed-tail lines shown per node card in DRILL view.
const FEED_TAIL: usize = 3;

/// Which view the surface is showing — the workflow list or one workflow's
/// nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    /// One row per workflow.
    List,
    /// The nodes of the workflow at this index in `app.workflows`.
    Drill(usize),
}

/// The workflows live-monitor surface. Implements the frozen `Surface`
/// trait.
pub struct WorkflowsSurface {
    /// Index of the highlighted workflow row in LIST mode. Clamped to the
    /// live list length at render time so a stale cursor never points past
    /// the end.
    selected: usize,
    /// The current view mode — LIST or DRILL into a workflow.
    mode: ViewMode,
}

impl WorkflowsSurface {
    /// Construct the surface in LIST mode with the first row selected.
    pub fn new() -> Self {
        Self {
            selected: 0,
            mode: ViewMode::List,
        }
    }

    /// The selection index clamped into `0..len` (or `0` when the list is
    /// empty). Stored unclamped + resolved here so a list that shrinks
    /// between frames can never produce an out-of-bounds index.
    fn clamped_selection(&self, len: usize) -> usize {
        if len == 0 {
            0
        } else {
            self.selected.min(len - 1)
        }
    }
}

impl Default for WorkflowsSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl Surface for WorkflowsSurface {
    fn id(&self) -> SurfaceId {
        SurfaceId::Workflows
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let workflows = &app.workflows;

        // Paint the surface background so the panel reads as one screen.
        frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), area);

        let [bar_area, body_area, hint_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);

        // A DRILL into an index that no longer exists falls back to LIST.
        let mode = match self.mode {
            ViewMode::Drill(i) if i < workflows.len() => ViewMode::Drill(i),
            _ => ViewMode::List,
        };

        match mode {
            ViewMode::List => {
                render_summary_bar(frame, bar_area, workflows, theme);
                if workflows.is_empty() {
                    render_empty(frame, body_area, theme);
                } else {
                    render_list(frame, body_area, workflows, self, theme);
                }
                render_list_hint(frame, hint_area, theme);
            }
            ViewMode::Drill(i) => {
                let workflow = &workflows[i];
                render_drill_bar(frame, bar_area, workflow, theme);
                render_nodes(frame, body_area, workflow, theme);
                render_drill_hint(frame, hint_area, theme);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, app: &mut App) -> SurfaceAction {
        let len = app.workflows.len();

        match self.mode {
            ViewMode::List => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    if len > 0 {
                        let cur = self.clamped_selection(len);
                        self.selected = (cur + 1) % len;
                    }
                    SurfaceAction::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if len > 0 {
                        let cur = self.clamped_selection(len);
                        self.selected = (cur + len - 1) % len;
                    }
                    SurfaceAction::None
                }
                // Drill into the selected workflow.
                KeyCode::Enter => {
                    if len > 0 {
                        self.mode = ViewMode::Drill(self.clamped_selection(len));
                    }
                    SurfaceAction::None
                }
                // Nothing of our own to close from the list — fall through
                // to the router as `CloseOverlay` (the "primary tab, nothing
                // to close" signal the router rewrites into a Workspace
                // switch). Mirrors `SubAgentsSurface`.
                KeyCode::Esc => SurfaceAction::CloseOverlay,
                KeyCode::Char('q') => SurfaceAction::Quit,
                KeyCode::Char('p') => SurfaceAction::OpenOverlay(SurfaceId::Palette),
                KeyCode::Tab => SurfaceAction::Switch(SurfaceId::Workflows.next_tab()),
                KeyCode::BackTab => SurfaceAction::Switch(SurfaceId::Workflows.prev_tab()),
                _ => SurfaceAction::None,
            },
            ViewMode::Drill(i) => match key.code {
                // Navigate nodes within the drilled workflow.
                KeyCode::Down | KeyCode::Char('j') => {
                    let nodes = app.workflows.get(i).map(|w| w.nodes.len()).unwrap_or(0);
                    if nodes > 0 {
                        let cur = self.clamped_selection(nodes);
                        self.selected = (cur + 1) % nodes;
                    }
                    SurfaceAction::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let nodes = app.workflows.get(i).map(|w| w.nodes.len()).unwrap_or(0);
                    if nodes > 0 {
                        let cur = self.clamped_selection(nodes);
                        self.selected = (cur + nodes - 1) % nodes;
                    }
                    SurfaceAction::None
                }
                // `Esc`/`Backspace` returns from DRILL to LIST, restoring the
                // workflow-row selection to the workflow we drilled into.
                KeyCode::Esc | KeyCode::Backspace => {
                    self.mode = ViewMode::List;
                    self.selected = i;
                    SurfaceAction::None
                }
                KeyCode::Char('q') => SurfaceAction::Quit,
                KeyCode::Char('p') => SurfaceAction::OpenOverlay(SurfaceId::Palette),
                _ => SurfaceAction::None,
            },
        }
    }
}

/// Count a workflow's nodes by lifecycle status: (running, done, failed).
fn tally(workflow: &WorkflowView) -> (usize, usize, usize) {
    let mut running = 0;
    let mut done = 0;
    let mut failed = 0;
    for node in &workflow.nodes {
        match node.status {
            SubAgentStatus::Running => running += 1,
            SubAgentStatus::Done => done += 1,
            SubAgentStatus::Failed => failed += 1,
        }
    }
    (running, done, failed)
}

/// Render the LIST summary bar: total workflows + aggregate node tallies.
fn render_summary_bar(frame: &mut Frame, area: Rect, workflows: &[WorkflowView], theme: &Theme) {
    if area.height == 0 {
        return;
    }
    let (mut running, mut done, mut failed) = (0, 0, 0);
    for w in workflows {
        let (r, d, f) = tally(w);
        running += r;
        done += d;
        failed += f;
    }

    let bg = Style::default().bg(theme.surface);
    let mut spans = vec![
        Span::styled(
            " WORKFLOWS · LIVE  ",
            bg.fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} running   ", workflows.len()),
            bg.fg(theme.text_dim),
        ),
        Span::styled(format!("● {running} running"), bg.fg(theme.orange)),
        Span::styled("   ", bg),
        Span::styled(format!("● {done} done"), bg.fg(theme.success)),
    ];
    if failed > 0 {
        spans.push(Span::styled("   ", bg));
        spans.push(Span::styled(
            format!("● {failed} failed"),
            bg.fg(theme.error),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(bg), area);
}

/// Render the DRILL summary bar: the workflow name + its node tally.
fn render_drill_bar(frame: &mut Frame, area: Rect, workflow: &WorkflowView, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    let (running, done, failed) = tally(workflow);
    let bg = Style::default().bg(theme.surface);
    let mut spans = vec![
        Span::styled(
            format!(" {} · NODES  ", workflow.name),
            bg.fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} nodes   ", workflow.nodes.len()),
            bg.fg(theme.text_dim),
        ),
        Span::styled(format!("● {running} running"), bg.fg(theme.orange)),
        Span::styled("   ", bg),
        Span::styled(format!("● {done} done"), bg.fg(theme.success)),
    ];
    if failed > 0 {
        spans.push(Span::styled("   ", bg));
        spans.push(Span::styled(
            format!("● {failed} failed"),
            bg.fg(theme.error),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(bg), area);
}

/// Render the empty state — no workflow is running.
fn render_empty(frame: &mut Frame, area: Rect, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    let para = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "  No workflows running. This tab is a read-only monitor.",
            Style::default().bg(theme.bg).fg(theme.text_dim),
        )),
        Line::from(Span::styled(
            "  Run a ForgeFlows workflow and its stages appear here live while \
             they run, grouped by workflow.",
            Style::default().bg(theme.bg).fg(theme.text_muted),
        )),
    ])
    .style(Style::default().bg(theme.bg));
    frame.render_widget(para, area);
}

/// Render the LIST footer hint.
fn render_list_hint(frame: &mut Frame, area: Rect, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    let bg = Style::default().bg(theme.surface);
    let spans = vec![
        Span::styled(" ↑↓ ", bg.fg(theme.orange)),
        Span::styled("select   ", bg.fg(theme.text_muted)),
        Span::styled("⏎ ", bg.fg(theme.orange)),
        Span::styled("drill into nodes   ", bg.fg(theme.text_muted)),
        Span::styled("Esc ", bg.fg(theme.orange)),
        Span::styled("back", bg.fg(theme.text_muted)),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)).style(bg), area);
}

/// Render the DRILL footer hint.
fn render_drill_hint(frame: &mut Frame, area: Rect, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    let bg = Style::default().bg(theme.surface);
    let spans = vec![
        Span::styled(" ↑↓ ", bg.fg(theme.orange)),
        Span::styled("select node   ", bg.fg(theme.text_muted)),
        Span::styled("Esc ", bg.fg(theme.orange)),
        Span::styled("back to workflows", bg.fg(theme.text_muted)),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)).style(bg), area);
}

/// Render the LIST of workflow rows. Each row is a fixed-height card; the
/// selected card carries an accent border.
fn render_list(
    frame: &mut Frame,
    area: Rect,
    workflows: &[WorkflowView],
    surface: &WorkflowsSurface,
    theme: &Theme,
) {
    if area.height == 0 {
        return;
    }
    let selected = surface.clamped_selection(workflows.len());

    // Each row is a 4-row card (border + 2 content + border).
    const ROW: u16 = 4;
    let constraints: Vec<Constraint> = workflows.iter().map(|_| Constraint::Length(ROW)).collect();
    let rows = Layout::vertical(constraints).split(area);

    for (i, workflow) in workflows.iter().enumerate() {
        let card_area = rows[i];
        if card_area.height == 0 {
            continue;
        }
        render_workflow_card(frame, card_area, workflow, i == selected, theme);
    }
}

/// Render one workflow row in LIST mode.
fn render_workflow_card(
    frame: &mut Frame,
    area: Rect,
    workflow: &WorkflowView,
    is_selected: bool,
    theme: &Theme,
) {
    let (running, done, failed) = tally(workflow);
    // The card accent: orange while any node runs, red on a failure, green
    // when every node is done.
    let accent = if running > 0 {
        theme.orange
    } else if failed > 0 {
        theme.error
    } else {
        theme.success
    };

    let border_color = if is_selected { accent } else { theme.border };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme.surface));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let bg = Style::default().bg(theme.surface);

    // Row 1: ● name   N nodes
    let header = Line::from(vec![
        Span::styled("● ", bg.fg(accent)),
        Span::styled(
            workflow.name.clone(),
            bg.fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled("   ", bg),
        Span::styled(
            format!("{} nodes", workflow.nodes.len()),
            bg.fg(theme.text_muted),
        ),
    ]);

    // Row 2: the running/done/failed tally.
    let mut tally_spans = vec![
        Span::styled("  ", bg),
        Span::styled(format!("● {running} running"), bg.fg(theme.orange)),
        Span::styled("   ", bg),
        Span::styled(format!("● {done} done"), bg.fg(theme.success)),
    ];
    if failed > 0 {
        tally_spans.push(Span::styled("   ", bg));
        tally_spans.push(Span::styled(
            format!("● {failed} failed"),
            bg.fg(theme.error),
        ));
    }

    let para = Paragraph::new(vec![header, Line::from(tally_spans)]).style(bg);
    frame.render_widget(para, inner);
}

/// Render the DRILL view — the workflow's node cards, each with status, a
/// feed tail, and a token count.
fn render_nodes(frame: &mut Frame, area: Rect, workflow: &WorkflowView, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    if workflow.nodes.is_empty() {
        let para = Paragraph::new(Line::from(Span::styled(
            "  (this workflow has no nodes yet)",
            Style::default().bg(theme.bg).fg(theme.text_muted),
        )))
        .style(Style::default().bg(theme.bg));
        frame.render_widget(para, area);
        return;
    }

    // Each node card is the header + token line + up to FEED_TAIL feed lines,
    // bordered (so +2). Sized to fit the available rows.
    const NODE_ROWS: u16 = (2 + FEED_TAIL as u16) + 2;
    let constraints: Vec<Constraint> = workflow
        .nodes
        .iter()
        .map(|_| Constraint::Length(NODE_ROWS))
        .collect();
    let rows = Layout::vertical(constraints).split(area);

    for (i, node) in workflow.nodes.iter().enumerate() {
        let card_area = rows[i];
        if card_area.height == 0 {
            continue;
        }
        render_node_card(frame, card_area, node, theme);
    }
}

/// Render one node card in DRILL mode.
fn render_node_card(frame: &mut Frame, area: Rect, node: &WorkflowNodeView, theme: &Theme) {
    let accent = status_color(node.status, theme);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(theme.surface));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let bg = Style::default().bg(theme.surface);

    // Row 1: ● node_id (agent)   STATUS   M tokens
    let header = Line::from(vec![
        Span::styled("● ", bg.fg(accent)),
        Span::styled(
            node.node_id.clone(),
            bg.fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" ({})", node.agent_name), bg.fg(theme.text_dim)),
        Span::styled("   ", bg),
        Span::styled(status_label(node.status), bg.fg(accent)),
        Span::styled("   ", bg),
        Span::styled(fmt_tokens(node.tokens), bg.fg(theme.text_muted)),
    ]);

    let mut lines = vec![header];

    // Feed tail — the last FEED_TAIL lines, oldest of the window first.
    if node.feed.is_empty() {
        lines.push(Line::from(Span::styled(
            "  waiting for first action…",
            bg.fg(theme.text_dim),
        )));
    } else {
        let budget = inner.height.saturating_sub(1) as usize;
        let window = FEED_TAIL.min(budget.max(1));
        let start = node.feed.len().saturating_sub(window);
        for line in &node.feed[start..] {
            lines.push(Line::from(Span::styled(
                format!("  {line}"),
                bg.fg(theme.text_dim),
            )));
        }
    }

    frame.render_widget(Paragraph::new(lines).style(bg), inner);
}

/// The accent color for a node's lifecycle status (Hearth Palette).
fn status_color(status: SubAgentStatus, theme: &Theme) -> ratatui::style::Color {
    match status {
        SubAgentStatus::Running => theme.orange,
        SubAgentStatus::Done => theme.success,
        SubAgentStatus::Failed => theme.error,
    }
}

/// A short, upper-cased status label for a node header.
fn status_label(status: SubAgentStatus) -> &'static str {
    match status {
        SubAgentStatus::Running => "RUNNING",
        SubAgentStatus::Done => "DONE",
        SubAgentStatus::Failed => "FAILED",
    }
}

/// Format a token count compactly: `9.8k` past a thousand, the raw count
/// otherwise. Mirrors `subagents.rs`.
fn fmt_tokens(tokens: u64) -> String {
    if tokens >= 1000 {
        format!("{:.1}k tokens", tokens as f64 / 1000.0)
    } else {
        format!("{tokens} tokens")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::App;
    use crate::tui::fixtures;
    use crate::tui::protocol_bridge::apply_event;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Drive the `workflow_run` fixture through the REAL bridge and return
    /// the resulting `App`.
    fn app_from_fixture() -> App {
        let mut app = App::new();
        for ev in fixtures::workflow_run() {
            apply_event(&mut app, ev);
        }
        app
    }

    /// Render the surface into a `TestBackend` and return the whole frame
    /// as a single string.
    fn render(surface: &mut WorkflowsSurface, app: &App, w: u16, h: u16) -> String {
        let theme = Theme::hearth();
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        terminal
            .draw(|f| surface.render(f, f.area(), app, &theme))
            .expect("render workflows surface");
        let buf = terminal.backend().buffer();
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
    fn id_is_workflows() {
        assert_eq!(WorkflowsSurface::new().id(), SurfaceId::Workflows);
    }

    #[test]
    fn fixture_populates_two_done_workflow_nodes() {
        // The bridge fold (not state set by hand) must group the two
        // `"workflow:"`-prefixed stages under one workflow, both Done.
        let app = app_from_fixture();
        assert_eq!(app.workflows.len(), 1, "one workflow group expected");
        let wf = &app.workflows[0];
        assert_eq!(wf.nodes.len(), 2, "two nodes expected");
        assert!(wf.nodes.iter().all(|n| n.status == SubAgentStatus::Done));
        // Bridge regression guard: the SubAgents tab still sees them too.
        assert_eq!(app.session.sub_agents.len(), 2);
    }

    #[test]
    fn list_renders_workflow_name_and_node_tally() {
        let app = app_from_fixture();
        let mut surface = WorkflowsSurface::new();
        let out = render(&mut surface, &app, 100, 24);
        assert!(out.contains("WORKFLOWS"), "summary bar missing:\n{out}");
        assert!(out.contains("Workflow"), "workflow name missing:\n{out}");
        assert!(out.contains("2 nodes"), "node count missing:\n{out}");
        // Both stages are Done — the aggregate tally must say so.
        assert!(out.contains("2 done"), "done tally missing:\n{out}");
    }

    #[test]
    fn enter_drills_into_node_feed_then_esc_returns_to_list() {
        let mut app = app_from_fixture();
        let mut surface = WorkflowsSurface::new();

        // LIST: the per-node ids/feeds are NOT shown yet.
        let list = render(&mut surface, &app, 100, 24);
        assert!(
            !list.contains("stage-1"),
            "node id leaked into list view:\n{list}"
        );

        // Enter drills into the selected workflow's nodes.
        let action = surface.handle_key(key(KeyCode::Enter), &mut app);
        assert!(matches!(action, SurfaceAction::None));
        let drill = render(&mut surface, &app, 100, 24);
        assert!(
            drill.contains("stage-1"),
            "node id missing in drill:\n{drill}"
        );
        assert!(
            drill.contains("stage-2"),
            "node id missing in drill:\n{drill}"
        );
        assert!(
            drill.contains("planner"),
            "agent name missing in drill:\n{drill}"
        );
        assert!(
            drill.contains("DONE"),
            "node status missing in drill:\n{drill}"
        );
        // The node feed tail is rendered.
        assert!(
            drill.contains("plan ready") || drill.contains("Planning the change"),
            "node feed tail missing in drill:\n{drill}"
        );

        // Esc returns to LIST — node ids are gone again.
        let action = surface.handle_key(key(KeyCode::Esc), &mut app);
        assert!(matches!(action, SurfaceAction::None));
        let back = render(&mut surface, &app, 100, 24);
        assert!(
            !back.contains("stage-1"),
            "still in drill view after Esc:\n{back}"
        );
        assert!(back.contains("WORKFLOWS"), "not back on list bar:\n{back}");
    }

    #[test]
    fn empty_state_renders_without_panicking() {
        let app = App::new();
        let mut surface = WorkflowsSurface::new();
        let out = render(&mut surface, &app, 80, 20);
        assert!(
            out.contains("No workflows running"),
            "empty copy missing:\n{out}"
        );
    }

    #[test]
    fn esc_on_the_list_asks_the_router_to_close() {
        // With nothing drilled, Esc returns CloseOverlay — the router
        // rewrites that into a Workspace switch (mirrors SubAgentsSurface).
        let mut app = app_from_fixture();
        let mut surface = WorkflowsSurface::new();
        assert!(matches!(
            surface.handle_key(key(KeyCode::Esc), &mut app),
            SurfaceAction::CloseOverlay
        ));
    }

    #[test]
    fn keys_on_an_empty_list_are_inert_and_safe() {
        let mut app = App::new();
        let mut surface = WorkflowsSurface::new();
        assert!(matches!(
            surface.handle_key(key(KeyCode::Down), &mut app),
            SurfaceAction::None
        ));
        assert_eq!(surface.selected, 0);
        // Enter on an empty list must not drill into a non-existent index.
        assert!(matches!(
            surface.handle_key(key(KeyCode::Enter), &mut app),
            SurfaceAction::None
        ));
        assert_eq!(surface.mode, ViewMode::List);
    }

    #[test]
    fn renders_tiny_area_without_panicking() {
        let app = app_from_fixture();
        let mut surface = WorkflowsSurface::new();
        let _ = render(&mut surface, &app, 1, 1);
        let _ = render(&mut surface, &app, 5, 3);
        // Same for the drill view.
        let mut app2 = app_from_fixture();
        surface.handle_key(key(KeyCode::Enter), &mut app2);
        let _ = render(&mut surface, &app2, 1, 1);
        let _ = render(&mut surface, &app2, 5, 3);
    }

    #[test]
    fn tab_keys_switch_to_adjacent_surfaces() {
        let mut app = app_from_fixture();
        let mut surface = WorkflowsSurface::new();
        // Workflows is the LAST tab; Tab wraps to TABS[0] (Workspace).
        assert!(matches!(
            surface.handle_key(key(KeyCode::Tab), &mut app),
            SurfaceAction::Switch(SurfaceId::Workspace)
        ));
        // BackTab -> the previous tab (Diagnostics).
        assert!(matches!(
            surface.handle_key(key(KeyCode::BackTab), &mut app),
            SurfaceAction::Switch(SurfaceId::Diagnostics)
        ));
    }
}
