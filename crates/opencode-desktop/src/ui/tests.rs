// Headless UI integration tests: the real view in a test window, a throwaway engine, no display.
use super::*;
use crate::model::attention::Found;
use crate::model::question::QuestionView;
use crate::model::state;
use std::path::Path;
use crate::ui::alerts::Recorder;
use crate::model::settings::{Scheme, Toggle};
use crate::model::state::{AgentItem, Catalog, ChildUpdate, CommandItem, ModelGroup, ModelItem, Outcome, PermissionView, SessionItem, Usage};
use crate::model::transcript::ToolStatus;
use gpui_kit::base::Root;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{Bounds, Point, ScrollDelta, TestAppContext, WindowHandle, WindowOptions, point, size};
use opencode_core::paths::Paths;
use opencode_session::{Engine, QuestionInfo, QuestionOption, Reply};
use opencode_storage::{JsonMap, Part, PartBody, Store, Todo, ToolPart, ToolState};
use serde_json::json;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

const PATCH: &str = "Index: f.txt\n===================================================================\n--- f.txt\n+++ f.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";

struct Fixture {
    _root: tempfile::TempDir,
    // Keeps the view's event pump alive; tests feed events through `apply` instead.
    _events: UnboundedSender<Event>,
    requests: UnboundedSender<Request>,
    project: PathBuf,
    window: WindowHandle<Root>,
    view: Entity<AppView>,
    // What would have been played and posted; nothing reaches a device or the notification center.
    alerts: Rc<Recorder>,
}

fn open(cx: &mut TestAppContext) -> Fixture {
    open_with(cx, Persisted::default())
}

fn open_with(cx: &mut TestAppContext, persisted: Persisted) -> Fixture {
    open_linked(cx, persisted, |_| Vec::new())
}

// `links` builds the start's `opencode://` arguments from the project directory.
fn open_linked(cx: &mut TestAppContext, mut persisted: Persisted, links: impl FnOnce(&Path) -> Vec<String>) -> Fixture {
    cx.update(|cx| {
        gpui_kit::init(cx);
        bind_keys(cx);
    });
    let root = tempfile::tempdir().unwrap();
    let dir = |name: &str| {
        let path = root.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    };
    let paths = Paths { home: dir("home"), data: dir("data"), cache: dir("cache"), config: dir("config"), state: dir("state"), tmp: dir("tmp") };
    let project = dir("project");
    // The backend's own events are dropped so nothing arrives at a time the test does not control.
    let (backend, _) = Backend::with_engine(crate::model::backend::offline(Engine::with_store(paths, Store::open_in_memory().unwrap())));
    let (sender, events) = unbounded_channel();
    let (requests, incoming) = unbounded_channel();
    persisted.settings.language.get_or_insert_with(|| "en".into());
    let alerts = Rc::new(Recorder::default());
    let launch = Launch {
        directory: project.clone(),
        persisted,
        saver: Saver::disabled(),
        view: None,
        alerts: Rc::new(alerts.clone()),
        links: links(&project),
        requests: Some(incoming),
    };
    let (window, view) = cx.update(|cx| {
        let bounds = Bounds { origin: Point::default(), size: size(px(1200.), px(800.)) };
        let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
        let (window, view) =
            gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| AppView::new(backend, events, launch, window, cx))).unwrap();
        (window.downcast::<Root>().unwrap(), view)
    });
    Fixture { _root: root, _events: sender, requests, project, window, view, alerts }
}

impl Fixture {
    fn with<R>(&self, cx: &mut TestAppContext, run: impl FnOnce(&mut AppView, &mut Window, &mut Context<AppView>) -> R) -> R {
        let view = self.view.clone();
        cx.update_window(self.window.into(), |_, window, cx| view.update(cx, |view, cx| run(view, window, cx))).unwrap()
    }

    fn window<R>(&self, cx: &mut TestAppContext, run: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.render_frame(cx);
            run(window, cx)
        })
        .unwrap()
    }

    fn apply(&self, cx: &mut TestAppContext, events: Vec<Event>) {
        self.with(cx, |view, window, cx| view.apply(events, window, cx));
    }

    fn set_active(&self, cx: &mut TestAppContext, active: bool) {
        if active {
            cx.update_window(self.window.into(), |_, window, _| window.activate_window()).unwrap();
        } else {
            gpui_kit::VisualTestContext::from_window(self.window.into(), cx).deactivate_window();
        }
        cx.run_until_parked();
        assert_eq!(self.with(cx, |_, window, _| window.is_window_active()), active);
    }

    fn signal(&self, kind: Kind, title: Option<&str>) -> Event {
        let session = title.map(|title| Found { title: title.into(), child: false });
        Event::Attention(Signal { kind, directory: self.project.clone(), session_id: Some("ses_1".into()), session })
    }

    fn notified(&self) -> Vec<(String, String)> {
        self.alerts.notified.borrow().iter().map(|n| (n.title.clone(), n.body.clone())).collect()
    }

    fn composer(&self, cx: &mut TestAppContext) -> String {
        self.with(cx, |view, _, cx| view.composer.read(cx).value().to_string())
    }

    fn sessions(&self, cx: &mut TestAppContext, sessions: Vec<SessionItem>) {
        self.apply(cx, vec![Event::Sessions { directory: self.project.clone(), sessions }]);
    }

    // A stored session on screen, as if it had been opened from the sidebar.
    fn show(&self, cx: &mut TestAppContext, id: &str, rows: Vec<Row>) {
        self.sessions(cx, vec![item(id, "Shown")]);
        self.with(cx, |view, window, cx| view.open_session(id, window, cx));
        self.apply(cx, vec![Event::Opened { session_id: id.into(), rows, model: None, agent: None }]);
    }
}

fn item(id: &str, title: &str) -> SessionItem {
    SessionItem { id: id.into(), title: title.into(), updated: 1, parent: None }
}

fn user_row(id: &str, text: &str) -> Row {
    Row { id: id.into(), kind: RowKind::User { text: text.into(), files: vec![] } }
}

fn text_row(id: &str, text: &str) -> Row {
    Row { id: id.into(), kind: RowKind::Text { markdown: text.into(), streaming: false } }
}

fn tool_part(id: &str, tool: &str, state: ToolState) -> Box<Part> {
    let body = PartBody::Tool(ToolPart { call_id: "call_1".into(), tool: tool.into(), state, metadata: None });
    Box::new(Part { id: id.into(), session_id: "ses_1".into(), message_id: "msg_a".into(), body })
}

fn object(value: serde_json::Value) -> JsonMap {
    value.as_object().cloned().unwrap_or_default()
}

fn catalog() -> Catalog {
    Catalog {
        models: vec![ModelGroup {
            provider: "Acme".into(),
            id: "acme".into(),
            models: vec![
                model("acme/large", "Large", 100_000, &["high", "max"]),
                model("acme/small", "Small", 0, &[]),
            ],
        }],
        agents: vec![AgentItem { name: "build".into(), description: "".into() }, AgentItem { name: "plan".into(), description: "".into() }],
        default_model: Some("acme/large".into()),
        default_agent: Some("build".into()),
        ..Default::default()
    }
}

fn model(spec: &str, name: &str, context: u64, variants: &[&str]) -> ModelItem {
    let (provider_id, _) = spec.split_once('/').unwrap();
    ModelItem {
        spec: spec.into(),
        name: name.into(),
        context,
        provider_id: provider_id.into(),
        provider: "Acme".into(),
        shown: true,
        variants: variants.iter().map(|v| (*v).into()).collect(),
        ..Default::default()
    }
}

fn kinds(view: &AppView) -> Vec<&'static str> {
    view.rows
        .iter()
        .map(|row| match &row.kind {
            RowKind::User { .. } => "user",
            RowKind::Text { .. } => "text",
            RowKind::Reasoning { .. } => "reasoning",
            RowKind::Tool(_) => "tool",
            RowKind::Error { .. } => "error",
            RowKind::Retry { .. } => "retry",
            RowKind::Working { .. } => "working",
            RowKind::Compaction => "compaction",
        })
        .collect()
}

#[gpui_kit::test]
fn enter_sends_the_draft_and_shift_enter_adds_a_line(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.window(cx, |window, cx| {
        window.input("first", cx);
        window.press("shift-enter", cx);
        window.input("second", cx);
    });
    assert_eq!(fx.composer(cx), "first\nsecond");
    fx.with(cx, |view, _, _| assert_eq!(view.state.current, None));

    fx.window(cx, |window, cx| window.press("enter", cx));
    fx.with(cx, |view, _, cx| {
        assert!(view.state.current.is_some(), "the draft became a session");
        assert!(view.state.current_running());
        assert_eq!(view.composer.read(cx).value().as_ref(), "");
        assert_eq!(view.state.sessions[0].title.as_ref(), "first");
        assert_eq!(kinds(view), ["user", "working"]);
        assert_eq!(view.scroller.read(cx).item_count(), 2);
        assert_eq!(view.history.entries(), ["first\nsecond"]);
        assert_eq!(view.persisted.history, ["first\nsecond"]);
    });
    // The stop button replaced send while the prompt runs.
    fx.window(cx, |window, _| {
        assert!(window.try_find("stop").is_some());
        assert!(window.try_find("send").is_none());
    });
}

