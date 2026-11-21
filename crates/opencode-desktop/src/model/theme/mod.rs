use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::{Map, Value, json};

macro_rules! theme_file {
    ($($part:literal),+) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../opencode-ts/packages/ui/src/theme/",
            $($part),+
        ))
    };
}

macro_rules! theme_sources {
    ($($id:literal),* $(,)?) => {
        &[$(($id, theme_file!("themes/", $id, ".json"))),*]
    };
}

mod color;
mod resolve;
mod v2;

// Rust build: the warm "clay" theme of this build is the default; the original's default is "oc-2".
pub const DEFAULT_THEME: &str = "clay";

// Rust build: themes that ship with this build only.
const OWN: &[(&str, &str)] = &[("clay", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/themes/clay.json")))];

const SOURCES: &[(&str, &str)] = theme_sources![
    "amoled",
    "aura",
    "ayu",
    "carbonfox",
    "catppuccin",
    "catppuccin-frappe",
    "catppuccin-macchiato",
    "cobalt2",
    "cursor",
    "dracula",
    "everforest",
    "flexoki",
    "github",
    "gruvbox",
    "kanagawa",
    "lucent-orng",
    "material",
    "matrix",
    "mercury",
    "monokai",
    "nightowl",
    "nord",
    "oc-2",
    "one-dark",
    "onedarkpro",
    "opencode",
    "orng",
    "osaka-jade",
    "palenight",
    "rosepine",
    "shadesofpurple",
    "solarized",
    "synthwave84",
    "tokyonight",
    "vercel",
    "vesper",
    "zenburn",
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    const BLACK: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
    const WHITE: Rgba = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };

    pub fn parse(text: &str) -> Option<Rgba> {
        let text = text.trim();
        if let Some(hex) = text.strip_prefix('#') {
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let digits: Vec<u32> = hex.chars().filter_map(|c| c.to_digit(16)).collect();
            let channels: Vec<u32> = match digits.len() {
                3 | 4 => digits.iter().map(|d| d * 17).collect(),
                6 | 8 => digits.chunks(2).map(|pair| pair[0] * 16 + pair[1]).collect(),
                _ => return None,
            };
            let at = |i: usize| channels.get(i).map_or(1.0, |v| *v as f32 / 255.0);
            return Some(Rgba { r: at(0), g: at(1), b: at(2), a: at(3) });
        }
        let inner = text
            .strip_prefix("rgba(")
            .or_else(|| text.strip_prefix("rgb("))?
            .strip_suffix(')')?;
        let parts: Vec<f32> = inner
            .split(',')
            .map(|part| part.trim().parse::<f32>().ok())
            .collect::<Option<_>>()?;
        if parts.len() != 3 && parts.len() != 4 {
            return None;
        }
        Some(Rgba {
            r: (parts[0] / 255.0).clamp(0.0, 1.0),
            g: (parts[1] / 255.0).clamp(0.0, 1.0),
            b: (parts[2] / 255.0).clamp(0.0, 1.0),
            a: parts.get(3).copied().unwrap_or(1.0).clamp(0.0, 1.0),
        })
    }

    pub fn hex(&self) -> String {
        let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!(
            "#{:02x}{:02x}{:02x}{:02x}",
            byte(self.r),
            byte(self.g),
            byte(self.b),
            byte(self.a)
        )
    }

    pub fn over(&self, background: Rgba) -> Rgba {
        let rest = background.a * (1.0 - self.a);
        let a = self.a + rest;
        if a <= 0.0 {
            return Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
        }
        let mix = |s: f32, b: f32| (s * self.a + b * rest) / a;
        Rgba {
            r: mix(self.r, background.r),
            g: mix(self.g, background.g),
            b: mix(self.b, background.b),
            a,
        }
    }

    pub fn with_alpha(&self, a: f32) -> Rgba {
        Rgba { a, ..*self }
    }

    fn luminance(&self) -> f32 {
        let lift = |v: f32| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lift(self.r) + 0.7152 * lift(self.g) + 0.0722 * lift(self.b)
    }

    fn readable(&self) -> Rgba {
        let lum = self.luminance();
        if 1.05 / (lum + 0.05) > (lum + 0.05) / 0.05 {
            Rgba::WHITE
        } else {
            Rgba::BLACK
        }
    }
}

#[derive(Debug, Clone, Default)]
struct TokenMap {
    entries: Vec<(String, String)>,
    index: HashMap<String, usize>,
}

impl TokenMap {
    // JS object semantics: reassignment keeps the original insertion slot.
    fn set(&mut self, key: &str, value: &str) {
        if let Some(slot) = self.index.get(key) {
            self.entries[*slot].1 = value.to_string();
            return;
        }
        self.index.insert(key.to_string(), self.entries.len());
        self.entries.push((key.to_string(), value.to_string()));
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.index.get(key).map(|slot| self.entries[*slot].1.as_str())
    }

    #[cfg(test)]
    fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(key, _)| key.as_str())
    }
}

