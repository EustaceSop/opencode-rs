//! Stand-in for `bun`, `npm` and `go` in the install tests: copy this binary under the tool's name.
//!
//! `fake_tool.json` next to the executable configures it: `lsp` (server binary to "install"), `log`
//! (one JSON line per invocation), `delay_ms`, `fail` (tool names that exit 1) and `bins` (package → bin names).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn place(lsp: &Path, dir: &Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::copy(lsp, dir.join(exe(name))).unwrap();
}

fn main() {
    let me = std::env::current_exe().unwrap();
    let tool = me.file_stem().unwrap().to_string_lossy().to_lowercase();
    let config: Value = serde_json::from_str(
        &std::fs::read_to_string(me.with_file_name("fake_tool.json")).unwrap(),
    )
    .unwrap();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cwd = std::env::current_dir().unwrap();
    let text = |key: &str| config.get(key).and_then(Value::as_str).map(PathBuf::from);

    if let Some(log) = text("log") {
        let line = json!({
            "tool": tool,
            "args": args,
            "cwd": cwd,
            "gobin": std::env::var("GOBIN").ok(),
        });
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .unwrap();
        writeln!(file, "{line}").unwrap();
    }
    if let Some(delay) = config.get("delay_ms").and_then(Value::as_u64) {
        std::thread::sleep(std::time::Duration::from_millis(delay));
    }
    let fails = config.get("fail").and_then(Value::as_array);
    if fails.is_some_and(|list| list.iter().any(|item| item.as_str() == Some(tool.as_str()))) {
        eprintln!("{tool}: simulated failure");
        std::process::exit(1);
    }

    let lsp = text("lsp").unwrap();
    let verb = args.first().map(String::as_str);
    let package = args.get(1).filter(|arg| !arg.starts_with('-'));
    match (tool.as_str(), verb, package) {
        ("bun", Some("add"), Some(pkg)) | ("npm", Some("install"), Some(pkg)) => {
            let modules = cwd.join("node_modules");
            let bins: Vec<String> = config
                .pointer(&format!("/bins/{}", pkg.replace('/', "~1")))
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_else(|| vec![pkg.clone()]);
            let package_dir = pkg
                .split('/')
                .fold(modules.clone(), |path, part| path.join(part));
            std::fs::create_dir_all(&package_dir).unwrap();
            let entries: serde_json::Map<String, Value> = bins
                .iter()
                .map(|bin| (bin.clone(), json!("cli.js")))
                .collect();
            let manifest = json!({ "name": pkg, "bin": entries });
            std::fs::write(package_dir.join("package.json"), manifest.to_string()).unwrap();
            for bin in &bins {
                place(&lsp, &modules.join(".bin"), bin);
            }
        }
        ("go", Some("install"), _) => {
            place(&lsp, Path::new(&std::env::var("GOBIN").unwrap()), "gopls");
        }
        _ => {}
    }
}