#[gpui_kit::test]
fn transcript_rows_line_up_with_the_composer(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.with(cx, |view, _, cx| {
        view.state.begin_prompt("ses_1", "hi", vec![], 1);
        view.sync(cx);
    });
    let session = || "ses_1".to_string();
    fx.apply(cx, vec![Event::Reasoning { session_id: session(), delta: "plan".into() }, Event::Text { session_id: session(), delta: "Hello there".into() }]);
    let ids: Vec<String> = fx.with(cx, |view, _, _| view.rows.iter().map(|row| row.id.to_string()).collect());
    assert!(ids.len() >= 3, "{ids:?}");
    fx.window(cx, |window, _| {
        let composer = window.find("composer-column").bounds();
        // Every row, the animated newest ones included, starts where the composer starts.
        for id in &ids {
            let bounds = window.find(SharedString::from(format!("column-{id}"))).bounds();
            assert_eq!(bounds.origin.x, composer.origin.x, "{id}: {bounds:?} vs {composer:?}");
        }
    });
}

#[gpui_kit::test]
fn engine_events_patch_the_transcript_and_tool_cards_expand(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.with(cx, |view, _, cx| {
        view.state.begin_prompt("ses_1", "change b", vec![], 1);
        view.sync(cx);
    });
    let session = || "ses_1".to_string();
    fx.apply(
        cx,
        vec![
            Event::Reasoning { session_id: session(), delta: "plan".into() },
            Event::Text { session_id: session(), delta: "Edit".into() },
            Event::Text { session_id: session(), delta: "ing `f.txt`".into() },
        ],
    );
    fx.with(cx, |view, _, cx| {
        assert_eq!(kinds(view), ["user", "reasoning", "text", "working"]);
        assert_eq!(view.scroller.read(cx).item_count(), 4);
    });

    let input = object(json!({"filePath": "f.txt"}));
    let running = ToolState::pending(input.clone(), "").to_running(input, 1);
    fx.apply(cx, vec![Event::Tool { session_id: session(), part: tool_part("prt_tool", "edit", running.clone()) }]);
    let done = running.to_completed("Edit applied", "f.txt", object(json!({"diff": PATCH})), None, 9);
    fx.apply(cx, vec![Event::Tool { session_id: session(), part: tool_part("prt_tool", "edit", done) }]);
    fx.with(cx, |view, _, cx| {
        assert_eq!(kinds(view), ["user", "reasoning", "text", "tool", "working"]);
        assert_eq!(view.scroller.read(cx).item_count(), 5);
        assert!(matches!(&view.rows[3].kind, RowKind::Tool(tool) if tool.status == ToolStatus::Completed));
    });

    // Collapsed by default; the header toggles the body and prepares the diff view.
    fx.window(cx, |window, cx| window.click("prt_tool-header", cx));
    fx.with(cx, |view, _, _| {
        assert!(view.expanded.contains("prt_tool"));
        assert!(view.diffs.contains_key("prt_tool"), "the tool's patch parsed into a diff");
    });
    fx.window(cx, |window, cx| window.click("prt_tool-header", cx));
    fx.with(cx, |view, _, _| assert!(!view.expanded.contains("prt_tool")));

    fx.apply(cx, vec![Event::Finished { session_id: session(), rows: None, outcome: Outcome::Failed("HTTP 500".into()) }]);
    fx.with(cx, |view, _, cx| {
        assert!(!view.state.current_running());
        assert_eq!(kinds(view).last(), Some(&"error"));
        assert_eq!(view.scroller.read(cx).item_count(), view.rows.len());
    });
    fx.window(cx, |window, _| assert!(window.try_find("send").is_some()));
}

#[gpui_kit::test]
fn tool_cards_and_reasoning_follow_the_settings(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.with(cx, |view, _, cx| {
        view.state.begin_prompt("ses_1", "go", vec![], 1);
        view.sync(cx);
    });
    let session = || "ses_1".to_string();
    let command = object(json!({"command": "ls"}));
    let ran = ToolState::pending(command.clone(), "").to_running(command, 1).to_completed("a\nb", "ls", JsonMap::new(), None, 5);
    let edited = ToolState::pending(JsonMap::new(), "").to_completed("ok", "f.txt", object(json!({"diff": PATCH})), None, 5);
    fx.apply(
        cx,
        vec![
            Event::Reasoning { session_id: session(), delta: "hm".into() },
            Event::Tool { session_id: session(), part: tool_part("prt_bash", "bash", ran) },
            Event::Tool { session_id: session(), part: tool_part("prt_edit", "edit", edited) },
        ],
    );
    fx.with(cx, |view, _, cx| {
        assert_eq!(kinds(view), ["user", "reasoning", "tool", "tool", "working"]);
        assert!(view.expanded.is_empty());

        view.set_toggle(Toggle::ShellExpanded, true, cx);
        assert!(view.expanded.contains("prt_bash") && !view.expanded.contains("prt_edit"));
        view.set_toggle(Toggle::EditExpanded, true, cx);
        assert!(view.diffs.contains_key("prt_edit"), "an edit opened by default gets its diff");

        view.set_toggle(Toggle::ShowReasoning, false, cx);
        assert_eq!(kinds(view), ["user", "tool", "tool", "working"]);
        assert_eq!(view.scroller.read(cx).item_count(), 4);
    });
    // A card that is open by default closes on a click.
    fx.window(cx, |window, cx| window.click("prt_bash-header", cx));
    fx.with(cx, |view, _, _| assert!(!view.expanded.contains("prt_bash") && view.expanded.contains("prt_edit")));
}

fn permission(id: u64) -> PermissionView {
    PermissionView {
        id,
        session_id: "ses_1".into(),
        permission: "edit".into(),
        patterns: vec!["f.txt".into()],
        detail: Some("f.txt".into()),
        diff: Some(PATCH.into()),
    }
}

#[gpui_kit::test]
fn permission_card_answers_the_engine(cx: &mut TestAppContext) {
    let fx = open(cx);
    let mut answer = fx.with(cx, |view, _, cx| {
        view.state.begin_prompt("ses_1", "change b", vec![], 1);
        view.sync(cx);
        view.backend.expect_reply(7)
    });
    fx.window(cx, |window, _| assert!(window.try_find("permission-once").is_none()));

    fx.apply(cx, vec![Event::Permission(permission(7))]);
    fx.with(cx, |view, _, _| assert!(view.diffs.contains_key("permission-7")));
    fx.window(cx, |window, cx| {
        assert!(window.try_find("permission-reject").is_some());
        assert!(window.try_find("permission-always").is_some());
        window.click("permission-once", cx);
    });
    assert_eq!(answer.try_recv(), Ok(Reply::Once));

    fx.apply(cx, vec![Event::PermissionResolved(7)]);
    fx.with(cx, |view, _, _| assert!(view.diffs.is_empty()));
    fx.window(cx, |window, _| assert!(window.try_find("permission-once").is_none()));
}

#[gpui_kit::test]
fn auto_accept_answers_pending_and_future_permissions(cx: &mut TestAppContext) {
    let fx = open(cx);
    let (mut first, mut second) = fx.with(cx, |view, _, cx| {
        view.state.begin_prompt("ses_1", "change b", vec![], 1);
        view.sync(cx);
        (view.backend.expect_reply(1), view.backend.expect_reply(2))
    });
    fx.apply(cx, vec![Event::Permission(permission(1))]);
    fx.window(cx, |window, cx| {
        assert!(window.try_find("permission-card").is_some());
        window.press("ctrl-shift-a", cx);
    });
    assert_eq!(first.try_recv(), Ok(Reply::Once), "the request on screen is answered when the mode turns on");
    fx.with(cx, |view, _, _| assert!(view.persisted.settings.auto_accept && view.state.permissions.is_empty()));

    fx.apply(cx, vec![Event::Permission(permission(2))]);
    assert_eq!(second.try_recv(), Ok(Reply::Once));
    fx.window(cx, |window, _| assert!(window.try_find("permission-card").is_none()));
}

