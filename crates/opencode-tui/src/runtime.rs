use crate::app::{Action, App, EditorPurpose, Effect, Init, ToastKind};
use crate::backend::{Backend, LocalBackend, RemoteBackend, Turn};
use crate::catalog::{self, Catalog};
use crate::config::TuiConfig;
use crate::stash::{Frecency, Stash};
use crate::state::{History, Kv, ModelState, Pins};
use crate::terminal::{self, Caps};
use crate::theme::Registry;
use crate::{tips, view};
use opencode_core::flag::{self, Lookup};
use opencode_core::paths::Paths;
use opencode_session::{Engine, PermissionAnswer};
use opencode_storage::{Store, id};
use ratatui::Terminal;
use ratatui::backend::{Backend as TerminalBackend, CrosstermBackend, TestBackend};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange, EnableMouseCapture, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement};
use ratatui::crossterm::{cursor, execute};
use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

const FILE_LIMIT: usize = 50_000;

#[derive(Debug, Clone, Default)]
pub struct TuiOptions {
    pub directory: PathBuf,
    // "provider/model"
    pub model: Option<String>,
    pub agent: Option<String>,
    pub session: Option<String>,
    pub continue_last: bool,
    // With `session` or `continue_last`: open a fork of that session instead.
    pub fork: bool,
    pub prompt: Option<String>,
    // Approve every permission request once without asking.
    pub auto: bool,
}

// Work only the owner of the real terminal can do.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Editor { purpose: EditorPurpose, text: String },
    Suspend,
}

fn branch(worktree: Option<&Path>) -> Option<String> {
    let head = std::fs::read_to_string(worktree?.join(".git").join("HEAD")).ok()?;
    head.trim().strip_prefix("ref: refs/heads/").map(str::to_string)
}

fn list_files(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(root).hidden(false).filter_entry(|entry| entry.file_name() != ".git").build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Ok(relative) = entry.path().strip_prefix(root) {
            files.push(relative.to_string_lossy().replace('\\', "/"));
        }
        if files.len() >= FILE_LIMIT {
            break;
        }
    }
    files.sort();
    files
}

pub(crate) fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16 | u32::from(*chunk.get(1).unwrap_or(&0)) << 8 | u32::from(*chunk.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            out.push(if i <= chunk.len() { TABLE[(n >> shift & 63) as usize] as char } else { '=' });
        }
    }
    out
}

// OSC 52 clipboard write, as the original always emits.
pub fn osc52(text: &str) -> String {
    format!("\u{1b}]52;c;{}\u{7}", base64(text.as_bytes()))
}

fn toast(error: impl std::fmt::Display) -> Action {
    Action::Toast(error.to_string(), ToastKind::Error)
}

async fn open(backend: &dyn Backend, session_id: &str) -> Vec<Action> {
    match backend.load(session_id).await {
        Ok(loaded) => loaded.actions(true, true),
        Err(error) => vec![toast(error)],
    }
}

// Owns the pure `App` and performs the effects its reducer asks for, against a session backend.
pub struct Driver {
    pub app: App,
    backend: Arc<dyn Backend>,
    // Set when the engine runs in this process.
    local: Option<Arc<LocalBackend>>,
    tx: UnboundedSender<Action>,
    aborts: HashMap<String, CancellationToken>,
    directory: PathBuf,
    paths: Paths,
    // Text queued for the terminal clipboard.
    pub clipboard: Vec<String>,
    // Editor and suspend requests for whoever owns the terminal.
    pub requests: Vec<Request>,
    // False in tests: the system clipboard is neither read nor written.
    pub native_clipboard: bool,
}

// Rust build: v2 `/btw` (`packages/tui/src/feature-plugins/prompt/btw.tsx`): the request sends the session's tools but
// runs no tool loop, so the model is told not to call any.
const ASIDE_INSTRUCTIONS: &str = "The user is asking a quick side question about the conversation so far. Answer directly and concisely in markdown from what you already know. Do not call any tools and do not take any actions.";

// Injected stores mean tests or smoke runs: never fetch the models catalog from the network there.
fn offline(engine: Engine) -> Engine {
    let source = opencode_llm::catalog::Source { disabled: true, ..opencode_llm::catalog::Source::from_env(&engine.paths) };
    engine.set_catalog_source(source);
    engine
}

impl Driver {
    // The engine in this process; `store` replaces the on-disk database (tests, smoke runs).
    pub fn new(options: TuiOptions, paths: Paths, store: Option<Store>, env: Lookup) -> anyhow::Result<(Self, UnboundedReceiver<Action>)> {
        let directory = if options.directory.as_os_str().is_empty() { std::env::current_dir()? } else { options.directory.clone() };
        // Injected stores are tests or smoke runs: they never offer downloads.
        let real = store.is_none();
        let engine = match store {
            Some(store) => offline(Engine::with_store(paths.clone(), store)),
            None => Engine::new(paths.clone())?,
        };
        let catalog = catalog::load_with(&paths, &directory, env);
        let (tx, rx) = unbounded_channel();
        let local = Arc::new(LocalBackend::new(engine, tx.clone(), directory.clone(), catalog.worktree.clone()));
        let mut driver = Self::build(options, paths, directory, catalog, local.clone(), Some(local), tx, env);
        if real && driver.app.dialog.is_none() && opencode_tools::mingit::should_offer(&driver.paths.state, &driver.paths.cache) {
            driver.app.offer_bash();
        }
        Ok((driver, rx))
    }

