use std::collections::HashMap;
use std::sync::LazyLock;

use super::color::{
    Oklch, blend, contrast_ratio, generate_neutral_scale, hex_to_oklch, oklch_to_hex, shift,
};
use super::{TokenMap, Variant};

const STEPS: [u32; 12] = [100, 200, 300, 400, 500, 600, 700, 800, 900, 1000, 1100, 1200];

type Table = Vec<(String, String)>;

struct Tables {
    light: Table,
    dark: Table,
    defaults: Table,
}

static TABLES: LazyLock<Tables> = LazyLock::new(|| {
    let mut objects = parse_objects(&[
        theme_file!("v2/avatar.ts"),
        theme_file!("v2/mapping.ts"),
        theme_file!("v2/default-primitives.ts"),
    ]);
    let mut take = |name: &str| {
        objects
            .remove(name)
            .filter(|table| !table.is_empty())
            .unwrap_or_else(|| panic!("v2 theme table `{name}` not found"))
    };
    Tables {
        light: take("light"),
        dark: take("dark"),
        defaults: take("V2_PRIMITIVES_DEFAULT"),
    }
});

static ALPHA: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    theme_file!("../v2/styles/colors.css")
        .lines()
        .filter_map(|line| {
            let (name, value) = line.trim().strip_prefix("--")?.split_once(':')?;
            let name = name.trim();
            if !name.starts_with("v2-alpha-") {
                return None;
            }
            Some((name.to_string(), value.trim().trim_end_matches(';').trim().to_string()))
        })
        .collect()
});

pub fn alpha(name: &str) -> Option<&'static str> {
    ALPHA.get(name).map(String::as_str)
}

fn object_name(line: &str) -> Option<&str> {
    if !line.ends_with("= {") {
        return None;
    }
    let rest = line
        .strip_prefix("export const ")
        .or_else(|| line.strip_prefix("const "))?;
    let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    Some(&rest[..end])
}

fn parse_value(text: &str, scalars: &HashMap<String, String>) -> Option<String> {
    let text = text.trim().trim_end_matches(',').trim();
    if let Some(value) = scalars.get(text) {
        return Some(value.clone());
    }
    if let Some(inner) = text.strip_prefix("ref(").and_then(|s| s.strip_suffix(')')) {
        let name = inner.trim().strip_prefix('"')?.strip_suffix('"')?;
        return Some(format!("var(--{name})"));
    }
    Some(text.strip_prefix('"')?.strip_suffix('"')?.to_string())
}

fn put(table: &mut Table, key: &str, value: String) {
    match table.iter_mut().find(|(k, _)| k == key) {
        Some(entry) => entry.1 = value,
        None => table.push((key.to_string(), value)),
    }
}

fn parse_objects(sources: &[&str]) -> HashMap<String, Table> {
    let mut objects: HashMap<String, Table> = HashMap::new();
    let mut scalars: HashMap<String, String> = HashMap::new();
    let mut current: Option<(String, Table)> = None;
    let mut pending: Option<String> = None;
    for line in sources.iter().flat_map(|source| source.lines()).map(str::trim) {
        let Some((_, table)) = current.as_mut() else {
            if let Some(name) = object_name(line) {
                current = Some((name.to_string(), Table::new()));
            } else if let Some((name, value)) = line
                .strip_prefix("export const ")
                .and_then(|rest| rest.split_once(" = "))
                && let Some(value) = parse_value(value, &scalars)
            {
                scalars.insert(name.to_string(), value);
            }
            continue;
        };
        if line.starts_with('}') {
            if let Some((name, table)) = current.take() {
                objects.insert(name, table);
            }
            pending = None;
            continue;
        }
        if let Some(key) = pending.take() {
            if let Some(value) = parse_value(line, &scalars) {
                put(table, &key, value);
            }
            continue;
        }
        if let Some(name) = line.strip_prefix("...") {
            let spread = objects.get(name.trim_end_matches(',').trim());
            for (key, value) in spread.into_iter().flatten() {
                put(table, key, value.clone());
            }
            continue;
        }
        let Some((key, rest)) = line
            .strip_prefix('"')
            .and_then(|rest| rest.split_once("\":"))
        else {
            continue;
        };
        if rest.trim().is_empty() {
            pending = Some(key.to_string());
            continue;
        }
        if let Some(value) = parse_value(rest, &scalars) {
            put(table, key, value);
        }
    }
    objects
}

fn clamp(v: f64, min: f64, max: f64) -> f64 {
    min.max(max.min(v))
}

fn hue_scale(seed: &str, dark: bool) -> Vec<String> {
    let base = hex_to_oklch(seed);
    let boost = if dark { 1.0 } else { 1.05 };
    let light_steps = [
        0.99,
        0.965,
        0.93,
        0.885,
        0.835,
        clamp(base.l, 0.48, 0.72),
        clamp(base.l - 0.07, 0.4, 0.64),
        clamp(base.l - 0.14, 0.32, 0.55),
        clamp(base.l - 0.21, 0.24, 0.46),
        clamp(base.l - 0.28, 0.17, 0.38),
        clamp(base.l - 0.34, 0.12, 0.3),
        clamp(base.l - 0.4, 0.08, 0.22),
    ];
    let chroma = [0.28, 0.48, 0.68, 0.86, 1.02, 1.28, 1.34, 1.28, 1.18, 1.08, 0.98, 0.88];
    light_steps
        .iter()
        .zip(chroma)
        .map(|(l, multiplier)| {
            oklch_to_hex(Oklch {
                l: *l,
                c: base.c * multiplier * boost,
                h: base.h,
            })
        })
        .collect()
}