#[gpui_kit::test]
fn question_card_collects_choices_and_typed_answers(cx: &mut TestAppContext) {
    let fx = open(cx);
    let option = |label: &str| QuestionOption { label: label.into(), description: format!("{label} it") };
    let question = |header: &str, multiple| QuestionInfo {
        question: format!("{header}?"),
        header: header.into(),
        options: vec![option("Red"), option("Green"), option("Blue")],
        multiple,
        custom: None,
    };
    let view = QuestionView { id: 4, session_id: "ses_1".into(), questions: vec![question("One", None), question("Many", Some(true))] };
    let mut answer = fx.with(cx, |view, _, cx| {
        view.state.begin_prompt("ses_1", "ask me", vec![], 1);
        view.sync(cx);
        view.backend.expect_answer(4)
    });
    fx.apply(cx, vec![Event::Question(view), Event::Permission(permission(9))]);
    fx.window(cx, |window, cx| {
        assert!(window.try_find("question-card").is_some());
        assert!(window.try_find("permission-card").is_none(), "one request at a time");
        window.click("question-0-0", cx);
        window.click("question-0-1", cx);
        window.click("question-submit", cx);
    });
    assert!(answer.try_recv().is_err(), "the second question is unanswered");
    fx.window(cx, |window, cx| {
        window.click("question-1-2", cx);
        window.click("question-1-0", cx);
    });
    fx.with(cx, |view, window, cx| {
        assert_eq!(view.question_inputs.len(), 2);
        view.question_inputs[1].update(cx, |input, cx| input.set_value("Purple", window, cx));
    });
    fx.window(cx, |window, cx| window.click("question-submit", cx));
    assert_eq!(answer.try_recv(), Ok(Some(vec![vec!["Green".to_string()], vec!["Red".into(), "Blue".into(), "Purple".into()]])));

    fx.apply(cx, vec![Event::QuestionResolved(4)]);
    fx.with(cx, |view, _, _| assert!(view.question.is_none() && view.question_inputs.is_empty()));
    fx.window(cx, |window, _| assert!(window.try_find("permission-card").is_some()));

    // Dismissing answers with nothing.
    let mut dismissed = fx.with(cx, |view, _, _| view.backend.expect_answer(5));
    let again = QuestionView { id: 5, session_id: "ses_1".into(), questions: vec![question("One", None)] };
    fx.apply(cx, vec![Event::Question(again)]);
    fx.window(cx, |window, cx| window.click("question-dismiss", cx));
    assert_eq!(dismissed.try_recv(), Ok(None));
}

#[gpui_kit::test]
fn sidebar_opens_sessions_nests_children_and_filters(cx: &mut TestAppContext) {
    let fx = open(cx);
    let child = SessionItem { id: "ses_c".into(), title: "Explore".into(), updated: 1, parent: Some("ses_b".into()) };
    fx.sessions(cx, vec![item("ses_a", "Alpha"), item("ses_b", "Beta"), child]);
    fx.with(cx, |view, _, _| {
        assert_eq!(view.state.current, None, "nothing to restore on a first launch");
        assert_eq!(view.sidebar.iter().map(|r| (r.session.id.as_ref(), r.depth)).collect::<Vec<_>>(), [("ses_a", 0), ("ses_b", 0), ("ses_c", 1)]);
    });

    fx.window(cx, |window, cx| window.click("session-ses_b", cx));
    fx.with(cx, |view, _, _| {
        assert_eq!(view.state.current.as_deref(), Some("ses_b"));
        assert!(view.state.loading);
        assert_eq!(view.state.current_title().as_ref(), "Beta");
        assert_eq!(view.persisted.project(&view.state.directory).last_session.as_deref(), Some("ses_b"));
    });

    let rows = vec![Row::error("row_1", "old failure"), Row::error("row_2", "another")];
    fx.apply(cx, vec![Event::Opened { session_id: "ses_b".into(), rows, model: None, agent: None }]);
    fx.with(cx, |view, _, cx| {
        assert!(!view.state.loading);
        assert_eq!(view.scroller.read(cx).item_count(), 2);
    });

    // A subagent session opens like any other but cannot be prompted.
    fx.window(cx, |window, cx| {
        window.click("session-ses_c", cx);
        assert!(window.try_find("back-to-parent").is_some());
        assert!(window.try_find("send").is_none());
        window.click("back-to-parent", cx);
    });
    fx.with(cx, |view, _, _| assert_eq!(view.state.current.as_deref(), Some("ses_b")));

    fx.window(cx, |window, cx| window.click("new-session", cx));
    fx.with(cx, |view, _, cx| {
        assert_eq!(view.state.current, None);
        assert!(view.rows.is_empty());
        assert_eq!(view.scroller.read(cx).item_count(), 0);
    });

    // Ctrl+F focuses the search box; typing filters the list.
    fx.window(cx, |window, cx| {
        window.press("ctrl-f", cx);
        window.input("alp", cx);
    });
    fx.with(cx, |view, _, _| {
        assert_eq!(view.state.filter, "alp");
        assert_eq!(view.sidebar.iter().map(|r| r.session.id.as_ref()).collect::<Vec<_>>(), ["ses_a"]);
    });
    fx.window(cx, |window, _| {
        assert!(window.try_find("session-ses_a").is_some());
        assert!(window.try_find("session-ses_b").is_none());
    });
}

#[gpui_kit::test]
fn last_session_is_restored_and_pickers_follow_the_catalog(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.with(cx, |view, _, _| {
        let directory = view.state.directory.clone();
        view.persisted.update_project(&directory, |project| project.last_session = Some("ses_b".into()));
        view.restore = true;
    });
    fx.sessions(cx, vec![item("ses_a", "ses_a"), item("ses_b", "ses_b")]);
    fx.with(cx, |view, _, _| assert_eq!(view.state.current.as_deref(), Some("ses_b")));

    fx.apply(cx, vec![Event::Catalog { directory: fx.project.clone(), catalog: catalog() }]);
    fx.with(cx, |view, _, cx| {
        assert_eq!(view.state.model_label().as_deref(), Some("Large"));
        assert_eq!(view.state.chip_label().as_deref(), Some("Acme / Large"));
        assert_eq!(view.agents.read(cx).selected_value().as_deref(), Some("build"));
        assert_eq!(view.state.model, None, "showing the default must not pin it");
    });

    // Opening a session adopts the model it last used.
    fx.apply(cx, vec![Event::Opened { session_id: "ses_b".into(), rows: vec![], model: Some("acme/small".into()), agent: Some("plan".into()) }]);
    fx.with(cx, |view, _, cx| {
        assert_eq!(view.state.effective_model(), Some("acme/small"));
        assert_eq!(view.agents.read(cx).selected_value().as_deref(), Some("plan"));
    });

    // Ctrl+. cycles the agent and the picker follows.
    fx.window(cx, |window, cx| window.press("ctrl-.", cx));
    fx.with(cx, |view, _, cx| {
        assert_eq!(view.state.agent.as_deref(), Some("build"));
        assert_eq!(view.agents.read(cx).selected_value().as_deref(), Some("build"));
    });
    fx.window(cx, |window, cx| window.press("ctrl-shift-.", cx));
    fx.with(cx, |view, _, _| assert_eq!(view.state.agent.as_deref(), Some("plan")));
}

#[gpui_kit::test]
fn the_model_picker_opens_on_a_click_and_selects(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.apply(cx, vec![Event::Catalog { directory: fx.project.clone(), catalog: catalog() }]);
    fx.window(cx, |window, cx| {
        assert!(window.try_find("model-menu").is_none());
        window.click("model-picker", cx);
    });
    fx.window(cx, |window, cx| {
        assert!(window.try_find("model-menu").is_some(), "a mouse click opens the list");
        assert!(window.try_find("model-connect").is_some() && window.try_find("model-manage").is_some());
        window.input("Small", cx);
    });
    cx.run_until_parked();
    fx.with(cx, |view, _, _| assert_eq!(view.model_active.as_deref(), Some("acme/small"), "the search highlights the first match"));
    fx.window(cx, |window, cx| window.press("enter", cx));
    fx.window(cx, |window, _| assert!(window.try_find("model-menu").is_none()));
    fx.with(cx, |view, _, _| {
        assert!(!view.model_menu);
        assert_eq!(view.state.model.as_deref(), Some("acme/small"));
        assert_eq!(view.persisted.project(&view.state.directory).model.as_deref(), Some("acme/small"));
    });
}

