use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::install::{Platform, npm};
use crate::util::{self, RunOptions};

const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "bun.lockb",
    "bun.lock",
    "pnpm-lock.yaml",
    "yarn.lock",
];
const TS_FAMILY: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"];

#[derive(Debug, Clone, Default)]
pub struct Context {
    pub directory: PathBuf,
    pub worktree: PathBuf,
    /// `<cache>`: binaries under `<cache>/bin`, npm packages under `<cache>/packages`.
    pub cache_dir: Option<PathBuf>,
    /// `OPENCODE_EXPERIMENTAL_LSP_TY`.
    pub experimental_ty: bool,
}

impl Context {
    pub fn bin_dir(&self) -> Option<PathBuf> {
        self.cache_dir.as_ref().map(|c| c.join("bin"))
    }
}

// The lookup environment of one resolution; `path` stands in for the process PATH when set.
#[derive(Clone, Copy)]
pub(crate) struct Env<'a> {
    pub ctx: &'a Context,
    pub path: Option<&'a OsStr>,
}

impl Env<'_> {
    pub(crate) fn which(&self, name: &str) -> Option<String> {
        util::which_in(name, self.ctx.bin_dir().as_deref(), self.path)
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn npm(&self, pkg: &str, bin: Option<&str>) -> Option<String> {
        npm::pick(self.ctx.cache_dir.as_deref()?, pkg, bin)
            .map(|p| p.to_string_lossy().into_owned())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Root {
    /// `NearestRoot(include, exclude)`: exclude hit ⇒ no server; no include hit ⇒ instance directory.
    Nearest {
        include: &'static [&'static str],
        exclude: &'static [&'static str],
    },
    /// `StrictNearestRoot`: no include hit ⇒ no server.
    Strict {
        include: &'static [&'static str],
    },
    Directory,
    Gopls,
    Rust,
    Jdtls,
    Kotlin,
    Nixd,
}

const fn nearest(include: &'static [&'static str]) -> Root {
    Root::Nearest {
        include,
        exclude: &[],
    }
}

pub struct ServerDef {
    pub id: &'static str,
    pub extensions: &'static [&'static str],
    pub root: Root,
    /// Where the server is fetched from when it is missing (see `install`).
    pub install: Option<&'static str>,
}