fn grey_hex(primitives: &TokenMap, step: u32) -> Option<&str> {
    primitives
        .get(&format!("v2-grey-{step}"))
        .filter(|hex| hex.starts_with('#'))
}

fn grey_ref(semantics: &Table, key: &str, primitives: &TokenMap) -> Option<String> {
    let value = &semantics.iter().find(|(k, _)| k == key)?.1;
    let step = value
        .strip_prefix("var(--v2-grey-")?
        .strip_suffix(')')?
        .parse()
        .ok()?;
    grey_hex(primitives, step).map(str::to_string)
}

fn pick_grey(primitives: &TokenMap, background: &str, min_contrast: f64, target: u32) -> u32 {
    STEPS
        .into_iter()
        .filter(|step| {
            grey_hex(primitives, *step)
                .is_some_and(|hex| contrast_ratio(hex, background) >= min_contrast)
        })
        .reduce(|best, step| {
            if step.abs_diff(target) < best.abs_diff(target) {
                step
            } else {
                best
            }
        })
        .unwrap_or(target)
}

fn foreground(
    ink: &str,
    dark: bool,
    primitives: &TokenMap,
    semantics: &Table,
    variant: &Variant,
) -> Table {
    let tint = hex_to_oklch(ink);
    let body = shift(
        ink,
        if dark {
            0.0_f64.max(0.88 - tint.l) * 0.4
        } else {
            -(0.0_f64.max(tint.l - 0.18)) * 0.24
        },
        if dark { 1.04 } else { 1.02 },
        0.0,
    );
    let fallback = || primitives.get("v2-grey-100").unwrap_or("#ffffff").to_string();
    let background = |key: &str| grey_ref(semantics, key, primitives).unwrap_or_else(fallback);
    let bg_base = background("v2-background-bg-base");
    let bg_contrast = background("v2-background-bg-contrast");
    let bg_inverse = background("v2-background-bg-inverse");
    let inverse_target = if hex_to_oklch(&bg_inverse).l > 0.55 {
        1100
    } else if grey_hex(primitives, 50).is_some() {
        50
    } else {
        100
    };
    let grey = |background: &str, min_contrast: f64, target: u32| {
        format!(
            "var(--v2-grey-{})",
            pick_grey(primitives, background, min_contrast, target)
        )
    };
    let muted = variant
        .overrides
        .iter()
        .find(|(k, _)| k == "text-weak")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| shift(&body, if dark { -0.11 } else { 0.11 }, 0.9, 0.0));
    let entries = [
        (
            "v2-text-text-base",
            if dark {
                blend("#ffffff", &body, 0.9)
            } else {
                shift(&body, -0.07, 1.04, 0.0)
            },
        ),
        ("v2-text-text-muted", muted),
        (
            "v2-text-text-faint",
            shift(
                &body,
                if dark { -0.2 } else { 0.21 },
                if dark { 0.78 } else { 0.72 },
                0.0,
            ),
        ),
        ("v2-icon-icon-base", grey(&bg_base, 7.0, if dark { 400 } else { 800 })),
        ("v2-icon-icon-muted", grey(&bg_base, 3.0, 600)),
        ("v2-icon-icon-inverse", grey(&bg_inverse, 7.0, inverse_target)),
        ("v2-icon-icon-contrast", grey(&bg_contrast, 7.0, 100)),
        (
            "v2-icon-icon-accent",
            if dark { "var(--v2-blue-400)" } else { "var(--v2-blue-600)" }.to_string(),
        ),
        (
            "v2-icon-icon-accent-hover",
            if dark { "var(--v2-blue-300)" } else { "var(--v2-blue-700)" }.to_string(),
        ),
    ];
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

pub fn resolve_theme_variant_v2(variant: &Variant, dark: bool) -> TokenMap {
    let colors = &variant.colors;
    let ink = colors.ink.as_deref().unwrap_or(&colors.neutral);
    let mut grey = generate_neutral_scale(&colors.neutral, dark, Some(ink));
    if dark {
        grey.reverse();
    }
    let ramps = [
        ("grey", grey),
        ("blue", hue_scale(&colors.interactive, dark)),
        ("green", hue_scale(&colors.success, dark)),
        ("yellow", hue_scale(&colors.warning, dark)),
        ("red", hue_scale(&colors.error, dark)),
        ("purple", hue_scale(&colors.accent, dark)),
        ("pink", hue_scale(&colors.info, dark)),
        (
            "orange",
            hue_scale(&shift(&colors.warning, -0.082, 0.94, -22.0), dark),
        ),
        (
            "cyan",
            hue_scale(&shift(&colors.info, 0.128, 1.12, -12.0), dark),
        ),
    ];

    let mut tokens = TokenMap::default();
    for (key, value) in &TABLES.defaults {
        tokens.set(key, value);
    }
    for (prefix, scale) in &ramps {
        for (step, hex) in STEPS.iter().zip(scale) {
            tokens.set(&format!("v2-{prefix}-{step}"), hex);
        }
    }
    let semantics = if dark { &TABLES.dark } else { &TABLES.light };
    let foreground = foreground(ink, dark, &tokens, semantics, variant);
    for (key, value) in semantics
        .iter()
        .chain(&foreground)
        .chain(&variant.v2_overrides)
    {
        tokens.set(key, value);
    }
    tokens
}
