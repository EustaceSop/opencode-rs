use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use super::i18n_supplement::SUPPLEMENT;

include!(concat!(env!("OUT_DIR"), "/i18n_locales.rs"));

type Dict = HashMap<String, Arc<str>>;

// Right-to-left locales of the original (`language.tsx`); layout mirroring is not implemented here.
#[cfg(test)]
const RTL: &[&str] = &["ar", "ur", "pa", "fa", "dv"];

static ENGLISH: LazyLock<Arc<Dict>> = LazyLock::new(|| Arc::new(parse(source("en"))));
static CACHE: LazyLock<Mutex<HashMap<&'static str, Arc<Dict>>>> = LazyLock::new(Mutex::default);
static NEWER: LazyLock<HashMap<&'static str, &'static [(&'static str, &'static str)]>> =
    LazyLock::new(|| super::i18n_v2::V2.iter().copied().collect());
static EXTRA: LazyLock<HashMap<&'static str, Arc<str>>> = LazyLock::new(|| {
    SUPPLEMENT
        .iter()
        .map(|(key, value)| (*key, Arc::from(*value)))
        .collect()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Locale {
    pub code: &'static str,
    pub label: &'static str,
    pub tag: &'static str,
}

pub fn locales() -> Vec<Locale> {
    LOCALES
        .iter()
        .map(|&(code, label, tag, _)| Locale { code, label, tag })
        .collect()
}

#[cfg(test)]
pub fn is_rtl(code: &str) -> bool {
    RTL.contains(&code)
}

pub fn matching(locale: &str) -> Option<&'static str> {
    let source = Tag::parse(locale)?;
    let exact = source.to_string();
    if let Some(entry) = LOCALES
        .iter()
        .find(|entry| entry.2.eq_ignore_ascii_case(&exact))
    {
        return Some(entry.0);
    }
    if matches!(source.language.as_str(), "no" | "nb" | "nn")
        && let Some(code) = known("no")
    {
        return Some(code);
    }
    let script = source.script();
    LOCALES
        .iter()
        .find(|entry| {
            Tag::parse(entry.2).is_some_and(|target| {
                target.language == source.language
                    && match (&script, target.script()) {
                        (Some(wanted), Some(found)) => *wanted == found,
                        _ => true,
                    }
            })
        })
        .map(|entry| entry.0)
}

// The POSIX locale variables when set, then (Rust build) the system's own settings: on Windows the user's display
// languages in order of preference and then the region's locale, which those variables never carry there.
pub fn detect() -> &'static str {
    let posix = ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"].iter().filter_map(|name| std::env::var(name).ok()).filter(|v| !v.trim().is_empty());
    detect_from(posix.chain(system_languages()))
}

#[cfg(windows)]
fn system_languages() -> Vec<String> {
    use windows_sys::Win32::Globalization::{GetUserDefaultLocaleName, GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};
    let mut out = Vec::new();
    let (mut count, mut size) = (0u32, 0u32);
    // SAFETY: the first call only reports the buffer size; the second fills a buffer of exactly that size.
    if unsafe { GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, std::ptr::null_mut(), &mut size) } != 0 && size > 0 {
        let mut buffer = vec![0u16; size as usize];
        if unsafe { GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, buffer.as_mut_ptr(), &mut size) } != 0 {
            out.extend(buffer.split(|&c| c == 0).filter(|name| !name.is_empty()).map(String::from_utf16_lossy));
        }
    }
    let mut name = [0u16; 85];
    // SAFETY: the buffer holds LOCALE_NAME_MAX_LENGTH (85) characters, as the call requires.
    let written = unsafe { GetUserDefaultLocaleName(name.as_mut_ptr(), name.len() as i32) };
    if written > 1 {
        out.push(String::from_utf16_lossy(&name[..written as usize - 1]));
    }
    out
}

#[cfg(not(windows))]
fn system_languages() -> Vec<String> {
    Vec::new()
}

fn detect_from(values: impl IntoIterator<Item = String>) -> &'static str {
    for value in values {
        for item in value.split(':') {
            let name = item.split(['.', '@']).next().unwrap_or(item).trim();
            if name == "C" || name == "POSIX" {
                return "en";
            }
            if let Some(code) = matching(item) {
                return code;
            }
        }
    }
    "en"
}

