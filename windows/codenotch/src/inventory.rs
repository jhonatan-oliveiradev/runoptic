use crate::{environment, profile};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeInventory {
    pub scanned_at_ms: u64,
    pub environment_report: environment::DiscoveryReport,
    pub profiles: Vec<profile::ToolProfile>,
}

pub fn scan() -> RuntimeInventory {
    let environment_report = environment::discover();
    let profiles = profile::discover(&environment_report);
    RuntimeInventory {
        scanned_at_ms: now_ms(),
        environment_report,
        profiles,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::now_ms;

    #[test]
    fn inventory_timestamp_is_epoch_milliseconds() {
        assert!(now_ms() > 1_700_000_000_000);
    }
}
