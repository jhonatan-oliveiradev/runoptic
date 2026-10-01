use crate::{telemetry, AppState};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
const POLL_ACTIVE_SECS: u64 = 60;
const POLL_IDLE_SECS: u64 = 300;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub status: String,
    pub base_url: String,
    pub auth_source: String,
    pub fetched_at_ms: u64,
    pub usage: Option<KeyUsage>,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct KeyUsage {
    pub observation_id: String,
    pub usage_total_usd: Option<f64>,
    pub usage_daily_usd: Option<f64>,
    pub usage_weekly_usd: Option<f64>,
    pub usage_monthly_usd: Option<f64>,
    pub byok_usage_daily_usd: Option<f64>,
    pub limit_usd: Option<f64>,
    pub limit_remaining_usd: Option<f64>,
    pub limit_reset: Option<String>,
    pub is_free_tier: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
struct KeyResponse {
    #[serde(default)]
    data: KeyData,
}

#[derive(Debug, Deserialize, Default)]
struct KeyData {
    #[serde(default)]
    usage: Option<f64>,
    #[serde(default)]
    usage_daily: Option<f64>,
    #[serde(default)]
    usage_weekly: Option<f64>,
    #[serde(default)]
    usage_monthly: Option<f64>,
    #[serde(default)]
    byok_usage_daily: Option<f64>,
    #[serde(default)]
    limit: Option<f64>,
    #[serde(default)]
    limit_remaining: Option<f64>,
    #[serde(default)]
    limit_reset: Option<String>,
    #[serde(default)]
    is_free_tier: Option<bool>,
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || loop {
        let snapshot = collect();
        {
            let state = app.state::<AppState>();
            *state.gateway_openrouter.lock().unwrap() = snapshot;
        }
        telemetry::refresh_from_app(&app);

        let active = {
            let state = app.state::<AppState>();
            let store = state.store.lock().unwrap();
            let snapshot = store.snapshot("en", "en", false, false);
            !snapshot.sessions.is_empty()
        };
        std::thread::sleep(Duration::from_secs(if active {
            POLL_ACTIVE_SECS
        } else {
            POLL_IDLE_SECS
        }));
    });
}

pub fn collect() -> Snapshot {
    let base_url = std::env::var("RUNOPTIC_OPENROUTER_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.into())
        .trim_end_matches('/')
        .to_string();

    if !is_safe_base_url(&base_url) {
        return Snapshot {
            status: "unsupported".into(),
            base_url,
            auth_source: "none".into(),
            fetched_at_ms: now_ms(),
            usage: None,
            note: "OpenRouter bearer auth requires HTTPS (or loopback HTTP for an explicit compatible endpoint)".into(),
        };
    }

    let Some((token, auth_source)) = api_key() else {
        return Snapshot {
            status: "disabled".into(),
            base_url,
            auth_source: "none".into(),
            fetched_at_ms: now_ms(),
            usage: None,
            note: "set RUNOPTIC_OPENROUTER_API_KEY or OPENROUTER_API_KEY to enable direct OpenRouter usage telemetry".into(),
        };
    };

    match fetch_key(&base_url, &token) {
        Ok(response) => Snapshot {
            status: "ok".into(),
            base_url,
            auth_source,
            fetched_at_ms: now_ms(),
            usage: Some(key_usage(response.data)),
            note: String::new(),
        },
        Err(error) => {
            let status = if error.contains("HTTP 401") || error.contains("HTTP 403") {
                "needs_auth"
            } else if error.contains("transport") || error.contains("connection") {
                "unavailable"
            } else {
                "error"
            };
            Snapshot {
                status: status.into(),
                base_url,
                auth_source,
                fetched_at_ms: now_ms(),
                usage: None,
                note: error,
            }
        }
    }
}

fn api_key() -> Option<(String, String)> {
    for (name, source) in [
        ("RUNOPTIC_OPENROUTER_API_KEY", "runoptic-env"),
        ("OPENROUTER_API_KEY", "openrouter-env"),
    ] {
        if let Ok(value) = std::env::var(name) {
            let value = value.trim();
            if !value.is_empty() {
                return Some((value.to_string(), source.into()));
            }
        }
    }
    None
}

fn key_usage(data: KeyData) -> KeyUsage {
    KeyUsage {
        observation_id: "openrouter:key:today".into(),
        usage_total_usd: finite_nonnegative(data.usage),
        usage_daily_usd: finite_nonnegative(data.usage_daily),
        usage_weekly_usd: finite_nonnegative(data.usage_weekly),
        usage_monthly_usd: finite_nonnegative(data.usage_monthly),
        byok_usage_daily_usd: finite_nonnegative(data.byok_usage_daily),
        limit_usd: finite_nonnegative(data.limit),
        limit_remaining_usd: finite_nonnegative(data.limit_remaining),
        limit_reset: data.limit_reset.filter(|value| !value.trim().is_empty()),
        is_free_tier: data.is_free_tier,
    }
}

fn finite_nonnegative(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && *value >= 0.0)
}

