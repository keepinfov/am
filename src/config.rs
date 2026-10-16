use anyhow::{Context, Result, ensure};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub provider: FakeConfig,
    pub limits: Limits,
    pub keys: crate::commands::KeyBindings,
    pub interface: Interface,
}

/// Deliberate allowlist for the internal attempt journal, not a full client config.
#[derive(Debug, Serialize)]
pub struct AttemptConfig<'a> {
    pub provider: &'a FakeConfig,
    pub limits: &'a Limits,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Interface {
    pub status_line: bool,
    pub prompt_history: bool,
    pub status_preset: StatusPreset,
    pub status_color: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StatusPreset {
    Text,
    Braille,
    Minimal,
    #[default]
    Bars,
}

impl Default for Interface {
    fn default() -> Self {
        Self {
            status_line: true,
            prompt_history: true,
            status_preset: StatusPreset::Bars,
            status_color: true,
        }
    }
}

pub(crate) const MAX_PREFERENCES_BYTES: u64 = 64 * 1024;

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Preferences {
    pub interface: InterfaceOverrides,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keys: Option<crate::commands::KeyBindings>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct InterfaceOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_line: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_history: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_preset: Option<StatusPreset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_color: Option<bool>,
}

impl InterfaceOverrides {
    fn apply(&self, interface: &mut Interface) {
        if let Some(value) = self.status_line {
            interface.status_line = value;
        }
        if let Some(value) = self.prompt_history {
            interface.prompt_history = value;
        }
        if let Some(value) = self.status_preset {
            interface.status_preset = value;
        }
        if let Some(value) = self.status_color {
            interface.status_color = value;
        }
    }

    fn record_sources(&self, sources: &mut BTreeMap<&'static str, String>, source: &str) {
        for (key, present) in [
            ("status-line", self.status_line.is_some()),
            ("prompt-history", self.prompt_history.is_some()),
            ("status", self.status_preset.is_some()),
            ("status-color", self.status_color.is_some()),
        ] {
            if present {
                sources.insert(key, source.to_owned());
            }
        }
    }
}

/// Startup-only provenance, keyed like `settings::Change::key()`.
/// Missing preferences use builtins; present invalid/unreadable files fail normally.
/// Labels are bounded to 512 characters; the UI must still clean them for rendering.
pub fn interface_sources(
    config_path: Option<&Path>,
    preferences_path: &Path,
) -> Result<BTreeMap<&'static str, String>> {
    Config::load_inner(config_path, Some(preferences_path)).map(|(_, sources)| sources)
}

/// The global UI defaults file, never a project or provider configuration.
pub fn preference_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "am")
        .context("cannot determine preferences directory; supply --preferences")?;
    Ok(dirs.config_dir().join("preferences.toml"))
}

pub(crate) fn read_preferences(path: &Path) -> Result<Preferences> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "preferences {} must be a regular file (not a symlink)",
                path.display()
            );
            ensure!(
                metadata.len() <= MAX_PREFERENCES_BYTES,
                "preferences {} exceeds 64 KiB",
                path.display()
            );
        }
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Preferences::default()),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect preferences {}", path.display()));
        }
    }
    let file =
        fs::File::open(path).with_context(|| format!("read preferences {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "preferences must be a regular file"
    );
    let mut text = String::new();
    file.take(MAX_PREFERENCES_BYTES + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("read preferences {}", path.display()))?;
    ensure!(
        text.len() as u64 <= MAX_PREFERENCES_BYTES,
        "preferences {} exceeds 64 KiB",
        path.display()
    );
    let preferences: Preferences =
        toml::from_str(&text).with_context(|| format!("invalid preferences {}", path.display()))?;
    if let Some(keys) = &preferences.keys {
        keys.compile_macros()
            .with_context(|| format!("invalid preferences keys {}", path.display()))?;
    }
    Ok(preferences)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FakeConfig {
    pub scenario: Scenario,
    pub delay_ms: u64,
    pub first_event_ms: u64,
    pub chunks: usize,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    #[default]
    Normal,
    Waiting,
    Interrupted,
    Error,
    Burst,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub queue_messages: usize,
    pub message_bytes: usize,
    pub response_bytes: usize,
    pub history_page: usize,
    pub checkpoint_ms: u64,
    pub storage_bytes: u64,
    pub output_bytes: usize,
    pub artifact_bytes: u64,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            scenario: Scenario::Normal,
            delay_ms: 70,
            first_event_ms: 200,
            chunks: 64,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            queue_messages: 32,
            message_bytes: 16 * 1024,
            response_bytes: 64 * 1024,
            history_page: 50,
            checkpoint_ms: 250,
            storage_bytes: 64 * 1024 * 1024,
            output_bytes: 16 * 1024 * 1024,
            artifact_bytes: 256 * 1024 * 1024,
        }
    }
}

