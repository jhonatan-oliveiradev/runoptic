use serde::Serialize;
use std::path::PathBuf;
use std::process::{Command, Output};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentKind {
    WindowsNative,
    Wsl,
}

/// A filesystem/runtime boundary that collectors may inspect.
///
/// Credentials still belong to the provider tool. RunOptic only records where a tool's
/// files can be observed; it never copies credentials into this model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceEnvironment {
    pub id: String,
    pub kind: EnvironmentKind,
    pub label: String,
    pub home: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linux_home: Option<String>,
    pub reachable: bool,
    pub system: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveryReport {
    pub environments: Vec<SourceEnvironment>,
    pub wsl_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wsl_error: Option<String>,
}

pub fn discover() -> DiscoveryReport {
    let mut environments = vec![windows_native()];

    match discover_wsl() {
        Ok(mut wsl) => {
            environments.append(&mut wsl);
            DiscoveryReport {
                environments,
                wsl_available: true,
                wsl_error: None,
            }
        }
        Err(err) => DiscoveryReport {
            environments,
            wsl_available: false,
            wsl_error: Some(err),
        },
    }
}

fn windows_native() -> SourceEnvironment {
    let home = dirs::home_dir()
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));

    SourceEnvironment {
        id: "windows-native".into(),
        kind: EnvironmentKind::WindowsNative,
        label: "Windows".into(),
        reachable: home.exists(),
        home,
        distro: None,
        linux_home: None,
        system: false,
        diagnostic: None,
    }
}

fn discover_wsl() -> Result<Vec<SourceEnvironment>, String> {
    let output = run_wsl(&["--list", "--quiet"])?;
    if !output.status.success() {
        let err = decode_output(&output.stderr);
        return Err(format!(
            "wsl.exe --list --quiet failed{}",
            if err.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", err.trim())
            }
        ));
    }

    let distros = parse_wsl_list(&decode_output(&output.stdout));
    let mut out = Vec::with_capacity(distros.len());

    for distro in distros {
        out.push(resolve_wsl_environment(&distro));
    }

    Ok(out)
}

fn resolve_wsl_environment(distro: &str) -> SourceEnvironment {
    let id = format!("wsl:{}", distro.to_ascii_lowercase());
    let system = is_system_distro(distro);

    match resolve_linux_home(distro) {
        Ok(linux_home) => {
            let (home, reachable) = resolve_unc_home(distro, &linux_home);
            SourceEnvironment {
                id,
                kind: EnvironmentKind::Wsl,
                label: format!("WSL · {distro}"),
                home,
                distro: Some(distro.to_string()),
                linux_home: Some(linux_home),
                reachable,
                system,
                diagnostic: None,
            }
        }
        Err(err) => SourceEnvironment {
            id,
            kind: EnvironmentKind::Wsl,
            label: format!("WSL · {distro}"),
            home: PathBuf::from(format!(r"\\wsl.localhost\{distro}")),
            distro: Some(distro.to_string()),
            linux_home: None,
            reachable: false,
            system,
            diagnostic: Some(err),
        },
    }
}

fn resolve_linux_home(distro: &str) -> Result<String, String> {
    let output = run_wsl(&[
        "-d",
        distro,
        "--",
        "sh",
        "-lc",
        "printf '%s' \"$HOME\"",
    ])?;

    if !output.status.success() {
        let err = decode_output(&output.stderr);
        return Err(format!(
            "could not resolve HOME{}",
            if err.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", err.trim())
            }
        ));
    }

    let home = decode_output(&output.stdout)
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string();

    if !home.starts_with('/') {
        return Err(format!("unexpected HOME value: {home:?}"));
    }

    Ok(home)
}

fn resolve_unc_home(distro: &str, linux_home: &str) -> (PathBuf, bool) {
    let modern = unc_home(r"\\wsl.localhost", distro, linux_home);
    if modern.exists() {
        return (modern, true);
    }

    let legacy = unc_home(r"\\wsl$", distro, linux_home);
    if legacy.exists() {
        return (legacy, true);
    }

    // Keep the modern namespace as the deterministic path even if Explorer access is currently
    // unavailable. The diagnostic layer can distinguish "resolved" from "reachable".
    (modern, false)
}

