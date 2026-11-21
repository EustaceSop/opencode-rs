use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::util::{self, RunOptions};

const JS: &[&str] = &[
    ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".html", ".htm", ".css", ".scss",
    ".sass", ".less", ".vue", ".svelte", ".json", ".jsonc", ".yaml", ".yml", ".toml", ".xml",
    ".md", ".mdx", ".graphql", ".gql",
];
const RUBY: &[&str] = &[".rb", ".rake", ".gemspec", ".ru"];

pub struct Builtin {
    /// Export identifier in `format/formatter.ts`; config keys are matched against this, not `name`.
    pub key: &'static str,
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub bun: bool,
}

const fn b(key: &'static str, name: &'static str, extensions: &'static [&'static str]) -> Builtin {
    Builtin {
        key,
        name,
        extensions,
        bun: false,
    }
}

pub const BUILTINS: &[Builtin] = &[
    b("gofmt", "gofmt", &[".go"]),
    b(
        "mix",
        "mix",
        &[".ex", ".exs", ".eex", ".heex", ".leex", ".neex", ".sface"],
    ),
    Builtin {
        key: "prettier",
        name: "prettier",
        extensions: JS,
        bun: true,
    },
    Builtin {
        key: "oxfmt",
        name: "oxfmt",
        extensions: &[".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts"],
        bun: true,
    },
    Builtin {
        key: "biome",
        name: "biome",
        extensions: JS,
        bun: true,
    },
    b("zig", "zig", &[".zig", ".zon"]),
    b(
        "clang",
        "clang-format",
        &[
            ".c", ".cc", ".cpp", ".cxx", ".c++", ".h", ".hh", ".hpp", ".hxx", ".h++", ".ino", ".C",
            ".H",
        ],
    ),
    b("ktlint", "ktlint", &[".kt", ".kts"]),
    b("ruff", "ruff", &[".py", ".pyi"]),
    b("rlang", "air", &[".R"]),
    b("uvformat", "uv", &[".py", ".pyi"]),
    b("rubocop", "rubocop", RUBY),
    b("standardrb", "standardrb", RUBY),
    b("htmlbeautifier", "htmlbeautifier", &[".erb", ".html.erb"]),
    b("dart", "dart", &[".dart"]),
    b("ocamlformat", "ocamlformat", &[".ml", ".mli"]),
    b("terraform", "terraform", &[".tf", ".tfvars"]),
    b("latexindent", "latexindent", &[".tex"]),
    b("gleam", "gleam", &[".gleam"]),
    b("shfmt", "shfmt", &[".sh", ".bash"]),
    b("nixfmt", "nixfmt", &[".nix"]),
    b("rustfmt", "rustfmt", &[".rs"]),
    b("pint", "pint", &[".php"]),
    b("ormolu", "ormolu", &[".hs"]),
    b("cljfmt", "cljfmt", &[".clj", ".cljs", ".cljc", ".edn"]),
    b("dfmt", "dfmt", &[".d"]),
];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Entry {
    disabled: Option<bool>,
    command: Option<Vec<String>>,
    environment: Option<HashMap<String, String>>,
    extensions: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
enum Enabled {
    Builtin(&'static str),
    Command(Option<Vec<String>>),
}

#[derive(Debug, Clone)]
struct Info {
    name: String,
    extensions: Vec<String>,
    environment: HashMap<String, String>,
    enabled: Enabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatterStatus {
    pub name: String,
    pub extensions: Vec<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FormatResult {
    /// At least one enabled formatter matched the extension (what `Format.file` returns in the source).
    pub ran: bool,
    /// The file's bytes differ after the run.
    pub changed: bool,
}

#[derive(Debug, Clone, Default)]
pub struct FormatOptions {
    pub directory: PathBuf,
    pub worktree: PathBuf,
    /// `<cache>` (bin dir is `<cache>/bin`, npm packages `<cache>/packages`).
    pub cache_dir: Option<PathBuf>,
    pub experimental_oxfmt: bool,
}

pub struct Formatter {
    opts: FormatOptions,
    formatters: IndexMap<String, Info>,
    commands: tokio::sync::Mutex<HashMap<String, Vec<String>>>,
}

impl Formatter {
    /// `config` is the raw `formatter` config value: absent/false ⇒ nothing, `true` ⇒ built-ins, object ⇒ overrides.
    pub fn new(config: Option<&Value>, opts: FormatOptions) -> Self {
        let mut formatters: IndexMap<String, Info> = IndexMap::new();
        let on = match config {
            None | Some(Value::Null) | Some(Value::Bool(false)) => false,
            Some(_) => true,
        };
        if on {
            for item in BUILTINS {
                formatters.insert(item.name.to_string(), builtin_info(item));
            }
        }
        if let Some(Value::Object(map)) = config {
            let disabled = |key: &str| {
                map.get(key)
                    .and_then(|v| v.get("disabled"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            };
            let python_off = disabled("ruff") || disabled("uv");
            for (name, raw) in map {
                let item: Entry = serde_json::from_value(raw.clone()).unwrap_or_default();
                // Ruff and uv are the same formatter, so disabling either disables both.
                if (name == "ruff" || name == "uv") && python_off {
                    formatters.shift_remove("ruff");
                    formatters.shift_remove("uv");
                    continue;
                }
                if item.disabled == Some(true) {
                    formatters.shift_remove(name);
                    continue;
                }
                let builtin = BUILTINS.iter().find(|x| x.key == name);
                let mut info = builtin.map(builtin_info).unwrap_or(Info {
                    name: name.clone(),
                    extensions: Vec::new(),
                    environment: HashMap::new(),
                    enabled: Enabled::Command(None),
                });
                info.name = name.clone();
                if let Some(extensions) = item.extensions {
                    info.extensions = extensions;
                }
                info.environment
                    .extend(item.environment.unwrap_or_default());
                if builtin.is_none() || item.command.is_some() {
                    info.enabled = Enabled::Command(item.command);
                }
                formatters.insert(name.clone(), info);
            }
        }
        Self {
            opts,
            formatters,
            commands: Default::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.formatters.is_empty()
    }

    async fn command(&self, item: &Info) -> Option<Vec<String>> {
        let mut cache = self.commands.lock().await;
        if let Some(hit) = cache.get(&item.name) {
            return Some(hit.clone());
        }
        let cmd = match &item.enabled {
            Enabled::Command(cmd) => cmd.clone(),
            Enabled::Builtin(key) => self.detect(key).await,
        }?;
        cache.insert(item.name.clone(), cmd.clone());
        Some(cmd)
    }

    /// `GET /formatter`.
    pub async fn status(&self) -> Vec<FormatterStatus> {
        let mut out = Vec::with_capacity(self.formatters.len());
        for item in self.formatters.values() {
            out.push(FormatterStatus {
                name: item.name.clone(),
                extensions: item.extensions.clone(),
                enabled: self.command(item).await.is_some(),
            });
        }
        out
    }

    /// Source semantics: run every enabled formatter matching the extension, in order; `true` if any matched.
    pub async fn file(&self, file: &Path) -> bool {
        let path = file.to_string_lossy();
        let ext = util::extname(&path);
        let mut ran = false;
        for item in self
            .formatters
            .values()
            .filter(|item| item.extensions.iter().any(|e| e == ext))
        {
            let Some(cmd) = self.command(item).await else {
                continue;
            };
            ran = true;
            let replaced: Vec<String> = cmd.iter().map(|x| x.replacen("$FILE", &path, 1)).collect();
            let Some((program, args)) = replaced.split_first() else {
                continue;
            };
            let mut child = util::command(program);
            child
                .args(args)
                .current_dir(&self.opts.directory)
                .envs(&item.environment)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let _ = child.status().await;
        }
        ran
    }

    /// `file` plus whether the bytes on disk changed.
    pub async fn run(&self, file: &Path) -> FormatResult {
        if self.formatters.is_empty() {
            return FormatResult::default();
        }
        let before = std::fs::read(file).ok();
        let ran = self.file(file).await;
        FormatResult {
            ran,
            changed: ran && before != std::fs::read(file).ok(),
        }
    }

    fn which(&self, name: &str) -> Option<String> {
        let bin = self.opts.cache_dir.as_ref().map(|c| c.join("bin"));
        util::which(name, bin.as_deref()).map(|p| p.to_string_lossy().into_owned())
    }

    fn find_up(&self, target: &str) -> Vec<PathBuf> {
        util::find_up(&[target], &self.opts.directory, &self.opts.worktree)
    }

    // Stand-in for `Npm.which`: project-local bin, then the opencode npm cache, then PATH. Never installs.
    fn npm_bin(&self, pkg: &str, bin: &str) -> Option<String> {
        let name = if cfg!(windows) {
            format!("{bin}.cmd")
        } else {
            bin.to_string()
        };
        let local = util::up(
            &[&format!("node_modules/.bin/{name}")],
            &self.opts.directory,
            &self.opts.worktree,
        );
        local
            .or_else(|| util::npm_cached_bin(self.opts.cache_dir.as_deref(), pkg, Some(bin)))
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| self.which(bin))
    }

    fn has_dependency(&self, manifest: &str, sections: &[&str], dep: &str) -> bool {
        self.find_up(manifest).iter().any(|item| {
            let json: Value = std::fs::read_to_string(item)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or(Value::Null);
            sections.iter().any(|section| {
                json.get(section)
                    .and_then(|s| s.get(dep))
                    .is_some_and(truthy)
            })
        })
    }

    fn ruff(&self) -> Option<Vec<String>> {
        self.which("ruff")?;
        let cmd = || Some(strings(&["ruff", "format", "$FILE"]));
        for config in ["pyproject.toml", "ruff.toml", ".ruff.toml"] {
            let found = self.find_up(config);
            let Some(first) = found.first() else { continue };
            if config != "pyproject.toml"
                || std::fs::read_to_string(first)
                    .unwrap_or_default()
                    .contains("[tool.ruff]")
            {
                return cmd();
            }
        }
        for dep in ["requirements.txt", "pyproject.toml", "Pipfile"] {
            let found = self.find_up(dep);
            let Some(first) = found.first() else { continue };
            if std::fs::read_to_string(first)
                .unwrap_or_default()
                .contains("ruff")
            {
                return cmd();
            }
        }
        None
    }

    async fn detect(&self, key: &str) -> Option<Vec<String>> {
        let simple = |bin: &str, args: &[&str]| {
            let found = self.which(bin)?;
            Some(
                std::iter::once(found)
                    .chain(args.iter().map(|s| s.to_string()))
                    .collect::<Vec<_>>(),
            )
        };
        match key {
            "gofmt" => simple("gofmt", &["-w", "$FILE"]),
            "mix" => simple("mix", &["format", "$FILE"]),
            "prettier" => {
                if !self.has_dependency(
                    "package.json",
                    &["dependencies", "devDependencies"],
                    "prettier",
                ) {
                    return None;
                }
                Some(vec![
                    self.npm_bin("prettier", "prettier")?,
                    "--write".into(),
                    "$FILE".into(),
                ])
            }
            "oxfmt" => {
                if !self.opts.experimental_oxfmt
                    || !self.has_dependency(
                        "package.json",
                        &["dependencies", "devDependencies"],
                        "oxfmt",
                    )
                {
                    return None;
                }
                Some(vec![self.npm_bin("oxfmt", "oxfmt")?, "$FILE".into()])
            }
            "biome" => {
                if self.find_up("biome.json").is_empty() && self.find_up("biome.jsonc").is_empty() {
                    return None;
                }
                Some(vec![
                    self.npm_bin("@biomejs/biome", "biome")?,
                    "format".into(),
                    "--write".into(),
                    "$FILE".into(),
                ])
            }
            "zig" => simple("zig", &["fmt", "$FILE"]),
            "clang" => {
                if self.find_up(".clang-format").is_empty() {
                    return None;
                }
                simple("clang-format", &["-i", "$FILE"])
            }
            "ktlint" => simple("ktlint", &["-F", "$FILE"]),
            "ruff" => self.ruff(),
            "rlang" => {
                let air = self.which("air")?;
                let output = util::run(&air, &["--help"], RunOptions::default()).await;
                let text = output.text();
                let first = text.split('\n').next().unwrap_or("");
                (output.code == 0 && first.contains("R language") && first.contains("formatter"))
                    .then(|| vec![air, "format".into(), "$FILE".into()])
            }
            "uvformat" => {
                if self.ruff().is_some() {
                    return None;
                }
                let uv = self.which("uv")?;
                let output = util::run(&uv, &["format", "--help"], RunOptions::default()).await;
                (output.code == 0).then(|| vec![uv, "format".into(), "--".into(), "$FILE".into()])
            }
            "rubocop" => simple("rubocop", &["--autocorrect", "$FILE"]),
            "standardrb" => simple("standardrb", &["--fix", "$FILE"]),
            "htmlbeautifier" => simple("htmlbeautifier", &["$FILE"]),
            "dart" => simple("dart", &["format", "$FILE"]),
            "ocamlformat" => {
                self.which("ocamlformat")?;
                (!self.find_up(".ocamlformat").is_empty())
                    .then(|| strings(&["ocamlformat", "-i", "$FILE"]))
            }
            "terraform" => simple("terraform", &["fmt", "$FILE"]),
            "latexindent" => simple("latexindent", &["-w", "-s", "$FILE"]),
            "gleam" => simple("gleam", &["format", "$FILE"]),
            "shfmt" => simple("shfmt", &["-w", "$FILE"]),
            "nixfmt" => simple("nixfmt", &["$FILE"]),
            "rustfmt" => simple("rustfmt", &["$FILE"]),
            "pint" => self
                .has_dependency("composer.json", &["require", "require-dev"], "laravel/pint")
                .then(|| strings(&["./vendor/bin/pint", "$FILE"])),
            "ormolu" => simple("ormolu", &["-i", "$FILE"]),
            "cljfmt" => simple("cljfmt", &["fix", "--quiet", "$FILE"]),
            "dfmt" => simple("dfmt", &["-i", "$FILE"]),
            _ => None,
        }
    }
}

fn builtin_info(item: &Builtin) -> Info {
    Info {
        name: item.name.to_string(),
        extensions: strings(item.extensions),
        environment: if item.bun {
            HashMap::from([("BUN_BE_BUN".to_string(), "1".to_string())])
        } else {
            HashMap::new()
        },
        enabled: Enabled::Builtin(item.key),
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn opts(dir: &Path) -> FormatOptions {
        FormatOptions {
            directory: dir.to_path_buf(),
            worktree: dir.to_path_buf(),
            ..Default::default()
        }
    }

    #[test]
    fn table_is_complete() {
        assert_eq!(BUILTINS.len(), 26);
        let mut names: Vec<_> = BUILTINS.iter().map(|x| x.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 26);
    }

    #[test]
    fn off_unless_configured() {
        let dir = Path::new(".");
        assert!(Formatter::new(None, opts(dir)).is_empty());
        assert!(Formatter::new(Some(&json!(false)), opts(dir)).is_empty());
        assert_eq!(
            Formatter::new(Some(&json!(true)), opts(dir))
                .formatters
                .len(),
            26
        );
    }

    #[test]
    fn overrides() {
        let dir = Path::new(".");
        let config = json!({
            "uv": {"disabled": true},
            "gofmt": {"disabled": true},
            "clang": {"command": ["x", "$FILE"]},
            "air": {"extensions": [".r"]},
            "prettier": {"environment": {"A": "1"}, "extensions": [".foo"]},
            "mine": {"command": ["fmt", "$FILE"], "extensions": [".txt"]}
        });
        let f = Formatter::new(Some(&config), opts(dir));
        assert!(!f.formatters.contains_key("ruff") && !f.formatters.contains_key("uv"));
        assert!(!f.formatters.contains_key("gofmt"));
        // Export-identifier lookup: `clang` inherits the built-in, the built-in `clang-format` entry stays too.
        assert!(
            f.formatters["clang"]
                .extensions
                .contains(&".ino".to_string())
        );
        assert!(matches!(
            f.formatters["clang"].enabled,
            Enabled::Command(Some(_))
        ));
        assert!(f.formatters.contains_key("clang-format"));
        // `air` is the display name, not the export identifier: no built-in detection is inherited.
        assert!(matches!(
            f.formatters["air"].enabled,
            Enabled::Command(None)
        ));
        let prettier = &f.formatters["prettier"];
        assert_eq!(prettier.extensions, vec![".foo"]);
        assert_eq!(
            prettier.environment.get("BUN_BE_BUN").map(String::as_str),
            Some("1")
        );
        assert_eq!(prettier.environment.get("A").map(String::as_str), Some("1"));
        assert!(matches!(prettier.enabled, Enabled::Builtin("prettier")));
        assert!(matches!(
            f.formatters["mine"].enabled,
            Enabled::Command(Some(_))
        ));
    }
}
