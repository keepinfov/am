//! Inline panel bodies. The common renderer owns composer, separators and status.
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{
    state::{HELP_CATEGORIES, Ui, View},
    theme::{muted, notice, strong, warning},
    transcript::clean,
};

pub(super) fn hint(ui: &Ui, width: u16) -> &'static str {
    let full = match ui.view {
        View::Settings if ui.settings_editor.is_some() => {
            "Up/Down choose     Enter apply     Esc cancel"
        }
        View::Settings => "Up/Down navigate     Space toggle     Enter open     Esc back",
        View::Help
            if ui
                .help_detail
                .is_some_and(|spec| crate::commands::parse(spec.name).is_err()) =>
        {
            "Enter insert command     Esc back"
        }
        View::Help if ui.help_detail.is_some() => {
            "Up/Down choose     Enter run/insert     Esc back"
        }
        View::Help => {
            "Up/Down navigate     Tab category     Enter open     Alt+Enter insert     Esc back"
        }
        _ => "",
    };
    if full.len() <= usize::from(width) {
        return full;
    }
    let compact = match ui.view {
        View::Settings if ui.settings_editor.is_some() => "Arrows / Enter apply / Esc",
        View::Settings => "Space toggle / Enter / Esc",
        View::Help
            if ui
                .help_detail
                .is_some_and(|spec| crate::commands::parse(spec.name).is_err()) =>
        {
            "Choose Insert / Enter / Esc"
        }
        View::Help if ui.help_detail.is_some() => "Arrows / Enter Run/Insert / Esc",
        View::Help => "Type filter / Tab / Enter / Esc",
        _ => "",
    };
    if compact.len() <= usize::from(width) {
        compact
    } else if width >= 16 {
        "Enter / Esc back"
    } else if width >= 8 {
        "Esc back"
    } else if width >= 3 {
        "Esc"
    } else {
        ""
    }
}

/// Preferred body rows, excluding composer, title, separators and status/hint.
pub(super) fn body_rows(ui: &Ui) -> u16 {
    match ui.view {
        View::Settings => {
            let catalog = crate::settings::catalog(&ui.interface);
            (catalog.len()
                + 5
                + ui.settings_editor.map_or(0, |_| {
                    catalog
                        .get(ui.settings_selected)
                        .map_or(0, |setting| setting.choices.len())
                })) as u16
        }
        View::Help if ui.help_detail.is_some() => 7,
        View::Help => ui.help_commands().len().clamp(1, 20) as u16,
        _ => 0,
    }
}

pub(super) fn render_settings(frame: &mut Frame, ui: &Ui, title: Rect, body: Rect) {
    frame.render_widget(
        Paragraph::new(Line::styled("Settings / 4 UI preferences", strong())),
        title,
    );
    let catalog = crate::settings::catalog(&ui.interface);
    let mut rows = Vec::new();
    let mut selected_row = 0;
    for (index, setting) in catalog.iter().enumerate() {
        let selected = index == ui.settings_selected;
        if selected {
            selected_row = rows.len();
        }
        let value = if setting.choices == ["true", "false"] {
            if setting.value == "true" {
                "[on]".to_owned()
            } else {
                "[off]".to_owned()
            }
        } else if selected {
            format!("{} \u{25be}", setting.value)
        } else {
            setting.value.clone()
        };
        rows.push(Line::styled(
            format!("  {:18} {}", setting.label, value),
            if selected { strong() } else { muted() },
        ));
        if selected && let Some(choice) = ui.settings_editor {
            for (option, value) in setting.choices.iter().enumerate() {
                if choice == option {
                    selected_row = rows.len();
                }
                let label = match *value {
                    "true" => "on",
                    "false" => "off",
                    value => value,
                };
                rows.push(Line::styled(
                    format!(
                        "    {} {label}",
                        if *value == setting.value {
                            "\u{25cf}"
                        } else {
                            "\u{25cb}"
                        }
                    ),
                    if choice == option { strong() } else { muted() },
                ));
            }
        }
    }
    rows.push(Line::from(""));
    for (offset, label) in [
        if ui.saving_defaults {
            "Saving defaults..."
        } else {
            "Save as defaults"
        },
        "Bindings",
    ]
    .into_iter()
    .enumerate()
    {
        let selected = ui.settings_selected == catalog.len() + offset;
        if selected {
            selected_row = rows.len();
        }
        rows.push(Line::styled(
            format!("  {label}"),
            if selected { strong() } else { muted() },
        ));
    }
    if let Some(setting) = catalog.get(ui.settings_selected) {
        rows.push(Line::styled(format!("  {}", setting.description), muted()));
    } else if ui.settings_selected == catalog.len() {
        rows.push(Line::styled(
            "  Future clients use these values; explicit config takes precedence.",
            muted(),
        ));
    } else {
        rows.push(Line::styled(
            "  Customize shortcuts and command bindings.",
            muted(),
        ));
    }
    // A one-row dropdown must show the choice that Enter will apply.
    if body.height == 1 && ui.settings_editor.is_some() {
        render_rows(frame, body, rows, selected_row);
        return;
    }
    render_rows(
        frame,
        Rect {
            height: body.height.saturating_sub(1),
            ..body
        },
        rows,
        selected_row,
    );
    if body.height > 0 {
        let feedback = ui.settings_feedback.as_ref().map_or_else(
            || Line::from(""),
            |(text, error)| {
                Line::styled(
                    format!("  {}", clean(text, 1024).replace(['\n', '\t'], " ")),
                    if *error { warning() } else { notice() },
                )
            },
        );
        frame.render_widget(
            Paragraph::new(feedback),
            Rect::new(body.x, body.y + body.height - 1, body.width, 1),
        );
    }
}