#[derive(Debug, Clone)]
struct Colors {
    compact: bool,
    neutral: String,
    ink: Option<String>,
    primary: String,
    accent: String,
    success: String,
    warning: String,
    error: String,
    info: String,
    interactive: String,
    diff_add: Option<String>,
    diff_delete: Option<String>,
}

#[derive(Debug, Clone)]
struct Variant {
    colors: Colors,
    overrides: Vec<(String, String)>,
    v2_overrides: Vec<(String, String)>,
}

impl Variant {
    fn parse(value: &Value) -> Result<Variant, String> {
        let palette = value.get("palette").filter(|v| v.is_object());
        let seeds = value.get("seeds").filter(|v| v.is_object());
        let (source, compact) = match (palette, seeds) {
            (Some(_), Some(_)) => {
                return Err("theme variant cannot define both `palette` and `seeds`".into());
            }
            (Some(palette), None) => (palette, true),
            (None, Some(seeds)) => (seeds, false),
            (None, None) => return Err("theme variant requires `palette` or `seeds`".into()),
        };
        let optional = |key: &str| source.get(key).and_then(Value::as_str).map(str::to_string);
        let required = |key: &str| optional(key).ok_or_else(|| format!("missing colour `{key}`"));
        let info = required("info")?;
        let primary = required("primary")?;
        let colors = Colors {
            compact,
            neutral: required("neutral")?,
            ink: if compact { optional("ink") } else { None },
            accent: if compact { optional("accent") } else { None }.unwrap_or_else(|| info.clone()),
            success: required("success")?,
            warning: required("warning")?,
            error: required("error")?,
            interactive: if compact {
                optional("interactive").unwrap_or_else(|| primary.clone())
            } else {
                required("interactive")?
            },
            diff_add: optional("diffAdd"),
            diff_delete: optional("diffDelete"),
            info,
            primary,
        };
        let strings = |key: &str| -> Vec<(String, String)> {
            value
                .get(key)
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        };
        Ok(Variant {
            colors,
            overrides: strings("overrides"),
            v2_overrides: strings("v2Overrides"),
        })
    }
}

pub struct Theme {
    pub id: String,
    pub name: String,
    light: Variant,
    dark: Variant,
}

impl Theme {
    fn parse(id: &str, source: &str) -> Result<Theme, String> {
        let value: Value = serde_json::from_str(source).map_err(|err| err.to_string())?;
        let variant = |key: &str| {
            Variant::parse(value.get(key).unwrap_or(&Value::Null))
                .map_err(|err| format!("{id}.{key}: {err}"))
        };
        let name = NAMES
            .get(id)
            .map(String::as_str)
            .or_else(|| value.get("name").and_then(Value::as_str))
            .unwrap_or(id);
        Ok(Theme {
            id: id.to_string(),
            name: name.to_string(),
            light: variant("light")?,
            dark: variant("dark")?,
        })
    }

    pub fn tokens(&self, dark: bool) -> Tokens {
        let variant = if dark { &self.dark } else { &self.light };
        Tokens {
            v1: resolve::resolve_theme_variant(variant, dark),
            v2: v2::resolve_theme_variant_v2(variant, dark),
        }
    }
}

// Display names come from the `names` table in context.tsx.
static NAMES: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    theme_file!("context.tsx")
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("const names:"))
        .skip(1)
        .take_while(|line| line.trim() != "}")
        .filter_map(|line| {
            let (key, value) = line.trim().split_once(':')?;
            let value = value.trim().trim_end_matches(',').trim();
            let value = value.strip_prefix('"')?.strip_suffix('"')?;
            Some((key.trim().trim_matches('"').to_string(), value.to_string()))
        })
        .collect()
});

static THEMES: LazyLock<Vec<Theme>> = LazyLock::new(|| {
    let mut themes: Vec<Theme> = SOURCES
        .iter()
        .chain(OWN)
        .map(|(id, source)| {
            Theme::parse(id, source).unwrap_or_else(|err| panic!("embedded theme {err}"))
        })
        .collect();
    themes.sort_by_cached_key(|theme| (theme.id != DEFAULT_THEME, theme.name.to_lowercase()));
    themes
});

pub fn themes() -> &'static [Theme] {
    &THEMES
}

pub fn find(id: &str) -> Option<&'static Theme> {
    let id = if id == "oc-1" { "oc-2" } else { id };
    themes().iter().find(|theme| theme.id == id)
}

pub struct Tokens {
    v1: TokenMap,
    v2: TokenMap,
}

impl Tokens {
    pub fn raw(&self, name: &str) -> Option<&str> {
        let name = name.strip_prefix("--").unwrap_or(name);
        if name.starts_with("v2-") {
            self.v2.get(name)
        } else {
            self.v1.get(name)
        }
    }

    pub fn color(&self, name: &str) -> Option<Rgba> {
        self.resolve(name, 0)
    }