pub const SERVERS: &[ServerDef] = &[
    ServerDef {
        id: "deno",
        extensions: &[".ts", ".tsx", ".js", ".jsx", ".mjs"],
        root: Root::Strict {
            include: &["deno.json", "deno.jsonc"],
        },
        install: None,
    },
    ServerDef {
        id: "typescript",
        extensions: TS_FAMILY,
        root: Root::Nearest {
            include: LOCKFILES,
            exclude: &["deno.json", "deno.jsonc"],
        },
        install: Some("npm typescript-language-server"),
    },
    ServerDef {
        id: "vue",
        extensions: &[".vue"],
        root: nearest(LOCKFILES),
        install: Some("npm @vue/language-server"),
    },
    ServerDef {
        id: "eslint",
        extensions: &[
            ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue",
        ],
        root: nearest(LOCKFILES),
        install: Some(
            "https://github.com/microsoft/vscode-eslint/archive/refs/heads/main.zip + npm install/compile",
        ),
    },
    ServerDef {
        id: "oxlint",
        extensions: &[
            ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue", ".astro",
            ".svelte",
        ],
        root: nearest(&[
            ".oxlintrc.json",
            "package-lock.json",
            "bun.lockb",
            "bun.lock",
            "pnpm-lock.yaml",
            "yarn.lock",
            "package.json",
        ]),
        install: None,
    },
    ServerDef {
        id: "biome",
        extensions: &[
            ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".json", ".jsonc",
            ".vue", ".astro", ".svelte", ".css", ".graphql", ".gql", ".html",
        ],
        root: nearest(&[
            "biome.json",
            "biome.jsonc",
            "package-lock.json",
            "bun.lockb",
            "bun.lock",
            "pnpm-lock.yaml",
            "yarn.lock",
        ]),
        install: Some("npm biome"),
    },
    ServerDef {
        id: "gopls",
        extensions: &[".go"],
        root: Root::Gopls,
        install: Some("go install golang.org/x/tools/gopls@latest"),
    },
    ServerDef {
        id: "ruby-lsp",
        extensions: &[".rb", ".rake", ".gemspec", ".ru"],
        root: nearest(&["Gemfile"]),
        install: Some("gem install rubocop"),
    },
    ServerDef {
        id: "ty",
        extensions: &[".py", ".pyi"],
        root: nearest(&[
            "pyproject.toml",
            "ty.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "Pipfile",
            "pyrightconfig.json",
        ]),
        install: None,
    },
    ServerDef {
        id: "pyright",
        extensions: &[".py", ".pyi"],
        root: nearest(&[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "Pipfile",
            "pyrightconfig.json",
        ]),
        install: Some("npm pyright"),
    },
    ServerDef {
        id: "elixir-ls",
        extensions: &[".ex", ".exs"],
        root: nearest(&["mix.exs", "mix.lock"]),
        install: Some(
            "https://github.com/elixir-lsp/elixir-ls/archive/refs/heads/master.zip + mix release",
        ),
    },
    ServerDef {
        id: "zls",
        extensions: &[".zig", ".zon"],
        root: nearest(&["build.zig"]),
        install: Some("https://api.github.com/repos/zigtools/zls/releases/latest"),
    },
    ServerDef {
        id: "csharp",
        extensions: &[".cs", ".csx"],
        root: nearest(&[".slnx", ".sln", ".csproj", "global.json"]),
        install: Some("dotnet tool install --global roslyn-language-server --prerelease"),
    },
    ServerDef {
        id: "razor",
        extensions: &[".razor", ".cshtml"],
        root: nearest(&[".slnx", ".sln", ".csproj", "global.json"]),
        install: Some("dotnet tool install --global roslyn-language-server --prerelease"),
    },
    ServerDef {
        id: "fsharp",
        extensions: &[".fs", ".fsi", ".fsx", ".fsscript"],
        root: nearest(&[".slnx", ".sln", ".fsproj", "global.json"]),
        install: Some("dotnet tool install fsautocomplete"),
    },
    ServerDef {
        id: "sourcekit-lsp",
        extensions: &[".swift", ".objc", "objcpp"],
        root: nearest(&["Package.swift", "*.xcodeproj", "*.xcworkspace"]),
        install: None,
    },
    ServerDef {
        id: "rust",
        extensions: &[".rs"],
        root: Root::Rust,
        install: None,
    },
    ServerDef {
        id: "clangd",
        extensions: &[
            ".c", ".cpp", ".cc", ".cxx", ".c++", ".h", ".hpp", ".hh", ".hxx", ".h++",
        ],
        root: nearest(&["compile_commands.json", "compile_flags.txt", ".clangd"]),
        install: Some("https://api.github.com/repos/clangd/clangd/releases/latest"),
    },
    ServerDef {
        id: "svelte",
        extensions: &[".svelte"],
        root: nearest(LOCKFILES),
        install: Some("npm svelte-language-server"),
    },
    ServerDef {
        id: "astro",
        extensions: &[".astro"],
        root: nearest(LOCKFILES),
        install: Some("npm @astrojs/language-server"),
    },
    ServerDef {
        id: "jdtls",
        extensions: &[".java"],
        root: Root::Jdtls,
        install: Some(
            "https://www.eclipse.org/downloads/download.php?file=/jdtls/snapshots/jdt-language-server-latest.tar.gz",
        ),
    },
    ServerDef {
        id: "kotlin-ls",
        extensions: &[".kt", ".kts"],
        root: Root::Kotlin,
        install: Some(
            "https://download-cdn.jetbrains.com/kotlin-lsp/<version>/kotlin-lsp-<version>-<os>-<arch>.zip",
        ),
    },
    ServerDef {
        id: "yaml-ls",
        extensions: &[".yaml", ".yml"],
        root: nearest(LOCKFILES),
        install: Some("npm yaml-language-server"),
    },
    ServerDef {
        id: "lua-ls",
        extensions: &[".lua"],
        root: nearest(&[
            ".luarc.json",
            ".luarc.jsonc",
            ".luacheckrc",
            ".stylua.toml",
            "stylua.toml",
            "selene.toml",
            "selene.yml",
        ]),
        install: Some("https://api.github.com/repos/LuaLS/lua-language-server/releases/latest"),
    },
    ServerDef {
        id: "php intelephense",
        extensions: &[".php"],
        root: nearest(&["composer.json", "composer.lock", ".php-version"]),
        install: Some("npm intelephense"),
    },
    ServerDef {
        id: "prisma",
        extensions: &[".prisma"],
        root: Root::Nearest {
            include: &["schema.prisma", "prisma/schema.prisma", "prisma"],
            exclude: &["package.json"],
        },
        install: None,
    },
    ServerDef {
        id: "dart",
        extensions: &[".dart"],
        root: nearest(&["pubspec.yaml", "analysis_options.yaml"]),
        install: None,
    },
    ServerDef {
        id: "ocaml-lsp",
        extensions: &[".ml", ".mli"],
        root: nearest(&["dune-project", "dune-workspace", ".merlin", "opam"]),
        install: None,
    },
    ServerDef {
        id: "bash",
        extensions: &[".sh", ".bash", ".zsh", ".ksh"],
        root: Root::Directory,
        install: Some("npm bash-language-server"),
    },
    ServerDef {
        id: "terraform",
        extensions: &[".tf", ".tfvars"],
        root: nearest(&[".terraform.lock.hcl", "terraform.tfstate", "*.tf"]),
        install: Some("https://api.releases.hashicorp.com/v1/releases/terraform-ls/latest"),
    },
    ServerDef {
        id: "texlab",
        extensions: &[".tex", ".bib"],
        root: nearest(&[".latexmkrc", "latexmkrc", ".texlabroot", "texlabroot"]),
        install: Some("https://api.github.com/repos/latex-lsp/texlab/releases/latest"),
    },
    ServerDef {
        id: "dockerfile",
        extensions: &[".dockerfile", "Dockerfile"],
        root: Root::Directory,
        install: Some("npm dockerfile-language-server-nodejs"),
    },
    ServerDef {
        id: "gleam",
        extensions: &[".gleam"],
        root: nearest(&["gleam.toml"]),
        install: None,
    },
    ServerDef {
        id: "clojure-lsp",
        extensions: &[".clj", ".cljs", ".cljc", ".edn"],
        root: nearest(&[
            "deps.edn",
            "project.clj",
            "shadow-cljs.edn",
            "bb.edn",
            "build.boot",
        ]),
        install: None,
    },
    ServerDef {
        id: "nixd",
        extensions: &[".nix"],
        root: Root::Nixd,
        install: None,
    },
    ServerDef {
        id: "tinymist",
        extensions: &[".typ", ".typc"],
        root: nearest(&["typst.toml"]),
        install: Some("https://api.github.com/repos/Myriad-Dreamin/tinymist/releases/latest"),
    },
    ServerDef {
        id: "haskell-language-server",
        extensions: &[".hs", ".lhs"],
        root: nearest(&["stack.yaml", "cabal.project", "hie.yaml", "*.cabal"]),
        install: None,
    },
    ServerDef {
        id: "julials",
        extensions: &[".jl"],
        root: nearest(&["Project.toml", "Manifest.toml", "*.jl"]),
        install: None,
    },
];

