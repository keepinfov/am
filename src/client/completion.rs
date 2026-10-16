use std::ops::Range;

use crate::{
    commands::{self, Arguments, KeyBindings},
    config::Interface,
    runtime::{DraftRecord, Snapshot},
    settings,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    Command,
    Setting,
    Value,
    Option,
    MessageId,
    History,
}

#[derive(Clone, Debug)]
pub(super) struct Item {
    pub label: String,
    pub description: String,
    pub value: String,
    pub trailing_space: bool,
}

#[derive(Clone, Debug)]
pub(super) struct Completion {
    pub range: Range<usize>,
    pub items: Vec<Item>,
    pub hint: String,
    pub stage: Stage,
}

impl Completion {
    pub fn ghost(&self, text: &str, cursor: usize, selected: usize) -> Option<&str> {
        if cursor != self.range.end {
            return None;
        }
        let token = text.get(self.range.clone())?;
        let item = self.items.get(selected)?;
        item.value
            .strip_prefix(token)
            .filter(|suffix| !suffix.is_empty())
    }
}

/// Query identity is checked again on reply/attachment, so stale async reads
/// never replace completions for a different editor argument.
pub(super) fn history_query(menu: &Completion, text: &str, cursor: usize) -> Option<String> {
    if menu.stage != Stage::History
        || cursor < menu.range.start
        || cursor > text.len()
        || (cursor > menu.range.end
            && !text
                .get(menu.range.end..cursor)?
                .chars()
                .all(char::is_whitespace))
    {
        return None;
    }
    let query = text.get(menu.range.start..cursor)?.trim().to_owned();
    crate::runtime::validate_history_query(&query).ok()?;
    Some(query)
}

pub(super) fn attach_history(
    mut menu: Completion,
    text: &str,
    cursor: usize,
    cached_query: &str,
    entries: &[crate::runtime::HistoryMatch],
) -> Completion {
    if menu.stage == Stage::History {
        menu.items.retain(|item| item.value == "fullscreen");
    }
    if history_query(&menu, text, cursor).as_deref() != Some(cached_query) {
        return menu;
    }
    let mut seen = std::collections::HashSet::new();
    for entry in entries.iter().take(crate::runtime::MAX_HISTORY_MATCHES) {
        if entry.seq <= 0
            || !matches!(entry.kind.as_str(), "user" | "assistant")
            || !seen.insert(entry.seq)
        {
            continue;
        }
        let preview = super::transcript::clean(&entry.preview, 640)
            .replace(['\n', '\t'], " ")
            .chars()
            .take(crate::runtime::MAX_HISTORY_PREVIEW_CHARS)
            .collect::<String>();
        menu.items.push(Item {
            label: format!("{} {}", entry.seq, entry.kind),
            description: preview,
            value: entry.seq.to_string(),
            trailing_space: false,
        });
    }
    menu
}

#[cfg(test)]
pub(super) fn build(
    text: &str,
    cursor: usize,
    interface: &Interface,
    snapshot: &Snapshot,
) -> Option<Completion> {
    build_with_drafts(text, cursor, interface, snapshot, &[])
}

#[cfg(test)]
pub(super) fn build_with_drafts(
    text: &str,
    cursor: usize,
    interface: &Interface,
    snapshot: &Snapshot,
    drafts: &[DraftRecord],
) -> Option<Completion> {
    build_with_keys(
        text,
        cursor,
        interface,
        snapshot,
        drafts,
        &KeyBindings::default(),
    )
}

pub(super) fn build_with_keys(
    text: &str,
    cursor: usize,
    interface: &Interface,
    snapshot: &Snapshot,
    drafts: &[DraftRecord],
    keys: &KeyBindings,
) -> Option<Completion> {
    build_inner(text, cursor, interface, snapshot, drafts, keys, 0)
}

