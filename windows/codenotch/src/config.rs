use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The Mac's notch sizes, as multiples of the designed size: Small, Medium, Large.
pub const SIZES: [f64; 3] = [0.8, 1.0, 1.25];

/// The nearest of `SIZES`, so a scale saved by the old 40–100 % slider still lands on a size that
/// exists. 0.9, halfway between Small and Medium, counts as Medium.
pub fn snap_scale(scale: f64) -> f64 {
    if scale < 0.9 {
        SIZES[0]
    } else if scale < 1.125 || !scale.is_finite() {
        SIZES[1]
    } else {
        SIZES[2]
    }
}

/// One ring on the notch: which provider. (A `window` key from older builds is ignored.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraySlot {
    pub provider: String,
}

/// User-facing colour tokens. RunOptic ships with a stable default theme, while Custom lets the
/// developer tune the monitor like a terminal or editor without changing provider semantics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemeConfig {
    #[serde(default = "default_theme_preset")]
    pub preset: String,
    #[serde(default = "default_theme_accent")]
    pub accent: String,
    #[serde(default = "default_theme_background")]
    pub background: String,
    #[serde(default = "default_theme_surface")]
    pub surface: String,
    #[serde(default = "default_theme_text")]
    pub text: String,
    #[serde(default = "default_theme_muted")]
    pub muted: String,
    #[serde(default = "default_theme_warning")]
    pub warning: String,
    #[serde(default = "default_theme_critical")]
    pub critical: String,
}

fn default_theme_preset() -> String { "runoptic".into() }
fn default_theme_accent() -> String { "#A7F432".into() }
fn default_theme_background() -> String { "#0B0D10".into() }
fn default_theme_surface() -> String { "#11161C".into() }
fn default_theme_text() -> String { "#F4F7F8".into() }
fn default_theme_muted() -> String { "#7F8995".into() }
fn default_theme_warning() -> String { "#F6B84A".into() }
fn default_theme_critical() -> String { "#FF5F68".into() }

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            preset: default_theme_preset(),
            accent: default_theme_accent(),
            background: default_theme_background(),
            surface: default_theme_surface(),
            text: default_theme_text(),
            muted: default_theme_muted(),
            warning: default_theme_warning(),
            critical: default_theme_critical(),
        }
    }
}

fn valid_hex(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value.as_bytes()[1..].iter().all(|b| b.is_ascii_hexdigit())
}

fn color_or(value: String, fallback: fn() -> String) -> String {
    if valid_hex(&value) { value.to_ascii_uppercase() } else { fallback() }
}

/// Keeps hand-edited config safe and deterministic. Selecting the RunOptic preset always restores
/// the official palette; Custom persists validated #RRGGBB values.
pub fn normalise_theme(mut theme: ThemeConfig) -> ThemeConfig {
    if theme.preset != "custom" {
        return ThemeConfig::default();
    }
    theme.preset = "custom".into();
    theme.accent = color_or(theme.accent, default_theme_accent);
    theme.background = color_or(theme.background, default_theme_background);
    theme.surface = color_or(theme.surface, default_theme_surface);
    theme.text = color_or(theme.text, default_theme_text);
    theme.muted = color_or(theme.muted, default_theme_muted);
    theme.warning = color_or(theme.warning, default_theme_warning);
    theme.critical = color_or(theme.critical, default_theme_critical);
    theme
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    /// "auto" | "zh" | "zh-Hant" | "en" | "ja" | "ko" | "ru" | "uk"
    #[serde(default = "default_lang")]
    pub lang: String,
    #[serde(default)]
    pub bar_x: Option<i32>,
    #[serde(default)]
    pub bar_y: Option<i32>,
    /// Logical width of the bar (wheel-adjustable, 220-520); None = default 360
    #[serde(default)]
    pub bar_w: Option<u32>,
    /// Allow dragging + wheel resizing (tray toggle, off by default to prevent accidental drags)
    #[serde(default)]
    pub drag_enabled: bool,
    /// Position of the notch along its edge: the window centre as a fraction of the monitor's height
    /// (left/right edges) or width (top/bottom edges), 0 = top/left, 1 = bottom/right, default 0.5;
    /// saved after a drag. Named `notch_y` from when the right edge was the only one, so an existing
    /// config keeps its place.
    #[serde(default = "default_notch_y")]
    pub notch_y: f64,
    /// Which screen edge the notch is pinned to: "right" (the default), "left", "top" or "bottom".
    #[serde(default = "default_notch_edge")]
    pub notch_edge: String,
    /// Which monitor the notch lives on, by the system's device name (`\\.\DISPLAY2`). None, or a
    /// name no longer attached, means the primary monitor — so unplugging a screen cannot strand it.
    #[serde(default)]
    pub notch_monitor: Option<String>,
    /// Notch size as a multiple of the designed size, one of `SIZES`.
    #[serde(default = "default_scale")]
    pub scale: f64,
    /// Where the weekly limit gets a ring of its own: "off", "inside" or "outside".
    #[serde(default = "default_weekly_ring")]
    pub weekly_ring: String,
    /// Which providers the notch itself shows, in order. Empty means every provider that has
    /// something to report — the original behaviour, and the default. Superseded by `notch_slots`,
    /// kept so an existing config migrates cleanly.
    #[serde(default)]
    pub notch_providers: Vec<String>,
    /// Which providers get a ring on the notch, in order. An empty list means every provider.
    #[serde(default)]
    pub notch_slots: Vec<TraySlot>,
    /// Antigravity's lane on the ring, as the Mac app's "Notch reads": "automatic", "5h" or "weekly"
    #[serde(default = "default_antigravity_limit")]
    pub antigravity_limit: String,
    /// The model family that choice looks at, as the Mac app's "Model data": "gemini" or "3p"
    #[serde(default = "default_antigravity_model")]
    pub antigravity_model: String,
    /// false = the pill is kept off the screen edge entirely; the tray icon is then the only way in
    #[serde(default = "yes")]
    pub notch_visible: bool,
    /// false = the tray icon is hidden. Refused while the notch is also hidden, because that would
    /// leave the app running with no way to reach it.
    #[serde(default = "yes")]
    pub tray_visible: bool,
    /// false = no arc above the notch to carry it by. Nothing is lost: Appearance → Edge moves it too.
    #[serde(default = "yes")]
    pub show_move_handle: bool,
    /// RunOptic palette. Missing in older configs -> official RunOptic theme.
    #[serde(default)]
    pub theme: ThemeConfig,
}

