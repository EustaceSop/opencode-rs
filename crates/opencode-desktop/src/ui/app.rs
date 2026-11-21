// The window's root view: observes `AppState`, forwards user actions to the backend.
use super::alerts::Alerts;
use super::rows::{Callback, RowContext, render_row};
use super::theme;
use crate::model::attention::{self, Attention, Kind, Signal, Target};
use crate::model::backend::Backend;
use crate::model::deeplink::{self, Link, Request};
use crate::model::i18n::{self, I18n};
use crate::model::keymap::{self, Command};
use crate::model::models::Shared;
use crate::model::persist::{Persisted, Saver, WindowGeometry};
use crate::model::prompt::{Attachment, History};
use crate::model::question::QuestionForm;
use crate::model::settings::Toggle;
use crate::model::state::{AppState, ChildActivity, Event, Notice, RunStatus, SidebarRow};
use crate::model::time;
use crate::model::transcript::{Row, RowKind, diff_rows};
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::combobox::{ComboboxEvent, ComboboxState};
use gpui_kit::component::diff::{DiffFile, DiffState};
use gpui_kit::component::input::{self, InputEvent, InputState, TextareaState};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, App, AppContext as _, Context, Entity, ExternalPaths, FocusHandle, FontWeight, InteractiveElement as _, IntoElement, KeyBinding,
    MouseButton, ParentElement as _, PathPromptOptions, Render, SharedString, StyleRefinement, Styled as _, Subscription, Task,
    TestSupportExt as _, WeakEntity, Window, WindowBounds, div, ease_out_quint, px,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc::UnboundedReceiver;

pub(super) const SIDEBAR_WIDTH: f32 = 272.0;
pub(super) const COLUMN_WIDTH: f32 = 820.0;
const KEY_CONTEXT: &str = "OpenCode";

// Frames drawn and transcript rows shown; `--smoke` reads them to prove the window rendered engine data.
pub static FRAMES: AtomicUsize = AtomicUsize::new(0);
pub static ROWS: AtomicUsize = AtomicUsize::new(0);

gpui_kit::actions!(
    opencode,
    [
        NewSession,
        OpenSettings,
        Shortcuts,
        ToggleSidebar,
        OpenProject,
        FocusInput,
        FocusSearch,
        PreviousSession,
        NextSession,
        ChooseModel,
        CycleAgent,
        CycleAgentReverse,
        AutoAccept,
        AttachFiles,
        CycleTheme,
        CloseOverlay
    ]
);

// Installs the original app's default keybinds for everything this client implements.
pub fn bind_keys(cx: &mut App) {
    let mac = cfg!(target_os = "macos");
    let mut bindings = vec![KeyBinding::new("escape", CloseOverlay, Some(KEY_CONTEXT))];
    for bind in keymap::KEYBINDS {
        for stroke in keymap::keystrokes(bind.combo, mac) {
            // Bindings live as long as the app; the handful of keystroke strings may as well.
            let stroke: &'static str = Box::leak(stroke.into_boxed_str());
            bindings.push(match bind.command {
                Command::NewSession => KeyBinding::new(stroke, NewSession, None),
                Command::OpenSettings => KeyBinding::new(stroke, OpenSettings, None),
                Command::Shortcuts => KeyBinding::new(stroke, Shortcuts, None),
                Command::ToggleSidebar => KeyBinding::new(stroke, ToggleSidebar, None),
                Command::OpenProject => KeyBinding::new(stroke, OpenProject, None),
                Command::FocusInput => KeyBinding::new(stroke, FocusInput, None),
                Command::FocusSearch => KeyBinding::new(stroke, FocusSearch, None),
                Command::PreviousSession => KeyBinding::new(stroke, PreviousSession, None),
                Command::NextSession => KeyBinding::new(stroke, NextSession, None),
                Command::ChooseModel => KeyBinding::new(stroke, ChooseModel, None),
                Command::CycleAgent => KeyBinding::new(stroke, CycleAgent, None),
                Command::CycleAgentReverse => KeyBinding::new(stroke, CycleAgentReverse, None),
                Command::AutoAccept => KeyBinding::new(stroke, AutoAccept, None),
                Command::AttachFiles => KeyBinding::new(stroke, AttachFiles, None),
                Command::CycleTheme => KeyBinding::new(stroke, CycleTheme, None),
            });
        }
    }
    cx.bind_keys(bindings);
}

#[derive(Debug, Clone)]
pub struct Choice {
    pub(super) value: SharedString,
    pub(super) name: SharedString,
}

impl SearchableListItem for Choice {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        self.name.clone()
    }

    fn value(&self) -> &SharedString {
        &self.value
    }
}

pub(super) type AgentPicker = ComboboxState<SearchableVec<SharedString>>;
pub(super) type LanguagePicker = ComboboxState<SearchableVec<Choice>>;
pub(super) type SoundPicker = ComboboxState<SearchableVec<Choice>>;
pub(super) type VariantPicker = ComboboxState<SearchableVec<Choice>>;

// The pages of the settings screen (`settings/pages.ts`); `General` is the original's "Preferences".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Providers,
    Models,
    General,
    Appearance,
    Notifications,
    Shortcuts,
    About,
}

// Where the connect dialog stands (`dialog-connect-provider.tsx`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectStep {
    // The provider list, or the provider's login methods.
    Choose,
    ApiKey { missing: bool },
    Pending,
    Waiting { url: String, code: Option<String> },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connect {
    pub provider: Option<String>,
    pub step: ConnectStep,
    // The settings page the dialog was opened from, shown again when it closes.
    pub back: Option<SettingsTab>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Settings(SettingsTab),
    ManageModels,
    Connect(Connect),
    // Session ids.
    Rename(String),
    Delete(String),
    // Rust build: the one-time offer of a portable bash (Windows without one).
    SetupBash,
}

