#[path = "src/model/tsdict.rs"]
mod tsdict;

use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Default)]
struct Dict {
    entries: Vec<(String, String)>,
    index: HashMap<String, usize>,
}

impl Dict {
    fn extend(&mut self, pairs: Vec<(String, String)>) {
        for (key, value) in pairs {
            match self.index.get(&key) {
                Some(&at) => self.entries[at].1 = value,
                None => {
                    self.index.insert(key.clone(), self.entries.len());
                    self.entries.push((key, value));
                }
            }
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.index.get(key).map(|&at| self.entries[at].1.as_str())
    }
}

struct Locale {
    code: String,
    label: String,
    tag: String,
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/model/tsdict.rs");
    println!("cargo:rerun-if-env-changed=OPENCODE_TS_DIR");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let root = env::var_os("OPENCODE_TS_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../../opencode-ts"));
    let app = root.join("packages/app/src/i18n");
    let ui = root.join("packages/ui/src/i18n");
    for dir in [&app, &ui] {
        if dir.is_dir() {
            println!("cargo:rerun-if-changed={}", dir.display());
        }
    }

    let tsv = out.join("i18n");
    fs::create_dir_all(&tsv).expect("create i18n output directory");
    let mut table = String::from("pub static LOCALES: &[(&str, &str, &str, &str)] = &[\n");
    for (locale, dict) in generate(&root, &app, &ui) {
        let path = tsv.join(format!("{}.tsv", locale.code));
        fs::write(&path, dict).expect("write locale table");
        let path = path.to_string_lossy();
        let _ = writeln!(
            table,
            "    ({:?}, {:?}, {:?}, include_str!({:?})),",
            locale.code, locale.label, locale.tag, path
        );
    }
    table.push_str("];\n");
    fs::write(out.join("i18n_locales.rs"), table).expect("write locale index");

    // Icon resource 1 is what the window class and the taskbar show (`gpui-pre-windows` `load_icon`).
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=resources/windows/opencode-desktop.rc");
        println!("cargo:rerun-if-changed=assets/icons/icon.ico");
        embed_resource::compile("resources/windows/opencode-desktop.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("compile the Windows icon resource");
    }
}

fn generate(root: &Path, app: &Path, ui: &Path) -> Vec<(Locale, String)> {
    let english_only = || {
        let locale = Locale {
            code: "en".to_owned(),
            label: "English".to_owned(),
            tag: "en".to_owned(),
        };
        vec![(locale, String::new())]
    };
    if !app.join("en.ts").is_file() {
        warn(&format!(
            "TypeScript locale tree not found at {}; building English-only i18n with an empty dictionary",
            root.display()
        ));
        return english_only();
    }

    let native = fs::read_to_string(app.join("desktop-native.ts")).unwrap_or_else(|error| {
        warn(&format!("cannot read desktop-native.ts: {error}"));
        String::new()
    });
    let desktop = tsdict::object(&native, "const DESKTOP_NATIVE_ENGLISH");
    let keys: Vec<&str> = desktop.iter().map(|(key, _)| key.as_str()).collect();
    let labels: HashMap<String, String> = tsdict::object(&native, "const DESKTOP_NATIVE_LABELS")
        .into_iter()
        .collect();
    let tags: HashMap<String, String> = tsdict::object(&native, "const DESKTOP_NATIVE_LOCALE_TAGS")
        .into_iter()
        .collect();
    let mut codes = tsdict::array(&native, "const DESKTOP_NATIVE_LOCALES");
    if codes.is_empty() {
        warn("DESKTOP_NATIVE_LOCALES not found; using the locale files present on disk");
        codes = listed(app);
    }
    codes.retain(|code| {
        let valid = !code.is_empty()
            && code
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
        if !valid {
            warn(&format!("skipping invalid locale code {code:?}"));
        }
        valid
    });
    codes.dedup();
    if !codes.iter().any(|code| code == "en") {
        codes.insert(0, "en".to_owned());
    }

    let mut english = Dict::default();
    english.extend(desktop.clone());
    english.extend(load(&app.join("en.ts"), &keys).unwrap_or_default());
    english.extend(load(&ui.join("en.ts"), &keys).unwrap_or_default());

    codes
        .into_iter()
        .map(|code| {
            let mut text = String::new();
            if code == "en" {
                for (key, value) in &english.entries {
                    line(&mut text, key, value);
                }
            } else {
                let mut dict = Dict::default();
                dict.extend(load(&app.join(format!("{code}.ts")), &keys).unwrap_or_default());
                dict.extend(load(&ui.join(format!("{code}.ts")), &keys).unwrap_or_default());
                for (key, value) in &dict.entries {
                    if english.get(key) != Some(value.as_str()) {
                        line(&mut text, key, value);
                    }
                }
            }
            let locale = Locale {
                label: labels.get(&code).cloned().unwrap_or_else(|| code.clone()),
                tag: tags.get(&code).cloned().unwrap_or_else(|| code.clone()),
                code,
            };
            (locale, text)
        })
        .collect()
}

fn load(path: &Path, desktop: &[&str]) -> Option<Vec<(String, String)>> {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) => {
            warn(&format!("skipping {}: {error}", path.display()));
            return None;
        }
    };
    let mut pairs = Vec::new();
    // Some locales keep desktop strings as an array zipped with DESKTOP_NATIVE_KEYS.
    if source.contains("DESKTOP_NATIVE_KEYS") {
        let values = tsdict::array(&source, "const desktop");
        if values.len() != desktop.len() {
            warn(&format!(
                "{}: {} desktop values for {} desktop keys",
                path.display(),
                values.len(),
                desktop.len()
            ));
        }
        pairs.extend(desktop.iter().map(|key| (*key).to_owned()).zip(values));
    }
    pairs.extend(tsdict::pairs(&source));
    if pairs.is_empty() {
        warn(&format!(
            "skipping {}: no dictionary entries found",
            path.display()
        ));
        return None;
    }
    Some(pairs)
}

fn listed(dir: &Path) -> Vec<String> {
    let mut codes: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| name.strip_suffix(".ts").map(str::to_owned))
        .filter(|name| !name.contains('.') && name != "desktop-native" && name != "en")
        .collect();
    codes.sort();
    codes.insert(0, "en".to_owned());
    codes
}

fn line(out: &mut String, key: &str, value: &str) {
    escape(out, key);
    out.push('\t');
    escape(out, value);
    out.push('\n');
}

fn escape(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
}

fn warn(message: &str) {
    println!("cargo:warning={message}");
}
