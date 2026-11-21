// `opencode import` without a file: bring MCP servers, agents, skills and sessions over from other AI agents.
use crate::prompt;
use crate::ui::{self, fail};
use anyhow::Result;
use opencode_core::paths::Paths;
use opencode_import::{Category, ImportReport, Plan, Selection, SourceReport};
use opencode_storage::Store;

pub struct Args {
    pub list: bool,
    pub dry_run: bool,
    pub yes: bool,
    pub sources: Vec<String>,
    pub categories: Vec<String>,
}

fn label(category: Category) -> &'static str {
    match category {
        Category::Mcp => "MCP servers",
        Category::Agents => "agents",
        Category::Skills => "skills",
        Category::Sessions => "sessions",
    }
}

fn list(reports: &[SourceReport]) {
    for report in reports {
        if !report.found {
            println!("{}: not found", report.name);
            continue;
        }
        println!("{} ({}): found", report.name, report.id);
        let counts: Vec<String> = Category::ALL
            .iter()
            .map(|c| {
                let (all, ok) = (report.count(*c), report.importable(*c));
                if all == ok { format!("{} {all}", label(*c)) } else { format!("{} {all} ({ok} importable)", label(*c)) }
            })
            .collect();
        println!("  {}", counts.join(", "));
        for shared in &report.shared {
            println!("  already shared: {shared}");
        }
        for warning in &report.warnings {
            println!("  warning: {warning}");
        }
    }
}

fn dry_run(reports: &[SourceReport], plan: &Plan) {
    println!("Dry run: nothing is written.");
    for report in reports.iter().filter(|r| r.found) {
        let lines: Vec<String> = Category::ALL
            .iter()
            .filter_map(|c| {
                let (planned, skipped) = (plan.count(report.id, *c), plan.skipped_count(report.id, *c));
                (planned + skipped > 0).then(|| format!("  {}: {planned} to import, {skipped} skipped", label(*c)))
            })
            .collect();
        if lines.is_empty() {
            continue;
        }
        println!("{}", report.name);
        lines.iter().for_each(|line| println!("{line}"));
    }
    let flagged: Vec<_> = plan.flagged().filter(|p| p.item.category != Category::Sessions).collect();
    if !flagged.is_empty() {
        println!("Flagged:");
        for planned in flagged {
            println!("  {} {} {}: {}", opencode_import::source_name(planned.source), planned.item.category, planned.item.name, planned.item.flags.join("; "));
        }
    }
    let skipped: Vec<_> = plan.skipped.iter().filter(|s| s.category != Category::Sessions).collect();
    if !skipped.is_empty() {
        println!("Skipped:");
        for skip in skipped {
            println!("  {} {} {}: {}", opencode_import::source_name(skip.source), skip.category, skip.name, skip.reason);
        }
    }
    println!("Total: {} to import, {} skipped; sessions already imported and unchanged are left as they are.", plan.items.len(), plan.skipped.len());
}

// The grouped checklist: one entry per source and category, everything selected.
fn choose(reports: &[SourceReport], selection: Selection) -> Result<Selection> {
    let mut groups: Vec<(String, (String, Category))> = Vec::new();
    for report in reports.iter().filter(|r| r.found) {
        for category in Category::ALL {
            let count = report.importable(category);
            if count == 0 || !selection.includes(report.id, category) {
                continue;
            }
            let secrets = report.items.iter().filter(|i| i.category == category && i.conflict.is_none() && i.secrets()).count();
            let mut text = format!("{} · {} ({count})", report.name, label(category));
            if secrets > 0 {
                text.push_str(&format!(" — {secrets} with secrets copied as-is"));
            }
            groups.push((text, (report.id.to_string(), category)));
        }
    }
    if groups.is_empty() {
        return Ok(Selection { groups: Some(Vec::new()), ..selection });
    }
    let labels: Vec<&str> = groups.iter().map(|(text, _)| text.as_str()).collect();
    let chosen = prompt::multiselect("Choose what to bring over to the Rust build", &labels)?;
    let picked = groups.into_iter().filter(|(text, _)| chosen.contains(text)).map(|(_, group)| group).collect();
    Ok(Selection { groups: Some(picked), ..selection })
}

fn summary(report: &ImportReport) {
    for outcome in &report.outcomes {
        let name = opencode_import::source_name(outcome.source);
        ui::info(&format!("{name} {}: {} imported, {} unchanged, {} skipped, {} failed", label(outcome.category), outcome.written, outcome.unchanged, outcome.skipped, outcome.failed));
    }
    for warning in &report.warnings {
        ui::warn(warning);
    }
    for error in &report.errors {
        ui::failure(error);
    }
}

pub fn run(paths: &Paths, args: Args) -> Result<i32> {
    for source in &args.sources {
        if !opencode_import::SOURCES.iter().any(|(id, _)| id == source) {
            let known: Vec<&str> = opencode_import::SOURCES.iter().map(|(id, _)| *id).collect();
            return Err(fail(format!("Unknown source: {source} (one of {})", known.join(", "))));
        }
    }
    let categories = args.categories.iter().filter_map(|c| Category::parse(c)).collect();
    let selection = Selection { sources: args.sources, categories, groups: None };
    let reports = opencode_import::scan(paths, &paths.home);
    if args.list {
        list(&reports);
        return Ok(0);
    }
    if args.dry_run {
        dry_run(&reports, &opencode_import::plan(&reports, &selection));
        return Ok(0);
    }
    let interactive = !args.yes;
    if interactive && !prompt::interactive() {
        return Err(fail("Not a terminal: pass --yes to import everything selected by --source/--category, or --dry-run to preview"));
    }
    ui::empty();
    ui::intro("Import from other AI agents");
    let selection = if interactive { choose(&reports, selection)? } else { selection };
    let plan = opencode_import::plan(&reports, &selection);
    if plan.is_empty() {
        let _ = opencode_import::mark_asked(paths);
        ui::outro("Nothing to import");
        return Ok(0);
    }
    for planned in plan.flagged().filter(|p| p.item.secrets()) {
        ui::warn(&format!("{} {} {}: {}", opencode_import::source_name(planned.source), planned.item.category, planned.item.name, planned.item.flags.join("; ")));
    }
    if interactive && !prompt::confirm(&format!("Import {} items?", plan.items.len()), true)? {
        ui::outro("Cancelled");
        return Ok(0);
    }
    std::fs::create_dir_all(&paths.data)?;
    let store = Store::open(Store::default_path(&paths.data))?;
    let report = opencode_import::apply(&plan, &store, paths);
    summary(&report);
    ui::outro(&format!("Imported into {} and {}", paths.rust_config().display(), Store::default_path(&paths.data).display()));
    Ok(if report.errors.is_empty() { 0 } else { 1 })
}

// The one-line first-run notice before the TUI or `run` starts; shown once.
pub fn hint(paths: &Paths) {
    if !opencode_import::first_run_pending(paths, &paths.home) {
        return;
    }
    let names = opencode_import::detect(paths, &paths.home);
    ui::println(&format!("Found data from {}; run `opencode import` to choose what to bring over.", names.join(", ")));
    let _ = opencode_import::mark_asked(paths);
}
