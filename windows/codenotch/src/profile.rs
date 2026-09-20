use crate::environment::{DiscoveryReport, EnvironmentKind, SourceEnvironment};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Codex,
    ClaudeCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolProfile {
    /// Stable, environment-qualified identity. Presentation/provider ids are deliberately deferred
    /// until collectors can consume profiles without breaking existing archives.
    pub key: String,
    pub tool: ToolKind,
    pub environment_id: String,
    pub display_name: String,
    pub config_dir: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    pub default_profile: bool,
}

pub fn discover(report: &DiscoveryReport) -> Vec<ToolProfile> {
    let mut out = Vec::new();

    for env in &report.environments {
        if env.system || !env.reachable {
            continue;
        }
        out.extend(discover_codex(env));
        out.extend(discover_claude(env));
    }

    out
}

fn discover_codex(env: &SourceEnvironment) -> Vec<ToolProfile> {
    const MARKERS: &[&str] = &[
        "auth.json",
        "config.toml",
        "sessions",
        "history.jsonl",
        "state_5.sqlite",
        "sqlite/codex-dev.db",
    ];

    let mut out = Vec::new();
    let default = env.home.join(".codex");

    if env.kind == EnvironmentKind::WindowsNative || has_any_marker(&default, MARKERS) {
        out.push(profile(env, ToolKind::Codex, None, default));
    }

    let mut extras = named_profiles(&env.home, ".codex-", MARKERS)
        .into_iter()
        .map(|(slug, dir)| profile(env, ToolKind::Codex, Some(slug), dir))
        .collect::<Vec<_>>();
    extras.sort_by(|a, b| a.slug.cmp(&b.slug));
    out.extend(extras);

    out
}

fn discover_claude(env: &SourceEnvironment) -> Vec<ToolProfile> {
    const DEFAULT_MARKERS: &[&str] = &[
        ".credentials.json",
        "settings.json",
        "sessions",
        "projects",
        "history.jsonl",
        ".claude.json",
    ];
    // Named Windows/WSL profiles are intentionally stricter than the default. A directory such as
    // ~/.claude-mem can look like Claude Code state, so require account/credential evidence before
    // presenting it as a second account. This can be relaxed later through explicit configuration.
    const NAMED_MARKERS: &[&str] = &[".credentials.json", ".claude.json"];

    let mut out = Vec::new();
    let default = env.home.join(".claude");

    if env.kind == EnvironmentKind::WindowsNative || has_any_marker(&default, DEFAULT_MARKERS) {
        out.push(profile(env, ToolKind::ClaudeCode, None, default));
    }

    let mut extras = named_profiles(&env.home, ".claude-", NAMED_MARKERS)
        .into_iter()
        .map(|(slug, dir)| profile(env, ToolKind::ClaudeCode, Some(slug), dir))
        .collect::<Vec<_>>();
    extras.sort_by(|a, b| a.slug.cmp(&b.slug));
    out.extend(extras);

    out
}

fn profile(
    env: &SourceEnvironment,
    tool: ToolKind,
    slug: Option<String>,
    config_dir: PathBuf,
) -> ToolProfile {
    let base = match tool {
        ToolKind::Codex => "codex",
        ToolKind::ClaudeCode => "claude",
    };
    let tool_name = match tool {
        ToolKind::Codex => "Codex",
        ToolKind::ClaudeCode => "Claude",
    };

    let local_id = slug
        .as_ref()
        .map(|slug| format!("{base}-{slug}"))
        .unwrap_or_else(|| base.to_string());
    let display_name = slug
        .as_ref()
        .map(|slug| format!("{tool_name} ({slug})"))
        .unwrap_or_else(|| tool_name.to_string());

    ToolProfile {
        key: format!("{}/{}", env.id, local_id),
        tool,
        environment_id: env.id.clone(),
        display_name,
        config_dir,
        default_profile: slug.is_none(),
        slug,
    }
}

fn named_profiles(home: &Path, prefix: &str, markers: &[&str]) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(home) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(slug) = slug_from_directory(&name, prefix) else {
            continue;
        };
        let dir = entry.path();
        if !dir.is_dir() || !has_any_marker(&dir, markers) {
            continue;
        }
        out.push((slug, dir));
    }
    out
}

