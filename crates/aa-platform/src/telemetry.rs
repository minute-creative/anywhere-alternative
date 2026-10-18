#![allow(clippy::doc_markdown)] // PostHog reads better plain
//! Anonymous usage numbers, sent only with the person's permission.
//!
//! In plain words: so we know how many people use Anywhere and whether it
//! works for them, each copy can send a few small notes to PostHog (an
//! analytics service): "this copy was opened today", and at the end of a
//! session how long it lasted and how often it froze or reconnected. Each
//! copy has a random ID; no names, computer names, addresses, screen
//! contents or keys are ever sent.
//!
//! Permission is a small file, `usage-stats`, next to the settings: "on" or
//! "off", written by the switch at the bottom of the app's Settings page.
//! With no file it counts as on (the owner's choice: on by default, easy to
//! switch off; it used to be a first-open card, moved to Settings in 0.5.10).
//!
//! Sending uses the `curl` that ships with Windows and macOS, started in the
//! background, so it never slows anything down and a failure costs nothing.
//! `AA_NO_TELEMETRY=1` switches it off completely (tests, mock runs).

use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;

use serde_json::{json, Map, Value};

/// The project's "write only" key: it can add numbers, never read them, so
/// it is safe inside the app.
const KEY: &str = "phc_yfU3zyEvTbzMsRyqfVnevFjsNiKzfXmgvmrZPAVodWCe";
/// PostHog's US region (the owner's project is there).
const URL: &str = "https://us.i.posthog.com/i/v0/e/";

fn consent_file() -> PathBuf {
    crate::trust::user_dir().join("usage-stats")
}

/// What the person chose: `Some(true)` on, `Some(false)` off, `None` not
/// asked yet.
pub fn consent() -> Option<bool> {
    match std::fs::read_to_string(consent_file()).ok()?.trim() {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// Whether notes are sent: on unless the person switched it off.
pub fn sending() -> bool {
    consent() != Some(false)
}

/// Save the person's choice (the Settings switch).
pub fn set_consent(on: bool) {
    let _ = std::fs::write(consent_file(), if on { "on" } else { "off" });
}

/// This copy's random ID, made on first use. Random, so it says nothing
/// about the person or the computer.
fn install_id() -> String {
    let path = crate::trust::user_dir().join("install-id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim();
        if id.len() >= 16 {
            return id.to_owned();
        }
    }
    let id: String =
        aa_core::secure::to_hex(&aa_core::secure::Identity::generate().public()).chars().take(32).collect();
    let _ = std::fs::write(&path, &id);
    id
}

/// Send one note, if allowed. `props` are added to the version and system,
/// which every note carries.
pub fn send(event: &str, props: &[(&str, Value)]) {
    if !sending() || std::env::var_os("AA_NO_TELEMETRY").is_some() {
        return;
    }
    let mut properties = Map::new();
    properties.insert("version".into(), json!(crate::update::current()));
    properties.insert("os".into(), json!(std::env::consts::OS));
    // No person profiles, no location lookup from the network address.
    properties.insert("$process_person_profile".into(), json!(false));
    properties.insert("$geoip_disable".into(), json!(true));
    for (k, v) in props {
        properties.insert((*k).to_owned(), v.clone());
    }
    let body = json!({
        "api_key": KEY,
        "event": event,
        "distinct_id": install_id(),
        "properties": properties,
    })
    .to_string();
    // In the background: the note may still be on its way after this
    // program has closed, which is fine.
    let child = crate::update::curl()
        .args(["--max-time", "10", "-X", "POST", "-H", "Content-Type: application/json", "--data-binary", "@-", URL])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut c) = child {
        if let Some(mut stdin) = c.stdin.take() {
            let _ = stdin.write_all(body.as_bytes());
        }
        // Collect it when it finishes, so no finished process lingers.
        let _ = std::thread::Builder::new().name("aa-usage".into()).spawn(move || {
            let _ = c.wait();
        });
    }
}

/// "This copy was opened" at most once a day (the daily-users count).
pub fn opened_today(props: &[(&str, Value)]) {
    let path = crate::trust::user_dir().join("usage-last-day");
    let today = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
        / 86_400)
        .to_string();
    if std::fs::read_to_string(&path).is_ok_and(|d| d.trim() == today) {
        return;
    }
    if sending() {
        let _ = std::fs::write(&path, &today);
        send("app_opened", props);
    }
}