#[gpui_kit::test]
fn slash_popup_lists_commands_and_runs_or_inserts_them(cx: &mut TestAppContext) {
    let fx = open(cx);
    let review = CommandItem { name: "review".into(), description: "Review changes".into(), badge: "prompt.slash.badge.custom" };
    fx.apply(cx, vec![Event::Commands { directory: fx.project.clone(), commands: vec![review] }]);
    fx.window(cx, |window, cx| window.input("/", cx));
    fx.with(cx, |view, _, _| {
        let popup = view.popup.as_ref().expect("typing a slash opens the popup");
        let names: Vec<&str> = popup.items.iter().map(|i| i.value.as_ref()).collect();
        assert_eq!(names, ["new", "undo", "redo", "compact", "fork", "model", "agent", "review"]);
        assert_eq!(popup.selected, 0);
    });
    fx.window(cx, |window, cx| {
        assert!(window.try_find("prompt-popup").is_some());
        window.input("re", cx);
    });
    fx.with(cx, |view, _, _| {
        let names: Vec<&str> = view.popup.as_ref().unwrap().items.iter().map(|i| i.value.as_ref()).collect();
        // Then the commands other agents call /rewind and /reset.
        assert_eq!(names, ["redo", "review", "new", "undo"]);
    });

    // Arrow keys move the selection instead of the caret; Enter inserts the custom command.
    fx.window(cx, |window, cx| {
        window.press("down", cx);
        window.press("enter", cx);
    });
    assert_eq!(fx.composer(cx), "/review ");
    fx.with(cx, |view, _, _| {
        assert!(view.popup.is_none());
        assert_eq!(view.state.current, None, "nothing was sent");
    });

    // Escape closes the popup without touching the text.
    fx.with(cx, |view, window, cx| view.set_composer("", window, cx));
    // (Input events are delivered when the update that typed them ends, hence the separate steps.)
    fx.window(cx, |window, cx| window.input("/zz", cx));
    fx.with(cx, |view, _, _| assert!(view.popup.as_ref().is_some_and(|popup| popup.items.is_empty())));
    fx.window(cx, |window, cx| window.press("escape", cx));
    fx.with(cx, |view, _, _| assert!(view.popup.is_none()));
    assert_eq!(fx.composer(cx), "/zz");

    // A built-in runs at once: `/new` from a session returns to the draft.
    fx.show(cx, "ses_1", vec![user_row("msg_1", "hello")]);
    fx.window(cx, |window, cx| window.input("/ne", cx));
    fx.window(cx, |window, cx| window.click("popup-0", cx));
    fx.with(cx, |view, _, _| assert_eq!(view.state.current, None));
    assert_eq!(fx.composer(cx), "/zz", "the new-session draft is back");
}

#[gpui_kit::test]
fn mention_popup_attaches_project_files(cx: &mut TestAppContext) {
    let fx = open(cx);
    let files = vec!["README.md".to_string(), "src/domain.rs".into(), "src/main.rs".into()];
    fx.apply(cx, vec![Event::Files { directory: fx.project.clone(), files }]);
    fx.window(cx, |window, cx| window.input("look at @mai", cx));
    fx.with(cx, |view, _, _| {
        let popup = view.popup.as_ref().expect("an @ opens the file popup");
        assert_eq!(popup.items[0].value.as_ref(), "src/main.rs");
        assert_eq!(popup.items[0].detail.as_ref(), "src");
    });
    fx.window(cx, |window, cx| window.press("tab", cx));
    assert_eq!(fx.composer(cx), "look at @src/main.rs ");
    fx.with(cx, |view, _, cx| {
        assert!(view.popup.is_none());
        assert_eq!(view.attachments.len(), 1);
        assert_eq!(view.attachments[0].name.as_ref(), "main.rs");
        assert!(view.attachments[0].url.starts_with("file:///") && view.attachments[0].url.ends_with("/project/src/main.rs"));
        assert_eq!(view.composer.read(cx).cursor(), "look at @src/main.rs ".len());
    });

    // The chip can be removed again; sending passes the attachment names to the transcript.
    fx.window(cx, |window, cx| {
        assert!(window.try_find("attachments").is_some());
        window.click("attachment-0-remove", cx);
    });
    fx.with(cx, |view, _, _| assert!(view.attachments.is_empty()));
    fx.window(cx, |window, cx| window.input("and @READ", cx));
    fx.window(cx, |window, cx| window.press("enter", cx));
    assert_eq!(fx.composer(cx), "look at @src/main.rs and @README.md ");
    fx.window(cx, |window, cx| window.press("enter", cx));
    fx.with(cx, |view, _, _| {
        assert!(view.state.current_running());
        assert!(matches!(&view.rows[0].kind, RowKind::User { text, files } if text.contains("@README.md") && files.len() == 1 && files[0].as_ref() == "README.md"));
        assert!(view.attachments.is_empty());
    });
}

#[gpui_kit::test]
fn dropped_files_become_attachments(cx: &mut TestAppContext) {
    let fx = open(cx);
    let picture = fx.project.join("shot.png");
    std::fs::write(&picture, b"png").unwrap();
    fx.with(cx, |view, _, cx| {
        view.attach_paths(&[picture.clone(), view.state.directory.clone(), picture.clone()], cx);
        assert_eq!(view.attachments.len(), 1, "folders and duplicates are skipped");
        assert_eq!((view.attachments[0].name.as_ref(), view.attachments[0].mime.as_ref()), ("shot.png", "image/png"));
    });
    // An attachment alone is enough to send.
    fx.window(cx, |window, cx| window.click("send", cx));
    fx.with(cx, |view, _, _| assert!(view.state.current_running() && view.attachments.is_empty()));
}

#[gpui_kit::test]
fn arrow_keys_walk_the_prompt_history(cx: &mut TestAppContext) {
    let persisted = Persisted { history: vec!["first prompt".into(), "second prompt".into()], ..Default::default() };
    let fx = open_with(cx, persisted);
    fx.window(cx, |window, cx| window.press("up", cx));
    assert_eq!(fx.composer(cx), "second prompt");
    fx.window(cx, |window, cx| window.press("up", cx));
    assert_eq!(fx.composer(cx), "first prompt");
    fx.window(cx, |window, cx| {
        window.press("up", cx);
        window.press("down", cx);
    });
    assert_eq!(fx.composer(cx), "second prompt");
    fx.window(cx, |window, cx| window.press("down", cx));
    assert_eq!(fx.composer(cx), "");

    // Once the user types, the arrows belong to the text again.
    fx.window(cx, |window, cx| {
        window.input("draft", cx);
        window.press("up", cx);
    });
    assert_eq!(fx.composer(cx), "draft");
    fx.with(cx, |view, _, _| assert!(!view.history.browsing()));
}

#[gpui_kit::test]
fn drafts_are_kept_per_session(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.sessions(cx, vec![item("ses_a", "Alpha"), item("ses_b", "Beta")]);
    fx.window(cx, |window, cx| {
        window.input("for nobody yet", cx);
        window.click("session-ses_a", cx);
    });
    assert_eq!(fx.composer(cx), "");
    fx.window(cx, |window, cx| {
        window.press("ctrl-l", cx);
        window.input("for alpha", cx);
        window.click("session-ses_b", cx);
    });
    assert_eq!(fx.composer(cx), "");
    fx.window(cx, |window, cx| window.click("session-ses_a", cx));
    assert_eq!(fx.composer(cx), "for alpha");
    fx.window(cx, |window, cx| window.click("new-session", cx));
    assert_eq!(fx.composer(cx), "for nobody yet");
    fx.with(cx, |view, _, _| {
        let saved = view.persisted.project(&view.state.directory);
        assert_eq!((saved.draft(None), saved.draft(Some("ses_a")), saved.draft(Some("ses_b"))), ("for nobody yet", "for alpha", ""));
    });

    // Sending clears the draft slot.
    fx.window(cx, |window, cx| {
        window.press("ctrl-l", cx);
        window.press("enter", cx);
    });
    fx.with(cx, |view, _, _| assert_eq!(view.persisted.project(&view.state.directory).draft(None), ""));
}

#[gpui_kit::test]
fn message_actions_copy_revert_and_fork(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.show(cx, "ses_1", vec![user_row("msg_1", "first question"), text_row("prt_1", "an **answer**"), user_row("msg_2", "second question")]);
    fx.window(cx, |window, cx| {
        window.click("prt_1-copy", cx);
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some("an **answer**"));
        window.click("msg_1-copy", cx);
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some("first question"));
        assert!(window.try_find("msg_2-fork").is_some());
        // Reverting hands the message text back for editing.
        window.click("msg_2-revert", cx);
    });
    assert_eq!(fx.composer(cx), "second question");

    // The engine's answer: one message hidden, with a way back.
    fx.apply(
        cx,
        vec![
            Event::Snapshot { session_id: "ses_1".into(), rows: vec![user_row("msg_1", "first question"), text_row("prt_1", "an **answer**")] },
            Event::Revert { session_id: "ses_1".into(), hidden: 1 },
        ],
    );
    fx.window(cx, |window, _| {
        assert!(window.try_find("revert-dock").is_some());
        assert!(window.try_find("revert-restore").is_some());
        assert!(window.try_find("msg_2-revert").is_none());
    });
    fx.apply(cx, vec![Event::Revert { session_id: "ses_1".into(), hidden: 0 }]);
    fx.window(cx, |window, _| assert!(window.try_find("revert-dock").is_none()));

    // A fork reported by the engine is opened right away.
    fx.apply(cx, vec![Event::Forked { directory: fx.project.clone(), session: item("ses_fork", "Shown (fork #1)") }]);
    fx.with(cx, |view, _, _| {
        assert_eq!(view.state.current.as_deref(), Some("ses_fork"));
        assert_eq!(view.state.sessions[0].id.as_ref(), "ses_fork");
    });
    fx.window(cx, |window, cx| window.click("session-ses_1", cx));
    fx.apply(cx, vec![Event::Opened { session_id: "ses_1".into(), rows: vec![user_row("msg_1", "first question")], model: None, agent: None }]);

    // While a prompt runs the history cannot be rewritten.
    fx.with(cx, |view, _, cx| {
        view.state.begin_prompt("ses_1", "more", vec![], 2);
        view.sync(cx);
    });
    fx.window(cx, |window, _| {
        assert!(window.try_find("msg_1-revert").is_none() && window.try_find("msg_1-fork").is_none());
        assert!(window.try_find("msg_1-copy").is_some());
    });
}

