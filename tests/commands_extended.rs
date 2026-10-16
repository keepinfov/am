use am::{
    commands::{self, Action, Arguments, Chord, CommandBinding, KeyBindings},
    config::{Config, Interface, StatusPreset},
    settings::{self, Change},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use fs2::FileExt;
use std::fs;

#[test]
fn extended_commands_dispatch_exact_actions_and_arguments() {
    for (text, action, arguments, args) in [
        ("/commands", Action::Commands, Arguments::Commands, ""),
        (
            "/commands keep",
            Action::Commands,
            Arguments::Commands,
            "keep",
        ),
        ("/exit", Action::Exit, Arguments::None, ""),
        ("/stop", Action::Stop, Arguments::None, ""),
        ("/draft save", Action::Draft, Arguments::Draft, "save"),
        (
            "/draft delete named_1",
            Action::Draft,
            Arguments::Draft,
            "delete named_1",
        ),
        ("/drafts", Action::Drafts, Arguments::None, ""),
        (
            "/send draft-1",
            Action::Send,
            Arguments::SendDraft,
            "draft-1",
        ),
        ("/bindings", Action::Bindings, Arguments::None, ""),
        (
            "/bind alt+r /stop",
            Action::Bind,
            Arguments::Bind,
            "alt+r /stop",
        ),
        (
            "/unbind alt+r --default",
            Action::Unbind,
            Arguments::Unbind,
            "alt+r --default",
        ),
    ] {
        let (spec, parsed_args) = commands::parse(text).unwrap().unwrap();
        assert_eq!(
            (spec.action, spec.arguments, parsed_args),
            (action, arguments, args)
        );
    }
    assert!(commands::parse("/resume").is_err());
    assert!(commands::parse("/actions").is_err());
    assert_eq!(
        commands::parse("/draft save").unwrap().unwrap().0.usage(),
        "/draft save | delete <id>"
    );
    assert!(
        !commands::COMMANDS
            .iter()
            .any(|spec| spec.action == Action::Resume)
    );
    for text in ["/history", "/history fullscreen"] {
        assert_eq!(
            commands::parse(text).unwrap().unwrap().0.action,
            Action::History
        );
    }
}

#[test]
fn invalid_single_line_commands_never_fall_through_to_prompt_text() {
    for text in [
        "/resume",
        "/draft",
        "/draft save id",
        "/draft delete",
        "/draft save id extra",
        "/draft save --default",
        "/draft save a/b",
        "/draft save .",
        "/draft delete a/b",
        "/draft delete --default",
        "/draft delete id extra",
        "/draft erase id",
        "/drafts id",
        "/send",
        "/send --force",
        "/send id --unknown",
        "/send id other",
        "/send id --after --force",
        "/send id --force --force",
        "/send id --run --hold",
        "/send id --hold --hold",
        "/mode",
        "/mode force",
        "/mode STEER",
        "/mode steer extra",
        "/queue resume",
        "/queue run pause",
        "/commands other",
        "/commands keep extra",
        "/exit extra",
        "/force extra",
        "/bindings extra",
        "/history inline",
        "/bind",
        "/bind alt+r",
        "/bind alt+r plain text",
        "/bind alt+r //literal",
        "/bind alt+r /help ;",
        "/bind alt+r ; /help",
        "/bind alt+r /resume",
        "/bind ctrl+c /help",
        "/unbind ctrl+c",
        "/unbind",
        "/unbind alt+r extra",
        "/unbind alt+r --default --default",
    ] {
        assert!(commands::parse(text).is_err(), "{text:?}");
    }
    assert!(commands::parse("//literal\ntext").unwrap().is_none());
    assert!(commands::parse("ordinary text").unwrap().is_none());
    assert!(commands::parse(&format!("/send {}", "a".repeat(129))).is_err());
    assert!(commands::parse(&format!("/help{}", " ".repeat(4091))).is_ok());
    assert!(commands::parse(&format!("/help{}", " ".repeat(4092))).is_err());
}

#[test]
fn send_accepts_exactly_one_valid_draft_id_and_no_flags() {
    for id in ["draft-1", "named_1", "A1", &"a".repeat(128)] {
        let text = format!("/send {id}");
        let (spec, args) = commands::parse(&text).unwrap().unwrap();
        assert_eq!(spec.action, Action::Send);
        assert_eq!(args, id);
        assert_eq!(spec.usage(), "/send <draft-id>");
    }
    for suffix in [
        "",
        "--force",
        "--after",
        "--run",
        "--hold",
        "a/b",
        ".",
        "-id",
        "caf\u{e9}",
        "id other",
        "id --force",
        "id --after",
        "id --run",
        "id --hold",
        "--force id",
        "id --force --run",
        "id --hold --after",
    ] {
        let text = format!("/send {suffix}");
        let error = commands::parse(&text).err().unwrap().to_string();
        assert!(error.contains("Usage: /send <draft-id>"), "{text}: {error}");
    }
}

#[test]
fn removed_commands_are_rejected_in_catalog_bindings_and_saved_macros() {
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
        assert!(!commands::COMMANDS.iter().any(|spec| spec.name == command));
        for text in [
            command.to_owned(),
            format!("{command} id"),
            command.to_ascii_uppercase(),
        ] {
            let error = commands::parse(&text).err().unwrap().to_string();
            assert!(error.contains("Unknown command"), "{text}: {error}");
            assert!(error.contains(command) || error.contains(&command.to_ascii_uppercase()));
            assert!(commands::parse_bind(&format!("alt+r /help ; {text}")).is_err());
            let keys = macro_keys(&["/help", &text]);
            let serialized = toml::to_string(&keys).unwrap();
            let saved: KeyBindings = toml::from_str(&serialized).unwrap();
            let error = saved.compile_macros().unwrap_err().to_string();
            assert!(error.contains("Unknown command"), "{text}: {error}");
            assert!(error.contains(command) || error.contains(&command.to_ascii_uppercase()));
        }
    }
}

