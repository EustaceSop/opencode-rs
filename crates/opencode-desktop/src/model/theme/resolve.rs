use super::color::{
    blend, generate_neutral_scale, generate_scale, hex_to_oklch, hex_to_rgb, shift, with_alpha,
};
use super::{TokenMap, Variant};

fn neutral_alpha_scale(neutral: &[String], dark: bool) -> Vec<String> {
    let alphas: [f64; 12] = if dark {
        [0.038, 0.066, 0.1, 0.142, 0.19, 0.252, 0.334, 0.446, 0.58, 0.718, 0.854, 0.985]
    } else {
        [0.03, 0.06, 0.1, 0.145, 0.2, 0.265, 0.35, 0.47, 0.61, 0.74, 0.86, 0.97]
    };
    alphas
        .iter()
        .map(|alpha| blend(&neutral[11], &neutral[0], *alpha))
        .collect()
}

fn lum(hex: &str) -> f64 {
    let (r, g, b) = hex_to_rgb(hex);
    let lift = |v: f64| {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lift(r) + 0.7152 * lift(g) + 0.0722 * lift(b)
}

fn hit(a: &str, b: &str) -> f64 {
    let x = lum(a);
    let y = lum(b);
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

fn on(fill: &str) -> &'static str {
    if hit("#ffffff", fill) > hit("#000000", fill) {
        "#ffffff"
    } else {
        "#000000"
    }
}

pub fn resolve_theme_variant(variant: &Variant, dark: bool) -> TokenMap {
    let colors = &variant.colors;
    let compact = colors.compact;
    let has_override = |key: &str| variant.overrides.iter().any(|(k, _)| k == key);
    let pick = |d: usize, l: usize| if dark { d } else { l };
    let either = |d: &'static str, l: &'static str| if dark { d } else { l };

    let neutral = generate_neutral_scale(&colors.neutral, dark, colors.ink.as_deref());
    let primary = generate_scale(&colors.primary, dark);
    let accent = generate_scale(&colors.accent, dark);
    let success = generate_scale(&colors.success, dark);
    let warning = generate_scale(&colors.warning, dark);
    let error = generate_scale(&colors.error, dark);
    let info = generate_scale(&colors.info, dark);
    let interactive = generate_scale(&colors.interactive, dark);
    let amber = generate_scale(
        &if dark {
            shift(&colors.warning, -0.058, 1.14, -16.0)
        } else {
            shift(&colors.warning, -0.082, 0.94, -22.0)
        },
        dark,
    );
    let blue = generate_scale(&shift(&colors.interactive, 0.128, 1.12, -12.0), dark);
    let diff_add = generate_scale(
        &colors.diff_add.clone().unwrap_or_else(|| {
            shift(
                &colors.success,
                if dark { -0.18 } else { 0.14 },
                if dark { 0.7 } else { 0.55 },
                0.0,
            )
        }),
        dark,
    );
    let diff_delete = generate_scale(
        &colors.diff_delete.clone().unwrap_or_else(|| {
            shift(
                &colors.error,
                if dark { -0.08 } else { 0.08 },
                if dark { 0.82 } else { 0.7 },
                0.0,
            )
        }),
        dark,
    );
    let ink = colors.ink.as_deref().unwrap_or(&colors.neutral);
    let body = if compact {
        let tint = hex_to_oklch(ink);
        shift(
            ink,
            if dark {
                0.0_f64.max(0.88 - tint.l) * 0.4
            } else {
                -(0.0_f64.max(tint.l - 0.18)) * 0.24
            },
            if dark { 1.04 } else { 1.02 },
            0.0,
        )
    } else {
        String::new()
    };
    let background_override = variant
        .overrides
        .iter()
        .find(|(k, _)| k == "background-base")
        .map(|(_, v)| v.as_str());
    let background_hex = background_override.filter(|v| v.starts_with('#'));
    let overlay = background_override.is_some_and(|v| !v.is_empty()) && background_hex.is_none();
    let content = |seed: &str, scale: &[String]| {
        let base = hex_to_oklch(seed);
        let value = if dark && base.l > 0.84 {
            shift(seed, 0.0, 1.18, 0.0)
        } else {
            scale[10].clone()
        };
        shift(
            &value,
            if dark { 0.034 } else { -0.024 },
            if dark { 1.3 } else { 1.18 },
            0.0,
        )
    };
    let modified = || {
        if !compact {
            return either("#ffba92", "#FF8C00").to_string();
        }
        let warning_hue = hex_to_oklch(&colors.warning).h;
        let delete_hue = hex_to_oklch(colors.diff_delete.as_deref().unwrap_or(&colors.error)).h;
        let delta = (((((delete_hue - warning_hue) % 360.0) + 540.0) % 360.0) - 180.0).abs();
        if delta < 48.0 {
            return either("#ffba92", "#FF8C00").to_string();
        }
        content(&colors.warning, &warning)
    };
    let background = background_hex.unwrap_or(&neutral[0]).to_string();
    let alpha_tone = |color: &str, alpha: f64| {
        if overlay {
            with_alpha(color, alpha)
        } else {
            blend(color, &background, alpha)
        }
    };
    let border_tone = |light: f64, darker: f64| {
        alpha_tone(
            ink,
            if dark {
                1.0_f64.min(darker + 0.024 + if compact { 0.08 } else { 0.0 })
            } else {
                1.0_f64.min(light + 0.024)
            },
        )
    };
    let hidden_seed = if dark {
        shift(&colors.interactive, 0.0, 0.55, 0.0)
    } else {
        shift(&colors.interactive, 0.08, 0.45, 0.0)
    };
    // base, weak, weaker, strong, stronger
    let hidden_alpha: [f64; 5] = if dark {
        [0.14, 0.08, 0.18, 0.26, 0.42]
    } else {
        [0.12, 0.08, 0.16, 0.24, 0.36]
    };

    let na = neutral_alpha_scale(&neutral, dark);
    let brandb = &primary[8];
    let brandh = &primary[9];
    let interb = &interactive[pick(6, 4)];
    let interh = &interactive[pick(7, 5)];
    let interw = &interactive[pick(5, 3)];
    let succb = &success[pick(6, 4)];
    let succw = &success[pick(5, 3)];
    let succs = &success[10];
    let warnb = &warning[pick(6, 4)];
    let warnw = &warning[pick(5, 3)];
    let warns = &warning[10];
    let critb = &error[pick(6, 4)];
    let critw = &error[pick(5, 3)];
    let crits = &error[10];
    let infob = &info[pick(6, 4)];
    let infow = &info[pick(5, 3)];
    let infos = &info[10];

    let mut t = TokenMap::default();

    t.set("background-base", &neutral[0]);
    t.set("background-weak", &neutral[2]);
    t.set("background-strong", &neutral[0]);
    t.set("background-stronger", if dark { &neutral[1] } else { "#fcfcfc" });

    t.set("surface-base", &na[1]);
    t.set("base", &na[1]);
    t.set("surface-base-hover", &na[2]);
    t.set("surface-base-active", &na[2]);
    t.set("surface-base-interactive-active", &with_alpha(&interactive[2], 0.3));
    t.set("base2", &na[1]);
    t.set("base3", &na[1]);
    t.set("surface-inset-base", &na[1]);
    t.set("surface-inset-base-hover", &na[2]);
    let inset_strong = if dark {
        with_alpha(&neutral[0], 0.5)
    } else {
        with_alpha(&neutral[3], 0.09)
    };
    t.set("surface-inset-strong", &inset_strong);
    t.set("surface-inset-strong-hover", &inset_strong);
    t.set("surface-raised-base", &na[0]);
    t.set("surface-float-base", &neutral[pick(1, 11)]);
    t.set("surface-float-base-hover", &neutral[pick(2, 10)]);
    t.set("surface-raised-base-hover", &na[1]);
    t.set("surface-raised-base-active", &na[2]);
    t.set("surface-raised-strong", if dark { &na[3] } else { &neutral[0] });
    t.set("surface-raised-strong-hover", if dark { &na[5] } else { "#ffffff" });
    t.set("surface-raised-stronger", if dark { &na[5] } else { "#ffffff" });
    t.set("surface-raised-stronger-hover", if dark { &na[6] } else { "#ffffff" });
    t.set("surface-weak", &na[2]);
    t.set("surface-weaker", &na[3]);
    t.set("surface-strong", if dark { &na[6] } else { "#ffffff" });
    t.set(
        "surface-raised-stronger-non-alpha",
        if dark { &neutral[2] } else { "#ffffff" },
    );

    t.set("surface-brand-base", brandb);
    t.set("surface-brand-hover", brandh);

    t.set("surface-interactive-base", interb);
    t.set("surface-interactive-hover", interh);
    t.set("surface-interactive-weak", interw);
    t.set("surface-interactive-weak-hover", interb);

    t.set("surface-success-base", succb);
    t.set("surface-success-weak", succw);
    t.set("surface-success-strong", succs);
    t.set("surface-warning-base", warnb);
    t.set("surface-warning-weak", warnw);
    t.set("surface-warning-strong", warns);
    t.set("surface-critical-base", critb);
    t.set("surface-critical-weak", critw);
    t.set("surface-critical-strong", crits);
    t.set("surface-info-base", infob);
    t.set("surface-info-weak", infow);
    t.set("surface-info-strong", infos);

    t.set(
        "surface-diff-unchanged-base",
        if dark { &neutral[0] } else { "#ffffff00" },
    );
    t.set("surface-diff-skip-base", if dark { &na[0] } else { &neutral[1] });
    t.set("surface-diff-hidden-base", &alpha_tone(&hidden_seed, hidden_alpha[0]));
    t.set("surface-diff-hidden-weak", &alpha_tone(&hidden_seed, hidden_alpha[1]));
    t.set("surface-diff-hidden-weaker", &alpha_tone(&hidden_seed, hidden_alpha[2]));
    t.set("surface-diff-hidden-strong", &alpha_tone(&hidden_seed, hidden_alpha[3]));
    t.set("surface-diff-hidden-stronger", &alpha_tone(&hidden_seed, hidden_alpha[4]));
    t.set("surface-diff-add-base", &diff_add[2]);
    t.set("surface-diff-add-weak", &diff_add[pick(3, 1)]);
    t.set("surface-diff-add-weaker", &diff_add[pick(2, 0)]);
    t.set("surface-diff-add-strong", &diff_add[4]);
    t.set("surface-diff-add-stronger", &diff_add[pick(10, 8)]);
    t.set("surface-diff-delete-base", &diff_delete[2]);
    t.set("surface-diff-delete-weak", &diff_delete[pick(3, 1)]);
    t.set("surface-diff-delete-weaker", &diff_delete[pick(2, 0)]);
    t.set("surface-diff-delete-strong", &diff_delete[pick(4, 5)]);
    t.set("surface-diff-delete-stronger", &diff_delete[pick(10, 8)]);

    t.set("input-base", &neutral[pick(1, 0)]);
    t.set("input-hover", &neutral[pick(2, 1)]);
    t.set("input-active", &interactive[pick(6, 0)]);
    t.set("input-selected", &interactive[pick(7, 3)]);
    t.set("input-focus", &interactive[pick(6, 0)]);
    t.set("input-disabled", &neutral[3]);

    let text_base = if compact { body.clone() } else { neutral[10].clone() };
    let text_weak = if compact {
        shift(&body, if dark { -0.11 } else { 0.11 }, 0.9, 0.0)
    } else {
        neutral[8].clone()
    };
    let text_weaker = if compact {
        shift(
            &body,
            if dark { -0.2 } else { 0.21 },
            if dark { 0.78 } else { 0.72 },
            0.0,
        )
    } else {
        neutral[7].clone()
    };
    let text_strong = if !compact {
        neutral[11].clone()
    } else if dark {
        blend("#ffffff", &body, 0.9)
    } else {
        shift(&body, -0.07, 1.04, 0.0)
    };
    t.set("text-base", &text_base);
    t.set("text-weak", &text_weak);
    t.set("text-weaker", &text_weaker);
    t.set("text-strong", &text_strong);
    t.set("text-invert-base", &neutral[pick(10, 1)]);
    t.set("text-invert-weak", &neutral[pick(8, 2)]);
    t.set("text-invert-weaker", &neutral[pick(7, 3)]);
    t.set("text-invert-strong", &neutral[pick(11, 0)]);
    t.set("text-interactive-base", &interactive[pick(10, 9)]);
    t.set("text-on-brand-base", on(brandb));
    t.set("text-on-interactive-base", on(interb));
    t.set("text-on-interactive-weak", on(interb));
    t.set("text-on-success-base", on(succb));
    t.set("text-on-critical-base", on(critb));
    t.set("text-on-critical-weak", on(critb));
    t.set("text-on-critical-strong", on(crits));
    t.set("text-on-warning-base", on(warnb));
    t.set("text-on-info-base", on(infob));
    t.set("text-diff-add-base", &diff_add[10]);
    t.set("text-diff-delete-base", &diff_delete[9]);
    t.set("text-diff-delete-strong", &diff_delete[11]);
    t.set("text-diff-add-strong", &diff_add[pick(7, 11)]);
    t.set("text-on-info-weak", on(infob));
    t.set("text-on-info-strong", on(infos));
    t.set("text-on-warning-weak", on(warnb));
    t.set("text-on-warning-strong", on(warns));
    t.set("text-on-success-weak", on(succb));
    t.set("text-on-success-strong", on(succs));
    t.set("text-on-brand-weak", on(brandb));
    t.set("text-on-brand-weaker", on(brandb));
    t.set("text-on-brand-strong", on(brandh));

    t.set("button-primary-base", &neutral[11]);
    t.set("button-secondary-base", &neutral[pick(2, 0)]);
    t.set("button-secondary-hover", &neutral[pick(3, 1)]);
    t.set("button-ghost-hover", &na[1]);
    t.set("button-ghost-hover2", &na[2]);

    let border = |light: f64, darker: f64, fallback: usize| {
        if compact {
            border_tone(light, darker)
        } else {
            na[fallback].clone()
        }
    };
    let border_base = border(0.22, 0.16, 6);
    t.set("border-base", &border_base);
    t.set("border-hover", &border(0.28, 0.2, 7));
    t.set("border-active", &border(0.34, 0.24, 8));
    t.set(
        "border-selected",
        &with_alpha(&interactive[8], if dark { 0.9 } else { 0.99 }),
    );
    t.set("border-disabled", &border(0.18, 0.12, 7));
    t.set("border-focus", &border(0.34, 0.24, 8));
    t.set("border-weak-base", &border(0.1, 0.08, pick(5, 4)));
    t.set("border-strong-base", &border(0.34, 0.24, pick(7, 6)));
    t.set("border-strong-hover", &border(0.4, 0.28, 7));
    t.set("border-strong-active", &border(0.46, 0.32, pick(7, 6)));
    t.set("border-strong-selected", &with_alpha(&interactive[5], 0.6));
    t.set("border-strong-disabled", &border(0.14, 0.1, 5));
    t.set("border-strong-focus", &border(0.46, 0.32, pick(7, 6)));
    t.set("border-weak-hover", &border(0.16, 0.12, pick(6, 5)));
    t.set("border-weak-active", &border(0.22, 0.16, pick(7, 6)));
    t.set(
        "border-weak-selected",
        &with_alpha(&interactive[4], if dark { 0.6 } else { 0.5 }),
    );
    t.set("border-weak-disabled", &border(0.08, 0.06, 5));
    t.set("border-weak-focus", &border(0.22, 0.16, pick(7, 6)));
    t.set("border-weaker-base", &border(0.06, 0.04, 2));

    t.set("border-interactive-base", &interactive[6]);
    t.set("border-interactive-hover", &interactive[7]);
    t.set("border-interactive-active", &interactive[8]);
    t.set("border-interactive-selected", &interactive[8]);
    t.set("border-interactive-disabled", &neutral[7]);
    t.set("border-interactive-focus", &interactive[8]);

    t.set("border-success-base", &success[6]);
    t.set("border-success-hover", &success[7]);
    t.set("border-success-selected", &success[8]);
    t.set("border-warning-base", &warning[6]);
    t.set("border-warning-hover", &warning[7]);
    t.set("border-warning-selected", &warning[8]);
    t.set("border-critical-base", &error[6]);
    t.set("border-critical-hover", &error[7]);
    t.set("border-critical-selected", &error[8]);
    t.set("border-info-base", &info[6]);
    t.set("border-info-hover", &info[7]);
    t.set("border-info-selected", &info[8]);
    t.set("border-color", "#ffffff");

    let inked = compact && !dark;
    t.set("icon-base", if inked { &text_weak } else { &neutral[pick(9, 8)] });
    t.set("icon-hover", if inked { &text_base } else { &neutral[10] });
    t.set("icon-active", if inked { &text_strong } else { &neutral[11] });
    t.set("icon-selected", if inked { &text_strong } else { &neutral[11] });
    t.set("icon-disabled", &neutral[pick(6, 7)]);
    t.set("icon-focus", if inked { &text_strong } else { &neutral[11] });
    t.set("icon-invert-base", if dark { &neutral[0] } else { "#ffffff" });
    t.set("icon-weak-base", &neutral[pick(5, 6)]);
    t.set("icon-weak-hover", &neutral[pick(11, 7)]);
    t.set("icon-weak-active", &neutral[8]);
    t.set("icon-weak-selected", &neutral[pick(8, 9)]);
    t.set("icon-weak-disabled", &neutral[pick(3, 5)]);
    t.set("icon-weak-focus", &neutral[8]);
    t.set("icon-strong-base", &neutral[11]);
    t.set("icon-strong-hover", either("#f6f3f3", "#151313"));
    t.set("icon-strong-active", either("#fcfcfc", "#020202"));
    t.set("icon-strong-selected", either("#fdfcfc", "#020202"));
    t.set("icon-strong-disabled", &neutral[7]);
    t.set("icon-strong-focus", either("#fdfcfc", "#020202"));
    t.set("icon-brand-base", if dark { "#ffffff" } else { &neutral[11] });
    t.set("icon-interactive-base", &interactive[8]);
    t.set("icon-success-base", &success[pick(8, 6)]);
    t.set("icon-success-hover", &success[9]);
    t.set("icon-success-active", &success[10]);
    t.set("icon-warning-base", &amber[pick(8, 6)]);
    t.set("icon-warning-hover", &amber[9]);
    t.set("icon-warning-active", &amber[10]);
    t.set("icon-critical-base", &error[pick(8, 9)]);
    t.set("icon-critical-hover", &error[9]);
    t.set("icon-critical-active", &error[10]);
    t.set("icon-info-base", &info[pick(8, 6)]);
    t.set("icon-info-hover", &info[pick(9, 7)]);
    t.set("icon-info-active", &info[10]);
    t.set("icon-on-brand-base", on(brandb));
    t.set("icon-on-brand-hover", on(brandh));
    t.set("icon-on-brand-selected", on(brandh));
    t.set("icon-on-interactive-base", on(interb));

    t.set("icon-agent-plan-base", &info[8]);
    t.set("icon-agent-docs-base", &amber[8]);
    t.set("icon-agent-ask-base", &blue[8]);
    t.set("icon-agent-build-base", &interactive[pick(10, 8)]);

    t.set("icon-on-success-base", on(succb));
    t.set("icon-on-success-hover", on(succs));
    t.set("icon-on-success-selected", on(succs));
    t.set("icon-on-warning-base", on(warnb));
    t.set("icon-on-warning-hover", on(warns));
    t.set("icon-on-warning-selected", on(warns));
    t.set("icon-on-critical-base", on(critb));
    t.set("icon-on-critical-hover", on(crits));
    t.set("icon-on-critical-selected", on(crits));
    t.set("icon-on-info-base", on(infob));
    t.set("icon-on-info-hover", on(infos));
    t.set("icon-on-info-selected", on(infos));

    t.set("icon-diff-add-base", &diff_add[10]);
    t.set("icon-diff-add-hover", &diff_add[pick(9, 11)]);
    t.set("icon-diff-add-active", &diff_add[pick(10, 11)]);
    t.set("icon-diff-delete-base", &diff_delete[9]);
    t.set("icon-diff-delete-hover", &diff_delete[10]);
    t.set("icon-diff-modified-base", &modified());

    t.set("syntax-comment", "var(--text-weak)");
    t.set("syntax-regexp", "var(--text-base)");
    if compact {
        t.set("syntax-string", &content(&colors.success, &success));
        t.set("syntax-keyword", &content(&colors.accent, &accent));
        t.set("syntax-primitive", &content(&colors.primary, &primary));
    } else {
        t.set("syntax-string", either("#00ceb9", "#006656"));
        t.set("syntax-keyword", "var(--text-weak)");
        t.set("syntax-primitive", either("#ffba92", "#fb4804"));
    }
    t.set("syntax-operator", either("var(--text-weak)", "var(--text-base)"));
    t.set("syntax-variable", "var(--text-strong)");
    if compact {
        t.set("syntax-property", &content(&colors.info, &info));
        t.set("syntax-type", &content(&colors.warning, &warning));
        t.set("syntax-constant", &content(&colors.accent, &accent));
    } else {
        t.set("syntax-property", either("#ff9ae2", "#ed6dc8"));
        t.set("syntax-type", either("#ecf58c", "#596600"));
        t.set("syntax-constant", either("#93e9f6", "#007b80"));
    }
    t.set("syntax-punctuation", either("var(--text-weak)", "var(--text-base)"));
    t.set("syntax-object", "var(--text-strong)");
    t.set("syntax-success", &success[10]);
    t.set("syntax-warning", &amber[10]);
    t.set("syntax-critical", &error[10]);
    if compact {
        t.set("syntax-info", &content(&colors.info, &info));
    } else {
        t.set("syntax-info", either("#93e9f6", "#0092a8"));
    }
    t.set("syntax-diff-add", &diff_add[10]);
    t.set("syntax-diff-delete", &diff_delete[10]);
    t.set("syntax-diff-unknown", "#ff0000");

    if compact {
        t.set("markdown-heading", &content(&colors.primary, &primary));
        t.set("markdown-text", &text_base);
        t.set("markdown-link", &content(&colors.interactive, &interactive));
        t.set("markdown-link-text", &content(&colors.info, &info));
        t.set("markdown-code", &content(&colors.success, &success));
        t.set("markdown-block-quote", &content(&colors.warning, &warning));
        t.set("markdown-emph", &content(&colors.warning, &warning));
        t.set("markdown-strong", &content(&colors.accent, &accent));
        t.set("markdown-horizontal-rule", &border_base);
        t.set("markdown-list-item", &content(&colors.interactive, &interactive));
        t.set("markdown-list-enumeration", &content(&colors.info, &info));
        t.set("markdown-image", &content(&colors.interactive, &interactive));
        t.set("markdown-image-text", &content(&colors.info, &info));
        t.set("markdown-code-block", &text_base);
    } else {
        t.set("markdown-heading", either("#9d7cd8", "#d68c27"));
        t.set("markdown-text", either("#eeeeee", "#1a1a1a"));
        t.set("markdown-link", either("#fab283", "#3b7dd8"));
        t.set("markdown-link-text", either("#56b6c2", "#318795"));
        t.set("markdown-code", either("#7fd88f", "#3d9a57"));
        t.set("markdown-block-quote", either("#e5c07b", "#b0851f"));
        t.set("markdown-emph", either("#e5c07b", "#b0851f"));
        t.set("markdown-strong", either("#f5a742", "#d68c27"));
        t.set("markdown-horizontal-rule", either("#808080", "#8a8a8a"));
        t.set("markdown-list-item", either("#fab283", "#3b7dd8"));
        t.set("markdown-list-enumeration", either("#56b6c2", "#318795"));
        t.set("markdown-image", either("#fab283", "#3b7dd8"));
        t.set("markdown-image-text", either("#56b6c2", "#318795"));
        t.set("markdown-code-block", either("#eeeeee", "#1a1a1a"));
    }

    t.set("avatar-background-pink", either("#501b3f", "#feeef8"));
    t.set("avatar-background-mint", either("#033a34", "#e1fbf4"));
    t.set("avatar-background-orange", either("#5f2a06", "#fff1e7"));
    t.set("avatar-background-purple", either("#432155", "#f9f1fe"));
    t.set("avatar-background-cyan", either("#0f3058", "#e7f9fb"));
    t.set("avatar-background-lime", either("#2b3711", "#eefadc"));
    t.set("avatar-text-pink", either("#e34ba9", "#cd1d8d"));
    t.set("avatar-text-mint", either("#95f3d9", "#147d6f"));
    t.set("avatar-text-orange", either("#ff802b", "#ed5f00"));
    t.set("avatar-text-purple", either("#9d5bd2", "#8445bc"));
    t.set("avatar-text-cyan", either("#369eff", "#0894b3"));
    t.set("avatar-text-lime", either("#c4f042", "#5d770d"));

    for (key, value) in &variant.overrides {
        t.set(key, value);
    }

    if compact && has_override("text-weak") && !has_override("text-weaker") {
        let weak = t.get("text-weak").unwrap_or_default().to_string();
        if weak.starts_with('#') {
            let shifted = shift(&weak, if dark { -0.12 } else { 0.12 }, 0.75, 0.0);
            t.set("text-weaker", &shifted);
        } else {
            t.set("text-weaker", &weak);
        }
    }

    if compact {
        let base = t.get("text-base").unwrap_or_default().to_string();
        if !has_override("markdown-text") {
            t.set("markdown-text", &base);
        }
        if !has_override("markdown-code-block") {
            t.set("markdown-code-block", &base);
        }
    }

    if !has_override("text-stronger") {
        let strong = t.get("text-strong").unwrap_or_default().to_string();
        t.set("text-stronger", &strong);
    }

    t
}