    // An opencode server over HTTP (`attach`, or the server this process started for the network flags). The
    // directory is the client's; without one, the server's own.
    pub async fn connect(options: TuiOptions, paths: Paths, client: opencode_client::Client, env: Lookup<'_>) -> anyhow::Result<(Self, UnboundedReceiver<Action>)> {
        let (catalog, path) = tokio::join!(catalog::remote(&client), client.path());
        let directory = match (options.directory.as_os_str().is_empty(), path) {
            (false, _) => options.directory.clone(),
            (true, Ok(path)) if !path.directory.is_empty() => PathBuf::from(path.directory),
            _ => std::env::current_dir()?,
        };
        let (tx, rx) = unbounded_channel();
        let backend = Arc::new(RemoteBackend::connect(client, tx.clone()).await?);
        Ok((Self::build(options, paths, directory, catalog, backend, None, tx, env), rx))
    }

    #[allow(clippy::too_many_arguments)]
    fn build(options: TuiOptions, paths: Paths, directory: PathBuf, catalog: Catalog, backend: Arc<dyn Backend>, local: Option<Arc<LocalBackend>>, tx: UnboundedSender<Action>, env: Lookup) -> Self {
        let config = TuiConfig::load_with(&paths, &directory, env);
        let kv = Kv::load(&paths.state);
        let (keymap, _) = crate::keybind::Keymap::new(&config.keybinds, config.leader_timeout, !cfg!(windows));
        let mut tips = tips::all(&keymap, !cfg!(windows));
        let pick = (id::now_ms() % tips.len().max(1) as u64) as usize;
        if !tips.is_empty() {
            tips.swap(0, pick);
        }
        let init = Init {
            home: paths.home.clone(),
            themes: Registry::discover(&paths.config, &directory),
            models: ModelState::load(&paths.state),
            history: History::load(&paths.state),
            stash: Stash::load(&paths.state),
            frecency: Frecency::load(&paths.state),
            pins: Pins::load(&paths.state),
            caps: Caps::detect(env, cfg!(windows)),
            disable_paste_summary: catalog.disable_paste_summary,
            branch: branch(catalog.worktree.as_deref()),
            directory: directory.clone(),
            config,
            kv,
            catalog,
            model: options.model.clone(),
            agent: options.agent.clone(),
            prompt: options.prompt.clone(),
            tips,
        };
        let mut driver = Self {
            app: App::new(init),
            backend,
            local,
            tx,
            aborts: HashMap::new(),
            directory,
            paths,
            clipboard: Vec::new(),
            requests: Vec::new(),
            native_clipboard: false,
        };
        driver.app.now_ms = id::now_ms();
        driver.app.auto_permissions = options.auto;
        driver.startup(options.session, options.continue_last, options.fork, options.prompt.is_some());
        driver
    }

    // `--session` / `--continue` (with `--fork`) open their session first; a `--prompt` is sent once it is open.
    fn startup(&mut self, session: Option<String>, continue_last: bool, fork: bool, submit: bool) {
        if session.is_none() && !continue_last {
            if submit {
                self.dispatch(Action::Submit);
            }
            return;
        }
        let backend = self.backend.clone();
        self.exec(async move {
            let target = match session {
                Some(id) => Ok(Some(id)),
                None => backend.sessions().await.map(|list| list.into_iter().next().map(|session| session.id)),
            };
            let mut actions = match target {
                Err(error) => vec![toast(error)],
                Ok(None) => Vec::new(),
                Ok(Some(id)) if fork => match backend.fork(&id, None).await {
                    Ok(forked) => open(&*backend, &forked.id).await,
                    Err(_) => vec![toast("Failed to fork session")],
                },
                Ok(Some(id)) => open(&*backend, &id).await,
            };
            if submit {
                actions.push(Action::Submit);
            }
            actions
        });
    }

    // The in-process engine; None against a server.
    pub fn engine(&self) -> Option<&Arc<Engine>> {
        self.local.as_ref().map(|local| &local.engine)
    }

