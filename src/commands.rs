use anyhow::{Result, bail, ensure};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};

use crate::{config::Interface, settings};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Commands,
    Help,
    History,
    Queue,
    Details,
    Settings,
    Pause,
    Resume,
    TogglePause,
    Interrupt,
    Stop,
    Quit,
    ClearDraft,
    CancelEdit,
    Edit,
    Cancel,
    Draft,
    Drafts,
    Send,
    Mode,
    Exit,
    Bindings,
    Bind,
    Unbind,
    ForceSend,
    FocusInput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arguments {
    None,
    MessageId,
    Settings,
    History,
    Draft,
    SendDraft,
    Mode,
    Queue,
    Commands,
    Bind,
    Unbind,
}

pub struct Spec {
    pub name: &'static str,
    pub description: &'static str,
    pub action: Action,
    pub arguments: Arguments,
}

impl Spec {
    pub fn category(&self) -> &'static str {
        match self.action {
            Action::Queue
            | Action::Pause
            | Action::Resume
            | Action::TogglePause
            | Action::Interrupt
            | Action::Stop
            | Action::Edit
            | Action::Cancel => "Session",
            Action::Draft
            | Action::Drafts
            | Action::Send
            | Action::ClearDraft
            | Action::CancelEdit
            | Action::ForceSend
            | Action::Mode => "Input",
            Action::Settings | Action::Bindings | Action::Bind | Action::Unbind => "Preferences",
            Action::Help
            | Action::History
            | Action::Details
            | Action::Commands
            | Action::Exit
            | Action::Quit
            | Action::FocusInput => "AM",
        }
    }

    pub fn opens_panel(&self) -> bool {
        matches!(
            self.action,
            Action::Settings
                | Action::Queue
                | Action::Bindings
                | Action::Drafts
                | Action::Details
                | Action::History
                | Action::Commands
                | Action::Help
        )
    }

    pub fn takes_arguments(&self) -> bool {
        self.arguments != Arguments::None
    }

    pub fn usage(&self) -> String {
        let arguments = match self.arguments {
            Arguments::None => "",
            Arguments::MessageId => " <message-id-prefix>",
            Arguments::Settings => " [<key> <value> [--default]]",
            Arguments::History => " [fullscreen|<seq>]",
            Arguments::Draft => " save | delete <id>",
            Arguments::SendDraft => " <draft-id>",
            Arguments::Mode => " steer|after|draft",
            Arguments::Queue => " [run|pause]",
            Arguments::Commands => " [keep]",
            Arguments::Bind => " <chord> <slash-command> [ ; <slash-command> ...] [--default]",
            Arguments::Unbind => " <chord> [--default]",
        };
        format!("{}{arguments}", self.name)
    }
}

pub const COMMANDS: &[Spec] = &[
    Spec {
        name: "/bind",
        description: "Bind a shortcut to one or more slash commands",
        action: Action::Bind,
        arguments: Arguments::Bind,
    },
    Spec {
        name: "/bindings",
        description: "Open the visual shortcut editor",
        action: Action::Bindings,
        arguments: Arguments::None,
    },
    Spec {
        name: "/help",
        description: "Browse commands by category and effective key bindings",
        action: Action::Help,
        arguments: Arguments::None,
    },
    Spec {
        name: "/clear-draft",
        description: "Clear the unsent draft",
        action: Action::ClearDraft,
        arguments: Arguments::None,
    },
    Spec {
        name: "/commands",
        description: "Open a secondary command field; keep makes it persistent",
        action: Action::Commands,
        arguments: Arguments::Commands,
    },
    Spec {
        name: "/details",
        description: "Show runtime IDs and response checkpoints",
        action: Action::Details,
        arguments: Arguments::None,
    },
    Spec {
        name: "/draft",
        description: "Save current text as a draft with a generated ID, or delete a saved draft",
        action: Action::Draft,
        arguments: Arguments::Draft,
    },
    Spec {
        name: "/drafts",
        description: "Show saved drafts and their IDs",
        action: Action::Drafts,
        arguments: Arguments::None,
    },
    Spec {
        name: "/exit",
        description: "Close the secondary command field without quitting",
        action: Action::Exit,
        arguments: Arguments::None,
    },
    Spec {
        name: "/history",
        description: "Open the fullscreen conversation journal",
        action: Action::History,
        arguments: Arguments::History,
    },
    Spec {
        name: "/quit",
        description: "Save the session and exit",
        action: Action::Quit,
        arguments: Arguments::None,
    },
    Spec {
        name: "/send",
        description: "Submit a saved draft by ID",
        action: Action::Send,
        arguments: Arguments::SendDraft,
    },
    Spec {
        name: "/settings",
        description: "Open UI preferences; arguments change this client, --default saves for future clients",
        action: Action::Settings,
        arguments: Arguments::Settings,
    },
    Spec {
        name: "/stop",
        description: "Stop active work",
        action: Action::Stop,
        arguments: Arguments::None,
    },
    Spec {
        name: "/unbind",
        description: "Remove a shortcut; --default saves for future clients",
        action: Action::Unbind,
        arguments: Arguments::Unbind,
    },
];

