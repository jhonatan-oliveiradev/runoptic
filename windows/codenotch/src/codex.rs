//! Codex usage adapter, implemented from the upstream Codenotch's documented behaviour.
//!
//! Two data paths (the same trade-off upstream made in 1.5.0):
//!   1. Live: borrow the session Codex keeps in `~/.codex/auth.json` (`tokens.access_token` +
//!      `tokens.account_id`) and GET `https://chatgpt.com/backend-api/wham/usage`. The reply carries
//!      `rate_limit.{primary_window,secondary_window}` with `used_percent / limit_window_seconds /
//!      reset_at (seconds) | reset_after_seconds`, plus a top-level `plan_type`. That is the number
//!      for *now*, and it starts no process. The token is read only — never refreshed, never written
//!      back; 401/403 becomes needsAuth and Codex renews it on its own.
//!      (The earlier `codex app-server` JSON-RPC route spawned a node process tree every five
//!      minutes, needed taskkill to clean up, and only ever reported the weekly window; the
//!      five-hour window came back with the endpoint.)
//!   2. Fallback: Codex writes the limits it saw on each turn into the thread's rollout log
//!      `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, as lines like
//!      `{"timestamp":"…","type":"event_msg","payload":{"type":"token_count","rate_limits":{
//!         "primary":{"used_percent":0.0,"window_minutes":300,"resets_at":1790585719},
//!         "secondary":{…}|null,"plan_type":"free"}}}`
//!      The reset is **resets_at, absolute seconds** (the documented resets_in_seconds is accepted
//!      too). This is the number from the *last run* — reading a file always succeeds instantly, so
//!      the reading is marked stale by the line's own timestamp (> 5 min).
//!      Upstream finds the newest rollout through the thread index in state_5.sqlite; this port
//!      walks the dated directories newest-first and picks by mtime, with no SQLite involved (and
//!      none of the immutable/WAL pitfalls).
//!
//! Credentials are borrowed, never managed: the numbers come from Codex's own sign-in and Codex's
//! own endpoint. No sign-in and no session history at all means absent (no cell is shown).

use crate::usage::{LimitWindow, UsageSnapshot};
use crate::AppState;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const POLL_SECS: u64 = 300; // Codex has no session state to key off, so a fixed 5 min (upstream cadence; a tray refresh interrupts it)
const TAIL_BYTES: u64 = 256 * 1024;
const CURRENT_FOR_MS: u64 = 5 * 60 * 1000;
const ENDPOINT: &str = "https://chatgpt.com/backend-api/wham/usage";
const BACKOFF_MIN_SECS: u64 = 60; // wait at least this long after a 429; Retry-After only raises it

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn request_refresh() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn codex_home() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex"))
}

/// A Codex configuration root can live in native Windows or in a resolved WSL environment.
/// Keep filesystem routing explicit so collectors never have to reinterpret `~` themselves.
fn auth_path_in(codex_home: &Path) -> PathBuf {
    codex_home.join("auth.json")
}

fn store_path() -> PathBuf {
    crate::config::config_path().with_file_name("codex.json")
}

fn persist(s: &UsageSnapshot) {
    if let Ok(t) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(store_path(), t);
    }
}

// ---------------- Locating the executable ----------------

/// Candidates in order: the native exe inside the global npm package (cleanest — no cmd/node
/// wrapper) → ~/.codex/bin → codex.exe / codex.cmd on PATH.
pub fn find_executable() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(appdata) = dirs::config_dir() {
        let pkg = appdata.join("npm").join("node_modules").join("@openai").join("codex");
        if let Ok(rd) = std::fs::read_dir(pkg.join("bin")) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_lowercase();
                if n.starts_with("codex-") && n.contains("windows") && n.ends_with(".exe") {
                    cands.push(e.path());
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(pkg.join("vendor")) {
            // Newer packages keep the native exe at vendor/<triple>/codex/codex.exe
            for e in rd.flatten() {
                let p = e.path().join("codex").join("codex.exe");
                if p.exists() {
                    cands.push(p);
                }
            }
        }
        cands.push(appdata.join("npm").join("codex.cmd"));
    }
    if let Some(h) = codex_home() {
        cands.push(h.join("bin").join("codex.exe"));
        cands.push(h.join("bin").join("codex"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            cands.push(dir.join("codex.exe"));
            cands.push(dir.join("codex.cmd"));
        }
    }
    cands.into_iter().find(|p| p.is_file())
}

// ---------------- Live: the usage endpoint ----------------

fn auth_path() -> Option<PathBuf> {
    codex_home().map(|h| auth_path_in(&h))
}

struct Credential {
    access_token: String,
    account_id: String,
    /// chatgpt_plan_type from the id_token (pro / plus / free…), used only as a label
    plan: Option<String>,
    /// The access_token's exp has passed: the request is still sent (the server decides); this only changes the 401 wording
    expired: bool,
}

/// Second JWT segment (base64url) → claims. Used only for labels and a local expiry hint; nothing is verified here — that is the server's job
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let part = token.split('.').nth(1)?;
    let raw = crate::antigravity::b64_decode(part)?;
    serde_json::from_slice(&raw).ok()
}

