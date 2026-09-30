use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

use crate::{codex, nx_agent, usage, AppState};

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PerformanceObservation {
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
    pub usage: Vec<UsageObservation>,
    pub sessions: Vec<AgentSessionObservation>,
    pub performance: Vec<PerformanceObservation>,
    pub updated_at_ms: u64,
}

#[derive(Debug, Default)]
pub struct TelemetryState {
    usage: BTreeMap<String, UsageObservation>,
    sessions: BTreeMap<String, AgentSessionObservation>,
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

        for observation in nx_performance_observations(nx) {
            self.performance
                .insert(performance_key(&observation), observation);
        }

        self.updated_at_ms = now_ms();
    }

    pub fn snapshot(&self) -> TelemetrySnapshot {
        TelemetrySnapshot {
            usage: self.usage.values().cloned().collect(),
            sessions: self.sessions.values().cloned().collect(),
            performance: self.performance.values().cloned().collect(),
            updated_at_ms: self.updated_at_ms,
        }
    }
}

fn usage_key(observation: &UsageObservation) -> String {
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

pub fn nx_usage_observation(
    snapshot: &nx_agent::Snapshot,
) -> std::option::IntoIter<UsageObservation> {
    let latest = latest_model_event(snapshot);
    let has_evidence = !snapshot.recent.is_empty();
    let observation = has_evidence.then(|| UsageObservation {
        provider: latest
            .and_then(|event| event.provider.clone())
            .unwrap_or_else(|| "nx-agent".into()),
        account_id: None,
        environment_id: nx_environment(latest.or_else(|| snapshot.recent.last())),
        model: latest.and_then(|event| event.model.clone()),
        project_id: nx_project(latest.or_else(|| snapshot.recent.last())),
        input_tokens: Some(snapshot.totals.input_tokens),
        output_tokens: Some(snapshot.totals.output_tokens),
        cached_tokens: Some(snapshot.totals.cached_tokens),
        reasoning_tokens: Some(snapshot.totals.reasoning_tokens),
        requests: Some(snapshot.totals.model_calls),
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
    let (codex_accounts, claude_accounts, nx_snapshot) = {
        let state = app.state::<AppState>();
        let codex_accounts = state.codex_account_usage.lock().unwrap().clone();
        let claude_accounts = state.claude_account_usage.lock().unwrap().clone();
        let nx_snapshot = state.nx_agent.lock().unwrap().snapshot();
        (codex_accounts, claude_accounts, nx_snapshot)
    };

    let snapshot = {
        let state = app.state::<AppState>();
        let mut telemetry = state.telemetry.lock().unwrap();
        telemetry.rebuild(&codex_accounts, &claude_accounts, &nx_snapshot);
        telemetry.snapshot()
    };

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
    fn nx_adapter_uses_isolated_source_identity_when_environment_is_unknown() {
        let mut collector = nx_agent::Collector::default();
        collector.ingest(nx_event("query.started", "s1", None));
        let snapshot = collector.snapshot();

        let sessions = nx_session_observations(&snapshot);
        assert_eq!(sessions[0].environment_id, "source:nx-agent");
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