pub struct Launch {
    pub directory: PathBuf,
    pub persisted: Persisted,
    pub saver: Saver,
    // An overlay to start with: "settings" or "shortcuts" (smoke screenshots), "setup-bash" (first start offer).
    pub view: Option<String>,
    pub alerts: Rc<dyn Alerts>,
    // `opencode://` links this start was given.
    pub links: Vec<String>,
    // What later starts of the app hand to this one.
    pub requests: Option<UnboundedReceiver<Request>>,
}

pub struct AppView {
    pub(super) state: AppState,
    pub(super) backend: Backend,
    pub(super) persisted: Persisted,
    pub(super) saver: Saver,
    pub(super) i18n: Rc<I18n>,
    // What the transcript currently shows; replaced wholesale on every change.
    pub(super) rows: Rc<Vec<Row>>,
    pub(super) sidebar: Rc<Vec<SidebarRow>>,
    pub(super) scroller: Entity<MessageScrollerState>,
    pub(super) composer: Entity<TextareaState>,
    pub(super) search: Entity<InputState>,
    pub(super) rename: Entity<InputState>,
    // The model picker: its search field, whether it is open, the highlighted model.
    pub(super) model_search: Entity<InputState>,
    pub(super) model_menu: bool,
    pub(super) model_active: Option<String>,
    // Favourites and variants of `<state>/model.json`, shared with the TUI.
    pub(super) shared: Shared,
    pub(super) variants: Entity<VariantPicker>,
    // Search of the manage-models dialog and the Models page, and the provider groups folded there.
    pub(super) model_filter: Entity<InputState>,
    pub(super) collapsed: HashSet<String>,
    pub(super) settings_search: Entity<InputState>,
    pub(super) connect_search: Entity<InputState>,
    pub(super) api_key: Entity<InputState>,
    pub(super) agents: Entity<AgentPicker>,
    pub(super) languages: Entity<LanguagePicker>,
    pub(super) sounds: Vec<(Toggle, Entity<SoundPicker>)>,
    pub(super) alerts: Rc<dyn Alerts>,
    attention: Attention,
    pub(super) focus: FocusHandle,
    // Rows whose body is open, and the ones the user flipped away from their default.
    pub(super) expanded: HashSet<Arc<str>>,
    toggled: HashSet<Arc<str>>,
    pub(super) diffs: HashMap<Arc<str>, Entity<DiffState>>,
    pub(super) attachments: Vec<Attachment>,
    pub(super) history: History,
    pub(super) popup: Option<super::composer::Popup>,
    pub(super) question: Option<QuestionForm>,
    pub(super) question_inputs: Vec<Entity<InputState>>,
    question_watch: Vec<Subscription>,
    pub(super) overlay: Option<Overlay>,
    // Theme shown while hovering the theme list.
    pub(super) preview: Option<String>,
    pub(super) todos_collapsed: bool,
    // The last session of a project is reopened once, when its session list first arrives.
    pub(super) restore: bool,
    _subscriptions: Vec<Subscription>,
    _pump: Task<()>,
    _requests: Option<Task<()>>,
    _ticker: Task<()>,
}

// Georgia ships with Windows and macOS; elsewhere the system substitutes a serif.
const SERIF: &str = "Georgia";

pub(super) fn now_ms() -> u64 {
    opencode_storage::id::now_ms()
}

pub(super) fn permission_key(id: u64) -> Arc<str> {
    format!("permission-{id}").into()
}

pub fn geometry(bounds: WindowBounds) -> WindowGeometry {
    let (rect, maximized) = match bounds {
        WindowBounds::Windowed(rect) => (rect, false),
        WindowBounds::Maximized(rect) | WindowBounds::Fullscreen(rect) => (rect, true),
    };
    WindowGeometry {
        x: f32::from(rect.origin.x),
        y: f32::from(rect.origin.y),
        width: f32::from(rect.size.width),
        height: f32::from(rect.size.height),
        maximized,
    }
}

// A row's click handler that runs on the view, if it is still there.
fn callback(view: &WeakEntity<AppView>, run: impl Fn(&mut AppView, &mut Window, &mut Context<AppView>) + 'static) -> Callback {
    let view = view.clone();
    Rc::new(move |_, window, cx| {
        let _ = view.update(cx, |this, cx| run(this, window, cx));
    })
}

fn language_code(persisted: &Persisted) -> &str {
    persisted.settings.language.as_deref().unwrap_or_else(|| i18n::detect())
}

// "None" first, then the original's sounds in its order (`settings-general.tsx:219-220`).
pub(super) fn sound_choices(i18n: &I18n) -> Vec<Choice> {
    std::iter::once(attention::NO_SOUND)
        .chain(attention::SOUNDS.iter().map(|sound| (sound.id, sound.label)))
        .map(|(id, label)| Choice { value: id.into(), name: i18n.t(label).to_string().into() })
        .collect()
}

// A switched-off sound, or one the table does not know, shows as "None" (`settings-general.tsx:232`).
pub(super) fn sound_selection(settings: &crate::model::settings::Settings, toggle: Toggle) -> SharedString {
    match settings.sound_id(toggle).filter(|id| settings.get(toggle) && attention::sound(id).is_some()) {
        Some(id) => id.to_string().into(),
        None => attention::NO_SOUND.0.into(),
    }
}