#[gpui_kit::test]
fn task_cards_show_subagent_activity_and_open_the_child_session(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.with(cx, |view, _, cx| {
        view.state.begin_prompt("ses_1", "delegate", vec![], 1);
        view.sync(cx);
    });
    let input = object(json!({"description": "Explore the parser"}));
    let mut running = ToolState::pending(input.clone(), "").to_running(input, 1);
    if let ToolState::Running(state) = &mut running {
        state.metadata = Some(object(json!({"sessionId": "ses_child"})));
    }
    let child = |update| Event::Child { child_id: "ses_child".into(), update };
    fx.apply(
        cx,
        vec![
            Event::Tool { session_id: "ses_1".into(), part: tool_part("prt_task", "task", running) },
            child(ChildUpdate::Running(true)),
            child(ChildUpdate::Tool { label: "grep parse_".into(), started: true }),
        ],
    );
    fx.with(cx, |view, _, _| {
        assert!(matches!(&view.rows[1].kind, RowKind::Tool(tool) if tool.child.as_deref() == Some("ses_child")));
        assert_eq!(view.state.child("ses_child").map(|c| (c.tools, c.running)), Some((1, true)));
    });
    fx.window(cx, |window, cx| window.click("prt_task-child", cx));
    fx.with(cx, |view, _, _| {
        assert_eq!(view.state.current.as_deref(), Some("ses_child"));
        assert!(view.state.is_running("ses_1"), "the parent keeps running in the background");
    });
}

#[gpui_kit::test]
fn todos_usage_and_status_are_shown_for_the_current_session(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.apply(cx, vec![Event::Catalog { directory: fx.project.clone(), catalog: catalog() }]);
    fx.show(cx, "ses_1", vec![user_row("msg_1", "plan it")]);
    fx.window(cx, |window, _| {
        assert!(window.try_find("todo-dock").is_none());
        assert!(window.try_find("usage").is_none());
    });
    let todo = |content: &str, status: &str| Todo { content: content.into(), status: status.into(), priority: "medium".into() };
    fx.apply(
        cx,
        vec![
            Event::Todos { session_id: "ses_1".into(), todos: vec![todo("one", "completed"), todo("two", "in_progress")] },
            Event::Usage { session_id: "ses_1".into(), usage: Usage { cost: 0.5, input: 4_000, output: 700, context: 25_000, ..Default::default() } },
        ],
    );
    fx.window(cx, |window, cx| {
        assert!(window.try_find("todo-dock").is_some());
        assert!(window.try_find("usage").is_some());
        window.click("todo-toggle", cx);
    });
    fx.with(cx, |view, _, _| {
        assert!(view.todos_collapsed);
        let (short, long) = view.usage_label().unwrap();
        assert_eq!(short.as_ref(), "4.7k · 25% · $0.50");
        assert!(long.contains("Tokens: 4.7k") && long.contains("Cost: $0.50"), "{long}");
    });
    // Nothing left to do: the dock goes away.
    fx.apply(cx, vec![Event::Todos { session_id: "ses_1".into(), todos: vec![todo("one", "completed"), todo("two", "completed")] }]);
    fx.window(cx, |window, _| assert!(window.try_find("todo-dock").is_none()));
}

// Accept is never clicked here: it would download.
#[gpui_kit::test]
fn portable_bash_offer_records_never_and_not_now_records_nothing(cx: &mut TestAppContext) {
    let fx = open(cx);
    let file = fx.with(cx, |view, window, cx| {
        view.open_overlay(Overlay::SetupBash, window, cx);
        opencode_tools::mingit::setup_file(view.backend.state_dir())
    });
    fx.window(cx, |window, cx| {
        assert!(window.try_find("setup-bash").is_some() && window.try_find("setup-bash-accept").is_some());
        window.click("setup-bash-later", cx);
    });
    fx.with(cx, |view, _, _| assert_eq!(view.overlay, None));
    assert!(!file.exists());
    fx.with(cx, |view, window, cx| view.open_overlay(Overlay::SetupBash, window, cx));
    fx.window(cx, |window, cx| window.click("setup-bash-never", cx));
    fx.with(cx, |view, _, _| {
        assert_eq!(view.overlay, None);
        assert_eq!(opencode_tools::mingit::consent(view.backend.state_dir()), Some(opencode_tools::mingit::Consent::Declined));
    });
}

#[gpui_kit::test]
fn session_can_be_renamed_and_deleted_through_dialogs(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.show(cx, "ses_1", vec![user_row("msg_1", "hello")]);
    fx.window(cx, |window, cx| {
        assert!(window.try_find("overlay").is_none());
        window.click("session-rename", cx);
    });
    fx.with(cx, |view, _, cx| {
        assert_eq!(view.overlay, Some(Overlay::Rename("ses_1".into())));
        assert_eq!(view.rename.read(cx).value().as_ref(), "Shown", "the dialog starts from the current title");
    });
    fx.window(cx, |window, cx| {
        assert!(window.try_find("rename-save").is_some());
        window.press("escape", cx);
    });
    fx.with(cx, |view, _, _| assert_eq!(view.overlay, None));

    fx.window(cx, |window, cx| {
        window.click("session-rename", cx);
        window.input(" again", cx);
        window.press("enter", cx);
    });
    fx.with(cx, |view, _, _| assert_eq!(view.overlay, None, "Enter confirms"));
    // What the backend reports once the store took the new title.
    fx.apply(cx, vec![Event::Title { session_id: "ses_1".into(), title: "Shown again".into() }]);
    fx.with(cx, |view, _, _| assert_eq!(view.state.current_title().as_ref(), "Shown again"));

    fx.window(cx, |window, cx| {
        window.click("session-delete", cx);
        assert!(window.try_find("delete-confirm").is_some());
        window.click("overlay-cancel", cx);
    });
    fx.with(cx, |view, _, _| assert_eq!((view.overlay.clone(), view.state.current.as_deref()), (None, Some("ses_1"))));
    fx.window(cx, |window, cx| {
        window.click("session-delete", cx);
        window.click("delete-confirm", cx);
    });
    fx.with(cx, |view, _, _| assert_eq!(view.overlay, None));
    fx.apply(cx, vec![Event::Deleted { session_id: "ses_1".into() }]);
    fx.with(cx, |view, _, _| {
        assert_eq!(view.state.current, None);
        assert!(view.state.sessions.is_empty() && view.rows.is_empty());
        assert_eq!(view.persisted.project(&view.state.directory).last_session, None);
    });
}

fn hex(color: gpui_kit::Hsla) -> String {
    let rgba = gpui_kit::Rgba::from(color);
    let byte = |v: f32| (v * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(rgba.r), byte(rgba.g), byte(rgba.b))
}

fn token(theme: &str, dark: bool, name: &str) -> String {
    crate::model::theme::find(theme).unwrap().tokens(dark).color(name).unwrap().hex()[..7].to_string()
}