fn slug_from_directory(name: &str, prefix: &str) -> Option<String> {
    let slug = name.strip_prefix(prefix)?;
    if slug.is_empty() {
        None
    } else {
        Some(slug.to_string())
    }
}

fn has_any_marker(dir: &Path, markers: &[&str]) -> bool {
    dir.is_dir() && markers.iter().any(|marker| dir.join(marker).exists())
}

pub fn probe(report: &DiscoveryReport) -> String {
    let profiles = discover(report);
    if profiles.is_empty() {
        return "  (no tool profiles discovered)".into();
    }

    profiles
        .iter()
        .map(|p| {
            let kind = match p.tool {
                ToolKind::Codex => "codex",
                ToolKind::ClaudeCode => "claude",
            };
            format!(
                "  {}: {} env={} dir={}",
                kind,
                if p.default_profile {
                    "default".to_string()
                } else {
                    p.slug.clone().unwrap_or_else(|| "?".into())
                },
                p.environment_id,
                p.config_dir.display()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::{discover, slug_from_directory, ToolKind};
    use crate::environment::{DiscoveryReport, EnvironmentKind, SourceEnvironment};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_home() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("runoptic-profile-test-{nonce}"));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn env(home: PathBuf, kind: EnvironmentKind, id: &str) -> SourceEnvironment {
        SourceEnvironment {
            id: id.into(),
            kind,
            label: id.into(),
            home,
            distro: None,
            linux_home: None,
            reachable: true,
            system: false,
            diagnostic: None,
        }
    }

    #[test]
    fn profile_slug_rejects_empty_and_unrelated_names() {
        assert_eq!(slug_from_directory(".codex-work", ".codex-").as_deref(), Some("work"));
        assert_eq!(slug_from_directory(".codex-", ".codex-"), None);
        assert_eq!(slug_from_directory("codex-work", ".codex-"), None);
    }

    #[test]
    fn wsl_only_reports_tools_with_local_evidence() {
        let home = temp_home();
        let report = DiscoveryReport {
            environments: vec![env(home.clone(), EnvironmentKind::Wsl, "wsl:ubuntu")],
            wsl_available: true,
            wsl_error: None,
        };
        assert!(discover(&report).is_empty());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn codex_named_profiles_follow_upstream_markers_and_order() {
        let home = temp_home();
        for (dir, marker) in [
            (".codex-work", "auth.json"),
            (".codex-alpha", "config.toml"),
            (".codex-empty", "README.md"),
        ] {
            let p = home.join(dir);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join(marker), b"").unwrap();
        }
        let report = DiscoveryReport {
            environments: vec![env(home.clone(), EnvironmentKind::Wsl, "wsl:ubuntu")],
            wsl_available: true,
            wsl_error: None,
        };
        let got = discover(&report);
        let keys = got
            .iter()
            .filter(|p| p.tool == ToolKind::Codex)
            .map(|p| p.key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(keys, vec!["wsl:ubuntu/codex-alpha", "wsl:ubuntu/codex-work"]);
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn named_claude_profiles_require_account_evidence() {
        let home = temp_home();
        let plugin = home.join(".claude-mem");
        std::fs::create_dir_all(plugin.join("sessions")).unwrap();

        let work = home.join(".claude-work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join(".credentials.json"), b"{}").unwrap();

        let report = DiscoveryReport {
            environments: vec![env(home.clone(), EnvironmentKind::Wsl, "wsl:ubuntu")],
            wsl_available: true,
            wsl_error: None,
        };
        let got = discover(&report);
        let claude = got
            .iter()
            .filter(|p| p.tool == ToolKind::ClaudeCode)
            .map(|p| p.key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(claude, vec!["wsl:ubuntu/claude-work"]);
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn native_defaults_stay_present_for_backward_compatibility() {
        let home = temp_home();
        let report = DiscoveryReport {
            environments: vec![env(home.clone(), EnvironmentKind::WindowsNative, "windows-native")],
            wsl_available: false,
            wsl_error: None,
        };
        let got = discover(&report);
        assert!(got.iter().any(|p| p.key == "windows-native/codex"));
        assert!(got.iter().any(|p| p.key == "windows-native/claude"));
        std::fs::remove_dir_all(home).ok();
    }
}