impl AppView {
    pub fn new(backend: Backend, mut events: UnboundedReceiver<Event>, launch: Launch, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let i18n = Rc::new(I18n::new(language_code(&launch.persisted)));
        let composer = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 10).submit_on_enter(true));
        let search = cx.new(|cx| InputState::new(window, cx));
        let rename = cx.new(|cx| InputState::new(window, cx));
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let model_search = cx.new(|cx| InputState::new(window, cx));
        let model_filter = cx.new(|cx| InputState::new(window, cx));
        let settings_search = cx.new(|cx| InputState::new(window, cx));
        let connect_search = cx.new(|cx| InputState::new(window, cx));
        let api_key = cx.new(|cx| InputState::new(window, cx).masked(true));
        let variants: Entity<VariantPicker> = cx.new(|cx| ComboboxState::new(SearchableVec::new(Vec::<Choice>::new()), vec![], window, cx));
        let shared = Shared::load(&crate::model::models::file(backend.state_dir()));
        let agents: Entity<AgentPicker> =
            cx.new(|cx| ComboboxState::new(SearchableVec::new(Vec::<SharedString>::new()), vec![], window, cx).searchable(true));
        let locales: Vec<Choice> = i18n::locales().iter().map(|l| Choice { value: l.code.into(), name: l.label.into() }).collect();
        let languages: Entity<LanguagePicker> = cx.new(|cx| {
            let mut picker = ComboboxState::new(SearchableVec::new(locales), vec![], window, cx).searchable(true);
            picker.set_selected_values(&[SharedString::from(i18n.code())], window, cx);
            picker
        });
        let sounds: Vec<(Toggle, Entity<SoundPicker>)> = Toggle::SOUNDS
            .into_iter()
            .map(|toggle| {
                let selected = sound_selection(&launch.persisted.settings, toggle);
                let picker = cx.new(|cx| {
                    let mut picker = ComboboxState::new(SearchableVec::new(sound_choices(&i18n)), vec![], window, cx).searchable(true);
                    picker.set_selected_values(&[selected], window, cx);
                    picker
                });
                (toggle, picker)
            })
            .collect();

        let mut subscriptions = vec![
            cx.observe(&scroller, |_, _, cx| cx.notify()),
            cx.subscribe_in(&composer, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { shift: false, .. } => this.submit(window, cx),
                InputEvent::Change => this.composer_changed(cx),
                InputEvent::Blur => this.save_draft(cx),
                _ => {}
            }),
            cx.subscribe_in(&search, window, |this, search, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.state.filter = search.read(cx).value().to_string();
                    this.sync(cx);
                }
            }),
            cx.subscribe_in(&rename, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.confirm_rename(window, cx);
                }
            }),
            cx.subscribe_in(&model_search, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.model_search_changed(cx),
                InputEvent::PressEnter { .. } => this.choose_active_model(window, cx),
                _ => {}
            }),
            cx.observe(&model_filter, |_, _, cx| cx.notify()),
            cx.observe(&settings_search, |_, _, cx| cx.notify()),
            cx.observe(&connect_search, |_, _, cx| cx.notify()),
            cx.subscribe_in(&api_key, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit_key(window, cx);
                }
            }),
            cx.subscribe_in(&variants, window, |this, _, event: &ComboboxEvent<SearchableVec<Choice>>, window, cx| {
                if let ComboboxEvent::Change(values) = event {
                    this.choose_variant(values.first().map(|v| v.to_string()), window, cx);
                }
            }),
            cx.subscribe_in(&agents, window, |this, _, event: &ComboboxEvent<SearchableVec<SharedString>>, window, cx| {
                if let ComboboxEvent::Change(values) = event {
                    let picked = values.first().map(|v| v.to_string());
                    if picked.as_deref() != this.state.effective_agent() {
                        this.state.agent = picked;
                        this.remember();
                    }
                    if values.is_empty() && this.state.effective_agent().is_some() {
                        this.select_in_pickers(window, cx);
                    }
                    cx.notify();
                }
            }),
            cx.subscribe_in(&languages, window, |this, _, event: &ComboboxEvent<SearchableVec<Choice>>, window, cx| {
                if let ComboboxEvent::Change(values) = event
                    && let Some(code) = values.first()
                {
                    this.set_language(code, window, cx);
                }
            }),
            cx.observe_window_bounds(window, |this, window, _| {
                if this.persisted.set_window(geometry(window.window_bounds())) {
                    this.saver.save(&this.persisted);
                }
            }),
            cx.observe_window_appearance(window, |this, window, cx| this.apply_theme(window, cx)),
            cx.on_app_quit(|this, cx| {
                this.save_draft(cx);
                this.saver.save(&this.persisted);
                this.saver.flush();
                async {}
            }),
        ];
        for (toggle, picker) in &sounds {
            let toggle = *toggle;
            subscriptions.push(cx.subscribe_in(picker, window, move |this, _, event: &ComboboxEvent<SearchableVec<Choice>>, window, cx| {
                if let ComboboxEvent::Change(values) = event {
                    this.choose_sound(toggle, values.first().map(|v| v.to_string()), window, cx);
                }
            }));
        }

        // A click on one of this app's notifications brings its session forward.
        let (view, handle) = (cx.weak_entity(), window.window_handle());
        cx.on_system_notification_response(move |response, cx| {
            let Some(target) = Target::from_tag(&response.tag) else { return };
            let _ = handle.update(cx, |_, window, cx| view.update(cx, |view, cx| view.reveal(&target, window, cx)));
        });

        let requests = launch.requests.map(|mut requests| {
            cx.spawn_in(window, async move |this, cx| {
                while let Some(request) = requests.recv().await {
                    if this.update_in(cx, |this, window, cx| this.handle_request(request, window, cx)).is_err() {
                        break;
                    }
                }
            })
        });

        // Engine events arrive from tokio threads; apply everything queued in one update.
        let pump = cx.spawn_in(window, async move |this, cx| {
            while let Some(first) = events.recv().await {
                let mut batch = vec![first];
                while let Ok(next) = events.try_recv() {
                    batch.push(next);
                }
                if this.update_in(cx, |this, window, cx| this.apply(batch, window, cx)).is_err() {
                    break;
                }
            }
        });

        // A retry row counts down once a second (`SessionRetry`); nothing else needs a clock.
        let ticker = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                let alive = this.update(cx, |this, cx| {
                    if this.rows.iter().any(|row| matches!(row.kind, RowKind::Retry { .. })) {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });

        let history = History::new(launch.persisted.history.clone());
        let overlay = match launch.view.as_deref() {
            Some("settings") => Some(Overlay::Settings(SettingsTab::General)),
            Some("shortcuts") => Some(Overlay::Settings(SettingsTab::Shortcuts)),
            Some("providers") => Some(Overlay::Settings(SettingsTab::Providers)),
            Some("models") => Some(Overlay::Settings(SettingsTab::Models)),
            Some("manage-models") => Some(Overlay::ManageModels),
            Some("connect") => Some(Overlay::Connect(Connect { provider: None, step: ConnectStep::Choose, back: None })),
            Some("setup-bash") => Some(Overlay::SetupBash),
            _ => None,
        };
        let mut view = Self {
            state: AppState::default(),
            backend,
            persisted: launch.persisted,
            saver: launch.saver,
            i18n,
            rows: Rc::new(Vec::new()),
            sidebar: Rc::new(Vec::new()),
            scroller,
            composer,
            search,
            rename,
            model_search,
            model_menu: false,
            model_active: None,
            shared,
            variants,
            model_filter,
            collapsed: HashSet::new(),
            settings_search,
            connect_search,
            api_key,
            agents,
            languages,
            sounds,
            alerts: launch.alerts,
            attention: Attention::default(),
            focus: cx.focus_handle(),
            expanded: HashSet::new(),
            toggled: HashSet::new(),
            diffs: HashMap::new(),
            attachments: Vec::new(),
            history,
            popup: None,
            question: None,
            question_inputs: Vec::new(),
            question_watch: Vec::new(),
            overlay,
            preview: None,
            todos_collapsed: false,
            restore: false,
            _subscriptions: subscriptions,
            _pump: pump,
            _requests: requests,
            _ticker: ticker,
        };
        view.apply_theme(window, cx);
        view.apply_placeholders(window, cx);
        view.open_project(launch.directory, window, cx);
        view.composer.update(cx, |composer, cx| composer.focus(window, cx));
        view.follow_links(&launch.links, window, cx);
        view
    }

    pub(super) fn t(&self, key: &str) -> SharedString {
        self.i18n.t(key).into()
    }

    pub(super) fn apply_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        theme::apply(&self.persisted.settings, self.preview.as_deref(), Some(window), cx);
        cx.notify();
    }

    fn apply_placeholders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (prompt, search) = (self.t("prompt.placeholder.simple"), self.t("home.sessions.search.placeholder"));
        self.composer.update(cx, |input, cx| input.set_placeholder(prompt, window, cx));
        self.search.update(cx, |input, cx| input.set_placeholder(search, window, cx));
        let placeholders = [
            (self.model_search.clone(), "dialog.model.search.placeholder"),
            (self.model_filter.clone(), "dialog.model.search.placeholder"),
            (self.settings_search.clone(), "settings.search.placeholder"),
            (self.connect_search.clone(), "dialog.provider.search.placeholder"),
            (self.api_key.clone(), "provider.connect.apiKey.placeholder"),
        ];
        for (input, key) in placeholders {
            let text = self.t(key);
            input.update(cx, |input, cx| input.set_placeholder(text, window, cx));
        }
        self.refresh_variants(window, cx);
    }

    pub(super) fn set_language(&mut self, code: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.i18n.code() == code {
            return;
        }
        self.i18n = Rc::new(I18n::new(code));
        self.persisted.settings.language = Some(self.i18n.code().to_string());
        self.saver.save(&self.persisted);
        self.apply_placeholders(window, cx);
        for (toggle, picker) in self.sounds.clone() {
            let (choices, selected) = (sound_choices(&self.i18n), sound_selection(&self.persisted.settings, toggle));
            picker.update(cx, |picker, cx| {
                picker.set_items(SearchableVec::new(choices), window, cx);
                picker.set_selected_values(&[selected], window, cx);
            });
        }
        cx.notify();
    }

    pub(super) fn save_settings(&mut self, cx: &mut Context<Self>) {
        self.saver.save(&self.persisted);
        self.sync(cx);
    }

    fn open_project(&mut self, directory: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.save_draft(cx);
        self.state.open_project(directory.clone());
        let saved = self.persisted.project(&directory);
        let settings = &self.persisted.settings;
        self.state.model = saved.model.or_else(|| settings.default_model.clone());
        self.state.agent = saved.agent.or_else(|| settings.default_agent.clone());
        self.restore = true;
        self.toggled.clear();
        self.diffs.clear();
        self.attachments.clear();
        self.popup = None;
        window.set_window_title(&self.state.window_title());
        self.backend.load_project(directory);
        self.load_draft(window, cx);
        self.sync(cx);
    }

    pub(super) fn remember(&mut self) {
        let (session, model, agent) = (self.state.current.clone(), self.state.model.clone(), self.state.agent.clone());
        let changed = self.persisted.update_project(&self.state.directory, |project| {
            if session.is_some() {
                project.last_session = session;
            }
            project.model = model;
            project.agent = agent;
        });
        if changed {
            self.saver.save(&self.persisted);
        }
    }

    pub(super) fn apply(&mut self, batch: Vec<Event>, window: &mut Window, cx: &mut Context<Self>) {
        let mut open = None;
        let had_session = self.state.current.is_some();
        for event in batch {
            let catalog = matches!(event, Event::Catalog { .. });
            let opened = matches!(event, Event::Opened { .. });
            let sessions = matches!(&event, Event::Sessions { directory, .. } if *directory == self.state.directory);
            match &event {
                Event::Permission(request) if self.persisted.settings.auto_accept => {
                    self.backend.reply(request.id, opencode_session::Reply::Once);
                    continue;
                }
                Event::Permission(request) => {
                    if let Some(patch) = &request.diff {
                        self.prepare_diff(permission_key(request.id), patch, cx);
                    }
                }
                Event::PermissionResolved(id) => drop(self.diffs.remove(&permission_key(*id))),
                Event::Forked { directory, session } if *directory == self.state.directory => open = Some(session.id.clone()),
                Event::Attention(signal) => {
                    self.alert(signal, window, cx);
                    continue;
                }
                Event::Login { provider_id, step } => {
                    self.login_progress(provider_id, step.clone(), window, cx);
                    continue;
                }
                _ => {}
            }
            self.state.apply(event);
            if catalog {
                self.refresh_pickers(window, cx);
            }
            if opened {
                self.select_in_pickers(window, cx);
            }
            if sessions && std::mem::take(&mut self.restore) {
                let last = self.persisted.project(&self.state.directory).last_session;
                if let Some(id) = last.filter(|id| self.state.sessions.iter().any(|s| s.id.as_ref() == id)) {
                    self.open_session(&id, window, cx);
                }
            }
        }
        if let Some(id) = open {
            self.open_session(&id, window, cx);
        }
        // The session on screen was deleted: its draft slot is gone, show the new-session one.
        if had_session && self.state.current.is_none() {
            self.load_draft(window, cx);
        }
        self.sync_question(window, cx);
        self.sync(cx);
    }

    // A request answered automatically (auto-accept) or already gone is not announced, like `permission.autoResponds`.
    fn alert(&mut self, signal: &Signal, window: &Window, cx: &mut Context<Self>) {
        let waiting = match signal.kind {
            Kind::Permission(id) => self.state.permissions.iter().any(|p| p.id == id),
            Kind::Question(id) => self.state.questions.iter().any(|q| q.id == id),
            Kind::Idle | Kind::Error => true,
        };
        if !waiting {
            return;
        }
        let alert = self.attention.decide(signal, &self.persisted.settings, &self.i18n, window.is_window_active(), now_ms());
        if let Some(sound) = alert.sound.and_then(attention::sound) {
            self.alerts.play(sound);
        }
        if let Some(notification) = &alert.notification {
            self.alerts.notify(notification, cx);
        }
    }

    // A notification click: the window comes forward on the session it named, in its project.
    pub(super) fn reveal(&mut self, target: &Target, window: &mut Window, cx: &mut Context<Self>) {
        window.activate_window();
        if target.directory != self.state.directory {
            if let Some(id) = &target.session_id {
                self.persisted.update_project(&target.directory, |project| project.last_session = Some(id.clone()));
            }
            self.open_project(target.directory.clone(), window, cx);
            return;
        }
        if let Some(id) = &target.session_id {
            self.open_session(id, window, cx);
        }
    }

    // A later start of the app: bring the window forward, then open what it was started with.
    pub(super) fn handle_request(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        window.activate_window();
        if let Some(directory) = request.directory.filter(|directory| *directory != self.state.directory) {
            self.open_project(directory, window, cx);
        }
        self.follow_links(&request.links, window, cx);
    }

    // `layout.tsx:1252-1270`: open-project links open the project, new-session links a new session with the prompt filled in.
    pub(super) fn follow_links(&mut self, links: &[String], window: &mut Window, cx: &mut Context<Self>) {
        for link in links.iter().filter_map(|link| deeplink::parse(link)) {
            let (directory, prompt) = match link {
                Link::OpenProject { directory } => (directory, None),
                Link::NewSession { directory, prompt } => (directory, Some(prompt)),
            };
            let path = std::path::absolute(&directory).ok().filter(|path| path.is_dir());
            let Some(path) = path else {
                self.state.notice = Some(Notice { key: "desktop.rs.error.openProject", error: directory });
                cx.notify();
                continue;
            };
            let other = path != self.state.directory;
            if other {
                self.open_project(path, window, cx);
            }
            if let Some(prompt) = prompt {
                // The project's last session is not reopened over the new one.
                self.restore = false;
                if !other {
                    self.new_session(window, cx);
                }
                if let Some(prompt) = prompt {
                    self.set_composer(&prompt, window, cx);
                }
            }
        }
    }

    fn sync_question(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.state.current_question().cloned();
        match (current, &self.question) {
            (Some(view), Some(form)) if form.id == view.id => {}
            (Some(view), _) => {
                let placeholder = self.t("ui.question.custom.placeholder");
                self.question_inputs =
                    view.questions.iter().map(|_| cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.clone()))).collect();
                // Typing an answer changes whether the card can be submitted.
                self.question_watch = self.question_inputs.iter().map(|input| cx.observe(input, |_, _, cx| cx.notify())).collect();
                self.question = Some(QuestionForm::new(&view));
            }
            (None, _) => {
                self.question = None;
                self.question_inputs.clear();
                self.question_watch.clear();
            }
        }
    }

    fn refresh_pickers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let agents: Vec<SharedString> = self.state.catalog.agents.iter().map(|a| SharedString::from(a.name.clone())).collect();
        self.agents.update(cx, |picker, cx| picker.set_items(SearchableVec::new(agents), window, cx));
        self.select_in_pickers(window, cx);
    }

    pub(super) fn select_in_pickers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let agent: Vec<SharedString> = self.state.effective_agent().map(|a| SharedString::from(a.to_string())).into_iter().collect();
        self.agents.update(cx, |picker, cx| picker.set_selected_values(&agent, window, cx));
        self.refresh_variants(window, cx);
    }

    fn open_by_default(&self, row: &Row) -> bool {
        let settings = &self.persisted.settings;
        match &row.kind {
            RowKind::Tool(view) => match view.tool.as_ref() {
                "bash" | "shell" => settings.shell_expanded,
                "edit" | "write" | "patch" | "apply_patch" => settings.edit_expanded,
                _ => false,
            },
            _ => false,
        }
    }

    // Brings the list in line with the state: a fresh list when nothing carries over, otherwise a tail patch.
    pub(super) fn sync(&mut self, cx: &mut Context<Self>) {
        self.sidebar = Rc::new(self.state.sidebar());
        let mut rows = self.state.visible();
        if !self.persisted.settings.show_reasoning {
            rows.retain(|row| !matches!(row.kind, RowKind::Reasoning { .. }));
        }
        // Rust build: a run in flight always shows what it is doing at the end of the conversation.
        if self.state.current_running() && !matches!(rows.last().map(|row| &row.kind), Some(RowKind::Retry { .. })) {
            let label = crate::model::transcript::working_label(&rows);
            rows.push(Row::new("working", RowKind::Working { label }));
        }
        let expanded: HashSet<Arc<str>> =
            rows.iter().filter(|row| self.open_by_default(row) != self.toggled.contains(&row.id)).map(|row| row.id.clone()).collect();
        let patch = diff_rows(&self.rows, &rows);
        let reopened: Vec<usize> = rows
            .iter()
            .take(patch.keep)
            .enumerate()
            .filter(|(index, row)| expanded.contains(&row.id) != self.expanded.contains(&row.id) && !patch.remeasure.contains(index))
            .map(|(index, _)| index)
            .collect();
        if !patch.is_noop() || !reopened.is_empty() {
            let count = rows.len();
            self.scroller.update(cx, |scroller, cx| {
                if patch.keep == 0 {
                    scroller.reset(count, cx);
                    return;
                }
                if patch.remove > 0 || patch.insert > 0 {
                    scroller.splice(patch.keep..patch.keep + patch.remove, patch.insert, cx);
                }
                for index in patch.remeasure.iter().chain(&reopened) {
                    scroller.remeasure_items(*index..*index + 1, cx);
                }
            });
            self.rows = Rc::new(rows);
            ROWS.store(self.rows.len(), Ordering::Relaxed);
        }
        self.expanded = expanded;
        let missing: Vec<Arc<str>> = self.expanded.iter().filter(|id| !self.diffs.contains_key(*id)).cloned().collect();
        for id in missing {
            self.ensure_diff(&id, cx);
        }
        cx.notify();
    }

    pub(super) fn prepare_diff(&mut self, key: Arc<str>, patch: &str, cx: &mut Context<Self>) {
        if let Ok(files) = DiffFile::parse(patch)
            && !files.is_empty()
        {
            self.diffs.insert(key, cx.new(|cx| DiffState::new(files, cx)));
        }
    }

    fn ensure_diff(&mut self, id: &Arc<str>, cx: &mut Context<Self>) {
        if self.diffs.contains_key(id) {
            return;
        }
        let patch = self.rows.iter().find(|row| row.id == *id).and_then(|row| match &row.kind {
            RowKind::Tool(view) => view.diff.clone(),
            _ => None,
        });
        if let Some(patch) = patch {
            self.prepare_diff(id.clone(), &patch, cx);
        }
    }

    fn toggle(&mut self, id: Arc<str>, cx: &mut Context<Self>) {
        if !self.toggled.remove(&id) {
            self.toggled.insert(id);
        }
        self.sync(cx);
    }

    pub(super) fn open_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.current.as_deref() == Some(id) {
            return;
        }
        self.save_draft(cx);
        self.state.select(id);
        if !self.state.is_running(id) {
            self.backend.open_session(id.to_string());
        }
        self.toggled.clear();
        self.diffs.retain(|key, _| key.starts_with("permission-"));
        self.attachments.clear();
        self.load_draft(window, cx);
        self.remember();
        self.sync_question(window, cx);
        self.sync(cx);
    }

    pub(super) fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_draft(cx);
        self.state.new_session();
        self.toggled.clear();
        self.attachments.clear();
        self.load_draft(window, cx);
        self.composer.update(cx, |composer, cx| composer.focus(window, cx));
        self.sync_question(window, cx);
        self.sync(cx);
    }

    pub(super) fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = &self.state.current {
            self.backend.cancel(id);
        }
        cx.notify();
    }

    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = Some(self.t("common.open"));
        let picked = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            if let Some(directory) = paths.into_iter().next() {
                let _ = this.update_in(cx, |this, window, cx| this.open_project(directory, window, cx));
            }
        })
        .detach();
    }

    pub(super) fn toggle_auto_accept(&mut self, cx: &mut Context<Self>) {
        let on = !self.persisted.settings.auto_accept;
        self.persisted.settings.auto_accept = on;
        // Requests already on screen were waiting for exactly this.
        if on {
            for request in std::mem::take(&mut self.state.permissions) {
                self.backend.reply(request.id, opencode_session::Reply::Once);
            }
        }
        self.save_settings(cx);
    }

    pub(super) fn cycle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let themes = crate::model::theme::themes();
        let at = themes.iter().position(|t| t.id == self.persisted.settings.theme().id).unwrap_or(0);
        self.persisted.settings.theme = themes[(at + 1) % themes.len()].id.clone();
        self.preview = None;
        self.apply_theme(window, cx);
        self.save_settings(cx);
    }

    pub(super) fn cycle_agent(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.cycle_agent(delta).is_some() {
            self.select_in_pickers(window, cx);
            self.remember();
            cx.notify();
        }
    }

    pub(super) fn run(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::NewSession => self.new_session(window, cx),
            Command::OpenSettings => self.open_overlay(Overlay::Settings(SettingsTab::General), window, cx),
            Command::Shortcuts => self.open_overlay(Overlay::Settings(SettingsTab::Shortcuts), window, cx),
            Command::ToggleSidebar => {
                self.persisted.settings.sidebar = !self.persisted.settings.sidebar;
                self.save_settings(cx);
            }
            Command::OpenProject => self.choose_folder(window, cx),
            Command::FocusInput => self.composer.update(cx, |composer, cx| composer.focus(window, cx)),
            Command::FocusSearch => {
                self.persisted.settings.sidebar = true;
                self.search.update(cx, |search, cx| search.focus(window, cx));
                cx.notify();
            }
            Command::PreviousSession | Command::NextSession => {
                let delta = if command == Command::NextSession { 1 } else { -1 };
                if let Some(id) = self.state.neighbor(delta) {
                    self.open_session(&id, window, cx);
                }
            }
            Command::ChooseModel => self.set_model_menu(true, window, cx),
            Command::CycleAgent => self.cycle_agent(1, window, cx),
            Command::CycleAgentReverse => self.cycle_agent(-1, window, cx),
            Command::AutoAccept => self.toggle_auto_accept(cx),
            Command::AttachFiles => self.choose_files(window, cx),
            Command::CycleTheme => self.cycle_theme(window, cx),
        }
    }

    fn usage_label(&self) -> Option<(SharedString, SharedString)> {
        let usage = self.state.current_usage()?;
        let tokens = time::compact_count(usage.total());
        let percent = self.state.context_percent().map(|p| format!("{p}%"));
        let cost = format!("${:.2}", usage.cost);
        let short: Vec<&str> = [Some(tokens.as_str()), percent.as_deref(), (usage.cost > 0.0).then_some(cost.as_str())].into_iter().flatten().collect();
        let long = format!(
            "{}: {tokens} · {}: {} · {}: {cost}",
            self.t("context.usage.tokens"),
            self.t("context.usage.usage"),
            percent.as_deref().unwrap_or("–"),
            self.t("context.usage.cost"),
        );
        Some((short.join(" · ").into(), long.into()))
    }

    fn render_title_bar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let menu = self.render_app_menu(window, cx);
        let theme = cx.theme();
        let session = self.state.current_session().filter(|_| !self.state.current_running()).map(|s| s.id.to_string());
        let status: Option<SharedString> = match self.state.current_status() {
            RunStatus::Retry { attempt, .. } => Some(
                format!("{} · {}", self.t("ui.sessionTurn.retry.retrying"), self.i18n.tf("ui.sessionTurn.retry.attempt", &[("attempt", &attempt.to_string())]))
                    .into(),
            ),
            _ => None,
        };
        let action = |id: &'static str, icon: IconName, tooltip: SharedString| Button::new(id).ghost().xsmall().icon(icon).tooltip(tooltip);
        TitleBar::new()
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .text_sm()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(menu)
                    .child(
                        action("toggle-sidebar", IconName::PanelLeft, self.t("command.sidebar.toggle"))
                            .on_click(cx.listener(|this, _, window, cx| this.run(Command::ToggleSidebar, window, cx))),
                    )
                    .child(div().flex_shrink_0().font_weight(FontWeight::MEDIUM).child(self.state.project_name()))
                    .child(div().text_color(theme.muted_foreground).child("/"))
                    .child(div().min_w_0().truncate().child(if self.state.current.is_some() {
                        SharedString::from(self.state.current_title())
                    } else {
                        self.t("command.session.new")
                    }))
                    .when_some(session, |this, id| {
                        let (rename, fork, delete) = (id.clone(), id.clone(), id);
                        this.child(
                            h_flex()
                                .flex_shrink_0()
                                .gap_0p5()
                                .child(action("session-rename", IconName::Replace, self.t("common.rename")).on_click(cx.listener(
                                    move |this, _, window, cx| this.open_overlay(Overlay::Rename(rename.clone()), window, cx),
                                )))
                                .child(action("session-fork", IconName::Copy, self.t("ui.message.forkMessage")).on_click(cx.listener(
                                    move |this, _, _, _| this.backend.fork(this.state.directory.clone(), fork.clone(), None),
                                )))
                                .child(action("session-delete", IconName::Delete, self.t("common.delete")).on_click(cx.listener(
                                    move |this, _, window, cx| this.open_overlay(Overlay::Delete(delete.clone()), window, cx),
                                ))),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_2()
                    .px_3()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .when_some(status, |this, status| this.child(status))
                    .when(self.state.current_running(), |this| this.child(super::motion::pulse("title-pulse", 7.0, cx)))
                    .when_some(self.usage_label(), |this, (short, long)| this.child(Button::new("usage").ghost().xsmall().label(short).tooltip(long)))
                    .child(self.state.model_label().map_or_else(|| self.t("toast.model.none.title"), SharedString::from))
                    .child(
                        action("open-settings", IconName::Settings, self.t("command.settings.open"))
                            .on_click(cx.listener(|this, _, window, cx| this.run(Command::OpenSettings, window, cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_transcript(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if self.rows.is_empty() {
            let hint: SharedString = if self.state.loading {
                self.t("session.messages.loading")
            } else {
                self.state.directory.to_string_lossy().into_owned().into()
            };
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .when(self.state.loading, |this| this.child(Spinner::new()))
                // Rust build: a serif greeting, as in the warm "clay" look; CJK text falls back to the system font.
                .when(!self.state.loading, |this| {
                    this.child(div().text_3xl().font_family(SERIF).font_weight(FontWeight::NORMAL).child(self.t("session.new.title")))
                })
                .child(div().text_sm().text_color(theme.muted_foreground).child(hint))
                .into_any_element();
        }
        let rows = self.rows.clone();
        let expanded = Rc::new(self.expanded.clone());
        let diffs = Rc::new(self.diffs.clone());
        let children: Rc<HashMap<Arc<str>, ChildActivity>> = Rc::new(
            rows.iter()
                .filter_map(|row| match &row.kind {
                    RowKind::Tool(view) => view.child.clone(),
                    _ => None,
                })
                .filter_map(|id| self.state.child(&id).cloned().map(|activity| (id, activity)))
                .collect(),
        );
        let i18n = self.i18n.clone();
        let idle = !self.state.current_running() && self.state.current_parent().is_none();
        let view = cx.weak_entity();
        MessageScroller::new("transcript", self.scroller.clone(), move |index, _, cx| {
            let Some(row) = rows.get(index) else {
                return div().into_any_element();
            };
            let id = row.id.clone();
            let toggle = callback(&view, move |this, _, cx| this.toggle(id.clone(), cx));
            let child_id = match &row.kind {
                RowKind::Tool(view) => view.child.clone(),
                _ => None,
            };
            let open_child = child_id.clone().map(|child| callback(&view, move |this, window, cx| this.open_session(&child, window, cx)));
            let message = (idle && matches!(row.kind, RowKind::User { .. }) && row.id.starts_with("msg")).then(|| row.id.clone());
            let fork = message.clone().map(|message| {
                callback(&view, move |this, _, _| {
                    if let Some(session) = this.state.current.clone() {
                        this.backend.fork(this.state.directory.clone(), session, Some(message.to_string()));
                    }
                })
            });
            let revert = message.map(|message| callback(&view, move |this, window, cx| this.revert_to(&message, window, cx)));
            let context = RowContext {
                expanded: expanded.contains(&row.id),
                diff: diffs.get(&row.id).cloned(),
                toggle,
                i18n: i18n.clone(),
                child: child_id.and_then(|id| children.get(&id).cloned()),
                open_child,
                fork,
                revert,
            };
            let content = render_row(row, context, cx);
            // Rust build: the newest rows ease in as they arrive; older ones scrolled back into view stay still. The
            // animation sits inside the centred column, so animated rows line up with the composer like the rest.
            let content = if index + 2 >= rows.len() {
                super::motion::enter(SharedString::from(format!("enter-{}", row.id)), 8.0, content)
            } else {
                content
            };
            let body = div().id(SharedString::from(format!("column-{}", row.id))).test_support().w_full().min_w_0().max_w(px(COLUMN_WIDTH)).child(content);
            div().w_full().flex().justify_center().child(body).into_any_element()
        })
        .with_list_style(StyleRefinement::default().px_4().pt_4().pb_6())
        .with_row_style(StyleRefinement::default().pb_4())
        .with_bottom_fade(theme.background)
        .with_jump_button_label(self.t("session.messages.jumpToLatest"))
        .size_full()
        .into_any_element()
    }

    fn render_notices(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Provider notices name the provider where others name the error.
        let notice = self.state.notice.as_ref().map(|notice| self.i18n.tf(notice.key, &[("error", &notice.error), ("provider", &notice.error)]));
        // An empty model list after the catalog arrived means nothing is configured.
        let unconfigured = (self.state.catalog.models.is_empty() && !self.state.catalog.agents.is_empty())
            .then(|| self.i18n.t("desktop.rs.catalog.noProviders").to_string());
        let message = notice.or_else(|| self.state.catalog.warning.clone()).or(unconfigured)?;
        let dismissible = self.state.notice.is_some();
        let alert = match self.state.notice.as_ref().map(|notice| notice.key) {
            Some("desktop.rs.setup.bash.started") => Alert::info("notice", SharedString::from(message)),
            Some("desktop.rs.setup.bash.ready" | "provider.connect.toast.connected.title" | "provider.disconnect.toast.disconnected.title") => {
                Alert::success("notice", SharedString::from(message))
            }
            _ => Alert::warning("notice", SharedString::from(message)),
        };
        let alert = alert.when(dismissible, |alert| {
            alert.on_close(cx.listener(|this, _, _, cx| {
                this.state.notice = None;
                cx.notify();
            }))
        });
        Some(div().w_full().max_w(px(COLUMN_WIDTH)).child(alert).into_any_element())
    }
}

macro_rules! commands {
    ($element:expr, $cx:expr, [$($action:ident),* $(,)?]) => {
        $element$(.on_action($cx.listener(|this: &mut AppView, _: &$action, window, cx| this.run(Command::$action, window, cx))))*
    };
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        FRAMES.fetch_add(1, Ordering::Relaxed);
        // Shortcuts are dispatched from the focused element; with nothing focused they would go nowhere.
        if window.focused(cx).is_none() {
            self.focus.focus(window, cx);
        }
        let (background, foreground, drop_target) = (cx.theme().background, cx.theme().foreground, cx.theme().drop_target);
        let column = || div().w_full().max_w(px(COLUMN_WIDTH));
        let root = v_flex()
            .id("app")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .on_action(cx.listener(|this, _: &CloseOverlay, window, cx| this.close_overlay(window, cx)))
            // An open overlay takes Escape before the prompt below it does.
            .capture_action(cx.listener(|this, _: &input::Escape, window, cx| {
                if this.overlay.is_some() {
                    this.close_overlay(window, cx);
                    cx.stop_propagation();
                }
            }))
            // Text inputs bind these two keys themselves (find, code actions); here they keep the original app's meaning.
            .capture_action(cx.listener(|this, _: &input::Search, window, cx| {
                this.run(Command::FocusSearch, window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &input::ToggleCodeActions, window, cx| {
                this.run(Command::CycleAgent, window, cx);
                cx.stop_propagation();
            }))
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(drop_target))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.attach_paths(paths.paths(), cx)));
        let root = super::menu::on_menu_actions(root, cx);
        commands!(
            root,
            cx,
            [
                NewSession,
                OpenSettings,
                Shortcuts,
                ToggleSidebar,
                OpenProject,
                FocusInput,
                FocusSearch,
                PreviousSession,
                NextSession,
                ChooseModel,
                CycleAgent,
                CycleAgentReverse,
                AutoAccept,
                AttachFiles,
                CycleTheme
            ]
        )
        .child(self.render_title_bar(window, cx))
        .child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                // Rust build: an opened sidebar slides in from the edge.
                .when(self.persisted.settings.sidebar, |this| {
                    this.child(
                        div()
                            .relative()
                            .h_full()
                            .flex_shrink_0()
                            .child(self.render_sidebar(cx))
                            .with_animation("sidebar-in", Animation::new(std::time::Duration::from_millis(220)).with_easing(ease_out_quint()), |this, t| {
                                this.opacity(t).left(px((t - 1.0) * 16.0))
                            }),
                    )
                })
                .child(
                    v_flex().flex_1().min_w_0().h_full().child(div().id("transcript-area").test_support().flex_1().min_h_0().child(self.render_transcript(cx))).child(
                        v_flex()
                            .w_full()
                            .items_center()
                            .px_4()
                            .pb_4()
                            .gap_2()
                            // Clicks here must not start a window drag or fall through to the transcript.
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .children(self.render_notices(cx))
                            .children(self.render_revert(cx).map(|dock| column().child(dock)))
                            .children(self.render_todos(cx).map(|dock| column().child(dock)))
                            .children(self.render_request(cx).map(|dock| column().child(dock)))
                            .children(self.render_popup(cx).map(|popup| column().child(popup)))
                            .child(column().id("composer-column").test_support().child(self.render_composer(cx))),
                    ),
                )
                .relative()
                .children(self.render_settings_page(cx)),
        )
        .children(self.render_overlay(cx))
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
