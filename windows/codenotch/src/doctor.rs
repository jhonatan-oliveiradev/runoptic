//! `runoptic.exe doctor` — self-diagnosis: look instead of guessing.
//! Checks the config, port occupancy, watch roots, the newest session file and how its tail parses,
//! and writes to stdout plus %APPDATA%\runoptic\doctor.log.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

fn collect(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, SystemTime)>) {
    if depth > 10 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, depth + 1, out);
        } else if crate::watcher::is_session_jsonl(&p) {
            if let Ok(m) = e.metadata() {
                if let Ok(t) = m.modified() {
                    out.push((p, t));
                }
            }
        }
    }
}

fn age_secs(t: SystemTime) -> u64 {
    SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn run() -> String {
    let mut o = String::new();
    o += &format!("== RunOptic doctor v{} ==\n", env!("CARGO_PKG_VERSION"));

    let cfg = crate::config::load();
    o += &format!(
        "config: port={} lang={} ({})\n",
        cfg.port,
        cfg.lang,
        crate::config::config_path().display()
    );

    match std::net::TcpListener::bind(("127.0.0.1", cfg.port)) {
        Ok(_) => o += "port: free — no RunOptic instance is running\n",
        Err(_) => o += "port: in use — an instance is already running (quit it from the tray before starting a new build)\n",
    }

    for root in crate::watcher::roots() {
        if !root.exists() {
            o += &format!("root: {} [missing]\n", root.display());
            continue;
        }
        o += &format!("root: {} exists, scanning for the newest session…\n", root.display());
        let mut files = Vec::new();
        collect(&root, 0, &mut files);
        files.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
        if files.is_empty() {
            o += "  (no session transcripts)\n";
        }
        for (p, m) in files.into_iter().take(5) {
            o += &format!("  updated {}s ago  {}\n", age_secs(m), p.display());
            match crate::watcher::tail_entry(&p) {
                Some(v) => {
                    o += &format!(
                        "    tail parses OK: type={} sessionId={}\n",
                        v.get("type").and_then(|x| x.as_str()).unwrap_or("?"),
                        v.get("sessionId").and_then(|x| x.as_str()).unwrap_or("(missing, the file name will be used)")
                    );
                }
                None => o += "    tail failed to parse (no valid JSON in the last 30 lines — please report this file)\n",
            }
        }
    }

    let environment_report = crate::environment::discover();
    let profiles = crate::profile::discover(&environment_report);
    o += &format!("\nenvironments:\n{}\n", crate::environment::probe(&environment_report));
    o += &format!("\ntool profiles:\n{}\n", crate::profile::probe(&environment_report));

    let codex_observation_values = crate::codex::observe_profiles(&profiles);

    let codex_targets = codex_observation_values
        .iter()
        .map(|obs| {
            format!(
                "  {}: auth={} plan={} config={} rollout={}",
                obs.profile_key,
                obs.auth_status,
                obs.plan.as_deref().unwrap_or("?"),
                obs.config_dir.display(),
                obs.newest_rollout
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "none".into())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    o += &format!(
        "\ncodex collector targets:\n{}\n",
        if codex_targets.is_empty() {
            "  (no Codex profiles discovered)"
        } else {
            &codex_targets
        }
    );

    let codex_observations = codex_observation_values
        .iter()
        .map(|obs| {
            format!(
                "  {}: auth={} plan={} snapshot={} windows={} fetched_at={} rollout={}",
                obs.profile_key,
                obs.auth_status,
                obs.plan.as_deref().unwrap_or("?"),
                obs.snapshot.status,
                obs.snapshot.windows.len(),
                obs.snapshot.fetched_at,
                obs.newest_rollout
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "none".into())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    o += &format!(
        "\ncodex local observations:\n{}\n",
        if codex_observations.is_empty() {
            "  (no Codex observations)"
        } else {
            &codex_observations
        }
    );

    let codex_accounts = crate::codex::group_accounts(&profiles)
        .into_iter()
        .map(|group| {
            format!(
                "  {}: profiles=[{}] selected={} credential={} plan={}",
                group.key,
                group.profile_keys.join(", "),
                group.selected_profile_key,
                group.credential_state,
                group.plan.as_deref().unwrap_or("?")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    o += &format!(
        "\ncodex quota accounts:\n{}\n",
        if codex_accounts.is_empty() {
            "  (no authenticated Codex account identities)"
        } else {
            &codex_accounts
        }
    );

    let claude_observation_values = crate::usage::observe_claude_profiles(&profiles);
    let claude_observations = claude_observation_values
        .iter()
        .map(|obs| {
            format!(
                "  {}: auth={} account={} config={} credential={}",
                obs.profile_key,
                obs.auth_status,
                obs.account_key.as_deref().unwrap_or("unknown"),
                obs.config_dir.display(),
                obs.credential_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "none".into())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    o += &format!(
        "\nclaude collector targets:\n{}\n",
        if claude_observations.is_empty() {
            "  (no Claude profiles discovered)"
        } else {
            &claude_observations
        }
    );

    let claude_accounts = crate::usage::group_claude_accounts(&claude_observation_values)
        .into_iter()
        .map(|group| {
            format!(
                "  {}: profiles=[{}] selected={} credential={}",
                group.key,
                group.profile_keys.join(", "),
                group.selected_profile_key,
                group.credential_state
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    o += &format!(
        "\nclaude quota accounts:\n{}\n",
        if claude_accounts.is_empty() {
            "  (no Claude quota identities)"
        } else {
            &claude_accounts
        }
    );

    let claude_cli_targets = crate::usage::observe_claude_clis(&profiles)
        .into_iter()
        .map(|obs| {
            format!(
                "  {}: available={} command={} diagnostic={}",
                obs.profile_key,
                obs.available,
                obs.command.as_deref().unwrap_or("none"),
                obs.diagnostic
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    o += &format!(
        "\nclaude execution targets:\n{}\n",
        if claude_cli_targets.is_empty() {
            "  (no Claude execution targets)"
        } else {
            &claude_cli_targets
        }
    );

    let codex_usage_source = if codex_observation_values.is_empty() {
        "Codex: no profiles discovered".to_string()
    } else {
        let account_count = crate::codex::group_accounts(&profiles).len();
        let local_with_windows = codex_observation_values
            .iter()
            .filter(|obs| !obs.snapshot.windows.is_empty())
            .count();
        format!(
            "Codex: {} profiles | {} quota accounts | {} local snapshots",
            codex_observation_values.len(),
            account_count,
            local_with_windows
        )
    };
    o += &format!("\nusage sources:\n  {}\n  {}\n", crate::usage::probe_credentials(), codex_usage_source);
    o += &format!("  {}\n", crate::cursor::probe());
    o += &format!("  {}\n", crate::grok::probe());
    o += &format!("  {}\n", crate::antigravity::probe());
    o += &format!("\nprovider glyphs:\n{}\n", crate::glyphs::probe());
    o += &format!("\nworking state:\n  {}\n", crate::activity::probe());

    o += "\nwatch.log (the most recent watcher log, if any):\n";
    if let Some(dir) = dirs::config_dir() {
        let p = dir.join("runoptic").join("watch.log");
        match std::fs::read_to_string(&p) {
            Ok(t) if !t.trim().is_empty() => {
                for line in t.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev() {
                    o += &format!("  {}\n", line);
                }
            }
            _ => o += "  (empty — the app has not run yet, which is normal on first use, or an older build without the watcher)\n",
        }
    }
    o
}
