//! Sharing that survives a restart.
//!
//! **Mac.** A small system file (a "launch agent") tells macOS to start the
//! host by itself in two places: at the login screen, before anyone logs
//! in, and again inside the logged-in session. Together with "start up
//! automatically after a power failure" (`pmset autorestart`), a Mac that
//! loses power comes back on, shows its login screen, and a paired
//! computer can see it and type the Mac password from afar. Installing it
//! needs the Mac password once (macOS asks).
//!
//! `FileVault` is the one thing no app can get past: with it on, the Mac
//! stops at a password screen *before* macOS (and any app) starts.
//!
//! **Windows.** "Start when I sign in" puts Anywhere in the user's startup
//! list; it opens minimised and shares.

#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;

#[cfg(target_os = "macos")]
pub const LABEL: &str = "com.minutecreative.anywhere.host";

#[cfg(target_os = "macos")]
fn plist_path() -> PathBuf {
    PathBuf::from(format!("/Library/LaunchAgents/{LABEL}.plist"))
}

/// Is the always-on sharing set up on this Mac?
pub fn installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        plist_path().exists()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// The log of the logged-in copy (what the Share page shows).
pub fn log_path() -> PathBuf {
    PathBuf::from(aa_platform::trust::MAC_SYSTEM_DIR).join("service.log")
}

/// Is `FileVault` on? `None` if we can't tell (not a Mac).
pub fn filevault_on() -> Option<bool> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::process::Command::new("/usr/bin/fdesetup").arg("status").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.contains("FileVault is On"))
}

#[cfg(target_os = "macos")]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Run a shell script as administrator (macOS shows its password prompt).
#[cfg(target_os = "macos")]
fn run_as_admin(script: &str) -> anyhow::Result<()> {
    let path = aa_platform::trust::user_dir().join("admin-step.sh");
    std::fs::write(&path, script)?;
    let cmd = format!("/bin/sh {}", shell_quote(&path.display().to_string()));
    let apple =
        format!("do shell script \"{}\" with administrator privileges", cmd.replace('\\', "\\\\").replace('"', "\\\""));
    let out = std::process::Command::new("/usr/bin/osascript").args(["-e", &apple]).output()?;
    let _ = std::fs::remove_file(&path);
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("-128") {
            anyhow::bail!("cancelled");
        }
        anyhow::bail!("{}", err.trim())
    }
}

#[cfg(target_os = "macos")]
fn uid() -> String {
    std::process::Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// Set up always-on sharing (Mac).
#[cfg(target_os = "macos")]
pub fn install() -> anyhow::Result<()> {
    let host = crate::procs::sibling("aa-host");
    anyhow::ensure!(host.is_absolute() && Path::new(&host).exists(), "aa-host was not found next to the app");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>--service</string></array>
  <key>LimitLoadToSessionType</key>
  <array><string>Aqua</string><string>LoginWindow</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#,
        xml_escape(&host.display().to_string())
    );
    let sys = aa_platform::trust::MAC_SYSTEM_DIR;
    let user = aa_platform::trust::user_dir();
    let script = format!(
        r#"set -e
D={sys}
U={user}
P={plist_path}
mkdir -p "$D"
# Keep this Mac's identity and pairings: the always-on copy is the same computer.
for f in host.key paired-viewers.json pair-code; do
  if [ ! -e "$D/$f" ] && [ -e "$U/$f" ]; then cp "$U/$f" "$D/$f"; fi
done
chown -R root:admin "$D"
chmod 0770 "$D"
find "$D" -type f -exec chmod 0660 {{}} +
cat > "$P" <<'PLIST'
{plist}PLIST
chown root:wheel "$P"
chmod 0644 "$P"
# Turn on by itself when power comes back; never fall asleep on its own.
pmset -a autorestart 1
pmset -a sleep 0
launchctl bootout gui/{uid}/{LABEL} 2>/dev/null || true
launchctl bootstrap gui/{uid} "$P" || true
"#,
        sys = shell_quote(sys),
        user = shell_quote(&user.display().to_string()),
        plist_path = shell_quote(&plist_path().display().to_string()),
        uid = uid(),
    );
    run_as_admin(&script)
}

/// Turn always-on sharing off again (Mac). Pairings are kept.
#[cfg(target_os = "macos")]
pub fn uninstall() -> anyhow::Result<()> {
    let script = format!(
        "launchctl bootout gui/{uid}/{LABEL} 2>/dev/null || true\nrm -f {p}\n",
        uid = uid(),
        p = shell_quote(&plist_path().display().to_string())
    );
    run_as_admin(&script)
}

#[cfg(not(target_os = "macos"))]
pub fn install() -> anyhow::Result<()> {
    anyhow::bail!("only on a Mac")
}

#[cfg(not(target_os = "macos"))]
pub fn uninstall() -> anyhow::Result<()> {
    anyhow::bail!("only on a Mac")
}

// ---------------------------------------------------------------------------
// Windows: start when I sign in
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(target_os = "windows")]
fn reg(args: &[&str]) -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("reg")
        .args(args)
        .creation_flags(0x0800_0000)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Does Windows start Anywhere at sign-in?
pub fn starts_at_sign_in() -> bool {
    #[cfg(target_os = "windows")]
    {
        reg(&["query", RUN_KEY, "/v", "Anywhere"])
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

pub fn set_start_at_sign_in(on: bool) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    {
        let ok = if on {
            let exe = std::env::current_exe()?;
            let value = format!("\"{}\" --background", exe.display());
            reg(&["add", RUN_KEY, "/v", "Anywhere", "/t", "REG_SZ", "/d", &value, "/f"])
        } else {
            reg(&["delete", RUN_KEY, "/v", "Anywhere", "/f"])
        };
        anyhow::ensure!(ok, "Windows did not accept the change");
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = on;
        anyhow::bail!("only on Windows")
    }
}
