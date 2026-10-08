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
//! **Windows.** The same idea with Windows' own tools: a background service
//! (it starts with Windows, before anyone signs in) runs the host on the
//! sign-in screen and on the signed-in desktop, so a paired computer can
//! sign in from afar. Setting it up needs one "allow changes?" prompt.
//! Separately, "Start when I sign in" opens the app minimised.

#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;

#[cfg(target_os = "macos")]
pub const LABEL: &str = "com.minutecreative.anywhere.host";

#[cfg(target_os = "macos")]
fn plist_path() -> PathBuf {
    PathBuf::from(format!("/Library/LaunchAgents/{LABEL}.plist"))
}

/// Is the always-on sharing set up on this computer?
pub fn installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        plist_path().exists()
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("sc")
            .args(["query", aa_platform::windows::session::SERVICE_NAME])
            .creation_flags(0x0800_0000)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

/// The log of the copy on the screen's session (what the Share page shows).
pub fn log_path() -> PathBuf {
    aa_platform::trust::system_dir().join("service.log")
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

// ---------------------------------------------------------------------------
// Windows: the sharing service
// ---------------------------------------------------------------------------

/// This user's security id ("S-1-5-21-…"), so the shared folder lets the
/// app (running as you, not as administrator) read the code and pairings.
#[cfg(target_os = "windows")]
fn user_sid() -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .creation_flags(0x0800_0000)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let sid = text.trim().rsplit(',').next()?.trim_matches('"').to_owned();
    sid.starts_with("S-1-").then_some(sid)
}

#[cfg(target_os = "windows")]
fn run_script_as_admin(name: &str, script: &str) -> anyhow::Result<()> {
    let path = aa_platform::trust::user_dir().join(name);
    // Windows' command interpreter reads scripts in the console code page;
    // keep it plain ASCII-safe by using CRLF and quoting every path.
    std::fs::write(&path, script.replace('\n', "\r\n"))?;
    let params = format!("/c call \"{}\"", path.display());
    let code = aa_platform::windows::session::run_elevated("cmd.exe", &params);
    let _ = std::fs::remove_file(&path);
    match code? {
        0 => Ok(()),
        c => anyhow::bail!("the setup step failed (code {c})"),
    }
}

/// Set up always-on sharing (Windows).
#[cfg(target_os = "windows")]
pub fn install() -> anyhow::Result<()> {
    let host = crate::procs::sibling("aa-host");
    anyhow::ensure!(host.is_absolute() && host.exists(), "aa-host.exe was not found next to the app");
    let sid = user_sid().ok_or_else(|| anyhow::anyhow!("could not read your Windows account id"))?;
    let name = aa_platform::windows::session::SERVICE_NAME;
    let user = aa_platform::trust::user_dir();
    let sys = aa_platform::trust::system_dir();
    let script = format!(
        r#"@echo off
set "D={sys}"
set "U={user}"
if not exist "%D%" mkdir "%D%"
rem Keep this PC's identity and pairings: the always-on copy is the same computer.
for %%f in (host.key paired-viewers.json pair-code) do if not exist "%D%\%%f" if exist "%U%\%%f" copy /y "%U%\%%f" "%D%\%%f" >nul
rem Only the system, administrators and you may read the keys.
icacls "%D%" /inheritance:r /grant:r *S-1-5-18:(OI)(CI)F *S-1-5-32-544:(OI)(CI)F *{sid}:(OI)(CI)M /T /Q >nul
sc query {name} >nul 2>&1
if errorlevel 1 goto create
sc stop {name} >nul 2>&1
sc config {name} binPath= "\"{host}\" --windows-service" start= auto >nul
goto configured
:create
sc create {name} binPath= "\"{host}\" --windows-service" start= auto DisplayName= "Anywhere sharing" >nul
:configured
if errorlevel 1 exit /b 2
sc description {name} "Lets your paired computers reach this PC, also at the sign-in screen." >nul
sc failure {name} reset= 60 actions= restart/5000/restart/5000/restart/5000 >nul
rem Never fall asleep while plugged in, even with the lid closed.
powercfg /change standby-timeout-ac 0
powercfg /change hibernate-timeout-ac 0
powercfg /setacvalueindex SCHEME_CURRENT SUB_BUTTONS LIDACTION 0
powercfg /setactive SCHEME_CURRENT
ping -n 3 127.0.0.1 >nul
sc start {name} >nul
exit /b 0
"#,
        sys = sys.display(),
        user = user.display(),
        host = host.display(),
    );
    run_script_as_admin("setup-sharing.cmd", &script)
}

/// Turn always-on sharing off again (Windows). Pairings are kept.
#[cfg(target_os = "windows")]
pub fn uninstall() -> anyhow::Result<()> {
    let name = aa_platform::windows::session::SERVICE_NAME;
    let script = format!(
        "@echo off\nsc stop {name} >nul 2>&1\nping -n 4 127.0.0.1 >nul\nsc delete {name} >nul 2>&1\nexit /b 0\n"
    );
    run_script_as_admin("remove-sharing.cmd", &script)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn install() -> anyhow::Result<()> {
    anyhow::bail!("only on a Mac or a PC")
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn uninstall() -> anyhow::Result<()> {
    anyhow::bail!("only on a Mac or a PC")
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
