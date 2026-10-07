//! Whether prbot is running: a lock file so only one copy runs per config (two copies would split
//! Slack's events between them), `prbot stop` / `prbot status`, and the signals that end the bot.

use crate::config;
use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::PathBuf;
use std::time::Duration;

fn lock_path() -> PathBuf {
    config::home_dir().join("prbot.lock")
}

fn stop_path() -> PathBuf {
    config::home_dir().join("prbot.stop")
}

fn open_lock() -> std::io::Result<File> {
    OpenOptions::new().create(true).truncate(false).write(true).open(lock_path())
}

/// Held for the life of the bot. The OS releases it even if the process is killed.
pub struct Lock(#[allow(dead_code)] File);

pub fn acquire() -> Result<Lock> {
    let file = open_lock()?;
    match file.try_lock() {
        Ok(()) => {
            let _ = std::fs::remove_file(stop_path());
            Ok(Lock(file))
        }
        Err(TryLockError::WouldBlock) => {
            bail!("prbot is already running for {} (stop it with `prbot stop`)", config::env_path().display())
        }
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

pub fn is_running() -> bool {
    open_lock().is_ok_and(|f| matches!(f.try_lock(), Err(TryLockError::WouldBlock)))
}

/// `prbot stop`: asks the running bot to shut down, which marks it as stopped in Slack.
pub async fn stop() -> Result<()> {
    if !is_running() {
        println!("prbot is not running.");
        return Ok(());
    }
    std::fs::write(stop_path(), "")?;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if !is_running() {
            println!("prbot stopped.");
            return Ok(());
        }
    }
    bail!("prbot did not stop within 15 seconds")
}

/// The prbot folder as an absolute path without Windows' `\\?\` prefix.
fn absolute_home() -> PathBuf {
    let home = config::home_dir();
    let abs = std::fs::canonicalize(&home).unwrap_or(home);
    PathBuf::from(abs.to_string_lossy().trim_start_matches(r"\\?\"))
}

#[cfg(windows)]
fn startup_shortcut() -> Result<PathBuf> {
    let appdata = std::env::var_os("APPDATA").context("APPDATA is not set")?;
    Ok(PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Startup\prbot.lnk"))
}

/// `prbot autostart on|off`: a shortcut in the Windows Startup folder that runs prbot without a window.
#[cfg(windows)]
pub fn autostart(on: bool) -> Result<()> {
    let lnk = startup_shortcut()?;
    if !on {
        if lnk.exists() {
            std::fs::remove_file(&lnk)?;
        }
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let quote = |p: &std::path::Path| p.to_string_lossy().replace('\'', "''");
    let script = format!(
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut('{}'); $s.TargetPath = 'conhost.exe'; \
         $s.Arguments = '--headless \"{}\"'; $s.WorkingDirectory = '{}'; $s.WindowStyle = 7; $s.Save()",
        quote(&lnk),
        quote(&exe),
        quote(&absolute_home())
    );
    let status = std::process::Command::new("powershell").args(["-NoProfile", "-Command", &script]).status()?;
    if !status.success() {
        bail!("could not create the Startup shortcut");
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn autostart(_on: bool) -> Result<()> {
    bail!("automatic start is only set up on Windows so far; see \"Run at login\" in the README")
}

/// Starts prbot in the background without a window, detached from this terminal.
#[cfg(windows)]
pub fn start_in_background() -> Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    std::process::Command::new("conhost.exe")
        .arg("--headless")
        .arg(std::env::current_exe()?)
        .current_dir(absolute_home())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
pub fn start_in_background() -> Result<()> {
    bail!("starting in the background is only set up on Windows so far; run `prbot`")
}

/// Resolves with the reason when the bot should shut down: `prbot stop`, Ctrl+C, closing its
/// console window, or (on macOS and Linux) SIGTERM from launchd or systemd.
pub async fn shutdown_signal() -> &'static str {
    let stop_requested = async {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if stop_path().exists() {
                let _ = std::fs::remove_file(stop_path());
                break "prbot stop";
            }
        }
    };
    #[cfg(windows)]
    let os = async {
        match tokio::signal::windows::ctrl_close() {
            Ok(mut close) => {
                close.recv().await;
                "console window closed"
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(unix)]
    let os = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
                "SIGTERM"
            }
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        r = stop_requested => r,
        r = os => r,
        _ = tokio::signal::ctrl_c() => "Ctrl+C",
    }
}
