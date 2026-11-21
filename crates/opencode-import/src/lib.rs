// Brings MCP servers, agents, skills and conversations over from other coding agents. Sources are only read
// (SQLite opened with `mode=ro`); everything is written to the Rust build's own store and `<XDG config>/opencode-rs/`.
mod agents;
mod convert;
pub mod ids;
mod mcp;
mod skills;
mod sources;
mod state;
mod target;

use std::path::{Path, PathBuf};

use opencode_core::flag::{self, Lookup};
use opencode_core::paths::Paths;
use opencode_storage::Store;
use serde_json::Value;

pub use state::{SourceState, State, detect, detect_with, first_run_pending, load_state, mark_asked, state_file};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    #[default]
    Mcp,
    Agents,
    Skills,
    Sessions,
}

impl Category {
    pub const ALL: [Category; 4] = [Category::Mcp, Category::Agents, Category::Skills, Category::Sessions];

    pub fn as_str(self) -> &'static str {
        match self {
            Category::Mcp => "mcp",
            Category::Agents => "agents",
            Category::Skills => "skills",
            Category::Sessions => "sessions",
        }
    }

    pub fn parse(text: &str) -> Option<Category> {
        Category::ALL.into_iter().find(|c| c.as_str() == text.trim().to_lowercase())
    }
}

impl std::fmt::Display for Category {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// (id, display name) of every supported source, in scan and precedence order.
pub const SOURCES: [(&str, &str); 8] = [
    ("opencode", "opencode (TypeScript)"),
    ("claude-code", "Claude Code"),
    ("codex", "Codex"),
    ("cursor", "Cursor"),
    ("windsurf", "Windsurf"),
    ("mcp-json", "~/.mcp.json"),
    ("grok-build", "Grok Build"),
    ("deepseek-harness", "DeepSeek Harness"),
];

pub fn source_name(id: &str) -> &'static str {
    SOURCES.iter().find(|(known, _)| *known == id).map_or("unknown", |(_, name)| name)
}

#[derive(Debug, Clone)]
pub(crate) enum Payload {
    Mcp(Value),
    Agent { file: String, content: String },
    Skill { dir: PathBuf, target: String },
    Session(sources::SessionSource),
}

// One thing that can be brought over.
#[derive(Debug, Clone)]
pub struct Item {
    pub category: Category,
    pub name: String,
    pub detail: String,
    // Notes the plan must show, e.g. literal secrets in env/headers or dropped settings.
    pub flags: Vec<String>,
    // Why it cannot be imported (a name the Rust build already uses); `None` when it can.
    pub conflict: Option<String>,
    pub(crate) payload: Payload,
}

impl Item {
    pub(crate) fn new(category: Category, name: impl Into<String>, detail: impl Into<String>, payload: Payload) -> Self {
        Item { category, name: name.into(), detail: detail.into(), flags: Vec::new(), conflict: None, payload }
    }

    pub fn secrets(&self) -> bool {
        self.flags.iter().any(|f| f.starts_with(mcp::SECRET_FLAG))
    }
}

#[derive(Debug, Clone)]
pub struct SourceReport {
    pub id: &'static str,
    pub name: &'static str,
    pub found: bool,
    pub locations: Vec<PathBuf>,
    pub items: Vec<Item>,
    // Things the Rust build already reads from this source without importing.
    pub shared: Vec<String>,
    pub warnings: Vec<String>,
}

impl SourceReport {
    pub(crate) fn new(id: &'static str) -> Self {
        SourceReport { id, name: source_name(id), found: false, locations: Vec::new(), items: Vec::new(), shared: Vec::new(), warnings: Vec::new() }
    }

    pub fn count(&self, category: Category) -> usize {
        self.items.iter().filter(|i| i.category == category).count()
    }

    pub fn importable(&self, category: Category) -> usize {
        self.items.iter().filter(|i| i.category == category && i.conflict.is_none()).count()
    }

    pub(crate) fn locate(&mut self, path: &Path) -> bool {
        if path.exists() {
            self.found = true;
            if !self.locations.iter().any(|p| p == path) {
                self.locations.push(path.to_path_buf());
            }
            return true;
        }
        false
    }
}

pub(crate) struct Ctx<'a> {
    pub paths: &'a Paths,
    pub home: &'a Path,
    pub env: Lookup<'a>,
    pub visible: target::Visible,
}

impl Ctx<'_> {
    pub fn var(&self, key: &str) -> Option<String> {
        (self.env)(key).filter(|v| !v.trim().is_empty())
    }
}

