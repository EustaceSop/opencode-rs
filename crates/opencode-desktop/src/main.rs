#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod instance;
mod model;
mod ui;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::{ActiveTheme as _, TitleBar};
use gpui_kit::{
    App, AppContext as _, Bounds, Context, IntoElement, ParentElement as _, Render, SharedString, Styled as _, Window, WindowBounds,
    WindowOptions, div, point, px, size,
};
use model::backend::Backend;
use model::deeplink::{self, Link, Request};
use model::i18n::{self, I18n};
use model::persist::{self, Persisted, Saver, WindowGeometry};
use model::settings::{Scheme, Settings};
use model::state::Event;
use opencode_core::paths::Paths;
use opencode_session::Engine;
use opencode_storage::Store;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use ui::alerts::{Alerts, Silent, System};
use ui::app::{AppView, FRAMES, Launch, ROWS};

const USAGE: &str = "Usage: opencode-desktop [DIRECTORY] [opencode://LINK ...] [--smoke]";

// The original's production identity is `ai.opencode.desktop` / "OpenCode" (`desktop/src/main/index.ts:53-62`). The id
// is suffixed so notifications and the taskbar keep this build apart from an installed Electron one; the installer uses it too.
const APP_ID: &str = "ai.opencode.desktop.rs";
const APP_NAME: &str = "OpenCode";

static SMOKE_PASSED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Default, PartialEq)]
struct Args {
    directory: Option<PathBuf>,
    links: Vec<String>,
    smoke: bool,
    help: bool,
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    for arg in args {
        match arg.as_str() {
            "--smoke" => parsed.smoke = true,
            "-h" | "--help" => parsed.help = true,
            link if link.starts_with(deeplink::PREFIX) => parsed.links.push(link.to_string()),
            flag if flag.starts_with('-') => return Err(format!("Unknown option: {flag}")),
            _ if parsed.directory.is_some() => return Err("Only one directory can be opened".to_string()),
            path => parsed.directory = Some(PathBuf::from(path)),
        }
    }
    Ok(parsed)
}

// Started for a link alone (a browser, the shell), the link's project is the one to open, not the working directory.
fn link_directory(links: &[String]) -> Option<PathBuf> {
    links.iter().filter_map(|link| deeplink::parse(link)).find_map(|link| {
        let (Link::OpenProject { directory } | Link::NewSession { directory, .. }) = link;
        std::path::absolute(directory).ok().filter(|path| path.is_dir())
    })
}

fn project_directory(requested: Option<PathBuf>) -> Result<PathBuf, String> {
    let directory = match requested {
        Some(path) => std::path::absolute(&path).map_err(|e| format!("{}: {e}", path.display()))?,
        None => {
            let current = std::env::current_dir().map_err(|e| format!("Cannot read the current directory: {e}"))?;
            let system = std::env::var_os("SystemRoot").map(PathBuf::from);
            let install = std::env::current_exe().ok().and_then(|exe| exe.parent().map(PathBuf::from));
            launch_directory(current, system.as_deref(), install.as_deref(), std::env::home_dir())
        }
    };
    if directory.is_dir() { Ok(directory) } else { Err(format!("Not a directory: {}", directory.display())) }
}

// Rust build: started without a directory from Explorer, a launcher or a service, the working directory is the Windows
// or the app's own folder; the AI must not get those as its project, so the home directory opens instead.
fn launch_directory(current: PathBuf, system: Option<&std::path::Path>, install: Option<&std::path::Path>, home: Option<PathBuf>) -> PathBuf {
    let same = |a: &std::path::Path, b: &std::path::Path| a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase();
    let inside = |a: &std::path::Path, b: &std::path::Path| a.to_string_lossy().to_lowercase().starts_with(&b.to_string_lossy().to_lowercase());
    let unsuitable = system.is_some_and(|system| inside(&current, system)) || install.is_some_and(|install| same(&current, install));
    match home {
        Some(home) if unsuitable => home,
        _ => current,
    }
}