#[gpui_kit::test]
fn settings_change_theme_scheme_font_language_and_toggles(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.window(cx, |window, cx| {
        window.press("ctrl-,", cx);
        assert!(window.try_find("settings").is_some());
        // Colours live on the Appearance page, as in opencode 2.x.
        window.click("settings-tab-appearance", cx);
    });
    fx.window(cx, |window, cx| {
        window.click("scheme-dark", cx);
        assert!(cx.theme().is_dark());
        assert_eq!(hex(cx.theme().background), token("clay", true, "v2-background-bg-base"));
        window.click("scheme-light", cx);
        assert!(!cx.theme().is_dark());
        assert_eq!(hex(cx.theme().background), token("clay", false, "v2-background-bg-base"));
    });
    fx.with(cx, |view, _, _| assert_eq!(view.persisted.settings.scheme, Scheme::Light));

    // Hovering a theme previews it, leaving restores the saved one, clicking keeps it.
    fx.window(cx, |window, cx| {
        window.hover("theme-aura", cx);
        assert_eq!(hex(cx.theme().background), token("aura", false, "v2-background-bg-base"), "the preview is live");
    });
    fx.with(cx, |view, _, _| {
        assert_eq!(view.preview.as_deref(), Some("aura"));
        assert_eq!(view.persisted.settings.theme, "clay", "a preview is not saved");
    });
    fx.window(cx, |window, cx| {
        window.hover("font-larger", cx);
        assert_eq!(hex(cx.theme().background), token("clay", false, "v2-background-bg-base"));
        window.click("theme-aura", cx);
        assert_eq!(hex(cx.theme().background), token("aura", false, "v2-background-bg-base"));
        window.click("font-larger", cx);
        window.click("font-larger", cx);
        window.click("font-smaller", cx);
        assert_eq!(cx.theme().font_size, px(15.));
    });
    fx.with(cx, |view, window, cx| {
        assert_eq!((view.persisted.settings.theme.as_str(), view.preview.as_deref()), ("aura", None));
        assert_eq!(view.persisted.settings.font_size, 15.0);

        // Switching language changes what `t` returns and is remembered.
        assert_eq!(view.t("common.cancel").as_ref(), "Cancel");
        view.set_language("de", window, cx);
        assert_eq!(view.t("common.cancel").as_ref(), "Abbrechen");
        assert_eq!(view.persisted.settings.language.as_deref(), Some("de"));
        view.set_language("en", window, cx);
    });

    fx.window(cx, |window, cx| window.click("settings-tab-general", cx));
    fx.window(cx, |window, cx| {
        window.click("general.showReasoningSummaries", cx);
        window.click("settings-tab-shortcuts", cx);
    });
    fx.window(cx, |window, cx| {
        assert!(window.try_find("settings-shortcuts").is_some());
        assert!(window.try_find("theme-aura").is_none());
        window.click("settings-back", cx);
    });
    fx.window(cx, |window, _| assert!(window.try_find("settings").is_none()));
    fx.with(cx, |view, _, _| assert!(!view.persisted.settings.show_reasoning, "the switch turned the default-on setting off"));
}

#[gpui_kit::test]
fn keyboard_shortcuts_reach_their_commands(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.apply(cx, vec![Event::Catalog { directory: fx.project.clone(), catalog: catalog() }]);
    fx.sessions(cx, vec![item("ses_a", "Alpha"), item("ses_b", "Beta")]);
    fx.window(cx, |window, cx| window.press("alt-down", cx));
    fx.with(cx, |view, _, _| assert_eq!(view.state.current.as_deref(), Some("ses_a")));
    fx.window(cx, |window, cx| window.press("alt-down", cx));
    fx.with(cx, |view, _, _| assert_eq!(view.state.current.as_deref(), Some("ses_b")));
    fx.window(cx, |window, cx| window.press("alt-up", cx));
    fx.with(cx, |view, _, _| assert_eq!(view.state.current.as_deref(), Some("ses_a")));
    fx.window(cx, |window, cx| window.press("ctrl-shift-s", cx));
    fx.with(cx, |view, _, _| assert_eq!(view.state.current, None));

    fx.window(cx, |window, cx| {
        assert!(window.try_find("sidebar").is_some());
        window.press("ctrl-b", cx);
    });
    fx.window(cx, |window, cx| {
        assert!(window.try_find("sidebar").is_none());
        window.press("ctrl-b", cx);
    });
    fx.window(cx, |window, cx| {
        assert!(window.try_find("sidebar").is_some());
        window.press("f1", cx);
    });
    fx.with(cx, |view, _, _| assert_eq!(view.overlay, Some(Overlay::Settings(SettingsTab::Shortcuts))));
    fx.window(cx, |window, cx| window.press("escape", cx));
    fx.with(cx, |view, _, _| assert_eq!(view.overlay, None));

    fx.window(cx, |window, cx| window.press("ctrl-shift-t", cx));
    fx.with(cx, |view, _, _| assert_ne!(view.persisted.settings.theme, "oc-2", "the theme cycled"));

    // Escape stops a running prompt when nothing else wants it.
    fx.window(cx, |window, cx| {
        window.press("ctrl-l", cx);
        window.input("go", cx);
        window.press("enter", cx);
    });
    fx.with(cx, |view, _, _| assert!(view.state.current_running()));
    fx.window(cx, |window, cx| window.press("escape", cx));
    fx.with(cx, |view, _, _| assert!(view.state.current_running(), "the run ends when the engine reports it"));
}

#[gpui_kit::test]
fn scrolling_up_releases_tail_follow_until_the_user_returns(cx: &mut TestAppContext) {
    let fx = open(cx);
    let rows: Vec<Row> = (0..80).map(|n| text_row(&format!("prt_{n}"), &format!("Paragraph {n}\n\nwith a second line of text."))).collect();
    fx.show(cx, "ses_1", rows.clone());
    fx.window(cx, |_, _| {});
    fx.with(cx, |view, _, cx| {
        let scroller = view.scroller.read(cx);
        assert_eq!(scroller.item_count(), 80);
        assert!(scroller.is_following_tail(), "a freshly opened session shows its end");
    });

    fx.window(cx, |window, cx| window.scroll("transcript-area", ScrollDelta::Pixels(point(px(0.), px(600.))), cx));
    fx.window(cx, |_, _| {});
    let top = fx.with(cx, |view, _, cx| {
        let scroller = view.scroller.read(cx);
        assert!(scroller.is_scrolled_up() && !scroller.is_following_tail(), "the wheel moved the view away from the end");
        scroller.logical_scroll_top()
    });

    // New rows arrive while the user reads further up: the view stays where it is.
    let mut more = rows.clone();
    more.push(text_row("prt_new", "A new answer"));
    fx.apply(cx, vec![Event::Snapshot { session_id: "ses_1".into(), rows: more }]);
    fx.window(cx, |_, _| {});
    fx.with(cx, |view, _, cx| {
        let scroller = view.scroller.read(cx);
        assert_eq!(scroller.item_count(), 81);
        assert!(!scroller.is_following_tail());
        let now = scroller.logical_scroll_top();
        assert_eq!((now.item_ix, now.offset_in_item), (top.item_ix, top.offset_in_item));
    });

    // Sending a prompt jumps back to the end.
    fx.window(cx, |window, cx| {
        window.press("ctrl-l", cx);
        window.input("next", cx);
        window.press("enter", cx);
    });
    fx.window(cx, |_, _| {});
    fx.with(cx, |view, _, cx| assert!(view.scroller.read(cx).is_following_tail()));
}

#[gpui_kit::test]
fn engine_signals_play_sounds_and_notify_only_while_unfocused(cx: &mut TestAppContext) {
    let fx = open(cx);
    let played = || fx.alerts.played.borrow().clone();

    fx.set_active(cx, true);
    fx.apply(cx, vec![fx.signal(Kind::Idle, Some("Fix the parser"))]);
    assert_eq!(played(), ["staplebops-01"]);
    assert!(fx.notified().is_empty(), "a focused window gets no system notification");

    fx.set_active(cx, false);
    fx.apply(cx, vec![fx.signal(Kind::Idle, Some("Fix the parser")), fx.signal(Kind::Idle, None)]);
    assert_eq!(played().len(), 2, "a session the store does not know stays silent");
    assert_eq!(fx.notified(), [("Response ready".to_string(), "Fix the parser".to_string())]);
    let target = fx.alerts.notified.borrow()[0].target.clone();
    assert_eq!((target.directory, target.session_id.as_deref()), (fx.project.clone(), Some("ses_1")));

    // A request is announced only while the view still waits on it.
    fx.apply(cx, vec![fx.signal(Kind::Permission(7), Some("Fix the parser"))]);
    assert_eq!(played().len(), 2);
    fx.apply(cx, vec![Event::Permission(permission(7)), fx.signal(Kind::Permission(7), Some("Fix the parser"))]);
    assert_eq!(played().last(), Some(&"staplebops-02"));
    assert_eq!(fx.notified().last().unwrap().1, format!("Fix the parser in {} needs permission", state::project_name(&fx.project)));

    // Auto-accept answers the request before anyone is told about it.
    fx.with(cx, |view, _, cx| view.set_toggle(Toggle::AutoAccept, true, cx));
    let before = (played().len(), fx.notified().len());
    fx.apply(cx, vec![Event::Permission(permission(8)), fx.signal(Kind::Permission(8), Some("Fix the parser"))]);
    assert_eq!((played().len(), fx.notified().len()), before);

    // Errors sound by default and notify only when switched on; switched-off sounds stay quiet.
    fx.apply(cx, vec![fx.signal(Kind::Error, Some("Fix the parser"))]);
    assert_eq!((played().last(), fx.notified().len()), (Some(&"nope-03"), before.1));
    fx.with(cx, |view, _, cx| {
        view.set_toggle(Toggle::NotifyErrors, true, cx);
        view.set_toggle(Toggle::SoundErrors, false, cx);
    });
    fx.apply(cx, vec![fx.signal(Kind::Error, None)]);
    assert_eq!(played().len(), before.0 + 1);
    assert_eq!(fx.notified().last().unwrap(), &("Session error".to_string(), "An error occurred".to_string()));
}