fn macro_keys(commands: &[&str]) -> KeyBindings {
    KeyBindings {
        macros: vec![CommandBinding {
            key: "alt+r".into(),
            commands: commands.iter().map(|command| (*command).into()).collect(),
        }],
        ..KeyBindings::default()
    }
}

#[test]
fn multiline_slash_text_is_literal_input_but_never_a_binding_step() {
    for text in [
        "/help\nordinary text",
        "/help\rordinary text",
        "/help\r\nordinary text",
        "/help\n",
        "/help\r",
        "/unknown\nordinary text",
        "/bind alt+r /help\n/queue",
    ] {
        assert!(commands::parse(text).unwrap().is_none(), "{text:?}");
        assert!(macro_keys(&[text]).compile_macros().is_err(), "{text:?}");
        assert!(
            commands::parse_bind(&format!("alt+r {text}")).is_err(),
            "{text:?}"
        );
    }
    assert!(
        commands::parse(&format!("/help\n{}", "x".repeat(4096)))
            .unwrap()
            .is_none()
    );
}

#[test]
fn binding_chains_validate_and_preserve_inert_legacy_config_fields() {
    let (key, commands, default) =
        commands::parse_bind("alt+r /commands keep ; /stop --default").unwrap();
    assert_eq!(key, "alt+r");
    assert_eq!(commands, ["/commands keep", "/stop"]);
    assert!(default);
    assert!(!commands::parse_bind("alt+r /help").unwrap().2);
    let mut keys = macro_keys(&["/commands keep", "/stop"]);
    keys.resume = vec!["alt+s".into()];
    assert!(keys.get(Action::Resume).is_empty());
    assert!(keys.get(Action::ForceSend).is_empty());
    assert_eq!(keys.get(Action::FocusInput), ["alt+i"]);
    assert!(
        !keys
            .compile()
            .unwrap()
            .iter()
            .any(|(_, action)| *action == Action::Resume)
    );
    let compiled = keys.compile_macros().unwrap();
    assert_eq!(compiled[0].1, ["/commands keep", "/stop"]);
    assert!(
        compiled[0]
            .0
            .matches(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT))
    );
    let serialized = toml::to_string(&keys).unwrap();
    let roundtrip: KeyBindings = toml::from_str(&serialized).unwrap();
    assert_eq!(roundtrip.macros, keys.macros);
    assert_eq!(roundtrip.resume, keys.resume);
    assert!(roundtrip.get(Action::Resume).is_empty());
    assert_eq!(
        roundtrip.get(Action::FocusInput),
        keys.get(Action::FocusInput)
    );
}