    #[cfg(test)]
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.v1.names().chain(self.v2.names())
    }

    fn resolve(&self, name: &str, depth: usize) -> Option<Rgba> {
        if depth > 24 {
            return None;
        }
        let name = name.strip_prefix("--").unwrap_or(name);
        let raw = self.raw(name).or_else(|| v2::alpha(name))?.trim();
        let Some(inner) = raw.strip_prefix("var(").and_then(|s| s.strip_suffix(')')) else {
            return Rgba::parse(raw);
        };
        let (target, fallback) = match inner.split_once(',') {
            Some((target, fallback)) => (target, Some(fallback)),
            None => (inner, None),
        };
        self.resolve(target.trim(), depth + 1)
            .or_else(|| Rgba::parse(fallback?))
    }
}

const SYNTAX: &[(&str, &str)] = &[
    ("comment", "syntax-comment"),
    ("comment.doc", "syntax-comment"),
    ("keyword", "syntax-keyword"),
    ("string", "syntax-string"),
    ("string.escape", "syntax-string"),
    ("string.special", "syntax-string"),
    ("string.regex", "syntax-regexp"),
    ("number", "syntax-constant"),
    ("boolean", "syntax-constant"),
    ("constant", "syntax-constant"),
    ("type", "syntax-type"),
    ("enum", "syntax-type"),
    ("constructor", "syntax-type"),
    ("function", "syntax-primitive"),
    ("property", "syntax-property"),
    ("attribute", "syntax-property"),
    ("tag", "syntax-property"),
    ("variable", "syntax-variable"),
    ("variable.special", "syntax-object"),
    ("operator", "syntax-operator"),
    ("punctuation", "syntax-punctuation"),
    ("punctuation.bracket", "syntax-punctuation"),
    ("punctuation.delimiter", "syntax-punctuation"),
    ("title", "markdown-heading"),
    ("link_uri", "markdown-link"),
    ("link_text", "markdown-link-text"),
    ("text.literal", "markdown-code"),
    ("emphasis", "markdown-emph"),
    ("emphasis.strong", "markdown-strong"),
];