/// Reads Codex's sign-in state from one explicit configuration root; a missing file or missing
/// field both mean "not signed in". This is intentionally filesystem-only and never writes tokens.
fn load_credential_in(codex_home: &Path) -> Option<Credential> {
    let text = std::fs::read_to_string(auth_path_in(codex_home)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let tokens = v.get("tokens")?;
    let access_token = tokens.get("access_token")?.as_str()?.trim().to_string();
    let account_id = tokens.get("account_id")?.as_str()?.trim().to_string();
    if access_token.is_empty() || account_id.is_empty() {
        return None;
    }
    let expired = jwt_claims(&access_token)
        .and_then(|c| c.get("exp").and_then(|x| x.as_f64()))
        .map(|exp| exp * 1000.0 <= now_ms() as f64)
        .unwrap_or(false);
    let plan = tokens
        .get("id_token")
        .and_then(|x| x.as_str())
        .and_then(jwt_claims)
        .and_then(|c| {
            c.get("https://api.openai.com/auth")?
                .get("chatgpt_plan_type")?
                .as_str()
                .map(String::from)
        });
    Some(Credential { access_token, account_id, plan, expired })
}

fn load_credential() -> Option<Credential> {
    load_credential_in(&codex_home()?)
}

enum LiveErr {
    NeedsAuth,
    /// Suggested wait in seconds (BACKOFF_MIN_SECS already applied)
    RateLimited(u64),
    Other(String),
}

fn fetch_usage(cred: &Credential) -> Result<serde_json::Value, LiveErr> {
    let resp = ureq::get(ENDPOINT)
        .set("Authorization", &format!("Bearer {}", cred.access_token))
        .set("ChatGPT-Account-Id", &cred.account_id)
        .set("Accept", "application/json")
        .set("Cache-Control", "no-cache, no-store")
        .set("User-Agent", concat!("runoptic/", env!("CARGO_PKG_VERSION"), " (Windows)"))
        .timeout(Duration::from_secs(15))
        .call();
    match resp {
        Ok(r) => r.into_json().map_err(|e| LiveErr::Other(format!("parse: {e}"))),
        Err(ureq::Error::Status(code @ (401 | 403), r)) => {
            // 401 is about the token; 403 can also be an edge node rejecting the user agent — record the status and the start of the body rather than folding both into "please sign in"
            let head: String = r
                .into_string()
                .unwrap_or_default()
                .chars()
                .filter(|c| !c.is_control())
                .take(160)
                .collect();
            crate::applog(&format!("codex: usage endpoint HTTP {code}: {head}"));
            Err(LiveErr::NeedsAuth)
        }
        Err(ureq::Error::Status(429, r)) => {
            let ra = r.header("retry-after").and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
            Err(LiveErr::RateLimited(ra.max(BACKOFF_MIN_SECS)))
        }
        Err(ureq::Error::Status(code, _)) => Err(LiveErr::Other(format!("HTTP {code}"))),
        Err(e) => Err(LiveErr::Other(format!("{e}"))),
    }
}

/// Upstream's label rule: Codex names windows only by length, and "5h limit" says more than "primary"
fn label_for(window_minutes: Option<f64>, id: &str) -> String {
    match window_minutes {
        Some(m) if m > 0.0 => {
            if m < 60.0 {
                format!("{}m limit", m as i64)
            } else if m < 60.0 * 24.0 {
                format!("{}h limit", (m / 60.0) as i64)
            } else {
                let days = (m / (60.0 * 24.0)).round() as i64;
                match days {
                    7 => "Weekly limit".into(),
                    30 => "Monthly limit".into(),
                    d => format!("{d}d limit"),
                }
            }
        }
        _ => {
            if id == "primary" {
                "Current session".into()
            } else {
                "Longer window".into()
            }
        }
    }
}

fn num(v: Option<&serde_json::Value>) -> Option<f64> {
    v.and_then(|x| x.as_f64())
}

/// Seconds → ms. Negative / non-finite values are treated as missing so a
/// garbage extra cannot wrap `now + ms` (debug overflow panics).
fn secs_to_ms(s: f64) -> Option<u64> {
    if !s.is_finite() || s < 0.0 {
        None
    } else {
        Some((s * 1000.0) as u64)
    }
}

fn reset_at_ms(w: &serde_json::Value, now: u64, epoch_key: &str, delay_key: &str) -> Option<u64> {
    num(w.get(epoch_key))
        .and_then(secs_to_ms)
        .or_else(|| num(w.get(delay_key)).and_then(secs_to_ms).map(|ms| now.saturating_add(ms)))
}

/// Skip a window with no `used_percent`. Extra ids still pass primary/secondary to `label_for`.
fn window_from(
    w: &serde_json::Value,
    id: &str,
    fallback: &str,
    now: u64,
    group: Option<&str>,
) -> Option<LimitWindow> {
    if !w.is_object() {
        return None;
    }
    let pct = num(w.get("used_percent"))?;
    Some(LimitWindow {
        id: id.into(),
        label: label_for(num(w.get("limit_window_seconds")).map(|s| s / 60.0), fallback),
        used: (pct / 100.0).clamp(0.0, 1.0),
        resets_at: reset_at_ms(w, now, "reset_at", "reset_after_seconds"),
        group: group.map(str::to_string),
        ..Default::default()
    })
}

fn names_spark(extra: &serde_json::Value) -> bool {
    if !extra.is_object() {
        return false;
    }
    ["limit_name", "metered_feature"].iter().any(|key| {
        extra
            .get(*key)
            .and_then(|x| x.as_str())
            .is_some_and(|s| s.to_lowercase().contains("spark"))
    })
}

/// Spark / code review sit after the main pair so `windows.first` stays primary.
/// The group is what the hover card uses to box them; omitting it leaves them
/// as extra ungrouped bars under the main windows.
fn append_extra(
    rl: Option<&serde_json::Value>,
    primary_id: &str,
    secondary_id: &str,
    group: &str,
    now: u64,
    out: &mut Vec<LimitWindow>,
) {
    let Some(rl) = rl.filter(|x| x.is_object()) else {
        return;
    };
    if let Some(w) = rl
        .get("primary_window")
        .and_then(|x| window_from(x, primary_id, "primary", now, Some(group)))
    {
        push_unique(out, w);
    }
    if let Some(w) = rl
        .get("secondary_window")
        .and_then(|x| window_from(x, secondary_id, "secondary", now, Some(group)))
    {
        push_unique(out, w);
    }
}

fn push_unique(out: &mut Vec<LimitWindow>, window: LimitWindow) {
    if out.iter().any(|w| w.id == window.id) {
        return;
    }
    out.push(window);
}

/// Usage reply → windows. Primary and secondary feed the ring; Spark
/// (`additional_rate_limits`) and Code review (`code_review_rate_limit`) belong
/// on the hover card, not as extra rings. The window id records which field it
/// came from and the label is derived from the length — the primary window is
/// not always five hours (a free plan has shown 30 days), and recognising only
/// fixed lengths would drop a window that is genuinely in use.
fn windows_from_usage(v: &serde_json::Value) -> Vec<LimitWindow> {
    let now = now_ms();
    let mut out = Vec::new();
    for (id, key) in [("primary", "primary_window"), ("secondary", "secondary_window")] {
        if let Some(w) = v
            .pointer(&format!("/rate_limit/{key}"))
            .and_then(|x| window_from(x, id, id, now, None))
        {
            out.push(w);
        }
    }
    // A non-array (null, object, string) is the same as omitting the field —
    // one junk extra must not discard the main pair or a later Spark row.
    if let Some(extras) = v.get("additional_rate_limits").and_then(|x| x.as_array()) {
        for extra in extras {
            if !names_spark(extra) {
                continue;
            }
            append_extra(extra.get("rate_limit"), "spark", "spark-secondary", "Spark", now, &mut out);
        }
    }
    append_extra(
        v.get("code_review_rate_limit"),
        "code-review",
        "code-review-secondary",
        "Code review",
        now,
        &mut out,
    );
    out
}

// ---------------- Fallback: the rollout snapshot ----------------

/// The most recently modified rollout below one Codex configuration root: dated directories
/// newest-first, looking only at the three most recent days that have files.
fn newest_rollout_in(codex_home: &Path) -> Option<PathBuf> {
    let root = codex_home.join("sessions");
    let mut days: Vec<PathBuf> = Vec::new();
    let mut years = list_dirs(&root);
    years.sort_by(|a, b| b.cmp(a));
    'outer: for y in years {
        let mut months = list_dirs(&y);
        months.sort_by(|a, b| b.cmp(a));
        for m in months {
            let mut ds = list_dirs(&m);
            ds.sort_by(|a, b| b.cmp(a));
            for d in ds {
                days.push(d);
                if days.len() >= 3 {
                    break 'outer;
                }
            }
        }
    }
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for d in days {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
                    continue;
                }
                let Ok(md) = e.metadata() else { continue };
                let Ok(mt) = md.modified() else { continue };
                if best.as_ref().map(|(t, _)| mt > *t).unwrap_or(true) {
                    best = Some((mt, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

pub fn newest_rollout() -> Option<PathBuf> {
    newest_rollout_in(&codex_home()?)
}

fn list_dirs(p: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(p)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default()
}

pub fn tail_text(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)));
    let mut raw = Vec::new();
    f.read_to_end(&mut raw).ok()?;
    Some(String::from_utf8_lossy(&raw).into_owned())
}

/// The last rate_limits snapshot at the tail of a rollout → (windows, recorded-at ms, plan)
pub fn snapshot_from_rollout(text: &str) -> Option<(Vec<LimitWindow>, Option<u64>, Option<String>)> {
    for line in text.lines().rev().filter(|l| l.contains("rate_limits")) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        // rate_limits may sit at the top level or under payload
        let rl = v
            .get("rate_limits")
            .or_else(|| v.pointer("/payload/rate_limits"))
            .filter(|x| x.is_object());
        let Some(rl) = rl else { continue };
        let recorded = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp_millis().max(0) as u64);
        let now = now_ms();
        let mut out = Vec::new();
        for id in ["primary", "secondary"] {
            let Some(w) = rl.get(id).filter(|x| x.is_object()) else { continue };
            let Some(pct) = num(w.get("used_percent")) else { continue };
            out.push(LimitWindow {
                id: id.into(),
                label: label_for(num(w.get("window_minutes")), id),
                used: (pct / 100.0).clamp(0.0, 1.0),
                resets_at: reset_at_ms(w, now, "resets_at", "resets_in_seconds"),
                ..Default::default()
            });
        }
        if out.is_empty() {
            continue;
        }
        let plan = rl.get("plan_type").and_then(|x| x.as_str()).map(String::from);
        return Some((out, recorded, plan));
    }
    None
}