#[test]
fn obsolete_key_lists_parse_and_roundtrip_but_never_execute_or_map_to_stop() {
    let mut keys = KeyBindings::default();
    let defaults = keys.compile().unwrap();
    for action in [
        Action::Queue,
        Action::Pause,
        Action::Resume,
        Action::TogglePause,
        Action::Interrupt,
        Action::CancelEdit,
        Action::Edit,
        Action::Cancel,
        Action::Mode,
        Action::ForceSend,
    ] {
        assert!(keys.get(action).is_empty());
        assert!(keys.label(action).is_empty());
        assert!(!defaults.iter().any(|(_, compiled)| *compiled == action));
    }
    for field in [
        &keys.queue,
        &keys.pause,
        &keys.resume,
        &keys.toggle_pause,
        &keys.interrupt,
        &keys.cancel_edit,
        &keys.edit,
        &keys.cancel,
    ] {
        assert!(field.is_empty());
    }
    // Even invalid, conflicting, or over-limit obsolete lists are inert.
    keys.queue = vec!["alt+q".into()];
    keys.pause = vec!["alt+s".into()];
    keys.resume = vec!["ctrl+c".into()];
    keys.toggle_pause = vec!["ctrl+p".into()];
    keys.interrupt = vec!["enter".into()];
    keys.cancel_edit = vec!["ctrl+x".into()];
    keys.edit = vec!["unsupported".into(); 9];
    keys.cancel = vec!["alt+s".into()];
    let serialized = toml::to_string(&keys).unwrap();
    let roundtrip: KeyBindings = toml::from_str(&serialized).unwrap();
    assert_eq!(toml::to_string(&roundtrip).unwrap(), serialized);
    assert_eq!(roundtrip.compile().unwrap(), defaults);
    assert!(roundtrip.label(Action::Pause).is_empty());
    assert!(roundtrip.label(Action::Stop).is_empty());
    assert!(roundtrip.compile_macros().unwrap().is_empty());
    let mut keys = roundtrip;
    keys.macros = macro_keys(&["/stop"]).macros;
    keys.macros[0].key = "alt+s".into();
    keys.compile_macros().unwrap();
    keys.stop = vec!["alt+s".into()];
    assert!(keys.compile_macros().is_err());
}

#[test]
fn focus_input_is_configurable_and_conflicts_with_actions_and_macros() {
    let mut keys = KeyBindings::default();
    assert_eq!(keys.label(Action::FocusInput), "alt+i");
    assert!(
        keys.compile()
            .unwrap()
            .iter()
            .any(|(chord, action)| *action == Action::FocusInput
                && *chord == Chord::parse("alt+i").unwrap())
    );
    assert!(
        !commands::COMMANDS
            .iter()
            .any(|spec| spec.action == Action::FocusInput)
    );
    keys.details = vec!["ALT+I".into()];
    assert!(keys.compile().is_err());
    keys.details = KeyBindings::default().details;
    keys.macros = vec![CommandBinding {
        key: "ALT+I".into(),
        commands: vec!["/draft save".into()],
    }];
    assert!(keys.compile_macros().is_err());
    keys.focus_input.clear();
    keys.compile_macros().unwrap();
    assert!(
        !keys
            .compile()
            .unwrap()
            .iter()
            .any(|(_, action)| *action == Action::FocusInput)
    );
    keys.focus_input = vec!["ctrl+c".into()];
    assert!(keys.compile().is_err());
    keys.focus_input = vec!["alt+v".into()];
    keys.compile_macros().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let preferences = dir.path().join("preferences.toml");
    settings::save_keys(&preferences, &keys).unwrap();
    assert_eq!(
        Config::load_with_preferences(None, &preferences)
            .unwrap()
            .keys
            .focus_input,
        ["alt+v"]
    );
    let config = dir.path().join("config.toml");
    fs::write(&config, "[keys]\nfocus_input = []\n").unwrap();
    assert!(
        Config::load_with_preferences(Some(&config), &preferences)
            .unwrap()
            .keys
            .focus_input
            .is_empty()
    );
    let example: Config = toml::from_str(include_str!("../examples/keybindings.toml")).unwrap();
    example.validate().unwrap();
}