fn parent(file: &Path) -> &Path {
    file.parent().unwrap_or(file)
}

fn nearest_root(
    include: &[&str],
    exclude: &[&str],
    file: &Path,
    ctx: &Context,
    strict: bool,
) -> Option<PathBuf> {
    if !exclude.is_empty() && util::up(exclude, parent(file), &ctx.directory).is_some() {
        return None;
    }
    match util::up(include, parent(file), &ctx.directory) {
        Some(first) => Some(parent(&first).to_path_buf()),
        None if strict => None,
        None => Some(ctx.directory.clone()),
    }
}

fn is_module_of(pom: &str, module_path: &str) -> bool {
    let normalized = module_path.replace('\\', "/");
    let normalized = normalized.strip_suffix('/').unwrap_or(&normalized);
    if normalized.is_empty() {
        return false;
    }
    let mut rest = pom;
    while let Some(start) = rest.find("<modules>") {
        let Some(len) = rest[start..].find("</modules>") else {
            break;
        };
        let mut block = rest[start + "<modules>".len()..start + len].to_string();
        rest = &rest[start + len + "</modules>".len()..];
        while let Some(open) = block.find("<!--") {
            let Some(close) = block[open..].find("-->") else {
                break;
            };
            block.replace_range(open..open + close + 3, "");
        }
        let mut cursor = block.as_str();
        while let Some(open) = cursor.find("<module>") {
            let after = &cursor[open + "<module>".len()..];
            let Some(close) = after.find('<') else { break };
            if after[close..].starts_with("</module>") {
                let decl = after[..close].trim().replace('\\', "/");
                let decl = decl.strip_prefix("./").unwrap_or(&decl);
                if decl.strip_suffix('/').unwrap_or(decl) == normalized {
                    return true;
                }
            }
            cursor = &after[close..];
        }
    }
    false
}