    // Lists project files in the background for `@` completion.
    pub fn index_files(&self) {
        if !self.directory.is_dir() {
            return;
        }
        let (tx, root) = (self.tx.clone(), self.directory.clone());
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(Action::FilesLoaded(list_files(&root)));
        });
    }

    // Backend calls of the in-process engine finish on their first poll and are applied at once, as the reducer
    // expects; a remote call runs in the background and reports through the channel.
    fn exec(&mut self, job: impl Future<Output = Vec<Action>> + Send + 'static) {
        let mut job = Box::pin(job);
        match job.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(actions) => {
                for action in actions {
                    self.dispatch(action);
                }
            }
            Poll::Pending => {
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    for action in job.await {
                        let _ = tx.send(action);
                    }
                });
            }
        }
    }

    fn refresh(&mut self, session_id: &str, open_view: bool, settled: bool) {
        let (backend, id) = (self.backend.clone(), session_id.to_string());
        self.exec(async move {
            match backend.load(&id).await {
                Ok(loaded) => loaded.actions(open_view, settled),
                Err(error) => vec![toast(error)],
            }
        });
    }

    fn toast(&mut self, message: String) {
        self.app.show_toast(message, ToastKind::Error);
    }

    pub fn dispatch(&mut self, action: Action) {
        // A pasted path to an image, PDF or SVG attaches the file instead of inserting the path.
        let pasted = match &action {
            Action::Paste(text) | Action::Clipboard(Some(terminal::ClipboardContent::Text(text))) if self.app.prompt_focused() => terminal::local_attachment(text, cfg!(windows)),
            _ => None,
        };
        let effects = self.app.update(pasted.map_or(action, Action::Attachment));
        self.run(effects);
    }

    fn run(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            self.execute(effect);
        }
    }

    // Runs one turn in the background and reports its end to the reducer.
    fn start(&mut self, turn: Turn) {
        let session_id = turn.session_id().to_string();
        let token = CancellationToken::new();
        self.aborts.insert(session_id.clone(), token.clone());
        let (backend, tx) = (self.backend.clone(), self.tx.clone());
        tokio::spawn(async move {
            let error = backend.run(turn, token).await.err();
            let _ = tx.send(Action::PromptFinished { session_id, error });
        });
    }

    // Rust build: a queued prompt runs beside the turn it waits for; its end is reported like any turn's, and its
    // abort token stays out of the map so stopping the running turn does not drop it.
    fn start_queued(&mut self, turn: Turn) {
        let session_id = turn.session_id().to_string();
        let (backend, tx) = (self.backend.clone(), self.tx.clone());
        tokio::spawn(async move {
            let error = backend.run(turn, CancellationToken::new()).await.err();
            let _ = tx.send(Action::PromptFinished { session_id, error });
        });
    }

    // A prompt from the home screen: the session is created first, then the prompt is sent to it.
    fn create_for(&mut self, effect: Effect) {
        let shown = match &effect {
            Effect::Submit { text, .. } => text.clone(),
            Effect::Shell { command, .. } => format!("!{command}"),
            Effect::Command { command, arguments, .. } => format!("/{command} {arguments}").trim_end().to_string(),
            _ => return,
        };
        let backend = self.backend.clone();
        self.exec(async move {
            match backend.create_session().await {
                Ok(info) => {
                    let mut then = effect;
                    if let Effect::Submit { session_id, .. } | Effect::Shell { session_id, .. } | Effect::Command { session_id, .. } = &mut then {
                        *session_id = Some(info.id.clone());
                    }
                    vec![Action::SessionStarted { info: Box::new(info), text: shown, then: Box::new(then) }]
                }
                Err(error) => vec![toast(error)],
            }
        });
    }

    pub fn execute(&mut self, effect: Effect) {
        match effect {
            Effect::Quit => {
                for token in self.aborts.values() {
                    token.cancel();
                }
                // Rust build: background shells end with the app.
                opencode_tools::bash::background::kill_all();
                self.backend.release();
            }
            effect @ (Effect::Submit { session_id: None, .. } | Effect::Shell { session_id: None, .. } | Effect::Command { session_id: None, .. }) => self.create_for(effect),
            Effect::Submit { session_id: Some(session_id), text, files, agent, model, variant } => self.start(Turn::Prompt { session_id, text, files, agent, model, variant }),
            Effect::Queue { session_id, text, files, agent, model, variant } => self.start_queued(Turn::Queued { session_id, text, files, agent, model, variant }),
            // Rust build: `/btw`, `/reload` and `/stats` (opencode 2.x).
            Effect::Aside { session_id, question } => {
                let backend = self.backend.clone();
                self.exec(async move {
                    let prompt = [ASIDE_INSTRUCTIONS, question.as_str()].join("\n\n");
                    let answer = backend.aside(&session_id, &prompt).await.map_err(|error| error.to_string());
                    vec![Action::Aside { question, answer }]
                });
            }
            Effect::ReloadConfig => {
                let backend = self.backend.clone();
                self.exec(async move { vec![Action::Reloaded(backend.reload().await.map(Box::new).map_err(|error| error.to_string()))] });
            }
            Effect::Stats => {
                let backend = self.backend.clone();
                self.exec(async move { vec![Action::Stats(backend.stats().await.map_err(|error| error.to_string()))] });
            }
            Effect::Shell { session_id: Some(session_id), command, agent, model } => self.start(Turn::Shell { session_id, command, agent, model }),
            Effect::Command { session_id: Some(session_id), command, arguments, agent, model } => self.start(Turn::Command { session_id, command, arguments, agent, model }),
            Effect::Compact { session_id } => {
                let model = self.app.current_model().map(|key| key.spec());
                self.start(Turn::Compact { session_id, model });
            }
            Effect::QuestionReply { id, answers } => self.backend.reply_question(id, answers),
            Effect::PermissionFeedback { id, message } => self.backend.reply_permission(id, PermissionAnswer::Corrected(message)),
            Effect::PermissionReply { id, reply } => self.backend.reply_permission(id, reply.into()),
            Effect::Fork { session_id, message_id, prompt } => {
                let backend = self.backend.clone();
                self.exec(async move {
                    match backend.fork(&session_id, message_id.as_deref()).await {
                        Ok(forked) => {
                            let mut actions = open(&*backend, &forked.id).await;
                            actions.extend(prompt.map(Action::PromptSet));
                            actions
                        }
                        Err(error) => vec![toast(error)],
                    }
                });
            }
            Effect::ReadClipboard => {
                if self.native_clipboard {
                    let tx = self.tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = tx.send(Action::Clipboard(terminal::clipboard_read()));
                    });
                }
            }
            Effect::OpenEditor { purpose, text } => self.requests.push(Request::Editor { purpose, text }),
            Effect::Suspend => self.requests.push(Request::Suspend),
            Effect::WriteFile { filename, text } => {
                if std::fs::write(self.directory.join(&filename), text).is_err() {
                    self.toast("Failed to export session".to_string());
                }
            }
            Effect::Stash(write) => {
                let _ = self.app.stash.persist(&self.paths.state, &write);
            }
            Effect::Frecency(write) => {
                let _ = self.app.frecency.persist(&self.paths.state, &write);
            }
            Effect::Abort { session_id } => {
                self.backend.abort(&session_id);
                if let Some(token) = self.aborts.remove(&session_id) {
                    token.cancel();
                }
            }
            // Reverting restores files through git; it runs beside the UI and reports back.
            Effect::Undo { session_id, message_id } => {
                let (backend, tx) = (self.backend.clone(), self.tx.clone());
                tokio::spawn(async move {
                    let error = backend.revert(&session_id, &message_id).await.err();
                    let _ = tx.send(Action::SessionChanged { session_id, error });
                });
            }
            Effect::Redo { session_id } => {
                let (backend, tx) = (self.backend.clone(), self.tx.clone());
                tokio::spawn(async move {
                    let error = backend.unrevert(&session_id).await.err();
                    let _ = tx.send(Action::SessionChanged { session_id, error });
                });
            }
            Effect::OpenSession(session_id) => self.refresh(&session_id, true, true),
            Effect::Reload { session_id, settled } => self.refresh(&session_id, false, settled),
            Effect::ListSessions => {
                let backend = self.backend.clone();
                self.exec(async move {
                    match backend.sessions().await {
                        Ok(list) => vec![Action::SessionsListed(list)],
                        Err(error) => vec![toast(error)],
                    }
                });
            }
            Effect::DeleteSession(session_id) => {
                let backend = self.backend.clone();
                self.exec(async move {
                    match backend.delete_session(&session_id).await {
                        Ok(()) => vec![Action::SessionDeleted(session_id)],
                        Err(error) => vec![toast(error)],
                    }
                });
            }
            Effect::RenameSession { session_id, title } => {
                let backend = self.backend.clone();
                self.exec(async move { backend.rename_session(&session_id, &title).await.err().map(toast).into_iter().collect() });
            }
            Effect::SaveKv => {
                if let Err(error) = self.app.kv.save() {
                    self.toast(format!("Failed to write KV state: {error}"));
                }
            }
            Effect::SavePins => {
                let _ = self.app.pins.save(&self.paths.state);
            }
            Effect::QuickSwitch(slot) => {
                let (backend, pins) = (self.backend.clone(), self.app.pins.clone());
                let current = self.app.session().map(|view| view.info.id.clone());
                self.exec(async move {
                    let sessions = backend.sessions().await.unwrap_or_default();
                    let existing: Vec<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
                    let target = slot.checked_sub(1).and_then(|index| pins.slots(&existing).get(index).map(|id| id.to_string()));
                    match target.filter(|target| current.as_deref() != Some(target.as_str())) {
                        Some(target) => open(&*backend, &target).await,
                        None => Vec::new(),
                    }
                });
            }
            Effect::OpenUrl(url) => {
                if self.native_clipboard {
                    let _ = terminal::open_url(&url);
                }
            }
            Effect::SaveModels => {
                let _ = self.app.models.save(&self.paths.state);
            }
            Effect::History(write) => {
                let _ = self.app.history.persist(&self.paths.state, &write);
            }
            Effect::Copy(text) => {
                // OSC 52 through the terminal plus the platform clipboard, as the original writes both.
                if self.native_clipboard {
                    let copy = text.clone();
                    tokio::task::spawn_blocking(move || terminal::clipboard_write(&copy));
                }
                self.clipboard.push(text);
            }
            Effect::SetupBash { accept } => self.setup_bash(accept),
        }
    }

    // Records the answer to the portable-bash offer; an accept downloads it in the background.
    fn setup_bash(&mut self, accept: bool) {
        use opencode_tools::mingit::{self, Consent};
        let answer = if accept { Consent::Accepted } else { Consent::Declined };
        if let Err(error) = mingit::set_consent(&self.paths.state, Some(answer)) {
            self.toast(error.to_string());
        }
        if !accept {
            return;
        }
        self.app.show_toast("Downloading the portable bash (~38 MB)...", ToastKind::Info);
        let (tx, root) = (self.tx.clone(), mingit::root_in(&self.paths.cache));
        tokio::spawn(async move {
            let action = match mingit::Installer::new(root).install(&|_, _| {}).await {
                Ok(outcome) => Action::Toast(format!("Portable bash ready: {}", outcome.installed.shell.display()), ToastKind::Info),
                Err(error) => Action::Toast(format!("Portable bash download failed: {error}. Run `opencode setup bash` to retry."), ToastKind::Error),
            };
            let _ = tx.send(action);
        });
    }
}

