//! UI-only settings. Run synchronous preference I/O on a blocking worker.
use crate::{
    commands::KeyBindings,
    config::{Interface, MAX_PREFERENCES_BYTES, Preferences, StatusPreset, read_preferences},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    StatusLine(bool),
    PromptHistory(bool),
    Status(StatusPreset),
    StatusColor(bool),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setting {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub examples: &'static [&'static str],
    pub choices: &'static [&'static str],
    pub value: String,
}

struct Spec {
    key: &'static str,
    label: &'static str,
    description: &'static str,
    examples: &'static [&'static str],
    choices: &'static [&'static str],
    kind: Kind,
}

#[derive(Clone, Copy)]
enum Kind {
    StatusLine,
    PromptHistory,
    Status,
    StatusColor,
}

const SPECS: &[Spec] = &[
    Spec {
        key: "status-line",
        label: "Status line",
        description: "Show the runtime status line.",
        examples: &["status-line true", "status-line false"],
        choices: &["true", "false"],
        kind: Kind::StatusLine,
    },
    Spec {
        key: "prompt-history",
        label: "Prompt history",
        description: "Enable navigation through previous prompts.",
        examples: &["prompt-history true", "prompt-history false"],
        choices: &["true", "false"],
        kind: Kind::PromptHistory,
    },
    Spec {
        key: "status",
        label: "Status preset",
        description: "Choose text, braille, minimal, or bars status presentation.",
        examples: &[
            "status text",
            "status braille",
            "status minimal",
            "status bars",
        ],
        choices: &["text", "braille", "minimal", "bars"],
        kind: Kind::Status,
    },
    Spec {
        key: "status-color",
        label: "Status color",
        description: "Use color in the status line.",
        examples: &["status-color true", "status-color false"],
        choices: &["true", "false"],
        kind: Kind::StatusColor,
    },
];

impl Kind {
    fn current(self, interface: &Interface) -> Change {
        match self {
            Self::StatusLine => Change::StatusLine(interface.status_line),
            Self::PromptHistory => Change::PromptHistory(interface.prompt_history),
            Self::Status => Change::Status(interface.status_preset),
            Self::StatusColor => Change::StatusColor(interface.status_color),
        }
    }
}

pub fn catalog(interface: &Interface) -> Vec<Setting> {
    SPECS
        .iter()
        .map(|spec| Setting {
            key: spec.key,
            label: spec.label,
            description: spec.description,
            examples: spec.examples,
            choices: spec.choices,
            value: spec.kind.current(interface).value().to_owned(),
        })
        .collect()
}

impl Change {
    /// Parse exactly two arguments, without command names or ignored trailing tokens.
    pub fn parse(args: &[&str]) -> Result<Self> {
        let [key, value] = args else {
            bail!("expected exactly <key> <value>");
        };
        let spec = SPECS
            .iter()
            .find(|spec| spec.key == *key)
            .with_context(|| format!("unknown setting {key}"))?;
        ensure!(
            spec.choices.contains(value),
            "invalid value for {key}; choose {}",
            spec.choices.join(", ")
        );
        Ok(match spec.kind {
            Kind::StatusLine => Self::StatusLine(*value == "true"),
            Kind::PromptHistory => Self::PromptHistory(*value == "true"),
            Kind::StatusColor => Self::StatusColor(*value == "true"),
            Kind::Status => Self::Status(match *value {
                "text" => StatusPreset::Text,
                "braille" => StatusPreset::Braille,
                "minimal" => StatusPreset::Minimal,
                "bars" => StatusPreset::Bars,
                _ => unreachable!("schema validated status preset"),
            }),
        })
    }

    pub fn key(&self) -> &'static str {
        match self {
            Self::StatusLine(_) => "status-line",
            Self::PromptHistory(_) => "prompt-history",
            Self::Status(_) => "status",
            Self::StatusColor(_) => "status-color",
        }
    }

    pub fn value(&self) -> &'static str {
        match self {
            Self::StatusLine(value) | Self::PromptHistory(value) | Self::StatusColor(value) => {
                if *value {
                    "true"
                } else {
                    "false"
                }
            }
            Self::Status(StatusPreset::Text) => "text",
            Self::Status(StatusPreset::Braille) => "braille",
            Self::Status(StatusPreset::Minimal) => "minimal",
            Self::Status(StatusPreset::Bars) => "bars",
        }
    }

    pub fn apply(&self, interface: &mut Interface) {
        match *self {
            Self::StatusLine(value) => interface.status_line = value,
            Self::PromptHistory(value) => interface.prompt_history = value,
            Self::Status(value) => interface.status_preset = value,
            Self::StatusColor(value) => interface.status_color = value,
        }
    }
}