pub fn component_theme(theme: &Theme, dark: bool) -> Value {
    let tokens = theme.tokens(dark);
    let pick = |name: &str, fallback: &str, default: Rgba| {
        tokens
            .color(name)
            .or_else(|| tokens.color(fallback))
            .unwrap_or(default)
    };
    let ground = if dark { Rgba::BLACK } else { Rgba::WHITE };
    let ink = if dark { Rgba::WHITE } else { Rgba::BLACK };
    let clear = ground.with_alpha(0.0);

    let bg = pick("v2-background-bg-base", "background-base", ground).over(ground);
    let deep = pick("v2-background-bg-deep", "background-weak", bg).over(bg);
    let layer = pick("v2-background-bg-layer-01", "background-weak", bg).over(bg);
    let text = pick("v2-text-text-base", "text-strong", ink);
    let muted = pick("v2-text-text-muted", "text-weak", text);
    let faint = pick("v2-text-text-faint", "text-weaker", muted);
    let accent = pick("v2-background-bg-accent", "surface-interactive-base", text);
    let link = pick("v2-text-text-accent", "text-interactive-base", accent);
    let link_hover = pick("v2-text-text-accent-hover", "text-interactive-base", link);
    let focus = pick("v2-border-border-focus", "border-interactive-focus", accent);
    let border = pick("v2-border-border-base", "border-base", muted.with_alpha(0.2));
    let border_muted = pick("v2-border-border-muted", "border-weak-base", border);
    let border_strong = pick("v2-border-border-strong", "border-strong-base", border);
    let hover = pick("v2-overlay-simple-overlay-hover", "surface-base-hover", clear);
    let pressed = pick("v2-overlay-simple-overlay-pressed", "surface-base-active", hover);
    let primary = pick("v2-background-bg-contrast", "button-primary-base", text).over(bg);
    let primary_text = pick("v2-text-text-contrast", "text-invert-strong", primary.readable());
    let primary_hover = pick("v2-overlay-simple-overlay-contrast-hover", "", clear);
    let primary_pressed = pick("v2-overlay-simple-overlay-contrast-pressed", "", primary_hover);
    let scrim = pick("v2-overlay-simple-overlay-scrim", "", Rgba::BLACK.with_alpha(0.4));
    let strong = pick("text-strong", "v2-text-text-base", text);
    let weak = pick("text-weak", "v2-text-text-muted", muted);

    let mut colors = Map::new();
    let mut put = |key: &str, color: Rgba| {
        colors.insert(key.to_string(), Value::String(color.hex()));
    };

    put("background", bg);
    put("foreground", text);
    put("muted.background", layer);
    put("muted.foreground", muted);
    put("border", border.over(bg));
    put("input.border", border.over(bg));
    put("accent.background", hover.over(bg));
    put("accent.foreground", text);
    put("primary.background", primary);
    put("primary.foreground", primary_text);
    put("primary.hover.background", primary_hover.over(primary));
    put("primary.active.background", primary_pressed.over(primary));
    put("secondary.background", layer);
    put("secondary.foreground", text);
    put("secondary.hover.background", hover.over(layer));
    put("secondary.active.background", pressed.over(layer));
    put("ring", focus);
    put("caret", text);
    put("link", link);
    put("link.hover", link_hover);
    put("link.active", link_hover);
    for (key, token) in [
        ("danger", "critical"),
        ("success", "success"),
        ("warning", "warning"),
        ("info", "info"),
    ] {
        let base = pick(&format!("icon-{token}-base"), &format!("surface-{token}-strong"), text);
        let hovered = pick(&format!("icon-{token}-hover"), "", base);
        let active = pick(&format!("icon-{token}-active"), "", hovered);
        put(&format!("{key}.background"), base);
        put(&format!("{key}.hover.background"), hovered);
        put(&format!("{key}.active.background"), active);
        put(&format!("{key}.foreground"), base.over(bg).readable());
    }
    put("sidebar.background", deep);
    put("sidebar.foreground", text);
    put("sidebar.border", border_muted.over(deep));
    put("sidebar.accent.background", pressed.over(deep));
    put("sidebar.accent.foreground", text);
    put("sidebar.primary.background", accent);
    put("sidebar.primary.foreground", Rgba::WHITE);
    put("title_bar.background", deep);
    put("title_bar.border", border_muted.over(deep));
    put("popover.background", bg);
    put("popover.foreground", text);
    put("list.background", bg);
    put("list.hover.background", hover.over(bg));
    put("list.active.background", pressed.over(bg));
    put("list.active.border", focus);
    put("list.even.background", bg);
    put("list.head.background", layer);
    put("table.background", bg);
    put("table.head.background", layer);
    put("table.head.foreground", muted);
    put("table.hover.background", hover.over(bg));
    put("table.active.background", pressed.over(bg));
    put("table.active.border", focus);
    put("table.even.background", bg);
    put("table.row.border", border_muted.over(bg));
    put("tab_bar.background", deep);
    put("tab.background", deep);
    put("tab.foreground", muted);
    put("tab.active.background", bg);
    put("tab.active.foreground", text);
    put("tab_bar.segmented.background", layer);
    put("selection.background", accent.with_alpha(0.3));
    put("scrollbar.background", bg.with_alpha(0.0));
    put("scrollbar.thumb.background", border_strong);
    put("scrollbar.thumb.hover.background", faint);
    put("switch.background", border_strong.over(bg));
    put("switch.thumb.background", Rgba::WHITE);
    put("slider.background", accent);
    put("slider.thumb.background", Rgba::WHITE);
    put("progress.bar.background", accent);
    put("skeleton.background", layer);
    put("accordion.background", bg);
    put("group_box.background", layer);
    put("group_box.foreground", text);
    put("overlay", scrim);
    put("window.border", border.over(bg));
    put("drag.border", focus);
    put("drop_target.background", accent.with_alpha(0.15));
    for (key, ramp) in [
        ("red", "red"),
        ("green", "green"),
        ("blue", "blue"),
        ("yellow", "yellow"),
        ("magenta", "pink"),
        ("cyan", "cyan"),
    ] {
        let base = pick(&format!("v2-{ramp}-600"), "", accent);
        put(&format!("base.{key}"), base);
        put(
            &format!("base.{key}.light"),
            pick(&format!("v2-{ramp}-300"), "", base),
        );
    }
    for (slot, step) in [300, 500, 600, 700, 800].into_iter().enumerate() {
        put(
            &format!("chart.{}", slot + 1),
            pick(&format!("v2-blue-{step}"), "", accent),
        );
    }

    let mut highlight = Map::new();
    let mut put = |key: &str, color: Rgba| {
        highlight.insert(key.to_string(), Value::String(color.hex()));
    };
    put("editor.background", bg);
    put("editor.foreground", strong);
    put("editor.line_number", weak);
    put("editor.active_line_number", strong);
    put("editor.active_line.background", hover.over(bg));
    for (key, token) in [
        ("created", "icon-diff-add-base"),
        ("created.background", "surface-diff-add-base"),
        ("deleted", "icon-diff-delete-base"),
        ("deleted.background", "surface-diff-delete-base"),
        ("modified", "icon-diff-modified-base"),
        ("error", "icon-critical-base"),
        ("warning", "icon-warning-base"),
        ("info", "icon-info-base"),
        ("success", "icon-success-base"),
        ("hint", "text-weak"),
    ] {
        put(key, pick(token, "", if key == "hint" { weak } else { strong }));
    }
    let syntax: Map<String, Value> = SYNTAX
        .iter()
        .map(|(name, token)| {
            (
                name.to_string(),
                json!({ "color": pick(token, "", strong).hex() }),
            )
        })
        .collect();
    highlight.insert("syntax".to_string(), Value::Object(syntax));

    json!({
        "name": format!("{} {}", theme.name, if dark { "Dark" } else { "Light" }),
        "mode": if dark { "dark" } else { "light" },
        "colors": colors,
        "highlight": highlight,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Dumped from the TypeScript `resolveThemeVariant` / `resolveThemeVariantV2`.
    const GOLDEN: &[(&str, bool, &str)] = &[
        ("oc-2", false, "background-base=#f8f8f8;text-base=#6F6F6F;text-weak=#8F8F8F;text-weaker=#C7C7C7;text-strong=#171717;border-base=#c1c0c0;surface-diff-add-base=#d9ffd6;icon-critical-base=#fa3012;icon-diff-modified-base=#675912;syntax-keyword=var(--v2-pink-800);markdown-heading=#5e5e11;surface-base-interactive-active=rgba(237, 243, 254, 0.3);v2-grey-100=#fafafaff;v2-grey-1200=#080808ff;v2-blue-600=#3b5cf6ff;v2-red-600=#f1484fff;v2-orange-500=#ffa478ff;v2-text-text-base=var(--v2-grey-1100);v2-text-text-muted=var(--v2-grey-700);v2-icon-icon-base=var(--v2-grey-800);v2-icon-icon-inverse=var(--v2-grey-50);v2-background-bg-base=var(--v2-grey-50)"),
        ("oc-2", true, "background-base=#121212;text-base=#A0A0A0;text-weak=#707070;text-weaker=#505050;text-strong=#EDEDED;border-base=#4d4c4a;surface-diff-add-base=#001401;icon-critical-base=#cb321c;icon-diff-modified-base=#fde280;syntax-keyword=var(--v2-pink-400);markdown-heading=#fed6bd;surface-base-interactive-active=rgba(1, 9, 49, 0.3);v2-grey-100=#fafafaff;v2-grey-1200=#080808ff;v2-blue-600=#3b5cf6ff;v2-red-600=#f1484fff;v2-orange-500=#ffa478ff;v2-text-text-base=var(--v2-grey-100);v2-text-text-muted=var(--v2-grey-500);v2-icon-icon-base=var(--v2-grey-400);v2-icon-icon-inverse=var(--v2-grey-1100);v2-background-bg-base=var(--v2-grey-1100)"),
        ("dracula", false, "background-base=#f9f9f4;text-base=#1b1b2b;text-weak=#363646;text-weaker=#51515f;text-strong=#0c0b1a;border-base=#c4c4c4;surface-diff-add-base=#ddfce5;icon-critical-base=#dd295e;icon-diff-modified-base=#824a05;syntax-keyword=#d16090;markdown-heading=#5722db;surface-base-interactive-active=rgba(241, 241, 254, 0.3);v2-grey-100=#f9f9f4;v2-grey-1200=#232636;v2-blue-600=#7c67ff;v2-red-600=#f02c66;v2-orange-500=#feb59b;v2-text-text-base=#0c0b1a;v2-text-text-muted=#363646;v2-icon-icon-base=var(--v2-grey-1100);v2-icon-icon-inverse=var(--v2-grey-50);v2-background-bg-base=var(--v2-grey-100)"),
        ("dracula", true, "background-base=#0f1019;text-base=#f8f8f2;text-weak=#d4d4ce;text-weaker=#b7b7b2;text-strong=#fefefe;border-base=#4d4d52;surface-diff-add-base=#001309;icon-critical-base=#d5212f;icon-diff-modified-base=#fed7b1;syntax-keyword=#ff79c6;markdown-heading=#e4d7fc;surface-base-interactive-active=rgba(22, 1, 44, 0.3);v2-grey-100=#f3f4ee;v2-grey-1200=#0f1019;v2-blue-600=#b987fd;v2-red-600=#f95c5a;v2-orange-500=#feb595;v2-text-text-base=#fefefe;v2-text-text-muted=#d4d4ce;v2-icon-icon-base=var(--v2-grey-200);v2-icon-icon-inverse=var(--v2-grey-1100);v2-background-bg-base=var(--v2-grey-1000)"),
        ("matrix", false, "background-base=#f0f5ed;text-base=#19291c;text-weak=#748476;text-weaker=#9ba79c;text-strong=#08180c;border-base=#bdc5bb;surface-diff-add-base=#d9fde6;icon-critical-base=#fd1f32;icon-diff-modified-base=#56610e;syntax-keyword=#c770ff;markdown-heading=#24f6d9;surface-base-interactive-active=rgba(231, 245, 255, 0.3);v2-grey-100=#f0f5ed;v2-grey-1200=#263628;v2-blue-600=#30aff9;v2-red-600=#fd4e4d;v2-orange-500=#ebc707;v2-text-text-base=#353535;v2-text-text-muted=#748476;v2-icon-icon-base=var(--v2-grey-1100);v2-icon-icon-inverse=var(--v2-grey-1100);v2-background-bg-base=var(--v2-grey-100)"),
        ("matrix", true, "background-base=#040704;text-base=#6ffd99;text-weak=#8ca391;text-weaker=#6d7d70;text-strong=#f1fff5;border-base=#1d482a;surface-diff-add-base=#001407;icon-critical-base=#d60823;icon-diff-modified-base=#f7ffde;syntax-keyword=#c770ff;markdown-heading=#00efff;surface-base-interactive-active=rgba(0, 16, 29, 0.3);v2-grey-100=#61fa91;v2-grey-1200=#040704;v2-blue-600=#1caffe;v2-red-600=#f95451;v2-orange-500=#e9c729;v2-text-text-base=#ececec;v2-text-text-muted=#8ca391;v2-icon-icon-base=var(--v2-grey-200);v2-icon-icon-inverse=var(--v2-grey-1100);v2-background-bg-base=var(--v2-grey-1000)"),
        ("nord", false, "background-base=#eef1f5;text-base=#252b37;text-weak=#414753;text-weaker=#5e636d;text-strong=#141a26;border-base=#bfc3c9;surface-diff-add-base=#eaf8e0;icon-critical-base=#c04959;icon-diff-modified-base=#FF8C00;syntax-keyword=#5e81ac;markdown-heading=#2a5b94;surface-base-interactive-active=rgba(235, 243, 254, 0.3);v2-grey-100=#eef1f5;v2-grey-1200=#343a46;v2-blue-600=#5181bb;v2-red-600=#d05061;v2-orange-500=#fdb2b6;v2-text-text-base=#141a26;v2-text-text-muted=#414753;v2-icon-icon-base=var(--v2-grey-1200);v2-icon-icon-inverse=var(--v2-grey-1100);v2-background-bg-base=var(--v2-grey-100)"),
        ("nord", true, "background-base=#191e29;text-base=#e5e9f0;text-weak=#c2c5cb;text-weaker=#a6a9ae;text-strong=#fcfdfe;border-base=#4f545e;surface-diff-add-base=#011020;icon-critical-base=#c02546;icon-diff-modified-base=#ffba92;syntax-keyword=#81a1c1;markdown-heading=#aaebfd;surface-base-interactive-active=rgba(1, 18, 23, 0.3);v2-grey-100=#e1e5ec;v2-grey-1200=#191e29;v2-blue-600=#67b1c6;v2-red-600=#cd5463;v2-orange-500=#fab3b7;v2-text-text-base=#fcfdfe;v2-text-text-muted=#c2c5cb;v2-icon-icon-base=var(--v2-grey-200);v2-icon-icon-inverse=var(--v2-grey-1100);v2-background-bg-base=var(--v2-grey-1000)"),
        ("github", false, "background-base=#ffffff;text-base=#1e2329;text-weak=#57606a;text-weaker=#7b828a;text-strong=#0e1319;border-base=#cacbcc;surface-diff-add-base=#e3fae5;icon-critical-base=#bc212b;icon-diff-modified-base=#785105;syntax-keyword=#cf222e;markdown-heading=#0969da;surface-base-interactive-active=rgba(235, 243, 255, 0.3);v2-grey-100=#ffffff;v2-grey-1200=#2a2f35;v2-blue-600=#0f69d8;v2-red-600=#d60025;v2-orange-500=#fbb88e;v2-text-text-base=#0e1319;v2-text-text-muted=#57606a;v2-icon-icon-base=var(--v2-grey-1100);v2-icon-icon-inverse=var(--v2-grey-1100);v2-background-bg-base=var(--v2-grey-100)"),
        ("github", true, "background-base=#05080e;text-base=#ccd4dc;text-weak=#8b949e;text-weaker=#6a7077;text-strong=#fafbfc;border-base=#393d44;surface-diff-add-base=#011403;icon-critical-base=#d01d21;icon-diff-modified-base=#fdda8e;syntax-keyword=#ff7b72;markdown-heading=#58a6ff;surface-base-interactive-active=rgba(0, 15, 36, 0.3);v2-grey-100=#c5cdd5;v2-grey-1200=#05080e;v2-blue-600=#5ea6f9;v2-red-600=#fb4d45;v2-orange-500=#fbba7e;v2-text-text-base=#fafbfc;v2-text-text-muted=#8b949e;v2-icon-icon-base=var(--v2-grey-200);v2-icon-icon-inverse=var(--v2-grey-1100);v2-background-bg-base=var(--v2-grey-1000)"),
        ("tokyonight", false, "background-base=#e5e6ea;text-base=#1f284a;text-weak=#3b4566;text-weaker=#59617d;text-strong=#0f1638;border-base=#b7bac5;surface-diff-add-base=#ddfaf0;icon-critical-base=#c52152;icon-diff-modified-base=#77510a;syntax-keyword=#9854f1;markdown-heading=#0654b3;surface-base-interactive-active=rgba(235, 243, 254, 0.3);v2-grey-100=#e5e6ea;v2-grey-1200=#2d3758;v2-blue-600=#217bf2;v2-red-600=#d7255b;v2-orange-500=#f1bc9b;v2-text-text-base=#0f1638;v2-text-text-muted=#3b4566;v2-icon-icon-base=var(--v2-grey-1200);v2-icon-icon-inverse=var(--v2-grey-1100);v2-background-bg-base=var(--v2-grey-100)"),
        ("tokyonight", true, "background-base=#0d0e17;text-base=#c4cefb;text-weak=#a3abd2;text-weaker=#8990b1;text-strong=#f9faff;border-base=#3c4052;surface-diff-add-base=#001215;icon-critical-base=#e3215f;icon-diff-modified-base=#fdd8a5;syntax-keyword=#bb9af7;markdown-heading=#d0dffe;surface-base-interactive-active=rgba(0, 1, 63, 0.3);v2-grey-100=#bdc7f1;v2-grey-1200=#0d0e17;v2-blue-600=#78a2fa;v2-red-600=#f9738c;v2-orange-500=#fdb78a;v2-text-text-base=#f9faff;v2-text-text-muted=#a3abd2;v2-icon-icon-base=var(--v2-grey-200);v2-icon-icon-inverse=var(--v2-grey-1100);v2-background-bg-base=var(--v2-grey-1000)"),
    ];

    // No shipped theme uses `seeds`; this synthetic one was run through the TypeScript too.
    const SEEDS: &str = r##"{"light":{"seeds":{"neutral":"#8e8b8b","primary":"#fab283","success":"#12c905","warning":"#fcd53a","error":"#fc533a","info":"#a753ae","interactive":"#034cff","diffAdd":"#c8ffc4","diffDelete":"#fc533a"},"overrides":{"text-weak":"#777777"}},"dark":{"seeds":{"neutral":"#8e8b8b","primary":"#fab283","success":"#12c905","warning":"#fcd53a","error":"#fc533a","info":"#a753ae","interactive":"#034cff","diffAdd":"#c8ffc4","diffDelete":"#fc533a"}}}"##;
    const SEEDS_GOLDEN: &[(bool, &str)] = &[
        (false, "background-base=#fefbfb;text-base=#716e6e;text-weak=#777777;text-weaker=#c9c6c6;text-strong=#191617;border-base=#aeabab;surface-diff-add-base=#e2fbe0;icon-critical-base=#fa3012;icon-diff-modified-base=#FF8C00;syntax-keyword=var(--text-weak);markdown-heading=#d68c27;surface-base-interactive-active=rgba(237, 243, 254, 0.3);v2-grey-100=#eeeeee;v2-grey-1200=#918e8e;v2-blue-600=#064efc;v2-red-600=#fa553d;v2-orange-500=#ffba5d;v2-text-text-base=#5a5757;v2-text-text-muted=#777777;v2-icon-icon-base=var(--v2-grey-800);v2-icon-icon-inverse=var(--v2-grey-1100);v2-background-bg-base=var(--v2-grey-100)"),
        (true, "background-base=#0a0808;text-base=#a6a3a3;text-weak=#6e6b6b;text-weaker=#4b4848;text-strong=#f3efef;border-base=#585555;surface-diff-add-base=#001401;icon-critical-base=#cb321c;icon-diff-modified-base=#ffba92;syntax-keyword=var(--text-weak);markdown-heading=#9d7cd8;surface-base-interactive-active=rgba(1, 9, 49, 0.3);v2-grey-100=#8d8a8a;v2-grey-1200=#5c5959;v2-blue-600=#0f51f5;v2-red-600=#f65a43;v2-orange-500=#fcbb64;v2-text-text-base=#f7f6f6;v2-text-text-muted=#8a8787;v2-icon-icon-base=var(--v2-grey-400);v2-icon-icon-inverse=var(--v2-grey-1100);v2-background-bg-base=var(--v2-grey-1000)"),
    ];

    fn assert_golden(label: &str, tokens: &Tokens, expected: &str) {
        for pair in expected.split(';') {
            let (key, value) = pair.split_once('=').unwrap();
            assert_eq!(tokens.raw(key), Some(value), "{label} {key}");
        }
    }

    fn is_hex8(text: &str) -> bool {
        text.len() == 9
            && text.starts_with('#')
            && text[1..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    #[test]
    fn lists_all_themes_in_display_order() {
        let all = themes();
        assert_eq!(all.len(), 38);
        assert_eq!(all[0].id, DEFAULT_THEME);
        assert_eq!(all[0].name, "Clay");
        assert_eq!(find("oc-1").unwrap().id, "oc-2");
        let rest: Vec<String> = all[1..].iter().map(|t| t.name.to_lowercase()).collect();
        assert!(rest.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(find("oc-1").map(|t| t.id.as_str()), Some("oc-2"));
        assert_eq!(find("synthwave84").map(|t| t.name.as_str()), Some("Synthwave '84"));
        assert_eq!(find("nightowl").map(|t| t.name.as_str()), Some("Night Owl"));
        assert!(find("nope").is_none());
        assert_eq!(NAMES.len(), 37);
    }

    #[test]
    fn every_theme_resolves_and_colours_parse() {
        for theme in themes() {
            for dark in [false, true] {
                let tokens = theme.tokens(dark);
                let names: Vec<&str> = tokens.names().collect();
                assert!(names.len() > 400, "{} has {} tokens", theme.id, names.len());
                for name in names {
                    let raw = tokens.raw(name).unwrap();
                    let colourish =
                        raw.starts_with('#') || raw.starts_with("rgb") || raw.starts_with("var(");
                    assert_eq!(
                        tokens.color(name).is_some(),
                        colourish,
                        "{} dark={dark} {name} = {raw}",
                        theme.id
                    );
                }
            }
        }
    }

    #[test]
    fn matches_typescript_golden_values() {
        for (id, dark, expected) in GOLDEN {
            let theme = find(id).unwrap();
            assert_golden(&format!("{id} dark={dark}"), &theme.tokens(*dark), expected);
        }
    }

    #[test]
    fn seeds_variant_matches_typescript() {
        let theme = Theme::parse("seeds", SEEDS).unwrap();
        for (dark, expected) in SEEDS_GOLDEN {
            assert_golden(&format!("seeds dark={dark}"), &theme.tokens(*dark), expected);
        }
        assert!(Theme::parse("bad", r#"{"light":{},"dark":{}}"#).is_err());
    }

    #[test]
    fn resolves_references_and_alpha_ramps() {
        let tokens = find("oc-2").unwrap().tokens(false);
        assert_eq!(tokens.color("text-strong"), Rgba::parse("#171717"));
        assert_eq!(tokens.color("--v2-text-text-base"), Rgba::parse("#161616"));
        assert_eq!(tokens.color("syntax-keyword"), tokens.color("v2-pink-800"));
        assert_eq!(
            tokens.color("v2-border-border-base").map(|c| c.hex()),
            Some("#0000001a".to_string())
        );
        assert_eq!(tokens.color("v2-elevation-raised"), None);
        assert_eq!(tokens.color("missing"), None);
        assert_eq!(v2::alpha("v2-alpha-light-24"), Some("#ffffff3d"));

        let mut looped = find("oc-2").unwrap().tokens(true);
        looped.v1.set("a", "var(--b)");
        looped.v1.set("b", "var(--a)");
        assert_eq!(looped.color("a"), None);
    }

    #[test]
    fn rgba_parses_formats_and_composites() {
        assert_eq!(Rgba::parse("#fff"), Some(Rgba::WHITE));
        assert_eq!(Rgba::parse("#000f"), Some(Rgba::BLACK));
        assert_eq!(Rgba::parse("#0000000A").map(|c| c.hex()), Some("#0000000a".into()));
        assert_eq!(
            Rgba::parse("rgba(237, 243, 254, 0.3)").map(|c| c.hex()),
            Some("#edf3fe4d".into())
        );
        assert_eq!(Rgba::parse("rgb(1, 2, 3)").map(|c| c.hex()), Some("#010203ff".into()));
        assert_eq!(Rgba::parse("#12345"), None);
        assert_eq!(Rgba::parse("0px 1px var(--x)"), None);
        let half = Rgba::BLACK.with_alpha(0.5).over(Rgba::WHITE);
        assert_eq!(half.hex(), "#808080ff");
        assert_eq!(Rgba::WHITE.over(Rgba::BLACK), Rgba::WHITE);
        assert_eq!(Rgba::BLACK.with_alpha(0.0).over(Rgba::WHITE.with_alpha(0.0)).a, 0.0);
    }

    #[test]
    fn component_theme_emits_hex_colours() {
        for theme in themes() {
            for dark in [false, true] {
                let value = component_theme(theme, dark);
                let label = format!("{} dark={dark}", theme.id);
                assert_eq!(value["mode"], if dark { "dark" } else { "light" });
                assert!(value["name"].as_str().unwrap().starts_with(&theme.name));
                let colors = value["colors"].as_object().unwrap();
                assert!(colors.len() >= 100, "{label} has {} colours", colors.len());
                for (key, colour) in colors {
                    assert!(is_hex8(colour.as_str().unwrap()), "{label} {key} = {colour}");
                }
                for key in ["background", "border", "sidebar.border", "primary.hover.background"] {
                    assert!(colors[key].as_str().unwrap().ends_with("ff"), "{label} {key}");
                }
                let highlight = value["highlight"].as_object().unwrap();
                for (key, colour) in highlight.iter().filter(|(key, _)| *key != "syntax") {
                    assert!(is_hex8(colour.as_str().unwrap()), "{label} {key} = {colour}");
                }
                let syntax = highlight["syntax"].as_object().unwrap();
                assert_eq!(syntax.len(), SYNTAX.len());
                for (key, style) in syntax {
                    assert!(is_hex8(style["color"].as_str().unwrap()), "{label} {key}");
                }
            }
        }
        let light = component_theme(find("oc-2").unwrap(), false);
        assert_eq!(light["name"], "OC-2 Light");
        assert_eq!(light["colors"]["background"], "#ffffffff");
        assert_eq!(light["colors"]["foreground"], "#161616ff");
        assert_eq!(light["colors"]["border"], "#e5e5e5ff");
        assert_eq!(light["colors"]["sidebar.primary.foreground"], "#ffffffff");
        assert_eq!(light["highlight"]["editor.foreground"], "#171717ff");
    }
}
