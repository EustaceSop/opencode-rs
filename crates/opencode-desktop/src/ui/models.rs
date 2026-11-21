// The model picker (`providers/models/select-dialog.tsx`), the variant choice, and the visibility lists of the
// manage-models dialog and the Models settings page (`providers/models/manage.tsx`, `settings/models/models.tsx`).
use super::app::{AppView, Choice, Overlay};
use crate::model::models::{self, Shared};
use crate::model::state::{ModelItem, Notice};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::combobox::Combobox;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::input::{self, Input};
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Anchor, AnyElement, App, Context, ElementId, Focusable as _, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, Window, div, px,
};

// A section of the picker: favourites, or one provider's models.
pub(super) struct Section<'a> {
    pub title: SharedString,
    pub items: Vec<&'a ModelItem>,
    // Favourites mix providers, so their rows name one.
    pub mixed: bool,
}

fn element_id(prefix: &str, value: &str) -> ElementId {
    ElementId::Name(format!("{prefix}-{value}").into())
}

// Variant names are shown capitalised, as the original's `capitalize` class does.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

fn model_id(spec: &str) -> &str {
    models::split(spec).map_or(spec, |(_, model)| model)
}

impl AppView {
    pub(super) fn model_visible(&self, item: &ModelItem) -> bool {
        self.persisted.models.visibility(&item.spec).unwrap_or(item.shown)
    }

