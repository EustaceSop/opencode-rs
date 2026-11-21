// `{"mcpServers": {...}}` files: Cursor `~/.cursor/mcp.json`, Windsurf `~/.windsurf/mcp.json`, `~/.mcp.json`.
use std::path::Path;

use serde_json::Value;

use crate::{Category, Ctx, Item, Payload, SourceReport, mcp};

pub fn items(ctx: &Ctx, servers: Option<&Value>, report: &mut SourceReport, origin: &str) -> Vec<Item> {
    let mut out = Vec::new();
    for (name, server) in servers.and_then(Value::as_object).into_iter().flatten() {
        match mcp::from_json(server) {
            Ok(converted) => out.push(mcp_item(ctx, name, converted, origin)),
            Err(error) => report.warnings.push(format!("MCP server {name} in {origin}: {error}")),
        }
    }
    out
}

pub fn mcp_item(ctx: &Ctx, name: &str, converted: mcp::Converted, origin: &str) -> Item {
    let kind = converted.entry["type"].as_str().unwrap_or_default().to_string();
    let mut item = Item::new(Category::Mcp, name, format!("{kind} ({origin})"), Payload::Mcp(converted.entry.clone()));
    item.flags = converted.flags;
    item.conflict = ctx.visible.mcp_conflict(name, &converted.entry);
    item
}

pub fn read_json(file: &Path, report: &mut SourceReport) -> Option<Value> {
    let text = std::fs::read_to_string(file).ok()?;
    match opencode_core::config::parse_jsonc(&text, &file.to_string_lossy()) {
        Ok(value) => Some(value),
        Err(error) => {
            report.warnings.push(error.to_string());
            None
        }
    }
}

pub fn scan(ctx: &Ctx, id: &'static str, file: &Path) -> SourceReport {
    let mut report = SourceReport::new(id);
    if !report.locate(file) {
        return report;
    }
    if let Some(value) = read_json(file, &mut report) {
        let origin = file.to_string_lossy().into_owned();
        let found = items(ctx, value.get("mcpServers"), &mut report, &origin);
        report.items = found;
    }
    report
}