// Smoke runs see neither the user's database nor their state: everything lives in a throwaway directory and memory.
fn smoke_backend(root: &std::path::Path) -> Result<(Backend, UnboundedReceiver<Event>), String> {
    let dir = |name: &str| root.join(name);
    let paths = Paths { home: dir("home"), data: dir("data"), cache: dir("cache"), config: dir("config"), state: dir("state"), tmp: dir("tmp") };
    for path in [&paths.home, &paths.data, &paths.cache, &paths.config, &paths.state, &paths.tmp] {
        std::fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let store = Store::open_in_memory().map_err(|e| e.to_string())?;
    Ok(Backend::with_engine(model::backend::offline(Engine::with_store(paths, store))))
}

// `OPENCODE_DESKTOP_SMOKE_*` variables shape the smoke window for screenshots and geometry checks.
fn smoke_var(name: &str) -> Option<String> {
    std::env::var(format!("OPENCODE_DESKTOP_SMOKE_{name}")).ok().filter(|value| !value.is_empty())
}

// "x,y,width,height" as a saved window geometry.
fn parse_bounds(text: &str) -> Option<WindowGeometry> {
    let numbers: Vec<f32> = text.split(',').map(|part| part.trim().parse().ok()).collect::<Option<_>>()?;
    let [x, y, width, height] = numbers[..] else { return None };
    Some(WindowGeometry { x, y, width, height, maximized: false })
}

// The smoke window reopens a seeded session, so its frames cover the engine bridge and every row renderer.
fn smoke_state(backend: &Backend, directory: &std::path::Path) -> Result<Persisted, String> {
    let session = backend.seed_demo(directory)?;
    let mut persisted = Persisted::default();
    persisted.update_project(directory, |project| project.last_session = Some(session));
    persisted.settings.language = Some(smoke_var("LANG").unwrap_or_else(|| "en".into()));
    if let Some(theme) = smoke_var("THEME") {
        persisted.settings.theme = theme;
    }
    persisted.settings.scheme = match smoke_var("SCHEME").as_deref() {
        Some("dark") => Scheme::Dark,
        Some("light") => Scheme::Light,
        _ => Scheme::System,
    };
    persisted.window = smoke_var("BOUNDS").as_deref().and_then(parse_bounds);
    Ok(persisted)
}

fn initial_bounds(saved: Option<WindowGeometry>, cx: &App) -> WindowBounds {
    let Some(saved) = saved.filter(WindowGeometry::is_sane) else {
        return WindowBounds::centered(size(px(1180.), px(800.)), cx);
    };
    let rect = Bounds::new(point(px(saved.x), px(saved.y)), size(px(saved.width), px(saved.height)));
    // A window saved on a display that is gone would open out of reach.
    let reachable = cx.displays().iter().any(|display| display.bounds().intersects(&rect));
    let rect = if reachable { rect } else { Bounds::centered(None, rect.size, cx) };
    if saved.maximized { WindowBounds::Maximized(rect) } else { WindowBounds::Windowed(rect) }
}

fn watch_smoke(cx: &mut App) {
    cx.spawn(async move |cx| {
        let hold = std::env::var("OPENCODE_DESKTOP_SMOKE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(500);
        let ready = || FRAMES.load(Ordering::Relaxed) > 0 && ROWS.load(Ordering::Relaxed) > 0;
        let mut waited = 0;
        while waited < 20_000 && !ready() {
            cx.background_executor().timer(Duration::from_millis(100)).await;
            waited += 100;
        }
        // Give the seeded transcript time to be laid out and painted before judging.
        cx.background_executor().timer(Duration::from_millis(hold)).await;
        let (frames, rows) = (FRAMES.load(Ordering::Relaxed), ROWS.load(Ordering::Relaxed));
        SMOKE_PASSED.store(ready(), Ordering::Relaxed);
        // What the platform actually gave the window, to compare against a requested geometry.
        let bounds = cx.update(|cx| {
            let window = cx.windows().into_iter().next()?;
            window.update(cx, |_, window, cx| (ui::app::geometry(window.window_bounds()), cx.theme().is_dark())).ok()
        });
        if let Some((g, dark)) = bounds {
            println!("smoke: window {},{} {}x{} maximized={} dark={dark}", g.x, g.y, g.width, g.height, g.maximized);
        }
        if ready() {
            println!("smoke: ok ({frames} frames, {rows} transcript rows)");
        } else {
            eprintln!("smoke: failed after {waited}ms ({frames} frames, {rows} transcript rows)");
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
}

struct Fatal(SharedString);

impl Render for Fatal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().p_6().bg(cx.theme().background).child(Alert::error("fatal", self.0.clone()).title("opencode"))
    }
}

fn show_fatal(message: String) {
    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
        gpui_kit::init(cx);
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        let options = WindowOptions { window_bounds: Some(WindowBounds::centered(size(px(560.), px(220.)), cx)), ..Default::default() };
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            ui::theme::apply(&Settings::default(), None, Some(window), cx);
            window.set_window_title("opencode");
            cx.new(|_| Fatal(message.into()))
        });
        match opened {
            Ok(_) => cx.activate(true),
            Err(_) => cx.quit(),
        }
    });
}

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(args) if args.help => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let (smoke, links, explicit) = (args.smoke, args.links, args.directory.is_some());
    let requested = args.directory.or_else(|| link_directory(&links));
    let directory = match project_directory(requested) {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };

    // Smoke runs neither hand off to nor accept starts from a running app.
    let paths = (!smoke).then(Paths::from_env);
    let mut requests = None;
    if let Some(paths) = &paths {
        let request = Request { directory: explicit.then(|| directory.clone()), links: links.clone() };
        match instance::start(&instance::name(&paths.state), &request) {
            instance::Start::Forwarded => return ExitCode::SUCCESS,
            instance::Start::First(listener) => {
                let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
                if let Some(listener) = listener {
                    instance::serve(listener, sender);
                }
                requests = Some(receiver);
            }
        }
    }

    let scratch = smoke.then(|| std::env::temp_dir().join(format!("opencode-desktop-smoke-{}", std::process::id())));
    let started = match (&scratch, paths) {
        (Some(root), _) => smoke_backend(root).and_then(|(backend, events)| {
            let persisted = smoke_state(&backend, &directory)?;
            Ok((backend, events, persisted, Saver::disabled()))
        }),
        (None, paths) => Backend::new(paths.unwrap_or_else(Paths::from_env)).map(|(backend, events)| {
            let file = persist::file(backend.state_dir());
            (backend, events, Persisted::load(&file), Saver::spawn(file))
        }),
    };
    let (backend, events, persisted, saver) = match started {
        Ok(started) => started,
        Err(error) => {
            eprintln!("opencode-desktop: {error}");
            match scratch {
                Some(root) => drop(std::fs::remove_dir_all(root)),
                // A windowed launch has no console to read the reason from.
                None => show_fatal(I18n::new(i18n::detect()).tf("desktop.rs.fatal", &[("error", &error)])),
            }
            return ExitCode::FAILURE;
        }
    };

    if smoke {
        // A hung platform must not leave a window or process behind.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(60));
            eprintln!("smoke: failed, timed out");
            std::process::exit(1);
        });
    }

    // Smoke runs pick their overlay; a real start may offer the portable bash once.
    let view = if smoke { smoke_var("VIEW") } else { backend.offer_bash().then(|| "setup-bash".to_string()) };
    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
        cx.set_app_identity(APP_ID, APP_NAME);
        gpui_kit::init(cx);
        ui::app::bind_keys(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let options = WindowOptions {
            window_bounds: Some(initial_bounds(persisted.window, cx)),
            window_min_size: Some(size(px(720.), px(480.))),
            focus: !smoke,
            ..TitleBar::window_options()
        };
        let alerts: Rc<dyn Alerts> = if smoke { Rc::new(Silent) } else { Rc::new(System::new()) };
        let launch = Launch { directory, persisted, saver, view, alerts, links, requests };
        let opened = gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| AppView::new(backend, events, launch, window, cx)));
        match opened {
            Ok(_) if smoke => watch_smoke(cx),
            Ok(_) => cx.activate(true),
            Err(error) => {
                eprintln!("opencode-desktop: cannot open a window: {error}");
                cx.quit();
            }
        }
    });

    if let Some(root) = scratch {
        let _ = std::fs::remove_dir_all(root);
        return if SMOKE_PASSED.load(Ordering::Relaxed) { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_directory_and_flags() {
        assert_eq!(args(&[]), Ok(Args::default()));
        assert_eq!(args(&["--smoke", "work"]), Ok(Args { directory: Some("work".into()), smoke: true, ..Default::default() }));
        assert!(args(&["--help"]).unwrap().help);
        assert!(args(&["--nope"]).unwrap_err().contains("--nope"));
        assert!(args(&["a", "b"]).is_err());
        // What the protocol handler the installer registers passes: `"opencode-desktop.exe" "%1"`.
        let link = "opencode://open-project?directory=C:%5Cwork";
        assert_eq!(args(&[link]), Ok(Args { links: vec![link.into()], ..Default::default() }));
        assert_eq!(args(&["work", link, link]).unwrap().links.len(), 2);
    }

    #[test]
    fn a_link_alone_opens_its_own_project() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().to_string_lossy().into_owned();
        let missing = dir.path().join("missing").to_string_lossy().into_owned();
        let links = [format!("opencode://open-project?directory={missing}"), format!("opencode://new-session?directory={project}&prompt=hi")];
        assert_eq!(link_directory(&links).as_deref(), Some(dir.path()));
        assert_eq!(link_directory(&links[..1]), None);
        assert_eq!(link_directory(&["opencode://other?directory=/".to_string()]), None);
    }

    #[test]
    fn project_directory_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(project_directory(Some(dir.path().to_path_buf())).unwrap(), dir.path());
        assert!(project_directory(Some(dir.path().join("missing"))).unwrap_err().starts_with("Not a directory"));
        assert!(project_directory(None).unwrap().is_absolute());
    }

    #[test]
    fn launches_from_system_or_install_folders_open_home() {
        let home = || Some(PathBuf::from(r"C:\Users\me"));
        let (system, install) = (PathBuf::from(r"C:\Windows"), PathBuf::from(r"C:\Users\me\AppData\Local\Programs\opencode-desktop-rs"));
        let pick = |current: &str| launch_directory(PathBuf::from(current), Some(&system), Some(&install), home());
        assert_eq!(pick(r"C:\Windows\System32"), PathBuf::from(r"C:\Users\me"));
        assert_eq!(pick(r"c:\users\me\appdata\local\programs\opencode-desktop-rs"), PathBuf::from(r"C:\Users\me"));
        assert_eq!(pick(r"D:\work\app"), PathBuf::from(r"D:\work\app"));
        assert_eq!(launch_directory(PathBuf::from(r"C:\Windows"), Some(&system), None, None), PathBuf::from(r"C:\Windows"));
    }

    #[test]
    fn smoke_backend_stays_inside_its_scratch_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (backend, _events) = smoke_backend(dir.path()).unwrap();
        let persisted = smoke_state(&backend, dir.path()).unwrap();
        assert!(persisted.project(dir.path()).last_session.is_some());
        assert_eq!(persisted.settings.language.as_deref(), Some("en"), "smoke text does not depend on the machine's locale");
        assert_eq!(parse_bounds("40, 60,900,700"), Some(WindowGeometry { x: 40.0, y: 60.0, width: 900.0, height: 700.0, maximized: false }));
        assert_eq!((parse_bounds("1,2,3"), parse_bounds("a,b,c,d")), (None, None));
        assert!(backend.state_dir().starts_with(dir.path()));
        assert!(!persist::file(backend.state_dir()).exists());
    }
}
