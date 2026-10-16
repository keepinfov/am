use am::{
    config::Config,
    runtime::{Client, Command, Phase, Reply},
};
use anyhow::{Context, Result, ensure};
use clap::Parser;
use directories::ProjectDirs;
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    time::Duration,
};

#[derive(Parser)]
#[command(
    version,
    about = "am nyam: local agent runtime with a deterministic test provider"
)]
struct Args {
    /// Explicit TOML file. No project configuration is loaded implicitly.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Global UI defaults TOML file (default: the user config directory/preferences.toml).
    #[arg(long)]
    preferences: Option<PathBuf>,
    /// SQLite session file. Reopening always starts paused.
    #[arg(long)]
    session: Option<PathBuf>,
    /// Run one fake response without a terminal; print the saved history as JSON.
    #[arg(long, requires = "prompt")]
    headless: bool,
    #[arg(long, requires = "headless")]
    prompt: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let preferences_path = match args.preferences {
        Some(path) => path,
        None => am::config::preference_path()?,
    };
    let config = Config::load_with_preferences(args.config.as_deref(), &preferences_path)
        .with_context(|| {
            format!(
                "load config {} and preferences {}",
                args.config.as_deref().map_or_else(
                    || "(builtins)".to_owned(),
                    |path| path.display().to_string()
                ),
                preferences_path.display()
            )
        })?;
    if !args.headless {
        ensure!(
            io::stdin().is_terminal() && io::stdout().is_terminal(),
            "interactive mode needs a terminal; use --headless --prompt TEXT for a fixture run"
        );
    }
    let path = if let Some(path) = args.session {
        path
    } else {
        let dirs = ProjectDirs::from("", "", "am")
            .context("cannot determine data directory; supply --session")?;
        std::fs::create_dir_all(dirs.data_dir())?;
        dirs.data_dir().join("session.sqlite3")
    };
    let client = Client::start(&path, config.clone()).await?;
    let result = if args.headless {
        tokio::select! {
            result = headless(&client, args.prompt.unwrap()) => result,
            result = terminate() => result,
        }
    } else {
        am::client::run(
            client.clone(),
            &config,
            args.config.as_deref(),
            &preferences_path,
        )
        .await
    };
    let shutdown = client.shutdown().await;
    result?;
    shutdown
}

async fn terminate() -> Result<()> {
    #[cfg(unix)]
    {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        signal
            .recv()
            .await
            .context("termination signal listener stopped")?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::future::pending::<Result<()>>().await
    }
}

async fn headless(client: &Client, prompt: String) -> Result<()> {
    // Explicit CLI invocation resumes the queue, unlike mere session recovery.
    client.command(Command::Send { text: prompt }).await?;
    client.command(Command::Resume).await?;
    let mut snapshots = client.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let snapshot = client.snapshot();
        ensure!(
            snapshot.phase != Phase::StorageError,
            "{}",
            snapshot.error.unwrap_or_default()
        );
        if snapshot.active.is_none() && snapshot.queue.is_empty() {
            break;
        }
        tokio::select! {
            change = snapshots.changed() => change.context("runtime stopped")?,
            _ = tokio::signal::ctrl_c() => { client.command(Command::Stop).await?; break; },
            _ = tokio::time::sleep_until(deadline) => anyhow::bail!("fixture deadline exceeded"),
        }
    }
    if let Reply::History(history) = client
        .command(Command::History {
            before: None,
            limit: 200,
        })
        .await?
    {
        println!("{}", serde_json::to_string_pretty(&history)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_preferences_path_is_available_for_headless_isolation() {
        let args = Args::try_parse_from([
            "am",
            "--headless",
            "--prompt",
            "fixture",
            "--preferences",
            "isolated.toml",
        ])
        .unwrap();
        assert_eq!(args.preferences, Some(PathBuf::from("isolated.toml")));
        assert!(args.headless);
    }
}
