// Installs an opencode theme into gpui-component's global theme.
use crate::model::settings::Settings;
use crate::model::theme::{self, Theme as Palette};
use gpui_kit::component::{Theme, ThemeConfig, ThemeMode};
use gpui_kit::{App, Window, px};
use std::rc::Rc;

pub fn system_is_dark(window: Option<&Window>, cx: &App) -> bool {
    let appearance = window.map_or_else(|| cx.window_appearance(), |window| window.appearance());
    ThemeMode::from(appearance).is_dark()
}

fn config(palette: &Palette, dark: bool) -> Option<Rc<ThemeConfig>> {
    serde_json::from_value::<ThemeConfig>(theme::component_theme(palette, dark)).ok().map(Rc::new)
}

// `preview` is a theme id shown without being saved (hovering the theme list).
pub fn apply(settings: &Settings, preview: Option<&str>, window: Option<&Window>, cx: &mut App) {
    let dark = settings.scheme.is_dark(system_is_dark(window, cx));
    let palette = preview.and_then(theme::find).unwrap_or_else(|| settings.theme());
    // The active variant goes last: it decides the mode; the other one stays registered for it.
    for variant in [!dark, dark] {
        if let Some(config) = config(palette, variant) {
            Theme::update(cx, |theme| theme.apply_config(&config));
        }
    }
    let size = settings.font_size();
    Theme::update(cx, |theme| {
        theme.font_size = px(size);
        theme.mono_font_size = px(size - 1.0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::Scheme;
    use gpui_kit::TestAppContext;
    use gpui_kit::component::ActiveTheme as _;

    fn hex(color: gpui_kit::Hsla) -> String {
        let rgba = gpui_kit::Rgba::from(color);
        let byte = |v: f32| (v * 255.0).round() as u8;
        format!("#{:02x}{:02x}{:02x}", byte(rgba.r), byte(rgba.g), byte(rgba.b))
    }

    fn token(palette: &Palette, dark: bool, name: &str) -> String {
        palette.tokens(dark).color(name).unwrap().hex()[..7].to_string()
    }

    #[gpui_kit::test]
    fn every_theme_installs_in_both_modes(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        for palette in theme::themes() {
            for scheme in [Scheme::Light, Scheme::Dark] {
                let settings = Settings { theme: palette.id.clone(), scheme, font_size: 15.0, ..Default::default() };
                cx.update(|cx| {
                    assert!(config(palette, scheme == Scheme::Dark).is_some(), "{} does not parse as a component theme", palette.id);
                    apply(&settings, None, None, cx);
                    let installed = cx.theme();
                    let dark = scheme == Scheme::Dark;
                    assert_eq!(installed.is_dark(), dark, "{}", palette.id);
                    assert_eq!(hex(installed.background), token(palette, dark, "v2-background-bg-base"), "{} background", palette.id);
                    assert_eq!(hex(installed.foreground), token(palette, dark, "v2-text-text-base"), "{} foreground", palette.id);
                    assert_eq!(hex(installed.sidebar), token(palette, dark, "v2-background-bg-deep"), "{} sidebar", palette.id);
                    assert_eq!(installed.font_size, px(15.0));
                });
            }
        }
    }

    #[gpui_kit::test]
    fn previews_override_the_saved_theme_until_cleared(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let settings = Settings { scheme: Scheme::Dark, ..Default::default() };
        let dracula = theme::find("dracula").unwrap();
        cx.update(|cx| {
            apply(&settings, Some("dracula"), None, cx);
            assert_eq!(hex(cx.theme().background), token(dracula, true, "v2-background-bg-base"));
            apply(&settings, Some("no-such-theme"), None, cx);
            assert_eq!(hex(cx.theme().background), token(settings.theme(), true, "v2-background-bg-base"));
            apply(&settings, None, None, cx);
            assert!(cx.theme().is_dark());
            // Both variants stay registered, so a system appearance change has something to switch to.
            assert!(cx.theme().light_theme.name.contains("Light") && cx.theme().dark_theme.name.contains("Dark"));
        });
    }
}