// Chinese, Japanese and Korean code pages: the console host draws ambiguous-width characters two cells wide.
#[cfg(windows)]
fn east_asian_console() -> bool {
    // SAFETY: reads the code page of this process's console; no pointers are involved.
    let page = unsafe { windows_sys::Win32::System::Console::GetConsoleOutputCP() };
    matches!(page, 932 | 936 | 949 | 950 | 1361 | 20932 | 20936 | 20949)
}

#[cfg(not(windows))]
fn east_asian_console() -> bool {
    false
}

struct Restore {
    mouse: bool,
    enhanced: bool,
}

impl Restore {
    fn run(&self) {
        let mut out = std::io::stdout();
        if self.enhanced {
            let _ = execute!(out, PopKeyboardEnhancementFlags);
        }
        if self.mouse {
            let _ = execute!(out, DisableMouseCapture);
        }
        let _ = execute!(out, DisableFocusChange, DisableBracketedPaste, LeaveAlternateScreen, cursor::Show);
        let _ = disable_raw_mode();
    }

    // Optional capabilities are best effort: a console without them still gets a working UI.
    fn enter(&self) -> std::io::Result<()> {
        enable_raw_mode()?;
        let mut out = std::io::stdout();
        execute!(out, EnterAlternateScreen)?;
        let _ = execute!(out, EnableBracketedPaste, EnableFocusChange);
        if self.mouse {
            let _ = execute!(out, EnableMouseCapture);
        }
        if self.enhanced {
            let _ = execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
        }
        Ok(())
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        self.run();
    }
}