pub fn matches(query: &str) -> Vec<&'static Spec> {
    let token = query.split_whitespace().next().unwrap_or("");
    if token.len()
        > COMMANDS
            .iter()
            .map(|spec| spec.name.len())
            .max()
            .unwrap_or(0)
    {
        return Vec::new();
    }
    let token = token
        .strip_prefix('/')
        .unwrap_or(token)
        .to_ascii_lowercase();
    let mut matches = COMMANDS
        .iter()
        .filter(|spec| spec.name[1..].contains(&token))
        .collect::<Vec<_>>();
    // Match fx's discoverable exact/prefix/substring ordering, without fuzziness.
    matches.sort_by_key(|spec| {
        if spec.name[1..] == token {
            0
        } else if spec.name[1..].starts_with(&token) {
            1
        } else {
            2
        }
    });
    matches
}

pub fn parse(text: &str) -> Result<Option<(&'static Spec, &str)>> {
    if !text.starts_with('/') || text.starts_with("//") || text.contains(['\n', '\r']) {
        return Ok(None);
    }
    ensure!(text.len() <= 4096, "command text exceeds 4096 bytes");
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    let token = &text[..end];
    let Some(spec) = COMMANDS
        .iter()
        .find(|spec| spec.name.eq_ignore_ascii_case(token))
    else {
        bail!("Unknown command {token:.32}; /help lists commands. Use // for literal slash text");
    };
    let args = text[end..].trim();
    let tokens = args.split_whitespace().collect::<Vec<_>>();
    match spec.arguments {
        Arguments::None => ensure!(args.is_empty(), "{} takes no arguments", spec.name),
        Arguments::MessageId => ensure!(
            tokens.len() == 1 && valid_id(tokens[0]),
            "Usage: {}; IDs are shown in /queue",
            spec.usage()
        ),
        Arguments::History => ensure!(
            tokens.is_empty()
                || tokens == ["fullscreen"]
                || (tokens.len() == 1
                    && tokens[0].bytes().all(|ch| ch.is_ascii_digit())
                    && tokens[0].parse::<i64>().is_ok_and(|seq| seq > 0)),
            "Usage: {}; the journal is always fullscreen",
            spec.usage()
        ),
        Arguments::Commands => ensure!(
            tokens.is_empty() || tokens == ["keep"],
            "Usage: {}",
            spec.usage()
        ),
        Arguments::Queue => ensure!(
            tokens.is_empty() || tokens == ["run"] || tokens == ["pause"],
            "Usage: {}",
            spec.usage()
        ),
        Arguments::Mode => ensure!(
            matches!(tokens.as_slice(), ["steer" | "after" | "draft"]),
            "Usage: {}",
            spec.usage()
        ),
        Arguments::Draft => ensure!(
            tokens == ["save"] || matches!(tokens.as_slice(), ["delete", id] if valid_id(id)),
            "Usage: {}",
            spec.usage()
        ),
        Arguments::SendDraft => ensure!(
            tokens.len() == 1 && valid_id(tokens[0]),
            "Usage: {}",
            spec.usage()
        ),
        Arguments::Bind => {
            parse_bind(args)?;
        }
        Arguments::Unbind => {
            ensure!(
                matches!(tokens.as_slice(), [_] | [_, "--default"]),
                "Usage: {}",
                spec.usage()
            );
            let chord = Chord::parse(tokens[0])?;
            ensure!(!chord.is_exit(), "Ctrl+C is reserved for confirmed exit");
        }
        Arguments::Settings => match tokens.as_slice() {
            [] => {}
            [key] => ensure!(
                settings::catalog(&Interface::default())
                    .iter()
                    .any(|setting| setting.key == *key),
                "Unknown setting {key}; {} lists UI preferences",
                spec.name
            ),
            [_, _] => {
                settings::Change::parse(&tokens)?;
            }
            [key, value, "--default"] => {
                settings::Change::parse(&[key, value])?;
            }
            _ => bail!("Usage: {}", spec.usage()),
        },
    }
    Ok(Some((spec, args)))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-' || ch == b'_')
}

