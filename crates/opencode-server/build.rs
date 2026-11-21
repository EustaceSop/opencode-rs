// Embeds the original web UI (`packages/app/dist`) when it has been built; otherwise the server has no UI.
use std::path::{Path, PathBuf};
use std::{env, fs};

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if path.extension().is_none_or(|ext| ext != "map") {
            let relative = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            out.push((relative, path));
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=OPENCODE_WEB_UI_DIR");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dir = env::var_os("OPENCODE_WEB_UI_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../../opencode-ts/packages/app/dist"));
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files = Vec::new();
    if dir.join("index.html").is_file() {
        let root = dir.canonicalize().unwrap();
        collect(&root, &root, &mut files);
        files.sort();
    }
    let mut code = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for (name, path) in &files {
        code.push_str(&format!("    ({name:?}, include_bytes!({:?})),\n", path.to_string_lossy()));
    }
    code.push_str("];\n");
    fs::write(PathBuf::from(env::var("OUT_DIR").unwrap()).join("web_ui.rs"), code).unwrap();
}
