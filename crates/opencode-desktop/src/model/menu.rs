// The app menu of the original as Windows shows it (`app/src/desktop-menu.ts:74-298`, items filtered for "windows",
// drawn by `app/src/components/windows-app-menu.tsx`), mapped onto what this client can do.
use super::keymap::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    Close,
    Minimize,
    ToggleMaximize,
    ToggleFullScreen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditAction {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    Delete,
    SelectAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Run {
    Command(Command),
    Window(WindowAction),
    Edit(EditAction),
    // The original zooms the web view; here the UI font size is the zoom (0 resets).
    Zoom(i8),
    Link(&'static str),
    // An entry of the original this client has no counterpart for; shown disabled.
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    Item(&'static str, Run),
    Separator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Menu {
    pub label: &'static str,
    pub entries: &'static [Entry],
}

use Entry::{Item, Separator};

// `windows-app-menu.tsx:82`: the dropdown's heading is the literal product name.
pub const HEADING: &str = "OpenCode";

pub const MENUS: &[Menu] = &[
    Menu {
        label: "desktop.menu.file",
        entries: &[
            Item("desktop.menu.newSession", Run::Command(Command::NewSession)),
            Item("desktop.menu.openProject", Run::Command(Command::OpenProject)),
            Item("desktop.menu.settings", Run::Command(Command::OpenSettings)),
            Item("desktop.menu.newWindow", Run::Unavailable),
            Separator,
            Item("desktop.menu.closeWindow", Run::Window(WindowAction::Close)),
        ],
    },
    Menu {
        label: "desktop.menu.edit",
        entries: &[
            Item("desktop.menu.undo", Run::Edit(EditAction::Undo)),
            Item("desktop.menu.redo", Run::Edit(EditAction::Redo)),
            Separator,
            Item("desktop.menu.cut", Run::Edit(EditAction::Cut)),
            Item("desktop.menu.copy", Run::Edit(EditAction::Copy)),
            Item("desktop.menu.paste", Run::Edit(EditAction::Paste)),
            Item("desktop.menu.delete", Run::Edit(EditAction::Delete)),
            Item("desktop.menu.selectAll", Run::Edit(EditAction::SelectAll)),
        ],
    },
    Menu {
        label: "desktop.menu.view",
        entries: &[
            Item("desktop.menu.toggleSidebar", Run::Command(Command::ToggleSidebar)),
            Item("desktop.menu.toggleTerminal", Run::Unavailable),
            Item("desktop.menu.toggleFileTree", Run::Unavailable),
            Separator,
            Item("desktop.menu.reload", Run::Unavailable),
            Item("desktop.menu.toggleDeveloperTools", Run::Unavailable),
            Separator,
            Item("desktop.menu.actualSize", Run::Zoom(0)),
            Item("desktop.menu.zoomIn", Run::Zoom(1)),
            Item("desktop.menu.zoomOut", Run::Zoom(-1)),
            Separator,
            Item("desktop.menu.toggleFullScreen", Run::Window(WindowAction::ToggleFullScreen)),
        ],
    },
    Menu {
        label: "desktop.menu.go",
        entries: &[
            Item("desktop.menu.back", Run::Unavailable),
            Item("desktop.menu.forward", Run::Unavailable),
            Separator,
            Item("desktop.menu.previousSession", Run::Command(Command::PreviousSession)),
            Item("desktop.menu.nextSession", Run::Command(Command::NextSession)),
            Separator,
            Item("desktop.menu.previousProject", Run::Unavailable),
            Item("desktop.menu.nextProject", Run::Unavailable),
        ],
    },
    Menu {
        label: "desktop.menu.window",
        entries: &[
            Item("desktop.menu.minimize", Run::Window(WindowAction::Minimize)),
            Item("desktop.menu.maximize", Run::Window(WindowAction::ToggleMaximize)),
            Separator,
            Item("desktop.menu.closeWindow", Run::Window(WindowAction::Close)),
        ],
    },
    Menu {
        label: "desktop.menu.help",
        entries: &[
            Item("desktop.menu.documentation", Run::Link("https://opencode.ai/docs")),
            Item("desktop.menu.supportForum", Run::Link("https://discord.com/invite/opencode")),
            Item("desktop.menu.exportLogs", Run::Unavailable),
            Separator,
            Item("desktop.menu.shareFeedback", Run::Link("https://github.com/anomalyco/opencode/issues/new?template=feature_request.yml")),
            Item("desktop.menu.reportBug", Run::Link("https://github.com/anomalyco/opencode/issues/new?template=bug_report.yml")),
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::i18n::I18n;

    #[test]
    fn every_entry_of_the_windows_menu_is_listed_with_its_text() {
        let english = I18n::default();
        let labels: Vec<&str> = MENUS
            .iter()
            .flat_map(|menu| std::iter::once(menu.label).chain(menu.entries.iter().filter_map(|entry| if let Item(label, _) = entry { Some(*label) } else { None })))
            .collect();
        for label in &labels {
            assert!(english.has(label), "no text for {label}");
        }
        assert_eq!(MENUS.len(), 6, "the macOS-only app menu is not part of the Windows menu");
        assert_eq!(labels.len(), 6 + 35);
        assert!(MENUS.iter().all(|menu| !matches!(menu.entries.first(), Some(Separator)) && !matches!(menu.entries.last(), Some(Separator))));
        assert_eq!(english.t("desktop.menu.ariaLabel").as_ref(), "OpenCode menu");
    }
}