#[test]
fn macros_reject_conflicts_reserved_keys_invalid_steps_and_resource_overflow() {
    for commands in [
        vec![],
        vec!["plain text"],
        vec!["/unknown"],
        vec!["/send id --run --hold"],
        vec!["/help\ntext"],
        vec!["/resume"],
    ] {
        assert!(macro_keys(&commands).compile_macros().is_err());
    }
    for key in ["CTRL+C", "ctrl+p", "ctrl+h", "enter"] {
        let mut keys = macro_keys(&["/help"]);
        keys.macros[0].key = key.into();
        assert!(keys.compile_macros().is_err(), "{key}");
    }
    let mut keys = macro_keys(&["/help"]);
    keys.resume = vec!["alt+r".into()];
    keys.compile_macros().unwrap();
    keys.stop = vec!["alt+r".into()];
    assert!(keys.compile_macros().is_err());
    keys.stop.clear();
    keys.macros.push(CommandBinding {
        key: "ALT+R".into(),
        commands: vec!["/help".into()],
    });
    assert!(keys.compile_macros().is_err());
    keys.macros.pop();
    keys.macros[0].commands = vec!["/help".into(); 16];
    keys.compile_macros().unwrap();
    keys.macros[0].commands.push("/help".into());
    assert!(keys.compile_macros().is_err());
    keys.macros[0].commands = vec![format!("/help{}", " ".repeat(4092))];
    assert!(keys.compile_macros().is_err());
    keys.macros = vec![
        CommandBinding {
            key: "alt+r".into(),
            commands: vec!["/help".into()]
        };
        33
    ];
    assert!(
        keys.compile_macros()
            .unwrap_err()
            .to_string()
            .contains("32")
    );
    keys.macros = (b'a'..=b'z')
        .chain(b'0'..=b'5')
        .map(|key| CommandBinding {
            key: format!("alt+{}", char::from(key)),
            commands: vec!["/help".into()],
        })
        .collect();
    keys.queue.clear();
    keys.details.clear();
    keys.focus_input.clear();
    assert_eq!(keys.compile_macros().unwrap().len(), 32);
    keys.quit.clear();
    keys.macros[0].key = "ctrl+c".into();
    assert!(keys.compile_macros().is_err());
    let chain = std::iter::repeat_n("/help", 17)
        .collect::<Vec<_>>()
        .join(" ; ");
    assert!(commands::parse_bind(&format!("alt+r {chain}")).is_err());
}

#[test]
fn shortcut_editor_events_roundtrip_and_navigation_is_reserved() {
    for key in [
        KeyEvent::new(KeyCode::Char('R'), KeyModifiers::ALT | KeyModifiers::SHIFT),
        KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ),
        KeyEvent::new(KeyCode::F(24), KeyModifiers::empty()),
    ] {
        let (text, chord) = Chord::from_event(key).unwrap();
        assert_eq!(Chord::parse(&text).unwrap(), chord);
        assert!(chord.matches(key));
    }
    for key in [
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()),
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::F(1), KeyModifiers::SHIFT),
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::SUPER),
    ] {
        assert!(Chord::from_event(key).is_err());
    }
}