fn fetch_key(base_url: &str, token: &str) -> Result<KeyResponse, String> {
    let url = format!("{base_url}/key");
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(3))
        .timeout_read(Duration::from_secs(8))
        .timeout_write(Duration::from_secs(8))
        .build();

    match agent
        .get(&url)
        .set("Accept", "application/json")
        .set("Authorization", &format!("Bearer {token}"))
        .call()
    {
        Ok(response) => response
            .into_json::<KeyResponse>()
            .map_err(|error| format!("OpenRouter response parse failed: {error}")),
        Err(ureq::Error::Status(code, _)) => Err(format!("OpenRouter returned HTTP {code}")),
        Err(ureq::Error::Transport(error)) => {
            Err(format!("OpenRouter transport/connection failed: {error}"))
        }
    }
}

fn is_safe_base_url(base_url: &str) -> bool {
    let Some((scheme, rest)) = base_url.split_once("://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    if scheme.eq_ignore_ascii_case("https") {
        return true;
    }
    if !scheme.eq_ignore_ascii_case("http") {
        return false;
    }

    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
}

pub fn probe() -> String {
    let snapshot = collect();
    let usage = snapshot.usage.as_ref();
    format!(
        "openrouter: status={} base={} auth={} daily_usd={} weekly_usd={} monthly_usd={} limit_usd={} remaining_usd={}{}",
        snapshot.status,
        snapshot.base_url,
        snapshot.auth_source,
        fmt_opt(usage.and_then(|value| value.usage_daily_usd)),
        fmt_opt(usage.and_then(|value| value.usage_weekly_usd)),
        fmt_opt(usage.and_then(|value| value.usage_monthly_usd)),
        fmt_opt(usage.and_then(|value| value.limit_usd)),
        fmt_opt(usage.and_then(|value| value.limit_remaining_usd)),
        if snapshot.note.is_empty() {
            String::new()
        } else {
            format!(" note={}", snapshot.note)
        }
    )
}

fn fmt_opt(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_auth_requires_https_except_loopback() {
        assert!(is_safe_base_url("https://openrouter.ai/api/v1"));
        assert!(is_safe_base_url("https://router.example.com/v1"));
        assert!(is_safe_base_url("http://127.0.0.1:8080/api/v1"));
        assert!(is_safe_base_url("http://localhost:8080/api/v1"));
        assert!(!is_safe_base_url("http://openrouter.ai/api/v1"));
        assert!(!is_safe_base_url("ftp://openrouter.ai/api/v1"));
        assert!(!is_safe_base_url("https://user:pass@openrouter.ai/api/v1"));
    }

    #[test]
    fn current_key_fields_preserve_unknowns_and_drop_invalid_numbers() {
        let usage = key_usage(KeyData {
            usage: Some(25.5),
            usage_daily: Some(1.25),
            usage_weekly: Some(4.5),
            usage_monthly: Some(12.0),
            byok_usage_daily: Some(0.75),
            limit: Some(100.0),
            limit_remaining: Some(74.5),
            limit_reset: Some("monthly".into()),
            is_free_tier: Some(false),
        });
        assert_eq!(usage.usage_daily_usd, Some(1.25));
        assert_eq!(usage.limit_remaining_usd, Some(74.5));
        assert_eq!(usage.limit_reset.as_deref(), Some("monthly"));
        assert_eq!(usage.is_free_tier, Some(false));

        let invalid = key_usage(KeyData {
            usage_daily: Some(f64::NAN),
            limit: Some(-1.0),
            ..Default::default()
        });
        assert_eq!(invalid.usage_daily_usd, None);
        assert_eq!(invalid.limit_usd, None);
        assert_eq!(invalid.usage_monthly_usd, None);
    }
}