fn default_notch_y() -> f64 { 0.5 }
fn default_notch_edge() -> String { "right".into() }

/// The four edges, in the order Settings lists them.
pub const EDGES: [&str; 4] = ["left", "right", "top", "bottom"];

/// An unreadable edge means the right-hand one, the layout every earlier build used.
pub fn edge_or_right(value: &str) -> String {
    if EDGES.contains(&value) { value.to_string() } else { "right".into() }
}

/// True for the edges the notch stands upright on (the pill is a column); false for top and bottom,
/// where it lies flat (the pill is a row) and the window's width and height swap.
pub fn edge_is_vertical(edge: &str) -> bool {
    matches!(edge, "left" | "right")
}
fn default_scale() -> f64 { 1.0 }
fn default_weekly_ring() -> String { "off".into() }

/// A second arc changes how every reading looks, so an unreadable value means off rather than a
/// guess at what was meant.
pub fn weekly_ring_or_off(value: &str) -> String {
    match value {
        "inside" | "outside" => value.to_string(),
        _ => default_weekly_ring(),
    }
}
fn yes() -> bool { true }
fn default_antigravity_limit() -> String { "automatic".into() }
fn default_antigravity_model() -> String { "gemini".into() }
fn default_port() -> u16 { 48666 }
fn default_lang() -> String { "auto".into() }

impl Default for Config {
    fn default() -> Self {
        Self {
            port: default_port(),
            lang: default_lang(),
            bar_x: None,
            bar_y: None,
            bar_w: None,
            drag_enabled: false,
            notch_y: default_notch_y(),
            notch_edge: default_notch_edge(),
            notch_monitor: None,
            scale: default_scale(),
            weekly_ring: default_weekly_ring(),
            notch_providers: Vec::new(),
            notch_slots: Vec::new(),
            antigravity_limit: default_antigravity_limit(),
            antigravity_model: default_antigravity_model(),
            notch_visible: true,
            tray_visible: true,
            show_move_handle: true,
            theme: ThemeConfig::default(),
        }
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("runoptic")
        .join("config.json")
}

pub fn load() -> Config {
    let path = config_path();
    let raw = std::fs::read_to_string(&path).ok();
    let mut cfg: Config = raw
        .as_deref()
        .and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or_default();

    if cfg.notch_slots.is_empty() {
        cfg.notch_slots = cfg
            .notch_providers
            .iter()
            .map(|p| TraySlot { provider: p.clone() })
            .collect();
    }

    if !cfg.notch_visible && !cfg.tray_visible {
        cfg.tray_visible = true;
    }

    cfg.scale = snap_scale(cfg.scale);
    cfg.weekly_ring = weekly_ring_or_off(&cfg.weekly_ring);
    cfg.theme = normalise_theme(cfg.theme);
    cfg
}

pub fn save(cfg: &Config) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(txt) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(path, txt);
    }
}

#[cfg(test)]
mod tests {
    use super::{normalise_theme, snap_scale, weekly_ring_or_off, ThemeConfig};

    #[test]
    fn a_saved_scale_snaps_to_the_nearest_size() {
        assert_eq!(snap_scale(0.4), 0.8);
        assert_eq!(snap_scale(0.85), 0.8);
        assert_eq!(snap_scale(0.9), 1.0);
        assert_eq!(snap_scale(1.0), 1.0);
        assert_eq!(snap_scale(1.2), 1.25);
        assert_eq!(snap_scale(3.0), 1.25);
    }

    #[test]
    fn only_the_two_placements_are_kept() {
        assert_eq!(weekly_ring_or_off("inside"), "inside");
        assert_eq!(weekly_ring_or_off("outside"), "outside");
        assert_eq!(weekly_ring_or_off("Inside"), "off");
        assert_eq!(weekly_ring_or_off(""), "off");
    }

    #[test]
    fn runoptic_preset_restores_official_palette() {
        let mut t = ThemeConfig::default();
        t.preset = "runoptic".into();
        t.accent = "#123456".into();
        assert_eq!(normalise_theme(t), ThemeConfig::default());
    }

    #[test]
    fn custom_theme_rejects_invalid_colours() {
        let mut t = ThemeConfig::default();
        t.preset = "custom".into();
        t.accent = "lime".into();
        t.background = "#123abc".into();
        let t = normalise_theme(t);
        assert_eq!(t.accent, "#A7F432");
        assert_eq!(t.background, "#123ABC");
    }
}