fn title(app: &App) -> String {
    match app.session() {
        Some(view) if !view.info.has_default_title() => format!("OC | {}", crate::text::truncate(&view.info.title, 40)),
        _ => "OpenCode".to_string(),
    }
}

// The terminal UI with the engine in this process.
pub async fn run(options: TuiOptions) -> anyhow::Result<()> {
    let env: Lookup = &flag::process_env;
    let (driver, rx) = Driver::new(options, Paths::from_env(), None, env)?;
    run_terminal(driver, rx, env).await
}

// The terminal UI against an opencode server.
pub async fn run_remote(options: TuiOptions, client: opencode_client::Client) -> anyhow::Result<()> {
    let env: Lookup = &flag::process_env;
    let (driver, rx) = Driver::connect(options, Paths::from_env(), client, env).await?;
    run_terminal(driver, rx, env).await
}

async fn run_terminal(mut driver: Driver, mut rx: UnboundedReceiver<Action>, env: Lookup<'_>) -> anyhow::Result<()> {
    // Raw mode reaches the console even when stdout is redirected; without a terminal to draw on there is no UI.
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        anyhow::bail!("the terminal UI needs a TTY stdout");
    }
    driver.index_files();

    driver.native_clipboard = true;
    // Rust build: the terminal library honours NO_COLOR and would drop every colour of the UI; the original's TUI
    // ignores it (no reference in its source), and a full-screen UI without colour is unusable.
    ratatui::crossterm::style::force_color_output(true);
    let mouse = driver.app.config.mouse && !flag::truthy(env, "OPENCODE_DISABLE_MOUSE");
    let titled = !flag::truthy(env, "OPENCODE_DISABLE_TERMINAL_TITLE");
    enable_raw_mode()?;
    // Rust build: on Windows the keys arrive as console input records; a terminal that accepts the enhanced keyboard
    // protocol (Windows Terminal) would send escape sequences instead, and the arrow keys stop working.
    let enhanced = !cfg!(windows) && supports_keyboard_enhancement().unwrap_or(false);
    let restore = Restore { mouse, enhanced };
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        Restore { mouse, enhanced }.run();
        previous(info);
    }));
    // Colour queries are answered on stdin, so they run before the event reader owns it. Windows consoles deliver
    // replies as console input records this reader cannot see; the mode stays at its default there.
    if !cfg!(windows) {
        driver.dispatch(Action::Terminal(terminal::detect(terminal::QUERY_TIMEOUT)));
    }
    restore.enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let size = terminal.size()?;
    driver.dispatch(Action::Resize(size.width, size.height));

    let stop = Arc::new(AtomicBool::new(false));
    // Set while an editor or the shell owns the terminal.
    let paused = Arc::new(AtomicBool::new(false));
    let (input_tx, input_stop, input_paused) = (driver.tx.clone(), stop.clone(), paused.clone());
    let reader = std::thread::spawn(move || {
        while !input_stop.load(Ordering::Relaxed) {
            if input_paused.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            if !event::poll(Duration::from_millis(50)).unwrap_or(false) {
                continue;
            }
            let action = match event::read() {
                Ok(Event::Key(key)) => Action::Key(key),
                Ok(Event::Paste(text)) => Action::Paste(text),
                Ok(Event::Mouse(mouse)) => Action::Mouse(mouse),
                Ok(Event::Resize(width, height)) => Action::Resize(width, height),
                Ok(Event::FocusGained) => Action::Focus(true),
                Ok(Event::FocusLost) => Action::Focus(false),
                Err(_) => break,
            };
            if input_tx.send(action).is_err() {
                break;
            }
        }
    });

    // The Windows console host (not Windows Terminal or an editor's terminal) can leave half-overwritten CJK characters
    // behind after incremental redraws; a settled reply is repainted from scratch there.
    let console_host = cfg!(windows) && env("WT_SESSION").is_none() && env("TERM_PROGRAM").is_none();
    driver.app.console_host = console_host;
    // OPENCODE_NARROW_GLYPHS=1/0 forces it, for terminals set to draw ambiguous characters wide.
    let narrow = match env("OPENCODE_NARROW_GLYPHS").as_deref() {
        Some("1" | "true") => true,
        Some("0" | "false") => false,
        _ => console_host && east_asian_console(),
    };
    driver.app.narrow_glyphs = narrow;
    let mut ticker = tokio::time::interval(Duration::from_millis(80));
    let mut shown_title = String::new();
    // Rust build: the desktop app's sounds; its settings are read once per start.
    let sound_choice = crate::sound::Choice::load(&driver.paths.state);
    let player = crate::sound::Player::start();
    let result: anyhow::Result<()> = loop {
        let mut failed = None;
        for request in std::mem::take(&mut driver.requests) {
            // Hand the terminal over, run the foreground program, then take it back and repaint from scratch.
            paused.store(true, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(70));
            restore.run();
            match request {
                Request::Editor { purpose, text } => {
                    let content = match terminal::editor_command(env) {
                        Some(editor) => match terminal::open_editor(&editor, &text, &driver.directory, &std::env::temp_dir(), id::now_ms()) {
                            Ok(content) => content,
                            Err(error) => {
                                driver.toast(error.to_string());
                                None
                            }
                        },
                        None => None,
                    };
                    driver.dispatch(Action::EditorClosed { purpose, content });
                }
                Request::Suspend => {
                    if !cfg!(windows) {
                        // `process.kill(0, "SIGTSTP")`: stops this process group until the shell resumes it.
                        let _ = std::process::Command::new("kill").args(["-TSTP", "0"]).status();
                    }
                }
            }
            if let Err(error) = restore.enter() {
                failed = Some(error);
                break;
            }
            let _ = terminal.clear();
            paused.store(false, Ordering::Relaxed);
        }
        if let Some(error) = failed {
            break Err(error.into());
        }
        if std::mem::take(&mut driver.app.repaint) && console_host {
            let _ = terminal.clear();
        }
        let drawn = terminal.draw(|frame| {
            view::render(frame, &driver.app);
            if narrow {
                crate::glyphs::narrow_buffer(frame.buffer_mut());
            }
        });
        if let Err(error) = drawn {
            break Err(error.into());
        }
        let wanted = if driver.app.terminal_title_enabled() { title(&driver.app) } else { String::new() };
        if titled && wanted != shown_title {
            let _ = execute!(std::io::stdout(), SetTitle(&wanted));
            shown_title = wanted;
        }
        for text in driver.clipboard.drain(..) {
            let mut out = std::io::stdout();
            let _ = out.write_all(osc52(&text).as_bytes());
            let _ = out.flush();
        }
        for chime in std::mem::take(&mut driver.app.chimes) {
            if let (Some(player), Some(id)) = (&player, sound_choice.sound(chime)) {
                player.play(id);
            }
        }
        tokio::select! {
            action = rx.recv() => {
                let Some(action) = action else { break Ok(()) };
                driver.dispatch(action);
                // Streaming deltas arrive in bursts; fold them into one frame.
                for _ in 0..512 {
                    match rx.try_recv() {
                        Ok(action) => driver.dispatch(action),
                        Err(_) => break,
                    }
                }
            }
            _ = ticker.tick() => driver.dispatch(Action::Tick(Instant::now(), id::now_ms())),
        }
        if driver.app.quit {
            break Ok(());
        }
    };
    stop.store(true, Ordering::Relaxed);
    let _ = reader.join();
    drop(restore);
    let _ = std::panic::take_hook();
    result
}

