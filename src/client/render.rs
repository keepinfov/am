use std::time::Duration;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    commands::{self, Action},
    config::StatusPreset,
    runtime::{Phase, QueueState, Snapshot},
};

use super::{
    ACTIVE_BYTES, EXIT_WINDOW, Ui, View,
    completion::{Completion, Stage},
    input::Editor,
    panels,
    theme::{divider, muted, notice as notice_style, strong, warning},
    transcript::{clean, markdown_rows, tail, wrapped},
};

pub(super) fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "ready",
        Phase::Waiting => "waiting",
        Phase::Receiving => "receiving",
        Phase::StorageError => "storage error",
        Phase::Stopping => "stopping",
    }
}

fn single_line(text: &str, limit: usize) -> String {
    clean(text, limit).replace(['\n', '\t'], " ")
}

pub(super) fn status_line(ui: &Ui, snapshot: &Snapshot) -> Line<'static> {
    status_line_elapsed(ui, snapshot, snapshot.phase_started.elapsed())
}

fn status_bars(elapsed: Duration) -> String {
    const GLYPHS: [char; 8] = [
        '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
        '\u{2588}',
    ];
    // Offset triangle waves move each column by one level per 100ms frame.
    const HEIGHTS: [usize; 14] = [0, 1, 2, 3, 4, 5, 6, 7, 6, 5, 4, 3, 2, 1];
    let frame = ((elapsed.as_millis() / 100) % HEIGHTS.len() as u128) as usize;
    [0, 4, 8]
        .into_iter()
        .map(|offset| GLYPHS[HEIGHTS[(frame + offset) % HEIGHTS.len()]])
        .collect()
}

fn status_line_elapsed(ui: &Ui, snapshot: &Snapshot, elapsed: Duration) -> Line<'static> {
    let error_style = if ui.interface.status_color {
        Style::default().fg(Color::Indexed(167))
    } else {
        warning()
    };
    if ui.exit_armed.is_some_and(|at| at.elapsed() < EXIT_WINDOW) {
        return Line::styled("press ctrl+c again to exit", muted());
    }
    if ui.exiting {
        return Line::styled("Saving session...", muted());
    }
    if ui
        .stop_armed
        .is_some_and(|at| at.elapsed() <= Duration::from_millis(700))
    {
        return Line::styled("press Esc again to stop", muted());
    }
    if let Some(error) = &snapshot.error {
        let label = if snapshot.phase == Phase::StorageError {
            "storage error"
        } else {
            "response error"
        };
        return Line::styled(format!("{label}: {}", single_line(error, 512)), error_style);
    }
    if let Some((notice, at, error)) = &ui.notice
        && (*error || at.elapsed() < Duration::from_secs(3))
        && (*error || !ui.interface.status_line || snapshot.active.is_none())
    {
        return Line::styled(
            single_line(notice, 512),
            if *error { error_style } else { notice_style() },
        );
    }
    if !ui.interface.status_line {
        if snapshot.phase == Phase::StorageError {
            return Line::styled(
                format!("storage error {:.1}s", elapsed.as_secs_f32()),
                error_style,
            );
        }
        let mut parts = Vec::new();
        if ui.saving_defaults {
            parts.push("saving defaults");
        }
        if ui.draft_busy {
            parts.push("saving input");
        } else if ui.edit.is_some() {
            parts.push("editing / Enter save");
        }
        if ui.stop_pending {
            parts.push("stopping");
        } else if snapshot.paused {
            parts.push("stopped / send to continue");
        }
        parts.push(if ui.view == View::Root {
            "/help /settings"
        } else {
            "Esc back"
        });
        return Line::styled(parts.join(" \u{00b7} "), muted());
    }

    let mut phase = phase_name(snapshot.phase).to_owned();
    if snapshot.phase != Phase::Idle {
        phase.push_str(&format!(" {:.1}s", elapsed.as_secs_f32()));
    }
    let animating = snapshot.active.is_some()
        && matches!(
            snapshot.phase,
            Phase::Waiting | Phase::Receiving | Phase::Stopping
        );
    if ui.interface.status_preset == StatusPreset::Braille && animating {
        const SPINNER: [char; 8] = [
            '\u{280b}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283c}', '\u{2834}', '\u{2826}',
            '\u{2827}',
        ];
        phase = format!(
            "{} {phase}",
            SPINNER[((elapsed.as_millis() / 100) % 8) as usize]
        );
    }
    let mut parts = vec![phase];
    if ui.interface.status_preset != StatusPreset::Minimal {
        parts.push("local test fixture".into());
    }
    let queued = snapshot
        .queue
        .iter()
        .filter(|item| item.state == QueueState::Queued)
        .count();
    let editing = snapshot.queue.len() - queued;
    if queued > 0 || ui.interface.status_preset == StatusPreset::Minimal {
        parts.push(format!("{queued} queued"));
    }
    if ui.stop_pending {
        parts.push("stopping".into());
    } else if snapshot.paused {
        parts.push("stopped (send to continue)".into());
    }
    if ui.draft_busy {
        parts.push("saving input".into());
    } else if ui.edit.is_some() {
        parts.push("editing / Enter send / Esc restore".into());
    } else if editing > 0 {
        parts.push(format!("{editing} editing"));
    }
    if ui.saving_defaults {
        parts.push("saving defaults".into());
    }
    if ui.bindings_task.is_some() {
        parts.push("saving bindings".into());
    }
    if ui.view == View::Root {
        parts.push(format!("{} drafts", ui.drafts.len()));
    }
    if ui.view == View::History {
        parts.push(format!(
            "{} history / Tab pane / arrows scroll / Space fold / Esc back",
            ui.keys.label(Action::History)
        ));
    } else if ui.view != View::Root {
        parts.push("Esc back".into());
    } else if ui.interface.status_preset != StatusPreset::Minimal {
        parts.push("/help".into());
    }
    if ui.view == View::Root && ui.completion_active {
        parts.push(if ui.pager_selected {
            "Enter confirms only".into()
        } else {
            "Tab selects / Enter closes only".into()
        });
    } else if ui.view == View::Root {
        let editor = ui.editor().text();
        let source = if ui.palette && ui.commands_focus && !editor.starts_with('/') {
            format!("/{editor}")
        } else {
            editor.to_owned()
        };
        if matches!(commands::parse(&source), Ok(Some(_))) {
            parts.push("Enter runs command".into());
        }
    }
    let style = if ui.interface.status_color {
        muted().fg(match snapshot.phase {
            Phase::StorageError => Color::Indexed(167),
            Phase::Waiting | Phase::Receiving => Color::Indexed(109),
            Phase::Stopping => Color::Indexed(179),
            Phase::Idle => Color::Indexed(245),
        })
    } else {
        muted()
    };
    let text = parts.join(" \u{00b7} ");
    if ui.interface.status_preset == StatusPreset::Bars && animating {
        Line::from(vec![
            Span::styled(
                status_bars(elapsed),
                Style::default().fg(Color::Indexed(255)),
            ),
            Span::styled(format!(" {text}"), style),
        ])
    } else {
        Line::styled(text, style)
    }
}

struct RootLayout {
    live: Rect,
    queue: Rect,
    composer: Rect,
    command_title: Rect,
    command: Rect,
    menu: Rect,
    top_divider: Rect,
    bottom_divider: Rect,
    status: Rect,
}

#[cfg(test)]
fn queue_rows(snapshot: &Snapshot, width: u16, budget: u16) -> Vec<Line<'static>> {
    queue_preview_rows(
        snapshot
            .queue
            .iter()
            .filter(|item| item.state == QueueState::Queued)
            .map(|item| (item.text.as_str(), false)),
        width,
        budget,
    )
}

pub(super) fn projected_queue_rows(
    ui: &Ui,
    snapshot: &Snapshot,
    width: u16,
    budget: u16,
    plan: Option<&super::state::PublicationPlan>,
) -> Vec<Line<'static>> {
    let projection = ui.live_projection(plan);
    queue_preview_rows(
        ui.queue_texts(snapshot, &projection).into_iter(),
        width,
        budget,
    )
}

fn queue_preview_rows<'a>(
    previews: impl DoubleEndedIterator<Item = (&'a str, bool)>,
    width: u16,
    budget: u16,
) -> Vec<Line<'static>> {
    let mut messages = Vec::new();
    let mut remaining = usize::from(budget);
    for (text, prefix_truncated) in previews.rev() {
        if remaining == 0 {
            break;
        }
        let (text, truncated) = super::transcript::sanitized(text, 4096);
        let mut rows = wrapped(&text, usize::from(width.saturating_sub(2)).max(1));
        let limit = remaining.min(2);
        let omitted = prefix_truncated || truncated || rows.len() > limit;
        rows.truncate(limit);
        if omitted && let Some(last) = rows.last_mut() {
            let content = last.to_string();
            let available = usize::from(width.saturating_sub(3));
            let mut shortened = String::new();
            for grapheme in Line::raw(content).styled_graphemes(Style::default()) {
                if Line::raw(&shortened).width() + Line::raw(grapheme.symbol).width() > available {
                    break;
                }
                shortened.push_str(grapheme.symbol);
            }
            *last = Line::raw(format!("{shortened}\u{2026}"));
        }
        for row in &mut rows {
            row.spans.insert(0, Span::raw("\u{250b} "));
            *row = row.clone().style(muted());
        }
        remaining -= rows.len();
        messages.push(rows);
    }
    messages.into_iter().rev().flatten().collect()
}

fn editor_width(width: u16) -> u16 {
    if width > 2 { width - 2 } else { width }
}

fn live_rows(ui: &Ui, snapshot: &Snapshot, width: u16) -> Vec<Line<'static>> {
    projected_live_rows(ui, snapshot, width, None)
}

fn projected_live_rows(
    ui: &Ui,
    snapshot: &Snapshot,
    width: u16,
    plan: Option<&super::state::PublicationPlan>,
) -> Vec<Line<'static>> {
    let projection = ui.live_projection(plan);
    let mut rows = Ui::projected_notice_rows(&projection, width as usize);
    rows.truncate(2);
    for text in ui.live_texts(snapshot, &projection) {
        let text = clean(tail(text, ACTIVE_BYTES), ACTIVE_BYTES);
        let mut response = markdown_rows(&text, width.saturating_sub(2) as usize);
        while response
            .last()
            .is_some_and(|line| line.to_string().trim().is_empty())
        {
            response.pop();
        }
        response.drain(..response.len().saturating_sub(2));
        if !rows.is_empty() && !response.is_empty() {
            rows.push(Line::from(""));
        }
        rows.extend(response);
    }
    rows
}

/// Root owns only the visible tail, composer, blank separators and optional pager.
pub(super) fn inline_height(ui: &Ui, snapshot: &Snapshot, width: u16, terminal_height: u16) -> u16 {
    projected_inline_height(ui, snapshot, width, terminal_height, None)
}

pub(super) fn publication_height(
    ui: &Ui,
    snapshot: &Snapshot,
    width: u16,
    terminal_height: u16,
    plan: &super::state::PublicationPlan,
) -> u16 {
    projected_inline_height(ui, snapshot, width, terminal_height, Some(plan))
}

fn projected_inline_height(
    ui: &Ui,
    snapshot: &Snapshot,
    width: u16,
    terminal_height: u16,
    plan: Option<&super::state::PublicationPlan>,
) -> u16 {
    if ui.view == View::Root {
        projected_root_layout(
            ui,
            snapshot,
            Rect::new(0, 0, width, terminal_height),
            ui.completion(snapshot).as_ref(),
            plan,
        )
        .status
        .bottom()
    } else if ui.full_history {
        terminal_height
    } else {
        projected_panel_layout(ui, snapshot, Rect::new(0, 0, width, terminal_height), plan)
            .status
            .bottom()
    }
}

fn root_layout(
    ui: &Ui,
    snapshot: &Snapshot,
    area: Rect,
    completion: Option<&Completion>,
) -> RootLayout {
    projected_root_layout(ui, snapshot, area, completion, None)
}

fn projected_root_layout(
    ui: &Ui,
    snapshot: &Snapshot,
    area: Rect,
    completion: Option<&Completion>,
    plan: Option<&super::state::PublicationPlan>,
) -> RootLayout {
    let status_height = area.height.min(1);
    let available = area.height.saturating_sub(status_height);
    let completion = completion.filter(|_| ui.completion_visible());
    let menu_needed = completion.is_some() || ui.pending_send.is_some();
    let menu_min = u16::from(menu_needed && available >= 2);
    let command_height = if ui.palette {
        (ui.palette_query
            .rows(editor_width(area.width), 3)
            .lines
            .len() as u16)
            .max(1)
            .min(available.saturating_sub(menu_min + 1))
    } else {
        0
    };
    let command_title_height = u16::from(ui.palette && available > command_height + menu_min + 1);
    let composer_height = (ui.draft.rows(editor_width(area.width), 3).lines.len() as u16)
        .max(1)
        .min(available.saturating_sub(menu_min + command_height + command_title_height));
    // On small terminals retain input/status and a choice before spending rows on gaps.
    let mut remaining = available
        .saturating_sub(composer_height + command_height + command_title_height + menu_min);
    let after_input_height = remaining.min(1);
    remaining -= after_input_height;
    let before_input_height = remaining.min(1);
    remaining -= before_input_height;
    let after_menu_height = if menu_needed { remaining.min(1) } else { 0 };
    remaining -= after_menu_height;
    let queue_height = (projected_queue_rows(ui, snapshot, area.width, 6, plan).len() as u16)
        .min(remaining.saturating_sub(1));
    let queue_gap = u16::from(queue_height > 0);
    remaining = remaining.saturating_sub(queue_height + queue_gap);
    let live_height =
        (projected_live_rows(ui, snapshot, area.width, plan).len() as u16).min(remaining);
    remaining -= live_height;
    let menu_height = if ui.pending_send.is_some() {
        menu_min
    } else if completion.is_some() {
        (menu_min + remaining).min(9)
    } else {
        0
    };
    let live = Rect::new(area.x, area.y, area.width, live_height);
    let queue = Rect::new(
        area.x,
        live.bottom() + before_input_height,
        area.width,
        queue_height,
    );
    let composer = Rect::new(
        area.x,
        queue.bottom() + queue_gap,
        area.width,
        composer_height,
    );
    let command_title = Rect::new(area.x, composer.bottom(), area.width, command_title_height);
    let command = Rect::new(area.x, command_title.bottom(), area.width, command_height);
    let after_input = Rect::new(area.x, command.bottom(), area.width, after_input_height);
    let menu = Rect::new(area.x, after_input.bottom(), area.width, menu_height);
    let after_menu = Rect::new(area.x, menu.bottom(), area.width, after_menu_height);
    RootLayout {
        live,
        queue,
        composer,
        command_title,
        command,
        menu,
        top_divider: if menu_needed {
            after_input
        } else {
            Rect::default()
        },
        bottom_divider: if menu_needed {
            after_menu
        } else {
            Rect::default()
        },
        status: Rect::new(area.x, after_menu.bottom(), area.width, status_height),
    }
}