/// Persist only this change, not the rest of the current session's interface.
/// A stable sibling lock protects read-modify-write across atomic replacements.
/// Explicit config fields still take precedence on the next load.
pub fn save_change(path: &Path, change: &Change) -> Result<()> {
    save_preferences(path, |preferences| match *change {
        Change::StatusLine(value) => preferences.interface.status_line = Some(value),
        Change::PromptHistory(value) => preferences.interface.prompt_history = Some(value),
        Change::Status(value) => preferences.interface.status_preset = Some(value),
        Change::StatusColor(value) => preferences.interface.status_color = Some(value),
    })
}

/// Save the four UI values in one atomic update, preserving key bindings.
pub fn save_interface(path: &Path, interface: &Interface) -> Result<()> {
    save_preferences(path, |preferences| {
        preferences.interface.status_line = Some(interface.status_line);
        preferences.interface.prompt_history = Some(interface.prompt_history);
        preferences.interface.status_preset = Some(interface.status_preset);
        preferences.interface.status_color = Some(interface.status_color);
    })
}

/// Save validated shortcuts without replacing existing interface preferences.
pub fn save_keys(path: &Path, keys: &KeyBindings) -> Result<()> {
    keys.compile_macros()?;
    save_preferences(path, |preferences| preferences.keys = Some(keys.clone()))
}