struct Tag {
    language: String,
    script: Option<String>,
    region: Option<String>,
}

impl Tag {
    fn parse(value: &str) -> Option<Self> {
        let (value, modifier) = match value.trim().split_once('@') {
            Some((value, modifier)) => (value, Some(modifier)),
            None => (value.trim(), None),
        };
        let value = value.split('.').next().unwrap_or(value);
        let mut parts = value.split(['-', '_']);
        let language = parts.next()?.to_ascii_lowercase();
        if !(2..=3).contains(&language.len()) || !language.bytes().all(|b| b.is_ascii_alphabetic())
        {
            return None;
        }
        let mut script = None;
        let mut region = None;
        for part in parts {
            let alpha = part.bytes().all(|b| b.is_ascii_alphabetic());
            let digits = part.bytes().all(|b| b.is_ascii_digit());
            if part.len() == 4 && alpha && script.is_none() && region.is_none() {
                script = Some(part.to_ascii_lowercase());
            } else if region.is_none()
                && ((part.len() == 2 && alpha) || (part.len() == 3 && digits))
            {
                region = Some(part.to_ascii_uppercase());
            }
        }
        if script.is_none() {
            script = match modifier.map(str::to_ascii_lowercase).as_deref() {
                Some("latin") => Some("latn".to_owned()),
                Some("cyrillic") => Some("cyrl".to_owned()),
                _ => None,
            };
        }
        Some(Self {
            language,
            script,
            region,
        })
    }

    // Explicit script, else the likely script for the few languages where it decides the match.
    fn script(&self) -> Option<String> {
        if self.script.is_some() {
            return self.script.clone();
        }
        let region = self.region.as_deref().unwrap_or_default();
        let script = match self.language.as_str() {
            "zh" if matches!(region, "TW" | "HK" | "MO") => "hant",
            "zh" => "hans",
            "pa" if region == "PK" => "arab",
            "pa" => "guru",
            "sr" if region == "ME" => "latn",
            "sr" => "cyrl",
            "uz" if region == "AF" => "arab",
            "uz" => "latn",
            "az" if matches!(region, "IR" | "IQ") => "arab",
            "az" if region == "RU" => "cyrl",
            "az" => "latn",
            "tg" if region == "PK" => "arab",
            "tg" => "cyrl",
            "mn" if region == "CN" => "mong",
            "mn" => "cyrl",
            "tk" | "bs" => "latn",
            _ => return None,
        };
        Some(script.to_owned())
    }
}

impl std::fmt::Display for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.language)?;
        for part in [&self.script, &self.region].into_iter().flatten() {
            write!(f, "-{part}")?;
        }
        Ok(())
    }
}

fn known(code: &str) -> Option<&'static str> {
    LOCALES
        .iter()
        .find(|entry| entry.0 == code)
        .map(|entry| entry.0)
}

fn source(code: &str) -> &'static str {
    LOCALES
        .iter()
        .find(|entry| entry.0 == code)
        .map_or("", |entry| entry.3)
}

fn parse(table: &str) -> Dict {
    table
        .split('\n')
        .filter_map(|line| line.split_once('\t'))
        .map(|(key, value)| (unescape(key).into_owned(), Arc::from(unescape(value))))
        .collect()
}

fn unescape(text: &str) -> Cow<'_, str> {
    if !text.contains('\\') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    Cow::Owned(out)
}

fn load(code: &'static str) -> Arc<Dict> {
    if code == "en" {
        return ENGLISH.clone();
    }
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    cache
        .entry(code)
        .or_insert_with(|| Arc::new(parse(source(code))))
        .clone()
}

fn interpolate(text: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start + 2..].find("}}") else {
            break;
        };
        let end = start + 2 + len;
        let name = rest[start + 2..end].trim();
        out.push_str(&rest[..start]);
        match vars.iter().find(|(key, _)| *key == name) {
            Some((_, value)) => out.push_str(value),
            None => out.push_str(&rest[start..end + 2]),
        }
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    out
}

#[derive(Clone)]
pub struct I18n {
    code: &'static str,
    dict: Arc<Dict>,
    english: Arc<Dict>,
}

