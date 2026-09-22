//! Claude usage adapter (official), implemented from the upstream Codenotch's documented behaviour.
//! Endpoint: GET https://api.anthropic.com/api/oauth/usage
//! Headers: Authorization: Bearer <token>; anthropic-beta: oauth-2025-04-20; 15 s timeout
//! Rules (upstream's discipline):
//!   - the credential comes from Claude Code's own store (Windows: ~/.claude/.credentials.json), read only
//!   - 401/403 → re-read the credential once and retry (Claude Code may have just refreshed the token) → still failing means needsAuth
//!   - 429 → back off 60 s × 2^n capped at 15 min, Retry-After only raises it, even past the cap; the deadline is persisted
//!   - an expired token is never sent: the endpoint answers it with 429 + Retry-After ≈ 3600, not 401, so sending it
//!     reads as "rate limited" for as long as the token stays stale (upstream's credentialExpired, no network)
//!   - the token is renewed by running the standalone `claude -p` with an empty stdin shortly before it expires
//!     (upstream's ClaudeTokenRefresher). Only that CLI writes ~/.claude/.credentials.json — Claude Code inside the
//!     desktop app renews its own copy elsewhere — so without this the file rots eight hours after the last CLI run
//!   - never invent a percentage on failure: keep the last reading marked stale, and the UI shows how old it is
//!
//! Reply (snake_case): { limits:[{kind,percent,resets_at}], five_hour:{utilization,resets_at}, seven_day:{...} }
//! limits is the forward-compatible main shape; five_hour/seven_day are merged in as a fallback (a window that just rolled over disappears from limits).