fn frame_text<B: TerminalBackend>(terminal: &Terminal<B>, buffer: &ratatui::buffer::Buffer) -> String {
    let _ = terminal;
    let area = buffer.area;
    (0..area.height).map(|y| (0..area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
}

// Runs the real startup path against an off-screen terminal and returns the frames it drew.
pub async fn smoke_with(options: TuiOptions, paths: Paths, store: Option<Store>, env: Lookup<'_>) -> anyhow::Result<Vec<String>> {
    let (mut driver, mut rx) = Driver::new(options, paths, store, env)?;
    let mut terminal = Terminal::new(TestBackend::new(100, 32))?;
    let mut frames = Vec::new();
    let mut draw = |driver: &mut Driver, terminal: &mut Terminal<TestBackend>| -> anyhow::Result<()> {
        terminal.draw(|frame| view::render(frame, &driver.app))?;
        let buffer = terminal.backend().buffer().clone();
        frames.push(frame_text(terminal, &buffer));
        Ok(())
    };
    driver.dispatch(Action::Resize(100, 32));
    driver.dispatch(Action::Tick(Instant::now(), id::now_ms()));
    draw(&mut driver, &mut terminal)?;
    // Let a `--prompt` run report back before the next frame.
    if !driver.app.running.is_empty() {
        let deadline = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                action = rx.recv() => match action {
                    Some(action) => {
                        let finished = matches!(action, Action::PromptFinished { .. });
                        driver.dispatch(action);
                        if finished { break }
                    }
                    None => break,
                },
                _ = &mut deadline => break,
            }
        }
    }
    for command in ["model_list", "help_show", "theme_list"] {
        let effects = driver.app.run(command);
        driver.run(effects);
        draw(&mut driver, &mut terminal)?;
        driver.dispatch(Action::Key(ratatui::crossterm::event::KeyEvent::new(ratatui::crossterm::event::KeyCode::Esc, ratatui::crossterm::event::KeyModifiers::NONE)));
    }
    driver.execute(Effect::ListSessions);
    draw(&mut driver, &mut terminal)?;
    driver.execute(Effect::Quit);
    if frames.iter().any(|frame| frame.trim().is_empty()) {
        anyhow::bail!("smoke run drew an empty frame");
    }
    Ok(frames)
}