impl Config {
    pub fn attempt_config(&self) -> AttemptConfig<'_> {
        AttemptConfig {
            provider: &self.provider,
            limits: &self.limits,
        }
    }

    pub fn load(path: Option<&Path>) -> Result<Self> {
        Self::load_inner(path, None).map(|(config, _)| config)
    }

    pub fn load_with_preferences(path: Option<&Path>, preferences_path: &Path) -> Result<Self> {
        Self::load_inner(path, Some(preferences_path)).map(|(config, _)| config)
    }

    fn load_inner(
        path: Option<&Path>,
        preferences_path: Option<&Path>,
    ) -> Result<(Self, BTreeMap<&'static str, String>)> {
        if let (Some(path), Some(preferences_path)) = (path, preferences_path) {
            ensure!(
                path != preferences_path
                    && !matches!((fs::canonicalize(path), fs::canonicalize(preferences_path)), (Ok(a), Ok(b)) if a == b),
                "config and preferences must be separate files"
            );
        }
        let mut interface = Interface::default();
        let mut keys = crate::commands::KeyBindings::default();
        let mut sources = ["status-line", "prompt-history", "status", "status-color"]
            .into_iter()
            .map(|key| (key, "Built-in defaults".to_owned()))
            .collect();
        if let Some(path) = preferences_path {
            let preferences = read_preferences(path)?;
            if let Some(saved) = preferences.keys {
                keys = saved;
            }
            let overrides = preferences.interface;
            overrides.apply(&mut interface);
            let source: String = format!("User defaults ({})", path.display())
                .chars()
                .take(512)
                .collect();
            overrides.record_sources(&mut sources, &source);
        }
        let mut config = Self::default();
        if let Some(path) = path {
            let text = fs::read_to_string(path)
                .with_context(|| format!("read config {}", path.display()))?;
            let value: toml::Value = toml::from_str(&text)
                .with_context(|| format!("invalid TOML configuration {}", path.display()))?;
            config = value
                .clone()
                .try_into()
                .with_context(|| format!("invalid configuration {}", path.display()))?;
            if let Some(value) = value.get("keys") {
                // Overlay only explicitly supplied action lists/macros, not serde defaults.
                let mut merged = toml::Value::try_from(&keys)?;
                let table = value.as_table().context("keys must be a table")?;
                for (key, value) in table {
                    merged
                        .as_table_mut()
                        .unwrap()
                        .insert(key.clone(), value.clone());
                }
                keys = merged.try_into()?;
            }
            if let Some(value) = value.get("interface") {
                let overrides: InterfaceOverrides = value.clone().try_into()?;
                overrides.apply(&mut interface);
                let source: String = format!("Explicit config ({})", path.display())
                    .chars()
                    .take(512)
                    .collect();
                overrides.record_sources(&mut sources, &source);
            }
        }
        config.interface = interface;
        config.keys = keys;
        config.validate()?;
        Ok((config, sources))
    }

    pub fn validate(&self) -> Result<()> {
        self.keys.compile_macros()?;
        ensure!(
            (1..=1024).contains(&self.limits.queue_messages),
            "queue_messages must be 1..=1024"
        );
        ensure!(
            (1..=1024 * 1024).contains(&self.limits.message_bytes),
            "message_bytes must be 1..=1048576"
        );
        ensure!(
            (1..=4 * 1024 * 1024).contains(&self.limits.response_bytes),
            "response_bytes must be 1..=4194304"
        );
        ensure!(
            (1..=200).contains(&self.limits.history_page),
            "history_page must be 1..=200"
        );
        ensure!(
            (10..=5000).contains(&self.limits.checkpoint_ms),
            "checkpoint_ms must be 10..=5000"
        );
        ensure!(
            (1024 * 1024..=4 * 1024 * 1024 * 1024).contains(&self.limits.storage_bytes),
            "storage_bytes must be 1MiB..=4GiB"
        );
        ensure!(
            (1024 * 1024..=1024 * 1024 * 1024).contains(&self.limits.output_bytes),
            "output_bytes must be 1MiB..=1GiB"
        );
        ensure!(
            (1024 * 1024..=4 * 1024 * 1024 * 1024).contains(&self.limits.artifact_bytes),
            "artifact_bytes must be 1MiB..=4GiB"
        );
        ensure!(
            self.limits.artifact_bytes >= self.limits.output_bytes as u64,
            "artifact_bytes must be at least output_bytes"
        );
        ensure!(
            self.provider.delay_ms <= 60_000 && self.provider.first_event_ms <= 60_000,
            "provider delay exceeds 60000ms"
        );
        ensure!(
            (1..=100_000).contains(&self.provider.chunks),
            "chunks must be 1..=100000"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_preferences_use_builtins_without_creating_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing/preferences.toml");
        let config = Config::load_with_preferences(None, &path).unwrap();
        assert_eq!(config.interface, Interface::default());
        assert_eq!(config.interface.status_preset, StatusPreset::Bars);
        assert_eq!(StatusPreset::default(), StatusPreset::Bars);
        let sources = interface_sources(None, &path).unwrap();
        assert_eq!(sources.len(), 4);
        assert!(sources.values().all(|source| source == "Built-in defaults"));
        assert_eq!(
            toml::to_string(&config).unwrap(),
            toml::to_string(&Config::default()).unwrap()
        );
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn interface_sources_track_field_presence_not_value_differences() {
        let dir = tempfile::tempdir().unwrap();
        let preferences = dir.path().join("preferences.toml");
        let config = dir.path().join("config.toml");
        for (field, key, value) in [
            ("status_line", "status-line", "true"),
            ("prompt_history", "prompt-history", "true"),
            ("status_preset", "status", "'bars'"),
            ("status_color", "status-color", "true"),
        ] {
            fs::write(&preferences, format!("[interface]\n{field} = {value}\n")).unwrap();
            fs::write(&config, "[interface]\n[provider]\nchunks = 17\n").unwrap();
            let sources = interface_sources(Some(&config), &preferences).unwrap();
            assert_eq!(sources.len(), 4);
            for (entry, source) in &sources {
                assert_eq!(
                    source,
                    &if *entry == key {
                        format!("User defaults ({})", preferences.display())
                    } else {
                        "Built-in defaults".to_owned()
                    }
                );
            }
            fs::write(&config, format!("[interface]\n{field} = {value}\n")).unwrap();
            let sources = interface_sources(Some(&config), &preferences).unwrap();
            assert_eq!(
                sources[key],
                format!("Explicit config ({})", config.display())
            );
            assert_eq!(
                Config::load_with_preferences(Some(&config), &preferences)
                    .unwrap()
                    .interface,
                Interface::default()
            );
        }
    }

    #[test]
    fn interface_sources_follow_mixed_partial_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let preferences = dir.path().join("preferences.toml");
        let config = dir.path().join("config.toml");
        fs::write(
            &preferences,
            "[interface]\nstatus_line = false\nprompt_history = false\nstatus_preset = 'braille'\n",
        )
        .unwrap();
        fs::write(&config, "[interface]\nstatus_preset = 'minimal'\n").unwrap();
        let sources = interface_sources(Some(&config), &preferences).unwrap();
        assert_eq!(
            sources["status-line"],
            format!("User defaults ({})", preferences.display())
        );
        assert_eq!(sources["prompt-history"], sources["status-line"]);
        assert_eq!(
            sources["status"],
            format!("Explicit config ({})", config.display())
        );
        assert_eq!(sources["status-color"], "Built-in defaults");
        assert_eq!(
            Config::load_with_preferences(Some(&config), &preferences)
                .unwrap()
                .interface,
            Interface {
                status_line: false,
                prompt_history: false,
                status_preset: StatusPreset::Minimal,
                status_color: true
            }
        );
        fs::write(&preferences, "[interface]\n").unwrap();
        fs::write(&config, "[interface]\n").unwrap();
        assert!(
            interface_sources(Some(&config), &preferences)
                .unwrap()
                .values()
                .all(|source| source == "Built-in defaults")
        );
    }

    #[test]
    fn interface_source_labels_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut parent = dir.path().to_owned();
        for _ in 0..6 {
            parent.push("x".repeat(100));
        }
        fs::create_dir_all(&parent).unwrap();
        let preferences = parent.join("preferences.toml");
        let config = parent.join("config.toml");
        fs::write(&preferences, "[interface]\nstatus_line = false\n").unwrap();
        fs::write(&config, "[interface]\nstatus_color = true\n").unwrap();
        let sources = interface_sources(Some(&config), &preferences).unwrap();
        assert_eq!(sources["status-line"].chars().count(), 512);
        assert_eq!(sources["status-color"].chars().count(), 512);
        assert!(sources.values().all(|source| source.chars().count() <= 512));
    }

    #[test]
    fn explicit_interface_overrides_only_fields_present() {
        let dir = tempfile::tempdir().unwrap();
        let preferences = dir.path().join("preferences.toml");
        let path = dir.path().join("config.toml");
        fs::write(&preferences, "[interface]\nstatus_line = false\nprompt_history = false\nstatus_preset = 'braille'\nstatus_color = true\n").unwrap();
        for (text, expected) in [
            (
                "",
                Interface {
                    status_line: false,
                    prompt_history: false,
                    status_preset: StatusPreset::Braille,
                    status_color: true,
                },
            ),
            (
                "[interface]\nstatus_line = true",
                Interface {
                    status_line: true,
                    prompt_history: false,
                    status_preset: StatusPreset::Braille,
                    status_color: true,
                },
            ),
            (
                "[interface]\nprompt_history = true",
                Interface {
                    status_line: false,
                    prompt_history: true,
                    status_preset: StatusPreset::Braille,
                    status_color: true,
                },
            ),
            (
                "[interface]\nstatus_preset = 'minimal'",
                Interface {
                    status_line: false,
                    prompt_history: false,
                    status_preset: StatusPreset::Minimal,
                    status_color: true,
                },
            ),
            (
                "[interface]\nstatus_color = false",
                Interface {
                    status_line: false,
                    prompt_history: false,
                    status_preset: StatusPreset::Braille,
                    status_color: false,
                },
            ),
        ] {
            fs::write(&path, text).unwrap();
            assert_eq!(
                Config::load_with_preferences(Some(&path), &preferences)
                    .unwrap()
                    .interface,
                expected
            );
        }
        fs::write(
            &path,
            "[provider]\nchunks = 17\n[limits]\nhistory_page = 12\n",
        )
        .unwrap();
        let config = Config::load_with_preferences(Some(&path), &preferences).unwrap();
        assert_eq!(config.provider.chunks, 17);
        assert_eq!(config.limits.history_page, 12);
        assert!(!config.interface.status_line);
        assert_eq!(
            Config::load(Some(&path)).unwrap().interface,
            Interface::default()
        );
    }

    #[test]
    fn present_invalid_or_unreadable_preferences_fail_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.toml");
        for text in [
            "[interface",
            "[interface]\nstatus_preset = 'unknown'",
            "[interface]\nstatus_color = 'true'",
            "[interface]\nunknown = false",
            "[limits]\nhistory_page = 1",
            "api_key = 'secret'",
        ] {
            fs::write(&path, text).unwrap();
            let error = Config::load_with_preferences(None, &path).unwrap_err();
            assert!(error.to_string().contains("preferences"));
            assert!(
                interface_sources(None, &path)
                    .unwrap_err()
                    .to_string()
                    .contains("preferences")
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(Config::load_with_preferences(None, &path).is_err());
        assert!(interface_sources(None, &path).is_err());
        assert!(Config::load_with_preferences(None, dir.path()).is_err());
        assert!(interface_sources(None, dir.path()).is_err());
        assert!(Config::load_with_preferences(None, &path.join("child")).is_err());
        assert!(interface_sources(None, &path.join("child")).is_err());
    }

    #[test]
    fn preference_reads_are_bounded_but_general_configuration_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.toml");
        fs::write(&path, " ".repeat(MAX_PREFERENCES_BYTES as usize + 1)).unwrap();
        assert!(
            Config::load_with_preferences(None, &path)
                .unwrap_err()
                .to_string()
                .contains("64 KiB")
        );
        Config::load(Some(&path)).unwrap();
    }

    #[test]
    fn explicit_config_errors_and_shared_paths_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let preferences = dir.path().join("preferences.toml");
        let config = dir.path().join("config.toml");
        assert!(Config::load_with_preferences(Some(&config), &preferences).is_err());
        assert!(interface_sources(Some(&config), &preferences).is_err());
        fs::write(&config, "[interface]\nstatus_preset = 'unknown'").unwrap();
        assert!(Config::load_with_preferences(Some(&config), &preferences).is_err());
        assert!(interface_sources(Some(&config), &preferences).is_err());
        fs::write(&config, "[interface]\nstatus_color = true").unwrap();
        assert!(Config::load_with_preferences(Some(&config), &config).is_err());
        assert!(interface_sources(Some(&config), &config).is_err());
        fs::write(&config, "[limits]\nhistory_page = 0").unwrap();
        assert!(interface_sources(Some(&config), &preferences).is_err());
    }

    #[test]
    fn status_presets_serialize_as_lowercase() {
        for (preset, value) in [
            (StatusPreset::Text, "text"),
            (StatusPreset::Braille, "braille"),
            (StatusPreset::Minimal, "minimal"),
            (StatusPreset::Bars, "bars"),
        ] {
            let interface = Interface {
                status_preset: preset,
                ..Interface::default()
            };
            assert!(
                toml::to_string(&interface)
                    .unwrap()
                    .contains(&format!("status_preset = \"{value}\""))
            );
            assert_eq!(
                toml::from_str::<Interface>(&toml::to_string(&interface).unwrap()).unwrap(),
                interface
            );
        }
    }

    #[test]
    fn shipped_example_matches_valid_defaults() {
        let config: Config = toml::from_str(include_str!("../examples/stage1.toml")).unwrap();
        config.validate().unwrap();
        assert_eq!(
            toml::to_string(&config).unwrap(),
            toml::to_string(&Config::default()).unwrap()
        );
    }

    #[test]
    fn unknown_configuration_is_not_silently_ignored() {
        assert!(toml::from_str::<Config>("api_key='not-a-supported-setting'").is_err());
        assert!(toml::from_str::<Config>("[provider]\nendpoint='not-supported-yet'").is_err());
    }

    #[test]
    fn invalid_limits_fail_before_startup() {
        let mut config = Config::default();
        config.limits.message_bytes = 0;
        assert!(config.validate().is_err());
        config.limits.message_bytes = 1024;
        config.limits.checkpoint_ms = 0;
        assert!(config.validate().is_err());
    }
}