use crate::AppState;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const POLL_ACTIVE_SECS: u64 = 60;
const POLL_IDLE_SECS: u64 = 300;
const BACKOFF_BASE_SECS: u64 = 60;
const BACKOFF_CAP_SECS: u64 = 900;
/// Renew when this close to expiry. Must stay under Claude Code's own five minutes: its start-up renews the token
/// only when now + 300 s >= expiresAt, so launching any earlier is a no-op that would be judged a failure
const RENEW_MARGIN_MS: u64 = 4 * 60 * 1000;
const RENEW_COOLDOWN_MS: u64 = 10 * 60 * 1000;
/// A token that did not renew is tried again, each wait twice the last, never more than an hour apart
const RENEW_RETRY_CAP_MS: u64 = 60 * 60 * 1000;
const RENEW_TIMEOUT_SECS: u64 = 30;
const EXPIRED_NOTE: &str = "Credential expired — run claude once in a terminal to renew it";

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Immediate refresh from the tray or a command
pub fn request_refresh() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Sleep in slices so request_refresh can interrupt it
fn sleep_interruptible(total_secs: u64) {
    for _ in 0..total_secs {
        if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LimitWindow {
    pub id: String,
    pub label: String,
    /// 0.0–1.0 (fraction used)
    pub used: f64,
    /// Reset time, ms epoch (None = unknown)
    pub resets_at: Option<u64>,
    /// Pure count window (no published denominator, e.g. Antigravity's requests today) — the cell shows ~N and the ring draws only its track
    #[serde(default)]
    pub count: Option<i64>,
    /// The number is ours, not the vendor's (upstream fidelity=.derived) — the card adds a ~ prefix
    #[serde(default)]
    pub derived: bool,
    /// The heading the window sits under on the card, for a provider that reports the same windows
    /// for several things (Antigravity: a 5-hour and a weekly lane per model family). None = ungrouped
    #[serde(default)]
    pub group: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageSnapshot {
    /// ok | stale | needsAuth | backoff | error
    pub status: String,
    pub windows: Vec<LimitWindow>,
    pub fetched_at: u64,
    pub note: String,
    #[serde(default)]
    pub backoff_until: u64,
}

fn store_path() -> std::path::PathBuf {
    crate::config::config_path().with_file_name("usage.json")
}

pub fn load_persisted() -> UsageSnapshot {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        // The status it was saved with is the status it comes back with: a reading persisted a
        // minute before a restart is a minute old, not stale, and `fetched_at` came back with it,
        // so whatever reads this can tell the difference on its own.
        .unwrap_or_default()
}

fn persist(s: &UsageSnapshot) {
    if let Ok(t) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(store_path(), t);
    }
}

struct Credential {
    token: String,
    /// ms epoch (None = the file names no expiry)
    expires_at: Option<u64>,
}

impl Credential {
    fn expired(&self, now: u64) -> bool {
        self.expires_at.map(|e| e <= now).unwrap_or(false)
    }
}

fn credential_path_in(claude_home: &std::path::Path) -> Option<std::path::PathBuf> {
    [".credentials.json", "credentials.json"]
        .into_iter()
        .map(|name| claude_home.join(name))
        .find(|path| path.is_file())
}

/// Reads Claude Code's OAuth credential from one explicit config root.
///
/// The root can be native Windows or a resolved WSL UNC path. Reading is side-effect free.
fn read_credentials_in(claude_home: &std::path::Path) -> Option<Credential> {
    let path = credential_path_in(claude_home)?;
    let text = std::fs::read_to_string(path).ok()?;
    let v = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    let oauth = v.get("claudeAiOauth").unwrap_or(&v);
    let token = oauth.get("accessToken").and_then(|x| x.as_str())?.trim();
    if token.is_empty() {
        return None;
    }
    let expires_at = oauth.get("expiresAt").and_then(|x| x.as_f64()).map(|ms| ms as u64);
    Some(Credential { token: token.to_string(), expires_at })
}

/// Native compatibility wrapper used by the legacy single-source Claude poller until account-aware
/// routing is enabled in the next Gate 3 slice.
fn read_credentials() -> Option<Credential> {
    let home = dirs::home_dir()?.join(".claude");
    read_credentials_in(&home)
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaudeProfileObservation {
    pub profile_key: String,
    pub environment_id: String,
    pub display_name: String,
    pub config_dir: std::path::PathBuf,
    pub credential_path: Option<std::path::PathBuf>,
    pub auth_status: String,
    pub expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_email: Option<String>,
    /// Opaque RunOptic identity derived from Claude Code's account metadata.
    ///
    /// The raw organization UUID is intentionally never serialized or logged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_key: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ClaudeAccountMetadata {
    email_address: Option<String>,
    organization_uuid: Option<String>,
}

fn claude_account_file(profile: &crate::profile::ToolProfile) -> std::path::PathBuf {
    if profile.default_profile {
        profile
            .config_dir
            .parent()
            .unwrap_or(&profile.config_dir)
            .join(".claude.json")
    } else {
        profile.config_dir.join(".claude.json")
    }
}

fn read_claude_account(profile: &crate::profile::ToolProfile) -> ClaudeAccountMetadata {
    let path = claude_account_file(profile);
    let Ok(file) = std::fs::File::open(path) else {
        return ClaudeAccountMetadata::default();
    };
    let Ok(value) = serde_json::from_reader::<_, serde_json::Value>(std::io::BufReader::new(file)) else {
        return ClaudeAccountMetadata::default();
    };
    let Some(account) = value.get("oauthAccount") else {
        return ClaudeAccountMetadata::default();
    };

    let clean = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };

    ClaudeAccountMetadata {
        email_address: clean(account.get("emailAddress").and_then(|value| value.as_str())),
        organization_uuid: clean(
            account
                .get("organizationUuid")
                .and_then(|value| value.as_str()),
        ),
    }
}

fn stable_claude_account_key(metadata: &ClaudeAccountMetadata) -> Option<String> {
    let email = metadata.email_address.as_deref()?;
    let organization = metadata.organization_uuid.as_deref()?;

    // FNV-1a is not used as security; it only keeps vendor identifiers out of runtime JSON/logs.
    // The input never leaves memory. Email + organization together avoid merging two seats in the
    // same organization when only the organization UUID matches.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in organization
        .bytes()
        .chain(std::iter::once(0))
        .chain(email.to_ascii_lowercase().bytes())
    {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Some(format!("claude-account-{hash:016x}"))
}

pub fn observe_claude_profile(
    profile: &crate::profile::ToolProfile,
) -> Option<ClaudeProfileObservation> {
    if profile.tool != crate::profile::ToolKind::ClaudeCode {
        return None;
    }

    let credential_path = credential_path_in(&profile.config_dir);
    let credential = read_credentials_in(&profile.config_dir);
    let now = now_ms();
    let auth_status = match credential.as_ref() {
        Some(c) if c.expired(now) => "expired",
        Some(_) => "usable",
        None if credential_path.is_some() => "invalid",
        None => "missing",
    }
    .to_string();

    let account = read_claude_account(profile);
    let account_key = stable_claude_account_key(&account);

    Some(ClaudeProfileObservation {
        profile_key: profile.key.clone(),
        environment_id: profile.environment_id.clone(),
        display_name: profile.display_name.clone(),
        config_dir: profile.config_dir.clone(),
        credential_path,
        auth_status,
        expires_at: credential.and_then(|c| c.expires_at),
        account_email: account.email_address,
        account_key,
    })
}

pub fn observe_claude_profiles(
    profiles: &[crate::profile::ToolProfile],
) -> Vec<ClaudeProfileObservation> {
    profiles.iter().filter_map(observe_claude_profile).collect()
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ClaudeAccountGroup {
    /// Opaque RunOptic identity. If Claude Code account metadata is unavailable, this is the
    /// environment-qualified profile key so unrelated profiles are never merged by guesswork.
    pub key: String,
    pub profile_keys: Vec<String>,
    pub selected_profile_key: String,
    pub credential_state: String,
}

fn claude_auth_rank(status: &str) -> u8 {
    match status {
        "usable" => 0,
        "expired" => 1,
        "invalid" => 2,
        "missing" => 3,
        _ => 4,
    }
}

pub fn group_claude_accounts(
    observations: &[ClaudeProfileObservation],
) -> Vec<ClaudeAccountGroup> {
    use std::collections::BTreeMap;

    let mut grouped: BTreeMap<String, Vec<&ClaudeProfileObservation>> = BTreeMap::new();

    for observation in observations {
        let identity = observation
            .account_key
            .clone()
            .unwrap_or_else(|| observation.profile_key.clone());
        grouped.entry(identity).or_default().push(observation);
    }

    let mut out = grouped
        .into_iter()
        .map(|(key, mut members)| {
            members.sort_by(|a, b| {
                claude_auth_rank(&a.auth_status)
                    .cmp(&claude_auth_rank(&b.auth_status))
                    .then(a.profile_key.cmp(&b.profile_key))
            });
            let selected = members[0];
            let mut profile_keys = members
                .iter()
                .map(|member| member.profile_key.clone())
                .collect::<Vec<_>>();
            profile_keys.sort();

            ClaudeAccountGroup {
                key,
                profile_keys,
                selected_profile_key: selected.profile_key.clone(),
                credential_state: selected.auth_status.clone(),
            }
        })
        .collect::<Vec<_>>();

    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

/// For doctor: credential probe report (prints no secret values)
pub fn probe_credentials() -> String {
    let cli = match find_cli() {
        Some(p) => format!("renews via {}", p.display()),
        None => "no standalone claude CLI found to renew it".into(),
    };
    match read_credentials() {
        Some(c) => format!(
            "credential: found (token {} chars, {}; {cli})",
            c.token.len(),
            if c.expired(now_ms()) { "expired" } else { "valid" }
        ),
        None => "credential: ~/.claude/.credentials.json not found (needsAuth; the desktop app may use another store — signing in once with the Claude Code CLI creates it)".into(),
    }
}

// ---------------- token renewal (upstream's ClaudeTokenRefresher) ----------------

/// Anything under these belongs to the desktop app: its bundled Claude Code keeps its token in the desktop app's
/// own store and never writes ~/.claude/.credentials.json, so renewing with it would change nothing here
fn is_desktop_owned(p: &std::path::Path) -> bool {
    let s = p.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
    s.contains("\\anthropicclaude\\") || s.contains("\\claude\\claude-code\\") || s.contains("\\windowsapps\\")
}

/// The standalone Claude Code command: its own installer's location first, then global npm/pnpm/Volta, then PATH
fn find_cli() -> Option<std::path::PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = dirs::home_dir() {
        v.push(h.join(".local").join("bin").join("claude.exe"));
    }
    if let Some(d) = dirs::config_dir() {
        v.push(d.join("npm").join("claude.cmd"));
    }
    if let Some(d) = dirs::data_local_dir() {
        v.push(d.join("pnpm").join("claude.cmd"));
    }
    if let Some(h) = dirs::home_dir() {
        v.push(h.join(".volta").join("bin").join("claude.exe"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            v.push(dir.join("claude.exe"));
            v.push(dir.join("claude.cmd"));
        }
    }
    v.into_iter().find(|p| p.is_file() && !is_desktop_owned(p))
}

/// Whether a launch is worth making. Pure, so every branch is testable without a clock or a subprocess
fn should_renew(
    expires_at: Option<u64>,
    now: u64,
    attempted_for: Option<u64>,
    last_attempt: Option<u64>,
    failures: u32,
) -> bool {
    // Nothing read yet: never launch on a guess
    let Some(exp) = expires_at else { return false };
    // Plenty of time left — also where launching would do nothing, because the CLI's own gate has not opened
    if exp > now + RENEW_MARGIN_MS {
        return false;
    }
    let Some(t) = last_attempt else { return true };
    // A launch that failed to move the expiry leaves the same value here. One failed launch — asleep, offline, a
    // busy CLI — must not freeze the ring until someone opens a terminal, so the same token is tried again, but
    // on a doubling wait, so a token that cannot renew does not become a launch every tick
    let wait = if attempted_for == Some(exp) { retry_wait_ms(failures) } else { RENEW_COOLDOWN_MS };
    now.saturating_sub(t) >= wait
}

fn retry_wait_ms(failures: u32) -> u64 {
    RENEW_COOLDOWN_MS.saturating_mul(1u64 << failures.min(16)).min(RENEW_RETRY_CAP_MS)
}

/// `claude -p` with a null stdin starts up (which is where it renews an aged token), then exits non-zero for want
/// of a prompt: no conversation, no transcript. Output goes nowhere — a token could in principle be echoed into it.
fn run_renewal(cli: &std::path::Path) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(cli);
    cmd.arg("-p").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // Launched from inside a Claude Code session, the child would take the host's auth and leave the file alone
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy();
        if k == "CLAUDECODE" || k.starts_with("CLAUDE_CODE_") {
            cmd.env_remove(k.as_ref());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(RENEW_TIMEOUT_SECS);
    while child.try_wait()?.is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[derive(Default)]
struct Renewer {
    attempted_for: Option<u64>,
    last_attempt: Option<u64>,
    /// Launches in a row that left `attempted_for` where it was
    failures: u32,
}

impl Renewer {
    /// Renews if the token is about to expire. Some(true) = the expiry moved; judged on the outcome, never on the
    /// exit status, because refusing the empty prompt is a non-zero exit and a successful renewal at the same time
    fn maybe_renew(&mut self, cred: &Credential) -> Option<bool> {
        let now = now_ms();
        if !should_renew(cred.expires_at, now, self.attempted_for, self.last_attempt, self.failures) {
            return None;
        }
        if self.attempted_for != cred.expires_at {
            self.failures = 0;
        }
        self.last_attempt = Some(now);
        self.attempted_for = cred.expires_at;
        self.failures = self.failures.saturating_add(1);
        let Some(cli) = find_cli() else {
            crate::applog("claude: token about to expire and no standalone claude CLI found to renew it");
            return Some(false);
        };
        if let Err(e) = run_renewal(&cli) {
            crate::applog(&format!("claude: token renewal could not start ({}): {e}", cli.display()));
            return Some(false);
        }
        let after = read_credentials().and_then(|c| c.expires_at);
        let renewed = matches!((after, cred.expires_at), (Some(a), Some(b)) if a > b);
        crate::applog(&if renewed {
            format!("claude: token renewed via {}", cli.display())
        } else {
            format!("claude: ran {} but the token expiry did not move", cli.display())
        });
        Some(renewed)
    }
}

fn parse_reset(v: &serde_json::Value) -> Option<u64> {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis().max(0) as u64)
}

fn label_for(kind: &str) -> String {
    match kind {
        "session" => "Current session".into(),
        "seven_day" | "weekly_all" => "Weekly (all models)".into(),
        "seven_day_opus" | "weekly_opus" => "Weekly (Opus)".into(),
        "weekly_scoped" => "Weekly (model-scoped)".into(),
        other => {
            // Forward compatibility: an unknown kind gets a readable label
            let mut s = other.replace('_', " ");
            if let Some(c) = s.get_mut(0..1) {
                c.make_ascii_uppercase();
            }
            s
        }
    }
}

fn parse_response(v: &serde_json::Value) -> Vec<LimitWindow> {
    let mut out: Vec<LimitWindow> = Vec::new();
    if let Some(arr) = v.get("limits").and_then(|x| x.as_array()) {
        for l in arr {
            let Some(kind) = l.get("kind").and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(pct) = l.get("percent").and_then(|x| x.as_f64()) else {
                continue;
            };
            let resets = l.get("resets_at").and_then(parse_reset);
            if resets.is_none() {
                continue; // upstream rule: a window without a reset time is not shown
            }
            out.push(LimitWindow {
                id: kind.to_string(),
                label: label_for(kind),
                used: (pct / 100.0).clamp(0.0, 1.0),
                resets_at: resets, ..Default::default()
            });
        }
    }
    // Fallback merge: a window that just rolled over disappears from limits while the named field remains.
    // In practice the kinds in limits are weekly_all/weekly_scoped, not seven_day — deduplicating by id
    // alone would add the seven_day fallback a second time (the card showed "Weekly all" and
    // "Weekly (all models)" as twins). Three dedupe rules: id alias / same resets_at and percentage / same label.
    let aliases: [(&str, &str, &[&str]); 2] = [
        ("five_hour", "session", &["session", "five_hour"]),
        ("seven_day", "seven_day", &["seven_day", "weekly_all", "weekly"]),
    ];
    for (field, id, alias) in aliases {
        let Some(w) = v.get(field) else { continue };
        let Some(u) = w.get("utilization").and_then(|x| x.as_f64()) else { continue };
        let used = (u / 100.0).clamp(0.0, 1.0);
        let resets_at = w.get("resets_at").and_then(parse_reset);
        let label = label_for(id);
        let dup = out.iter().any(|x| {
            alias.contains(&x.id.as_str())
                || x.label == label
                || (resets_at.is_some()
                    && x.resets_at.map(|r| r / 1000) == resets_at.map(|r| r / 1000)
                    && (x.used - used).abs() < 0.005)
        });
        if dup {
            continue;
        }
        out.push(LimitWindow { id: id.into(), label, used, resets_at, ..Default::default() });
    }
    // session always comes first (upstream display order)
    out.sort_by_key(|w| if w.id == "session" { 0 } else { 1 });
    out
}

enum FetchErr {
    NeedsAuth,
    RateLimited(u64), // suggested wait in seconds (the Retry-After before the floor is applied)
    Other(String),
}

fn fetch_once(token: &str) -> Result<Vec<LimitWindow>, FetchErr> {
    let resp = ureq::get(ENDPOINT)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .timeout(Duration::from_secs(15))
        .call();
    match resp {
        Ok(r) => {
            let v: serde_json::Value = r
                .into_json()
                .map_err(|e| FetchErr::Other(format!("parse: {e}")))?;
            Ok(parse_response(&v))
        }
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            Err(FetchErr::NeedsAuth)
        }
        Err(ureq::Error::Status(429, r)) => {
            let ra = r
                .header("retry-after")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            Err(FetchErr::RateLimited(ra))
        }
        Err(ureq::Error::Status(code, _)) => Err(FetchErr::Other(format!("HTTP {code}"))),
        Err(e) => Err(FetchErr::Other(format!("{e}"))),
    }
}

fn backoff_secs(consecutive: u32, retry_after_floor: u64) -> u64 {
    let exp = BACKOFF_BASE_SECS.saturating_mul(1u64 << consecutive.min(4));
    // The server's Retry-After is honoured in full: with expired tokens no longer
    // sent, a long one is a real rate limit, and retrying early only earns another.
    exp.clamp(BACKOFF_BASE_SECS, BACKOFF_CAP_SECS).max(retry_after_floor)
}

fn set_and_broadcast(app: &AppHandle, mutate: impl FnOnce(&mut UsageSnapshot)) {
    let st = app.state::<AppState>();
    let snap = {
        let mut u = st.usage.lock().unwrap();
        mutate(&mut u);
        u.clone()
    };
    persist(&snap);
    let _ = app.emit("usage", &snap);
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        // Broadcast the persisted old reading at startup (stale beats blank)
        {
            let st = app.state::<AppState>();
            let snap = st.usage.lock().unwrap().clone();
            let _ = app.emit("usage", &snap);
        }
        let mut consecutive_429: u32 = 0;
        let mut renewer = Renewer::default();
        loop {
            // Ahead of the back-off: renewing never touches the usage endpoint, and a fresh token deserves a fresh try
            if let Some(cred) = read_credentials() {
                if renewer.maybe_renew(&cred) == Some(true) {
                    consecutive_429 = 0;
                    set_and_broadcast(&app, |u| u.backoff_until = 0);
                }
            }
            // No requests inside the backoff window
            let bu = {
                let st = app.state::<AppState>();
                let u = st.usage.lock().unwrap();
                u.backoff_until
            };
            let now = now_ms();
            if bu > now {
                sleep_interruptible(((bu - now) / 1000).clamp(1, 30));
                continue;
            }
            match read_credentials() {
                None => set_and_broadcast(&app, |u| {
                    u.status = "needsAuth".into();
                    u.note = "No Claude Code credential found".into();
                }),
                // Expired is not signed out: keep the last reading, dimmed and dated, and send nothing
                Some(cred) if cred.expired(now_ms()) => set_and_broadcast(&app, |u| {
                    u.status = if u.windows.is_empty() { "needsAuth" } else { "stale" }.into();
                    u.note = EXPIRED_NOTE.into();
                }),
                Some(cred) => {
                    let token = cred.token;
                    // On 401/403 re-read the credential and retry once (Claude Code may have just refreshed it)
                    let result = match fetch_once(&token) {
                        Err(FetchErr::NeedsAuth) => match read_credentials() {
                            Some(c2) if c2.token != token => fetch_once(&c2.token),
                            _ => Err(FetchErr::NeedsAuth),
                        },
                        other => other,
                    };
                    let auth_note = "Credential rejected (switched accounts?)";
                    match result {
                        Ok(windows) => {
                            consecutive_429 = 0;
                            set_and_broadcast(&app, |u| {
                                u.status = "ok".into();
                                u.windows = windows;
                                u.fetched_at = now_ms();
                                u.note.clear();
                                u.backoff_until = 0;
                            });
                        }
                        Err(FetchErr::NeedsAuth) => set_and_broadcast(&app, |u| {
                            u.status = "needsAuth".into();
                            u.note = auth_note.into();
                        }),
                        Err(FetchErr::RateLimited(ra)) => {
                            consecutive_429 += 1;
                            let wait = backoff_secs(consecutive_429 - 1, ra);
                            set_and_broadcast(&app, |u| {
                                // The status is left alone: a refused refresh says nothing about the
                                // reading we are holding, which is exactly as old as it was a moment
                                // ago. Marking it stale here dimmed the ring on the first 429, which
                                // on Windows is often the first minute of a rate limit. Age decides,
                                // as it does on the Mac (`UsageStore` keeps the previous status until
                                // `staleAfter`), and the note below says why it is not moving.
                                u.note = format!("Rate limited, retrying in {wait}s");
                                u.backoff_until = now_ms() + wait * 1000;
                            });
                        }
                        Err(FetchErr::Other(msg)) => set_and_broadcast(&app, |u| {
                            // No reading at all is an error worth showing; a reading we could not
                            // refresh is just a reading, and its own age is what makes it stale.
                            if u.windows.is_empty() {
                                u.status = "error".into();
                            }
                            u.note = msg;
                        }),
                    }
                }
            }
            // 60 s while a session is active, 300 s otherwise (upstream throttling discipline)
            let active = {
                let st = app.state::<AppState>();
                let store = st.store.lock().unwrap();
                let s = store.snapshot("en", "en", false, false);
                !s.sessions.is_empty()
            };
            sleep_interruptible(if active {
                POLL_ACTIVE_SECS
            } else {
                POLL_IDLE_SECS
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_profile(key: &str, root: std::path::PathBuf) -> crate::profile::ToolProfile {
        crate::profile::ToolProfile {
            key: key.into(),
            tool: crate::profile::ToolKind::ClaudeCode,
            environment_id: key.split('/').next().unwrap_or("env").into(),
            display_name: "Claude".into(),
            config_dir: root,
            slug: None,
            default_profile: true,
        }
    }

    #[test]
    fn claude_profile_observation_reads_explicit_root() {
        let root = std::env::temp_dir().join(format!("runoptic-claude-profile-{}", now_ms()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(".credentials.json"),
            serde_json::to_vec(&serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "test-token",
                    "expiresAt": now_ms() + 60_000
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let profile = claude_profile("wsl:ubuntu/claude", root.clone());
        let obs = observe_claude_profile(&profile).expect("Claude profile should be observed");

        assert_eq!(obs.profile_key, "wsl:ubuntu/claude");
        assert_eq!(obs.auth_status, "usable");
        assert!(obs.credential_path.is_some());

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn claude_account_group_merges_same_identity_across_environments() {
        let account_key = Some("claude-account-test".to_string());
        let observations = vec![
            ClaudeProfileObservation {
                profile_key: "windows-native/claude".into(),
                environment_id: "windows-native".into(),
                display_name: "Claude".into(),
                config_dir: std::path::PathBuf::from(r"C:\Users\test\.claude"),
                credential_path: None,
                auth_status: "expired".into(),
                expires_at: Some(1),
                account_email: Some("user@example.com".into()),
                account_key: account_key.clone(),
            },
            ClaudeProfileObservation {
                profile_key: "wsl:ubuntu/claude".into(),
                environment_id: "wsl:ubuntu".into(),
                display_name: "Claude".into(),
                config_dir: std::path::PathBuf::from(r"\\wsl.localhost\Ubuntu\home\test\.claude"),
                credential_path: None,
                auth_status: "usable".into(),
                expires_at: Some(u64::MAX),
                account_email: Some("user@example.com".into()),
                account_key,
            },
        ];

        let groups = group_claude_accounts(&observations);

        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].profile_keys,
            vec!["windows-native/claude", "wsl:ubuntu/claude"]
        );
        assert_eq!(groups[0].selected_profile_key, "wsl:ubuntu/claude");
        assert_eq!(groups[0].credential_state, "usable");
    }

    #[test]
    fn claude_profiles_without_identity_are_never_merged() {
        let observations = vec![
            ClaudeProfileObservation {
                profile_key: "windows-native/claude".into(),
                environment_id: "windows-native".into(),
                display_name: "Claude".into(),
                config_dir: std::path::PathBuf::new(),
                credential_path: None,
                auth_status: "missing".into(),
                expires_at: None,
                account_email: None,
                account_key: None,
            },
            ClaudeProfileObservation {
                profile_key: "wsl:ubuntu/claude".into(),
                environment_id: "wsl:ubuntu".into(),
                display_name: "Claude".into(),
                config_dir: std::path::PathBuf::new(),
                credential_path: None,
                auth_status: "usable".into(),
                expires_at: None,
                account_email: None,
                account_key: None,
            },
        ];

        let groups = group_claude_accounts(&observations);

        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn default_claude_profile_reads_account_file_beside_config_directory() {
        let home = std::env::temp_dir().join(format!("runoptic-claude-account-{}", now_ms()));
        let config = home.join(".claude");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            home.join(".claude.json"),
            serde_json::to_vec(&serde_json::json!({
                "oauthAccount": {
                    "emailAddress": "User@Example.com",
                    "organizationUuid": "org-test"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let profile = claude_profile("wsl:ubuntu/claude", config);
        let obs = observe_claude_profile(&profile).unwrap();

        assert_eq!(obs.account_email.as_deref(), Some("User@Example.com"));
        assert!(obs.account_key.as_deref().unwrap().starts_with("claude-account-"));

        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn claude_account_identity_requires_email_and_organization() {
        let with_both = ClaudeAccountMetadata {
            email_address: Some("a@example.com".into()),
            organization_uuid: Some("org".into()),
        };
        let missing_email = ClaudeAccountMetadata {
            email_address: None,
            organization_uuid: Some("org".into()),
        };
        assert!(stable_claude_account_key(&with_both).is_some());
        assert!(stable_claude_account_key(&missing_email).is_none());
    }

    #[test]
    fn claude_profile_observation_marks_expired_credentials() {
        let root = std::env::temp_dir().join(format!("runoptic-claude-expired-{}", now_ms()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("credentials.json"),
            serde_json::to_vec(&serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "test-token",
                    "expiresAt": now_ms().saturating_sub(1)
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let profile = claude_profile("windows-native/claude", root.clone());
        let obs = observe_claude_profile(&profile).expect("Claude profile should be observed");

        assert_eq!(obs.auth_status, "expired");

        std::fs::remove_dir_all(root).ok();
    }

    const EXP: u64 = 1_000_000_000;

    #[test]
    fn renews_only_inside_the_margin() {
        assert!(!should_renew(None, EXP, None, None, 0), "never launch on a guess");
        assert!(!should_renew(Some(EXP), EXP - RENEW_MARGIN_MS - 1, None, None, 0), "plenty of time left");
        assert!(should_renew(Some(EXP), EXP - RENEW_MARGIN_MS, None, None, 0));
        assert!(should_renew(Some(EXP), EXP + 3_600_000, None, None, 0), "already expired still renews");
    }

    #[test]
    fn a_new_token_waits_out_the_cooldown() {
        let now = EXP + 1;
        assert!(!should_renew(Some(EXP + 5), now, Some(EXP), Some(now - 1000), 1), "cooldown holds a new token back");
        assert!(should_renew(Some(EXP + 5), now, Some(EXP), Some(now - RENEW_COOLDOWN_MS), 1));
    }

    #[test]
    fn a_failed_token_is_retried_on_a_doubling_wait() {
        let now = EXP + 1;
        assert!(!should_renew(Some(EXP), now, Some(EXP), Some(now - RENEW_COOLDOWN_MS), 1), "no retry at the plain cooldown");
        assert!(should_renew(Some(EXP), now, Some(EXP), Some(now - 2 * RENEW_COOLDOWN_MS), 1), "retried after twice the cooldown");
        assert!(!should_renew(Some(EXP), now, Some(EXP), Some(now - 2 * RENEW_COOLDOWN_MS), 2), "the wait doubles");
        assert!(should_renew(Some(EXP), now, Some(EXP), Some(now - 4 * RENEW_COOLDOWN_MS), 2));
        assert!(!should_renew(Some(EXP), now, Some(EXP), Some(now - RENEW_RETRY_CAP_MS + 1), 30), "never a tight loop");
        assert!(should_renew(Some(EXP), now, Some(EXP), Some(now - RENEW_RETRY_CAP_MS), 30), "but never more than an hour apart");
    }

    #[test]
    fn retry_after_never_exceeds_the_cap() {
        assert_eq!(backoff_secs(0, 3600), 3600);
        assert_eq!(backoff_secs(0, 0), BACKOFF_BASE_SECS);
        assert_eq!(backoff_secs(1, 300), 300);
        assert_eq!(backoff_secs(9, 0), BACKOFF_CAP_SECS);
    }

    #[test]
    fn desktop_bundled_cli_is_refused() {
        use std::path::Path;
        assert!(is_desktop_owned(Path::new(r"C:\Users\u\AppData\Local\AnthropicClaude\app-1.2.3\claude.exe")));
        assert!(is_desktop_owned(Path::new(r"C:\Users\u\AppData\Roaming\Claude\claude-code\2.1.0\claude.exe")));
        assert!(!is_desktop_owned(Path::new(r"C:\Users\u\.local\bin\claude.exe")));
        assert!(!is_desktop_owned(Path::new(r"C:\Users\u\AppData\Roaming\npm\claude.cmd")));
    }

    #[test]
    #[ignore = "Runs the installed standalone claude CLI; opt in for integration verification"]
    fn live_renewal_runs_the_standalone_cli() {
        let cli = find_cli().expect("a standalone claude CLI");
        assert!(!is_desktop_owned(&cli));
        let before = read_credentials().and_then(|c| c.expires_at);
        let t = std::time::Instant::now();
        run_renewal(&cli).expect("spawned");
        assert!(t.elapsed() < Duration::from_secs(RENEW_TIMEOUT_SECS), "returned before the timeout");
        let after = read_credentials().and_then(|c| c.expires_at);
        assert!(after >= before, "the expiry never moves backwards");
        eprintln!("cli: {}", cli.display());
    }

    #[test]
    fn expired_is_judged_against_now() {
        let c = Credential { token: "t".into(), expires_at: Some(EXP) };
        assert!(c.expired(EXP));
        assert!(!c.expired(EXP - 1));
        assert!(!Credential { token: "t".into(), expires_at: None }.expired(EXP));
    }
}