fn menu_window(ui: &Ui, completion: &Completion, height: u16) -> (usize, usize, bool) {
    let count = completion.items.len();
    let footer = height > 1 || ui.pager_search.is_some();
    let header = if height >= 4 { 2 } else { 0 };
    let available = usize::from(height.saturating_sub(u16::from(footer) + header)).min(6);
    let overflow = count > available && height > 1;
    let visible = available.min(count);
    let selected = if ui.pager_selected {
        ui.completion_selected
    } else {
        0
    };
    let start = selected
        .min(count.saturating_sub(1))
        .saturating_sub(visible.saturating_sub(1));
    (start, visible, overflow)
}

/// Whether the complete selected label is on screen; the caller owns activation.
pub(super) fn selection_visible(ui: &Ui, snapshot: &Snapshot, width: u16, height: u16) -> bool {
    if ui.view == View::Bindings && ui.binding_key.is_some() {
        let Some(completion) = ui.binding_completion(snapshot) else {
            return false;
        };
        let body = panel_layout(ui, snapshot, Rect::new(0, 0, width, height)).body;
        let (_, menu) = binding_layout(ui, body);
        return ui.pager_selected && menu_selection_visible(ui, &completion, menu);
    }
    if ui.view != View::Root || !ui.completion_visible() || width < 24 || height < 4 {
        return false;
    }
    let Some(completion) = ui.completion(snapshot) else {
        return false;
    };
    if !ui.pager_selected
        && !(ui.palette
            && !ui.completion_active
            && completion.stage == Stage::Command
            && ui.completion_selected == 0)
    {
        return false;
    }
    let layout = root_layout(
        ui,
        snapshot,
        Rect::new(0, 0, width, height),
        Some(&completion),
    );
    menu_selection_visible(ui, &completion, layout.menu)
}

fn menu_selection_visible(ui: &Ui, completion: &Completion, area: Rect) -> bool {
    let selected = ui.completion_selected;
    let Some(item) = completion.items.get(selected) else {
        return false;
    };
    let (start, visible, _) = menu_window(ui, completion, area.height);
    area.width >= 24
        && selected >= start
        && selected < start + visible
        && Line::raw(single_line(&item.label, item.label.len())).width() + 2
            <= usize::from(area.width)
}

/// Inline readers also reserve a blank between the retained composer and status.
pub(super) fn history_height(area_height: u16, full: bool) -> u16 {
    area_height.saturating_sub(if full { 3 } else { 6 })
}

struct PanelLayout {
    notice: Rect,
    queue: Rect,
    composer: Rect,
    top_divider: Rect,
    title: Rect,
    body: Rect,
    bottom_divider: Rect,
    status: Rect,
}

fn panel_layout(ui: &Ui, snapshot: &Snapshot, area: Rect) -> PanelLayout {
    projected_panel_layout(ui, snapshot, area, None)
}

fn projected_panel_layout(
    ui: &Ui,
    snapshot: &Snapshot,
    area: Rect,
    plan: Option<&super::state::PublicationPlan>,
) -> PanelLayout {
    let status_height = area.height.min(1);
    let mut remaining = area.height.saturating_sub(status_height);
    let editor = if ui.view == View::Help && ui.help_detail.is_none() {
        &ui.help_query
    } else {
        &ui.draft
    };
    let composer_height = (editor.rows(editor_width(area.width), 3).lines.len() as u16)
        .max(1)
        .min(remaining);
    remaining -= composer_height;
    let top_height = remaining.min(1);
    remaining -= top_height;
    let title_height = remaining.min(1);
    remaining -= title_height;
    let bottom_height = remaining.min(1);
    remaining -= bottom_height;
    let blank_height = u16::from(remaining >= 2);
    remaining -= blank_height;
    let before_input_height = remaining.min(1);
    remaining -= before_input_height;
    let desired = match ui.view {
        View::Settings | View::Help => panels::body_rows(ui),
        View::Queue => snapshot.queue.len().clamp(1, 12) as u16,
        View::Drafts => ui.drafts.len().clamp(1, 12) as u16,
        View::Bindings => {
            if ui.binding_key.is_some() || ui.binding_capture {
                12
            } else {
                ui.keys.macros.len().clamp(1, 12) as u16
            }
        }
        _ => 14,
    };
    let body_height = desired.min(remaining);
    remaining -= body_height;
    let queue_height = (projected_queue_rows(ui, snapshot, area.width, 6, plan).len() as u16)
        .min(remaining.saturating_sub(1));
    let queue_gap = u16::from(queue_height > 0);
    remaining = remaining.saturating_sub(queue_height + queue_gap);
    let projection = ui.live_projection(plan);
    let notice_height = (Ui::projected_notice_rows(&projection, area.width as usize).len() as u16)
        .min(2)
        .min(remaining);
    let notice = Rect::new(area.x, area.y, area.width, notice_height);
    let queue = Rect::new(
        area.x,
        notice.bottom() + before_input_height,
        area.width,
        queue_height,
    );
    let composer = Rect::new(
        area.x,
        queue.bottom() + queue_gap,
        area.width,
        composer_height,
    );
    let top_divider = Rect::new(area.x, composer.bottom(), area.width, top_height);
    let title = Rect::new(area.x, top_divider.bottom(), area.width, title_height);
    let body = Rect::new(
        area.x,
        title.bottom() + blank_height,
        area.width,
        body_height,
    );
    let bottom_divider = Rect::new(area.x, body.bottom(), area.width, bottom_height);
    let status = Rect::new(area.x, bottom_divider.bottom(), area.width, status_height);
    PanelLayout {
        notice,
        queue,
        composer,
        top_divider,
        title,
        body,
        bottom_divider,
        status,
    }
}

fn binding_layout(ui: &Ui, body: Rect) -> (Rect, Rect) {
    let label_height = body.height.min(1);
    let available = body.height - label_height;
    let menu_min = u16::from(ui.completion_active && available >= 2);
    let input_height = (ui
        .binding_commands
        .rows(editor_width(body.width), 3)
        .lines
        .len() as u16)
        .max(1)
        .min(available.saturating_sub(menu_min));
    let input = Rect::new(body.x, body.y + label_height, body.width, input_height);
    let gap = available.saturating_sub(input_height + menu_min).min(1);
    let menu = Rect::new(
        body.x,
        input.bottom() + gap,
        body.width,
        if ui.completion_active {
            available.saturating_sub(input_height + gap).min(9)
        } else {
            0
        },
    );
    (input, menu)
}

fn render_composer(
    frame: &mut Frame,
    ui: &Ui,
    editor: &Editor,
    area: Rect,
    completion: Option<&Completion>,
    active: bool,
) {
    if area.is_empty() {
        return;
    }
    let rail_width = if area.width > 2 { 2 } else { 0 };
    let mut rows = editor.rows(editor_width(area.width), area.height.min(3));
    if active
        && !ui.completion_active
        && editor.cursor() == editor.text().len()
        && let Some(completion) = completion
        && let Some(ghost) = completion.ghost(editor.text(), editor.cursor(), 0)
        && let Some((x, y)) = rows.cursor
        && let Some(line) = rows.lines.get_mut(usize::from(y))
    {
        let remaining = usize::from(area.width.saturating_sub(rail_width + x));
        let ghost = single_line(ghost, 512);
        let mut suffix = String::new();
        let mut used = 0;
        for grapheme in Line::raw(ghost).styled_graphemes(Style::default()) {
            let cells = Line::raw(grapheme.symbol).width();
            if used + cells > remaining {
                break;
            }
            suffix.push_str(grapheme.symbol);
            used += cells;
        }
        line.spans.push(Span::styled(suffix, muted()));
    }
    let last = rows.lines.len().saturating_sub(1);
    for (index, line) in rows.lines.iter_mut().enumerate() {
        if rail_width > 0 {
            let rail = if index == 0 && rows.hidden_above > 0 {
                "\u{2503}\u{2191}"
            } else if index == last && rows.hidden_below > 0 {
                "\u{2503}\u{2193}"
            } else {
                "\u{2503} "
            };
            line.spans.insert(
                0,
                Span::styled(rail, if active { strong() } else { muted() }),
            );
        }
    }
    frame.render_widget(
        Paragraph::new(rows.lines).style(if active { Style::default() } else { muted() }),
        area,
    );
    if active
        && !ui.exiting
        && !ui.draft_busy
        && !ui.saving_defaults
        && ui.pager_search.is_none()
        && let Some((x, y)) = rows.cursor
    {
        frame.set_cursor_position((area.x + rail_width + x, area.y + y));
    }
}

fn render_menu(frame: &mut Frame, ui: &Ui, completion: &Completion, area: Rect) {
    if area.is_empty() {
        return;
    }
    let count = completion.items.len();
    let mut lines = Vec::new();
    let (start, visible, overflow) = menu_window(ui, completion, area.height);
    let header = if area.height >= 4 { 2 } else { 0 };
    if header > 0 {
        let title = match completion.stage {
            Stage::Command => "Commands",
            Stage::Setting | Stage::Value => "Settings",
            Stage::History => "History",
            Stage::MessageId => "Messages",
            Stage::Option => "Options",
        };
        lines.push(Line::styled(format!("{title} {count}"), strong()));
        lines.push(Line::from(""));
    }
    let label_width = completion
        .items
        .iter()
        .map(|item| Line::raw(&item.label).width())
        .max()
        .unwrap_or(0);
    if count == 0 && area.height > u16::from(ui.pager_search.is_some()) {
        lines.push(Line::styled("  No matches", muted()));
    } else {
        for (index, item) in completion
            .items
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
        {
            let selected = ui.pager_selected && index == ui.completion_selected;
            let style = if selected { strong() } else { muted() };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {}", single_line(&item.label, item.label.len())),
                    style,
                ),
                Span::styled(
                    format!(
                        "{}{}",
                        " ".repeat(label_width.saturating_sub(Line::raw(&item.label).width()) + 4),
                        single_line(&item.description, 256)
                    ),
                    style,
                ),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
    if area.height > 1 && ui.pager_search.is_none() && ui.completion_active {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(
                    "  {}{}",
                    if overflow {
                        format!("{}-{}/{count} / ", start + 1, start + visible)
                    } else {
                        String::new()
                    },
                    ""
                ),
                muted(),
            )),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
    if let Some(search) = &ui.pager_search {
        let prefix = "search: ";
        let prefix_width = (prefix.len() as u16).min(area.width.saturating_sub(1));
        let rows = search.rows(area.width - prefix_width, 1);
        let mut line = rows.lines.into_iter().next().unwrap_or_default();
        line.spans.insert(
            0,
            Span::styled(prefix[..usize::from(prefix_width)].to_owned(), muted()),
        );
        let y = area.bottom() - 1;
        frame.render_widget(Paragraph::new(line), Rect::new(area.x, y, area.width, 1));
        if !ui.exiting
            && !ui.draft_busy
            && !ui.saving_defaults
            && let Some((x, _)) = rows.cursor
        {
            frame.set_cursor_position((area.x + prefix_width + x, y));
        }
    }
}

fn render_divider(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::styled(
            "\u{2500}".repeat(area.width as usize),
            divider(),
        )),
        area,
    );
}

fn completion_hint(ui: &Ui, width: u16) -> &'static str {
    if !ui.completion_active {
        if width >= 50 {
            "Up/Down navigate / Enter opens / Esc closes"
        } else {
            "Arrows / Enter open / Esc"
        }
    } else if width >= 70 {
        "Tab next     Shift+Tab previous     Enter accepts only     Esc restores"
    } else if width >= 40 {
        "Tab next / Enter accepts only / Esc restores"
    } else {
        "Tab / Enter accept / Esc"
    }
}

fn panel_feedback(ui: &Ui, snapshot: &Snapshot) -> bool {
    snapshot.error.is_some()
        || ui
            .notice
            .as_ref()
            .is_some_and(|notice| notice.2 || notice.1.elapsed() < Duration::from_secs(3))
        || ui.saving_defaults
        || ui.bindings_task.is_some()
}

