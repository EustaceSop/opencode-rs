// The app menu button of the title bar, as the original draws it on Windows, and the window actions it adds.
use super::app::{
    AppView, AttachFiles, AutoAccept, ChooseModel, CycleAgent, CycleAgentReverse, CycleTheme, FocusInput, FocusSearch, NewSession, NextSession,
    OpenProject, OpenSettings, PreviousSession, Shortcuts, ToggleSidebar,
};
use crate::model::i18n::I18n;
use crate::model::keymap::Command;
use crate::model::menu::{self, EditAction, Entry, Menu, Run, WindowAction};
use crate::model::settings::FONT_SIZE_DEFAULT;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::{Action, Context, Div, InteractiveElement as _, IntoElement, Stateful, Window};
use std::rc::Rc;

gpui_kit::actions!(opencode, [CloseWindow, MinimizeWindow, ToggleMaximize, ToggleFullScreen, ZoomIn, ZoomOut, ActualSize]);

fn command_action(command: Command) -> Box<dyn Action> {
    match command {
        Command::NewSession => Box::new(NewSession),
        Command::OpenSettings => Box::new(OpenSettings),
        Command::Shortcuts => Box::new(Shortcuts),
        Command::ToggleSidebar => Box::new(ToggleSidebar),
        Command::OpenProject => Box::new(OpenProject),
        Command::FocusInput => Box::new(FocusInput),
        Command::FocusSearch => Box::new(FocusSearch),
        Command::PreviousSession => Box::new(PreviousSession),
        Command::NextSession => Box::new(NextSession),
        Command::ChooseModel => Box::new(ChooseModel),
        Command::CycleAgent => Box::new(CycleAgent),
        Command::CycleAgentReverse => Box::new(CycleAgentReverse),
        Command::AutoAccept => Box::new(AutoAccept),
        Command::AttachFiles => Box::new(AttachFiles),
        Command::CycleTheme => Box::new(CycleTheme),
    }
}

fn action(run: Run) -> Option<Box<dyn Action>> {
    Some(match run {
        Run::Command(command) => command_action(command),
        Run::Window(WindowAction::Close) => Box::new(CloseWindow),
        Run::Window(WindowAction::Minimize) => Box::new(MinimizeWindow),
        Run::Window(WindowAction::ToggleMaximize) => Box::new(ToggleMaximize),
        Run::Window(WindowAction::ToggleFullScreen) => Box::new(ToggleFullScreen),
        Run::Edit(EditAction::Undo) => Box::new(input::Undo),
        Run::Edit(EditAction::Redo) => Box::new(input::Redo),
        Run::Edit(EditAction::Cut) => Box::new(input::Cut),
        Run::Edit(EditAction::Copy) => Box::new(input::Copy),
        Run::Edit(EditAction::Paste) => Box::new(input::Paste),
        Run::Edit(EditAction::Delete) => Box::new(input::Delete),
        Run::Edit(EditAction::SelectAll) => Box::new(input::SelectAll),
        Run::Zoom(0) => Box::new(ActualSize),
        Run::Zoom(step) if step > 0 => Box::new(ZoomIn),
        Run::Zoom(_) => Box::new(ZoomOut),
        Run::Link(_) | Run::Unavailable => return None,
    })
}

fn fill(mut popup: PopupMenu, menu: &Menu, i18n: &I18n) -> PopupMenu {
    for entry in menu.entries {
        popup = match *entry {
            Entry::Separator => popup.separator(),
            Entry::Item(label, Run::Link(href)) => popup.link(i18n.t(label).to_string(), href),
            Entry::Item(label, run) => match action(run) {
                Some(action) => popup.menu(i18n.t(label).to_string(), action),
                None => popup.item(PopupMenuItem::new(i18n.t(label).to_string()).disabled(true)),
            },
        };
    }
    popup
}

impl AppView {
    // Edit entries act on whatever had focus before the menu opened, as `windows-app-menu.tsx` restores it.
    pub(super) fn render_app_menu(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement + use<> {
        let i18n: Rc<I18n> = self.i18n.clone();
        let context = window.focused(cx).unwrap_or_else(|| self.focus.clone());
        Button::new("app-menu").ghost().xsmall().icon(IconName::Menu).tooltip(self.t("desktop.menu.ariaLabel")).dropdown_menu(move |popup, window, cx| {
            let mut popup = popup.action_context(context.clone()).label(menu::HEADING);
            for menu in menu::MENUS {
                let i18n = i18n.clone();
                popup = popup.submenu(i18n.t(menu.label).to_string(), window, cx, move |sub, _, _| fill(sub, menu, &i18n));
            }
            popup
        })
    }

    pub(super) fn reset_font(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.persisted.settings.font_size = FONT_SIZE_DEFAULT;
        self.apply_theme(window, cx);
        self.save_settings(cx);
    }
}

pub(super) fn on_menu_actions(root: Stateful<Div>, cx: &mut Context<AppView>) -> Stateful<Div> {
    root.on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
        .on_action(cx.listener(|_, _: &MinimizeWindow, window, _| window.minimize_window()))
        .on_action(cx.listener(|_, _: &ToggleMaximize, window, _| window.zoom_window()))
        .on_action(cx.listener(|_, _: &ToggleFullScreen, window, _| window.toggle_fullscreen()))
        .on_action(cx.listener(|this, _: &ZoomIn, window, cx| this.step_font(1.0, window, cx)))
        .on_action(cx.listener(|this, _: &ZoomOut, window, cx| this.step_font(-1.0, window, cx)))
        .on_action(cx.listener(|this, _: &ActualSize, window, cx| this.reset_font(window, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_with_a_counterpart_maps_to_an_action() {
        for menu in menu::MENUS {
            for entry in menu.entries {
                if let Entry::Item(label, run) = entry {
                    let expected = !matches!(run, Run::Link(_) | Run::Unavailable);
                    assert_eq!(action(*run).is_some(), expected, "{label}");
                }
            }
        }
        assert_eq!(action(Run::Zoom(-1)).unwrap().name(), ZoomOut.name());
        assert_eq!(action(Run::Command(Command::NewSession)).unwrap().name(), NewSession.name());
    }
}
