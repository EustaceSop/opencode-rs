// Plugin spec provenance while config layers merge (`opencode/src/config/plugin.ts`).
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

const INDEX_FILES: [&str; 5] = ["index.ts", "index.tsx", "index.js", "index.mjs", "index.cjs"];

fn windows_absolute(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn absolute(raw: &str) -> bool {
    Path::new(raw).is_absolute() || raw.starts_with('/') || windows_absolute(raw)
}

// `isPathPluginSpec`
pub fn is_path_spec(spec: &str) -> bool {
    spec.starts_with("file://") || spec.starts_with('.') || absolute(spec)
}

// `pluginSpecifier`
pub fn specifier(plugin: &Value) -> Option<&str> {
    match plugin {
        Value::String(spec) => Some(spec),
        Value::Array(pair) => pair.first().and_then(Value::as_str),
        _ => None,
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

// `pathToFileURL(path).href`
pub fn file_url(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !text.starts_with('/') {
        out.push('/');
    }
    for byte in text.bytes() {
        match byte {
            0..=0x20 | 0x7f.. | b'"' | b'#' | b'%' | b'<' | b'>' | b'?' | b'^' | b'`' | b'{' | b'}' => out.push_str(&format!("%{byte:02X}")),
            other => out.push(other as char),
        }
    }
    out
}

fn url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%').then(|| rest.get(i + 1..i + 3)).flatten().and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(byte) => {
                decoded.push(byte);
                i += 3;
            }
            None => {
                decoded.push(bytes[i]);
                i += 1;
            }
        }
    }
    let text = String::from_utf8_lossy(&decoded).into_owned();
    match text.strip_prefix('/').filter(|p| windows_absolute(p)) {
        Some(windows) => Some(PathBuf::from(windows.replace('/', "\\"))),
        None => Some(PathBuf::from(text)),
    }
}

// `resolvePathPluginTarget`, falling back to the plain file URL.
fn target(url: String) -> String {
    let Some(file) = url_path(&url) else { return url };
    if !file.is_dir() {
        return url;
    }
    if file.join("package.json").exists() {
        return file_url(&file);
    }
    match INDEX_FILES.iter().map(|name| file.join(name)).find(|index| index.exists()) {
        Some(index) => file_url(&index),
        None => url,
    }
}

// `resolvePluginSpec`: path-like specs become `file://` URLs relative to the config file that declared them.
pub fn resolve(plugin: &mut Value, config_file: &Path) {
    let Some(spec) = specifier(plugin).filter(|spec| is_path_spec(spec)) else { return };
    let base = config_file.parent().unwrap_or(Path::new("."));
    let url = if spec.starts_with("file://") {
        spec.to_string()
    } else if absolute(spec) {
        file_url(Path::new(spec))
    } else {
        file_url(&normalize(&base.join(spec)))
    };
    let resolved = Value::String(target(url));
    match plugin {
        Value::Array(pair) if !pair.is_empty() => pair[0] = resolved,
        other => *other = resolved,
    }
}

fn name_part(part: &str) -> bool {
    !part.is_empty() && !part.starts_with('.') && part.chars().all(|c| c.is_ascii_alphanumeric() || "-_.~".contains(c))
}

// `parsePluginSpecifier(spec).pkg` for registry specs; anything else is its own identity.
fn package(spec: &str) -> &str {
    let at = match spec.strip_prefix('@') {
        Some(rest) => rest.find('@').map(|i| i + 1),
        None => spec.find('@'),
    };
    let name = at.map_or(spec, |i| &spec[..i]);
    let valid = match name.strip_prefix('@') {
        Some(scoped) => scoped.split_once('/').is_some_and(|(scope, pkg)| name_part(scope) && name_part(pkg)),
        None => name_part(name),
    };
    if valid { name } else { spec }
}

// `deduplicatePluginOrigins`: the last declaration of an identity wins and keeps its (last) position.
pub fn dedupe(plugins: Vec<Value>) -> Vec<Value> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Value> = plugins
        .into_iter()
        .rev()
        .filter(|plugin| {
            let spec = specifier(plugin).unwrap_or_default();
            let identity = if spec.starts_with("file://") { spec } else { package(spec) };
            seen.insert(identity.to_string())
        })
        .collect();
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identity_is_package_name_or_url() {
        let list = vec![json!("a@1.0.0"), json!("@s/b"), json!(["a@2.0.0", {"x": 1}]), json!("file:///p/x.ts"), json!("@s/b@next"), json!("file:///p/x.ts")];
        assert_eq!(dedupe(list), [json!(["a@2.0.0", {"x": 1}]), json!("@s/b@next"), json!("file:///p/x.ts")]);
        assert_eq!(package("github:user/repo"), "github:user/repo");
    }

    #[test]
    fn path_specs_resolve_against_the_declaring_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("plugins/dir")).unwrap();
        std::fs::write(root.join("plugins/dir/index.ts"), "").unwrap();
        std::fs::write(root.join("plugins/one.ts"), "").unwrap();
        let config = root.join("opencode.json");

        let mut file = json!("./plugins/one.ts");
        resolve(&mut file, &config);
        assert_eq!(file, json!(file_url(&root.join("plugins/one.ts"))));
        assert_eq!(url_path(file.as_str().unwrap()).unwrap(), root.join("plugins/one.ts"));

        let mut index = json!(["./plugins/dir", {"k": true}]);
        resolve(&mut index, &config);
        assert_eq!(index, json!([file_url(&root.join("plugins/dir/index.ts")), {"k": true}]));

        let mut npm = json!("some-plugin@1.2.3");
        resolve(&mut npm, &config);
        assert_eq!(npm, json!("some-plugin@1.2.3"));

        let spaced = file_url(Path::new(if cfg!(windows) { r"C:\a b\x#1.ts" } else { "/a b/x#1.ts" }));
        assert!(spaced.ends_with("/a%20b/x%231.ts") && spaced.starts_with("file:///"), "{spaced}");
    }
}
