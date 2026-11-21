use ratatui::style::Color;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// Rust build: the warm "clay" theme of this build is the default; the original's default is "opencode".
pub const DEFAULT_THEME: &str = "clay";

// Rust build: the theme picked in this build is kept under its own key, so the shared `kv.json` keeps the original
// TUI's choice (`theme`) untouched.
pub const KV_THEME: &str = "rs.theme";

macro_rules! builtin {
    ($($name:literal),* $(,)?; $($own:literal),* $(,)?) => {
        pub const BUILTIN: &[(&str, &str)] = &[
            $(($name, include_str!(concat!("../../../../opencode-ts/packages/tui/src/theme/assets/", $name, ".json"))),)*
            $(($own, include_str!(concat!("../themes/", $own, ".json"))),)*
        ];
    };
}

builtin!(
    "aura", "ayu", "carbonfox", "catppuccin", "catppuccin-frappe", "catppuccin-macchiato", "cobalt2", "cursor", "dracula",
    "everforest", "flexoki", "github", "gruvbox", "kanagawa", "lucent-orng", "material", "matrix", "mercury", "monokai",
    "nightowl", "nord", "one-dark", "opencode", "orng", "osaka-jade", "palenight", "rosepine", "solarized", "synthwave84",
    "tokyonight", "vercel", "vesper", "zenburn";
    "clay",
);

fn default_source() -> &'static str {
    BUILTIN.iter().find(|(name, _)| *name == DEFAULT_THEME).map_or(BUILTIN[0].1, |(_, text)| text)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        let hex = text.strip_prefix('#')?;
        let digit = |i: usize| u8::from_str_radix(hex.get(i..i + 1)?, 16).ok();
        let pair = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
        match hex.len() {
            3 | 4 => {
                let a = if hex.len() == 4 { digit(3)? * 17 } else { 255 };
                Some(Self { r: digit(0)? * 17, g: digit(1)? * 17, b: digit(2)? * 17, a })
            }
            6 => Some(Self::rgb(pair(0)?, pair(2)?, pair(4)?)),
            8 => Some(Self { r: pair(0)?, g: pair(2)?, b: pair(4)?, a: pair(6)? }),
            _ => None,
        }
    }

    // A fully transparent color lets the terminal's own color show through.
    pub fn color(self) -> Color {
        if self.a == 0 { Color::Reset } else { Color::Rgb(self.r, self.g, self.b) }
    }

    pub fn luminance(self) -> f64 {
        (0.299 * f64::from(self.r) + 0.587 * f64::from(self.g) + 0.114 * f64::from(self.b)) / 255.0
    }
}

pub fn tint(base: Rgba, overlay: Rgba, alpha: f64) -> Rgba {
    let mix = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * alpha).round().clamp(0.0, 255.0) as u8;
    Rgba::rgb(mix(base.r, overlay.r), mix(base.g, overlay.g), mix(base.b, overlay.b))
}

