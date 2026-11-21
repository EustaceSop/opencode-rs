// `opencode uninstall` (`opencode/src/cli/cmd/uninstall.ts`), narrowed to what the Rust build owns.
//
// DEVIATION: the original removes the whole data, cache, config and state directories. Those are shared with the
// TypeScript build (auth.json, config, skills, plugins, `opencode.db`), so this build removes only its own files:
// `<data>/opencode-rs.db*`, `<data>/snapshot-rs/` and `<state>/desktop-rs.json`. It never edits shell rc files and
// never calls a package manager.
use crate::prompt;
use crate::stats::to_fixed;
use crate::ui::{self, DIM};
use anyhow::Result;
use opencode_core::paths::Paths;
use std::path::{Path, PathBuf};

pub struct Args {
    pub keep_config: bool,
    pub keep_data: bool,
    pub dry_run: bool,
    pub force: bool,
}

pub struct Target {
    pub path: PathBuf,
    pub label: &'static str,
    // The flag that keeps it, as `--keep-<name>`.
    pub flag: &'static str,
    pub keep: bool,
}

// `curl` when the binary sits where the install script puts it, else `unknown`; package managers are not probed
// because they ship the TypeScript build.
pub fn method(exe: &Path) -> &'static str {
    let text = exe.to_string_lossy().replace('\\', "/");
    if text.contains(".opencode/bin") || text.contains(".local/bin") { "curl" } else { "unknown" }
}

pub fn targets(paths: &Paths, args: &Args) -> Vec<Target> {
    let mut out = Vec::new();
    let database = opencode_storage::DATABASE_FILE;
    let mut databases: Vec<PathBuf> = std::fs::read_dir(&paths.data)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with(database)))
        .collect();
    databases.sort();
    for path in databases {
        out.push(Target { path, label: "Database", flag: "data", keep: args.keep_data });
    }
    out.push(Target { path: paths.data.join("snapshot-rs"), label: "Snapshots", flag: "data", keep: args.keep_data });
    out.push(Target { path: paths.state.join("desktop-rs.json"), label: "Desktop state", flag: "config", keep: args.keep_config });
    out
}

fn size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    std::fs::read_dir(path).into_iter().flatten().flatten().map(|entry| size(&entry.path())).sum()
}

fn format_size(bytes: u64) -> String {
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", to_fixed(b / 1024.0, 1))
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{} MB", to_fixed(b / (1024.0 * 1024.0), 1))
    } else {
        format!("{} GB", to_fixed(b / (1024.0 * 1024.0 * 1024.0), 1))
    }
}

pub fn run(paths: &Paths, args: Args) -> Result<i32> {
    ui::empty();
    ui::println(&ui::logo("  "));
    ui::empty();
    ui::intro("Uninstall OpenCode");
    let exe = std::env::current_exe().unwrap_or_default();
    let method = method(&exe);
    ui::info(&format!("Installation method: {method}"));
    let targets = targets(paths, &args);

    ui::message("The following will be removed:");
    for target in targets.iter().filter(|target| target.path.exists()) {
        let status = if target.keep { format!("{DIM}(keeping)") } else { String::new() };
        let prefix = if target.keep { "○" } else { "✓" };
        ui::info(&format!("  {prefix} {}: {} {DIM}({}){status}", target.label, ui::shorten(&target.path, &paths.home), format_size(size(&target.path))));
    }
    if method == "curl" {
        ui::info(&format!("  ✓ Binary: {}", ui::shorten(&exe, &paths.home)));
    }
    ui::info("Shared with the TypeScript build and left in place: auth.json, config, cache, opencode.db");

    if !args.force && !args.dry_run && !prompt::confirm("Are you sure you want to uninstall?", false).unwrap_or(false) {
        ui::outro("Cancelled");
        return Ok(0);
    }
    if args.dry_run {
        ui::warn("Dry run - no changes made");
        ui::outro("Done");
        return Ok(0);
    }

    let mut errors = Vec::new();
    for target in &targets {
        if target.keep {
            ui::step(&format!("Skipping {} (--keep-{})", target.label, target.flag));
            continue;
        }
        if !target.path.exists() {
            continue;
        }
        let removed = if target.path.is_dir() { std::fs::remove_dir_all(&target.path) } else { std::fs::remove_file(&target.path) };
        match removed {
            Ok(()) => ui::done(&format!("Removed {}", target.label), true),
            Err(error) => {
                ui::done(&format!("Failed to remove {}", target.label), false);
                errors.push(format!("{}: {error}", target.label));
            }
        }
    }
    if method == "curl" {
        ui::empty();
        ui::message("To finish removing the binary, run:");
        ui::info(&format!("  rm \"{}\"", exe.display()));
    }
    if !errors.is_empty() {
        ui::empty();
        ui::warn("Some operations failed:");
        for error in &errors {
            ui::failure(&format!("  {error}"));
        }
    }
    ui::empty();
    ui::success("Thank you for using OpenCode!");
    ui::outro("Done");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_and_sizes() {
        assert_eq!(method(Path::new("/home/u/.opencode/bin/opencode")), "curl");
        assert_eq!(method(Path::new(r"C:\Users\u\.local\bin\opencode.exe")), "curl");
        assert_eq!(method(Path::new("/usr/lib/node_modules/opencode-ai/bin/opencode")), "unknown");
        assert_eq!((format_size(0), format_size(1023), format_size(1536), format_size(5 * 1024 * 1024)), ("0 B".into(), "1023 B".into(), "1.5 KB".into(), "5.0 MB".into()));
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn targets_never_name_shared_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = |name: &str| root.path().join(name).join("opencode");
        let paths = Paths { home: root.path().to_path_buf(), data: dir("data"), cache: dir("cache"), config: dir("config"), state: dir("state"), tmp: dir("tmp") };
        std::fs::create_dir_all(&paths.data).unwrap();
        for name in ["opencode-rs.db", "opencode-rs.db-shm", "opencode-rs.db-wal", "opencode.db", "opencode.db-wal", "auth.json", "mcp-auth.json"] {
            std::fs::write(paths.data.join(name), "x").unwrap();
        }
        let args = Args { keep_config: false, keep_data: true, dry_run: false, force: true };
        let found = targets(&paths, &args);
        let names: Vec<(String, bool)> = found.iter().map(|target| (target.path.strip_prefix(root.path()).unwrap().to_string_lossy().replace('\\', "/"), target.keep)).collect();
        let expected = [
            ("data/opencode/opencode-rs.db", true),
            ("data/opencode/opencode-rs.db-shm", true),
            ("data/opencode/opencode-rs.db-wal", true),
            ("data/opencode/snapshot-rs", true),
            ("state/opencode/desktop-rs.json", false),
        ];
        assert_eq!(names, expected.map(|(path, keep)| (path.to_string(), keep)));
        assert_eq!(size(&paths.data), 7);
    }
}