pub(super) fn render_help(frame: &mut Frame, ui: &Ui, title: Rect, body: Rect) {
    if let Some(spec) = ui.help_detail {
        frame.render_widget(
            Paragraph::new(Line::styled(format!("Commands / {}", spec.name), strong())),
            title,
        );
        let runnable = crate::commands::parse(spec.name).is_ok();
        let rows = vec![
            Line::styled(format!("  {}", spec.usage()), strong()),
            Line::styled(format!("  {}", spec.description), muted()),
            Line::styled(
                format!("  Keys: {}", clean(&ui.keys.label(spec.action), 256)),
                muted(),
            ),
            Line::styled(
                if runnable {
                    "  Nothing runs until you choose Run."
                } else {
                    "  Arguments required. Insert to complete the command."
                },
                muted(),
            ),
            Line::styled(
                if runnable {
                    "  Run"
                } else {
                    "  Run unavailable (arguments required)"
                },
                if runnable && ui.help_detail_selected == 0 {
                    strong()
                } else {
                    muted()
                },
            ),
            Line::styled(
                "  Insert into command field",
                if ui.help_detail_selected == 1 {
                    strong()
                } else {
                    muted()
                },
            ),
            Line::styled(
                "  Insert preserves the primary draft and its caret.",
                muted(),
            ),
        ];
        render_rows(frame, body, rows, 4 + ui.help_detail_selected);
        return;
    }
    let commands = ui.help_commands();
    frame.render_widget(
        Paragraph::new(help_header(commands.len(), ui.help_category, title.width)),
        title,
    );
    let rows = if commands.is_empty() {
        vec![Line::styled(
            "  No matching commands. Backspace clears the filter; Tab changes category.",
            muted(),
        )]
    } else {
        let command_width = commands
            .iter()
            .map(|spec| Span::raw(spec.name).width())
            .max()
            .unwrap_or(0);
        commands
            .iter()
            .enumerate()
            .map(|(index, spec)| {
                let style = if index == ui.help_selected {
                    strong()
                } else {
                    muted()
                };
                let keys = ui.keys.label(spec.action);
                Line::from(vec![
                    Span::styled(
                        format!("  {:command_width$}    {}", spec.name, spec.description),
                        style,
                    ),
                    Span::styled(
                        if keys.is_empty() {
                            String::new()
                        } else {
                            format!(" / {}", clean(&keys, 256))
                        },
                        muted(),
                    ),
                ])
            })
            .collect()
    };
    render_rows(frame, body, rows, ui.help_selected);
}