impl Root {
    pub fn resolve(&self, file: &Path, ctx: &Context) -> Option<PathBuf> {
        match self {
            Root::Nearest { include, exclude } => nearest_root(include, exclude, file, ctx, false),
            Root::Strict { include } => nearest_root(include, &[], file, ctx, true),
            Root::Directory => Some(ctx.directory.clone()),
            Root::Gopls => nearest_root(&["go.work"], &[], file, ctx, false)
                .or_else(|| nearest_root(&["go.mod", "go.sum"], &[], file, ctx, false)),
            Root::Kotlin => nearest_root(
                &["settings.gradle.kts", "settings.gradle"],
                &[],
                file,
                ctx,
                false,
            )
            .or_else(|| nearest_root(&["gradlew", "gradlew.bat"], &[], file, ctx, false))
            .or_else(|| nearest_root(&["build.gradle.kts", "build.gradle"], &[], file, ctx, false))
            .or_else(|| nearest_root(&["pom.xml"], &[], file, ctx, false)),
            Root::Nixd => {
                let flake = nearest_root(&["flake.nix"], &[], file, ctx, false);
                if let Some(root) = flake.filter(|root| *root != ctx.directory) {
                    return Some(root);
                }
                if !ctx.worktree.as_os_str().is_empty() && ctx.worktree != ctx.directory {
                    return Some(ctx.worktree.clone());
                }
                Some(ctx.directory.clone())
            }
            Root::Rust => {
                let crate_root =
                    nearest_root(&["Cargo.toml", "Cargo.lock"], &[], file, ctx, false)?;
                let mut current = crate_root.clone();
                while let Some(up) = current.parent().map(Path::to_path_buf) {
                    if std::fs::read_to_string(current.join("Cargo.toml"))
                        .is_ok_and(|t| t.contains("[workspace]"))
                    {
                        return Some(current);
                    }
                    current = up;
                    // The source compares path strings here.
                    if !current
                        .to_string_lossy()
                        .starts_with(&*ctx.worktree.to_string_lossy())
                    {
                        break;
                    }
                }
                Some(crate_root)
            }
            Root::Jdtls => {
                let settings: &[&str] = &["settings.gradle", "settings.gradle.kts"];
                let wrapper = nearest_root(&["gradlew", "gradlew.bat"], settings, file, ctx, true);
                let settings_root = nearest_root(settings, &[], file, ctx, true);
                if let Some(root) = wrapper.or(settings_root) {
                    return Some(root);
                }
                if let Some(root) =
                    nearest_root(&["build.gradle", "build.gradle.kts"], &[], file, ctx, true)
                {
                    return Some(root);
                }
                let poms = util::find_up(&["pom.xml"], parent(file), &ctx.directory);
                if let Some(first) = poms.first() {
                    let mut root = parent(first).to_path_buf();
                    for pom in &poms[1..] {
                        let dir = parent(pom);
                        let rel = root.strip_prefix(dir).map(util::slash).unwrap_or_default();
                        match std::fs::read_to_string(pom) {
                            Ok(content) if is_module_of(&content, &rel) => root = dir.to_path_buf(),
                            _ => break,
                        }
                    }
                    return Some(root);
                }
                nearest_root(&[".project", ".classpath"], &[], file, ctx, true)
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Launch {
    pub command: Vec<String>,
    pub env: HashMap<String, String>,
    pub initialization: Option<Value>,
}

fn launch(bin: String, args: &[&str]) -> Launch {
    Launch {
        command: std::iter::once(bin)
            .chain(args.iter().map(|s| s.to_string()))
            .collect(),
        ..Default::default()
    }
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

// `<bin>/lua-language-server-<arch>-<platform>/bin/lua-language-server[.exe]`.
pub(crate) fn lua_ls_bin(bin_dir: &Path, platform: Platform) -> PathBuf {
    bin_dir
        .join(format!(
            "lua-language-server-{}-{}",
            platform.arch, platform.os
        ))
        .join("bin")
        .join(platform.exe("lua-language-server"))
}

fn python_path(root: &Path) -> Option<String> {
    let venvs = [
        std::env::var_os("VIRTUAL_ENV").map(PathBuf::from),
        Some(root.join(".venv")),
        Some(root.join("venv")),
    ];
    venvs
        .into_iter()
        .flatten()
        .map(|venv| {
            if cfg!(windows) {
                venv.join("Scripts").join("python.exe")
            } else {
                venv.join("bin").join("python")
            }
        })
        .find(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
}

fn home() -> Option<PathBuf> {
    std::env::home_dir()
}

pub(crate) fn roslyn(env: Env<'_>) -> Option<String> {
    if let Some(found) = env.which("roslyn-language-server") {
        return Some(found);
    }
    let base = std::env::var_os("DOTNET_CLI_HOME")
        .map(PathBuf::from)
        .or_else(home)?;
    let name = if cfg!(windows) {
        "roslyn-language-server.cmd"
    } else {
        "roslyn-language-server"
    };
    let bin = base.join(".dotnet").join("tools").join(name);
    bin.exists().then(|| bin.to_string_lossy().into_owned())
}

pub(crate) fn razor_extension() -> Option<(PathBuf, PathBuf, PathBuf)> {
    let mut roots: Vec<PathBuf> = std::env::var_os("VSCODE_EXTENSIONS")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    if let Some(home) = home() {
        for name in [
            ".vscode",
            ".vscode-insiders",
            ".vscode-server",
            ".vscode-server-insiders",
        ] {
            roots.push(home.join(name).join("extensions"));
        }
    }
    for root in roots {
        let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.path().is_dir()
                    && e.file_name()
                        .to_string_lossy()
                        .starts_with("ms-dotnettools.csharp-")
            })
            .map(|e| {
                let modified = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                (modified, e.path().join(".razorExtension"))
            })
            .collect();
        candidates.sort_by_key(|a| std::cmp::Reverse(a.0));
        for (_, entry) in candidates {
            let compiler = entry.join("Microsoft.CodeAnalysis.Razor.Compiler.dll");
            let targets = entry
                .join("Targets")
                .join("Microsoft.NET.Sdk.Razor.DesignTime.targets");
            let extension = entry.join("Microsoft.VisualStudioCode.RazorExtension.dll");
            if compiler.exists() && targets.exists() && extension.exists() {
                return Some((compiler, targets, extension));
            }
        }
    }
    None
}

pub(crate) fn java_major(stderr: &str) -> Option<u32> {
    // /"(\d+)\.\d+\.\d+"/
    for (start, _) in stderr.match_indices('"') {
        let rest = &stderr[start + 1..];
        let Some(end) = rest.find('"') else { continue };
        let parts: Vec<&str> = rest[..end].split('.').collect();
        if parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        {
            return parts[0].parse().ok();
        }
    }
    None
}

async fn oxlint(root: &Path, env: Env<'_>) -> Option<Launch> {
    let ctx = env.ctx;
    let ext = if cfg!(windows) { ".cmd" } else { "" };
    let resolve = |name: &str| {
        let target = format!("node_modules/.bin/{name}{ext}");
        let local = root.join(&target);
        if local.exists() {
            return Some(local.to_string_lossy().into_owned());
        }
        util::up(&[&target], root, &ctx.worktree).map(|p| p.to_string_lossy().into_owned())
    };
    if let Some(lint) = resolve("oxlint").or_else(|| env.which("oxlint")) {
        let help = util::run(&lint, &["--help"], RunOptions::default()).await;
        if help.text().contains("--lsp") {
            return Some(launch(lint, &["--lsp"]));
        }
    }
    let server = resolve("oxc_language_server").or_else(|| env.which("oxc_language_server"))?;
    Some(launch(server, &[]))
}

async fn jdtls(env: Env<'_>) -> Option<Launch> {
    let ctx = env.ctx;
    let java = env.which("java")?;
    let version = util::run("java", &["-version"], RunOptions::default()).await;
    if java_major(&version.stderr)? < 21 {
        return None;
    }
    let dist = ctx.bin_dir()?.join("jdtls");
    let plugins = dist.join("plugins");
    let jar = std::fs::read_dir(&plugins)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("org.eclipse.equinox.launcher_") && name.ends_with(".jar"))?;
    let config = dist.join(match std::env::consts::OS {
        "macos" => "config_mac",
        "windows" => "config_win",
        _ => "config_linux",
    });
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let data = std::env::temp_dir().join(format!(
        "opencode-jdtls-data{:x}{stamp:x}",
        std::process::id()
    ));
    std::fs::create_dir_all(&data).ok()?;
    Some(Launch {
        command: vec![
            java,
            "-jar".into(),
            plugins.join(jar).to_string_lossy().into_owned(),
            "-configuration".into(),
            config.to_string_lossy().into_owned(),
            "-data".into(),
            data.to_string_lossy().into_owned(),
            "-Declipse.application=org.eclipse.jdt.ls.core.id1".into(),
            "-Dosgi.bundles.defaultStartLevel=4".into(),
            "-Declipse.product=org.eclipse.jdt.ls.core.product".into(),
            "-Dlog.level=ALL".into(),
            "--add-modules=ALL-SYSTEM".into(),
            "--add-opens java.base/java.util=ALL-UNNAMED".into(),
            "--add-opens java.base/java.lang=ALL-UNNAMED".into(),
        ],
        ..Default::default()
    })
}

/// Resolve the built-in server's command for `root`; `None` when it is not installed (nothing is downloaded).
pub async fn resolve(id: &str, root: &Path, ctx: &Context) -> Option<Launch> {
    resolve_with(id, root, Env { ctx, path: None }).await
}

pub(crate) async fn resolve_with(id: &str, root: &Path, env: Env<'_>) -> Option<Launch> {
    let ctx = env.ctx;
    let path_or_npm = |bin: &str, pkg: &str| env.which(bin).or_else(|| env.npm(pkg, None));
    let tsserver = || util::resolve_module("typescript/lib/tsserver.js", &ctx.directory);
    let with_init = |mut l: Launch, init: Value| {
        l.initialization = Some(init);
        l
    };
    match id {
        "deno" => Some(launch(env.which("deno")?, &["lsp"])),
        "typescript" => {
            let tsserver = tsserver()?;
            let bin = env
                .npm("typescript-language-server", None)
                .or_else(|| env.which("typescript-language-server"))?;
            Some(with_init(
                launch(bin, &["--stdio"]),
                json!({ "tsserver": { "path": tsserver.to_string_lossy() } }),
            ))
        }
        "vue" => Some(with_init(
            launch(
                path_or_npm("vue-language-server", "@vue/language-server")?,
                &["--stdio"],
            ),
            json!({}),
        )),
        "eslint" => {
            util::resolve_module("eslint", &ctx.directory)?;
            let server = ctx
                .bin_dir()?
                .join("vscode-eslint")
                .join("server")
                .join("out")
                .join("eslintServer.js");
            server
                .exists()
                .then(|| launch("node".into(), &[&server.to_string_lossy(), "--stdio"]))
        }
        "oxlint" => oxlint(root, env).await,
        "biome" => {
            let shim = if cfg!(windows) { "biome.cmd" } else { "biome" };
            let local = root.join("node_modules").join(".bin").join(shim);
            let bin = if local.exists() {
                Some(local.to_string_lossy().into_owned())
            } else {
                env.which("biome").or_else(|| {
                    util::resolve_module("biome", root)?;
                    env.npm("biome", None)
                })
            };
            Some(launch(bin?, &["lsp-proxy", "--stdio"]))
        }
        "gopls" => Some(launch(env.which("gopls")?, &[])),
        "ruby-lsp" => Some(launch(env.which("rubocop")?, &["--lsp"])),
        "ty" => {
            if !ctx.experimental_ty {
                return None;
            }
            let bin = env.which("ty").or_else(|| {
                let venvs = [
                    std::env::var_os("VIRTUAL_ENV").map(PathBuf::from),
                    Some(root.join(".venv")),
                    Some(root.join("venv")),
                ];
                venvs
                    .into_iter()
                    .flatten()
                    .map(|v| {
                        if cfg!(windows) {
                            v.join("Scripts").join("ty.exe")
                        } else {
                            v.join("bin").join("ty")
                        }
                    })
                    .find(|p| p.exists())
                    .map(|p| p.to_string_lossy().into_owned())
            })?;
            let init = python_path(root).map_or_else(|| json!({}), |p| json!({ "pythonPath": p }));
            Some(with_init(launch(bin, &["server"]), init))
        }
        "pyright" => {
            let bin = env
                .which("pyright-langserver")
                .or_else(|| env.npm("pyright", Some("pyright-langserver")))?;
            let init = python_path(root).map_or_else(|| json!({}), |p| json!({ "pythonPath": p }));
            Some(with_init(launch(bin, &["--stdio"]), init))
        }
        "elixir-ls" => {
            let bin = env.which("elixir-ls").or_else(|| {
                let script = if cfg!(windows) {
                    "language_server.bat"
                } else {
                    "language_server.sh"
                };
                let path = ctx
                    .bin_dir()?
                    .join("elixir-ls-master")
                    .join("release")
                    .join(script);
                path.exists().then(|| path.to_string_lossy().into_owned())
            })?;
            Some(launch(bin, &[]))
        }
        "zls" => Some(launch(env.which("zls")?, &[])),
        "csharp" => Some(launch(roslyn(env)?, &["--stdio", "--autoLoadProjects"])),
        "razor" => {
            let bin = roslyn(env)?;
            let (compiler, targets, extension) = razor_extension()?;
            Some(launch(
                bin,
                &[
                    "--stdio",
                    "--autoLoadProjects",
                    &format!("--razorSourceGenerator={}", compiler.display()),
                    &format!("--razorDesignTimePath={}", targets.display()),
                    "--extension",
                    &extension.to_string_lossy(),
                ],
            ))
        }
        "fsharp" => Some(launch(env.which("fsautocomplete")?, &[])),
        "sourcekit-lsp" => {
            if let Some(bin) = env.which("sourcekit-lsp") {
                return Some(launch(bin, &[]));
            }
            env.which("xcrun")?;
            let found =
                util::run("xcrun", &["--find", "sourcekit-lsp"], RunOptions::default()).await;
            (found.code == 0).then(|| launch(found.text().trim().to_string(), &[]))
        }
        "rust" => Some(launch(env.which("rust-analyzer")?, &[])),
        "clangd" => {
            let args = ["--background-index", "--clang-tidy"];
            if let Some(bin) = env.which("clangd") {
                return Some(launch(bin, &args));
            }
            let bin_dir = ctx.bin_dir()?;
            std::fs::read_dir(&bin_dir)
                .ok()?
                .flatten()
                .filter(|e| {
                    e.path().is_dir() && e.file_name().to_string_lossy().starts_with("clangd_")
                })
                .map(|e| e.path().join("bin").join(exe("clangd")))
                .find(|p| p.exists())
                .map(|p| launch(p.to_string_lossy().into_owned(), &args))
        }
        "svelte" => Some(with_init(
            launch(
                path_or_npm("svelteserver", "svelte-language-server")?,
                &["--stdio"],
            ),
            json!({}),
        )),
        "astro" => {
            let tsserver = tsserver()?;
            let tsdk = parent(&tsserver).to_string_lossy().into_owned();
            let bin = path_or_npm("astro-ls", "@astrojs/language-server")?;
            Some(with_init(
                launch(bin, &["--stdio"]),
                json!({ "typescript": { "tsdk": tsdk } }),
            ))
        }
        "jdtls" => jdtls(env).await,
        "kotlin-ls" => {
            let script = if cfg!(windows) {
                "kotlin-lsp.cmd"
            } else {
                "kotlin-lsp.sh"
            };
            let path = ctx.bin_dir()?.join("kotlin-ls").join(script);
            path.exists()
                .then(|| launch(path.to_string_lossy().into_owned(), &["--stdio"]))
        }
        "yaml-ls" => Some(launch(
            path_or_npm("yaml-language-server", "yaml-language-server")?,
            &["--stdio"],
        )),
        "lua-ls" => {
            let bin = env.which("lua-language-server").or_else(|| {
                let path = lua_ls_bin(&ctx.bin_dir()?, Platform::current());
                path.exists().then(|| path.to_string_lossy().into_owned())
            })?;
            Some(launch(bin, &[]))
        }
        "php intelephense" => Some(with_init(
            launch(path_or_npm("intelephense", "intelephense")?, &["--stdio"]),
            json!({ "telemetry": { "enabled": false } }),
        )),
        "prisma" => Some(launch(env.which("prisma")?, &["language-server"])),
        "dart" => Some(launch(env.which("dart")?, &["language-server", "--lsp"])),
        "ocaml-lsp" => Some(launch(env.which("ocamllsp")?, &[])),
        "bash" => Some(launch(
            path_or_npm("bash-language-server", "bash-language-server")?,
            &["start"],
        )),
        "terraform" => Some(with_init(
            launch(env.which("terraform-ls")?, &["serve"]),
            json!({ "experimentalFeatures": { "prefillRequiredFields": true, "validateOnSave": true } }),
        )),
        "texlab" => Some(launch(env.which("texlab")?, &[])),
        "dockerfile" => Some(launch(
            path_or_npm("docker-langserver", "dockerfile-language-server-nodejs")?,
            &["--stdio"],
        )),
        "gleam" => Some(launch(env.which("gleam")?, &["lsp"])),
        "clojure-lsp" => Some(launch(env.which("clojure-lsp")?, &["listen"])),
        "nixd" => Some(launch(env.which("nixd")?, &[])),
        "tinymist" => Some(launch(env.which("tinymist")?, &[])),
        "haskell-language-server" => Some(launch(
            env.which("haskell-language-server-wrapper")?,
            &["--lsp"],
        )),
        "julials" => Some(launch(
            env.which("julia")?,
            &[
                "--startup-file=no",
                "--history-file=no",
                "-e",
                "using LanguageServer; runserver()",
            ],
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `builtinServerIds` in core/src/v1/config/lsp.ts.
    const IDS: [&str; 38] = [
        "deno",
        "typescript",
        "vue",
        "eslint",
        "oxlint",
        "biome",
        "gopls",
        "ruby-lsp",
        "ty",
        "pyright",
        "elixir-ls",
        "zls",
        "csharp",
        "razor",
        "fsharp",
        "sourcekit-lsp",
        "rust",
        "clangd",
        "svelte",
        "astro",
        "jdtls",
        "kotlin-ls",
        "yaml-ls",
        "lua-ls",
        "php intelephense",
        "prisma",
        "dart",
        "ocaml-lsp",
        "bash",
        "terraform",
        "texlab",
        "dockerfile",
        "gleam",
        "clojure-lsp",
        "nixd",
        "tinymist",
        "haskell-language-server",
        "julials",
    ];

    fn ctx(dir: &Path) -> Context {
        Context {
            directory: dir.to_path_buf(),
            worktree: dir.to_path_buf(),
            ..Default::default()
        }
    }

    #[test]
    fn registry_matches_builtin_ids() {
        let ids: Vec<&str> = SERVERS.iter().map(|s| s.id).collect();
        assert_eq!(ids, IDS);
        assert_eq!(SERVERS.iter().filter(|s| s.install.is_some()).count(), 25);
    }

    #[test]
    fn nearest_and_strict_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("pkg/src")).unwrap();
        std::fs::write(dir.join("pkg/yarn.lock"), "").unwrap();
        let file = dir.join("pkg/src/a.ts");
        let c = ctx(dir);
        let ts = SERVERS.iter().find(|s| s.id == "typescript").unwrap();
        let deno = SERVERS.iter().find(|s| s.id == "deno").unwrap();
        assert_eq!(ts.root.resolve(&file, &c), Some(dir.join("pkg")));
        assert_eq!(deno.root.resolve(&file, &c), None);
        assert_eq!(
            ts.root.resolve(&dir.join("other.ts"), &c),
            Some(dir.to_path_buf())
        );
        std::fs::write(dir.join("deno.json"), "{}").unwrap();
        assert_eq!(ts.root.resolve(&file, &c), None);
        assert_eq!(deno.root.resolve(&file, &c), Some(dir.to_path_buf()));
    }

    #[test]
    fn rust_root_climbs_to_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("crates/a/src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        std::fs::write(dir.join("crates/a/Cargo.toml"), "[package]\n").unwrap();
        let file = dir.join("crates/a/src/lib.rs");
        assert_eq!(
            Root::Rust.resolve(&file, &ctx(dir)),
            Some(dir.to_path_buf())
        );
        std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(
            Root::Rust.resolve(&file, &ctx(dir)),
            Some(dir.join("crates").join("a"))
        );
    }

    #[test]
    fn jdtls_root_follows_maven_modules() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("app/src")).unwrap();
        std::fs::write(
            dir.join("pom.xml"),
            "<modules><!-- <module>x</module> --><module> ./app/ </module></modules>",
        )
        .unwrap();
        std::fs::write(dir.join("app/pom.xml"), "<project/>").unwrap();
        let file = dir.join("app/src/A.java");
        assert_eq!(
            Root::Jdtls.resolve(&file, &ctx(dir)),
            Some(dir.to_path_buf())
        );
        std::fs::write(
            dir.join("pom.xml"),
            "<modules><module>other</module></modules>",
        )
        .unwrap();
        assert_eq!(Root::Jdtls.resolve(&file, &ctx(dir)), Some(dir.join("app")));
        assert!(!is_module_of(
            "<modules><!-- <module>app</module> --></modules>",
            "app"
        ));
    }

    #[test]
    fn java_version_parse() {
        assert_eq!(
            java_major("openjdk version \"21.0.2\" 2024-01-16"),
            Some(21)
        );
        assert_eq!(java_major("java version \"1.8.0_292\""), None);
        assert_eq!(java_major("nothing"), None);
    }

    #[tokio::test]
    async fn missing_servers_are_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(tmp.path());
        // `ty` is flag-gated, so it never resolves here whatever is installed.
        assert_eq!(resolve("ty", tmp.path(), &c).await, None);
        assert_eq!(resolve("no-such-server", tmp.path(), &c).await, None);
    }
}