#[gpui_kit::test]
fn a_notification_click_opens_its_session_in_its_project(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.sessions(cx, vec![item("ses_a", "Alpha"), item("ses_b", "Beta")]);
    let target = Target { directory: fx.project.clone(), session_id: Some("ses_b".into()) };
    fx.with(cx, |view, window, cx| view.reveal(&Target::from_tag(&target.tag("turn-complete")).unwrap(), window, cx));
    fx.with(cx, |view, _, _| assert_eq!(view.state.current.as_deref(), Some("ses_b")));

    let other = fx._root.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let elsewhere = Target { directory: other.clone(), session_id: Some("ses_x".into()) };
    fx.with(cx, |view, window, cx| view.reveal(&elsewhere, window, cx));
    fx.with(cx, |view, _, _| {
        assert_eq!(view.state.directory, other);
        assert_eq!(view.persisted.project(&other).last_session.as_deref(), Some("ses_x"), "reopened once its list arrives");
        assert!(view.restore);
    });
}

fn link(kind: &str, directory: &Path, prompt: Option<&str>) -> String {
    let encode = |text: &str| url::form_urlencoded::byte_serialize(text.as_bytes()).collect::<String>();
    let prompt = prompt.map(|p| format!("&prompt={}", encode(p))).unwrap_or_default();
    format!("opencode://{kind}?directory={}{prompt}", encode(&directory.to_string_lossy()))
}

#[gpui_kit::test]
fn later_starts_and_links_open_projects_and_prefilled_sessions(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.show(cx, "ses_a", vec![user_row("msg_1", "hi")]);
    let other = fx._root.path().join("other");
    std::fs::create_dir_all(&other).unwrap();

    // A second start with a directory switches the window to that project.
    fx.requests.send(Request { directory: Some(other.clone()), links: vec![] }).unwrap();
    cx.run_until_parked();
    fx.with(cx, |view, _, _| assert_eq!(view.state.directory, other));

    // new-session: the project, a fresh session, the prompt waiting in the input; nothing is sent.
    fx.requests.send(Request { directory: None, links: vec![link("new-session", &fx.project, Some("ship it"))] }).unwrap();
    cx.run_until_parked();
    assert_eq!(fx.composer(cx), "ship it");
    fx.with(cx, |view, _, _| {
        assert_eq!((view.state.directory.clone(), view.state.current.clone()), (fx.project.clone(), None));
        assert!(!view.restore, "the last session does not replace the new one");
        assert!(view.state.sessions.is_empty() && !view.state.current_running());
    });

    fx.requests.send(Request { directory: None, links: vec![link("open-project", &other, None), "opencode://other?directory=/x".into()] }).unwrap();
    cx.run_until_parked();
    fx.with(cx, |view, _, _| assert_eq!((view.state.directory.clone(), view.state.notice.clone()), (other.clone(), None)));

    let missing = fx._root.path().join("missing");
    fx.requests.send(Request { directory: None, links: vec![link("open-project", &missing, None)] }).unwrap();
    cx.run_until_parked();
    fx.with(cx, |view, _, _| {
        assert_eq!(view.state.directory, other);
        assert_eq!(view.state.notice.as_ref().map(|n| n.key), Some("desktop.rs.error.openProject"));
    });
}

#[gpui_kit::test]
fn a_cold_start_link_prefills_a_new_session(cx: &mut TestAppContext) {
    let fx = open_linked(cx, Persisted::default(), |project| vec![link("new-session", project, Some("from the browser"))]);
    assert_eq!(fx.composer(cx), "from the browser");
    fx.with(cx, |view, _, _| assert!(!view.restore && view.state.current.is_none()));

    // Without a prompt the project's last session comes back as usual.
    let fx = open_linked(cx, Persisted::default(), |project| vec![link("open-project", project, None)]);
    fx.with(cx, |view, _, _| assert!(view.restore));
    assert_eq!(fx.composer(cx), "");
}

#[gpui_kit::test]
fn sound_pickers_choose_preview_and_switch_off(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.window(cx, |window, cx| window.press("ctrl-,", cx));
    fx.window(cx, |window, cx| {
        assert!(window.try_find("settings").is_some());
        window.click("settings-tab-notifications", cx);
    });
    fx.window(cx, |window, _| assert!(window.try_find("settings-notifications").is_some()));
    fx.with(cx, |view, window, cx| {
        let selected = |view: &AppView, cx: &App, toggle| view.sounds.iter().find(|(t, _)| *t == toggle).unwrap().1.read(cx).selected_values();
        assert_eq!(selected(view, cx, Toggle::SoundErrors), [SharedString::from("nope-03")]);

        view.choose_sound(Toggle::SoundAgent, Some("yup-01".into()), window, cx);
        assert_eq!((view.persisted.settings.sound_agent_id.as_str(), view.persisted.settings.sound_agent), ("yup-01", true));
        view.choose_sound(Toggle::SoundAgent, Some("none".into()), window, cx);
        assert!(!view.persisted.settings.sound_agent);
        assert_eq!(view.persisted.settings.sound_agent_id, "yup-01");
        assert_eq!(*fx.alerts.previews.borrow(), [Some("yup-01"), None], "a choice is heard once; None stops it");

        // A cleared selection puts the stored choice back on display.
        view.choose_sound(Toggle::SoundErrors, None, window, cx);
        assert_eq!(selected(view, cx, Toggle::SoundErrors), [SharedString::from("nope-03")]);
        assert!(view.persisted.settings.sound_errors);

        // Labels follow the language.
        view.set_language("de", window, cx);
        view.set_language("en", window, cx);
        assert_eq!(selected(view, cx, Toggle::SoundAgent), [SharedString::from("none")]);
    });
    fx.window(cx, |_, _| {});
}

#[gpui_kit::test]
fn the_app_menu_opens_and_its_window_actions_work(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.window(cx, |window, cx| {
        assert!(window.try_find("app-menu").is_some());
        window.click("app-menu", cx);
    });
    fx.window(cx, |_, _| {});
    fx.window(cx, |window, cx| {
        window.dispatch_action(Box::new(super::super::menu::ZoomIn), cx);
        window.dispatch_action(Box::new(super::super::menu::ZoomIn), cx);
    });
    fx.with(cx, |view, _, _| assert_eq!(view.persisted.settings.font_size, 16.0));
    fx.window(cx, |window, cx| window.dispatch_action(Box::new(super::super::menu::ActualSize), cx));
    fx.with(cx, |view, _, _| assert_eq!(view.persisted.settings.font_size, 14.0));
    fx.window(cx, |window, cx| window.dispatch_action(Box::new(NewSession), cx));
    fx.with(cx, |view, _, _| assert_eq!(view.state.current, None));
}

#[gpui_kit::test]
fn window_geometry_is_remembered_on_resize(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.with(cx, |view, _, _| assert_eq!(view.persisted.window, None));
    cx.simulate_window_resize(fx.window.into(), size(px(1000.), px(700.)));
    cx.run_until_parked();
    fx.window(cx, |_, _| {});
    fx.with(cx, |view, window, _| {
        let saved = view.persisted.window.expect("the new size was recorded");
        assert_eq!((saved.width, saved.height), (1000.0, 700.0));
        assert_eq!(geometry(window.window_bounds()), saved);
    });
}

fn with_providers(mut catalog: Catalog) -> Catalog {
    let entry = |id: &str, name: &str| state::ProviderEntry { id: id.into(), name: name.into() };
    catalog.providers = vec![entry("zeta", "Zeta"), entry("openai", "OpenAI"), entry("anthropic", "Anthropic")];
    catalog.connected =
        vec![state::ConnectedProvider { id: "acme".into(), name: "Acme".into(), source: state::ProviderSource::Api, removable: true }];
    catalog
}

