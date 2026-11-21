// Skill folders (`<dir>/SKILL.md`) of other tools.
use std::path::{Path, PathBuf};

use opencode_core::glob;

use crate::target::{Visible, safe_name, skill_name};
use crate::{Category, Item, Payload};

// Directories holding a SKILL.md under `root`; hidden directories (e.g. bundled `.system` skills) are left out.
pub fn find(root: &Path) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut dirs: Vec<PathBuf> = glob::scan("**/SKILL.md", root, glob::Options { dot: false, follow: true })
        .into_iter()
        .filter_map(|file| file.parent().map(Path::to_path_buf))
        .filter(|dir| dir != root)
        .collect();
    dirs.sort();
    dirs.dedup();
    // A skill nested inside another skill's folder travels with its parent.
    let parents = dirs.clone();
    dirs.retain(|dir| !parents.iter().any(|p| p != dir && dir.starts_with(p)));
    dirs
}

pub fn items(root: &Path, visible: &Visible) -> Vec<Item> {
    find(root)
        .into_iter()
        .map(|dir| {
            let name = skill_name(&dir.join("SKILL.md"));
            let target = safe_name(&name);
            let conflict = visible.skill_conflict(&name, &dir, &target);
            let detail = dir.to_string_lossy().into_owned();
            let mut item = Item::new(Category::Skills, name, detail, Payload::Skill { dir, target });
            item.conflict = conflict;
            item
        })
        .collect()
}

pub fn count(root: &Path) -> usize {
    find(root).len()
}