impl I18n {
    pub fn new(code: &str) -> Self {
        let code = known(code).unwrap_or("en");
        Self {
            code,
            dict: load(code),
            english: ENGLISH.clone(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    #[cfg(test)]
    pub fn has(&self, key: &str) -> bool {
        self.original(key).is_some()
    }

    pub fn t(&self, key: &str) -> Arc<str> {
        self.find(key).unwrap_or_else(|| Arc::from(key))
    }

    pub fn tf(&self, key: &str, vars: &[(&str, &str)]) -> String {
        interpolate(&self.t(key), vars)
    }

    pub fn tn(&self, key: &str, count: u64, vars: &[(&str, &str)]) -> String {
        let count_text = count.to_string();
        let mut all = Vec::with_capacity(vars.len() + 1);
        all.push(("count", count_text.as_str()));
        all.extend_from_slice(vars);
        let category = if count == 1 { "one" } else { "other" };
        let text = self
            .find(&format!("{key}.{category}"))
            .or_else(|| self.find(&format!("{key}.other")))
            .unwrap_or_else(|| self.t(key));
        interpolate(&text, &all)
    }

    fn original(&self, key: &str) -> Option<&Arc<str>> {
        self.dict.get(key).or_else(|| self.english.get(key))
    }

    fn find(&self, key: &str) -> Option<Arc<str>> {
        // Keys of the v2.2.7 dictionaries win over the older dictionaries, which lack them or carry older text.
        if let Some(values) = NEWER.get(key) {
            let pick = |code: &str| values.iter().find(|(c, _)| *c == code).map(|(_, text)| Arc::from(*text));
            return pick(self.code).or_else(|| pick("en"));
        }
        self.original(key).or_else(|| EXTRA.get(key)).cloned()
    }
}

impl Default for I18n {
    fn default() -> Self {
        Self::new("en")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_locales_load() {
        let all = locales();
        assert_eq!(all.len(), 62);
        assert_eq!(
            all[0],
            Locale {
                code: "en",
                label: "English",
                tag: "en"
            }
        );
        assert!(ENGLISH.len() > 1200, "english has {} keys", ENGLISH.len());
        for locale in &all {
            assert!(!locale.label.is_empty() && !locale.tag.is_empty());
            let i18n = I18n::new(locale.code);
            assert_eq!(i18n.code(), locale.code);
            assert!(i18n.has("common.cancel"));
            if locale.code != "en" {
                assert!(
                    i18n.dict.len() > 1000,
                    "{} has {} keys",
                    locale.code,
                    i18n.dict.len()
                );
                assert!(!i18n.t("desktop.menu.file").is_empty());
            }
        }
        let label = |code: &str| {
            all.iter()
                .find(|locale| locale.code == code)
                .map(|locale| (locale.label, locale.tag))
        };
        assert_eq!(label("zht"), Some(("繁體中文", "zh-Hant")));
        assert_eq!(label("sr").map(|found| found.1), Some("sr-Cyrl-RS"));
    }

    #[test]
    fn known_keys_resolve() {
        let en = I18n::default();
        assert_eq!(&*en.t("common.cancel"), "Cancel");
        assert_eq!(&*en.t("desktop.menu.file"), "File");
        assert_eq!(&*en.t("ui.common.cancel"), "Cancel");
        assert_eq!(&*I18n::new("de").t("common.cancel"), "Abbrechen");
        for code in ["zh", "ja", "hr"] {
            let i18n = I18n::new(code);
            assert!(i18n.dict.contains_key("common.cancel"), "{code}");
            assert_ne!(&*i18n.t("common.cancel"), "Cancel", "{code}");
            assert_ne!(&*i18n.t("desktop.menu.file"), "File", "{code}");
        }
        assert!(Arc::ptr_eq(&en.t("common.cancel"), &en.t("common.cancel")));
    }

    #[test]
    fn interpolation() {
        let en = I18n::default();
        assert_eq!(
            en.tf("session.delete.confirm", &[("name", "demo")]),
            "Delete session \"demo\"?"
        );
        assert_eq!(
            I18n::new("de").tf("session.delete.confirm", &[("name", "demo")]),
            "Sitzung \"demo\" löschen?"
        );
        assert_eq!(
            en.tf("session.delete.confirm", &[]),
            "Delete session \"{{name}}\"?"
        );
        assert_eq!(
            interpolate("{{ a }}-{{b}}-{{c}}-{{a", &[("a", "1"), ("b", "{{a}}")]),
            "1-{{a}}-{{c}}-{{a"
        );
    }

    #[test]
    fn plurals() {
        let en = I18n::default();
        assert_eq!(
            en.tn("session.revertDock.summary", 1, &[]),
            "1 rolled back message"
        );
        assert_eq!(
            en.tn("session.revertDock.summary", 3, &[]),
            "3 rolled back messages"
        );
        assert_eq!(
            en.tn("session.revertDock.summary", 0, &[]),
            "0 rolled back messages"
        );
        assert_eq!(
            I18n::new("de").tn("session.revertDock.summary", 2, &[]),
            "2 zurückgesetzte Nachrichten"
        );
        assert_eq!(
            en.tn("session.delete.confirm", 2, &[("name", "x")]),
            "Delete session \"x\"?"
        );
        assert_eq!(en.tn("missing.plural", 2, &[]), "missing.plural");
    }

    #[test]
    fn fallback_chain() {
        let de = I18n::new("de");
        assert!(!de.dict.contains_key("desktop.menu.app"));
        assert_eq!(&*de.t("desktop.menu.app"), "OpenCode");
        assert!(de.has("desktop.menu.app"));
        assert_eq!(&*de.t("desktop.rs.tool.input"), "Input");
        assert!(!de.has("desktop.rs.tool.input"));
        assert_eq!(&*de.t("missing.key"), "missing.key");
        assert!(!de.has("missing.key"));
        assert_eq!(I18n::new("xx").code(), "en");
        assert_eq!(I18n::new("").code(), "en");
    }

    #[test]
    fn escaped_values_round_trip() {
        let hr = I18n::new("hr");
        assert_eq!(
            &*hr.t("error.chain.responseBody"),
            "Tijelo odgovora:\n{{body}}"
        );
        assert_eq!(unescape("a\\tb\\nc\\rd\\\\e\\"), "a\tb\nc\rd\\e\\");
        for locale in locales() {
            for value in I18n::new(locale.code).dict.values() {
                assert!(!value.contains("${"), "{}: {value}", locale.code);
            }
        }
    }

    #[test]
    fn matching_locales() {
        for (input, expected) in [
            ("en_US.UTF-8", Some("en")),
            ("zh_TW.UTF-8", Some("zht")),
            ("zh-HK", Some("zht")),
            ("zh-Hant", Some("zht")),
            ("zh-Hans-HK", Some("zh")),
            ("zh_CN", Some("zh")),
            ("zh", Some("zh")),
            ("pt-BR", Some("br")),
            ("pt_PT", Some("br")),
            ("de-AT", Some("de")),
            ("DE", Some("de")),
            ("nb_NO.UTF-8", Some("no")),
            ("nn", Some("no")),
            ("no", Some("no")),
            ("sr_RS", Some("sr")),
            ("sr_RS@latin", None),
            ("sr-Latn-RS", None),
            ("uz", Some("uz")),
            ("uz-Cyrl", None),
            ("pa-PK", Some("pa")),
            ("pa-IN", None),
            ("ja_JP.eucJP", Some("ja")),
            ("C", None),
            ("POSIX", None),
            ("xx-YY", None),
            ("", None),
        ] {
            assert_eq!(matching(input), expected, "{input}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_reports_its_languages() {
        let languages = system_languages();
        eprintln!("system languages: {languages:?}");
        assert!(!languages.is_empty() && languages.iter().all(|name| !name.is_empty() && !name.contains('\0')));
    }

    #[test]
    fn detection_and_direction() {
        let from = |values: &[&str]| detect_from(values.iter().map(|value| (*value).to_owned()));
        assert_eq!(from(&[]), "en");
        assert_eq!(from(&["", "de_DE.UTF-8", "fr"]), "de");
        assert_eq!(from(&["xx", "tlh:fr_FR:de"]), "fr");
        assert_eq!(from(&["C.UTF-8", "de_DE"]), "en");
        // Windows names: display languages and region locales.
        assert_eq!(from(&["zh-TW"]), matching("zh-Hant").unwrap());
        assert_eq!(from(&["zh-CN", "en-US"]), matching("zh-Hans").unwrap());
        assert_eq!(from(&["ja-JP"]), "ja");
        for code in ["ar", "ur", "pa", "fa", "dv"] {
            assert!(is_rtl(code));
        }
        assert!(!is_rtl("en") && !is_rtl("zh") && !is_rtl("he"));
    }
}