pub(super) fn render(frame: &mut Frame, ui: &Ui, snapshot: &Snapshot) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    if ui.view == View::Root {
        let completion = ui.completion(snapshot);
        let layout = root_layout(ui, snapshot, area, completion.as_ref());
        let rows = live_rows(ui, snapshot, area.width);
        let start = rows.len().saturating_sub(layout.live.height as usize);
        frame.render_widget(Paragraph::new(rows[start..].to_vec()), layout.live);
        frame.render_widget(
            Paragraph::new(projected_queue_rows(
                ui,
                snapshot,
                area.width,
                layout.queue.height,
                None,
            )),
            layout.queue,
        );
        render_composer(
            frame,
            ui,
            &ui.draft,
            layout.composer,
            completion
                .as_ref()
                .filter(|_| !ui.palette || !ui.commands_focus),
            !ui.palette || !ui.commands_focus,
        );
        if ui.palette {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!(
                        "commands{} / {} focus / Esc or /exit closes",
                        if ui.palette_pinned { " (pinned)" } else { "" },
                        ui.keys.label(Action::FocusInput)
                    ),
                    muted(),
                )),
                layout.command_title,
            );
            render_composer(
                frame,
                ui,
                &ui.palette_query,
                layout.command,
                completion.as_ref().filter(|_| ui.commands_focus),
                ui.commands_focus,
            );
        }
        if ui.completion_visible()
            && let Some(completion) = &completion
        {
            render_menu(frame, ui, completion, layout.menu);
        }
        render_divider(frame, layout.top_divider);
        render_divider(frame, layout.bottom_divider);
        if let Some(pending) = &ui.pending_send {
            let choices = ["hold", "run queue", "cancel"];
            let line = Line::from(
                choices
                    .iter()
                    .enumerate()
                    .map(|(index, text)| {
                        Span::styled(
                            format!("  {text}    "),
                            if index == pending.selected {
                                strong()
                            } else {
                                muted()
                            },
                        )
                    })
                    .collect::<Vec<_>>(),
            );
            frame.render_widget(Paragraph::new(line), layout.menu);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!(
                        "Queue paused ({} waiting) / arrows choose / Enter confirms / Esc cancels",
                        snapshot.queue.len()
                    ),
                    notice_style(),
                )),
                layout.status,
            );
            return;
        }
        let hint = completion_hint(ui, area.width);
        frame.render_widget(
            Paragraph::new(
                if ui.completion_visible()
                    && snapshot.error.is_none()
                    && !ui.notice.as_ref().is_some_and(|notice| notice.2)
                {
                    Line::styled(hint, muted())
                } else {
                    status_line(ui, snapshot)
                },
            ),
            layout.status,
        );
        return;
    }

    let full = ui.view == View::History && ui.full_history;
    if full {
        let title = Rect::new(area.x, area.y, area.width, area.height.min(1));
        let divider = Rect::new(
            area.x,
            title.bottom(),
            area.width,
            area.height.saturating_sub(1).min(1),
        );
        let status = Rect::new(
            area.x,
            area.bottom().saturating_sub(1),
            area.width,
            area.height.min(1),
        );
        let body = Rect::new(
            area.x,
            divider.bottom(),
            area.width,
            history_height(area.height, true),
        );
        frame.render_widget(
            Paragraph::new(Line::styled("History / messages + actions", strong())),
            title,
        );
        render_divider(frame, divider);
        ui.history_browser.render(frame, body, &ui.journal);
        frame.render_widget(
            Paragraph::new(
                if snapshot.error.is_some() || ui.notice.as_ref().is_some_and(|notice| notice.2) {
                    status_line(ui, snapshot)
                } else {
                    Line::styled(ui.history_browser.hint(status.width), muted())
                },
            ),
            status,
        );
        return;
    }
    let layout = panel_layout(ui, snapshot, area);
    let title_area = layout.title;
    let body = layout.body;
    let body_height = body.height;
    let composer = layout.composer;
    let status = layout.status;
    let title = match ui.view {
        View::History => "History".to_owned(),
        View::Queue => format!("Queue {}", snapshot.queue.len()),
        View::Help => "Commands".into(),
        View::Settings => "Settings".into(),
        View::Details => "Details".into(),
        View::Drafts => format!("Drafts {}", ui.drafts.len()),
        View::Bindings => format!("Bindings {}", ui.keys.macros.len()),
        View::Root => unreachable!(),
    };
    frame.render_widget(Paragraph::new(Line::styled(title, strong())), title_area);
    let mut notices = ui.native_tail_rows(area.width as usize);
    notices.truncate(layout.notice.height as usize);
    frame.render_widget(Paragraph::new(notices), layout.notice);
    frame.render_widget(
        Paragraph::new(projected_queue_rows(
            ui,
            snapshot,
            area.width,
            layout.queue.height,
            None,
        )),
        layout.queue,
    );
    render_composer(
        frame,
        ui,
        if ui.view == View::Help && ui.help_detail.is_none() {
            &ui.help_query
        } else {
            &ui.draft
        },
        composer,
        None,
        ui.view == View::Help && ui.help_detail.is_none(),
    );
    render_divider(frame, layout.top_divider);
    render_divider(frame, layout.bottom_divider);
    if matches!(ui.view, View::Settings | View::Help) {
        if ui.view == View::Settings {
            panels::render_settings(frame, ui, title_area, body);
        } else {
            panels::render_help(frame, ui, title_area, body);
        }
        let line = if panel_feedback(ui, snapshot) {
            status_line(ui, snapshot)
        } else {
            Line::styled(panels::hint(ui, status.width), muted())
        };
        frame.render_widget(Paragraph::new(line), status);
        return;
    }
    if ui.view == View::Bindings && (ui.binding_capture || ui.binding_key.is_some()) {
        let label = if ui.binding_capture {
            "Press the shortcut to bind / Esc cancels".to_owned()
        } else {
            format!(
                "{} / Tab completes / ; chains / Enter saves / Esc cancels",
                ui.binding_key.as_deref().unwrap_or("")
            )
        };
        frame.render_widget(Paragraph::new(Line::styled(label, strong())), body);
        if !ui.binding_capture {
            let completion = ui.binding_completion(snapshot);
            let (edit_area, menu_area) = binding_layout(ui, body);
            render_composer(
                frame,
                ui,
                &ui.binding_commands,
                edit_area,
                completion.as_ref(),
                true,
            );
            if ui.completion_active
                && let Some(completion) = completion
            {
                render_menu(frame, ui, &completion, menu_area);
            }
        }
        frame.render_widget(
            Paragraph::new(
                if ui.completion_active
                    && snapshot.error.is_none()
                    && !ui.notice.as_ref().is_some_and(|notice| notice.2)
                {
                    Line::styled(completion_hint(ui, status.width), muted())
                } else {
                    status_line(ui, snapshot)
                },
            ),
            status,
        );
        return;
    }
    let height = usize::from(body_height);
    let lines = match ui.view {
        View::Drafts => {
            if ui.drafts.is_empty() {
                vec![Line::styled(
                    if ui.drafts_busy {
                        "Loading drafts..."
                    } else {
                        "No saved drafts. Use /draft save from the command field."
                    },
                    muted(),
                )]
            } else {
                ui.drafts
                    .iter()
                    .enumerate()
                    .skip(ui.selected.saturating_sub(height.saturating_sub(1)))
                    .take(height)
                    .map(|(index, draft)| {
                        Line::styled(
                            format!(
                                "  {}    {}",
                                &draft.id[..draft.id.len().min(8)],
                                single_line(&draft.text, 256)
                            ),
                            if index == ui.selected {
                                strong()
                            } else {
                                muted()
                            },
                        )
                    })
                    .collect()
            }
        }
        View::Bindings => {
            if ui.keys.macros.is_empty() {
                vec![Line::styled(
                    "No command bindings. Press n to capture a shortcut.",
                    muted(),
                )]
            } else {
                ui.keys
                    .macros
                    .iter()
                    .enumerate()
                    .skip(ui.selected.saturating_sub(height.saturating_sub(1)))
                    .take(height)
                    .map(|(index, binding)| {
                        Line::styled(
                            format!(
                                "  {}    {}",
                                binding.key,
                                single_line(&binding.commands.join(" ; "), 4096)
                            ),
                            if index == ui.selected {
                                strong()
                            } else {
                                muted()
                            },
                        )
                    })
                    .collect()
            }
        }
        View::History => {
            if ui.history_rows.is_empty() {
                vec![Line::styled(
                    if ui.history_busy {
                        "  loading history..."
                    } else {
                        "  No messages yet."
                    },
                    muted(),
                )]
            } else {
                ui.history_rows.iter().take(height).cloned().collect()
            }
        }
        View::Queue => {
            let start = ui.selected.saturating_sub(height.saturating_sub(1));
            if snapshot.queue.is_empty() {
                vec![Line::styled("  Nothing queued.", muted())]
            } else {
                snapshot
                    .queue
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(height)
                    .map(|(index, item)| {
                        let state = match item.state {
                            QueueState::Queued => "queued",
                            QueueState::Editing => "editing",
                        };
                        let style = if index == ui.selected {
                            strong()
                        } else {
                            muted()
                        };
                        Line::from(vec![
                            Span::styled(format!("  {}  ", index + 1), style),
                            Span::styled(
                                format!(
                                    "{state:8} {}  ",
                                    single_line(queue_id(item, &snapshot.queue), 128)
                                ),
                                muted(),
                            ),
                            Span::styled(single_line(&item.text, 256), style),
                        ])
                    })
                    .collect()
            }
        }
        View::Details => {
            let mut details = vec![
                ("Provider", "local test fixture / no API or tools".into()),
                ("Session", single_line(&snapshot.session_id, 128)),
                ("Agent", single_line(&snapshot.agent_id, 128)),
                ("Conversation", single_line(&snapshot.conversation_id, 128)),
                ("Event cursor", snapshot.cursor.to_string()),
                (
                    "Phase",
                    format!(
                        "{} / {:.1}s",
                        phase_name(snapshot.phase),
                        snapshot.phase_started.elapsed().as_secs_f32()
                    ),
                ),
                (
                    "Last event",
                    format!("{:.1}s ago", snapshot.last_event.elapsed().as_secs_f32()),
                ),
                (
                    "Queue",
                    format!(
                        "{} messages / {}",
                        snapshot.queue.len(),
                        if snapshot.paused { "paused" } else { "running" }
                    ),
                ),
                ("Reopened", snapshot.recovered.to_string()),
                ("Client commands", format!("{} pending", ui.outstanding)),
            ];
            if let Some(active) = &snapshot.active {
                details.push(("Attempt", single_line(&active.attempt_id, 128)));
                details.push((
                    "Response",
                    format!(
                        "{} bytes / {} committed{}",
                        active.text.len(),
                        active.durable_bytes,
                        if active.truncated { " / truncated" } else { "" }
                    ),
                ));
            }
            let mut lines = details
                .into_iter()
                .flat_map(|(label, value)| {
                    wrapped(&format!("  {label:16}{value}"), area.width as usize)
                })
                .collect::<Vec<_>>();
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "  UI preferences / effective sources",
                strong(),
            ));
            for setting in crate::settings::catalog(&ui.interface) {
                let source = ui
                    .setting_sources
                    .get(setting.key)
                    .map(String::as_str)
                    .unwrap_or("Unknown");
                lines.extend(wrapped(
                    &format!(
                        "  {} = {} / {}",
                        setting.key,
                        setting.value,
                        single_line(source, 512)
                    ),
                    area.width as usize,
                ));
            }
            lines.extend(wrapped(
                &format!("  Config: {}", single_line(&ui.config_source, 512)),
                area.width as usize,
            ));
            lines.extend(wrapped(
                &format!(
                    "  Defaults: {}",
                    single_line(&ui.preferences_path.display().to_string(), 512)
                ),
                area.width as usize,
            ));
            lines.extend(wrapped(
                "  Saved UI defaults affect future clients; explicit config fields override them.",
                area.width as usize,
            ));
            lines
        }
        View::Root | View::Help | View::Settings => unreachable!(),
    };
    let lines = if matches!(ui.view, View::Help | View::Details) {
        let start = (ui.info_scroll as usize).min(lines.len().saturating_sub(height));
        lines[start..(start + height).min(lines.len())].to_vec()
    } else {
        lines
    };
    frame.render_widget(Paragraph::new(lines), body);
    let hint = match ui.view {
        View::Queue => "Up/Down select / Enter edit / Delete cancel / Esc back",
        View::Drafts => "Up/Down select / Enter load / Delete remove / Esc back",
        View::Bindings => {
            "Up/Down select / n add / Enter edit / Delete remove / d defaults / Esc back"
        }
        View::Details => "Up/Down scroll / PgUp/PgDn page / Esc back",
        _ => "Esc back",
    };
    frame.render_widget(
        Paragraph::new(if panel_feedback(ui, snapshot) {
            status_line(ui, snapshot)
        } else {
            Line::styled(hint, muted())
        }),
        status,
    );
}