fn unc_home(prefix: &str, distro: &str, linux_home: &str) -> PathBuf {
    let suffix = linux_home
        .trim()
        .trim_start_matches('/')
        .replace('/', r"\");
    if suffix.is_empty() {
        PathBuf::from(format!(r"{prefix}\{distro}"))
    } else {
        PathBuf::from(format!(r"{prefix}\{distro}\{suffix}"))
    }
}

pub fn is_system_distro(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "docker-desktop" | "docker-desktop-data"
    )
}

fn parse_wsl_list(text: &str) -> Vec<String> {
    let mut out = Vec::new();

    for raw in text.lines() {
        let name = raw
            .trim_matches(|c: char| c == '\0' || c == '\u{feff}' || c.is_whitespace())
            .trim_start_matches('*')
            .trim();

        if name.is_empty() || out.iter().any(|existing| existing == name) {
            continue;
        }

        out.push(name.to_string());
    }

    out
}

/// wsl.exe output is UTF-8 on some Windows builds and UTF-16LE on others, especially when stdout
/// is redirected. Detect the latter instead of accepting strings full of NUL bytes.
fn decode_output(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    let bom_utf16le = bytes.starts_with(&[0xff, 0xfe]);
    let odd_nuls = bytes
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|&&byte| byte == 0)
        .count();
    let looks_utf16le = bytes.len() >= 4 && odd_nuls * 4 >= bytes.len();

    if bom_utf16le || looks_utf16le {
        let start = if bom_utf16le { 2 } else { 0 };
        let units = bytes[start..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16_lossy(&units);
    }

    String::from_utf8_lossy(bytes).into_owned()
}

fn run_wsl(args: &[&str]) -> Result<Output, String> {
    let mut cmd = hidden_command("wsl.exe");
    cmd.args(args);
    cmd.output()
        .map_err(|err| format!("wsl.exe unavailable: {err}"))
}

#[cfg(windows)]
fn hidden_command(program: &str) -> Command {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new(program);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(not(windows))]
fn hidden_command(program: &str) -> Command {
    Command::new(program)
}

#[cfg(test)]
mod tests {
    use super::{decode_output, is_system_distro, parse_wsl_list, unc_home};
    use std::path::PathBuf;

    fn utf16le(s: &str) -> Vec<u8> {
        let mut out = vec![0xff, 0xfe];
        for unit in s.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }

    #[test]
    fn parses_utf8_wsl_list() {
        let got = parse_wsl_list("Ubuntu-24.04\r\nDebian\r\n");
        assert_eq!(got, vec!["Ubuntu-24.04", "Debian"]);
    }

    #[test]
    fn decodes_and_parses_utf16le_wsl_list() {
        let raw = utf16le("Ubuntu-24.04\r\nDocker-Desktop\r\n");
        let got = parse_wsl_list(&decode_output(&raw));
        assert_eq!(got, vec!["Ubuntu-24.04", "Docker-Desktop"]);
    }

    #[test]
    fn parser_tolerates_default_marker_and_duplicates() {
        let got = parse_wsl_list("* Ubuntu-24.04\nUbuntu-24.04\nDebian\n");
        assert_eq!(got, vec!["Ubuntu-24.04", "Debian"]);
    }

    #[test]
    fn linux_home_maps_to_modern_unc_namespace() {
        assert_eq!(
            unc_home(r"\\wsl.localhost", "Ubuntu-24.04", "/home/jhonatan"),
            PathBuf::from(r"\\wsl.localhost\Ubuntu-24.04\home\jhonatan")
        );
    }

    #[test]
    fn docker_desktop_is_classified_as_infrastructure() {
        assert!(is_system_distro("docker-desktop"));
        assert!(is_system_distro("Docker-Desktop"));
        assert!(!is_system_distro("Ubuntu-24.04"));
    }
}