/// Parse the arguments returned for /bind. Only a final --default saves the binding.
pub fn parse_bind(args: &str) -> Result<(String, Vec<String>, bool)> {
    ensure!(
        args.len() <= 4096 && !args.contains(['\n', '\r']),
        "binding must be a single line of at most 4096 bytes"
    );
    let end = args.find(char::is_whitespace).unwrap_or(args.len());
    let key = &args[..end];
    let chord = Chord::parse(key)?;
    ensure!(!chord.is_exit(), "Ctrl+C is reserved for confirmed exit");
    let mut chain = args[end..].trim();
    let default = chain.split_whitespace().last() == Some("--default");
    if default {
        chain = chain.strip_suffix("--default").unwrap().trim_end();
    }
    let commands = chain
        .split(';')
        .map(|step| step.trim().to_owned())
        .collect::<Vec<_>>();
    validate_commands(&commands)?;
    Ok((key.to_owned(), commands, default))
}

fn validate_commands(commands: &[String]) -> Result<()> {
    ensure!(
        (1..=16).contains(&commands.len()),
        "a binding needs 1..=16 command steps"
    );
    for command in commands {
        ensure!(command.len() <= 4096, "command text exceeds 4096 bytes");
        ensure!(
            !command.contains(['\n', '\r']),
            "binding steps must be single-line slash commands"
        );
        ensure!(
            parse(command)?.is_some(),
            "binding steps must be slash commands"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandBinding {
    pub key: String,
    pub commands: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeyBindings {
    pub commands: Vec<String>,
    pub focus_input: Vec<String>,
    pub help: Vec<String>,
    pub history: Vec<String>,
    // Obsolete action lists are retained only for shipped configuration data.
    pub queue: Vec<String>,
    pub details: Vec<String>,
    pub settings: Vec<String>,
    pub pause: Vec<String>,
    pub resume: Vec<String>,
    pub toggle_pause: Vec<String>,
    pub interrupt: Vec<String>,
    pub stop: Vec<String>,
    pub quit: Vec<String>,
    pub clear_draft: Vec<String>,
    pub cancel_edit: Vec<String>,
    pub edit: Vec<String>,
    pub cancel: Vec<String>,
    pub macros: Vec<CommandBinding>,
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self {
            commands: vec!["ctrl+p".into()],
            focus_input: vec!["alt+i".into()],
            help: vec![],
            history: vec!["ctrl+o".into()],
            queue: vec![],
            details: vec!["alt+d".into()],
            settings: vec![],
            pause: vec![],
            resume: vec![],
            toggle_pause: vec![],
            interrupt: vec![],
            stop: vec![],
            quit: vec!["ctrl+c".into()],
            clear_draft: vec!["ctrl+u".into()],
            cancel_edit: vec![],
            edit: vec![],
            cancel: vec![],
            macros: vec![],
        }
    }
}

impl KeyBindings {
    pub fn get(&self, action: Action) -> &[String] {
        match action {
            Action::Commands => &self.commands,
            Action::FocusInput => &self.focus_input,
            Action::Help => &self.help,
            Action::History => &self.history,
            Action::Details => &self.details,
            Action::Settings => &self.settings,
            Action::Stop => &self.stop,
            Action::Quit => &self.quit,
            Action::ClearDraft => &self.clear_draft,
            Action::Queue
            | Action::Pause
            | Action::Resume
            | Action::TogglePause
            | Action::Interrupt
            | Action::CancelEdit
            | Action::Edit
            | Action::Cancel
            | Action::Draft
            | Action::Drafts
            | Action::Send
            | Action::Mode
            | Action::Exit
            | Action::Bindings
            | Action::Bind
            | Action::Unbind
            | Action::ForceSend => &[],
        }
    }

    pub fn label(&self, action: Action) -> String {
        let mut keys = self.get(action).to_vec();
        if action == Action::Quit && !keys.iter().any(|key| key.eq_ignore_ascii_case("ctrl+c")) {
            keys.push("ctrl+c".into());
        }
        keys.into_iter()
            .map(|key| {
                if key.eq_ignore_ascii_case("ctrl+c") {
                    "ctrl+c twice".into()
                } else {
                    key
                }
            })
            .collect::<Vec<_>>()
            .join(" / ")
    }

    pub fn compile(&self) -> Result<Vec<(Chord, Action)>> {
        let mut map = Vec::new();
        // Only public commands and input focus are active keyboard actions.
        for action in COMMANDS
            .iter()
            .map(|spec| spec.action)
            .chain([Action::FocusInput])
        {
            let keys = self.get(action);
            ensure!(keys.len() <= 8, "at most 8 shortcuts per action");
            for key in keys {
                let chord = Chord::parse(key)?;
                ensure!(
                    chord
                        != Chord {
                            code: KeyCode::Char('c'),
                            modifiers: KeyModifiers::CONTROL
                        }
                        || action == Action::Quit,
                    "Ctrl+C is reserved for confirmed exit"
                );
                ensure!(
                    !map.iter().any(|(existing, _)| existing == &chord),
                    "duplicate/conflicting shortcut {key}"
                );
                map.push((chord, action));
            }
        }
        Ok(map)
    }

    pub fn compile_macros(&self) -> Result<Vec<(Chord, Vec<String>)>> {
        ensure!(self.macros.len() <= 32, "at most 32 command bindings");
        let builtins = self.compile()?;
        let mut map = Vec::new();
        for binding in &self.macros {
            let chord = Chord::parse(&binding.key)?;
            ensure!(!chord.is_exit(), "Ctrl+C is reserved for confirmed exit");
            ensure!(
                !builtins.iter().any(|(key, _)| *key == chord)
                    && !map.iter().any(|(key, _)| *key == chord),
                "duplicate/conflicting shortcut {}",
                binding.key
            );
            validate_commands(&binding.commands)?;
            map.push((chord, binding.commands.clone()));
        }
        Ok(map)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl Chord {
    pub fn parse(text: &str) -> Result<Self> {
        ensure!(text.len() <= 32, "shortcut is too long");
        let text = text.to_ascii_lowercase();
        let mut parts = text.split('+').collect::<Vec<_>>();
        let key = parts.pop().unwrap_or("");
        let mut modifiers = KeyModifiers::empty();
        for modifier in parts {
            let flag = match modifier {
                "ctrl" => KeyModifiers::CONTROL,
                "alt" => KeyModifiers::ALT,
                _ => bail!("unsupported modifier in {text}; use ctrl/alt"),
            };
            ensure!(!modifiers.contains(flag), "duplicate modifier in {text}");
            modifiers |= flag;
        }
        let code = if let Some(number) = key
            .strip_prefix('f')
            .and_then(|number| number.parse::<u8>().ok())
        {
            ensure!((1..=24).contains(&number), "function key must be F1..F24");
            KeyCode::F(number)
        } else if key.len() == 1
            && key.as_bytes()[0].is_ascii_alphanumeric()
            && !modifiers.is_empty()
        {
            KeyCode::Char(key.chars().next().unwrap())
        } else {
            bail!(
                "unsupported shortcut {text}; use Ctrl/Alt + ASCII letter/digit, or F1..F24. Input/navigation keys are reserved"
            );
        };
        ensure!(
            !(modifiers.contains(KeyModifiers::CONTROL)
                && matches!(code, KeyCode::Char('h' | 'i' | 'j' | 'm'))),
            "Ctrl+H/I/J/M are indistinguishable from Backspace/Tab/Enter in classic terminals; use another shortcut"
        );
        ensure!(
            !(modifiers.contains(KeyModifiers::CONTROL)
                && matches!(code,KeyCode::Char(ch) if ch.is_ascii_digit())),
            "Ctrl+digits are not portable in classic terminals; use Alt+digit instead"
        );
        Ok(Self { code, modifiers })
    }

    fn is_exit(self) -> bool {
        self.code == KeyCode::Char('c') && self.modifiers == KeyModifiers::CONTROL
    }

    /// Produce a portable, parseable shortcut label from an editor key event.
    pub fn from_event(key: KeyEvent) -> Result<(String, Self)> {
        let modifiers = if matches!(key.code, KeyCode::Char(_)) {
            key.modifiers - KeyModifiers::SHIFT
        } else {
            key.modifiers
        };
        ensure!(
            (modifiers - (KeyModifiers::CONTROL | KeyModifiers::ALT)).is_empty(),
            "unsupported shortcut modifiers; use ctrl/alt"
        );
        let mut text = String::new();
        if modifiers.contains(KeyModifiers::CONTROL) {
            text.push_str("ctrl+");
        }
        if modifiers.contains(KeyModifiers::ALT) {
            text.push_str("alt+");
        }
        match key.code {
            KeyCode::Char(ch) => text.push(ch.to_ascii_lowercase()),
            KeyCode::F(number) => text.push_str(&format!("f{number}")),
            _ => bail!("input/navigation keys are reserved"),
        }
        let chord = Self::parse(&text)?;
        Ok((text, chord))
    }

    pub fn matches(self, key: KeyEvent) -> bool {
        let (code, modifiers) = match key.code {
            KeyCode::Char(ch) => (
                KeyCode::Char(ch.to_ascii_lowercase()),
                key.modifiers - KeyModifiers::SHIFT,
            ),
            code => (code, key.modifiers),
        };
        self.code == code && self.modifiers == modifiers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_ranks_exact_prefix_and_substring_stably() {
        assert_eq!(
            matches("/q")
                .iter()
                .map(|spec| spec.name)
                .collect::<Vec<_>>(),
            ["/quit"]
        );
        assert!(matches("hist").iter().any(|spec| spec.name == "/history"));
        assert_eq!(matches("istory")[0].name, "/history");
        assert_eq!(matches("/stop")[0].name, "/stop");
    }

    #[test]
    fn only_exact_single_line_commands_dispatch_and_unknown_slashes_do_not_become_chat() {
        assert!(parse("/q").is_err());
        assert!(parse("/quit extra").is_err());
        assert!(parse("/unknown extra").is_err());
        assert_eq!(parse("/QUIT").unwrap().unwrap().0.action, Action::Quit);
        for text in [
            "explain /help",
            " /help",
            "//path/file",
            "/help\nordinary text",
            "/help\r",
        ] {
            assert!(parse(text).unwrap().is_none());
        }
        assert!(parse("/edit").is_err());
        assert!(parse("/cancel").is_err());
        assert!(parse("/edit abcdef12 extra").is_err());
        assert!(parse("/cancel abcdef12 extra").is_err());
        assert!(parse("/edit abcdef12").is_err());
    }

    #[test]
    fn schemas_describe_arguments_and_share_the_binding_catalog() {
        for spec in COMMANDS {
            assert_eq!(spec.takes_arguments(), spec.arguments != Arguments::None);
            assert!(spec.usage().starts_with(spec.name));
            assert_eq!(
                COMMANDS
                    .iter()
                    .filter(|other| other.action == spec.action)
                    .count(),
                1
            );
        }
        assert_eq!(
            parse("/send id").unwrap().unwrap().0.arguments,
            Arguments::SendDraft
        );
        assert_eq!(
            parse("/send id").unwrap().unwrap().0.usage(),
            "/send <draft-id>"
        );
        assert_eq!(parse("/stop").unwrap().unwrap().0.category(), "Session");
        for command in ["/send id", "/drafts", "/draft save"] {
            assert_eq!(parse(command).unwrap().unwrap().0.category(), "Input");
        }
        assert_eq!(parse("/help").unwrap().unwrap().0.usage(), "/help");
        assert_eq!(
            parse("/history").unwrap().unwrap().0.usage(),
            "/history [fullscreen|<seq>]"
        );
        KeyBindings::default().compile().unwrap();
    }

    #[test]
    fn history_accepts_fullscreen_or_a_positive_saved_sequence() {
        for text in ["/history", "/HISTORY ", "/history fullscreen"] {
            let (spec, args) = parse(text).unwrap().unwrap();
            assert_eq!(spec.arguments, Arguments::History);
            assert_eq!(args == "fullscreen", text.ends_with("fullscreen"));
        }
        for text in [
            "/history full",
            "/history FULLSCREEN",
            "/history inline",
            "/history fullscreen extra",
            "/history 0",
            "/history -1",
            "/history +1",
            "/history 9223372036854775808",
            "/history 1 extra",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
        for text in [
            "/history 1",
            "/history 0042",
            "/history 9223372036854775807",
        ] {
            assert!(parse(text).is_ok(), "{text}");
        }
    }

    #[test]
    fn settings_accepts_guidance_or_an_exact_catalog_change() {
        assert_eq!(
            parse("/settings").unwrap().unwrap().0.arguments,
            Arguments::Settings
        );
        for setting in settings::catalog(&Interface::default()) {
            assert!(parse(&format!("/settings {}", setting.key)).is_ok());
            for value in setting.choices {
                assert!(parse(&format!("/settings {} {value}", setting.key)).is_ok());
                assert!(parse(&format!("/settings {} {value} --default", setting.key)).is_ok());
            }
        }
        for text in [
            "/settings model",
            "/settings provider fake",
            "/settings status_preset",
            "/settings status TEXT",
            "/settings status-color yes",
            "/settings status text extra",
            "/settings status text --save",
            "/settings status text --default --default",
            "/settings status text --default extra",
            "/settings status --default",
            "/settings --default status text",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn bindings_allow_multiple_keys_and_disabling_and_reject_collisions() {
        let mut keys = KeyBindings {
            history: vec!["alt+h".into(), "f2".into()],
            ..KeyBindings::default()
        };
        keys.commands.clear();
        let map = keys.compile().unwrap();
        assert!(map.iter().any(|(chord, action)| *action == Action::History
            && chord.matches(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT))));
        assert!(!map.iter().any(|(_, action)| *action == Action::Commands));
        keys.stop = vec!["ctrl+c".into()];
        assert!(keys.compile().is_err());
        keys.stop = vec!["ALT+H".into()];
        assert!(keys.compile().is_err());
        keys.stop = vec!["enter".into()];
        assert!(keys.compile().is_err());
        keys.stop = vec!["ctrl+h".into()];
        assert!(keys.compile().is_err());
        keys.stop = vec!["ctrl+1".into()];
        assert!(keys.compile().is_err());
        assert!(
            KeyBindings::default()
                .compile()
                .unwrap()
                .iter()
                .all(|(chord, _)| !matches!(chord.code, KeyCode::F(_)))
        );
    }
}