pub(super) fn queue_id<'a>(
    item: &'a crate::runtime::QueueItem,
    queue: &[crate::runtime::QueueItem],
) -> &'a str {
    for length in [8, 12, 16, 36] {
        let prefix = item.id.get(..length).unwrap_or(&item.id);
        if queue
            .iter()
            .filter(|other| other.id.starts_with(prefix))
            .count()
            == 1
        {
            return prefix;
        }
    }
    &item.id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::Modifier;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
    use std::time::Instant;

    fn fixture() -> (Ui, Snapshot) {
        let snapshot = Snapshot {
            session_id: "session-hidden".into(),
            agent_id: "agent-hidden".into(),
            conversation_id: "conversation-hidden".into(),
            phase: Phase::Idle,
            phase_started: Instant::now(),
            last_event: Instant::now(),
            active: None,
            queue: Vec::new(),
            paused: false,
            recovered: false,
            cursor: 0,
            error: None,
        };
        (
            Ui::new(&crate::config::Config::default(), &snapshot),
            snapshot,
        )
    }

    fn draw(ui: &Ui, snapshot: &Snapshot, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, ui, snapshot)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn plain(buffer: &Buffer) -> String {
        buffer.content.iter().map(|cell| cell.symbol()).collect()
    }

    fn key(ui: &mut Ui, snapshot: &Snapshot, code: KeyCode, modifiers: KeyModifiers) {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        ui.key(KeyEvent::new(code, modifiers), snapshot, &tx, 80, 14)
            .unwrap();
    }

    #[test]
    fn root_height_owns_only_actual_live_content_with_one_blank_gaps() {
        let (mut ui, mut snapshot) = fixture();
        for text in ["", "one", "first\nsecond\nthird\n\n"] {
            let live_height = if text.is_empty() {
                0
            } else if text == "one" {
                1
            } else {
                2
            };
            snapshot.active = Some(crate::runtime::ActiveView {
                attempt_id: "attempt".into(),
                text: text.into(),
                truncated: false,
                message_id: String::new(),
                durable_bytes: 0,
            });
            for phase in [Phase::Idle, Phase::Waiting, Phase::Receiving] {
                snapshot.phase = phase;
                for input_height in 1..=3 {
                    ui.draft.set_text(&vec!["draft"; input_height].join("\n"));
                    let height = inline_height(&ui, &snapshot, 80, 40);
                    assert_eq!(height, live_height + input_height as u16 + 3);
                    let layout = root_layout(&ui, &snapshot, Rect::new(0, 0, 80, height), None);
                    let oversized = root_layout(&ui, &snapshot, Rect::new(0, 0, 80, 40), None);
                    assert_eq!(layout.live.height, live_height);
                    assert_eq!(layout.composer, oversized.composer);
                    assert_eq!(layout.status, oversized.status);
                    assert_eq!(layout.composer.y, layout.live.bottom() + 1);
                    assert_eq!(layout.status.y, layout.composer.bottom() + 1);
                    assert_eq!(layout.status.bottom(), height);
                    let buffer = draw(&ui, &snapshot, 80, height);
                    for y in [layout.live.bottom(), layout.composer.bottom()] {
                        assert!((0..80).all(|x| buffer[(x, y)].symbol() == " "));
                    }
                    assert!(!plain(&buffer).contains('\u{2500}'));
                    for physical_height in 0..height {
                        assert_eq!(
                            inline_height(&ui, &snapshot, 80, physical_height),
                            physical_height
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn pager_filtering_and_search_reserve_six_choices_without_moving_input() {
        let (mut ui, snapshot) = fixture();
        ui.draft.set_text("/");
        let closed_height = inline_height(&ui, &snapshot, 80, 40);
        let closed = root_layout(&ui, &snapshot, Rect::new(0, 0, 80, closed_height), None);
        key(&mut ui, &snapshot, KeyCode::Tab, KeyModifiers::NONE);
        let all = ui.completion(&snapshot).unwrap();
        assert!(all.items.len() > 6);
        for count in [0, 1, 2, 6, all.items.len()] {
            let mut completion = all.clone();
            completion.items.truncate(count);
            for search in [false, true] {
                ui.pager_search = search.then(|| Editor::new(128));
                let height = inline_height(&ui, &snapshot, 80, 40);
                assert_eq!(height, closed_height + 10);
                let layout = root_layout(
                    &ui,
                    &snapshot,
                    Rect::new(0, 0, 80, height),
                    Some(&completion),
                );
                assert_eq!(layout.composer, closed.composer);
                assert_eq!(layout.menu.y, layout.composer.bottom() + 1);
                assert_eq!(layout.menu.height, 9);
                assert_eq!(layout.status.y, layout.menu.bottom() + 1);
                assert_eq!(
                    menu_window(&ui, &completion, layout.menu.height).1,
                    count.min(6)
                );
                let buffer = draw(&ui, &snapshot, 80, height);
                assert!((0..80).all(|x| buffer[(x, layout.live.bottom())].symbol() == " "));
                for y in [layout.top_divider.y, layout.bottom_divider.y] {
                    assert!((0..80).all(|x| buffer[(x, y)].symbol() == "\u{2500}"));
                }
                assert!(!plain(&buffer).contains('>'));
            }
        }
        ui.pager_search = None;
        ui.reset_completion();
        ui.palette = true;
        for query in ["", "q", "quit", "no-such-command"] {
            ui.palette_query.set_text(query);
            let completion = ui.completion(&snapshot).unwrap();
            let height = inline_height(&ui, &snapshot, 80, 40);
            let layout = root_layout(
                &ui,
                &snapshot,
                Rect::new(0, 0, 80, height),
                Some(&completion),
            );
            assert_eq!(height, closed_height + 12);
            assert_eq!(layout.composer, closed.composer);
            assert_eq!(layout.command_title.y, layout.composer.bottom());
            assert_eq!(layout.command.height, 1);
            assert_eq!(layout.menu.height, 9);
        }
    }

    #[test]
    fn pager_is_compact_below_composer_with_real_selection_and_search_cursor() {
        let (mut ui, snapshot) = fixture();
        ui.draft.set_text("/s");
        key(&mut ui, &snapshot, KeyCode::Tab, KeyModifiers::NONE);
        assert!(!ui.pager_selected);
        assert!(!selection_visible(&ui, &snapshot, 80, 14));
        let buffer = draw(&ui, &snapshot, 80, 14);
        assert!(!plain(&buffer).contains("> /"));
        let completion = ui.completion(&snapshot).unwrap();
        let layout = root_layout(&ui, &snapshot, Rect::new(0, 0, 80, 14), Some(&completion));
        assert_eq!(buffer[(2, layout.menu.y + 2)].fg, Color::Indexed(245));
        assert!(
            !buffer[(2, layout.menu.y + 2)]
                .modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(
            root_layout(&ui, &snapshot, Rect::new(0, 0, 80, 14), Some(&completion))
                .menu
                .height,
            9
        );
        key(&mut ui, &snapshot, KeyCode::Tab, KeyModifiers::NONE);
        key(&mut ui, &snapshot, KeyCode::Down, KeyModifiers::NONE);
        let completion = ui.completion(&snapshot).unwrap();
        for width in [1, 2, 23, 24, 80] {
            for height in 0..20 {
                let area = Rect::new(3, 5, width, height);
                let layout = root_layout(&ui, &snapshot, area, Some(&completion));
                assert_eq!(layout.status.height, height.min(1));
                assert!(layout.menu.y >= layout.composer.bottom());
                assert!(layout.menu.y - layout.composer.bottom() <= 1);
                if height > 1 {
                    assert!(layout.composer.height > 0);
                }
                let mut occupied = Vec::new();
                for region in [layout.live, layout.composer, layout.menu, layout.status] {
                    assert!(region.y >= area.y && region.bottom() <= area.bottom());
                    for y in region.y..region.bottom() {
                        assert!(!occupied.contains(&y));
                        occupied.push(y);
                    }
                }
                assert!(occupied.len() <= height as usize);
                assert_eq!(
                    layout.status.bottom() - area.y,
                    inline_height(&ui, &snapshot, width, height)
                );
                assert_eq!(
                    selection_visible(&ui, &snapshot, width, height),
                    width >= 24 && height >= 4
                );
                if width >= 24 && height >= 4 {
                    let buffer = draw(&ui, &snapshot, width, height);
                    assert!(!plain(&buffer).contains('>'));
                    let layout = root_layout(
                        &ui,
                        &snapshot,
                        Rect::new(0, 0, width, height),
                        Some(&completion),
                    );
                    let (start, _, _) = menu_window(&ui, &completion, layout.menu.height);
                    let header = if layout.menu.height >= 4 { 2 } else { 0 };
                    let cell = &buffer[(
                        2,
                        layout.menu.y + header + (ui.completion_selected - start) as u16,
                    )];
                    assert_eq!(cell.symbol(), "/");
                    assert_eq!(cell.fg, Color::Indexed(255));
                    assert!(cell.modifier.contains(Modifier::BOLD));
                }
            }
        }
        ui.completion_selected = usize::MAX;
        assert!(!selection_visible(&ui, &snapshot, 80, 14));
        ui.reset_completion();
        ui.palette = true;
        ui.palette_query.set_text("q");
        assert!(selection_visible(&ui, &snapshot, 80, 14));
        assert!(!plain(&draw(&ui, &snapshot, 80, 14)).contains("> /"));
        ui.palette = false;
        ui.draft.set_text("/");
        key(&mut ui, &snapshot, KeyCode::Tab, KeyModifiers::NONE);
        for _ in 0..13 {
            key(&mut ui, &snapshot, KeyCode::Tab, KeyModifiers::NONE);
        }
        let completion = ui.completion(&snapshot).unwrap();
        let layout = root_layout(&ui, &snapshot, Rect::new(0, 0, 80, 14), Some(&completion));
        assert_eq!(layout.menu.height, 9);
        assert!(
            plain(&draw(&ui, &snapshot, 80, 14))
                .contains(&format!("8-13/{}", completion.items.len()))
        );
        ui.reset_completion();
        ui.draft.set_text("/");
        key(&mut ui, &snapshot, KeyCode::BackTab, KeyModifiers::SHIFT);
        ui.pager_search
            .as_mut()
            .unwrap()
            .set_text("1234567890".repeat(10).as_str());
        ui.pager_search.as_mut().unwrap().set_cursor(30);
        let mut terminal = Terminal::new(TestBackend::new(40, 14)).unwrap();
        terminal
            .draw(|frame| render(frame, &ui, &snapshot))
            .unwrap();
        let completion = ui.completion(&snapshot).unwrap();
        let layout = root_layout(&ui, &snapshot, Rect::new(0, 0, 40, 14), Some(&completion));
        let (x, _) = ui
            .pager_search
            .as_ref()
            .unwrap()
            .rows(32, 1)
            .cursor
            .unwrap();
        let position = terminal.get_cursor_position().unwrap();
        assert_eq!((position.x, position.y), (8 + x, layout.menu.bottom() - 1));
        assert_eq!(
            plain(terminal.backend().buffer())
                .matches("search:")
                .count(),
            1
        );
    }

    #[test]
    fn ghost_uses_first_candidate_only_when_not_previewing_and_preserves_suffix() {
        let (mut ui, snapshot) = fixture();
        ui.draft.set_text("/s");
        ui.completion_selected = 1;
        let buffer = draw(&ui, &snapshot, 80, 14);
        let completion = ui.completion(&snapshot).unwrap();
        let layout = root_layout(&ui, &snapshot, Rect::new(0, 0, 80, 14), Some(&completion));
        assert_eq!(layout.menu.height, 0);
        assert_eq!(buffer[(4, layout.composer.y)].symbol(), "e");
        assert_eq!(buffer[(4, layout.composer.y)].fg, Color::Indexed(245));
        assert!(
            buffer
                .content
                .iter()
                .all(|cell| cell.bg == Color::Reset && !cell.modifier.contains(Modifier::DIM))
        );
        key(&mut ui, &snapshot, KeyCode::Tab, KeyModifiers::NONE);
        let completion = ui.completion(&snapshot).unwrap();
        let layout = root_layout(&ui, &snapshot, Rect::new(0, 0, 80, 14), Some(&completion));
        let buffer = draw(&ui, &snapshot, 80, 14);
        assert!(ui.completion_active);
        assert_eq!(ui.draft.text(), "/s");
        assert_eq!(buffer[(4, layout.composer.y)].fg, Color::Reset);
        assert_eq!(buffer[(5, layout.composer.y)].symbol(), " ");
        ui.reset_completion();
        ui.draft.set_text("/his fullscreen");
        ui.draft.set_cursor(4);
        let buffer = draw(&ui, &snapshot, 80, 14);
        assert!(plain(&buffer).contains("/his fullscreen"));
        assert!(!plain(&buffer).contains("/history fullscreen"));
        ui.draft.set_text("/history ful");
        let buffer = draw(&ui, &snapshot, 15, 4);
        let completion = ui.completion(&snapshot).unwrap();
        let layout = root_layout(&ui, &snapshot, Rect::new(0, 0, 15, 4), Some(&completion));
        assert_eq!(buffer[(14, layout.composer.y)].symbol(), "l");
        assert_eq!(buffer[(14, layout.composer.y)].fg, Color::Indexed(245));
        assert!(!plain(&buffer).contains("screen"));
    }

    #[test]
    fn composer_cursor_is_editor_caret_not_text_tail() {
        let (mut ui, snapshot) = fixture();
        ui.draft.set_text("zero\none\ntwo\nthree\nfour");
        ui.draft.set_cursor(9);
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
        terminal
            .draw(|frame| render(frame, &ui, &snapshot))
            .unwrap();
        let layout = root_layout(&ui, &snapshot, Rect::new(0, 0, 20, 6), None);
        let rows = ui.draft.rows(18, layout.composer.height);
        let (x, y) = rows.cursor.unwrap();
        let position = terminal.get_cursor_position().unwrap();
        assert_eq!(
            (position.x, position.y),
            (layout.composer.x + 2 + x, layout.composer.y + y)
        );
        assert!(plain(terminal.backend().buffer()).contains("\u{2503}\u{2191}"));
        assert!(plain(terminal.backend().buffer()).contains("\u{2503}\u{2193}"));
    }

    #[test]
    fn fullscreen_history_uses_browser_and_never_paints_the_primary_draft() {
        let (mut ui, snapshot) = fixture();
        ui.view = View::History;
        ui.draft.set_text("retained draft");
        ui.full_history = true;
        ui.history.apply(
            vec![crate::runtime::HistoryEntry {
                seq: 42,
                attempt_id: "saved-attempt".into(),
                kind: "user".into(),
                text: "saved history message".into(),
                status: "completed".into(),
            }],
            super::super::history::Load::Latest,
        );
        for width in [40, 80, 120] {
            for height in 1..20 {
                ui.prepare_history(width, height);
                let text = plain(&draw(&ui, &snapshot, width, height));
                assert!(!text.contains("retained draft"));
                if height >= 6 {
                    assert!(text.contains("#42"));
                }
                if width >= 100 && height >= 6 {
                    assert!(text.contains("saved history message"));
                }
            }
        }
    }

    #[test]
    fn bars_frames_are_three_cells_smooth_periodic_and_time_driven() {
        let expected: [[u32; 3]; 14] = [
            [0, 4, 6],
            [1, 5, 5],
            [2, 6, 4],
            [3, 7, 3],
            [4, 6, 2],
            [5, 5, 1],
            [6, 4, 0],
            [7, 3, 1],
            [6, 2, 2],
            [5, 1, 3],
            [4, 0, 4],
            [3, 1, 5],
            [2, 2, 6],
            [1, 3, 7],
        ];
        let mut seen = [0u8; 3];
        for (frame, heights) in expected.iter().enumerate() {
            let next = expected[(frame + 1) % expected.len()];
            for column in 0..3 {
                assert!(heights[column].abs_diff(next[column]) <= 1);
                seen[column] |= 1 << heights[column];
            }
            let glyphs: String = heights
                .iter()
                .map(|height| char::from_u32(0x2581 + *height).unwrap())
                .collect();
            for offset in 0..100 {
                let elapsed = Duration::from_millis(frame as u64 * 100 + offset);
                let bars = status_bars(elapsed);
                assert_eq!(bars, glyphs);
                assert_eq!(bars.chars().count(), 3);
                assert_eq!(Span::raw(&bars).width(), 3);
                assert_eq!(status_bars(elapsed), bars);
                assert_eq!(status_bars(elapsed + Duration::from_millis(1400)), bars);
            }
        }
        assert_eq!(seen, [u8::MAX; 3]);
        assert_ne!(
            status_bars(Duration::from_millis(99)),
            status_bars(Duration::from_millis(100))
        );
        assert_eq!(
            status_bars(Duration::ZERO),
            status_bars(Duration::from_millis(1400))
        );
        assert_eq!(Span::raw(status_bars(Duration::MAX)).width(), 3);
    }

    #[test]
    fn bars_render_identically_at_fixed_times_and_ignore_response_telemetry() {
        let (ui, mut snapshot) = fixture();
        assert_eq!(ui.interface.status_preset, StatusPreset::Bars);
        snapshot.phase = Phase::Receiving;
        snapshot.active = Some(crate::runtime::ActiveView {
            attempt_id: "attempt".into(),
            text: String::new(),
            truncated: false,
            message_id: String::new(),
            durable_bytes: 0,
        });
        for frame in 0..14 {
            let elapsed = Duration::from_millis(frame * 100);
            let line = status_line_elapsed(&ui, &snapshot, elapsed);
            assert_eq!(status_line_elapsed(&ui, &snapshot, elapsed), line);
            assert_eq!(line.spans[0].content, status_bars(elapsed));
            assert_eq!(line.spans[0].width(), 3);
            let active = snapshot.active.as_mut().unwrap();
            active.text.push_str("not an audio signal");
            active.durable_bytes += 1024;
            active.truncated = !active.truncated;
            snapshot.last_event = Instant::now();
            assert_eq!(status_line_elapsed(&ui, &snapshot, elapsed), line);
        }
        let before = status_line_elapsed(&ui, &snapshot, Duration::from_millis(99));
        let after = status_line_elapsed(&ui, &snapshot, Duration::from_millis(100));
        assert_ne!(before.spans[0], after.spans[0]);
        assert_eq!(before.width(), after.width());
    }

    #[test]
    fn bars_require_an_active_animated_phase_and_reserve_blank_idle_cells() {
        let (ui, mut snapshot) = fixture();
        for phase in [
            Phase::Idle,
            Phase::Waiting,
            Phase::Receiving,
            Phase::Stopping,
            Phase::StorageError,
        ] {
            snapshot.phase = phase;
            for active in [false, true] {
                snapshot.active = active.then(|| crate::runtime::ActiveView {
                    attempt_id: "attempt".into(),
                    text: String::new(),
                    truncated: false,
                    message_id: String::new(),
                    durable_bytes: 0,
                });
                for paused in [false, true] {
                    snapshot.paused = paused;
                    let animating = active
                        && matches!(phase, Phase::Waiting | Phase::Receiving | Phase::Stopping);
                    for elapsed in [
                        Duration::ZERO,
                        Duration::from_millis(100),
                        Duration::from_secs(3600),
                    ] {
                        let line = status_line_elapsed(&ui, &snapshot, elapsed);
                        if animating {
                            assert_eq!(line.spans[0].width(), 3);
                            assert_eq!(line.spans[0].content, status_bars(elapsed));
                        } else {
                            assert!(!line.to_string().starts_with(' '));
                        }
                        assert_eq!(
                            line.to_string()
                                .chars()
                                .any(|ch| ('\u{2581}'..='\u{2588}').contains(&ch)),
                            animating
                        );
                        assert!(
                            !line
                                .to_string()
                                .chars()
                                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))
                        );
                        if phase == Phase::Idle {
                            assert!(line.to_string().starts_with("ready "));
                            assert!(!line.to_string().contains("3600"));
                        }
                        if paused {
                            assert!(line.to_string().contains("stopped (send to continue)"));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn bars_are_bright_white_separate_from_phase_colors_and_monochrome_text() {
        let (mut ui, mut snapshot) = fixture();
        snapshot.active = Some(crate::runtime::ActiveView {
            attempt_id: "attempt".into(),
            text: String::new(),
            truncated: false,
            message_id: String::new(),
            durable_bytes: 0,
        });
        for (phase, color) in [
            (Phase::Idle, 245),
            (Phase::Waiting, 109),
            (Phase::Receiving, 109),
            (Phase::Stopping, 179),
            (Phase::StorageError, 167),
        ] {
            snapshot.phase = phase;
            for status_color in [false, true] {
                ui.interface.status_color = status_color;
                let line = status_line_elapsed(&ui, &snapshot, Duration::ZERO);
                let animating =
                    matches!(phase, Phase::Waiting | Phase::Receiving | Phase::Stopping);
                let phase_color = Color::Indexed(if status_color { color } else { 245 });
                if animating {
                    assert_eq!(
                        line.spans[0].style,
                        Style::default().fg(Color::Indexed(255))
                    );
                    assert_eq!(line.spans[1].style, muted().fg(phase_color));
                } else {
                    assert_eq!(line.style, muted().fg(phase_color));
                }
                let mut terminal = Terminal::new(TestBackend::new(100, 1)).unwrap();
                terminal
                    .draw(|frame| frame.render_widget(Paragraph::new(line.clone()), frame.area()))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                if animating {
                    for x in 0..3 {
                        assert_eq!(buffer[(x, 0)].fg, Color::Indexed(255));
                        assert_eq!(buffer[(x, 0)].bg, Color::Reset);
                    }
                }
                assert_eq!(buffer[(if animating { 4 } else { 0 }, 0)].fg, phase_color);
            }
        }
    }

    #[test]
    fn text_minimal_and_explicit_braille_keep_their_status_semantics() {
        let (mut ui, mut snapshot) = fixture();
        snapshot.phase = Phase::Waiting;
        snapshot.active = Some(crate::runtime::ActiveView {
            attempt_id: "attempt".into(),
            text: String::new(),
            truncated: false,
            message_id: String::new(),
            durable_bytes: 0,
        });
        for status_color in [false, true] {
            ui.interface.status_color = status_color;
            ui.interface.status_preset = StatusPreset::Text;
            let line = status_line_elapsed(&ui, &snapshot, Duration::ZERO);
            assert_eq!(
                line.to_string(),
                "waiting 0.0s \u{00b7} local test fixture \u{00b7} 0 drafts \u{00b7} /help"
            );
            let phase_color = Some(Color::Indexed(if status_color { 109 } else { 245 }));
            assert_eq!(line.style.fg, phase_color);
            ui.interface.status_preset = StatusPreset::Minimal;
            assert_eq!(
                status_line_elapsed(&ui, &snapshot, Duration::ZERO).to_string(),
                "waiting 0.0s \u{00b7} 0 queued \u{00b7} 0 drafts"
            );
            ui.interface.status_preset = StatusPreset::Braille;
            for (frame, glyph) in [
                '\u{280b}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283c}', '\u{2834}', '\u{2826}',
                '\u{2827}',
            ]
            .iter()
            .enumerate()
            {
                let elapsed = Duration::from_millis(frame as u64 * 100);
                let line = status_line_elapsed(&ui, &snapshot, elapsed);
                assert_eq!(line.to_string().chars().next(), Some(*glyph));
                assert_eq!(line.style.fg, phase_color);
                assert_eq!(status_line_elapsed(&ui, &snapshot, elapsed), line);
            }
        }
    }

    #[test]
    fn status_presets_omit_idle_time_and_keep_active_animation_when_queue_is_paused() {
        let (mut ui, mut snapshot) = fixture();
        ui.interface.status_preset = StatusPreset::Braille;
        snapshot.phase_started = Instant::now() - Duration::from_secs(3600);
        let line = status_line(&ui, &snapshot);
        assert!(line.to_string().contains("local test fixture"));
        assert!(line.to_string().starts_with("ready "));
        assert!(!line.to_string().contains("3600"));
        ui.interface.status_preset = StatusPreset::Text;
        assert_eq!(line.to_string(), status_line(&ui, &snapshot).to_string());
        assert_eq!(line.style.fg, Some(Color::Indexed(245)));
        ui.interface.status_preset = StatusPreset::Minimal;
        let text = status_line(&ui, &snapshot).to_string();
        assert!(text.contains("0 queued"));
        assert!(!text.contains("fixture"));
        ui.interface.status_preset = StatusPreset::Braille;
        snapshot.phase = Phase::Receiving;
        snapshot.phase_started = Instant::now() - Duration::from_millis(2400);
        snapshot.active = Some(crate::runtime::ActiveView {
            attempt_id: "hidden-attempt".into(),
            text: String::new(),
            truncated: false,
            message_id: String::new(),
            durable_bytes: 0,
        });
        assert!(
            status_line(&ui, &snapshot)
                .to_string()
                .chars()
                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))
        );
        snapshot.paused = true;
        assert!(
            status_line(&ui, &snapshot)
                .to_string()
                .chars()
                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))
        );
        assert!(status_line(&ui, &snapshot).to_string().contains("stopped"));
        ui.notice = Some(("status set to braille".into(), Instant::now(), false));
        assert!(
            status_line(&ui, &snapshot)
                .to_string()
                .contains("receiving 2.4s")
        );
        assert!(
            !status_line(&ui, &snapshot)
                .to_string()
                .contains("status set")
        );
        snapshot.active = None;
        snapshot.phase = Phase::Idle;
        ui.notice = None;
        assert!(
            !status_line(&ui, &snapshot)
                .to_string()
                .chars()
                .any(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))
        );
        ui.interface.status_line = false;
        ui.interface.status_color = false;
        snapshot.error = Some("bad\x1b[31m\nresponse".into());
        let line = status_line(&ui, &snapshot);
        assert!(line.to_string().contains("response error:"));
        assert!(!line.to_string().contains(['\x1b', '\n']));
        assert_eq!(line.style.fg, Some(Color::Indexed(252)));
        ui.interface.status_color = true;
        assert_eq!(
            status_line(&ui, &snapshot).style.fg,
            Some(Color::Indexed(167))
        );
    }

    #[test]
    fn pager_has_no_tutorial_chrome_and_descriptions_are_sanitized() {
        let (mut ui, snapshot) = fixture();
        ui.draft.set_text("/settings ");
        key(&mut ui, &snapshot, KeyCode::Tab, KeyModifiers::NONE);
        let text = plain(&draw(&ui, &snapshot, 100, 14));
        for unwanted in [
            "Usage:",
            "Examples:",
            "Source:",
            "Defaults:",
            "Tab activate",
            "Commands 1/",
            "Apply / Scope",
        ] {
            assert!(!text.contains(unwanted));
        }
        assert!(text.contains("status-line"));
        ui.reset_completion();
        ui.draft.set_text("/settings status text ");
        key(
            &mut ui,
            &snapshot,
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        );
        let text = plain(&draw(&ui, &snapshot, 100, 14));
        assert!(text.contains("--default    Make default"));
        assert!(!text.contains("Apply"));
        ui.reset_completion();
        ui.draft.set_text("/send ");
        ui.drafts.push(crate::runtime::DraftRecord {
            id: "12345678-1234-1234-1234-123456789012".into(),
            text: "payload\x1b[31m\nsecond\tline".into(),
            revision: 0,
        });
        key(
            &mut ui,
            &snapshot,
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        );
        let text = plain(&draw(&ui, &snapshot, 100, 14));
        assert!(text.contains("payload[31m second line"));
        assert!(!text.contains(['\x1b', '\n', '\t']));
        key(&mut ui, &snapshot, KeyCode::Tab, KeyModifiers::NONE);
        assert!(!selection_visible(&ui, &snapshot, 37, 14));
        assert!(selection_visible(&ui, &snapshot, 38, 14));
        ui.view = View::Details;
        ui.info_scroll = 10;
        ui.setting_sources
            .insert("status", "actual client source".into());
        assert!(
            plain(&draw(&ui, &snapshot, 100, 40)).contains("status = bars / actual client source")
        );
        ui.view = View::Help;
        ui.help_query.set_text("settings");
        ui.help_detail = commands::COMMANDS
            .iter()
            .find(|spec| spec.action == Action::Settings);
        let text = plain(&draw(&ui, &snapshot, 100, 80));
        assert!(text.contains("/settings [<key> <value> [--default]]"));
        assert!(text.contains("Insert into command field"));
    }

    #[test]
    fn all_views_survive_tiny_viewports_and_root_keeps_live_text() {
        let (mut ui, mut snapshot) = fixture();
        ui.draft.set_text("\u{754c}\nretained draft\nmore");
        snapshot.phase = Phase::Receiving;
        snapshot.active = Some(crate::runtime::ActiveView {
            attempt_id: "hidden-attempt".into(),
            text: "**live response**".into(),
            truncated: false,
            message_id: String::new(),
            durable_bytes: 0,
        });
        assert!(plain(&draw(&ui, &snapshot, 80, 8)).contains("live response"));
        for view in [
            View::Root,
            View::History,
            View::Queue,
            View::Help,
            View::Settings,
            View::Details,
            View::Drafts,
            View::Bindings,
        ] {
            ui.view = view;
            for width in [1, 2, 3, 23, 24] {
                for height in 1..8 {
                    let buffer = draw(&ui, &snapshot, width, height);
                    assert!(buffer.content.iter().all(|cell| cell.bg == Color::Reset));
                }
            }
        }
        ui.view = View::Root;
        ui.draft.set_text("/");
        key(
            &mut ui,
            &snapshot,
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        );
        for width in [1, 2, 3, 8, 24] {
            for height in 1..8 {
                draw(&ui, &snapshot, width, height);
            }
        }
        ui.interface.status_line = false;
        snapshot.phase = Phase::StorageError;
        assert!(
            status_line(&ui, &snapshot)
                .to_string()
                .contains("storage error")
        );
    }

    #[test]
    fn secondary_field_renders_both_buffers_and_moves_only_the_active_cursor() {
        let (mut ui, snapshot) = fixture();
        ui.draft.set_text("primary message");
        ui.draft.set_cursor(4);
        ui.palette = true;
        ui.palette_pinned = true;
        ui.palette_query.set_text("/settings status braille");
        ui.palette_query.set_cursor(9);
        for focus in [true, false] {
            ui.commands_focus = focus;
            let height = inline_height(&ui, &snapshot, 100, 40);
            let layout = root_layout(
                &ui,
                &snapshot,
                Rect::new(0, 0, 100, height),
                ui.completion(&snapshot).as_ref(),
            );
            let mut terminal = Terminal::new(TestBackend::new(100, height)).unwrap();
            terminal
                .draw(|frame| render(frame, &ui, &snapshot))
                .unwrap();
            let text = plain(terminal.backend().buffer());
            assert!(text.contains("primary message"));
            assert!(text.contains("commands (pinned)"));
            assert!(text.contains("/settings status braille"));
            let (area, editor) = if focus {
                (layout.command, &ui.palette_query)
            } else {
                (layout.composer, &ui.draft)
            };
            let (x, y) = editor.rows(98, area.height).cursor.unwrap();
            let cursor = terminal.get_cursor_position().unwrap();
            assert_eq!((cursor.x, cursor.y), (area.x + 2 + x, area.y + y));
        }
        for width in [1, 2, 24, 80] {
            for height in 0..16 {
                let area = Rect::new(0, 0, width, height);
                let layout = root_layout(&ui, &snapshot, area, ui.completion(&snapshot).as_ref());
                for rect in [
                    layout.composer,
                    layout.command_title,
                    layout.command,
                    layout.menu,
                    layout.status,
                ] {
                    assert!(rect.bottom() <= area.bottom());
                }
                draw(&ui, &snapshot, width, height);
            }
        }
    }

    #[test]
    fn stopped_send_has_no_choice_menu_and_binding_editor_retains_primary_text() {
        let (mut ui, mut snapshot) = fixture();
        snapshot.paused = true;
        ui.draft.set_text("unsent message");
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        ui.submit(&snapshot, &tx).unwrap();
        let text = plain(&draw(
            &ui,
            &snapshot,
            100,
            inline_height(&ui, &snapshot, 100, 40),
        ));
        assert!(text.contains("unsent message"));
        assert!(ui.draft_busy && ui.pending_send.is_none());
        assert!(!text.contains("hold") && !text.contains("run queue"));
        assert!(!text.contains("cancel"));
        ui.draft_busy = false;
        ui.pending_send = None;
        ui.view = View::Bindings;
        ui.binding_capture = true;
        assert!(plain(&draw(&ui, &snapshot, 100, 14)).contains("Press the shortcut"));
        ui.binding_capture = false;
        ui.binding_key = Some("alt+z".into());
        ui.binding_commands
            .set_text("/stop ; /settings status minimal");
        let text = plain(&draw(&ui, &snapshot, 100, 14));
        assert!(text.contains("alt+z"));
        assert!(text.contains("/stop ; /settings status minimal"));
    }

    #[test]
    fn completion_feedback_distinguishes_confirmation_from_execution() {
        let (mut ui, snapshot) = fixture();
        ui.draft.set_text("/settings status ");
        key(&mut ui, &snapshot, KeyCode::Tab, KeyModifiers::NONE);
        key(&mut ui, &snapshot, KeyCode::Tab, KeyModifiers::NONE);
        assert!(plain(&draw(&ui, &snapshot, 100, 14)).contains("Enter accepts only"));
        let preset = ui.interface.status_preset;
        key(&mut ui, &snapshot, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(ui.interface.status_preset, preset);
        assert!(
            status_line(&ui, &snapshot)
                .to_string()
                .contains("Enter runs command")
        );
    }

    #[test]
    fn visual_binding_completion_pager_uses_its_own_input_and_readable_selection() {
        let (mut ui, snapshot) = fixture();
        ui.draft.set_text("protected primary");
        ui.view = View::Bindings;
        ui.binding_key = Some("f2".into());
        ui.binding_commands.set_text("/settings status ");
        key(&mut ui, &snapshot, KeyCode::Tab, KeyModifiers::NONE);
        key(&mut ui, &snapshot, KeyCode::Tab, KeyModifiers::NONE);
        let buffer = draw(&ui, &snapshot, 100, 14);
        let text = plain(&buffer);
        assert!(text.contains("Tab completes"));
        assert!(text.contains("/settings status text"));
        assert!(
            text.contains("Current value")
                || text.contains("Choose text, braille, minimal, or bars")
        );
        assert!(text.contains("Enter accepts only"));
        assert!(selection_visible(&ui, &snapshot, 100, 14));
        assert!(!selection_visible(&ui, &snapshot, 1, 1));
        assert_eq!(ui.draft.text(), "protected primary");
        for width in [1, 2, 24, 80] {
            for height in 0..16 {
                draw(&ui, &snapshot, width, height);
            }
        }
    }

    #[test]
    fn latest_notice_is_a_live_real_last_child_not_an_empty_native_branch() {
        let (mut ui, snapshot) = fixture();
        ui.completed_with(
            super::super::state::Done {
                op: super::super::state::Op::Tail,
                result: Ok(crate::runtime::Reply::NativeTranscript(vec![
                    crate::runtime::NativeEntry {
                        entry: crate::runtime::HistoryEntry {
                            seq: 1,
                            attempt_id: String::new(),
                            kind: "notice".into(),
                            text: "status-line disabled".into(),
                            status: "info".into(),
                        },
                        category: Some("settings".into()),
                    },
                    crate::runtime::NativeEntry {
                        entry: crate::runtime::HistoryEntry {
                            seq: 2,
                            attempt_id: String::new(),
                            kind: "notice".into(),
                            text: "status-line enabled".into(),
                            status: "info".into(),
                        },
                        category: Some("settings".into()),
                    },
                ])),
            },
            |_| Ok(()),
        )
        .unwrap();
        let mut native = Vec::new();
        for _ in 0..2 {
            ui.publish_with(&snapshot, 80, |batch| {
                native.extend(batch.iter().map(ToString::to_string));
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(
            native,
            ["", "\u{25cf} Settings", "  \u{251c} status-line disabled"]
        );
        let height = inline_height(&ui, &snapshot, 80, 40);
        assert_eq!(height, 5);
        let text = plain(&draw(&ui, &snapshot, 80, height));
        assert!(text.contains("\u{2514} status-line enabled"));
        assert!(!text.contains("\u{2514}\u{2500}"));
        ui.draft.set_text("new message");
        for width in [1, 2, 24, 80] {
            for height in 0..10 {
                draw(&ui, &snapshot, width, height);
            }
        }
        ui.exiting = true;
        ui.publish_with(&snapshot, 80, |batch| {
            native.extend(batch.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
        assert_eq!(native.last().unwrap(), "  \u{2514} status-line enabled");
        assert!(ui.native_tail_rows(80).is_empty());
    }

    #[test]
    fn pending_messages_are_bounded_muted_previews_above_input_in_queue_order() {
        let (mut ui, mut snapshot) = fixture();
        ui.draft.set_text("editable draft");
        for index in 0..5 {
            snapshot.queue.push(crate::runtime::QueueItem {
                id: format!("queued-{index}"),
                text: format!("pending-{index}\nsecond-{index}\nthird-{index}"),
                revision: 0,
                state: QueueState::Queued,
            });
        }
        let height = inline_height(&ui, &snapshot, 80, 40);
        let layout = root_layout(&ui, &snapshot, Rect::new(0, 0, 80, height), None);
        assert_eq!(layout.queue.height, 6);
        assert_eq!(layout.composer.y, layout.queue.bottom() + 1);
        let buffer = draw(&ui, &snapshot, 80, height);
        let text = plain(&buffer);
        assert!(!text.contains("pending-0") && !text.contains("pending-1"));
        assert!(text.find("pending-2").unwrap() < text.find("pending-3").unwrap());
        assert!(text.find("pending-3").unwrap() < text.find("pending-4").unwrap());
        assert!(text.find("pending-4").unwrap() < text.find("editable draft").unwrap());
        assert_eq!(text.matches('\u{250b}').count(), 6);
        assert_eq!(text.matches('\u{2026}').count(), 3);
        assert!(!text.contains("third-"));
        for y in layout.queue.y..layout.queue.bottom() {
            assert_eq!(buffer[(0, y)].fg, Color::Indexed(245));
            assert!(!buffer[(0, y)].modifier.contains(Modifier::BOLD));
        }
        snapshot.queue.pop();
        let text = plain(&draw(
            &ui,
            &snapshot,
            80,
            inline_height(&ui, &snapshot, 80, 40),
        ));
        assert!(!text.contains("pending-4"));
        assert!(text.contains("pending-1"));
        snapshot.queue.clear();
        assert!(!plain(&draw(&ui, &snapshot, 80, 40)).contains('\u{250b}'));
    }

    #[test]
    fn queue_preview_sanitizes_wraps_unicode_and_never_becomes_a_saved_turn() {
        let (_, mut snapshot) = fixture();
        snapshot.queue.push(crate::runtime::QueueItem {
            id: "queue-id".into(),
            text: "\u{754c} text\x1b[31m\nmore\ntruncated".into(),
            revision: 0,
            state: QueueState::Queued,
        });
        for width in [4, 10, 80] {
            let rows = queue_rows(&snapshot, width, 2);
            assert_eq!(rows.len(), 2);
            assert!(rows.iter().all(|row| row.width() <= usize::from(width)));
            assert!(rows.iter().all(|row| !row.to_string().contains('\x1b')));
            assert!(rows.last().unwrap().to_string().ends_with('\u{2026}'));
        }
    }

    #[test]
    fn queue_and_panel_geometry_is_stable_when_resized_to_its_owned_height() {
        let (mut ui, mut snapshot) = fixture();
        snapshot.queue.push(crate::runtime::QueueItem {
            id: "queued".into(),
            text: "waiting message\nsecond row".into(),
            revision: 0,
            state: QueueState::Queued,
        });
        ui.draft.set_text("retained\nmultiline draft");
        for view in [
            View::Root,
            View::Settings,
            View::Help,
            View::Queue,
            View::Drafts,
            View::Bindings,
            View::Details,
        ] {
            ui.view = view;
            for width in [1, 2, 10, 80] {
                for height in 0..40 {
                    let owned = inline_height(&ui, &snapshot, width, height);
                    assert!(owned <= height);
                    assert_eq!(inline_height(&ui, &snapshot, width, owned), owned);
                    if view == View::Root {
                        let full =
                            root_layout(&ui, &snapshot, Rect::new(0, 0, width, height), None);
                        let actual =
                            root_layout(&ui, &snapshot, Rect::new(0, 0, width, owned), None);
                        assert_eq!(full.composer, actual.composer);
                        assert_eq!(full.queue, actual.queue);
                    } else {
                        let full = panel_layout(&ui, &snapshot, Rect::new(0, 0, width, height));
                        let actual = panel_layout(&ui, &snapshot, Rect::new(0, 0, width, owned));
                        for rect in [
                            actual.queue,
                            actual.composer,
                            actual.top_divider,
                            actual.title,
                            actual.body,
                            actual.bottom_divider,
                            actual.status,
                        ] {
                            assert!(rect.bottom() <= owned);
                        }
                        assert_eq!(full.composer, actual.composer);
                        assert_eq!(full.body, actual.body);
                    }
                    draw(&ui, &snapshot, width, owned);
                }
            }
        }
    }

    #[test]
    fn every_inline_panel_is_framed_below_the_composer_with_a_contextual_footer() {
        let (mut ui, snapshot) = fixture();
        ui.draft.set_text("protected draft");
        ui.draft.set_cursor(4);
        for view in [
            View::Settings,
            View::Help,
            View::Queue,
            View::Drafts,
            View::Bindings,
            View::Details,
        ] {
            ui.view = view;
            let height = inline_height(&ui, &snapshot, 100, 40);
            let layout = panel_layout(&ui, &snapshot, Rect::new(0, 0, 100, height));
            let buffer = draw(&ui, &snapshot, 100, height);
            assert_eq!(layout.top_divider.y, layout.composer.bottom());
            assert!(layout.title.y > layout.composer.y);
            assert!(layout.body.y > layout.title.y);
            for y in [layout.top_divider.y, layout.bottom_divider.y] {
                assert!((0..100).all(|x| buffer[(x, y)].symbol() == "\u{2500}"));
            }
            assert!(plain(&buffer).contains("Esc"));
            assert_eq!(ui.draft.text(), "protected draft");
            assert_eq!(ui.draft.cursor(), 4);
        }
        ui.view = View::Help;
        ui.help_query.set_text("settings");
        let height = inline_height(&ui, &snapshot, 80, 40);
        let layout = panel_layout(&ui, &snapshot, Rect::new(0, 0, 80, height));
        let mut terminal = Terminal::new(TestBackend::new(80, height)).unwrap();
        terminal
            .draw(|frame| render(frame, &ui, &snapshot))
            .unwrap();
        assert!(plain(terminal.backend().buffer()).contains("\u{2503} settings"));
        assert_eq!(terminal.get_cursor_position().unwrap().y, layout.composer.y);
    }

    #[test]
    fn active_publication_projects_actual_post_height_without_a_high_water_reserve() {
        let (mut ui, mut snapshot) = fixture();
        ui.draft.set_text("retained input");
        snapshot.active = Some(crate::runtime::ActiveView {
            attempt_id: "active".into(),
            text: String::new(),
            truncated: false,
            message_id: String::new(),
            durable_bytes: 0,
        });
        let height = inline_height(&ui, &snapshot, 80, 40);
        let initial = root_layout(&ui, &snapshot, Rect::new(0, 0, 80, height), None);
        assert_eq!(initial.live.height, 0);
        assert_eq!(height, 4);
        for text in [
            String::new(),
            "one".into(),
            "one\ntwo\nthree\n".into(),
            "long ".repeat(80),
        ] {
            snapshot.active.as_mut().unwrap().text = text.clone();
            ui.pending.insert(
                1,
                crate::runtime::HistoryEntry {
                    seq: 1,
                    attempt_id: "active".into(),
                    kind: "assistant".into(),
                    text: text.clone(),
                    status: "running".into(),
                },
            );
            for offset in [0, text.len()] {
                ui.native_offset = offset;
                let height = inline_height(&ui, &snapshot, 80, 40);
                let layout = root_layout(&ui, &snapshot, Rect::new(0, 0, 80, height), None);
                assert_eq!(
                    layout.live.height as usize,
                    live_rows(&ui, &snapshot, 80).len()
                );
                if offset == text.len() {
                    assert_eq!(height, 4);
                }
                let buffer = draw(&ui, &snapshot, 80, height);
                assert!(plain(&buffer).contains("retained input"));
                assert_eq!(buffer[(0, layout.composer.y)].symbol(), "\u{2503}");
            }
        }
        snapshot.active.as_mut().unwrap().text = "first\nsecond\n".into();
        ui.pending.get_mut(&1).unwrap().text = "first\nsecond\n".into();
        ui.native_offset = 0;
        ui.observe_live(&snapshot);
        let plan = ui.plan_publication(&snapshot, 80);
        assert_eq!(inline_height(&ui, &snapshot, 80, 40), 6);
        assert_eq!(publication_height(&ui, &snapshot, 80, 40, &plan), 4);
        assert_eq!(ui.native_offset, 0);
        ui.commit_publication(plan);
        assert_eq!(inline_height(&ui, &snapshot, 80, 40), 4);
    }

    #[test]
    fn publication_retains_only_two_bounded_utf8_attempt_tails() {
        let (mut ui, mut snapshot) = fixture();
        for id in ["first", "retiring", "current"] {
            snapshot.active = Some(crate::runtime::ActiveView {
                attempt_id: id.into(),
                text: format!("{} {id}", "\u{03bb}".repeat(ACTIVE_BYTES)),
                truncated: false,
                durable_bytes: 0,
                message_id: String::new(),
            });
            ui.observe_live(&snapshot);
        }
        snapshot.active = None;
        let projection = ui.live_projection(None);
        let texts = ui.live_texts(&snapshot, &projection);
        assert_eq!(texts.len(), 2);
        assert!(texts.iter().all(|text| text.len() <= ACTIVE_BYTES));
        assert!(texts[0].ends_with("retiring"));
        assert!(texts[1].ends_with("current"));
    }

    #[test]
    fn publication_live_tails_keep_newest_attempt_order_while_older_native_head_waits() {
        let (mut ui, mut snapshot) = fixture();
        ui.view = View::History;
        ui.full_history = true;
        for (seq, id) in [(1, "A"), (2, "B"), (3, "C")] {
            let text = format!("tail {id}");
            snapshot.active = Some(crate::runtime::ActiveView {
                attempt_id: id.into(),
                text: text.clone(),
                truncated: false,
                durable_bytes: 0,
                message_id: String::new(),
            });
            ui.observe_live(&snapshot);
            ui.pending.insert(
                seq,
                crate::runtime::HistoryEntry {
                    seq,
                    attempt_id: id.into(),
                    kind: "assistant".into(),
                    text,
                    status: "completed".into(),
                },
            );
        }
        ui.view = View::Root;
        ui.full_history = false;
        ui.observe_live(&snapshot);
        assert!(ui.plan_publication(&snapshot, 80).rows.is_none());
        let entries = ui
            .pending
            .values()
            .cloned()
            .map(|entry| crate::runtime::NativeEntry {
                entry,
                category: None,
            })
            .collect();
        ui.completed_with(
            super::super::state::Done {
                op: super::super::state::Op::Tail,
                result: Ok(crate::runtime::Reply::NativeTranscript(entries)),
            },
            |_| Ok(()),
        )
        .unwrap();
        for active in [snapshot.active.clone(), None] {
            snapshot.active = active;
            assert_eq!(
                ui.live_texts(&snapshot, &ui.live_projection(None)),
                ["tail B", "tail C"]
            );
            let plan = ui.plan_publication(&snapshot, 80);
            assert_eq!(
                ui.live_texts(&snapshot, &ui.live_projection(Some(&plan))),
                ["tail B", "tail C"]
            );
            assert_eq!(ui.pending.first_key_value().unwrap().0, &1);
            assert_eq!(ui.native_offset, 0);
        }
        let mut printed = Vec::new();
        for seq in 1..=3 {
            assert_eq!(ui.pending.first_key_value().unwrap().0, &seq);
            ui.publish_with(&snapshot, 80, |rows| {
                printed.extend(rows.iter().map(ToString::to_string));
                Ok(())
            })
            .unwrap();
            assert_eq!(ui.published, Some(seq));
            assert_eq!(ui.native_offset, 0);
            if seq == 1 {
                assert_eq!(
                    ui.live_texts(&snapshot, &ui.live_projection(None)),
                    ["tail B", "tail C"]
                );
            }
        }
        assert_eq!(
            printed
                .iter()
                .filter(|row| !row.trim().is_empty())
                .map(|row| row.trim())
                .collect::<Vec<_>>(),
            ["tail A", "tail B", "tail C"]
        );
        assert!(ui.pending.is_empty());
        assert!(
            ui.live_texts(&snapshot, &ui.live_projection(None))
                .is_empty()
        );
    }

    #[test]
    fn publication_live_fallback_is_not_evicted_by_completion_tombstones() {
        let (mut ui, mut snapshot) = fixture();
        for (seq, id) in [(1, "A"), (2, "B")] {
            let text = format!("tail {id}");
            snapshot.active = Some(crate::runtime::ActiveView {
                attempt_id: id.into(),
                text: text.clone(),
                truncated: false,
                durable_bytes: 0,
                message_id: String::new(),
            });
            ui.pending.insert(
                seq,
                crate::runtime::HistoryEntry {
                    seq,
                    attempt_id: id.into(),
                    kind: "assistant".into(),
                    text,
                    status: "completed".into(),
                },
            );
            ui.publish_with(&snapshot, 80, |_| Ok(())).unwrap();
        }
        ui.pending.insert(
            3,
            crate::runtime::HistoryEntry {
                seq: 3,
                attempt_id: "C".into(),
                kind: "assistant".into(),
                text: "tail C".into(),
                status: "completed".into(),
            },
        );
        // The active snapshot still identifies completed B; C was never observed live.
        assert_eq!(
            ui.live_texts(&snapshot, &ui.live_projection(None)),
            ["tail C"]
        );
        snapshot.active = None;
        assert_eq!(
            ui.live_texts(&snapshot, &ui.live_projection(None)),
            ["tail C"]
        );
        snapshot.active = Some(crate::runtime::ActiveView {
            attempt_id: "D".into(),
            text: "tail D".into(),
            truncated: false,
            durable_bytes: 0,
            message_id: String::new(),
        });
        assert_eq!(
            ui.live_texts(&snapshot, &ui.live_projection(None)),
            ["tail C", "tail D"]
        );
        let plan = ui.plan_publication(&snapshot, 80);
        assert_eq!(
            ui.live_texts(&snapshot, &ui.live_projection(Some(&plan))),
            ["tail D"]
        );
        assert_eq!(ui.pending.first_key_value().unwrap().0, &3);
        assert_eq!(ui.published, Some(2));
    }

    #[test]
    fn queued_publication_does_not_hold_cancelled_editing_or_ambiguous_cards() {
        for change in ["cancel", "edit", "ambiguous", "not-fifo", "edit-new-active"] {
            let (mut ui, mut snapshot) = fixture();
            snapshot.active = Some(crate::runtime::ActiveView {
                attempt_id: "old".into(),
                text: String::new(),
                truncated: false,
                durable_bytes: 0,
                message_id: String::new(),
            });
            snapshot.queue = ["first", "second"]
                .into_iter()
                .map(|id| crate::runtime::QueueItem {
                    id: id.into(),
                    text: id.into(),
                    revision: 0,
                    state: QueueState::Queued,
                })
                .collect();
            ui.observe_live(&snapshot);
            match change {
                "cancel" => {
                    snapshot.queue.remove(0);
                }
                "edit" | "edit-new-active" => {
                    snapshot.queue[0].state = QueueState::Editing;
                }
                "ambiguous" => snapshot.queue.clear(),
                "not-fifo" => {
                    snapshot.queue.remove(1);
                }
                _ => unreachable!(),
            }
            if matches!(change, "ambiguous" | "not-fifo" | "edit-new-active") {
                snapshot.active.as_mut().unwrap().attempt_id = "new".into();
            }
            ui.observe_live(&snapshot);
            assert_eq!(
                projected_queue_rows(&ui, &snapshot, 80, 6, None),
                queue_rows(&snapshot, 80, 6),
                "{change}"
            );
        }
    }

    #[test]
    fn queued_publication_never_associates_a_cancelled_card_with_an_unrelated_coalesced_start() {
        for text in ["same input", "different input"] {
            let (mut ui, mut snapshot) = fixture();
            snapshot.active = Some(crate::runtime::ActiveView {
                attempt_id: "old".into(),
                message_id: "old-message".into(),
                text: String::new(),
                truncated: false,
                durable_bytes: 0,
            });
            snapshot.queue.push(crate::runtime::QueueItem {
                id: "cancelled-A".into(),
                text: "same input".into(),
                revision: 0,
                state: QueueState::Queued,
            });
            ui.observe_live(&snapshot);
            snapshot.queue.clear();
            snapshot.active = Some(crate::runtime::ActiveView {
                attempt_id: "unrelated-B".into(),
                message_id: "new-message-B".into(),
                text: text.into(),
                truncated: false,
                durable_bytes: 0,
            });
            ui.observe_live(&snapshot);
            assert!(
                ui.queue_texts(&snapshot, &ui.live_projection(None))
                    .is_empty()
            );
            assert!(projected_queue_rows(&ui, &snapshot, 80, 6, None).is_empty());
        }
    }

    #[test]
    fn queued_publication_skipped_snapshots_hold_only_the_authoritatively_started_oldest_id() {
        for message_id in ["A", "C"] {
            let (mut ui, mut snapshot) = fixture();
            snapshot.queue = ["A", "B", "C"]
                .into_iter()
                .map(|id| crate::runtime::QueueItem {
                    id: id.into(),
                    text: format!("input {id}"),
                    revision: 0,
                    state: QueueState::Queued,
                })
                .collect();
            ui.observe_live(&snapshot);
            snapshot.queue.clear();
            snapshot.active = Some(crate::runtime::ActiveView {
                attempt_id: "latest".into(),
                message_id: message_id.into(),
                text: String::new(),
                truncated: false,
                durable_bytes: 0,
            });
            ui.observe_live(&snapshot);
            let texts = ui.queue_texts(&snapshot, &ui.live_projection(None));
            if message_id == "A" {
                assert_eq!(texts, [("input A", false)]);
            } else {
                assert!(texts.is_empty());
            }
        }
    }

    #[test]
    fn queued_publication_keeps_prefix_ellipsis_and_accepted_before_runtime_queue() {
        let (mut ui, mut snapshot) = fixture();
        snapshot.active = Some(crate::runtime::ActiveView {
            attempt_id: "old".into(),
            text: String::new(),
            truncated: false,
            durable_bytes: 0,
            message_id: String::new(),
        });
        snapshot.queue = vec![
            crate::runtime::QueueItem {
                id: "first".into(),
                text: "\u{03bb}".repeat(5000),
                revision: 0,
                state: QueueState::Queued,
            },
            crate::runtime::QueueItem {
                id: "second".into(),
                text: "new queued".into(),
                revision: 0,
                state: QueueState::Queued,
            },
        ];
        ui.observe_live(&snapshot);
        let expected: Vec<_> = [1, 2, 10, 80]
            .into_iter()
            .map(|width| queue_rows(&snapshot, width, 6))
            .collect();
        snapshot.queue.remove(0);
        snapshot.active.as_mut().unwrap().attempt_id = "new".into();
        snapshot.active.as_mut().unwrap().message_id = "first".into();
        ui.observe_live(&snapshot);
        for (width, expected) in [1, 2, 10, 80].into_iter().zip(expected) {
            assert_eq!(
                projected_queue_rows(&ui, &snapshot, width, 6, None),
                expected
            );
        }
        let texts = ui.queue_texts(&snapshot, &ui.live_projection(None));
        assert_eq!(texts.len(), 2);
        assert!(texts[0].0.len() <= 4096 && texts[0].1);
        assert_eq!(texts[1], ("new queued", false));
        assert_eq!(snapshot.queue.len(), 1);
        assert_eq!(snapshot.queue[0].id, "second");
        for width in [1, 2, 10, 80] {
            for height in 0..24 {
                let owned = inline_height(&ui, &snapshot, width, height);
                assert_eq!(inline_height(&ui, &snapshot, width, owned), owned);
            }
        }
    }

    #[test]
    fn queued_publication_matches_attempt_ids_not_duplicate_text_and_bounds_fullscreen_backlog() {
        let (mut ui, mut snapshot) = fixture();
        ui.view = View::History;
        ui.full_history = true;
        snapshot.queue = ["message-A", "message-B", "message-C"]
            .into_iter()
            .map(|id| crate::runtime::QueueItem {
                id: id.into(),
                text: "same input".into(),
                revision: 0,
                state: QueueState::Queued,
            })
            .collect();
        ui.observe_live(&snapshot);
        for id in ["attempt-A", "attempt-B", "attempt-C"] {
            let message_id = snapshot.queue.remove(0).id;
            snapshot.active = Some(crate::runtime::ActiveView {
                attempt_id: id.into(),
                text: String::new(),
                truncated: false,
                durable_bytes: 0,
                message_id,
            });
            ui.observe_live(&snapshot);
        }
        ui.view = View::Root;
        ui.full_history = false;
        ui.observe_live(&snapshot);
        assert!(ui.plan_publication(&snapshot, 80).rows.is_none());
        let entries = [(1, "attempt-A"), (2, "attempt-B"), (3, "attempt-C")]
            .into_iter()
            .map(|(seq, id)| crate::runtime::NativeEntry {
                entry: crate::runtime::HistoryEntry {
                    seq,
                    attempt_id: id.into(),
                    kind: "user".into(),
                    text: "same input".into(),
                    status: "accepted".into(),
                },
                category: None,
            })
            .collect();
        ui.completed_with(
            super::super::state::Done {
                op: super::super::state::Op::Tail,
                result: Ok(crate::runtime::Reply::NativeTranscript(entries)),
            },
            |_| Ok(()),
        )
        .unwrap();
        assert!(snapshot.queue.is_empty());
        assert_eq!(
            ui.queue_texts(&snapshot, &ui.live_projection(None)),
            [("same input", false); 2]
        );
        let mut printed = Vec::new();
        for (seq, id, expected) in [
            (1, "attempt-A", 2),
            (2, "attempt-B", 1),
            (3, "attempt-C", 0),
        ] {
            ui.pending.insert(
                seq,
                crate::runtime::HistoryEntry {
                    seq,
                    attempt_id: id.into(),
                    kind: "user".into(),
                    text: "same input".into(),
                    status: "accepted".into(),
                },
            );
            let plan = ui.plan_publication(&snapshot, 80);
            assert_eq!(
                ui.queue_texts(&snapshot, &ui.live_projection(Some(&plan)))
                    .len(),
                expected
            );
            let before = ui.queue_texts(&snapshot, &ui.live_projection(None)).len();
            assert!(
                ui.publish_with(&snapshot, 80, |_| anyhow::bail!("output failed"))
                    .is_err()
            );
            assert_eq!(
                ui.queue_texts(&snapshot, &ui.live_projection(None)).len(),
                before
            );
            ui.publish_with(&snapshot, 80, |rows| {
                printed.extend(rows.iter().map(ToString::to_string));
                Ok(())
            })
            .unwrap();
            assert_eq!(
                ui.queue_texts(&snapshot, &ui.live_projection(None)).len(),
                expected
            );
        }
        assert_eq!(
            printed
                .iter()
                .filter(|row| row.as_str() == "\u{2503} same input")
                .count(),
            3
        );
        assert_eq!(ui.published, Some(3));
    }

    #[test]
    fn publication_projection_includes_the_newly_visible_pending_head() {
        for kind in ["user", "notice"] {
            let (mut ui, snapshot) = fixture();
            ui.pending.insert(
                1,
                crate::runtime::HistoryEntry {
                    seq: 1,
                    attempt_id: "old".into(),
                    kind: kind.into(),
                    text: "entry".into(),
                    status: "complete".into(),
                },
            );
            ui.pending.insert(
                2,
                crate::runtime::HistoryEntry {
                    seq: 2,
                    attempt_id: "old".into(),
                    kind: "assistant".into(),
                    text: "authoritative response".into(),
                    status: "complete".into(),
                },
            );
            let plan = ui.plan_publication(&snapshot, 80);
            let expected: Vec<_> = (0..40)
                .map(|height| publication_height(&ui, &snapshot, 80, height, &plan))
                .collect();
            ui.commit_publication(plan);
            for (height, expected) in expected.into_iter().enumerate() {
                assert_eq!(
                    inline_height(&ui, &snapshot, 80, height as u16),
                    expected,
                    "{kind} at height {height}"
                );
            }
            assert!(
                live_rows(&ui, &snapshot, 80)
                    .iter()
                    .any(|row| row.to_string().contains("authoritative response"))
            );
        }
    }

    #[test]
    fn publication_keeps_retiring_text_until_authoritative_output_and_never_repeats_it() {
        let (mut ui, mut snapshot) = fixture();
        snapshot.active = Some(crate::runtime::ActiveView {
            attempt_id: "old".into(),
            text: "published\nold partial".into(),
            truncated: false,
            durable_bytes: 0,
            message_id: String::new(),
        });
        ui.pending.insert(
            1,
            crate::runtime::HistoryEntry {
                seq: 1,
                attempt_id: "old".into(),
                kind: "assistant".into(),
                text: "published\nold partial".into(),
                status: "running".into(),
            },
        );
        let mut printed = Vec::new();
        ui.publish_with(&snapshot, 80, |rows| {
            printed.extend(rows.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            live_rows(&ui, &snapshot, 80)[0].to_string().trim(),
            "old partial"
        );
        snapshot.active = None;
        ui.observe_live(&snapshot);
        assert!(
            live_rows(&ui, &snapshot, 80)
                .iter()
                .any(|row| row.to_string().contains("old partial"))
        );
        snapshot.active = Some(crate::runtime::ActiveView {
            attempt_id: "new".into(),
            text: "new partial".into(),
            truncated: false,
            durable_bytes: 0,
            message_id: String::new(),
        });
        ui.observe_live(&snapshot);
        let rows = live_rows(&ui, &snapshot, 80);
        assert!(
            rows.iter()
                .any(|row| row.to_string().contains("old partial"))
        );
        assert!(
            rows.iter()
                .any(|row| row.to_string().contains("new partial"))
        );
        ui.pending.get_mut(&1).unwrap().status = "complete".into();
        ui.pending.get_mut(&1).unwrap().text = "published\nold partial final".into();
        let plan = ui.plan_publication(&snapshot, 80);
        let projected = publication_height(&ui, &snapshot, 80, 40, &plan);
        assert!(
            ui.publish_with(&snapshot, 80, |_| anyhow::bail!("write failed"))
                .is_err()
        );
        assert_eq!(ui.native_offset, "published\n".len());
        assert!(
            live_rows(&ui, &snapshot, 80)
                .iter()
                .any(|row| row.to_string().contains("old partial final"))
        );
        ui.publish_with(&snapshot, 80, |rows| {
            printed.extend(rows.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
        assert_eq!(inline_height(&ui, &snapshot, 80, 40), projected);
        assert_eq!(
            printed
                .iter()
                .filter(|row| row.trim() == "published")
                .count(),
            1
        );
        assert_eq!(
            printed
                .iter()
                .filter(|row| row.trim() == "old partial final")
                .count(),
            1
        );
        assert!(
            !live_rows(&ui, &snapshot, 80)
                .iter()
                .any(|row| row.to_string().contains("old partial"))
        );
        ui.pending.insert(
            2,
            crate::runtime::HistoryEntry {
                seq: 2,
                attempt_id: "new".into(),
                kind: "assistant".into(),
                text: "new partial".into(),
                status: "complete".into(),
            },
        );
        ui.publish_with(&snapshot, 80, |_| Ok(())).unwrap();
        // A late watch snapshot of the just-published attempt is not a fresh live response.
        assert!(live_rows(&ui, &snapshot, 80).is_empty());
        assert_eq!(inline_height(&ui, &snapshot, 80, 40), 4);
        ui.publish_with(&snapshot, 80, |_| panic!("completed response replayed"))
            .unwrap();
        ui.observe_live(&snapshot);
        assert!(live_rows(&ui, &snapshot, 80).is_empty());
    }

    #[test]
    fn leased_message_disappears_from_pending_preview_and_hints_use_stop_only_controls() {
        let (mut ui, mut snapshot) = fixture();
        snapshot.paused = true;
        snapshot.queue.push(crate::runtime::QueueItem {
            id: "pending".into(),
            text: "pending original".into(),
            revision: 1,
            state: QueueState::Editing,
        });
        ui.edit = Some(("pending".into(), 1));
        ui.draft.set_text("pending original");
        let layout = root_layout(&ui, &snapshot, Rect::new(0, 0, 100, 30), None);
        assert_eq!(layout.queue.height, 0);
        let text = plain(&draw(&ui, &snapshot, 100, layout.status.bottom()));
        assert!(!text.contains('\u{250b}'));
        assert_eq!(text.matches("pending original").count(), 1);
        assert!(text.contains("stopped") && text.contains("send to continue"));
        assert!(text.contains("Esc restore"));
        for removed in ["/queue", "/pause", "/run", "steer", "after", "Esc commands"] {
            assert!(!text.contains(removed), "obsolete hint {removed}");
        }
    }

    #[test]
    fn idle_bars_status_starts_at_column_zero_and_clears_previous_animation_cells() {
        let (ui, mut snapshot) = fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 1)).unwrap();
        snapshot.phase = Phase::Receiving;
        snapshot.active = Some(crate::runtime::ActiveView {
            attempt_id: "active".into(),
            text: String::new(),
            truncated: false,
            durable_bytes: 0,
            message_id: String::new(),
        });
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(status_line_elapsed(&ui, &snapshot, Duration::ZERO)),
                    frame.area(),
                )
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "\u{2581}");
        snapshot.phase = Phase::Idle;
        snapshot.active = None;
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new(status_line(&ui, &snapshot)), frame.area())
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "r");
        assert!(plain(buffer).starts_with("ready"));
        assert!(
            !plain(buffer)
                .chars()
                .any(|ch| ('\u{2581}'..='\u{2588}').contains(&ch))
        );
    }

    #[test]
    fn settings_show_the_held_outcome_and_keep_one_blank_row_before_input() {
        let (mut ui, mut snapshot) = fixture();
        ui.view = View::Settings;
        ui.interface.status_preset = StatusPreset::Minimal;
        ui.settings_feedback = Some(("Status preset: bars -> minimal".into(), false));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        ui.record_action(
            "status set to minimal",
            crate::runtime::NoticeLevel::Info,
            "settings",
        );
        ui.flush_notices(&tx);
        let work = rx.try_recv().unwrap();
        ui.completed_with(
            super::super::state::Done {
                op: work.op,
                result: Ok(crate::runtime::Reply::Notice(
                    crate::runtime::HistoryEntry {
                        seq: 1,
                        attempt_id: String::new(),
                        kind: "notice".into(),
                        text: "status set to minimal".into(),
                        status: "info".into(),
                    },
                )),
            },
            |_| Ok(()),
        )
        .unwrap();
        let before = inline_height(&ui, &snapshot, 100, 40);
        let plan = ui.plan_publication(&snapshot, 100);
        let expected = publication_height(&ui, &snapshot, 100, 40, &plan);
        assert_eq!(expected, before + 1);
        let mut native = Vec::new();
        ui.publish_with(&snapshot, 100, |rows| {
            native.extend(rows.iter().map(ToString::to_string));
            Ok(())
        })
        .unwrap();
        assert_eq!(native, ["", "\u{25cf} Settings"]);
        assert_eq!(inline_height(&ui, &snapshot, 100, 40), expected);
        ui.notice = Some((
            "status set to minimal".into(),
            Instant::now() - Duration::from_secs(5),
            false,
        ));
        for active in [false, true] {
            snapshot.active = active.then(|| crate::runtime::ActiveView {
                attempt_id: "active".into(),
                text: "generating".into(),
                truncated: false,
                durable_bytes: 0,
                message_id: String::new(),
            });
            snapshot.phase = if active {
                Phase::Receiving
            } else {
                Phase::Idle
            };
            let layout = panel_layout(&ui, &snapshot, Rect::new(0, 0, 100, expected));
            let buffer = draw(&ui, &snapshot, 100, expected);
            let text = plain(&buffer);
            assert!(text.contains("\u{2514} status set to minimal"));
            assert!(text.contains("Status preset: bars -> minimal"));
            assert_eq!(layout.composer.y, layout.notice.bottom() + 1);
            assert!((0..100).all(|x| buffer[(x, layout.composer.y - 1)].symbol() == " "));
        }
        ui.view = View::Details;
        let height = inline_height(&ui, &snapshot, 100, 40);
        let layout = panel_layout(&ui, &snapshot, Rect::new(0, 0, 100, height));
        assert_eq!(layout.composer.y, layout.notice.bottom() + 1);
    }
}
