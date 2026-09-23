use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

const MAX_RECENT: usize = 500;
const MAX_TEXT: usize = 256;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, rename = "inputTokens")]
    pub input_tokens: Option<u64>,
    #[serde(default, rename = "outputTokens")]
    pub output_tokens: Option<u64>,
    #[serde(default, rename = "totalTokens")]
    pub total_tokens: Option<u64>,
    #[serde(default, rename = "cachedTokens")]
    pub cached_tokens: Option<u64>,
    #[serde(default, rename = "reasoningTokens")]
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryEvent {
    pub protocol: String,
    pub id: String,
    pub timestamp: String,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "queryId")]
    pub query_id: String,
    #[serde(default)]
    pub surface: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default, rename = "latencyMs")]
    pub latency_ms: Option<u64>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default, rename = "skillId")]
    pub skill_id: Option<String>,
    #[serde(default, rename = "toolName")]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub permission: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default, rename = "toolCount")]
    pub tool_count: Option<u64>,
}

impl TelemetryEvent {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.protocol != "nx.telemetry.v1" {
            return Err("unsupported protocol");
        }
        if self.id.is_empty() || self.id.len() > MAX_TEXT {
            return Err("invalid id");
        }
        if self.session_id.is_empty() || self.session_id.len() > MAX_TEXT {
            return Err("invalid sessionId");
        }
        if self.query_id.is_empty() || self.query_id.len() > MAX_TEXT {
            return Err("invalid queryId");
        }
        if self.timestamp.is_empty() || self.timestamp.len() > MAX_TEXT {
            return Err("invalid timestamp");
        }
        if !matches!(
            self.event_type.as_str(),
            "query.started" | "model.completed" | "tool.completed" | "query.completed"
        ) {
            return Err("unsupported event type");
        }
        for value in [
            self.surface.as_deref(),
            self.mode.as_deref(),
            self.provider.as_deref(),
            self.model.as_deref(),
            self.skill_id.as_deref(),
            self.tool_name.as_deref(),
            self.decision.as_deref(),
            self.permission.as_deref(),
            self.error.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if value.len() > MAX_TEXT {
                return Err("field too long");
            }
        }
        if let Some(mode) = self.mode.as_deref() {
            if !matches!(mode, "default" | "deep") {
                return Err("invalid mode");
            }
        }
        if let Some(decision) = self.decision.as_deref() {
            if !matches!(decision, "allowed" | "denied" | "failed") {
                return Err("invalid decision");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub queries_started: u64,
    pub queries_completed: u64,
    pub model_calls: u64,
    pub tool_calls: u64,
    pub tool_denied: u64,
    pub tool_failed: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
    pub last_event_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub protocol: &'static str,
    pub totals: Totals,
    pub recent: Vec<TelemetryEvent>,
}

#[derive(Default)]
pub struct Collector {
    recent: VecDeque<TelemetryEvent>,
    totals: Totals,
}

impl Collector {
    pub fn ingest(&mut self, event: TelemetryEvent) {
        match event.event_type.as_str() {
            "query.started" => self.totals.queries_started += 1,
            "query.completed" => self.totals.queries_completed += 1,
            "model.completed" => {
                self.totals.model_calls += 1;
                if let Some(usage) = &event.usage {
                    self.totals.input_tokens += usage.input_tokens.unwrap_or(0);
                    self.totals.output_tokens += usage.output_tokens.unwrap_or(0);
                    self.totals.cached_tokens += usage.cached_tokens.unwrap_or(0);
                    self.totals.reasoning_tokens += usage.reasoning_tokens.unwrap_or(0);
                }
            }
            "tool.completed" => {
                self.totals.tool_calls += 1;
                match event.decision.as_deref() {
                    Some("denied") => self.totals.tool_denied += 1,
                    Some("failed") => self.totals.tool_failed += 1,
                    _ => {}
                }
            }
            _ => {}
        }

        self.totals.last_event_at = Some(event.timestamp.clone());

        if self.recent.len() >= MAX_RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(event.clone());

        let _ = append_jsonl(&event);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            protocol: "nx.telemetry.v1",
            totals: self.totals.clone(),
            recent: self.recent.iter().cloned().collect(),
        }
    }
}

pub fn telemetry_path() -> PathBuf {
    crate::config::config_path().with_file_name("nx-agent-telemetry.jsonl")
}

fn append_jsonl(event: &TelemetryEvent) -> std::io::Result<()> {
    let path = telemetry_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    if let Ok(line) = serde_json::to_string(event) {
        writeln!(file, "{line}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str) -> TelemetryEvent {
        TelemetryEvent {
            protocol: "nx.telemetry.v1".into(),
            id: "event-1".into(),
            timestamp: "2026-09-22T23:00:00.000Z".into(),
            event_type: kind.into(),
            session_id: "session-1".into(),
            query_id: "query-1".into(),
            surface: Some("cli".into()),
            mode: Some("default".into()),
            provider: None,
            model: None,
            latency_ms: None,
            usage: None,
            skill_id: None,
            tool_name: None,
            decision: None,
            permission: None,
            error: None,
            tool_count: None,
        }
    }

    #[test]
    fn validates_protocol_and_event_types() {
        assert!(event("query.started").validate().is_ok());

        let mut invalid = event("made.up");
        assert_eq!(invalid.validate(), Err("unsupported event type"));

        invalid.event_type = "query.started".into();
        invalid.protocol = "other".into();
        assert_eq!(invalid.validate(), Err("unsupported protocol"));
    }

    #[test]
    fn aggregates_model_and_tool_usage() {
        let mut collector = Collector::default();

        let mut model = event("model.completed");
        model.provider = Some("9router".into());
        model.usage = Some(Usage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            cached_tokens: Some(40),
            reasoning_tokens: Some(5),
            total_tokens: Some(120),
        });
        collector.ingest(model);

        let mut tool = event("tool.completed");
        tool.decision = Some("denied".into());
        collector.ingest(tool);

        let snap = collector.snapshot();
        assert_eq!(snap.totals.model_calls, 1);
        assert_eq!(snap.totals.tool_calls, 1);
        assert_eq!(snap.totals.tool_denied, 1);
        assert_eq!(snap.totals.input_tokens, 100);
        assert_eq!(snap.totals.output_tokens, 20);
        assert_eq!(snap.totals.cached_tokens, 40);
        assert_eq!(snap.totals.reasoning_tokens, 5);
        assert_eq!(snap.recent.len(), 2);
    }
}