    // Favourites lead until a search narrows the list; then every match sits in its provider's group.
    pub(super) fn picker_sections(&self, query: &str) -> Vec<Section<'_>> {
        let catalog = &self.state.catalog;
        let matches = |m: &ModelItem| models::matches_search(query, &[&m.name, model_id(&m.spec), &m.provider]);
        let searching = !query.trim().is_empty();
        let favorites: Vec<&ModelItem> =
            if searching { Vec::new() } else { self.shared.favorite.iter().filter_map(|spec| catalog.model(spec)).collect() };
        let mut sections = Vec::new();
        if !favorites.is_empty() {
            sections.push(Section { title: self.t("dialog.model.favorites"), items: favorites.clone(), mixed: true });
        }
        for group in &catalog.models {
            let items: Vec<&ModelItem> = group
                .models
                .iter()
                .filter(|m| self.model_visible(m) && matches(m) && !favorites.iter().any(|f| f.spec == m.spec))
                .collect();
            if !items.is_empty() {
                sections.push(Section { title: SharedString::from(group.provider.to_string()), items, mixed: false });
            }
        }
        sections
    }

    pub(super) fn set_model_menu(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.model_menu == open {
            return;
        }
        self.model_menu = open;
        if open {
            self.model_search.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            let order = self.picker_order_in(cx);
            let current = self.state.effective_model().map(str::to_string);
            self.model_active = current.filter(|spec| order.contains(spec)).or_else(|| order.first().cloned());
        } else {
            self.composer.update(cx, |composer, cx| composer.focus(window, cx));
        }
        cx.notify();
    }

    fn picker_order_in(&self, cx: &App) -> Vec<String> {
        let query = self.model_search.read(cx).value().to_string();
        self.picker_sections(&query).iter().flat_map(|s| s.items.iter().map(|m| m.spec.to_string())).collect()
    }

    pub(super) fn model_search_changed(&mut self, cx: &mut Context<Self>) {
        self.model_active = self.picker_order_in(cx).into_iter().next();
        cx.notify();
    }

    fn move_active_model(&mut self, delta: isize, cx: &mut Context<Self>) {
        let order = self.picker_order_in(cx);
        if order.is_empty() {
            return;
        }
        let at = self.model_active.as_ref().and_then(|spec| order.iter().position(|s| s == spec));
        let next = match at {
            Some(at) => (at as isize + delta).rem_euclid(order.len() as isize) as usize,
            None => 0,
        };
        self.model_active = Some(order[next].clone());
        cx.notify();
    }

    pub(super) fn choose_active_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(spec) = self.model_active.clone() {
            self.choose_model(&spec, window, cx);
        }
    }

    pub(super) fn choose_model(&mut self, spec: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.effective_model() != Some(spec) {
            self.state.model = Some(spec.to_string());
            self.remember();
        }
        self.set_model_menu(false, window, cx);
        self.refresh_variants(window, cx);
        cx.notify();
    }

    // Favourites and variants are read again and written back, so the TUI's changes are kept.
    fn update_shared(&mut self, edit: impl FnOnce(&mut Shared)) {
        match Shared::update(&models::file(self.backend.state_dir()), edit) {
            Ok(shared) => self.shared = shared,
            Err(error) => self.state.notice = Some(Notice { key: "desktop.rs.error.modelState", error: error.to_string() }),
        }
    }

    pub(super) fn toggle_favorite(&mut self, spec: &str, cx: &mut Context<Self>) {
        self.update_shared(|shared| shared.toggle_favorite(spec));
        cx.notify();
    }

    pub(super) fn current_variants(&self) -> Vec<std::sync::Arc<str>> {
        self.state.effective_model().and_then(|spec| self.state.catalog.model(spec)).map(|m| m.variants.clone()).unwrap_or_default()
    }

    pub(super) fn current_variant(&self) -> Option<String> {
        let spec = self.state.effective_model()?;
        self.shared.variant(spec, &self.current_variants())
    }

    pub(super) fn refresh_variants(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let choices: Vec<Choice> = std::iter::once(Choice { value: "default".into(), name: self.t("common.default") })
            .chain(self.current_variants().iter().map(|v| Choice { value: v.to_string().into(), name: capitalize(v).into() }))
            .collect();
        let selected = SharedString::from(self.current_variant().unwrap_or_else(|| "default".into()));
        self.variants.update(cx, |picker, cx| {
            picker.set_items(SearchableVec::new(choices), window, cx);
            picker.set_selected_values(&[selected], window, cx);
        });
    }

    pub(super) fn choose_variant(&mut self, value: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(value), Some(spec)) = (value, self.state.effective_model().map(str::to_string)) else {
            // Clicking the chosen entry cleared the selection; keep showing it.
            self.refresh_variants(window, cx);
            return;
        };
        if Some(value.as_str()) != self.current_variant().as_deref().or(Some("default")) {
            self.update_shared(|shared| shared.set_variant(&spec, (value != "default").then_some(value.as_str())));
        }
        cx.notify();
    }

    pub(super) fn set_model_visible(&mut self, spec: &str, visible: bool, cx: &mut Context<Self>) {
        if self.persisted.models.set_visible(spec, visible) {
            self.saver.save(&self.persisted);
        }
        cx.notify();
    }

    pub(super) fn set_provider_visible(&mut self, provider_id: &str, visible: bool, cx: &mut Context<Self>) {
        let specs: Vec<String> =
            self.state.catalog.models.iter().filter(|g| g.id.as_ref() == provider_id).flat_map(|g| g.models.iter().map(|m| m.spec.to_string())).collect();
        let mut changed = false;
        for spec in specs {
            changed |= self.persisted.models.set_visible(&spec, visible);
        }
        if changed {
            self.saver.save(&self.persisted);
        }
        cx.notify();
    }

    fn render_model_row(&self, item: &ModelItem, mixed: bool, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let spec = item.spec.to_string();
        let current = self.state.effective_model() == Some(spec.as_str());
        let active = self.model_active.as_deref() == Some(spec.as_str());
        let favorite = self.shared.is_favorite(&spec);
        let (choose, star) = (spec.clone(), spec.clone());
        // The star sits beside the clickable part, not inside it, so starring never picks the model.
        h_flex()
            .w_full()
            .h(px(28.))
            .pr_1()
            .rounded(theme.radius)
            .text_sm()
            .when(active, |this| this.bg(theme.accent).text_color(theme.accent_foreground))
            .when(!active, |this| this.hover(|this| this.bg(theme.accent.opacity(0.5))))
            .child(
                h_flex()
                    .id(element_id("model-option", &spec))
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .px_2()
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| this.choose_model(&choose, window, cx)))
                    .child(div().w_4().flex_shrink_0().when(current, |this| this.child(Icon::new(IconName::Check).small())))
                    .child(div().min_w_0().truncate().child(SharedString::from(item.name.to_string())))
                    .when(mixed, |this| {
                        this.child(div().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(SharedString::from(item.provider.to_string())))
                    })
                    .when(item.free, |this| this.child(Tag::secondary().small().child(self.t("model.tag.free"))))
                    .when(item.latest, |this| this.child(Tag::secondary().small().child(self.t("model.tag.latest")))),
            )
            .child(
                Button::new(element_id("model-favorite", &spec))
                    .ghost()
                    .xsmall()
                    .icon(if favorite { IconName::StarFill } else { IconName::Star })
                    .tooltip(self.t(if favorite { "dialog.model.unfavorite" } else { "dialog.model.favorite" }))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_favorite(&star, cx))),
            )
            .into_any_element()
    }

    fn render_model_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let query = self.model_search.read(cx).value().to_string();
        let sections = self.picker_sections(&query);
        let empty = sections.is_empty();
        let list = sections.iter().map(|section| {
            v_flex()
                .w_full()
                .child(div().px_2().pt_2().pb_1().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child(section.title.clone()))
                .children(section.items.iter().map(|item| self.render_model_row(item, section.mixed, cx)))
        });
        let list: Vec<_> = list.collect();
        v_flex()
            .id("model-menu")
            .test_support()
            .w(px(340.))
            .gap_1()
            // The surface sits inside the trigger's dispatch path; a press in it must not reach the trigger's toggle.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .capture_action(cx.listener(|this, _: &input::MoveUp, _, cx| {
                this.move_active_model(-1, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &input::MoveDown, _, cx| {
                this.move_active_model(1, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &input::Escape, window, cx| {
                this.set_model_menu(false, window, cx);
                cx.stop_propagation();
            }))
            .child(
                h_flex()
                    .gap_1()
                    .child(div().flex_1().min_w_0().child(Input::new(&self.model_search).focus_ring(false).small().prefix(Icon::new(IconName::Search).small()).cleanable(true)))
                    .child(
                        Button::new("model-connect")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip(self.t("command.provider.connect"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.set_model_menu(false, window, cx);
                                this.open_connect(None, None, window, cx);
                            })),
                    )
                    .child(
                        Button::new("model-manage")
                            .ghost()
                            .small()
                            .icon(Lucide::SlidersHorizontal)
                            .tooltip(self.t("dialog.model.manage"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.set_model_menu(false, window, cx);
                                this.open_overlay(Overlay::ManageModels, window, cx);
                            })),
                    ),
            )
            .child(div().h(px(1.)).w_full().bg(theme.border))
            .child(
                v_flex()
                    .id("model-list")
                    .w_full()
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .when(empty, |this| this.child(div().px_2().py_2().text_sm().text_color(theme.muted_foreground).child(self.t("dialog.model.empty"))))
                    .children(list),
            )
            .into_any_element()
    }

    // The composer's model chip, which opens the picker above it.
    pub(super) fn render_model_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let label = self.state.chip_label().map_or_else(|| self.t("dialog.model.select.title"), SharedString::from);
        let view = cx.entity().downgrade();
        let focus = self.model_search.read(cx).focus_handle(cx);
        let popover = Popover::new("model-popover")
            .anchor(Anchor::BottomLeft)
            .open(self.model_menu)
            .track_focus(&focus)
            .on_open_change(move |open, window, cx| {
                let _ = view.update(cx, |this, cx| this.set_model_menu(*open, window, cx));
            })
            .trigger(Button::new("model-picker").small().ghost().label(label).dropdown_caret(true));
        let popover = if self.model_menu { popover.child(self.render_model_menu(cx)) } else { popover };
        div().id("model-chip").test_support().min_w_0().child(popover).into_any_element()
    }

    pub(super) fn render_variant_picker(&self) -> Option<AnyElement> {
        if self.current_variants().is_empty() {
            return None;
        }
        Some(
            div()
                .id("variant-picker")
                .test_support()
                .w(px(112.))
                .flex_shrink_0()
                .child(Combobox::new(&self.variants).small().w_full().placeholder(self.t("common.default")))
                .into_any_element(),
        )
    }

    pub(super) fn toggle_collapsed(&mut self, provider_id: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(provider_id) {
            self.collapsed.insert(provider_id.to_string());
        }
        cx.notify();
    }

    // Providers as foldable groups with a switch for all their models and one per model; `prefix` keeps the ids of
    // the dialog and the settings page apart.
    pub(super) fn render_model_groups(&self, prefix: &'static str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let query = self.model_filter.read(cx).value().to_string();
        let searching = !query.trim().is_empty();
        let groups: Vec<_> = self
            .state
            .catalog
            .models
            .iter()
            .map(|group| {
                let items: Vec<&ModelItem> =
                    group.models.iter().filter(|m| models::matches_search(&query, &[&group.provider, &m.name, model_id(&m.spec)])).collect();
                (group, items)
            })
            .filter(|(_, items)| !items.is_empty())
            .collect();
        if groups.is_empty() {
            return div().py_4().text_sm().text_color(theme.muted_foreground).child(self.t("dialog.model.empty")).into_any_element();
        }
        let sections = groups.into_iter().map(|(group, items)| {
            let id = group.id.to_string();
            let expanded = searching || !self.collapsed.contains(&id);
            let all = group.models.iter().all(|m| self.model_visible(m));
            let (fold, toggle_all) = (id.clone(), id.clone());
            let initial: String = group.provider.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
            let header = h_flex()
                .w_full()
                .gap_2()
                .py_1()
                .child(
                    h_flex()
                        .id(element_id(&format!("{prefix}-group"), &id))
                        .test_support()
                        .flex_1()
                        .min_w_0()
                        .gap_2()
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !searching {
                                this.toggle_collapsed(&fold, cx);
                            }
                        }))
                        .child(Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight }).small().text_color(theme.muted_foreground))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(18.))
                                .flex_shrink_0()
                                .rounded(theme.radius)
                                .bg(theme.muted)
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .child(initial),
                        )
                        .child(div().min_w_0().truncate().text_sm().font_weight(FontWeight::MEDIUM).child(SharedString::from(group.provider.to_string()))),
                )
                .child(
                    Switch::new(element_id(&format!("{prefix}-provider"), &id))
                        .checked(all)
                        .tooltip(self.i18n.tf("dialog.model.manage.provider.toggle", &[("provider", &group.provider)]))
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| this.set_provider_visible(&toggle_all, *checked, cx))),
                );
            // As in the official settings: the provider's models in one rounded card, a hairline between rows.
            let count = items.len();
            let rows = items.into_iter().enumerate().map(|(index, item)| {
                let spec = item.spec.to_string();
                let toggle = spec.clone();
                h_flex()
                    .w_full()
                    .min_h(px(40.))
                    .px_4()
                    .gap_3()
                    .text_sm()
                    .when(index + 1 < count, |this| this.border_b_1().border_color(theme.border))
                    .child(div().flex_1().min_w_0().truncate().child(SharedString::from(item.name.to_string())))
                    .child(
                        Switch::new(element_id(&format!("{prefix}-model"), &spec))
                            .checked(self.model_visible(item))
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| this.set_model_visible(&toggle, *checked, cx))),
                    )
            });
            let card = v_flex().w_full().overflow_hidden().rounded(theme.radius_lg).border_1().border_color(theme.border).bg(theme.secondary).children(rows);
            v_flex().w_full().gap_2().child(header).when(expanded, |this| this.child(card))
        });
        v_flex().w_full().gap_5().children(sections).into_any_element()
    }

    pub(super) fn render_manage_models(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .id("manage-models")
            .test_support()
            .w(px(560.))
            .h(px(600.))
            .max_h_full()
            .gap_3()
            .p_4()
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(self.t("dialog.model.manage")))
                            .child(div().text_xs().text_color(theme.muted_foreground).child(self.t("dialog.model.manage.description"))),
                    )
                    .child(
                        Button::new("manage-connect")
                            .small()
                            .outline()
                            .icon(IconName::Plus)
                            .label(self.t("command.provider.connect"))
                            .on_click(cx.listener(|this, _, window, cx| this.open_connect(None, None, window, cx))),
                    )
                    .child(
                        Button::new("overlay-close")
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .tooltip(self.t("common.close"))
                            .on_click(cx.listener(|this, _, window, cx| this.close_overlay(window, cx))),
                    ),
            )
            .child(Input::new(&self.model_filter).focus_ring(false).small().prefix(Icon::new(IconName::Search).small()).cleanable(true))
            .child(div().id("manage-models-list").flex_1().min_h_0().overflow_y_scroll().child(self.render_model_groups("manage", cx)))
            .into_any_element()
    }
}