#[gpui_kit::test]
fn favourites_lead_the_picker_and_hidden_models_stay_out(cx: &mut TestAppContext) {
    let mut persisted = Persisted::default();
    persisted.models.set_visible("acme/small", false);
    let fx = open_with(cx, persisted);
    fx.apply(cx, vec![Event::Catalog { directory: fx.project.clone(), catalog: catalog() }]);
    fx.window(cx, |window, cx| window.click("model-picker", cx));
    fx.window(cx, |window, cx| {
        assert!(window.try_find("model-option-acme/large").is_some());
        assert!(window.try_find("model-option-acme/small").is_none(), "a hidden model is not offered");
        window.click("model-favorite-acme/large", cx);
        assert!(window.try_find("model-menu").is_some(), "starring keeps the picker open");
    });
    let titles = fx.with(cx, |view, _, _| view.picker_sections("").iter().map(|s| s.title.to_string()).collect::<Vec<_>>());
    assert_eq!(titles, ["Favorites"], "the favourite leaves its provider group");
    let file = fx.with(cx, |view, _, _| crate::model::models::file(view.backend.state_dir()));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), r#"{"recent":[],"favorite":[{"providerID":"acme","modelID":"large"}],"variant":{}}"#);
    // A search lists matches by provider again; the TUI's file is read back on every change.
    fx.with(cx, |view, _, _| {
        let sections = view.picker_sections("lar");
        assert_eq!((sections.len(), sections[0].title.as_ref()), (1, "Acme"));
        assert!(view.picker_sections("small").is_empty());
    });
    fx.window(cx, |window, cx| window.click("model-favorite-acme/large", cx));
    assert!(std::fs::read_to_string(&file).unwrap().contains(r#""favorite":[]"#));
}

#[gpui_kit::test]
fn manage_models_switches_models_and_whole_providers(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.apply(cx, vec![Event::Catalog { directory: fx.project.clone(), catalog: catalog() }]);
    fx.window(cx, |window, cx| window.click("model-picker", cx));
    fx.window(cx, |window, cx| window.click("model-manage", cx));
    fx.with(cx, |view, _, _| assert_eq!((view.overlay.clone(), view.model_menu), (Some(Overlay::ManageModels), false)));
    fx.window(cx, |window, cx| window.click("manage-model-acme/small", cx));
    fx.with(cx, |view, _, _| assert_eq!(view.persisted.models.visibility("acme/small"), Some(false)));
    fx.window(cx, |window, cx| window.click("manage-provider-acme", cx));
    fx.with(cx, |view, _, _| {
        let all = |visible| ["acme/large", "acme/small"].iter().all(|spec| view.persisted.models.visibility(spec) == Some(visible));
        assert!(all(true), "the provider switch was off with one model hidden, so it shows all");
    });
    fx.window(cx, |window, cx| window.click("manage-provider-acme", cx));
    fx.with(cx, |view, _, _| assert!(view.picker_sections("").is_empty()));
    // Folding a provider hides its rows.
    fx.window(cx, |window, cx| window.click("manage-group-acme", cx));
    fx.window(cx, |window, _| assert!(window.try_find("manage-model-acme/large").is_none()));
}

#[gpui_kit::test]
fn the_variant_choice_follows_the_model_and_is_shared(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.apply(cx, vec![Event::Catalog { directory: fx.project.clone(), catalog: catalog() }]);
    fx.window(cx, |window, _| assert!(window.try_find("variant-picker").is_some(), "the default model has variants"));
    fx.with(cx, |view, window, cx| {
        assert_eq!(view.current_variant(), None);
        view.choose_variant(Some("max".into()), window, cx);
        assert_eq!(view.current_variant().as_deref(), Some("max"));
        let file = crate::model::models::file(view.backend.state_dir());
        assert!(std::fs::read_to_string(file).unwrap().contains(r#""variant":{"acme/large":"max"}"#));
        view.choose_model("acme/small", window, cx);
        assert_eq!(view.current_variant(), None);
    });
    fx.window(cx, |window, _| assert!(window.try_find("variant-picker").is_none(), "a model without variants has no choice"));
    fx.with(cx, |view, window, cx| {
        view.choose_model("acme/large", window, cx);
        assert_eq!(view.current_variant().as_deref(), Some("max"), "each model keeps its own variant");
        view.choose_variant(Some("default".into()), window, cx);
        assert_eq!(view.current_variant(), None);
    });
}

#[gpui_kit::test]
fn settings_is_a_page_with_a_searchable_navigation(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.apply(cx, vec![Event::Catalog { directory: fx.project.clone(), catalog: with_providers(catalog()) }]);
    fx.window(cx, |window, cx| window.press("ctrl-,", cx));
    fx.window(cx, |window, cx| {
        for page in ["providers", "models", "general", "appearance", "notifications", "shortcuts", "about"] {
            assert!(window.try_find(SharedString::from(format!("settings-tab-{page}"))).is_some(), "{page}");
        }
        assert!(window.try_find("overlay").is_none(), "settings is a page, not a dialog");
        window.click("settings-tab-about", cx);
    });
    fx.window(cx, |window, _| assert!(window.try_find("settings-about").is_some()));
    fx.with(cx, |view, window, cx| view.settings_search.update(cx, |input, cx| input.set_value("shortcut", window, cx)));
    fx.window(cx, |window, _| {
        assert!(window.try_find("settings-tab-shortcuts").is_some());
        assert!(window.try_find("settings-tab-providers").is_none());
    });
    fx.with(cx, |view, window, cx| view.settings_search.update(cx, |input, cx| input.set_value("", window, cx)));

    // The Models page lists the same switches as the dialog.
    fx.window(cx, |window, cx| window.click("settings-tab-models", cx));
    fx.window(cx, |window, cx| window.click("models-model-acme/large", cx));
    fx.with(cx, |view, _, _| assert_eq!(view.persisted.models.visibility("acme/large"), Some(false)));

    // Providers: connected ones can be disconnected, popular ones connected; the dialog returns to the page.
    fx.window(cx, |window, cx| window.click("settings-tab-providers", cx));
    fx.window(cx, |window, cx| {
        assert!(window.try_find("provider-disconnect-acme").is_some());
        assert!(window.try_find("provider-connect-zeta").is_none(), "only popular providers are offered here");
        window.click("provider-connect-anthropic", cx);
    });
    fx.with(cx, |view, _, _| {
        let Some(Overlay::Connect(connect)) = &view.overlay else { panic!("{:?}", view.overlay) };
        assert_eq!((connect.provider.as_deref(), &connect.step, connect.back), (Some("anthropic"), &ConnectStep::ApiKey { missing: false }, Some(SettingsTab::Providers)));
    });
    fx.window(cx, |window, cx| window.press("escape", cx));
    fx.with(cx, |view, _, _| assert_eq!(view.overlay, Some(Overlay::Settings(SettingsTab::Providers))));
    fx.window(cx, |window, cx| window.press("escape", cx));
    fx.with(cx, |view, _, _| assert_eq!(view.overlay, None));
}

#[gpui_kit::test]
fn connecting_a_provider_stores_its_api_key(cx: &mut TestAppContext) {
    let fx = open(cx);
    fx.apply(cx, vec![Event::Catalog { directory: fx.project.clone(), catalog: with_providers(catalog()) }]);
    fx.window(cx, |window, cx| window.click("model-picker", cx));
    fx.window(cx, |window, cx| window.click("model-connect", cx));
    fx.window(cx, |window, cx| {
        assert!(window.try_find("connect-dialog").is_some());
        // Popular providers first, in the original's order, then the rest by name.
        let ids: Vec<f32> = ["anthropic", "openai", "zeta"]
            .iter()
            .map(|id| f32::from(window.find(SharedString::from(format!("connect-provider-{id}"))).bounds().origin.y))
            .collect();
        assert!(ids[0] < ids[1] && ids[1] < ids[2], "{ids:?}");
        window.click("connect-provider-openai", cx);
    });
    // OpenAI offers the CLI's ChatGPT flows before its API key.
    fx.window(cx, |window, cx| {
        assert!(window.try_find("connect-method-2").is_some());
        window.click("connect-back", cx);
    });
    fx.window(cx, |window, cx| window.click("connect-provider-anthropic", cx));
    fx.window(cx, |window, cx| window.click("connect-submit", cx));
    fx.with(cx, |view, _, _| assert!(matches!(&view.overlay, Some(Overlay::Connect(c)) if c.step == ConnectStep::ApiKey { missing: true })));
    fx.window(cx, |window, cx| {
        window.input("sk-ant-test", cx);
        window.click("connect-submit", cx);
    });
    fx.with(cx, |view, _, _| assert!(matches!(&view.overlay, Some(Overlay::Connect(c)) if c.step == ConnectStep::Pending)));
    let file = fx.project.parent().unwrap().join("data").join("auth.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !std::fs::read_to_string(&file).is_ok_and(|text| text.contains("sk-ant-test")) {
        assert!(std::time::Instant::now() < deadline, "auth.json was not written");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let stored: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(stored["anthropic"], json!({"type": "api", "key": "sk-ant-test"}));
    // The backend's answer closes the dialog and says so.
    fx.apply(cx, vec![Event::Login { provider_id: "anthropic".into(), step: state::LoginStep::Done }]);
    fx.with(cx, |view, _, _| {
        assert_eq!(view.overlay, None);
        assert_eq!(view.state.notice.as_ref().map(|n| (n.key, n.error.as_str())), Some(("provider.connect.toast.connected.title", "Anthropic")));
    });
}