fn build_inner(
    text: &str,
    cursor: usize,
    interface: &Interface,
    snapshot: &Snapshot,
    drafts: &[DraftRecord],
    keys: &KeyBindings,
    depth: usize,
) -> Option<Completion> {
    if !text.starts_with('/')
        || text.starts_with("//")
        || text.contains(['\n', '\r'])
        || text.len() > 4096
        || depth >= 16
        || !text.is_char_boundary(cursor)
    {
        return None;
    }

    let mut tokens = Vec::new();
    let mut start = None;
    for (offset, ch) in text.char_indices() {
        if ch.is_whitespace() {
            if let Some(start) = start.take() {
                tokens.push(start..offset);
            }
        } else {
            start.get_or_insert(offset);
        }
    }
    if let Some(start) = start {
        tokens.push(start..text.len());
    }
    let index = tokens
        .iter()
        .position(|range| range.start <= cursor && cursor <= range.end)
        .unwrap_or_else(|| tokens.iter().filter(|range| range.end < cursor).count());
    let range = tokens
        .get(index)
        .filter(|range| range.start <= cursor && cursor <= range.end)
        .cloned()
        .unwrap_or(cursor..cursor);
    let query = &text[range.start..cursor];
    let mut completion = Completion {
        range,
        items: Vec::new(),
        hint: String::new(),
        stage: Stage::Command,
    };
    if index == 0 {
        completion.items = commands::matches(query)
            .into_iter()
            .map(|spec| Item {
                label: spec.name.into(),
                description: spec.description.into(),
                value: spec.name.into(),
                trailing_space: true,
            })
            .collect();
        completion.hint = "Choose a command; /help lists commands and key bindings".into();
        return Some(completion);
    }

    let command = &text[tokens.first()?.clone()];
    let spec = commands::COMMANDS
        .iter()
        .find(|spec| spec.name.eq_ignore_ascii_case(command))?;
    completion.hint = format!("Usage: {}", spec.usage());
    match spec.arguments {
        Arguments::None | Arguments::MessageId | Arguments::Mode | Arguments::Queue => return None,
        Arguments::Settings => {
            let catalog = settings::catalog(interface);
            if index == 1 {
                completion.stage = Stage::Setting;
                completion.items = catalog
                    .iter()
                    .map(|setting| Item {
                        label: setting.key.into(),
                        description: format!("{} Current: {}", setting.description, setting.value),
                        value: setting.key.into(),
                        trailing_space: true,
                    })
                    .collect();
                completion.hint =
                    "UI preferences for this client; choose a key, then a value".into();
                if let Some(setting) = catalog.iter().find(|setting| setting.key == query) {
                    completion.hint = setting_hint(setting);
                }
            } else {
                let key = &text[tokens.get(1)?.clone()];
                let setting = catalog.iter().find(|setting| setting.key == key)?;
                completion.hint = setting_hint(setting);
                if index == 3 {
                    let value = &text[tokens.get(2)?.clone()];
                    settings::Change::parse(&[key, value]).ok()?;
                    if tokens.len() > 4 {
                        return None;
                    }
                    completion.stage = Stage::Option;
                    completion.items.push(Item {
                        label: "--default".into(),
                        description: "Make default".into(),
                        value: "--default".into(),
                        trailing_space: true,
                    });
                } else if index != 2 {
                    return None;
                } else {
                    completion.stage = Stage::Value;
                    completion.items = setting
                        .choices
                        .iter()
                        .map(|value| Item {
                            label: (*value).into(),
                            description: if *value == setting.value {
                                "Current value".into()
                            } else {
                                setting.description.into()
                            },
                            value: (*value).into(),
                            trailing_space: true,
                        })
                        .collect();
                }
            }
        }
        Arguments::History => {
            if index < 1 || (tokens.len() > 2 && &text[tokens[1].clone()] == "fullscreen") {
                return None;
            }
            let start = tokens[0].end + text[tokens[0].end..].len()
                - text[tokens[0].end..].trim_start().len();
            if cursor < start {
                return None;
            }
            completion.range = start..text.trim_end().len().max(start);
            completion.stage = Stage::History;
            completion
                .hint
                .push_str("; opens the fullscreen conversation and action journal; Tab searches saved dialogue by text or sequence");
            completion.items.push(Item {
                label: "fullscreen".into(),
                description: "Open the unified fullscreen journal".into(),
                value: "fullscreen".into(),
                trailing_space: true,
            });
            return Some(rank(completion, text[start..cursor].trim()));
        }
        Arguments::Draft | Arguments::SendDraft => {
            if spec.arguments == Arguments::Draft && index == 1 {
                completion.stage = Stage::Value;
                completion.items = ["save", "delete"]
                    .into_iter()
                    .map(|value| Item {
                        label: value.into(),
                        value: value.into(),
                        description: if value == "save" {
                            "Save the primary composer"
                        } else {
                            "Delete a saved draft by ID"
                        }
                        .into(),
                        trailing_space: true,
                    })
                    .collect();
            } else if (spec.arguments == Arguments::SendDraft && index == 1)
                || (spec.arguments == Arguments::Draft
                    && index == 2
                    && &text[tokens.get(1)?.clone()] == "delete")
            {
                completion.stage = Stage::MessageId;
                completion.items = drafts
                    .iter()
                    .map(|draft| Item {
                        label: draft.id.clone(),
                        value: draft.id.clone(),
                        description: super::transcript::clean(&draft.text, 160).replace('\n', " "),
                        trailing_space: spec.arguments == Arguments::Draft,
                    })
                    .collect();
            } else {
                return None;
            }
        }
        Arguments::Commands => {
            if index != 1 {
                return None;
            }
            completion.stage = Stage::Value;
            completion.items = ["keep"]
                .iter()
                .map(|value| Item {
                    label: (*value).into(),
                    value: (*value).into(),
                    description: "Keep the secondary command field open".into(),
                    trailing_space: true,
                })
                .collect();
        }
        Arguments::Bind | Arguments::Unbind if index == 1 => {
            completion.stage = Stage::Value;
            completion.items = keys
                .macros
                .iter()
                .filter(|binding| commands::parse_bind(&format!("{} /help", binding.key)).is_ok())
                .map(|binding| Item {
                    label: binding.key.clone(),
                    value: binding.key.clone(),
                    description: super::transcript::clean(&binding.commands.join("; "), 160),
                    trailing_space: true,
                })
                .collect();
            completion
                .hint
                .push_str("; choose an existing macro chord or type a new portable chord");
        }
        Arguments::Unbind => {
            if index != 2 || tokens.len() > 3 {
                return None;
            }
            let chord = &text[tokens.get(1)?.clone()];
            commands::parse(&format!("/unbind {chord}")).ok()?;
            completion.stage = Stage::Option;
            completion.items.push(default_binding_option());
        }
        Arguments::Bind => {
            let chord = tokens.get(1)?;
            commands::parse_bind(&format!("{} /help", &text[chord.clone()])).ok()?;
            let last = tokens.last()?;
            let existing_default = &text[last.clone()] == "--default";
            let default = index + 1 >= tokens.len()
                && (query.is_empty() || query.starts_with('-'))
                && (!existing_default || completion.range.start == last.start)
                && complete_binding_args(
                    text[chord.start..completion.range.start].trim_end(),
                    depth,
                )
                .is_some_and(|saved| !saved);
            let end = if existing_default {
                last.start
            } else {
                text.len()
            };
            let mut nested = if existing_default && cursor >= last.start {
                None
            } else {
                chain_completion(
                    text,
                    chord.end..end,
                    (cursor, depth),
                    interface,
                    snapshot,
                    drafts,
                    keys,
                )
            };
            if existing_default
                && cursor < last.start
                && !text[cursor..last.start].contains(';')
                && let Some(menu) = &mut nested
            {
                menu.items.retain(|item| item.value != "--default");
            }
            if default {
                let menu = nested.get_or_insert_with(|| Completion {
                    range: completion.range.clone(),
                    items: Vec::new(),
                    hint: completion.hint.clone(),
                    stage: Stage::Option,
                });
                if menu.range == completion.range {
                    menu.items.retain(|item| item.value != "--default");
                    menu.items.push(default_binding_option());
                }
            }
            let menu = nested?;
            let query = &text[menu.range.start..cursor];
            return Some(rank(menu, query));
        }
    }
    Some(rank(completion, query))
}

