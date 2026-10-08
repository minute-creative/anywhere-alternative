//! Starting and stopping the two engines the app drives: `aa-host` (share
//! this computer) and `aa-viewer` (watch another one). They are separate
//! programs shipped next to the app, so a crash in one never takes the app
//! down, and they keep their own well-tested command-line behaviour.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Where the app keeps its logs and settings.
pub fn data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"));
    let dir = base.unwrap_or_else(std::env::temp_dir).join("AnywhereAlternative");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// A program shipped next to this one (same folder in the installed app).
pub fn sibling(name: &str) -> PathBuf {
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(&exe)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(exe))
}

fn quiet(cmd: &mut Command) -> &mut Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn log_file(path: &Path) -> std::io::Result<(Stdio, Stdio)> {
    // A log left by an administrator run belongs to root; ours is a new file.
    let _ = std::fs::remove_file(path);
    let f = std::fs::File::create(path)?;
    Ok((Stdio::from(f.try_clone()?), Stdio::from(f)))
}

/// This computer, shared.
pub struct Host {
    child: Option<Child>,
    stop_file: PathBuf,
    pub log: PathBuf,
    pub admin: bool,
    stopping: Option<Instant>,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Host").field("admin", &self.admin).finish_non_exhaustive()
    }
}

impl Host {
    /// Start sharing. `admin`: run as administrator (needed on a Mac for
    /// controllers); macOS shows its own password prompt.
    pub fn start(admin: bool) -> anyhow::Result<Self> {
        let dir = data_dir();
        let log = dir.join("host.log");
        let stop_file = dir.join("host.stop");
        let _ = std::fs::remove_file(&stop_file);
        let exe = sibling("aa-host");
        let child = if admin && cfg!(target_os = "macos") {
            let _ = std::fs::remove_file(&log);
            // `do shell script … with administrator privileges` is the
            // standard macOS password prompt; the host runs in the background
            // and stops itself when the stop file appears.
            let q = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', r"'\''"));
            let script = format!(
                "do shell script \"{} --stop-file {} > {} 2>&1 &\" with administrator privileges",
                q(&exe).replace('"', "\\\""),
                q(&stop_file).replace('"', "\\\""),
                q(&log).replace('"', "\\\""),
            );
            let status = Command::new("/usr/bin/osascript").args(["-e", &script]).status()?;
            anyhow::ensure!(status.success(), "the administrator password prompt was cancelled");
            None
        } else {
            let (out, err) = log_file(&log)?;
            let mut cmd = Command::new(&exe);
            cmd.arg("--stop-file").arg(&stop_file).stdout(out).stderr(err).stdin(Stdio::null());
            // AA_HOST_MOCK=1: test pattern instead of the screen (testing the app).
            if std::env::var_os("AA_HOST_MOCK").is_some() {
                cmd.arg("--mock");
            }
            Some(quiet(&mut cmd).spawn().map_err(|e| anyhow::anyhow!("could not start {}: {e}", exe.display()))?)
        };
        Ok(Self { child, stop_file, log, admin, stopping: None })
    }

    /// Ask it to stop (it says goodbye to the viewer and restores sound).
    pub fn stop(&mut self) {
        if self.stopping.is_none() {
            let _ = std::fs::write(&self.stop_file, b"stop");
            self.stopping = Some(Instant::now());
        }
    }

    /// Still running? Also finishes a stop that takes too long.
    pub fn alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => {
                if matches!(c.try_wait(), Ok(Some(_))) {
                    return false;
                }
                if self.stopping.is_some_and(|t| t.elapsed() > Duration::from_secs(5)) {
                    let _ = c.kill();
                    let _ = c.wait();
                    return false;
                }
                true
            }
            // Started as administrator: we can't see the process; it is gone
            // once it has removed the stop file (or a while after asking).
            None => match self.stopping {
                Some(t) => self.stop_file.exists() && t.elapsed() < Duration::from_secs(8),
                None => true,
            },
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A viewer window showing another computer.
pub struct Viewer {
    pub child: Child,
    pub target: String,
    pub log: PathBuf,
}

impl std::fmt::Debug for Viewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Viewer").field("target", &self.target).finish_non_exhaustive()
    }
}

impl Viewer {
    pub fn start(target: &str, fullscreen: bool, mic: bool) -> anyhow::Result<Self> {
        let log = data_dir().join("viewer.log");
        let (out, err) = log_file(&log)?;
        let exe = sibling("aa-viewer");
        let mut cmd = Command::new(&exe);
        cmd.arg(target).stdout(out).stderr(err).stdin(Stdio::null());
        if fullscreen {
            cmd.arg("--fullscreen");
        }
        if mic {
            cmd.arg("--mic");
        }
        let child = quiet(&mut cmd).spawn().map_err(|e| anyhow::anyhow!("could not start {}: {e}", exe.display()))?;
        Ok(Self { child, target: target.to_owned(), log })
    }

    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn close(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The last `max` lines of a log file (cheap: logs are small).
pub fn tail(path: &Path, max: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(max)..].iter().map(|l| (*l).to_owned()).collect()
}

/// A log line without its timestamp and module path, for people.
pub fn plain(line: &str) -> String {
    // "2026-10-08T06:21:43.67Z  INFO aa_host::session: listening on …"
    let Some((stamp, rest)) = line.split_once("Z ") else { return line.to_owned() };
    if !stamp.starts_with(|c: char| c.is_ascii_digit()) || !stamp.contains('T') {
        return line.to_owned(); // not a log line (e.g. a library's own output)
    }
    let rest = rest.trim_start();
    let (level, rest) = rest.split_once(' ').unwrap_or(("", rest));
    let rest = rest.trim_start();
    let msg = rest.split_once(": ").map_or(rest, |(_, m)| m);
    match level {
        "WARN" | "ERROR" => format!("⚠ {msg}"),
        _ => msg.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lines_read_like_sentences() {
        assert_eq!(
            plain("2026-10-08T06:21:43.678616Z  INFO aa_host::session: listening on 0.0.0.0:7700"),
            "listening on 0.0.0.0:7700"
        );
        assert_eq!(plain("2026-10-08T06:21:43.6Z  WARN aa_platform::macos::input: allow it"), "⚠ allow it");
        assert_eq!(plain("not a log line"), "not a log line");
    }
}