fn save_preferences(path: &Path, update: impl FnOnce(&mut Preferences)) -> Result<()> {
    let name = path
        .file_name()
        .context("preferences path needs a file name")?;
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create preferences directory {}", parent.display()))?;
    let mut lock_name = name.to_os_string();
    lock_name.push(".lock");
    let lock_path = parent.join(lock_name);
    if let Ok(metadata) = fs::symlink_metadata(&lock_path) {
        ensure!(
            metadata.is_file(),
            "preferences lock must be a regular file (not a symlink)"
        );
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = match options.open(&lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("open preferences lock {}", lock_path.display()))?,
        Err(error) => return Err(error).context("create preferences lock"),
    };
    FileExt::try_lock_exclusive(&lock)
        .context("preferences are busy or cannot be locked; retry later")?;
    let mut preferences = read_preferences(path)?;
    update(&mut preferences);
    let text = toml::to_string(&preferences).context("serialize preferences")?;
    ensure!(
        text.len() as u64 <= MAX_PREFERENCES_BYTES,
        "preferences exceeds 64 KiB"
    );
    let mut temp_name = name.to_os_string();
    temp_name.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let temp_path = parent.join(temp_name);
    let mut temp = options
        .open(&temp_path)
        .context("create private preferences temporary file")?;
    let result = (|| -> Result<()> {
        temp.write_all(text.as_bytes())
            .context("write preferences")?;
        temp.sync_all().context("sync preferences")?;
        // Reject a symlink or invalid file introduced while preparing the replacement.
        read_preferences(path)?;
        fs::rename(&temp_path, path).context("replace preferences atomically")?;
        #[cfg(unix)]
        fs::File::open(parent)?
            .sync_all()
            .context("sync preferences directory")?;
        Ok(())
    })();
    drop(temp);
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn catalog_and_parser_share_exact_choices_and_current_values() {
        let mut interface = Interface::default();
        let entries = catalog(&interface);
        assert_eq!(
            entries.iter().map(|entry| entry.key).collect::<Vec<_>>(),
            ["status-line", "prompt-history", "status", "status-color"]
        );
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.value.as_str())
                .collect::<Vec<_>>(),
            ["true", "true", "bars", "true"]
        );
        for entry in entries {
            assert!(!entry.label.is_empty() && !entry.description.is_empty());
            for value in entry.choices {
                let change = Change::parse(&[entry.key, value]).unwrap();
                assert_eq!(change.key(), entry.key);
                assert_eq!(change.value(), *value);
                change.apply(&mut interface);
                assert_eq!(
                    catalog(&interface)
                        .iter()
                        .find(|setting| setting.key == entry.key)
                        .unwrap()
                        .value,
                    *value
                );
            }
            for example in entry.examples {
                Change::parse(&example.split_whitespace().collect::<Vec<_>>()).unwrap();
            }
        }
    }

    #[test]
    fn bad_arguments_make_no_changes() {
        let interface = Interface::default();
        for args in [
            vec![],
            vec!["status"],
            vec!["status", "text", "extra"],
            vec!["model", "fake"],
            vec!["provider", "fake"],
            vec!["status_preset", "text"],
            vec!["status", "unknown"],
            vec!["status", "TEXT"],
            vec!["status-color", "yes"],
            vec!["status-line", "1"],
            vec!["prompt-history", "False"],
        ] {
            assert!(Change::parse(&args).is_err(), "{args:?}");
        }
        assert_eq!(interface, Interface::default());
    }

    #[test]
    fn save_changes_only_requested_fields_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/preferences.toml");
        save_change(&path, &Change::StatusColor(true)).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[interface]\nstatus_color = true\n"
        );
        let mut session = Config::load_with_preferences(None, &path)
            .unwrap()
            .interface;
        session.status_line = false;
        session.prompt_history = false;
        let change = Change::Status(StatusPreset::Braille);
        change.apply(&mut session);
        save_change(&path, &change).unwrap();
        let loaded = Config::load_with_preferences(None, &path).unwrap();
        assert_ne!(loaded.interface, session);
        assert_eq!(
            loaded.interface,
            Interface {
                status_preset: StatusPreset::Braille,
                status_color: true,
                ..Interface::default()
            }
        );
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("status_preset = \"braille\""));
        assert!(!text.contains("status_line") && !text.contains("prompt_history"));
        assert!(!text.contains("provider") && !text.contains("keys") && !text.contains("limits"));
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 2);
    }

    #[test]
    fn bars_save_reload_and_unrelated_updates_preserve_explicit_braille() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.toml");
        save_change(&path, &Change::parse(&["status", "bars"]).unwrap()).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[interface]\nstatus_preset = \"bars\"\n"
        );
        assert_eq!(
            Config::load_with_preferences(None, &path)
                .unwrap()
                .interface
                .status_preset,
            StatusPreset::Bars
        );
        fs::write(&path, "[interface]\nstatus_preset = 'braille'\n").unwrap();
        save_change(&path, &Change::StatusColor(false)).unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("status_preset = \"braille\"")
        );
        let loaded = Config::load_with_preferences(None, &path).unwrap();
        assert_eq!(loaded.interface.status_preset, StatusPreset::Braille);
        assert!(!loaded.interface.status_color);
    }

    #[test]
    fn disjoint_updates_preserve_existing_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.toml");
        fs::write(
            &path,
            "[interface]\nstatus_line = false\nprompt_history = false\n",
        )
        .unwrap();
        save_change(&path, &Change::StatusColor(true)).unwrap();
        save_change(&path, &Change::Status(StatusPreset::Minimal)).unwrap();
        let loaded = Config::load_with_preferences(None, &path).unwrap();
        assert_eq!(
            loaded.interface,
            Interface {
                status_line: false,
                prompt_history: false,
                status_preset: StatusPreset::Minimal,
                status_color: true,
            }
        );
    }

    #[test]
    fn saved_defaults_do_not_replace_explicit_config_values_or_sources() {
        let dir = tempfile::tempdir().unwrap();
        let preferences = dir.path().join("preferences.toml");
        let config = dir.path().join("config.toml");
        let original = "[interface]\nstatus_preset = 'minimal'\n";
        fs::write(&config, original).unwrap();
        save_change(&preferences, &Change::Status(StatusPreset::Braille)).unwrap();
        save_change(&preferences, &Change::StatusColor(true)).unwrap();
        let loaded = Config::load_with_preferences(Some(&config), &preferences).unwrap();
        assert_eq!(loaded.interface.status_preset, StatusPreset::Minimal);
        assert!(loaded.interface.status_color);
        let sources = crate::config::interface_sources(Some(&config), &preferences).unwrap();
        assert_eq!(
            sources["status"],
            format!("Explicit config ({})", config.display())
        );
        assert_eq!(
            sources["status-color"],
            format!("User defaults ({})", preferences.display())
        );
        assert_eq!(fs::read_to_string(&config).unwrap(), original);
        assert_eq!(
            Config::load_with_preferences(None, &preferences)
                .unwrap()
                .interface
                .status_preset,
            StatusPreset::Braille
        );
    }

    #[test]
    fn concurrent_disjoint_updates_are_preserved_after_busy_retries() {
        use std::{sync::Barrier, thread, time::Duration};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.toml");
        let changes = [
            Change::StatusLine(false),
            Change::PromptHistory(false),
            Change::Status(StatusPreset::Minimal),
            Change::StatusColor(true),
        ];
        let barrier = Barrier::new(changes.len());
        thread::scope(|scope| {
            for change in changes {
                let path = &path;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..1000 {
                        match save_change(path, &change) {
                            Ok(()) => return,
                            Err(error) if error.to_string().contains("busy") => {
                                thread::sleep(Duration::from_millis(1));
                            }
                            Err(error) => panic!("unexpected save failure: {error:#}"),
                        }
                    }
                    panic!("preference lock never became available");
                });
            }
        });
        let loaded = Config::load_with_preferences(None, &path).unwrap();
        assert_eq!(
            loaded.interface,
            Interface {
                status_line: false,
                prompt_history: false,
                status_preset: StatusPreset::Minimal,
                status_color: true,
            }
        );
    }

    #[test]
    fn locked_preferences_fail_without_waiting_or_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.toml");
        let lock = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.path().join("preferences.toml.lock"))
            .unwrap();
        FileExt::try_lock_exclusive(&lock).unwrap();
        let error = save_change(&path, &Change::StatusColor(true)).unwrap_err();
        assert!(error.to_string().contains("busy"));
        assert!(!path.exists());
    }

    #[test]
    fn malformed_unknown_and_oversized_preferences_are_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.toml");
        for text in [
            "[interface\n".to_owned(),
            "[interface]\nmodel = 'secret'\n".to_owned(),
            "[provider]\nscenario = 'normal'\n".to_owned(),
            "[interface]\nstatus_color = 'true'\n".to_owned(),
            " ".repeat(MAX_PREFERENCES_BYTES as usize + 1),
        ] {
            fs::write(&path, &text).unwrap();
            assert!(save_change(&path, &Change::StatusColor(true)).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn bad_paths_do_not_report_success() {
        let dir = tempfile::tempdir().unwrap();
        assert!(save_change(dir.path(), &Change::StatusColor(true)).is_err());
        let parent = dir.path().join("file");
        fs::write(&parent, "untouched").unwrap();
        assert!(save_change(&parent.join("preferences.toml"), &Change::StatusColor(true)).is_err());
        assert_eq!(fs::read_to_string(parent).unwrap(), "untouched");
        assert!(save_change(Path::new("/"), &Change::StatusColor(true)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_replacement_is_private_and_preserves_the_old_inode() {
        use std::{io::Read, os::unix::fs::PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.toml");
        let original = "[interface]\nstatus_line = false\n";
        fs::write(&path, original).unwrap();
        let mut old_file = fs::File::open(&path).unwrap();
        save_change(&path, &Change::StatusColor(true)).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(dir.path().join("preferences.toml.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let mut old_text = String::new();
        old_file.read_to_string(&mut old_text).unwrap();
        assert_eq!(old_text, original);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("status_color = true")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_paths_and_locks_cannot_overwrite_other_files() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("config.toml");
        let original = "[interface]\nstatus_line = false\n";
        fs::write(&other, original).unwrap();
        let path = dir.path().join("preferences.toml");
        symlink(&other, &path).unwrap();
        assert!(save_change(&path, &Change::StatusColor(true)).is_err());
        fs::remove_file(&path).unwrap();
        fs::remove_file(dir.path().join("preferences.toml.lock")).unwrap();
        symlink(&other, dir.path().join("preferences.toml.lock")).unwrap();
        assert!(save_change(&path, &Change::StatusColor(true)).is_err());
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(other).unwrap(), original);
    }
}
