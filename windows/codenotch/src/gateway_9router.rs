use crate::{environment, telemetry, AppState};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:20128";
const CLI_TOKEN_SALT: &str = "9r-cli-auth";
const POLL_ACTIVE_SECS: u64 = 15;
const POLL_IDLE_SECS: u64 = 60;

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
    pub source_environment_id: String,
    pub auth_source: String,
    pub fetched_at_ms: u64,
    pub usage: Vec<UsageSample>,
    pub performance: Vec<RequestPerformance>,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageSample {
    pub observation_id: String,
    pub provider: String,
    pub model: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RequestPerformance {
    pub observation_id: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub latency_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct StatsResponse {
    #[serde(default, rename = "byModel")]
    by_model: BTreeMap<String, StatsBucket>,
}

#[derive(Debug, Deserialize, Default)]
struct StatsBucket {
    #[serde(default)]
    requests: u64,
    #[serde(default, rename = "promptTokens")]
    prompt_tokens: u64,
    #[serde(default, rename = "completionTokens")]
    completion_tokens: u64,
    #[serde(default, rename = "cachedTokens")]
    cached_tokens: u64,
    #[serde(default)]
    cost: f64,
    #[serde(default, rename = "rawModel")]
    raw_model: String,
    #[serde(default)]
    provider: String,
}

#[derive(Debug, Deserialize, Default)]
struct RequestDetailsResponse {
    #[serde(default)]
    details: Vec<RequestDetail>,
}

#[derive(Debug, Deserialize, Default)]
struct RequestDetail {
    #[serde(default)]
    id: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    latency: Latency,
}

#[derive(Debug, Deserialize, Default)]
struct Latency {
    #[serde(default)]
    total: Option<f64>,
    #[serde(default)]
    ttft: Option<f64>,
}

#[derive(Debug, Clone)]
struct AuthCandidate {
    token: Option<String>,
    environment_id: String,
    source: String,
}

pub fn start(app: AppHandle, environments: Vec<environment::SourceEnvironment>) {
    std::thread::spawn(move || loop {
        let snapshot = collect(&environments);
        {
            let state = app.state::<AppState>();
            *state.gateway_9router.lock().unwrap() = snapshot;
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

pub fn collect(environments: &[environment::SourceEnvironment]) -> Snapshot {
    let base_url = std::env::var("RUNOPTIC_9ROUTER_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.into())
        .trim_end_matches('/')
        .to_string();

    let candidates = auth_candidates(environments);
    let mut last_error = String::new();

    for candidate in candidates {
        match fetch_stats(&base_url, candidate.token.as_deref()) {
            Ok(stats) => {
                let details = fetch_request_details(&base_url, candidate.token.as_deref())
                    .unwrap_or_default();
                return Snapshot {
                    status: "ok".into(),
                    base_url,
                    source_environment_id: candidate.environment_id,
                    auth_source: candidate.source,
                    fetched_at_ms: now_ms(),
                    usage: usage_samples(stats),
                    performance: performance_samples(details),
                    note: String::new(),
                };
            }
            Err(error) => {
                last_error = error;
            }
        }
    }

    Snapshot {
        status: if last_error.contains("connection") || last_error.contains("transport") {
            "unavailable".into()
        } else {
            "needs_auth".into()
        },
        base_url,
        source_environment_id: "gateway:9router".into(),
        auth_source: "none".into(),
        fetched_at_ms: now_ms(),
        usage: Vec::new(),
        performance: Vec::new(),
        note: if last_error.is_empty() {
            "9router usage API unavailable".into()
        } else {
            last_error
        },
    }
}

fn usage_samples(stats: StatsResponse) -> Vec<UsageSample> {
    stats
        .by_model
        .into_iter()
        .map(|(key, bucket)| {
            let model = if bucket.raw_model.trim().is_empty() {
                key.split(" (").next().unwrap_or(&key).to_string()
            } else {
                bucket.raw_model
            };
            let provider = if bucket.provider.trim().is_empty() {
                "9router".into()
            } else {
                bucket.provider
            };
            UsageSample {
                observation_id: format!("9router:today:{provider}:{model}"),
                provider,
                model,
                requests: bucket.requests,
                input_tokens: bucket.prompt_tokens,
                output_tokens: bucket.completion_tokens,
                cached_tokens: bucket.cached_tokens,
                cost_usd: bucket.cost,
            }
        })
        .collect()
}

fn performance_samples(details: RequestDetailsResponse) -> Vec<RequestPerformance> {
    details
        .details
        .into_iter()
        .filter(|detail| !detail.id.trim().is_empty())
        .map(|detail| RequestPerformance {
            observation_id: detail.id,
            provider: detail.provider,
            model: detail.model,
            latency_ms: finite_ms(detail.latency.total),
            ttft_ms: finite_ms(detail.latency.ttft),
        })
        .collect()
}

fn finite_ms(value: Option<f64>) -> Option<u64> {
    value
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value.round() as u64)
}

fn fetch_stats(base_url: &str, token: Option<&str>) -> Result<StatsResponse, String> {
    fetch_json(
        &format!("{base_url}/api/usage/stats?period=today"),
        token,
    )
}

fn fetch_request_details(
    base_url: &str,
    token: Option<&str>,
) -> Result<RequestDetailsResponse, String> {
    fetch_json(
        &format!("{base_url}/api/usage/request-details?page=1&pageSize=20"),
        token,
    )
}

fn fetch_json<T: for<'de> Deserialize<'de>>(url: &str, token: Option<&str>) -> Result<T, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout_read(Duration::from_secs(5))
        .timeout_write(Duration::from_secs(5))
        .build();
    let mut request = agent.get(url).set("Accept", "application/json");
    if let Some(token) = token {
        request = request.set("x-9r-cli-token", token);
    }

    match request.call() {
        Ok(response) => response
            .into_json::<T>()
            .map_err(|error| format!("9router response parse failed: {error}")),
        Err(ureq::Error::Status(code, _)) => Err(format!("9router returned HTTP {code}")),
        Err(ureq::Error::Transport(error)) => {
            Err(format!("9router transport/connection failed: {error}"))
        }
    }
}

fn auth_candidates(environments: &[environment::SourceEnvironment]) -> Vec<AuthCandidate> {
    let mut out = Vec::new();

    if let Ok(token) = std::env::var("RUNOPTIC_9ROUTER_CLI_TOKEN") {
        if !token.trim().is_empty() {
            out.push(AuthCandidate {
                token: Some(token),
                environment_id: std::env::var("RUNOPTIC_9ROUTER_ENVIRONMENT_ID")
                    .unwrap_or_else(|_| "gateway:9router".into()),
                source: "explicit".into(),
            });
        }
    }

    let mut dirs = Vec::<(String, PathBuf, String)>::new();
    if let Ok(data_dir) = std::env::var("RUNOPTIC_9ROUTER_DATA_DIR") {
        if !data_dir.trim().is_empty() {
            dirs.push((
                std::env::var("RUNOPTIC_9ROUTER_ENVIRONMENT_ID")
                    .unwrap_or_else(|_| "gateway:9router".into()),
                PathBuf::from(data_dir),
                "explicit-data-dir".into(),
            ));
        }
    }

    if let Some(config) = dirs::config_dir() {
        dirs.push((
            "windows-native".into(),
            config.join("9router"),
            "windows-auto".into(),
        ));
    }

    for environment in environments {
        if environment.system || !environment.running || !environment.reachable {
            continue;
        }
        if environment.kind == environment::EnvironmentKind::Wsl {
            dirs.push((
                environment.id.clone(),
                environment.home.join(".9router"),
                "wsl-auto".into(),
            ));
        }
    }

    let mut seen = HashSet::new();
    for (environment_id, data_dir, source) in dirs {
        let key = data_dir.to_string_lossy().to_ascii_lowercase();
        if !seen.insert(key) {
            continue;
        }
        if let Some(token) = token_from_data_dir(&data_dir) {
            out.push(AuthCandidate {
                token: Some(token),
                environment_id,
                source,
            });
        }
    }

    // 9router permits dashboard APIs without auth when requireLogin=false.
    out.push(AuthCandidate {
        token: None,
        environment_id: "gateway:9router".into(),
        source: "unauthenticated".into(),
    });

    out
}

fn token_from_data_dir(data_dir: &Path) -> Option<String> {
    let machine_id = std::fs::read_to_string(data_dir.join("machine-id")).ok()?;
    let secret = std::fs::read_to_string(data_dir.join("auth").join("cli-secret")).ok()?;
    let machine_id = machine_id.trim();
    let secret = secret.trim();
    if machine_id.is_empty() || secret.is_empty() {
        return None;
    }
    Some(sha256_prefix16(&format!(
        "{machine_id}{CLI_TOKEN_SALT}{secret}"
    )))
}

fn sha256_prefix16(input: &str) -> String {
    let digest = sha256(input.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
        0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
        0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
        0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
        0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
        0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
        0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
        0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2,
    ];
    let mut h = [
        0x6a09e667u32,0xbb67ae85,0x3c6ef372,0xa54ff53a,
        0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19,
    ];

    let bit_len = (input.len() as u64) * 8;
    let mut msg = input.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).take(16).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i-15].rotate_right(7) ^ w[i-15].rotate_right(18) ^ (w[i-15] >> 3);
            let s1 = w[i-2].rotate_right(17) ^ w[i-2].rotate_right(19) ^ (w[i-2] >> 10);
            w[i] = w[i-16]
                .wrapping_add(s0)
                .wrapping_add(w[i-7])
                .wrapping_add(s1);
        }

        let [mut a,mut b,mut c,mut d,mut e,mut f,mut g,mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i*4..i*4+4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

pub fn probe(environments: &[environment::SourceEnvironment]) -> String {
    let snapshot = collect(environments);
    format!(
        "9router: status={} base={} env={} auth={} usage={} performance={}{}",
        snapshot.status,
        snapshot.base_url,
        snapshot.source_environment_id,
        snapshot.auth_source,
        snapshot.usage.len(),
        snapshot.performance.len(),
        if snapshot.note.is_empty() {
            String::new()
        } else {
            format!(" note={}", snapshot.note)
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(sha256_prefix16("abc"), "ba7816bf8f01cfea");
    }

    #[test]
    fn stats_are_mapped_without_inventing_missing_fields() {
        let mut stats = StatsResponse::default();
        stats.by_model.insert(
            "model-a (provider-a)".into(),
            StatsBucket {
                requests: 3,
                prompt_tokens: 100,
                completion_tokens: 25,
                cached_tokens: 40,
                cost: 0.123,
                raw_model: "model-a".into(),
                provider: "provider-a".into(),
            },
        );
        let samples = usage_samples(stats);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].provider, "provider-a");
        assert_eq!(samples[0].model, "model-a");
        assert_eq!(samples[0].requests, 3);
        assert_eq!(samples[0].cached_tokens, 40);
        assert!((samples[0].cost_usd - 0.123).abs() < f64::EPSILON);
    }

    #[test]
    fn invalid_latency_is_not_reported() {
        assert_eq!(finite_ms(Some(-1.0)), None);
        assert_eq!(finite_ms(Some(f64::NAN)), None);
        assert_eq!(finite_ms(Some(42.4)), Some(42));
    }
}