#[test]
fn key_preferences_overlay_only_explicit_config_fields() {
    let dir = tempfile::tempdir().unwrap();
    let preferences = dir.path().join("preferences.toml");
    let config = dir.path().join("config.toml");
    fs::write(&preferences, "[keys]\ncommands = ['alt+p']\nhistory = ['alt+h']\n[[keys.macros]]\nkey = 'alt+r'\ncommands = ['/stop']\n").unwrap();
    let saved = Config::load_with_preferences(None, &preferences).unwrap();
    assert_eq!(saved.keys.commands, ["alt+p"]);
    assert_eq!(saved.keys.history, ["alt+h"]);
    assert_eq!(saved.keys.queue, KeyBindings::default().queue);
    for text in [
        "",
        "[provider]\nchunks = 17\n",
        "[keys]\nstop = ['alt+j']\n",
    ] {
        fs::write(&config, text).unwrap();
        let loaded = Config::load_with_preferences(Some(&config), &preferences).unwrap();
        assert_eq!(loaded.keys.commands, saved.keys.commands);
        assert_eq!(loaded.keys.history, saved.keys.history);
        assert_eq!(loaded.keys.macros, saved.keys.macros);
    }
    fs::write(&config, "[keys]\ncommands = []\nmacros = []\n").unwrap();
    let loaded = Config::load_with_preferences(Some(&config), &preferences).unwrap();
    assert!(loaded.keys.commands.is_empty() && loaded.keys.macros.is_empty());
    assert_eq!(loaded.keys.history, saved.keys.history);
    assert_eq!(
        Config::load(Some(&config)).unwrap().keys.history,
        KeyBindings::default().history
    );
    fs::write(&config, "[keys]\nstop = ['ALT+H']\n").unwrap();
    assert!(Config::load_with_preferences(Some(&config), &preferences).is_err());
}

#[test]
fn preference_writes_preserve_keys_and_interface_in_both_directions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/preferences.toml");
    settings::save_change(&path, &Change::StatusLine(false)).unwrap();
    let keys = macro_keys(&["/stop"]);
    settings::save_keys(&path, &keys).unwrap();
    settings::save_change(&path, &Change::Status(StatusPreset::Minimal)).unwrap();
    let config = Config::load_with_preferences(None, &path).unwrap();
    assert_eq!(config.keys.macros, keys.macros);
    assert!(!config.interface.status_line);
    assert_eq!(config.interface.status_preset, StatusPreset::Minimal);
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 2);
    let original = fs::read_to_string(&path).unwrap();
    let invalid = macro_keys(&["/resume"]);
    assert!(settings::save_keys(&path, &invalid).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_file_name("preferences.toml.lock"))
        .unwrap();
    FileExt::try_lock_exclusive(&lock).unwrap();
    assert!(
        settings::save_keys(&path, &keys)
            .unwrap_err()
            .to_string()
            .contains("busy")
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn invalid_saved_macros_are_not_loaded_or_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("preferences.toml");
    for text in [
        "[keys]\n[[keys.macros]]\nkey = 'ctrl+c'\ncommands = ['/help']\n",
        "[keys]\n[[keys.macros]]\nkey = 'alt+r'\ncommands = ['/resume']\n",
        "[keys]\n[[keys.macros]]\nkey = 'alt+r'\ncommands = ['/pause']\n",
        "[keys]\n[[keys.macros]]\nkey = 'alt+r'\ncommands = ['/queue run']\n",
        "[keys]\n[[keys.macros]]\nkey = 'alt+r'\ncommands = ['/send id --force']\n",
        "[keys]\nunknown = []\n",
    ] {
        fs::write(&path, text).unwrap();
        assert!(Config::load_with_preferences(None, &path).is_err());
        assert!(settings::save_change(&path, &Change::StatusLine(false)).is_err());
        assert!(settings::save_keys(&path, &KeyBindings::default()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }
}

#[test]
fn defaults_and_artifact_limits_are_bounded() {
    assert_eq!(Interface::default().status_preset, StatusPreset::Bars);
    assert_eq!(StatusPreset::default(), StatusPreset::Bars);
    assert!(Interface::default().status_color);
    let mut config = Config::default();
    assert_eq!(config.limits.output_bytes, 16 * 1024 * 1024);
    assert_eq!(config.limits.artifact_bytes, 256 * 1024 * 1024);
    config.validate().unwrap();
    for output in [0, 1024 * 1024 - 1, 1024 * 1024 * 1024 + 1] {
        config.limits.output_bytes = output;
        assert!(config.validate().is_err());
    }
    config.limits.output_bytes = 16 * 1024 * 1024;
    for artifact in [
        0,
        1024 * 1024 - 1,
        4 * 1024 * 1024 * 1024 + 1,
        8 * 1024 * 1024,
    ] {
        config.limits.artifact_bytes = artifact;
        assert!(config.validate().is_err());
    }
    config.limits.artifact_bytes = config.limits.output_bytes as u64;
    config.validate().unwrap();
}