fn default_binding_option() -> Item {
    Item {
        label: "--default".into(),
        description: "Save binding defaults".into(),
        value: "--default".into(),
        trailing_space: true,
    }
}

fn complete_binding_args(args: &str, depth: usize) -> Option<bool> {
    if depth >= 16 {
        return None;
    }
    let (_, steps, saved) = commands::parse_bind(args).ok()?;
    for step in steps {
        let (spec, args) = commands::parse(&step).ok()??;
        match spec.arguments {
            Arguments::Settings if args.split_whitespace().count() < 2 => return None,
            Arguments::Bind => {
                complete_binding_args(args, depth + 1)?;
            }
            _ => {}
        }
    }
    Some(saved)
}

fn chain_completion(
    text: &str,
    range: Range<usize>,
    (cursor, depth): (usize, usize),
    interface: &Interface,
    snapshot: &Snapshot,
    drafts: &[DraftRecord],
    keys: &KeyBindings,
) -> Option<Completion> {
    let Range { start, end } = range;
    if cursor < start || cursor > end || text[start..end].split(';').count() > 16 {
        return None;
    }
    let start = text[start..cursor]
        .rfind(';')
        .map_or(start, |offset| start + offset + 1);
    let end = text[cursor..end]
        .find(';')
        .map_or(end, |offset| cursor + offset);
    let segment = &text[start..end];
    let start = start + segment.len() - segment.trim_start().len();
    if cursor < start {
        return None;
    }
    let segment = &text[start..end];
    let prefix = usize::from(!segment.starts_with('/'));
    let source = if prefix == 1 {
        format!("/{segment}")
    } else {
        segment.to_owned()
    };
    let mut menu = build_inner(
        &source,
        cursor - start + prefix,
        interface,
        snapshot,
        drafts,
        keys,
        depth + 1,
    )?;
    menu.range.start = start + menu.range.start.saturating_sub(prefix);
    menu.range.end = start + menu.range.end.saturating_sub(prefix);
    Some(menu)
}

