use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

use crate::{codex, gateway_9router, nx_agent, usage, AppState};

pub const TELEMETRY_PROTOCOL: &str = "runoptic.telemetry.v1";
pub const TELEMETRY_HISTORY_PROTOCOL: &str = "runoptic.telemetry.history.v1";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvironmentKind {
    WindowsNative,
    Wsl { distro: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceEnvironment {
    pub id: String,
    pub kind: EnvironmentKind,
    pub home: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceKind {
    Official,
    Derived,
    Estimated,
    LocalObservation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Provenance {
    pub kind: ProvenanceKind,
    pub collector: String,
    pub observed_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuotaWindow {
    pub id: String,
    pub label: String,
    pub used_fraction: Option<f64>,
    pub used_count: Option<f64>,
    pub limit_count: Option<f64>,
    pub resets_at_ms: Option<u64>,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageObservation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_id: Option<String>,
    pub provider: String,
    pub account_id: Option<String>,
    pub environment_id: String,
    pub model: Option<String>,
    pub project_id: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub requests: Option<u64>,
    pub cost_usd: Option<f64>,
    pub quota_windows: Vec<QuotaWindow>,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Working,
    Waiting,
    Done,
    Idle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSessionObservation {
    pub session_id: String,
    pub agent: String,
    pub provider: Option<String>,
    pub environment_id: String,
    pub project_id: Option<String>,
    pub model: Option<String>,
    pub state: AgentState,
    pub state_since_ms: Option<u64>,
    pub attention_reason: Option<String>,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    QueryStarted,
    ModelCompleted,
    ToolCompleted,
    QueryCompleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivityObservation {
    pub id: String,
    pub kind: ActivityKind,
    pub session_id: String,
    pub agent: String,
    pub environment_id: String,
    pub project_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub skill_id: Option<String>,
    pub tool_name: Option<String>,
    pub decision: Option<String>,
    pub permission: Option<String>,
    pub error: Option<String>,
    pub latency_ms: Option<u64>,
    /// Producer timestamp retained as reported. History normalization/parsing is deferred.
    pub occurred_at: String,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PerformanceObservation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_id: Option<String>,
    pub source_id: String,
    pub environment_id: String,
    pub model: Option<String>,
    pub latency_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub tokens_per_second: Option<f64>,
    pub context_used_tokens: Option<u64>,
    pub context_limit_tokens: Option<u64>,
    pub ram_bytes: Option<u64>,
    pub vram_bytes: Option<u64>,
    pub queue_depth: Option<u32>,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TelemetrySnapshot {
    pub protocol: String,
    pub usage: Vec<UsageObservation>,
    pub sessions: Vec<AgentSessionObservation>,
    pub activity: Vec<ActivityObservation>,
    pub performance: Vec<PerformanceObservation>,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "observation", rename_all = "snake_case")]
pub enum HistoryPayload {
    Usage(UsageObservation),
    Session(AgentSessionObservation),
    Activity(ActivityObservation),
    Performance(PerformanceObservation),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelemetryHistoryRecord {
    pub protocol: String,
    pub key: String,
    pub recorded_at_ms: u64,
    pub payload: HistoryPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelemetryHistorySnapshot {
    pub protocol: String,
    pub records: Vec<TelemetryHistoryRecord>,
}

#[derive(Debug, Default)]
struct HistoryIndex {
    last_by_key: BTreeMap<String, String>,
}

static HISTORY_INDEX: OnceLock<Mutex<HistoryIndex>> = OnceLock::new();

pub fn history_path() -> PathBuf {
    crate::config::config_path().with_file_name("telemetry-history.jsonl")
}

fn normalize_payload_for_fingerprint(payload: &HistoryPayload) -> HistoryPayload {
    let mut payload = payload.clone();
    match &mut payload {
        HistoryPayload::Usage(observation) => {
            observation.provenance.observed_at_ms = 0;
            for window in &mut observation.quota_windows {
                window.provenance.observed_at_ms = 0;
            }
        }
        HistoryPayload::Session(observation) => {
            observation.provenance.observed_at_ms = 0;
        }
        HistoryPayload::Activity(observation) => {
            observation.provenance.observed_at_ms = 0;
        }
        HistoryPayload::Performance(observation) => {
            observation.provenance.observed_at_ms = 0;
        }
    }
    payload
}

fn record_fingerprint(payload: &HistoryPayload) -> String {
    serde_json::to_string(&normalize_payload_for_fingerprint(payload)).unwrap_or_default()
}

fn history_records_from_snapshot(snapshot: &TelemetrySnapshot) -> Vec<TelemetryHistoryRecord> {
    let recorded_at_ms = now_ms();
    let mut records = Vec::new();

    records.extend(snapshot.usage.iter().cloned().map(|observation| {
        let key = format!("usage:{}", usage_key(&observation));
        TelemetryHistoryRecord {
            protocol: TELEMETRY_HISTORY_PROTOCOL.into(),
            key,
            recorded_at_ms,
            payload: HistoryPayload::Usage(observation),
        }
    }));
    records.extend(snapshot.sessions.iter().cloned().map(|observation| {
        let key = format!("session:{}", session_key(&observation));
        TelemetryHistoryRecord {
            protocol: TELEMETRY_HISTORY_PROTOCOL.into(),
            key,
            recorded_at_ms,
            payload: HistoryPayload::Session(observation),
        }
    }));
    records.extend(snapshot.activity.iter().cloned().map(|observation| {
        let key = format!("activity:{}:{}", observation.agent, observation.id);
        TelemetryHistoryRecord {
            protocol: TELEMETRY_HISTORY_PROTOCOL.into(),
            key,
            recorded_at_ms,
            payload: HistoryPayload::Activity(observation),
        }
    }));
    records.extend(snapshot.performance.iter().cloned().map(|observation| {
        let key = format!("performance:{}", performance_key(&observation));
        TelemetryHistoryRecord {
            protocol: TELEMETRY_HISTORY_PROTOCOL.into(),
            key,
            recorded_at_ms,
            payload: HistoryPayload::Performance(observation),
        }
    }));

    records
}

fn load_history_index() -> HistoryIndex {
    let path = history_path();
    let Ok(file) = std::fs::File::open(path) else {
        return HistoryIndex::default();
    };

    let mut index = HistoryIndex::default();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(record) = serde_json::from_str::<TelemetryHistoryRecord>(&line) else {
            continue;
        };
        if record.protocol != TELEMETRY_HISTORY_PROTOCOL {
            continue;
        }
        index
            .last_by_key
            .insert(record.key, record_fingerprint(&record.payload));
    }
    index
}

fn history_index() -> &'static Mutex<HistoryIndex> {
    HISTORY_INDEX.get_or_init(|| Mutex::new(load_history_index()))
}

pub fn append_history(snapshot: &TelemetrySnapshot) -> std::io::Result<usize> {
    let candidates = history_records_from_snapshot(snapshot);
    if candidates.is_empty() {
        return Ok(0);
    }

    let path = history_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    let mut index = history_index().lock().unwrap();
    let changed = candidates
        .into_iter()
        .filter_map(|record| {
            let fingerprint = record_fingerprint(&record.payload);
            let same = index
                .last_by_key
                .get(&record.key)
                .is_some_and(|previous| previous == &fingerprint);
            (!same).then_some((record, fingerprint))
        })
        .collect::<Vec<_>>();

    if changed.is_empty() {
        return Ok(0);
    }

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut written = 0;
    for (record, fingerprint) in changed {
        let line = serde_json::to_string(&record)
            .map_err(std::io::Error::other)?;
        writeln!(file, "{line}")?;
        index.last_by_key.insert(record.key, fingerprint);
        written += 1;
    }
    Ok(written)
}

pub fn read_history_tail(limit: usize) -> TelemetryHistorySnapshot {
    let limit = limit.clamp(1, 500);
    let path = history_path();
    let records = std::fs::File::open(path)
        .ok()
        .map(|file| {
            BufReader::new(file)
                .lines()
                .map_while(Result::ok)
                .filter_map(|line| serde_json::from_str::<TelemetryHistoryRecord>(&line).ok())
                .filter(|record| record.protocol == TELEMETRY_HISTORY_PROTOCOL)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let start = records.len().saturating_sub(limit);
    TelemetryHistorySnapshot {
        protocol: TELEMETRY_HISTORY_PROTOCOL.into(),
        records: records[start..].to_vec(),
    }
}

#[derive(Debug, Default)]
pub struct TelemetryState {
    usage: BTreeMap<String, UsageObservation>,
    sessions: BTreeMap<String, AgentSessionObservation>,
    activity: Vec<ActivityObservation>,
    performance: BTreeMap<String, PerformanceObservation>,
    updated_at_ms: u64,
}

impl TelemetryState {
    pub fn rebuild(
        &mut self,
        codex_accounts: &[codex::AccountUsage],
        claude_accounts: &[usage::ClaudeAccountUsage],
        nx: &nx_agent::Snapshot,
    ) {
        self.usage.clear();
        self.sessions.clear();
        self.activity.clear();
        self.performance.clear();

        for observation in codex_usage_observations(codex_accounts)
            .into_iter()
            .chain(claude_usage_observations(claude_accounts))
            .chain(nx_usage_observation(nx))
        {
            self.usage.insert(usage_key(&observation), observation);
        }

        for observation in nx_session_observations(nx) {
            self.sessions.insert(session_key(&observation), observation);
        }

        self.activity = nx_activity_observations(nx);

        for observation in nx_performance_observations(nx) {
            self.performance
                .insert(performance_key(&observation), observation);
        }

        self.updated_at_ms = now_ms();
    }

    pub fn merge_gateway(&mut self, gateway: &gateway_9router::Snapshot) {
        self.usage
            .retain(|_, observation| observation.provenance.collector != "9router");
        self.performance
            .retain(|_, observation| observation.provenance.collector != "9router");

        for observation in gateway_usage_observations(gateway) {
            self.usage.insert(usage_key(&observation), observation);
        }
        for observation in gateway_performance_observations(gateway) {
            self.performance
                .insert(performance_key(&observation), observation);
        }
        self.updated_at_ms = now_ms();
    }

    pub fn snapshot(&self) -> TelemetrySnapshot {
        TelemetrySnapshot {
            protocol: TELEMETRY_PROTOCOL.to_string(),
            usage: self.usage.values().cloned().collect(),
            sessions: self.sessions.values().cloned().collect(),
            activity: self.activity.clone(),
            performance: self.performance.values().cloned().collect(),
            updated_at_ms: self.updated_at_ms,
        }
    }
}

fn usage_key(observation: &UsageObservation) -> String {
    if let Some(id) = observation.observation_id.as_deref() {
        return format!("observation|{id}");
    }
    format!(
        "{}|{}|{}|{}|{}",
        observation.provider,
        observation.account_id.as_deref().unwrap_or("-"),
        observation.environment_id,
        observation.model.as_deref().unwrap_or("-"),
        observation.project_id.as_deref().unwrap_or("-"),
    )
}

fn session_key(observation: &AgentSessionObservation) -> String {
    format!(
        "{}|{}|{}",
        observation.agent, observation.environment_id, observation.session_id
    )
}

fn performance_key(observation: &PerformanceObservation) -> String {
    if let Some(id) = observation.observation_id.as_deref() {
        return format!("observation|{id}");
    }
    format!("{}|{}", observation.environment_id, observation.source_id)
}

fn profile_environment_id(profile_key: &str) -> String {
    profile_key
        .split_once('/')
        .map(|(environment, _)| environment)
        .unwrap_or(profile_key)
        .to_string()
}

fn provider_provenance(
    collector: &str,
    source_kind: &str,
    observed_at_ms: u64,
    derived: bool,
) -> Provenance {
    let kind = if derived {
        ProvenanceKind::Derived
    } else {
        match source_kind {
            "live" => ProvenanceKind::Official,
            "local" | "cached" => ProvenanceKind::LocalObservation,
            _ => ProvenanceKind::LocalObservation,
        }
    };

    Provenance {
        kind,
        collector: collector.to_string(),
        observed_at_ms,
    }
}

fn normalized_windows(
    windows: &[usage::LimitWindow],
    collector: &str,
    source_kind: &str,
    observed_at_ms: u64,
) -> Vec<QuotaWindow> {
    windows
        .iter()
        .map(|window| {
            let provenance =
                provider_provenance(collector, source_kind, observed_at_ms, window.derived);
            QuotaWindow {
                id: window.id.clone(),
                label: window.label.clone(),
                used_fraction: Some(window.used.clamp(0.0, 1.0)),
                used_count: window.count.map(|count| count as f64),
                limit_count: None,
                resets_at_ms: window.resets_at,
                provenance,
            }
        })
        .collect()
}

pub fn codex_usage_observations(accounts: &[codex::AccountUsage]) -> Vec<UsageObservation> {
    accounts
        .iter()
        .filter(|account| !account.snapshot.windows.is_empty())
        .map(|account| {
            let observed_at = account.snapshot.fetched_at;
            let environment_id = account
                .source_profile_key
                .as_deref()
                .unwrap_or(&account.selected_profile_key);
            UsageObservation {
                observation_id: None,
                provider: "codex".into(),
                account_id: Some(account.key.clone()),
                environment_id: profile_environment_id(environment_id),
                model: None,
                project_id: None,
                input_tokens: None,
                output_tokens: None,
                cached_tokens: None,
                reasoning_tokens: None,
                requests: None,
                cost_usd: None,
                quota_windows: normalized_windows(
                    &account.snapshot.windows,
                    "codex",
                    &account.source_kind,
                    observed_at,
                ),
                provenance: provider_provenance(
                    "codex",
                    &account.source_kind,
                    observed_at,
                    false,
                ),
            }
        })
        .collect()
}

pub fn claude_usage_observations(
    accounts: &[usage::ClaudeAccountUsage],
) -> Vec<UsageObservation> {
    accounts
        .iter()
        .filter(|account| !account.snapshot.windows.is_empty())
        .map(|account| {
            let observed_at = account.snapshot.fetched_at;
            let environment_id = account
                .source_profile_key
                .as_deref()
                .unwrap_or(&account.selected_profile_key);
            UsageObservation {
                observation_id: None,
                provider: "claude".into(),
                account_id: Some(account.key.clone()),
                environment_id: profile_environment_id(environment_id),
                model: None,
                project_id: None,
                input_tokens: None,
                output_tokens: None,
                cached_tokens: None,
                reasoning_tokens: None,
                requests: None,
                cost_usd: None,
                quota_windows: normalized_windows(
                    &account.snapshot.windows,
                    "claude",
                    &account.source_kind,
                    observed_at,
                ),
                provenance: provider_provenance(
                    "claude",
                    &account.source_kind,
                    observed_at,
                    false,
                ),
            }
        })
        .collect()
}

fn nx_environment(event: Option<&nx_agent::TelemetryEvent>) -> String {
    event
        .and_then(|event| event.environment_id.clone())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "source:nx-agent".into())
}

fn nx_project(event: Option<&nx_agent::TelemetryEvent>) -> Option<String> {
    event
        .and_then(|event| event.project_id.clone())
        .filter(|value| !value.trim().is_empty())
}

fn latest_model_event(snapshot: &nx_agent::Snapshot) -> Option<&nx_agent::TelemetryEvent> {
    snapshot
        .recent
        .iter()
        .rev()
        .find(|event| event.event_type == "model.completed")
}

fn sum_known_usage<F>(snapshot: &nx_agent::Snapshot, field: F) -> Option<u64>
where
    F: Fn(&nx_agent::Usage) -> Option<u64>,
{
    let mut saw_value = false;
    let total = snapshot
        .recent
        .iter()
        .filter(|event| event.event_type == "model.completed")
        .filter_map(|event| event.usage.as_ref())
        .filter_map(|usage| {
            let value = field(usage);
            saw_value |= value.is_some();
            value
        })
        .sum::<u64>();
    saw_value.then_some(total)
}

fn nx_context_event(snapshot: &nx_agent::Snapshot) -> Option<&nx_agent::TelemetryEvent> {
    snapshot
        .recent
        .iter()
        .rev()
        .find(|event| event.environment_id.is_some() || event.project_id.is_some())
        .or_else(|| snapshot.recent.last())
}

pub fn nx_usage_observation(
    snapshot: &nx_agent::Snapshot,
) -> std::option::IntoIter<UsageObservation> {
    let latest = latest_model_event(snapshot);
    let context = nx_context_event(snapshot);
    let model_calls = snapshot
        .recent
        .iter()
        .filter(|event| event.event_type == "model.completed")
        .count() as u64;

    let observation = latest.map(|latest_model| UsageObservation {
            observation_id: None,
        provider: latest_model
            .provider
            .clone()
            .unwrap_or_else(|| "nx-agent".into()),
        account_id: None,
        environment_id: nx_environment(context),
        model: latest_model.model.clone(),
        project_id: nx_project(context),
        input_tokens: sum_known_usage(snapshot, |usage| usage.input_tokens),
        output_tokens: sum_known_usage(snapshot, |usage| usage.output_tokens),
        cached_tokens: sum_known_usage(snapshot, |usage| usage.cached_tokens),
        reasoning_tokens: sum_known_usage(snapshot, |usage| usage.reasoning_tokens),
        requests: Some(model_calls),
        cost_usd: None,
        quota_windows: Vec::new(),
        provenance: Provenance {
            kind: ProvenanceKind::LocalObservation,
            collector: "nx-agent".into(),
            observed_at_ms: now_ms(),
        },
    });
    observation.into_iter()
}

pub fn nx_session_observations(snapshot: &nx_agent::Snapshot) -> Vec<AgentSessionObservation> {
    let mut latest_by_session: BTreeMap<(String, String), &nx_agent::TelemetryEvent> =
        BTreeMap::new();
    for event in &snapshot.recent {
        latest_by_session.insert((nx_environment(Some(event)), event.session_id.clone()), event);
    }

    latest_by_session
        .into_values()
        .map(|event| {
            let state = match event.event_type.as_str() {
                "query.completed" => AgentState::Done,
                "query.started" | "model.completed" | "tool.completed" => AgentState::Working,
                _ => AgentState::Idle,
            };
            AgentSessionObservation {
                session_id: event.session_id.clone(),
                agent: "nx-agent".into(),
                provider: event.provider.clone(),
                environment_id: nx_environment(Some(event)),
                project_id: nx_project(Some(event)),
                model: event.model.clone(),
                state,
                state_since_ms: None,
                attention_reason: event.error.clone(),
                provenance: Provenance {
                    kind: ProvenanceKind::LocalObservation,
                    collector: "nx-agent".into(),
                    observed_at_ms: now_ms(),
                },
            }
        })
        .collect()
}

pub fn nx_activity_observations(snapshot: &nx_agent::Snapshot) -> Vec<ActivityObservation> {
    snapshot
        .recent
        .iter()
        .filter_map(|event| {
            let kind = match event.event_type.as_str() {
                "query.started" => ActivityKind::QueryStarted,
                "model.completed" => ActivityKind::ModelCompleted,
                "tool.completed" => ActivityKind::ToolCompleted,
                "query.completed" => ActivityKind::QueryCompleted,
                _ => return None,
            };

            Some(ActivityObservation {
                id: event.id.clone(),
                kind,
                session_id: event.session_id.clone(),
                agent: "nx-agent".into(),
                environment_id: nx_environment(Some(event)),
                project_id: nx_project(Some(event)),
                provider: event.provider.clone(),
                model: event.model.clone(),
                skill_id: event.skill_id.clone(),
                tool_name: event.tool_name.clone(),
                decision: event.decision.clone(),
                permission: event.permission.clone(),
                error: event.error.clone(),
                latency_ms: event.latency_ms,
                occurred_at: event.timestamp.clone(),
                provenance: Provenance {
                    kind: ProvenanceKind::LocalObservation,
                    collector: "nx-agent".into(),
                    observed_at_ms: now_ms(),
                },
            })
        })
        .collect()
}

pub fn gateway_usage_observations(
    snapshot: &gateway_9router::Snapshot,
) -> Vec<UsageObservation> {
    if snapshot.status != "ok" {
        return Vec::new();
    }
    snapshot
        .usage
        .iter()
        .map(|sample| UsageObservation {
            observation_id: Some(sample.observation_id.clone()),
            provider: sample.provider.clone(),
            account_id: None,
            environment_id: snapshot.source_environment_id.clone(),
            model: Some(sample.model.clone()),
            project_id: None,
            input_tokens: Some(sample.input_tokens),
            output_tokens: Some(sample.output_tokens),
            cached_tokens: Some(sample.cached_tokens),
            reasoning_tokens: None,
            requests: Some(sample.requests),
            cost_usd: Some(sample.cost_usd),
            quota_windows: Vec::new(),
            provenance: Provenance {
                kind: ProvenanceKind::LocalObservation,
                collector: "9router".into(),
                observed_at_ms: snapshot.fetched_at_ms,
            },
        })
        .collect()
}

pub fn gateway_performance_observations(
    snapshot: &gateway_9router::Snapshot,
) -> Vec<PerformanceObservation> {
    if snapshot.status != "ok" {
        return Vec::new();
    }
    snapshot
        .performance
        .iter()
        .map(|sample| PerformanceObservation {
            observation_id: Some(sample.observation_id.clone()),
            source_id: format!("9router:{}", sample.observation_id),
            environment_id: snapshot.source_environment_id.clone(),
            model: sample.model.clone(),
            latency_ms: sample.latency_ms,
            ttft_ms: sample.ttft_ms,
            tokens_per_second: None,
            context_used_tokens: None,
            context_limit_tokens: None,
            ram_bytes: None,
            vram_bytes: None,
            queue_depth: None,
            provenance: Provenance {
                kind: ProvenanceKind::LocalObservation,
                collector: "9router".into(),
                observed_at_ms: snapshot.fetched_at_ms,
            },
        })
        .collect()
}

pub fn nx_performance_observations(
    snapshot: &nx_agent::Snapshot,
) -> Vec<PerformanceObservation> {
    snapshot
        .recent
        .iter()
        .rev()
        .filter(|event| event.latency_ms.is_some())
        .take(1)
        .map(|event| PerformanceObservation {
            observation_id: None,
            source_id: format!("{}:{}", event.session_id, event.query_id),
            environment_id: nx_environment(Some(event)),
            model: event.model.clone(),
            latency_ms: event.latency_ms,
            ttft_ms: None,
            tokens_per_second: None,
            context_used_tokens: None,
            context_limit_tokens: None,
            ram_bytes: None,
            vram_bytes: None,
            queue_depth: None,
            provenance: Provenance {
                kind: ProvenanceKind::LocalObservation,
                collector: "nx-agent".into(),
                observed_at_ms: now_ms(),
            },
        })
        .collect()
}

pub fn refresh_from_app(app: &AppHandle) -> TelemetrySnapshot {
    let (codex_accounts, claude_accounts, nx_snapshot, gateway_snapshot) = {
        let state = app.state::<AppState>();
        let codex_accounts = state.codex_account_usage.lock().unwrap().clone();
        let claude_accounts = state.claude_account_usage.lock().unwrap().clone();
        let nx_snapshot = state.nx_agent.lock().unwrap().snapshot();
        let gateway_snapshot = state.gateway_9router.lock().unwrap().clone();
        (codex_accounts, claude_accounts, nx_snapshot, gateway_snapshot)
    };

    let snapshot = {
        let state = app.state::<AppState>();
        let mut telemetry = state.telemetry.lock().unwrap();
        telemetry.rebuild(&codex_accounts, &claude_accounts, &nx_snapshot);
        telemetry.merge_gateway(&gateway_snapshot);
        telemetry.snapshot()
    };

    if let Err(error) = append_history(&snapshot) {
        eprintln!("[runoptic] telemetry history append failed: {error}");
    }

    let _ = app.emit("telemetry-state", &snapshot);
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: &str, used: f64) -> usage::LimitWindow {
        usage::LimitWindow {
            id: id.into(),
            label: id.into(),
            used,
            resets_at: Some(123),
            count: None,
            derived: false,
            group: None,
        }
    }

    #[test]
    fn snapshot_exposes_versioned_consumer_protocol() {
        let snapshot = TelemetryState::default().snapshot();
        assert_eq!(snapshot.protocol, TELEMETRY_PROTOCOL);
    }

    #[test]
    fn codex_adapter_preserves_unknown_token_fields() {
        let account = codex::AccountUsage {
            key: "account-a".into(),
            profile_keys: vec!["windows-native/codex".into()],
            selected_profile_key: "windows-native/codex".into(),
            plan: Some("plus".into()),
            source_kind: "live".into(),
            source_profile_key: Some("windows-native/codex".into()),
            snapshot: usage::UsageSnapshot {
                status: "ok".into(),
                windows: vec![window("5h", 0.25)],
                fetched_at: 42,
                note: String::new(),
                backoff_until: 0,
            },
        };

        let observations = codex_usage_observations(&[account]);
        assert_eq!(observations.len(), 1);
        let observation = &observations[0];
        assert_eq!(observation.environment_id, "windows-native");
        assert_eq!(observation.input_tokens, None);
        assert_eq!(observation.output_tokens, None);
        assert_eq!(observation.cost_usd, None);
        assert_eq!(observation.quota_windows[0].used_fraction, Some(0.25));
        assert_eq!(
            observation.provenance.kind,
            ProvenanceKind::Official
        );
    }

    #[test]
    fn claude_adapter_keeps_wsl_environment_identity() {
        let account = usage::ClaudeAccountUsage {
            key: "claude-account-a".into(),
            profile_keys: vec!["wsl:ubuntu-24.04/claude".into()],
            selected_profile_key: "wsl:ubuntu-24.04/claude".into(),
            source_kind: "cached".into(),
            source_profile_key: Some("wsl:ubuntu-24.04/claude".into()),
            snapshot: usage::UsageSnapshot {
                status: "stale".into(),
                windows: vec![window("weekly", 0.5)],
                fetched_at: 84,
                note: String::new(),
                backoff_until: 0,
            },
        };

        let observations = claude_usage_observations(&[account]);
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].environment_id, "wsl:ubuntu-24.04");
        assert_eq!(
            observations[0].provenance.kind,
            ProvenanceKind::LocalObservation
        );
    }

    fn nx_event(kind: &str, session: &str, environment: Option<&str>) -> nx_agent::TelemetryEvent {
        nx_agent::TelemetryEvent {
            protocol: "nx.telemetry.v1".into(),
            id: format!("{session}-{kind}"),
            timestamp: "2026-09-30T12:00:00.000Z".into(),
            event_type: kind.into(),
            session_id: session.into(),
            query_id: "query-1".into(),
            surface: Some("cli".into()),
            mode: Some("default".into()),
            provider: Some("9router".into()),
            model: Some("gpt-test".into()),
            latency_ms: None,
            usage: None,
            skill_id: None,
            tool_name: None,
            decision: None,
            permission: None,
            error: None,
            tool_count: None,
            environment_id: environment.map(str::to_string),
            project_id: Some("runoptic".into()),
        }
    }

    #[test]
    fn nx_usage_keeps_unreported_token_fields_unknown() {
        let mut collector = nx_agent::Collector::default();
        let mut event = nx_event("model.completed", "s1", Some("wsl:ubuntu-24.04"));
        event.usage = Some(nx_agent::Usage {
            input_tokens: Some(120),
            output_tokens: None,
            total_tokens: None,
            cached_tokens: None,
            reasoning_tokens: None,
        });
        collector.ingest(event);

        let snapshot = collector.snapshot();
        let observation = nx_usage_observation(&snapshot).next().unwrap();

        assert_eq!(observation.input_tokens, Some(120));
        assert_eq!(observation.output_tokens, None);
        assert_eq!(observation.cached_tokens, None);
        assert_eq!(observation.reasoning_tokens, None);
        assert_eq!(observation.requests, Some(1));
    }

    #[test]
    fn nx_query_without_model_usage_emits_no_usage_observation() {
        let mut collector = nx_agent::Collector::default();
        collector.ingest(nx_event("query.started", "s1", Some("windows-native")));

        let snapshot = collector.snapshot();
        assert!(nx_usage_observation(&snapshot).next().is_none());
    }

    #[test]
    fn nx_adapter_uses_isolated_source_identity_when_environment_is_unknown() {
        let mut collector = nx_agent::Collector::default();
        collector.ingest(nx_event("query.started", "s1", None));
        let snapshot = collector.snapshot();

        let sessions = nx_session_observations(&snapshot);
        assert_eq!(sessions[0].environment_id, "source:nx-agent");
    }

    #[test]
    fn nx_activity_preserves_tool_decision_and_context() {
        let mut collector = nx_agent::Collector::default();
        let mut event = nx_event("tool.completed", "s1", Some("wsl:ubuntu-24.04"));
        event.skill_id = Some("github".into());
        event.tool_name = Some("merge".into());
        event.decision = Some("denied".into());
        event.permission = Some("write".into());
        collector.ingest(event);

        let activity = nx_activity_observations(&collector.snapshot());

        assert_eq!(activity.len(), 1);
        assert_eq!(activity[0].kind, ActivityKind::ToolCompleted);
        assert_eq!(activity[0].environment_id, "wsl:ubuntu-24.04");
        assert_eq!(activity[0].project_id.as_deref(), Some("runoptic"));
        assert_eq!(activity[0].skill_id.as_deref(), Some("github"));
        assert_eq!(activity[0].tool_name.as_deref(), Some("merge"));
        assert_eq!(activity[0].decision.as_deref(), Some("denied"));
        assert_eq!(activity[0].permission.as_deref(), Some("write"));
    }

    #[test]
    fn telemetry_snapshot_includes_normalized_activity() {
        let snapshot = nx_agent::Snapshot {
            protocol: "nx.telemetry.v1",
            totals: nx_agent::Totals::default(),
            recent: vec![nx_event(
                "query.started",
                "session-a",
                Some("windows-native"),
            )],
        };
        let mut state = TelemetryState::default();
        state.rebuild(&[], &[], &snapshot);
        let current = state.snapshot();

        assert_eq!(current.activity.len(), 1);
        assert_eq!(current.activity[0].kind, ActivityKind::QueryStarted);
        assert_eq!(current.activity[0].agent, "nx-agent");
    }

    #[test]
    fn history_fingerprint_ignores_provenance_clock_only() {
        let mut first = nx_event("query.started", "s1", Some("windows-native"));
        first.id = "activity-1".into();
        let mut collector = nx_agent::Collector::default();
        collector.ingest(first);
        let activity = nx_activity_observations(&collector.snapshot()).remove(0);

        let mut later = activity.clone();
        later.provenance.observed_at_ms = later.provenance.observed_at_ms.saturating_add(60_000);

        assert_eq!(
            record_fingerprint(&HistoryPayload::Activity(activity.clone())),
            record_fingerprint(&HistoryPayload::Activity(later.clone()))
        );

        later.project_id = Some("another-project".into());
        assert_ne!(
            record_fingerprint(&HistoryPayload::Activity(activity)),
            record_fingerprint(&HistoryPayload::Activity(later))
        );
    }

    #[test]
    fn history_records_use_stable_activity_identity() {
        let mut collector = nx_agent::Collector::default();
        let mut event = nx_event("tool.completed", "s1", Some("windows-native"));
        event.id = "event-stable".into();
        collector.ingest(event);

        let mut state = TelemetryState::default();
        state.rebuild(&[], &[], &collector.snapshot());
        let records = history_records_from_snapshot(&state.snapshot());
        let activity = records
            .iter()
            .find(|record| matches!(record.payload, HistoryPayload::Activity(_)))
            .unwrap();

        assert_eq!(activity.key, "activity:nx-agent:event-stable");
        assert_eq!(activity.protocol, TELEMETRY_HISTORY_PROTOCOL);
    }

    #[test]
    fn gateway_usage_maps_tokens_requests_and_cost_without_quota_guessing() {
        let snapshot = gateway_9router::Snapshot {
            status: "ok".into(),
            source_environment_id: "wsl:ubuntu-24.04".into(),
            fetched_at_ms: 123,
            usage: vec![gateway_9router::UsageSample {
                observation_id: "9router:today:p:m".into(),
                provider: "p".into(),
                model: "m".into(),
                requests: 2,
                input_tokens: 100,
                output_tokens: 20,
                cached_tokens: 30,
                cost_usd: 0.5,
            }],
            ..Default::default()
        };

        let observations = gateway_usage_observations(&snapshot);
        assert_eq!(observations.len(), 1);
        let observation = &observations[0];
        assert_eq!(observation.observation_id.as_deref(), Some("9router:today:p:m"));
        assert_eq!(observation.environment_id, "wsl:ubuntu-24.04");
        assert_eq!(observation.input_tokens, Some(100));
        assert_eq!(observation.output_tokens, Some(20));
        assert_eq!(observation.cached_tokens, Some(30));
        assert_eq!(observation.requests, Some(2));
        assert_eq!(observation.cost_usd, Some(0.5));
        assert!(observation.quota_windows.is_empty());
        assert_eq!(observation.provenance.collector, "9router");
    }

    #[test]
    fn gateway_performance_preserves_request_identity() {
        let snapshot = gateway_9router::Snapshot {
            status: "ok".into(),
            source_environment_id: "gateway:9router".into(),
            fetched_at_ms: 456,
            performance: vec![gateway_9router::RequestPerformance {
                observation_id: "request-1".into(),
                provider: Some("p".into()),
                model: Some("m".into()),
                latency_ms: Some(900),
                ttft_ms: Some(120),
            }],
            ..Default::default()
        };

        let observations = gateway_performance_observations(&snapshot);
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].observation_id.as_deref(), Some("request-1"));
        assert_eq!(observations[0].latency_ms, Some(900));
        assert_eq!(observations[0].ttft_ms, Some(120));
    }

    #[test]
    fn current_state_never_collapses_identical_session_ids_across_environments() {
        let snapshot = nx_agent::Snapshot {
            protocol: "nx.telemetry.v1",
            totals: nx_agent::Totals::default(),
            recent: vec![
                nx_event("query.started", "same", Some("windows-native")),
                nx_event("query.started", "same", Some("wsl:ubuntu-24.04")),
            ],
        };

        let sessions = nx_session_observations(&snapshot);
        let mut state = TelemetryState::default();
        state.rebuild(&[], &[], &snapshot);
        let current = state.snapshot();

        assert_eq!(sessions.len(), 2);
        assert_eq!(current.sessions.len(), 2);
        assert_eq!(current.sessions[0].environment_id, "windows-native");
        assert_eq!(current.sessions[1].environment_id, "wsl:ubuntu-24.04");
    }
}
