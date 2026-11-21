use globset::{GlobBuilder, GlobMatcher};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub dot: bool,
    pub follow: bool,
}

enum Segment {
    Literal(String),
    Wild { matcher: GlobMatcher, dotted: bool },
    Globstar,
}

fn expand_braces(pattern: &str) -> Vec<String> {
    let bytes = pattern.as_bytes();
    let mut start = None;
    let mut depth = 0usize;
    let mut commas: Vec<usize> = Vec::new();
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                    commas.clear();
                }
                depth += 1;
            }
            b',' if depth == 1 => commas.push(i),
            b'}' if depth > 0 => {
                depth -= 1;
                if depth > 0 {
                    continue;
                }
                let open = start.unwrap_or(0);
                if commas.is_empty() {
                    continue;
                }
                let mut bounds = vec![open];
                bounds.extend(&commas);
                bounds.push(i);
                let mut out = Vec::new();
                for pair in bounds.windows(2) {
                    let alt = &pattern[pair[0] + 1..pair[1]];
                    let candidate = format!("{}{}{}", &pattern[..open], alt, &pattern[i + 1..]);
                    out.extend(expand_braces(&candidate));
                }
                return out;
            }
            _ => {}
        }
    }
    vec![pattern.to_string()]
}

fn segments(pattern: &str) -> Option<Vec<Segment>> {
    let mut out = Vec::new();
    for part in pattern.split('/').filter(|p| !p.is_empty()) {
        if part == "**" {
            out.push(Segment::Globstar);
        } else if part.contains(['*', '?', '[']) {
            let matcher = GlobBuilder::new(part)
                .literal_separator(true)
                .case_insensitive(cfg!(any(windows, target_os = "macos")))
                .build()
                .ok()?
                .compile_matcher();
            out.push(Segment::Wild { matcher, dotted: part.starts_with('.') });
        } else {
            out.push(Segment::Literal(part.to_string()));
        }
    }
    Some(out)
}

fn entries(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    let mut list: Vec<(String, PathBuf)> = read
        .flatten()
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    list.sort();
    list
}

struct Walker {
    options: Options,
    out: Vec<PathBuf>,
    seen: HashSet<PathBuf>,
}

impl Walker {
    fn walk(&mut self, base: &Path, segs: &[Segment], depth: usize) {
        let Some((head, rest)) = segs.split_first() else {
            if base.is_file() {
                self.out.push(base.to_path_buf());
            }
            return;
        };
        if depth > 64 {
            return;
        }
        match head {
            Segment::Literal(name) => {
                let next = base.join(name);
                if rest.is_empty() || next.is_dir() {
                    self.walk(&next, rest, depth);
                }
            }
            Segment::Wild { matcher, dotted } => {
                for (name, path) in entries(base) {
                    if name.starts_with('.') && !self.options.dot && !dotted {
                        continue;
                    }
                    if matcher.is_match(&name) && (rest.is_empty() || path.is_dir()) {
                        self.walk(&path, rest, depth);
                    }
                }
            }
            Segment::Globstar => {
                if rest.is_empty() {
                    self.all_files(base, depth);
                    return;
                }
                self.walk(base, rest, depth);
                for (name, path) in entries(base) {
                    if self.descend(&name, &path) {
                        self.walk(&path, segs, depth + 1);
                    }
                }
            }
        }
    }

    fn descend(&mut self, name: &str, path: &Path) -> bool {
        if name.starts_with('.') && !self.options.dot {
            return false;
        }
        let link = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
        if !path.is_dir() || (link && !self.options.follow) {
            return false;
        }
        // Symlink cycles are cut by tracking the resolved directory.
        !link || fs::canonicalize(path).is_ok_and(|real| self.seen.insert(real))
    }

    fn all_files(&mut self, base: &Path, depth: usize) {
        if depth > 64 {
            return;
        }
        for (name, path) in entries(base) {
            if name.starts_with('.') && !self.options.dot {
                continue;
            }
            if path.is_file() {
                self.out.push(path);
            } else if self.descend(&name, &path) {
                self.all_files(&path, depth + 1);
            }
        }
    }
}

// Files matching `pattern` under `cwd`, absolute, sorted, de-duplicated.
pub fn scan(pattern: &str, cwd: &Path, options: Options) -> Vec<PathBuf> {
    let mut walker = Walker { options, out: Vec::new(), seen: HashSet::new() };
    for expanded in expand_braces(pattern) {
        if let Some(segs) = segments(&expanded) {
            walker.seen.clear();
            walker.walk(cwd, &segs, 0);
        }
    }
    let mut out = walker.out;
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str) {
        let file = root.join(rel);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, "x").unwrap();
    }

    fn rel(root: &Path, list: Vec<PathBuf>) -> Vec<String> {
        list.iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"))
            .collect()
    }

    #[test]
    fn braces() {
        assert_eq!(expand_braces("{a,b}/x"), ["a/x", "b/x"]);
        assert_eq!(expand_braces("x.{ts,js}"), ["x.ts", "x.js"]);
        assert_eq!(expand_braces("{a,{b,c}}"), ["a", "b", "c"]);
        assert_eq!(expand_braces("{solo}"), ["{solo}"]);
    }

    #[test]
    fn recursive_and_dot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for f in ["command/a.md", "commands/n/b.md", "commands/.h/c.md", "command/x.txt", "other/d.md", "agent"] {
            touch(root, f);
        }
        let dot = Options { dot: true, follow: true };
        assert_eq!(
            rel(root, scan("{command,commands}/**/*.md", root, dot)),
            ["command/a.md", "commands/.h/c.md", "commands/n/b.md"]
        );
        assert_eq!(
            rel(root, scan("{command,commands}/**/*.md", root, Options::default())),
            ["command/a.md", "commands/n/b.md"]
        );
        assert_eq!(rel(root, scan("{mode,modes}/*.md", root, dot)), Vec::<String>::new());
        assert_eq!(rel(root, scan("other/d.md", root, dot)), ["other/d.md"]);
        assert_eq!(rel(root, scan("**/*.md", root, Options::default())).len(), 3);
        assert_eq!(rel(root, scan("other/**", root, dot)), ["other/d.md"]);
        assert!(scan("agent/**/*.md", root, dot).is_empty());
    }
}