fn rank(mut completion: Completion, query: &str) -> Completion {
    // Stable ranking retains catalog/draft order among equally good matches.
    let query = query.to_ascii_lowercase();
    completion
        .items
        .retain(|item| item.value.to_ascii_lowercase().contains(&query));
    completion.items.sort_by_key(|item| {
        let value = item.value.to_ascii_lowercase();
        if value == query {
            0
        } else if value.starts_with(&query) {
            1
        } else {
            2
        }
    });
    completion
}

fn setting_hint(setting: &settings::Setting) -> String {
    format!(
        "{} Current: {}. Examples: {}",
        setting.description,
        setting.value,
        setting
            .examples
            .iter()
            .map(|example| format!("/settings {example}"))
            .collect::<Vec<_>>()
            .join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{Phase, QueueItem, QueueState};
    use std::time::Instant;

    fn snapshot() -> Snapshot {
        Snapshot {
            session_id: "session".into(),
            agent_id: "agent".into(),
            conversation_id: "conversation".into(),
            phase: Phase::Idle,
            phase_started: Instant::now(),
            last_event: Instant::now(),
            active: None,
            queue: Vec::new(),
            paused: false,
            recovered: false,
            cursor: 0,
            error: None,
        }
    }

    fn complete(text: &str, cursor: usize) -> Completion {
        build(text, cursor, &Interface::default(), &snapshot()).unwrap()
    }

    fn values(completion: &Completion) -> Vec<&str> {
        completion
            .items
            .iter()
            .map(|item| item.value.as_str())
            .collect()
    }

    fn macro_keys() -> KeyBindings {
        KeyBindings {
            macros: vec![
                commands::CommandBinding {
                    key: "f2".into(),
                    commands: vec!["/settings status-color false".into()],
                },
                commands::CommandBinding {
                    key: "alt+k".into(),
                    commands: vec!["/stop".into()],
                },
            ],
            ..KeyBindings::default()
        }
    }

    #[test]
    fn bind_completion_reuses_every_command_argument_schema_and_live_keymap() {
        let keys = macro_keys();
        let mut snapshot = snapshot();
        snapshot.queue.push(QueueItem {
            id: "aaaa1111".into(),
            text: "queued text".into(),
            revision: 1,
            state: QueueState::Queued,
        });
        let drafts = [DraftRecord {
            id: "bbbb2222".into(),
            text: "saved draft".into(),
            revision: 3,
        }];
        for spec in commands::COMMANDS {
            let text = format!("/bind f9 {} ", spec.name);
            let menu = build_with_keys(
                &text,
                text.len(),
                &Interface::default(),
                &snapshot,
                &drafts,
                &keys,
            )
            .unwrap_or_else(|| panic!("missing nested completion for {}", spec.name));
            let expected = match spec.arguments {
                Arguments::None => "--default",
                Arguments::Settings => "status",
                Arguments::History => "fullscreen",
                Arguments::Draft => "save",
                Arguments::SendDraft => "bbbb2222",
                Arguments::MessageId | Arguments::Mode | Arguments::Queue => {
                    panic!("obsolete argument schema in command catalog: {}", spec.name)
                }
                Arguments::Commands => "keep",
                Arguments::Bind | Arguments::Unbind => "f2",
            };
            assert!(
                values(&menu).contains(&expected),
                "{}: {:?}",
                spec.name,
                values(&menu)
            );
        }
        for command in ["/bind ", "/unbind "] {
            let menu = build_with_keys(
                command,
                command.len(),
                &Interface::default(),
                &snapshot,
                &drafts,
                &keys,
            )
            .unwrap();
            assert_eq!(values(&menu), ["f2", "alt+k"]);
            assert!(menu.items[0].description.contains("status-color false"));
        }
        let text = "/unbind ALT+";
        let menu = build_with_keys(
            text,
            text.len(),
            &Interface::default(),
            &snapshot,
            &drafts,
            &keys,
        )
        .unwrap();
        assert_eq!(values(&menu), ["alt+k"]);
    }

    #[test]
    fn nested_bind_ranges_preserve_unicode_caret_semicolons_and_following_flags() {
        let keys = macro_keys();
        let text = "/bind f9 /help;\u{2003}/settings\u{2003}status\tbr\u{e9} --default";
        let cursor = text.find('\u{e9}').unwrap();
        let menu =
            build_with_keys(text, cursor, &Interface::default(), &snapshot(), &[], &keys).unwrap();
        assert_eq!(&text[menu.range.clone()], "br\u{e9}");
        assert_eq!(values(&menu), ["braille"]);
        let mut editor = super::super::input::Editor::new(4096);
        editor.set_text(text);
        editor.set_cursor(cursor);
        editor.replace(menu.range, "braille").unwrap();
        assert_eq!(
            editor.text(),
            "/bind f9 /help;\u{2003}/settings\u{2003}status\tbraille --default"
        );
        assert!(
            build_with_keys(
                text,
                cursor + 1,
                &Interface::default(),
                &snapshot(),
                &[],
                &keys
            )
            .is_none()
        );
        let text = "/bind f9 /settings status braille --d ; /stop";
        let cursor = text.find("--d").unwrap() + 3;
        let menu =
            build_with_keys(text, cursor, &Interface::default(), &snapshot(), &[], &keys).unwrap();
        assert_eq!(&text[menu.range.clone()], "--d");
        assert_eq!(values(&menu), ["--default"]);
        assert_eq!(menu.items[0].description, "Make default");
        let text = "/bind f9 /help;/quBAD";
        let cursor = text.find("BAD").unwrap();
        let menu =
            build_with_keys(text, cursor, &Interface::default(), &snapshot(), &[], &keys).unwrap();
        assert_eq!(&text[menu.range.clone()], "/quBAD");
        assert_eq!(values(&menu), ["/quit"]);
    }

    #[test]
    fn binding_default_flags_require_a_complete_chain_and_are_never_duplicated() {
        let keys = macro_keys();
        for text in ["/bind f9 /help ; /stop --d", "/unbind f2 --d"] {
            let menu = build_with_keys(
                text,
                text.len(),
                &Interface::default(),
                &snapshot(),
                &[],
                &keys,
            )
            .unwrap();
            assert_eq!(values(&menu), ["--default"]);
            assert_eq!(menu.items[0].description, "Save binding defaults");
        }
        for text in [
            "/bind f9 /settings ",
            "/bind f9 /settings status ",
            "/bind f9 /settings status bogus ",
            "/bind f9 /help ; ",
            "/bind ctrl+c /help ",
            "/bind f9 /bind f8 /settings status ",
        ] {
            let menu = build_with_keys(
                text,
                text.len(),
                &Interface::default(),
                &snapshot(),
                &[],
                &keys,
            );
            assert!(
                menu.is_none_or(|menu| !values(&menu).contains(&"--default")),
                "{text}"
            );
        }
        for text in ["/bind f9 /help --default ", "/unbind f2 --default "] {
            assert!(
                build_with_keys(
                    text,
                    text.len(),
                    &Interface::default(),
                    &snapshot(),
                    &[],
                    &keys
                )
                .is_none()
            );
        }
        for text in ["/bind f9 ", "/bind f9 /help ; "] {
            let menu = build_with_keys(
                text,
                text.len(),
                &Interface::default(),
                &snapshot(),
                &[],
                &keys,
            )
            .unwrap();
            assert_eq!(menu.range, text.len()..text.len());
            assert_eq!(menu.stage, Stage::Command);
            assert!(values(&menu).contains(&"/settings"));
        }
        let text = "/bind f9 /help ; /stop ";
        let menu = build_with_keys(
            text,
            text.len(),
            &Interface::default(),
            &snapshot(),
            &[],
            &keys,
        )
        .unwrap();
        assert_eq!(values(&menu), ["--default"]);
        let text = "/bind f9 /settings status braille    --default";
        let cursor = text.find("--default").unwrap();
        let menu =
            build_with_keys(text, cursor, &Interface::default(), &snapshot(), &[], &keys).unwrap();
        assert_eq!(menu.range, cursor..text.len());
        assert_eq!(values(&menu), ["--default"]);
        let menu = build_with_keys(
            text,
            cursor - 1,
            &Interface::default(),
            &snapshot(),
            &[],
            &keys,
        );
        assert!(menu.is_none_or(|menu| !values(&menu).contains(&"--default")));
    }

    #[test]
    fn binding_completion_bounds_recursive_nesting_and_input_bytes() {
        for text in [
            format!("{} /settings ", "/bind f9 ".repeat(32)),
            format!("/bind f9 {}", "x".repeat(4096)),
        ] {
            assert!(
                build_with_keys(
                    &text,
                    text.len(),
                    &Interface::default(),
                    &snapshot(),
                    &[],
                    &macro_keys()
                )
                .is_none()
            );
        }
    }

    #[test]
    fn command_completion_uses_schema_and_stable_ranking() {
        let completion = complete("/draft", 6);
        assert_eq!(completion.stage, Stage::Command);
        assert_eq!(values(&completion), ["/draft", "/drafts", "/clear-draft"]);
        assert!(completion.items[0].trailing_space);
        assert!(completion.items[1].trailing_space);
        for command in ["/history", "/settings", "/stop"] {
            assert!(complete(command, command.len()).items[0].trailing_space);
        }
        assert_eq!(values(&complete("/q", 2)), ["/quit"]);
        assert_eq!(values(&complete("/istory", 7)), ["/history"]);
        let text = "/hisBAD fullscreen";
        let completion = complete(text, 4);
        assert_eq!(completion.range, 0..7);
        assert_eq!(values(&completion), ["/history"]);
        assert_eq!(completion.ghost(text, 4, 0), None);
    }

    #[test]
    fn settings_keys_and_values_come_only_from_the_catalog() {
        let completion = complete("/settings ", 10);
        assert_eq!(completion.stage, Stage::Setting);
        assert_eq!(
            values(&completion),
            ["status-line", "prompt-history", "status", "status-color"]
        );
        assert!(completion.items.iter().all(|item| item.trailing_space));
        assert_eq!(
            values(&complete("/settings status", 16)),
            ["status", "status-line", "status-color"]
        );
        assert_eq!(values(&complete("/settings line", 14)), ["status-line"]);
        let completion = complete("/settings status ", 17);
        assert_eq!(completion.stage, Stage::Value);
        assert_eq!(values(&completion), ["text", "braille", "minimal", "bars"]);
        assert!(completion.items.iter().all(|item| item.trailing_space));
        assert!(completion.hint.contains("Current: bars"));
        assert!(completion.hint.contains("/settings status braille"));
        assert_eq!(values(&complete("/settings status rai", 20)), ["braille"]);
        assert_eq!(values(&complete("/settings status ba", 19)), ["bars"]);
        assert_eq!(
            values(&complete("/settings status-color ", 23)),
            ["true", "false"]
        );
    }

    #[test]
    fn hints_report_the_actual_client_value_and_valid_examples() {
        let interface = Interface {
            status_color: true,
            ..Interface::default()
        };
        let text = "/settings status-color ";
        let completion = build(text, text.len(), &interface, &snapshot()).unwrap();
        assert!(completion.hint.contains("Current: true"));
        assert!(completion.hint.contains("/settings status-color false"));
        assert_eq!(completion.items[0].description, "Current value");
        assert!(build("/settings model ", 16, &interface, &snapshot()).is_none());
        assert!(build("/settings provider ", 19, &interface, &snapshot()).is_none());
    }

    #[test]
    fn default_option_requires_a_complete_valid_change_and_whitespace() {
        for text in [
            "/settings status text ",
            "/settings status-color true ",
            "/settings status text  ",
        ] {
            let completion = complete(text, text.len());
            assert_eq!(completion.stage, Stage::Option);
            assert_eq!(completion.range, text.len()..text.len());
            assert_eq!(completion.items.len(), 1);
            assert_eq!(completion.items[0].label, "--default");
            assert_eq!(completion.items[0].description, "Make default");
            assert_eq!(completion.ghost(text, text.len(), 0), Some("--default"));
        }
        let text = "/settings status text";
        assert_eq!(complete(text, text.len()).stage, Stage::Value);
        assert_eq!(values(&complete(text, text.len())), ["text"]);
        assert_eq!(complete(text, 16).stage, Stage::Setting);
        assert_eq!(complete(text, 19).stage, Stage::Value);
        for text in ["/settings status TEXT", "/settings status bogus"] {
            let completion = complete(text, text.len());
            assert_eq!(completion.stage, Stage::Value);
            assert!(
                completion
                    .items
                    .iter()
                    .all(|item| item.value != "--default")
            );
        }
        assert!(
            build(
                "/settings status text extra",
                27,
                &Interface::default(),
                &snapshot()
            )
            .is_some_and(|menu| menu.items.is_empty())
        );
    }

    #[test]
    fn ghost_only_extends_the_whole_token_without_rewriting() {
        for (text, suffix) in [
            ("/his", "tory"),
            ("/settings status br", "aille"),
            ("/history full", "screen"),
        ] {
            let completion = complete(text, text.len());
            assert_eq!(completion.ghost(text, text.len(), 0), Some(suffix));
            assert_eq!(completion.ghost(text, text.len(), usize::MAX), None);
        }
        for text in [
            "/istory",
            "/HIS",
            "/settings line",
            "/settings status rai",
            "/history fullscreen",
        ] {
            let completion = complete(text, text.len());
            assert_eq!(completion.ghost(text, text.len(), 0), None, "{text}");
        }
        let text = "/settings status brBAD";
        let completion = complete(text, 19);
        assert_eq!(completion.range, 17..22);
        assert_eq!(values(&completion), ["braille"]);
        assert_eq!(completion.ghost(text, 19, 0), None);
        let text = "/his fullscreen";
        let completion = complete(text, 4);
        assert_eq!(completion.ghost(text, 4, 0), Some("tory"));
    }

    #[test]
    fn cursor_ranges_preserve_utf8_and_whitespace() {
        let text = "/settings\u{2003}status\tbr\u{00e9}";
        let completion = complete(text, text.len() - 2);
        assert_eq!(&text[completion.range.clone()], "br\u{00e9}");
        assert_eq!(completion.stage, Stage::Value);
        assert_eq!(values(&completion), ["braille"]);
        assert!(build(text, text.len() - 1, &Interface::default(), &snapshot()).is_none());
        assert!(build(text, text.len() + 1, &Interface::default(), &snapshot()).is_none());
        let text = "/settings   status text";
        let completion = complete(text, 11);
        assert_eq!(completion.stage, Stage::Setting);
        assert_eq!(completion.range, 11..11);
        assert_eq!(complete(text, 18).stage, Stage::Setting);
        let text = "/settings status-color true";
        assert_eq!(complete(text, 18).range, 10..22);
        assert_eq!(complete(text, 18).stage, Stage::Setting);
    }

    #[test]
    fn removed_commands_have_no_catalog_or_argument_completions() {
        let mut snapshot = snapshot();
        snapshot.queue.push(QueueItem {
            id: "queued-1111".into(),
            text: "queued text".into(),
            revision: 4,
            state: QueueState::Queued,
        });
        for command in [
            "/queue",
            "/pause",
            "/resume",
            "/toggle-pause",
            "/interrupt",
            "/mode",
            "/force",
            "/edit",
            "/cancel",
            "/cancel-edit",
            "/run",
        ] {
            assert!(
                complete(command, command.len()).items.is_empty(),
                "{command}"
            );
            let text = format!("{command} ");
            assert!(build(&text, text.len(), &Interface::default(), &snapshot).is_none());
            let text = format!("/bind f9 {command} ");
            assert!(build(&text, text.len(), &Interface::default(), &snapshot).is_none());
        }
    }

    #[test]
    fn send_completes_only_saved_draft_ids_and_never_flags() {
        let mut snapshot = snapshot();
        snapshot.queue.push(QueueItem {
            id: "queued-1111".into(),
            text: "queued text".into(),
            revision: 4,
            state: QueueState::Queued,
        });
        let drafts = ["aaaa-1111", "baaa-2222", "aaaa-3333"].map(|id| DraftRecord {
            id: id.into(),
            text: "saved text".into(),
            revision: 1,
        });
        for command in ["/send", "/bind f9 /send"] {
            let text = format!("{command} aaa");
            let menu =
                build_with_drafts(&text, text.len(), &Interface::default(), &snapshot, &drafts)
                    .unwrap();
            assert_eq!(menu.stage, Stage::MessageId);
            assert_eq!(values(&menu), ["aaaa-1111", "aaaa-3333", "baaa-2222"]);
            assert!(menu.items.iter().all(|item| !item.trailing_space));
            assert_eq!(menu.hint, "Usage: /send <draft-id>");
            for suffix in [
                "aaaa-1111 ",
                "aaaa-1111 --",
                "aaaa-1111 --force",
                "aaaa-1111 --after",
                "aaaa-1111 --run",
                "aaaa-1111 --hold",
            ] {
                let text = format!("{command} {suffix}");
                let menu =
                    build_with_drafts(&text, text.len(), &Interface::default(), &snapshot, &drafts);
                if command == "/send" {
                    assert!(menu.is_none(), "{text}");
                } else {
                    // Only the binding's save option can follow a complete nested /send.
                    assert!(
                        menu.as_ref().is_none_or(|menu| {
                            values(menu).iter().all(|value| *value == "--default")
                        }),
                        "{text}"
                    );
                    if suffix == "aaaa-1111 " || suffix == "aaaa-1111 --" {
                        assert_eq!(values(&menu.unwrap()), ["--default"]);
                    }
                }
            }
        }
    }

    #[test]
    fn history_has_only_the_optional_fullscreen_value() {
        let completion = complete("/history ", 9);
        assert_eq!(completion.stage, Stage::History);
        assert_eq!(values(&completion), ["fullscreen"]);
        assert!(completion.items[0].trailing_space);
        assert!(completion.hint.contains("fullscreen"));
        assert!(completion.hint.contains("action journal"));
        assert_eq!(values(&complete("/history screen", 15)), ["fullscreen"]);
        assert!(
            build(
                "/history fullscreen extra",
                25,
                &Interface::default(),
                &snapshot()
            )
            .is_none()
        );
    }

    #[test]
    fn history_attaches_bounded_query_identified_matches_and_replaces_with_seq() {
        let text = "/history \u{00e4}pfel old";
        let menu = complete(text, text.len());
        assert_eq!(&text[menu.range.clone()], "\u{00e4}pfel old");
        assert_eq!(
            history_query(&menu, text, text.len()).as_deref(),
            Some("\u{00e4}pfel old")
        );
        let entries = [crate::runtime::HistoryMatch {
            seq: 1,
            kind: "user".into(),
            preview: "Old \u{00c4}PFEL old".into(),
        }];
        let attached = attach_history(menu.clone(), text, text.len(), "\u{00e4}pfel old", &entries);
        assert_eq!(values(&attached), ["1"]);
        assert_eq!(attached.items[0].label, "1 user");
        assert_eq!(attached.items[0].description, "Old \u{00c4}PFEL old");
        assert!(!attached.items[0].trailing_space);
        let mut editor = super::super::input::Editor::new(4096);
        editor.set_text(text);
        editor
            .replace(attached.range, &attached.items[0].value)
            .unwrap();
        assert_eq!(editor.text(), "/history 1");
        assert!(
            attach_history(menu, text, text.len(), "stale", &entries)
                .items
                .is_empty()
        );
        let invalid = [
            crate::runtime::HistoryMatch {
                seq: -1,
                kind: "user".into(),
                preview: "bad".into(),
            },
            crate::runtime::HistoryMatch {
                seq: 2,
                kind: "notice".into(),
                preview: "bad".into(),
            },
            entries[0].clone(),
            entries[0].clone(),
        ];
        assert_eq!(
            attach_history(
                complete(text, text.len()),
                text,
                text.len(),
                "\u{00e4}pfel old",
                &invalid
            )
            .items
            .len(),
            1
        );
        let entries = (1..=100)
            .map(|seq| crate::runtime::HistoryMatch {
                seq,
                kind: "assistant".into(),
                preview: "text".into(),
            })
            .collect::<Vec<_>>();
        let attached = attach_history(complete("/history ", 9), "/history ", 9, "", &entries);
        assert_eq!(
            attached.items.len(),
            crate::runtime::MAX_HISTORY_MATCHES + 1
        );
        assert!(values(&attached).contains(&"fullscreen"));
    }

    #[test]
    fn literal_multiline_and_non_argument_inputs_do_not_complete() {
        for text in [
            "",
            "hello",
            " /help",
            "//history",
            "/help\ntext",
            "/history\r",
            "/help extra",
            "/hist full",
            "/edit id extra",
        ] {
            assert!(
                build(text, text.len(), &Interface::default(), &snapshot()).is_none(),
                "{text}"
            );
        }
    }
}