fn help_header(count: usize, category: usize, width: u16) -> Line<'static> {
    let width = usize::from(width);
    let prefix = format!("Commands {count}");
    let labels = HELP_CATEGORIES
        .iter()
        .enumerate()
        .map(|(index, label)| {
            if index == category {
                format!("[{label}]")
            } else {
                (*label).to_owned()
            }
        })
        .collect::<Vec<_>>();
    let active = &labels[category];
    if width < active.len() {
        let label = HELP_CATEGORIES[category]
            .chars()
            .take(width.saturating_sub(2))
            .collect::<String>();
        return Line::styled(
            format!("[{label}]").chars().take(width).collect::<String>(),
            strong(),
        );
    }
    let (mut start, mut end) = (0, labels.len());
    // Drop distant tabs first so the active category survives title clipping.
    while prefix.len()
        + labels[start..end]
            .iter()
            .map(|label| label.len() + 2)
            .sum::<usize>()
        > width
        && end - start > 1
    {
        if category - start > end - 1 - category {
            start += 1;
        } else {
            end -= 1;
        }
    }
    let prefix = prefix
        .chars()
        .take(width.saturating_sub(active.len() + 2))
        .collect::<String>();
    let mut spans = Vec::new();
    if !prefix.is_empty() {
        spans.push(Span::styled(prefix, strong()));
    }
    for (index, label) in labels.into_iter().enumerate().take(end).skip(start) {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            label,
            if index == category { strong() } else { muted() },
        ));
    }
    Line::from(spans)
}