pub fn scan(paths: &Paths, home: &Path) -> Vec<SourceReport> {
    scan_with(paths, home, &flag::process_env)
}

pub fn scan_with(paths: &Paths, home: &Path, env: Lookup) -> Vec<SourceReport> {
    let ctx = Ctx { paths, home, env, visible: target::Visible::load(paths, home) };
    sources::scan_all(&ctx)
}

// What to import: every (source, category) group that passes both filters. Empty filters pass everything.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub sources: Vec<String>,
    pub categories: Vec<Category>,
    // When set, only these groups (after the filters above).
    pub groups: Option<Vec<(String, Category)>>,
}

impl Selection {
    pub fn all() -> Self {
        Selection::default()
    }

    pub fn includes(&self, source: &str, category: Category) -> bool {
        (self.sources.is_empty() || self.sources.iter().any(|s| s == source))
            && (self.categories.is_empty() || self.categories.contains(&category))
            && self.groups.as_ref().is_none_or(|groups| groups.iter().any(|(s, c)| s == source && *c == category))
    }
}

#[derive(Debug, Clone)]
pub struct Planned {
    pub source: &'static str,
    pub item: Item,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub source: &'static str,
    pub category: Category,
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub items: Vec<Planned>,
    pub skipped: Vec<Skipped>,
}

impl Plan {
    pub fn count(&self, source: &str, category: Category) -> usize {
        self.items.iter().filter(|p| p.source == source && p.item.category == category).count()
    }

    pub fn skipped_count(&self, source: &str, category: Category) -> usize {
        self.skipped.iter().filter(|s| s.source == source && s.category == category).count()
    }

    pub fn flagged(&self) -> impl Iterator<Item = &Planned> {
        self.items.iter().filter(|p| !p.item.flags.is_empty())
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

// Selected, non-conflicting items. Of two sources defining the same MCP server, agent or skill name, the first wins.
pub fn plan(reports: &[SourceReport], selection: &Selection) -> Plan {
    let mut plan = Plan::default();
    let mut taken: Vec<(Category, String, &'static str, Option<&Payload>)> = Vec::new();
    for report in reports.iter().filter(|r| r.found) {
        for item in &report.items {
            if !selection.includes(report.id, item.category) {
                continue;
            }
            let skip = |reason: String| Skipped { source: report.id, category: item.category, name: item.name.clone(), reason };
            if let Some(reason) = &item.conflict {
                plan.skipped.push(skip(reason.clone()));
                continue;
            }
            if item.category != Category::Sessions {
                let key = target::name_key(item.category, &item.name);
                if let Some((_, _, owner, payload)) = taken.iter().find(|(c, n, _, _)| *c == item.category && *n == key) {
                    let same = payload.is_some_and(|p| same_payload(p, &item.payload));
                    let reason = if same { format!("duplicate of {}", source_name(owner)) } else { format!("name also defined by {}", source_name(owner)) };
                    plan.skipped.push(skip(reason));
                    continue;
                }
                taken.push((item.category, key, report.id, Some(&item.payload)));
            }
            plan.items.push(Planned { source: report.id, item: item.clone() });
        }
    }
    plan
}

fn same_payload(a: &Payload, b: &Payload) -> bool {
    match (a, b) {
        (Payload::Mcp(x), Payload::Mcp(y)) => x == y,
        (Payload::Agent { content: x, .. }, Payload::Agent { content: y, .. }) => x == y,
        _ => false,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub source: &'static str,
    pub category: Category,
    pub written: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    pub outcomes: Vec<Outcome>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    // Files written under `<XDG config>/opencode-rs/`.
    pub files: Vec<PathBuf>,
}

impl ImportReport {
    pub(crate) fn outcome(&mut self, source: &'static str, category: Category) -> &mut Outcome {
        if let Some(index) = self.outcomes.iter().position(|o| o.source == source && o.category == category) {
            return &mut self.outcomes[index];
        }
        self.outcomes.push(Outcome { source, category, ..Default::default() });
        self.outcomes.last_mut().expect("just pushed")
    }

    pub fn total(&self, pick: impl Fn(&Outcome) -> usize) -> usize {
        self.outcomes.iter().map(pick).sum()
    }
}

// Writes the plan: config items into `<XDG config>/opencode-rs/`, sessions into `store`. Records the run in the state file.
pub fn apply(plan: &Plan, store: &Store, paths: &Paths) -> ImportReport {
    let mut report = ImportReport::default();
    target::apply_config(plan, paths, &mut report);
    sources::apply_sessions(plan, store, &mut report);
    state::record(paths, &report);
    report
}