pub(crate) fn ansi(code: u64) -> Rgba {
    const BASE: [&str; 16] = [
        "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0", "#808080", "#ff0000", "#00ff00",
        "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    match code {
        0..=15 => Rgba::from_hex(BASE[code as usize]).unwrap_or_default(),
        16..=231 => {
            let index = code - 16;
            let val = |x: u64| if x == 0 { 0 } else { (x * 40 + 55) as u8 };
            Rgba::rgb(val(index / 36), val(index / 6 % 6), val(index % 6))
        }
        232..=255 => {
            let gray = ((code - 232) * 10 + 8) as u8;
            Rgba::rgb(gray, gray, gray)
        }
        _ => Rgba::rgb(0, 0, 0),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

impl Mode {
    pub fn key(self) -> &'static str {
        match self {
            Mode::Dark => "dark",
            Mode::Light => "light",
        }
    }
}

macro_rules! theme {
    ($($field:ident => $key:literal),* $(,)?) => {
        #[derive(Debug, Clone, PartialEq)]
        pub struct Theme {
            $(pub $field: Rgba,)*
            pub selected_list_item_text: Rgba,
            pub background_menu: Rgba,
            pub thinking_opacity: f64,
            pub has_selected_list_item_text: bool,
        }

        pub const COLOR_KEYS: &[&str] = &[$($key),*];

        fn build(colors: &BTreeMap<String, Rgba>, json: &Map<String, Value>) -> Result<Theme, String> {
            let get = |key: &str| colors.get(key).copied().ok_or_else(|| format!("theme color \"{key}\" is missing"));
            let background = get("background")?;
            let element = get("backgroundElement")?;
            Ok(Theme {
                $($field: get($key)?,)*
                selected_list_item_text: colors.get("selectedListItemText").copied().unwrap_or(background),
                background_menu: colors.get("backgroundMenu").copied().unwrap_or(element),
                thinking_opacity: json.get("thinkingOpacity").and_then(Value::as_f64).unwrap_or(0.6),
                has_selected_list_item_text: colors.contains_key("selectedListItemText"),
            })
        }
    };
}

theme!(
    primary => "primary", secondary => "secondary", accent => "accent", error => "error", warning => "warning",
    success => "success", info => "info", text => "text", text_muted => "textMuted", background => "background",
    background_panel => "backgroundPanel", background_element => "backgroundElement", border => "border",
    border_active => "borderActive", border_subtle => "borderSubtle", diff_added => "diffAdded",
    diff_removed => "diffRemoved", diff_context => "diffContext", diff_hunk_header => "diffHunkHeader",
    diff_highlight_added => "diffHighlightAdded", diff_highlight_removed => "diffHighlightRemoved",
    diff_added_bg => "diffAddedBg", diff_removed_bg => "diffRemovedBg", diff_context_bg => "diffContextBg",
    diff_line_number => "diffLineNumber", diff_added_line_number_bg => "diffAddedLineNumberBg",
    diff_removed_line_number_bg => "diffRemovedLineNumberBg", markdown_text => "markdownText",
    markdown_heading => "markdownHeading", markdown_link => "markdownLink", markdown_link_text => "markdownLinkText",
    markdown_code => "markdownCode", markdown_block_quote => "markdownBlockQuote", markdown_emph => "markdownEmph",
    markdown_strong => "markdownStrong", markdown_horizontal_rule => "markdownHorizontalRule",
    markdown_list_item => "markdownListItem", markdown_list_enumeration => "markdownListEnumeration",
    markdown_image => "markdownImage", markdown_image_text => "markdownImageText",
    markdown_code_block => "markdownCodeBlock", syntax_comment => "syntaxComment", syntax_keyword => "syntaxKeyword",
    syntax_function => "syntaxFunction", syntax_variable => "syntaxVariable", syntax_string => "syntaxString",
    syntax_number => "syntaxNumber", syntax_type => "syntaxType", syntax_operator => "syntaxOperator",
    syntax_punctuation => "syntaxPunctuation",
);

impl Theme {
    // `selectedForeground`
    pub fn selected_foreground(&self, bg: Option<Rgba>) -> Rgba {
        if self.has_selected_list_item_text {
            return self.selected_list_item_text;
        }
        if self.background.a == 0 {
            return if bg.unwrap_or(self.primary).luminance() > 0.5 { Rgba::rgb(0, 0, 0) } else { Rgba::rgb(255, 255, 255) };
        }
        self.background
    }

    // Named theme color (agent colors may reference one).
    pub fn named(&self, key: &str) -> Option<Rgba> {
        Some(match key {
            "primary" => self.primary,
            "secondary" => self.secondary,
            "accent" => self.accent,
            "error" => self.error,
            "warning" => self.warning,
            "success" => self.success,
            "info" => self.info,
            _ => return None,
        })
    }
}

pub fn is_theme(value: &Value) -> bool {
    value.get("theme").is_some_and(Value::is_object)
}

// `resolveTheme`: defs first, then theme keys; `{dark, light}` variants; numbers are ANSI indexes.
pub fn resolve(json: &Value, mode: Mode) -> Result<Theme, String> {
    let empty = Map::new();
    let defs = json.get("defs").and_then(Value::as_object).unwrap_or(&empty);
    let theme = json.get("theme").and_then(Value::as_object).ok_or("theme object is missing")?;

    fn color(value: &Value, defs: &Map<String, Value>, theme: &Map<String, Value>, mode: Mode, chain: &mut Vec<String>) -> Result<Rgba, String> {
        match value {
            Value::String(text) if text == "transparent" || text == "none" => Ok(Rgba { r: 0, g: 0, b: 0, a: 0 }),
            Value::String(text) if text.starts_with('#') => Rgba::from_hex(text).ok_or_else(|| format!("invalid color \"{text}\"")),
            Value::String(name) => {
                if chain.contains(name) {
                    chain.push(name.clone());
                    return Err(format!("Circular color reference: {}", chain.join(" -> ")));
                }
                let next = defs
                    .get(name)
                    .or_else(|| theme.get(name))
                    .ok_or_else(|| format!("Color reference \"{name}\" not found in defs or theme"))?;
                chain.push(name.clone());
                let out = color(next, defs, theme, mode, chain);
                chain.pop();
                out
            }
            Value::Number(n) => Ok(ansi(n.as_u64().unwrap_or(256))),
            Value::Object(variant) => {
                let picked = variant.get(mode.key()).ok_or_else(|| format!("color variant \"{}\" is missing", mode.key()))?;
                color(picked, defs, theme, mode, chain)
            }
            _ => Err("invalid color value".into()),
        }
    }

    let mut colors = BTreeMap::new();
    for (key, value) in theme {
        if key == "thinkingOpacity" {
            continue;
        }
        colors.insert(key.clone(), color(value, defs, theme, mode, &mut Vec::new())?);
    }
    build(&colors, theme)
}

#[derive(Debug, Clone)]
pub struct Registry {
    themes: BTreeMap<String, Value>,
}

impl Registry {
    pub fn builtin() -> Self {
        let themes = BUILTIN.iter().filter_map(|(name, text)| Some((name.to_string(), serde_json::from_str(text).ok()?))).collect();
        Self { themes }
    }

    // Global config dir first, then `.opencode` of every ancestor from cwd up; later directories win.
    pub fn discover(config: &Path, cwd: &Path) -> Self {
        Self::discover_within(config, cwd, None)
    }

    // `stop` bounds the ancestor walk (inclusive); the original always walks to the filesystem root.
    pub fn discover_within(config: &Path, cwd: &Path, stop: Option<&Path>) -> Self {
        let mut registry = Self::builtin();
        let mut directories: Vec<PathBuf> = vec![config.to_path_buf()];
        for dir in cwd.ancestors() {
            directories.push(dir.join(".opencode"));
            if stop == Some(dir) {
                break;
            }
        }
        for directory in directories {
            let Ok(entries) = std::fs::read_dir(directory.join("themes")) else { continue };
            let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
            files.sort();
            for file in files {
                let Some(name) = file.file_stem().and_then(|s| s.to_str()) else { continue };
                let parsed = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
                if let Some(value) = parsed.filter(is_theme) {
                    registry.themes.insert(name.to_string(), value);
                }
            }
        }
        registry
    }

    pub fn has(&self, name: &str) -> bool {
        self.themes.contains_key(name)
    }

    pub fn insert(&mut self, name: &str, theme: Value) {
        self.themes.insert(name.to_string(), theme);
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.themes.keys().cloned().collect();
        names.sort_by_key(|name| name.to_lowercase());
        names
    }

    // Unknown or broken themes (and `system`, which needs a terminal palette) fall back to the default.
    pub fn resolve(&self, name: &str, mode: Mode) -> Theme {
        self.themes
            .get(name)
            .and_then(|json| resolve(json, mode).ok())
            .or_else(|| self.themes.get(DEFAULT_THEME).and_then(|json| resolve(json, mode).ok()))
            .unwrap_or_else(|| resolve(&serde_json::from_str(default_source()).unwrap_or_default(), mode).expect("built-in theme resolves"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_builtin_resolves_in_both_modes() {
        let registry = Registry::builtin();
        assert_eq!(registry.names().len(), 34);
        for (name, text) in BUILTIN {
            let value: Value = serde_json::from_str(text).unwrap();
            for mode in [Mode::Dark, Mode::Light] {
                resolve(&value, mode).unwrap_or_else(|e| panic!("{name}: {e}"));
            }
        }
        assert_eq!(BUILTIN[22].0, "opencode");
        assert!(registry.has(DEFAULT_THEME) && default_source().contains("Rust build"));
        // Clay: warm ivory/charcoal grounds with a clay accent.
        let (dark, light) = (registry.resolve("clay", Mode::Dark), registry.resolve("clay", Mode::Light));
        assert_eq!((dark.primary, light.background), (Rgba::rgb(0xd9, 0x77, 0x57), Rgba::rgb(0xfa, 0xf9, 0xf5)));
    }

    #[test]
    fn resolves_refs_variants_ansi_and_defaults() {
        let mut theme = Map::new();
        for key in COLOR_KEYS {
            theme.insert((*key).into(), json!("base"));
        }
        theme.insert("primary".into(), json!({"dark": "accentDef", "light": "#ffffff"}));
        theme.insert("secondary".into(), json!("primary"));
        theme.insert("error".into(), json!(196));
        theme.insert("background".into(), json!("none"));
        theme.insert("backgroundElement".into(), json!("#abc"));
        let value = json!({"defs": {"base": "#102030", "accentDef": "#fab283"}, "theme": theme});
        let dark = resolve(&value, Mode::Dark).unwrap();
        assert_eq!(dark.primary, Rgba::rgb(0xfa, 0xb2, 0x83));
        assert_eq!(dark.secondary, dark.primary);
        assert_eq!(dark.error, Rgba::rgb(255, 0, 0));
        assert_eq!(dark.text, Rgba::rgb(0x10, 0x20, 0x30));
        assert_eq!(dark.background.color(), Color::Reset);
        assert_eq!(dark.background_menu, Rgba::rgb(0xaa, 0xbb, 0xcc));
        assert_eq!(dark.selected_list_item_text, dark.background);
        assert!(!dark.has_selected_list_item_text);
        assert_eq!(dark.thinking_opacity, 0.6);
        assert_eq!(resolve(&value, Mode::Light).unwrap().primary, Rgba::rgb(255, 255, 255));
    }

    #[test]
    fn reports_cycles_and_missing_refs() {
        let mut theme = Map::new();
        for key in COLOR_KEYS {
            theme.insert((*key).into(), json!("#000000"));
        }
        theme.insert("primary".into(), json!("a"));
        let cyclic = json!({"defs": {"a": "b", "b": "a"}, "theme": theme.clone()});
        assert!(resolve(&cyclic, Mode::Dark).unwrap_err().contains("Circular color reference: a -> b -> a"));
        theme.insert("primary".into(), json!("nope"));
        assert!(resolve(&json!({"theme": theme}), Mode::Dark).unwrap_err().contains("not found"));
    }

    #[test]
    fn discovers_custom_themes_and_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        let cwd = dir.path().join("project").join("sub");
        std::fs::create_dir_all(config.join("themes")).unwrap();
        std::fs::create_dir_all(cwd.join(".opencode").join("themes")).unwrap();
        let mut custom: Value = serde_json::from_str(BUILTIN[22].1).unwrap();
        custom["theme"]["primary"] = json!("#010203");
        std::fs::write(config.join("themes").join("mine.json"), custom.to_string()).unwrap();
        std::fs::write(cwd.join(".opencode").join("themes").join("broken.json"), "{\"nope\":1}").unwrap();
        let registry = Registry::discover_within(&config, &cwd, Some(dir.path()));
        assert!(registry.has("mine") && !registry.has("broken"));
        assert_eq!(registry.resolve("mine", Mode::Dark).primary, Rgba::rgb(1, 2, 3));
        let fallback = registry.resolve("system", Mode::Dark);
        assert_eq!(fallback, registry.resolve(DEFAULT_THEME, Mode::Dark));
    }
}