fn render_rows(frame: &mut Frame, body: Rect, rows: Vec<Line<'static>>, selected: usize) {
    let height = usize::from(body.height);
    let start = selected
        .saturating_sub(height.saturating_sub(1))
        .min(rows.len().saturating_sub(height));
    frame.render_widget(
        Paragraph::new(
            rows.into_iter()
                .skip(start)
                .take(height)
                .collect::<Vec<_>>(),
        ),
        body,
    );
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use ratatui::{
        Terminal,
        backend::TestBackend,
        buffer::Buffer,
        style::{Color, Modifier},
    };

    use super::*;
    use crate::{
        commands::{Action, COMMANDS},
        config::Config,
        runtime::{Phase, Snapshot},
    };

    fn ui(view: View) -> Ui {
        let now = Instant::now();
        let snapshot = Snapshot {
            session_id: "session".into(),
            agent_id: "agent".into(),
            conversation_id: "conversation".into(),
            phase: Phase::Idle,
            phase_started: now,
            last_event: now,
            active: None,
            queue: Vec::new(),
            paused: false,
            recovered: false,
            cursor: 0,
            error: None,
        };
        let mut ui = Ui::new(&Config::default(), &snapshot);
        ui.view = view;
        ui
    }

    fn rendered(ui: &Ui, width: u16, body_height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, body_height + 1)).unwrap();
        terminal
            .draw(|frame| {
                let title = Rect::new(0, 0, width, 1);
                let body = Rect::new(0, 1, width, body_height);
                if ui.view == View::Settings {
                    render_settings(frame, ui, title, body);
                } else {
                    render_help(frame, ui, title, body);
                }
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    fn assert_bright(buffer: &Buffer, x: u16, y: u16) {
        assert_eq!(buffer[(x, y)].fg, Color::Indexed(255));
        assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn help_header_matches_fx_tabs_count_and_styles_at_column_zero() {
        let mut ui = ui(View::Help);
        let buffer = rendered(&ui, 100, 2);
        let expected = format!(
            "Commands {}  [All]  AM  Input  Session  Preferences",
            COMMANDS.len()
        );
        assert_eq!(row(&buffer, 0), expected);
        assert_bright(&buffer, 0, 0);
        assert_bright(&buffer, expected.find("[All]").unwrap() as u16, 0);
        assert_eq!(
            buffer[(expected.find("AM").unwrap() as u16, 0)].fg,
            Color::Indexed(245)
        );
        ui.help_category = 3;
        let buffer = rendered(&ui, 100, 2);
        let expected = format!(
            "Commands {}  All  AM  Input  [Session]  Preferences",
            ui.help_commands().len()
        );
        assert_eq!(row(&buffer, 0), expected);
        assert_bright(&buffer, expected.find("[Session]").unwrap() as u16, 0);
        ui.help_category = 0;
        ui.help_query.set_text("/settings");
        let buffer = rendered(&ui, 100, 1);
        assert!(
            row(&buffer, 0).starts_with(&format!("Commands {}  [All]", ui.help_commands().len()))
        );
    }

    #[test]
    fn help_header_keeps_each_active_tab_on_narrow_terminals() {
        let mut ui = ui(View::Help);
        for (category, label) in HELP_CATEGORIES.iter().enumerate() {
            ui.help_category = category;
            let active = format!("[{label}]");
            for width in [80, 32, 20, active.len() as u16] {
                let buffer = rendered(&ui, width, 1);
                let title = row(&buffer, 0);
                assert!(title.contains(&active), "{width}: {title}");
                assert_bright(&buffer, title.find(&active).unwrap() as u16, 0);
            }
            for width in 1..active.len() as u16 {
                let buffer = rendered(&ui, width, 1);
                assert!(row(&buffer, 0).starts_with('['));
                if width > 1 {
                    assert!(row(&buffer, 0).ends_with(']'));
                }
            }
        }
    }

    #[test]
    fn help_descriptions_align_after_the_widest_command_and_share_selection_style() {
        let mut ui = ui(View::Help);
        ui.help_selected = 4;
        let commands = ui.help_commands();
        let description_x = commands.iter().map(|spec| spec.name.len()).max().unwrap() + 6;
        let buffer = rendered(&ui, 180, commands.len() as u16);
        for (index, spec) in commands.iter().enumerate() {
            let y = index as u16 + 1;
            let text = row(&buffer, y);
            assert!(text.starts_with(&format!("  {}", spec.name)));
            assert_eq!(text.find(spec.description), Some(description_x));
            assert!(!text.starts_with("  >"));
            if index == ui.help_selected {
                assert_bright(&buffer, 2, y);
                assert_bright(&buffer, description_x as u16, y);
            } else {
                assert_eq!(buffer[(description_x as u16, y)].fg, Color::Indexed(245));
                assert!(
                    !buffer[(description_x as u16, y)]
                        .modifier
                        .contains(Modifier::BOLD)
                );
            }
        }
    }

    #[test]
    fn settings_switches_and_action_spacing_match_the_preferred_body_height() {
        let mut ui = ui(View::Settings);
        ui.interface.prompt_history = false;
        assert_eq!(body_rows(&ui), 9);
        let buffer = rendered(&ui, 100, body_rows(&ui));
        assert_eq!(row(&buffer, 0), "Settings / 4 UI preferences");
        assert!(row(&buffer, 1).ends_with("[on]"));
        assert!(row(&buffer, 2).ends_with("[off]"));
        assert_eq!(row(&buffer, 5), "");
        assert_eq!(row(&buffer, 6), "  Save as defaults");
        assert_eq!(row(&buffer, 7), "  Bindings");
        assert_eq!(row(&buffer, 8), "  Show the runtime status line.");
        assert_eq!(row(&buffer, 9), "");
        ui.settings_selected = 5;
        let buffer = rendered(&ui, 100, body_rows(&ui));
        assert_eq!(
            row(&buffer, 8),
            "  Customize shortcuts and command bindings."
        );
        ui.saving_defaults = true;
        assert_eq!(
            row(&rendered(&ui, 100, body_rows(&ui)), 6),
            "  Saving defaults..."
        );
    }

    #[test]
    fn settings_radio_applied_marker_is_independent_of_the_bright_choice_cursor() {
        let mut ui = ui(View::Settings);
        ui.settings_selected = 2;
        ui.settings_editor = Some(2);
        assert_eq!(body_rows(&ui), 13);
        let buffer = rendered(&ui, 100, body_rows(&ui));
        assert!(row(&buffer, 3).ends_with("bars \u{25be}"));
        assert_eq!(row(&buffer, 4), "    \u{25cb} text");
        assert_eq!(row(&buffer, 5), "    \u{25cb} braille");
        assert_eq!(row(&buffer, 6), "    \u{25cb} minimal");
        assert_eq!(row(&buffer, 7), "    \u{25cf} bars");
        assert_eq!(buffer[(4, 7)].fg, Color::Indexed(245));
        assert_bright(&buffer, 4, 6);
        assert_eq!(row(&buffer, 9), "");
        assert!(row(&buffer, 12).contains("Choose text, braille, minimal, or bars"));
        ui.settings_editor = Some(1);
        let buffer = rendered(&ui, 100, body_rows(&ui));
        assert_bright(&buffer, 4, 5);
        assert_eq!(buffer[(4, 6)].fg, Color::Indexed(245));
        ui.settings_editor = Some(3);
        assert_bright(&rendered(&ui, 100, body_rows(&ui)), 4, 7);
        ui.settings_selected = 0;
        ui.settings_editor = Some(1);
        assert_eq!(body_rows(&ui), 11);
        let buffer = rendered(&ui, 100, body_rows(&ui));
        assert_eq!(row(&buffer, 2), "    \u{25cf} on");
        assert_eq!(row(&buffer, 3), "    \u{25cb} off");
        assert_bright(&buffer, 4, 3);
    }

    #[test]
    fn short_panel_windows_retain_the_selected_setting_choice_action_or_command() {
        let mut ui = ui(View::Settings);
        ui.settings_selected = 5;
        let buffer = rendered(&ui, 16, 2);
        assert_eq!(row(&buffer, 1), "  Bindings");
        assert_bright(&buffer, 2, 1);
        ui.settings_selected = 2;
        ui.settings_editor = Some(2);
        let buffer = rendered(&ui, 16, 1);
        assert_eq!(row(&buffer, 1), "    \u{25cb} minimal");
        assert_bright(&buffer, 4, 1);
        ui.settings_editor = Some(3);
        let buffer = rendered(&ui, 16, 1);
        assert_eq!(row(&buffer, 1), "    \u{25cf} bars");
        assert_bright(&buffer, 4, 1);
        ui.view = View::Help;
        ui.help_selected = ui.help_commands().len() - 1;
        let name = ui.help_commands()[ui.help_selected].name;
        let buffer = rendered(&ui, 16, 1);
        assert!(row(&buffer, 1).starts_with(&format!("  {name}")));
        assert_bright(&buffer, 2, 1);
        for width in [0, 1, 2] {
            rendered(&ui, width, 0);
        }
    }

    #[test]
    fn argument_required_run_is_unavailable_not_highlighted_as_an_action() {
        let mut ui = ui(View::Help);
        ui.help_detail = COMMANDS.iter().find(|spec| spec.action == Action::Send);
        ui.help_detail_selected = 0;
        let buffer = rendered(&ui, 100, body_rows(&ui));
        assert_eq!(row(&buffer, 0), "Commands / /send");
        assert_eq!(row(&buffer, 5), "  Run unavailable (arguments required)");
        assert_eq!(buffer[(2, 5)].fg, Color::Indexed(245));
        assert!(!buffer[(2, 5)].modifier.contains(Modifier::BOLD));
        assert!(hint(&ui, 32).to_ascii_lowercase().contains("insert"));
        ui.help_detail_selected = 1;
        let buffer = rendered(&ui, 100, body_rows(&ui));
        assert_bright(&buffer, 2, 6);
        ui.help_detail = COMMANDS.iter().find(|spec| spec.action == Action::Stop);
        ui.help_detail_selected = 0;
        let buffer = rendered(&ui, 100, body_rows(&ui));
        assert_eq!(row(&buffer, 5), "  Run");
        assert_bright(&buffer, 2, 5);
    }

    #[test]
    fn common_layout_hints_fit_a_thirty_two_column_terminal() {
        let mut ui = ui(View::Settings);
        assert!(hint(&ui, 32).len() <= 32);
        ui.settings_editor = Some(0);
        assert!(hint(&ui, 32).len() <= 32);
        ui.view = View::Help;
        assert!(hint(&ui, 32).len() <= 32);
        ui.help_detail = COMMANDS.iter().find(|spec| spec.action == Action::Send);
        assert!(hint(&ui, 32).len() <= 32);
        ui.help_detail = COMMANDS.iter().find(|spec| spec.action == Action::Stop);
        assert!(hint(&ui, 32).len() <= 32);
    }
}