pub async fn smoke(options: TuiOptions) -> anyhow::Result<()> {
    smoke_with(options, Paths::from_env(), None, &flag::process_env).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Route;
    use crate::backend::TuiHost;
    use crate::state::ModelKey;
    use opencode_session::{Host, Reply};
    use opencode_storage::Session;
    use opencode_tools::PermissionRequest;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    impl Driver {
        fn store(&self) -> &Store {
            &self.engine().expect("an in-process engine").store
        }

        fn host(&self) -> Arc<TuiHost> {
            self.local.as_ref().expect("an in-process engine").host.clone()
        }

        // The in-process backend answers on the first poll.
        fn create_session(&mut self) -> anyhow::Result<Session> {
            let backend = self.backend.clone();
            let mut job = Box::pin(async move { backend.create_session().await });
            match job.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
                Poll::Ready(result) => result,
                Poll::Pending => panic!("the local backend should not wait"),
            }
        }
    }

    fn paths(root: &Path) -> Paths {
        Paths { home: root.join("home"), data: root.join("data"), cache: root.join("cache"), config: root.join("config"), state: root.join("state"), tmp: root.join("tmp") }
    }

    // Nothing but a terminal type, so glyph and colour fallbacks stay off on every platform.
    fn no_env(key: &str) -> Option<String> {
        (key == "TERM").then(|| "xterm-direct".to_string())
    }

    fn driver(root: &Path, options: TuiOptions) -> (Driver, UnboundedReceiver<Action>) {
        let project = root.join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::write(project.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let options = TuiOptions { directory: project, ..options };
        Driver::new(options, paths(root), Some(Store::open_in_memory().unwrap()), &no_env).unwrap()
    }

    fn key(driver: &mut Driver, code: KeyCode, modifiers: KeyModifiers) {
        driver.dispatch(Action::Key(KeyEvent::new(code, modifiers)));
    }

    fn type_text(driver: &mut Driver, text: &str) {
        for c in text.chars() {
            key(driver, KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    #[tokio::test]
    async fn sessions_can_be_created_listed_renamed_and_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let (mut driver, _rx) = driver(dir.path(), TuiOptions::default());
        assert_eq!(driver.app.branch.as_deref(), Some("main"));
        let first = driver.create_session().unwrap();
        let second = driver.create_session().unwrap();
        driver.store().patch_session(&second.id, |s| s.time.updated += 10).unwrap();
        driver.execute(Effect::ListSessions);
        let Some(crate::dialog::Dialog::Select(select)) = &driver.app.dialog else { panic!("no session dialog") };
        assert_eq!(select.items.iter().map(|i| i.value.clone()).collect::<Vec<_>>(), vec![second.id.clone(), first.id.clone()]);

        key(&mut driver, KeyCode::Char('r'), KeyModifiers::CONTROL);
        key(&mut driver, KeyCode::Char('u'), KeyModifiers::CONTROL);
        type_text(&mut driver, "Renamed");
        key(&mut driver, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(driver.store().get_session(&second.id).unwrap().unwrap().title, "Renamed");

        driver.execute(Effect::ListSessions);
        key(&mut driver, KeyCode::Char('d'), KeyModifiers::CONTROL);
        key(&mut driver, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(driver.store().get_session(&second.id).unwrap().is_none());
        key(&mut driver, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(&driver.app.route, Route::Session(view) if view.info.id == first.id));
    }

    #[tokio::test]
    async fn state_files_are_written_in_the_original_formats() {
        let dir = tempfile::tempdir().unwrap();
        let (mut driver, mut rx) = driver(dir.path(), TuiOptions::default());
        driver.app.catalog = catalog::fixture();
        type_text(&mut driver, "hello there");
        key(&mut driver, KeyCode::Enter, KeyModifiers::NONE);
        let state = dir.path().join("state");
        assert_eq!(std::fs::read_to_string(state.join("prompt-history.jsonl")).unwrap(), "{\"input\":\"hello there\",\"parts\":[],\"mode\":\"normal\"}\n");
        assert_eq!(
            std::fs::read_to_string(state.join("model.json")).unwrap(),
            r#"{"recent":[{"providerID":"anthropic","modelID":"claude-sonnet-4-5"}],"favorite":[],"variant":{}}"#
        );
        // The session exists before the engine answers, and the prompt shows as sent.
        let Route::Session(view) = &driver.app.route else { panic!("no session route") };
        let session_id = view.info.id.clone();
        assert_eq!(view.pending_user.as_ref().unwrap().0, "hello there");
        assert!(driver.store().get_session(&session_id).unwrap().is_some());
        // No provider is configured in the temp dirs, so the engine rejects the model and the UI recovers.
        loop {
            let action = tokio::time::timeout(Duration::from_secs(20), rx.recv()).await.unwrap().unwrap();
            let finished = matches!(action, Action::PromptFinished { .. });
            driver.dispatch(action);
            if finished {
                break;
            }
        }
        assert!(!driver.app.session().unwrap().busy());
        assert!(driver.app.toast.as_ref().unwrap().message.contains("Model not found"), "{:?}", driver.app.toast);

        driver.app.run("theme_list");
        type_text(&mut driver, "nord");
        key(&mut driver, KeyCode::Enter, KeyModifiers::NONE);
        let kv: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(state.join("kv.json")).unwrap()).unwrap();
        assert_eq!((&kv["rs.theme"], &kv["theme"]), (&serde_json::json!("nord"), &serde_json::Value::Null));
        let (reloaded, _rx) = super::tests::driver(dir.path(), TuiOptions { continue_last: true, ..Default::default() });
        assert_eq!(reloaded.app.theme_name, "nord");
        assert_eq!(reloaded.app.models.recent, vec![ModelKey::new("anthropic", "claude-sonnet-4-5")]);
        assert_eq!(reloaded.app.history.entries.len(), 1);
    }

    #[tokio::test]
    async fn permission_requests_round_trip_through_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let (mut driver, mut rx) = driver(dir.path(), TuiOptions::default());
        let session = driver.create_session().unwrap();
        driver.execute(Effect::OpenSession(session.id));
        let host = driver.host();
        let asked = tokio::spawn(async move {
            let request = PermissionRequest { permission: "bash".into(), patterns: vec!["ls".into()], always: vec!["ls".into()], metadata: serde_json::json!({"command": "ls"}) };
            host.permission(&request).await
        });
        let action = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
        driver.dispatch(action);
        assert_eq!(driver.app.permissions.len(), 1);
        key(&mut driver, KeyCode::Right, KeyModifiers::NONE);
        key(&mut driver, KeyCode::Enter, KeyModifiers::NONE);
        key(&mut driver, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(asked.await.unwrap(), Reply::Always);
        assert!(driver.app.permissions.is_empty());

        // Quitting releases anything still waiting.
        let host = driver.host();
        let asked = tokio::spawn(async move {
            let request = PermissionRequest { permission: "edit".into(), patterns: vec![], always: vec![], metadata: serde_json::json!({}) };
            host.permission(&request).await
        });
        let action = rx.recv().await.unwrap();
        driver.dispatch(action);
        driver.execute(Effect::Quit);
        assert_eq!(asked.await.unwrap(), Reply::Reject);
    }

    #[tokio::test]
    async fn auto_mode_approves_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let (mut driver, mut rx) = driver(dir.path(), TuiOptions { auto: true, ..Default::default() });
        let host = driver.host();
        let asked = tokio::spawn(async move {
            let request = PermissionRequest { permission: "bash".into(), patterns: vec!["rm x".into()], always: vec![], metadata: serde_json::json!({}) };
            host.permission(&request).await
        });
        let action = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
        driver.dispatch(action);
        assert_eq!(asked.await.unwrap(), Reply::Once);
        assert!(driver.app.permissions.is_empty());
        assert_eq!(crate::dialog::command_title(&driver.app, "permission_mode"), Some("Disable auto-approve permissions"));
        driver.app.run("permission_mode");
        assert!(!driver.app.auto_permissions);
    }

    #[tokio::test]
    async fn options_select_session_agent_and_model() {
        let dir = tempfile::tempdir().unwrap();
        let (mut seed, _rx) = driver(dir.path(), TuiOptions::default());
        let session = seed.create_session().unwrap();
        let store = seed.store().clone();
        let project = dir.path().join("project");
        let options = TuiOptions { directory: project.clone(), session: Some(session.id.clone()), agent: Some("plan".into()), model: Some("openai/gpt-5".into()), ..Default::default() };
        let (mut driver, _rx) = Driver::new(options, paths(dir.path()), Some(store.clone()), &no_env).unwrap();
        assert!(matches!(&driver.app.route, Route::Session(view) if view.info.id == session.id));
        assert_eq!(driver.app.agent, "plan");
        driver.app.catalog = catalog::fixture();
        assert_eq!(driver.app.current_model(), Some(ModelKey::new("openai", "gpt-5")));

        let options = TuiOptions { directory: project.clone(), continue_last: true, ..Default::default() };
        let (continued, _rx) = Driver::new(options, paths(dir.path()), Some(store.clone()), &no_env).unwrap();
        assert!(matches!(&continued.app.route, Route::Session(view) if view.info.id == session.id));

        // `--fork` opens a copy of the session it continues.
        let options = TuiOptions { directory: project.clone(), session: Some(session.id.clone()), fork: true, ..Default::default() };
        let (forked, _rx) = Driver::new(options, paths(dir.path()), Some(store.clone()), &no_env).unwrap();
        assert!(matches!(&forked.app.route, Route::Session(view) if view.info.id != session.id));

        let options = TuiOptions { directory: project, session: Some("ses_missing".into()), ..Default::default() };
        let (missing, _rx) = Driver::new(options, paths(dir.path()), Some(store), &no_env).unwrap();
        assert!(matches!(missing.app.route, Route::Home));
        assert!(missing.app.toast.unwrap().message.contains("Session not found"));
    }

    #[tokio::test]
    async fn file_index_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "target\n").unwrap();
        std::fs::write(root.join("src").join("main.rs"), "").unwrap();
        std::fs::write(root.join("target").join("out.bin"), "").unwrap();
        std::fs::write(root.join(".git").join("config"), "").unwrap();
        assert_eq!(list_files(&root), vec![".gitignore".to_string(), "src/main.rs".to_string()]);
    }

    #[tokio::test]
    async fn smoke_draws_frames_without_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let options = TuiOptions { directory: project, ..Default::default() };
        let frames = smoke_with(options, paths(dir.path()), Some(Store::open_in_memory().unwrap()), &no_env).await.unwrap();
        assert_eq!(frames.len(), 5);
        assert!(frames[0].contains("Ask anything…") && frames[0].contains("│ OpenCode") && frames[0].contains("╰─"), "{}", frames[0]);
        assert!(frames[1].contains("Select model"), "{}", frames[1]);
        assert!(frames[2].contains("Help"));
        assert!(frames[3].contains("Themes") && frames[3].contains("clay"), "{}", frames[3]);
        assert!(frames[4].contains("Sessions"));
    }

    #[test]
    fn encodes_clipboard_sequences() {
        assert_eq!(osc52("hi"), "\u{1b}]52;c;aGk=\u{7}");
        assert_eq!(base64(b"any carnal pleas"), "YW55IGNhcm5hbCBwbGVhcw==");
        assert_eq!(base64(b"abc"), "YWJj");
    }
}