// ---------------- Putting it together ----------------

/// Is Codex present on this machine (CLI installed, signed in, or has had sessions)? If not, no cell is shown
pub fn present() -> bool {
    find_executable().is_some()
        || auth_path().map(|p| p.is_file()).unwrap_or(false)
        || codex_home().map(|h| h.join("sessions").is_dir()).unwrap_or(false)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProfileObservation {
    pub profile_key: String,
    pub environment_id: String,
    pub display_name: String,
    pub config_dir: PathBuf,
    pub auth_status: String,
    pub plan: Option<String>,
    pub credential_expired: bool,
    pub newest_rollout: Option<PathBuf>,
    pub snapshot: UsageSnapshot,
}

/// Read-only/local observation for a discovered Codex profile.
///
/// This deliberately does not hit the live usage endpoint yet. Each profile/account needs its own
/// polling/backoff state before live requests can be enabled safely.
pub fn observe_profile(profile: &crate::profile::ToolProfile) -> Option<ProfileObservation> {
    if profile.tool != crate::profile::ToolKind::Codex {
        return None;
    }

    let home = &profile.config_dir;
    let credential = load_credential_in(home);
    let auth_present = auth_path_in(home).is_file();
    let credential_expired = credential.as_ref().is_some_and(|c| c.expired);
    let plan = credential.as_ref().and_then(|c| c.plan.clone());
    let auth_status = match credential.as_ref() {
        Some(_) if credential_expired => "expired",
        Some(_) => "usable",
        None if auth_present => "invalid",
        None => "missing",
    }
    .to_string();

    let newest_rollout = newest_rollout_in(home);
    let mut snapshot = UsageSnapshot::default();

    match newest_rollout
        .as_ref()
        .and_then(|path| tail_text(path))
        .and_then(|text| snapshot_from_rollout(&text))
    {
        Some((windows, recorded, rollout_plan)) => {
            let recorded = recorded.unwrap_or(0);
            let fresh = recorded > 0 && now_ms().saturating_sub(recorded) <= CURRENT_FOR_MS;
            snapshot.status = if fresh { "ok" } else { "stale" }.into();
            snapshot.windows = windows;
            snapshot.fetched_at = recorded;
            snapshot.note = rollout_plan
                .or_else(|| plan.clone())
                .map(|p| format!("{} · local Codex rollout", cap(&p)))
                .unwrap_or_else(|| "local Codex rollout".into());
            if credential_expired {
                snapshot.note = format!("Credential expired · {}", snapshot.note);
            }
        }
        None => {
            snapshot.status = match auth_status.as_str() {
                "missing" | "invalid" => "needsAuth",
                _ => "none",
            }
            .into();
            snapshot.note = match auth_status.as_str() {
                "expired" => "Codex credential expired; no local usage snapshot found".into(),
                "missing" => "No Codex credential or local usage snapshot found".into(),
                "invalid" => "Codex auth.json is present but unusable".into(),
                _ => "Codex has not recorded a local usage snapshot yet".into(),
            };
        }
    }

    Some(ProfileObservation {
        profile_key: profile.key.clone(),
        environment_id: profile.environment_id.clone(),
        display_name: profile.display_name.clone(),
        config_dir: home.clone(),
        auth_status,
        plan,
        credential_expired,
        newest_rollout,
        snapshot,
    })
}

pub fn observe_profiles(profiles: &[crate::profile::ToolProfile]) -> Vec<ProfileObservation> {
    profiles.iter().filter_map(observe_profile).collect()
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct AccountGroup {
    /// Opaque public identity derived from source provenance, never from the vendor account id.
    pub key: String,
    pub profile_keys: Vec<String>,
    pub selected_profile_key: String,
    pub plan: Option<String>,
    pub credential_state: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct AccountUsage {
    pub key: String,
    pub profile_keys: Vec<String>,
    pub selected_profile_key: String,
    pub plan: Option<String>,
    /// live | local | none
    pub source_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_profile_key: Option<String>,
    pub snapshot: UsageSnapshot,
}

fn account_store_path() -> PathBuf {
    crate::config::config_path().with_file_name("codex-accounts.json")
}

pub fn load_account_usage() -> Vec<AccountUsage> {
    std::fs::read_to_string(account_store_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<AccountUsage>>(&text).ok())
        .map(|mut items| {
            for item in &mut items {
                if !item.snapshot.windows.is_empty() {
                    item.snapshot.status = "stale".into();
                }
            }
            items
        })
        .unwrap_or_default()
}

pub fn bootstrap_legacy_snapshot(accounts: &[AccountUsage]) -> UsageSnapshot {
    let mut snapshot = accounts
        .first()
        .map(|account| account.snapshot.clone())
        .unwrap_or_default();

    if !snapshot.windows.is_empty() {
        snapshot.status = "stale".into();
        snapshot.note = if snapshot.note.is_empty() {
            "Cached Codex account snapshot".into()
        } else {
            format!("Cached · {}", snapshot.note)
        };
    }
    snapshot
}

fn persist_account_usage(items: &[AccountUsage]) {
    if let Ok(text) = serde_json::to_string_pretty(items) {
        let _ = std::fs::write(account_store_path(), text);
    }
}

/// Group authenticated Codex profiles by vendor account id without ever serializing/logging that id.
///
/// The lexicographically first source provides the public group key. A non-expired credential is
/// preferred as the future live-poll source; otherwise the first source is retained for diagnostics.
pub fn group_accounts(profiles: &[crate::profile::ToolProfile]) -> Vec<AccountGroup> {
    use std::collections::BTreeMap;

    struct Member {
        profile_key: String,
        plan: Option<String>,
        expired: bool,
    }

    let mut by_account: BTreeMap<String, Vec<Member>> = BTreeMap::new();

    for profile in profiles.iter().filter(|p| p.tool == crate::profile::ToolKind::Codex) {
        let Some(credential) = load_credential_in(&profile.config_dir) else {
            continue;
        };
        by_account.entry(credential.account_id).or_default().push(Member {
            profile_key: profile.key.clone(),
            plan: credential.plan,
            expired: credential.expired,
        });
    }

    let mut groups = Vec::new();
    for (_, mut members) in by_account {
        members.sort_by(|a, b| a.profile_key.cmp(&b.profile_key));
        let profile_keys = members.iter().map(|m| m.profile_key.clone()).collect::<Vec<_>>();
        let selected = members
            .iter()
            .find(|m| !m.expired)
            .unwrap_or(&members[0]);
        let key = profile_keys[0].clone();
        let plan = selected
            .plan
            .clone()
            .or_else(|| members.iter().find_map(|m| m.plan.clone()));
        let credential_state = if members.iter().any(|m| !m.expired) {
            "usable"
        } else {
            "expired"
        }
        .to_string();

        groups.push(AccountGroup {
            key,
            profile_keys,
            selected_profile_key: selected.profile_key.clone(),
            plan,
            credential_state,
        });
    }

    groups.sort_by(|a, b| a.key.cmp(&b.key));
    groups
}

fn best_local_observation<'a>(
    group: &AccountGroup,
    observations: &'a [ProfileObservation],
) -> Option<&'a ProfileObservation> {
    observations
        .iter()
        .filter(|obs| group.profile_keys.iter().any(|key| key == &obs.profile_key))
        .max_by(|a, b| {
            let a_has = !a.snapshot.windows.is_empty();
            let b_has = !b.snapshot.windows.is_empty();
            a_has
                .cmp(&b_has)
                .then(a.snapshot.fetched_at.cmp(&b.snapshot.fetched_at))
        })
}

fn poll_account(
    group: &AccountGroup,
    profiles: &[crate::profile::ToolProfile],
    observations: &[ProfileObservation],
    previous: Option<&AccountUsage>,
) -> AccountUsage {
    let now = now_ms();
    let selected = profiles.iter().find(|p| p.key == group.selected_profile_key);
    let local = best_local_observation(group, observations);

    let local_snapshot = |note: Option<String>| {
        let mut snapshot = local.map(|obs| obs.snapshot.clone()).unwrap_or_default();
        if let Some(note) = note {
            snapshot.note = if snapshot.note.is_empty() {
                note
            } else {
                format!("{note} · {}", snapshot.note)
            };
        }
        (
            local.map(|obs| obs.profile_key.clone()),
            if snapshot.windows.is_empty() { "none" } else { "local" }.to_string(),
            snapshot,
        )
    };

    let held_until = previous.map(|p| p.snapshot.backoff_until).unwrap_or(0);
    let (source_profile_key, source_kind, mut snapshot) = if held_until > now {
        let seconds = held_until.saturating_sub(now) / 1000;
        let (source, kind, mut snapshot) =
            local_snapshot(Some(format!("Rate limited — retrying in {seconds}s")));
        snapshot.backoff_until = held_until;
        (source, kind, snapshot)
    } else if let Some(profile) = selected {
        match load_credential_in(&profile.config_dir) {
            Some(credential) if credential.expired => local_snapshot(Some(
                "Codex credential expired — open Codex once to refresh it".into(),
            )),
            Some(credential) => match fetch_usage(&credential) {
                Ok(value) => {
                    let windows = windows_from_usage(&value);
                    if windows.is_empty() {
                        local_snapshot(Some("Codex reported no usage windows".into()))
                    } else {
                        let plan = value
                            .get("plan_type")
                            .and_then(|x| x.as_str())
                            .map(String::from)
                            .or_else(|| credential.plan.clone());
                        (
                            Some(profile.key.clone()),
                            "live".into(),
                            UsageSnapshot {
                                status: "ok".into(),
                                windows,
                                fetched_at: now_ms(),
                                note: plan
                                    .map(|p| format!("{} · via Codex", cap(&p)))
                                    .unwrap_or_default(),
                                ..Default::default()
                            },
                        )
                    }
                }
                Err(LiveErr::NeedsAuth) => local_snapshot(Some(
                    "Codex rejected its sign-in — sign in to Codex again".into(),
                )),
                Err(LiveErr::RateLimited(seconds)) => {
                    let until = now_ms() + seconds * 1000;
                    let (source, kind, mut snapshot) =
                        local_snapshot(Some(format!("Rate limited — retrying in {seconds}s")));
                    snapshot.backoff_until = until;
                    (source, kind, snapshot)
                }
                Err(LiveErr::Other(error)) => {
                    local_snapshot(Some(format!("Live read failed ({error})")))
                }
            },
            None => local_snapshot(Some("Codex credential is unavailable".into())),
        }
    } else {
        local_snapshot(Some("Selected Codex profile is unavailable".into()))
    };

    // Preserve an account-specific server backoff even when a local fallback has an older snapshot.
    if snapshot.backoff_until == 0 && held_until > now {
        snapshot.backoff_until = held_until;
    }

    AccountUsage {
        key: group.key.clone(),
        profile_keys: group.profile_keys.clone(),
        selected_profile_key: group.selected_profile_key.clone(),
        plan: group.plan.clone(),
        source_kind,
        source_profile_key,
        snapshot,
    }
}

fn best_legacy_snapshot(
    accounts: &[AccountUsage],
    observations: &[ProfileObservation],
) -> UsageSnapshot {
    if let Some(first) = accounts.first() {
        let mut snapshot = first.snapshot.clone();
        if accounts.len() > 1 {
            let suffix = format!("{} Codex accounts detected", accounts.len());
            snapshot.note = if snapshot.note.is_empty() {
                suffix
            } else {
                format!("{} · {suffix}", snapshot.note)
            };
        }
        return snapshot;
    }

    observations
        .iter()
        .max_by_key(|obs| (!obs.snapshot.windows.is_empty(), obs.snapshot.fetched_at))
        .map(|obs| obs.snapshot.clone())
        .unwrap_or_default()
}

fn broadcast_account_usage(
    app: &AppHandle,
    groups: Vec<AccountGroup>,
    accounts: Vec<AccountUsage>,
    observations: Vec<ProfileObservation>,
) {
    let legacy = best_legacy_snapshot(&accounts, &observations);
    {
        let state = app.state::<AppState>();
        *state.codex_accounts.lock().unwrap() = groups.clone();
        *state.codex_account_usage.lock().unwrap() = accounts.clone();
        *state.codex_profiles.lock().unwrap() = observations.clone();
        *state.codex.lock().unwrap() = legacy.clone();
    }

    persist_account_usage(&accounts);
    persist(&legacy);

    let _ = app.emit("codex_accounts", &groups);
    let _ = app.emit("codex_account_usage", &accounts);
    let _ = app.emit("codex_profiles", &observations);
    let _ = app.emit("codex", &legacy);
}

/// Poll quota once per vendor account, not once per installation/profile.
///
/// Profiles are rescanned locally each cycle so a refreshed token is picked up without restarting
/// RunOptic. Profile discovery itself remains launch-scoped in this gate.
pub fn start_accounts(app: AppHandle, profiles: Vec<crate::profile::ToolProfile>) {
    std::thread::spawn(move || {
        let mut previous = load_account_usage();

        loop {
            let observations = observe_profiles(&profiles);
            let groups = group_accounts(&profiles);
            let accounts = groups
                .iter()
                .map(|group| {
                    let prior = previous.iter().find(|item| item.key == group.key);
                    poll_account(group, &profiles, &observations, prior)
                })
                .collect::<Vec<_>>();

            broadcast_account_usage(&app, groups, accounts.clone(), observations);
            previous = accounts;

            for _ in 0..POLL_SECS {
                if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn windows(json: &str) -> Vec<LimitWindow> {
        windows_from_usage(&serde_json::from_str(json).unwrap())
    }

    fn ids(ws: &[LimitWindow]) -> Vec<&str> {
        ws.iter().map(|w| w.id.as_str()).collect()
    }

    fn labels(ws: &[LimitWindow]) -> Vec<&str> {
        ws.iter().map(|w| w.label.as_str()).collect()
    }

    fn groups(ws: &[LimitWindow]) -> Vec<Option<&str>> {
        ws.iter().map(|w| w.group.as_deref()).collect()
    }

    fn b64url_no_pad(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        let mut i = 0;
        while i < bytes.len() {
            let b0 = bytes[i];
            let b1 = bytes.get(i + 1).copied();
            let b2 = bytes.get(i + 2).copied();

            out.push(TABLE[(b0 >> 2) as usize] as char);
            out.push(TABLE[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
            if let Some(b1) = b1 {
                out.push(TABLE[(((b1 & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char);
            }
            if let Some(b2) = b2 {
                out.push(TABLE[(b2 & 0x3f) as usize] as char);
            }
            i += 3;
        }
        out
    }

    fn write_codex_auth(root: &Path, account: &str, exp: u64) {
        std::fs::create_dir_all(root).unwrap();
        let payload = format!(r#"{{"exp":{exp}}}"#);
        let payload = b64url_no_pad(payload.as_bytes());
        let token = format!("header.{payload}.signature");
        let body = serde_json::json!({
            "tokens": {
                "access_token": token,
                "account_id": account
            }
        });
        std::fs::write(auth_path_in(root), serde_json::to_vec(&body).unwrap()).unwrap();
    }

    fn codex_profile(key: &str, root: PathBuf) -> crate::profile::ToolProfile {
        crate::profile::ToolProfile {
            key: key.into(),
            tool: crate::profile::ToolKind::Codex,
            environment_id: key.split('/').next().unwrap_or("env").into(),
            display_name: "Codex".into(),
            config_dir: root,
            slug: None,
            default_profile: true,
        }
    }

    #[test]
    fn expired_account_uses_best_local_profile_without_network() {
        let base = std::env::temp_dir().join(format!("runoptic-codex-expired-local-{}", now_ms()));
        let windows = base.join("windows");
        let wsl = base.join("wsl");
        let past = (now_ms() / 1000).saturating_sub(3600);
        write_codex_auth(&windows, "same-account", past);
        write_codex_auth(&wsl, "same-account", past);

        let day = wsl.join("sessions").join("2026").join("09").join("21");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join("rollout-test.jsonl"),
            r#"{"timestamp":"2026-09-21T12:00:00Z","rate_limits":{"primary":{"used_percent":25,"window_minutes":300},"secondary":{"used_percent":10,"window_minutes":10080}}}"#,
        )
        .unwrap();

        let profiles = vec![
            codex_profile("windows-native/codex", windows),
            codex_profile("wsl:ubuntu/codex", wsl),
        ];
        let observations = observe_profiles(&profiles);
        let groups = group_accounts(&profiles);
        let usage = poll_account(&groups[0], &profiles, &observations, None);

        assert_eq!(usage.source_kind, "local");
        assert_eq!(usage.source_profile_key.as_deref(), Some("wsl:ubuntu/codex"));
        assert_eq!(usage.snapshot.windows.len(), 2);
        assert!(usage.snapshot.note.contains("credential expired"));

        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn same_account_across_windows_and_wsl_becomes_one_quota_group() {
        let base = std::env::temp_dir().join(format!("runoptic-codex-account-group-{}", now_ms()));
        let windows = base.join("windows");
        let wsl = base.join("wsl");
        let future = (now_ms() / 1000) + 3600;
        write_codex_auth(&windows, "same-account", future);
        write_codex_auth(&wsl, "same-account", future);

        let profiles = vec![
            codex_profile("windows-native/codex", windows),
            codex_profile("wsl:ubuntu/codex", wsl),
        ];
        let groups = group_accounts(&profiles);

        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].profile_keys,
            vec!["windows-native/codex", "wsl:ubuntu/codex"]
        );
        assert_eq!(groups[0].credential_state, "usable");
        assert_eq!(groups[0].selected_profile_key, "windows-native/codex");

        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn account_group_prefers_a_non_expired_source() {
        let base = std::env::temp_dir().join(format!("runoptic-codex-account-preference-{}", now_ms()));
        let windows = base.join("windows");
        let wsl = base.join("wsl");
        let past = (now_ms() / 1000).saturating_sub(3600);
        let future = (now_ms() / 1000) + 3600;
        write_codex_auth(&windows, "same-account", past);
        write_codex_auth(&wsl, "same-account", future);

        let profiles = vec![
            codex_profile("windows-native/codex", windows),
            codex_profile("wsl:ubuntu/codex", wsl),
        ];
        let groups = group_accounts(&profiles);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].selected_profile_key, "wsl:ubuntu/codex");
        assert_eq!(groups[0].credential_state, "usable");

        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn profile_observation_reads_rollout_from_explicit_home() {
        let root = std::env::temp_dir().join(format!("runoptic-codex-observation-{}", now_ms()));
        let day = root.join("sessions").join("2026").join("09").join("21");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join("rollout-test.jsonl"),
            r#"{"timestamp":"2026-09-21T12:00:00Z","rate_limits":{"primary":{"used_percent":25,"window_minutes":300}}}"#,
        )
        .unwrap();

        let profile = crate::profile::ToolProfile {
            key: "wsl:ubuntu/codex".into(),
            tool: crate::profile::ToolKind::Codex,
            environment_id: "wsl:ubuntu".into(),
            display_name: "Codex".into(),
            config_dir: root.clone(),
            slug: None,
            default_profile: true,
        };

        let obs = observe_profile(&profile).expect("Codex profile should produce an observation");
        assert_eq!(obs.profile_key, "wsl:ubuntu/codex");
        assert_eq!(obs.snapshot.windows.len(), 1);
        assert_eq!(obs.snapshot.windows[0].label, "5h limit");

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn explicit_codex_home_routes_auth_without_native_home() {
        let root = std::env::temp_dir().join(format!(
            "runoptic-codex-source-{}",
            now_ms()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            auth_path_in(&root),
            r#"{"tokens":{"access_token":"header.eyJleHAiOjQxMDI0NDQ4MDB9.signature","account_id":"test"}}"#,
        )
        .unwrap();

        let credential = load_credential_in(&root).expect("explicit Codex root should be readable");
        assert_eq!(credential.account_id, "test");

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn extra_spark_and_code_review_follow_primary_secondary() {
        let ws = windows(
            r#"{
            "rate_limit":{
              "primary_window":{"used_percent":25,"limit_window_seconds":18000,"reset_at":1800001000},
              "secondary_window":{"used_percent":10,"limit_window_seconds":604800,"reset_at":1800600000}},
            "additional_rate_limits":[{"limit_name":"Spark","rate_limit":{
              "primary_window":{"used_percent":99,"limit_window_seconds":18000}}}],
            "code_review_rate_limit":{"primary_window":{"used_percent":90,"limit_window_seconds":604800}},
            "credits":{"balance":"100"},"model_usage":{"spark":99}
        }"#,
        );
        assert_eq!(ids(&ws), ["primary", "secondary", "spark", "code-review"]);
        assert_eq!(labels(&ws), ["5h limit", "Weekly limit", "5h limit", "Weekly limit"]);
        assert_eq!(groups(&ws), [None, None, Some("Spark"), Some("Code review")]);
        assert!((ws[0].used - 0.25).abs() < 1e-9);
        assert!((ws[2].used - 0.99).abs() < 1e-9);
        assert!((ws[3].used - 0.90).abs() < 1e-9);
        assert_eq!(ws[0].resets_at, Some(1_800_001_000_000));
    }

    #[test]
    fn spark_matches_limit_name_or_metered_feature_case_insensitively() {
        // "GPT-5.3-Codex-Spark" still contains the substring "Spark", so it
        // would pass a case-sensitive contains("Spark"). SPARK / spark would not.
        for (field, name) in [
            ("limit_name", "SPARK"),
            ("limit_name", "spark"),
            ("metered_feature", "GPT-5.3-Codex-SPARK"),
            ("metered_feature", "gpt-5.3-codex-spark"),
        ] {
            let ws = windows(&format!(
                r#"{{
                "rate_limit":{{"primary_window":{{"used_percent":1,"limit_window_seconds":18000}}}},
                "additional_rate_limits":[{{"{field}":"{name}","rate_limit":{{
                  "primary_window":{{"used_percent":40,"limit_window_seconds":18000}},
                  "secondary_window":{{"used_percent":5,"limit_window_seconds":604800}}}}}}]
            }}"#
            ));
            assert_eq!(ids(&ws), ["primary", "spark", "spark-secondary"], "{field}={name}");
            assert_eq!(groups(&ws)[1..], [Some("Spark"), Some("Spark")], "{field}={name}");
            assert_eq!(labels(&ws)[1..], ["5h limit", "Weekly limit"], "{field}={name}");
        }
    }

    #[test]
    fn extras_alone_are_still_a_reading() {
        let ws = windows(
            r#"{"additional_rate_limits":[{"limit_name":"Spark","rate_limit":{
              "primary_window":{"used_percent":40,"limit_window_seconds":18000},
              "secondary_window":{"used_percent":70,"limit_window_seconds":604800}}}]}"#,
        );
        assert_eq!(ids(&ws), ["spark", "spark-secondary"]);
        assert_eq!(groups(&ws), [Some("Spark"), Some("Spark")]);
        assert_eq!(labels(&ws), ["5h limit", "Weekly limit"]);
        assert!((ws[0].used - 0.40).abs() < 1e-9);
        assert!((ws[1].used - 0.70).abs() < 1e-9);
    }

    #[test]
    fn non_spark_additional_limits_are_ignored() {
        let ws = windows(
            r#"{
            "rate_limit":{"primary_window":{"used_percent":1,"limit_window_seconds":18000}},
            "additional_rate_limits":[{"limit_name":"codex_other","metered_feature":"codex_other","rate_limit":{
              "primary_window":{"used_percent":70,"limit_window_seconds":3600}}}]
        }"#,
        );
        assert_eq!(ids(&ws), ["primary"]);
    }

    #[test]
    fn empty_additional_rate_limits_leave_the_main_windows() {
        let ws = windows(
            r#"{
            "rate_limit":{
              "primary_window":{"used_percent":25,"limit_window_seconds":18000},
              "secondary_window":{"used_percent":10,"limit_window_seconds":604800}},
            "additional_rate_limits":[]
        }"#,
        );
        assert_eq!(ids(&ws), ["primary", "secondary"]);
        assert_eq!(groups(&ws), [None, None]);
    }

    #[test]
    fn extras_without_used_percent_are_skipped() {
        let ws = windows(
            r#"{
            "rate_limit":{"primary_window":{"used_percent":1,"limit_window_seconds":18000}},
            "additional_rate_limits":[{"limit_name":"Spark","rate_limit":{
              "primary_window":{"used_percent":null,"limit_window_seconds":18000},
              "secondary_window":{"used_percent":12,"limit_window_seconds":604800}}}],
            "code_review_rate_limit":{
              "primary_window":{"limit_window_seconds":604800},
              "secondary_window":{"used_percent":8,"limit_window_seconds":18000}}
        }"#,
        );
        assert_eq!(ids(&ws), ["primary", "spark-secondary", "code-review-secondary"]);
        assert_eq!(groups(&ws)[1..], [Some("Spark"), Some("Code review")]);
        assert_eq!(ws[1].label, "Weekly limit");
        assert_eq!(ws[2].label, "5h limit");
    }

    #[test]
    fn malformed_extras_do_not_drop_the_main_windows() {
        let ws = windows(
            r#"{
            "rate_limit":{"primary_window":{"used_percent":25,"limit_window_seconds":18000}},
            "additional_rate_limits":[
              "nope",
              42,
              null,
              {"limit_name":"Spark"},
              {"limit_name":"Spark","rate_limit":"nope"},
              {"limit_name":"Spark","rate_limit":{"primary_window":{
                "used_percent":40,"limit_window_seconds":18000,"reset_after_seconds":1e20}}},
              {"limit_name":"Spark","rate_limit":{"primary_window":{
                "used_percent":15,"limit_window_seconds":18000}}}
            ],
            "code_review_rate_limit":"nope"
        }"#,
        );
        assert_eq!(ids(&ws), ["primary", "spark"]);
        assert_eq!(groups(&ws), [None, Some("Spark")]);
        assert!((ws[1].used - 0.40).abs() < 1e-9);
        assert!(ws[1].resets_at.is_some());
    }

    #[test]
    fn two_spark_extras_do_not_duplicate_window_ids() {
        let ws = windows(
            r#"{
            "rate_limit":{
              "primary_window":{"used_percent":25,"limit_window_seconds":18000},
              "secondary_window":{"used_percent":10,"limit_window_seconds":604800}},
            "additional_rate_limits":[
              {"limit_name":"Spark","rate_limit":{
                "primary_window":{"used_percent":40,"limit_window_seconds":18000}}},
              {"limit_name":"GPT-5.3-Codex-Spark","metered_feature":"spark","rate_limit":{
                "primary_window":{"used_percent":99,"limit_window_seconds":18000},
                "secondary_window":{"used_percent":12,"limit_window_seconds":604800}}}
            ]
        }"#,
        );
        assert_eq!(ids(&ws), ["primary", "secondary", "spark", "spark-secondary"]);
        assert_eq!(groups(&ws), [None, None, Some("Spark"), Some("Spark")]);
        assert_eq!(labels(&ws), ["5h limit", "Weekly limit", "5h limit", "Weekly limit"]);
        assert!((ws[2].used - 0.40).abs() < 1e-9);
        assert!((ws[3].used - 0.12).abs() < 1e-9);
    }

    #[test]
    fn a_non_array_additional_rate_limits_is_ignored() {
        for extras in [
            r#"{"x":{"limit_name":"Spark","rate_limit":{"primary_window":{"used_percent":9,"limit_window_seconds":18000}}}}"#,
            r#""nope""#,
            "null",
        ] {
            let ws = windows(&format!(
                r#"{{"rate_limit":{{"primary_window":{{"used_percent":1,"limit_window_seconds":18000}}}},"additional_rate_limits":{extras}}}"#
            ));
            assert_eq!(ids(&ws), ["primary"], "{extras}");
        }
    }

    #[test]
    fn a_monthly_primary_window_is_not_dropped() {
        let ws = windows(
            r#"{"rate_limit":{"primary_window":{"used_percent":16,"limit_window_seconds":2592000,
            "reset_after_seconds":1838382,"reset_at":1790585722},"secondary_window":null},
             "plan_type":"free"}"#,
        );
        assert_eq!(ids(&ws), ["primary"]);
        assert_eq!(ws[0].label, "Monthly limit");
        assert!((ws[0].used - 0.16).abs() < 1e-4);
    }
}
