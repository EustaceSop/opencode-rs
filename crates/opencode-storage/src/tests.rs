use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::store::{Store, StoreError};
use crate::types::*;
use crate::{id, identity};

fn round_trip<T: Serialize + DeserializeOwned>(value: Value) -> T {
    let typed: T =
        serde_json::from_value(value.clone()).unwrap_or_else(|error| panic!("{error}: {value}"));
    assert_eq!(serde_json::to_value(&typed).unwrap(), value);
    typed
}

fn part_base(body: Value) -> Value {
    let mut value = json!({ "id": "prt_1", "sessionID": "ses_1", "messageID": "msg_1" });
    value
        .as_object_mut()
        .unwrap()
        .extend(body.as_object().unwrap().clone());
    value
}

#[test]
fn session_round_trips_full_and_minimal() {
    let session: Session = round_trip(json!({
        "id": "ses_4301a97ffffeAbCdEfGhIjKlMn",
        "slug": "brave-otter",
        "projectID": "476fbfe6befa9e63ddd7f9ae71655999a0104256",
        "workspaceID": "wrk_1",
        "directory": "/repo",
        "path": "packages/app",
        "parentID": "ses_parent",
        "summary": {
            "additions": 3,
            "deletions": 1,
            "files": 2,
            "diffs": [
                { "file": "a.txt", "patch": "@@", "additions": 3, "deletions": 1, "status": "modified" },
                { "additions": 0, "deletions": 0 }
            ]
        },
        "cost": 0.005,
        "tokens": { "input": 500, "output": 800, "reasoning": 200, "cache": { "read": 100, "write": 50 } },
        "share": { "url": "https://share.example/s/1" },
        "title": "Fix the bug",
        "agent": "build",
        "model": { "id": "model-a", "providerID": "provider-a", "variant": "high" },
        "version": "1.19.5",
        "metadata": { "zeta": 1, "alpha": { "nested": [1, 2.5, null] } },
        "time": { "created": 1700000000000u64, "updated": 1700000000500u64, "compacting": 1700000000600u64, "archived": -1 },
        "permission": [
            { "permission": "bash", "pattern": "git *", "action": "allow" },
            { "permission": "edit", "pattern": "*", "action": "ask" },
            { "permission": "webfetch", "pattern": "*", "action": "deny" }
        ],
        "revert": { "messageID": "msg_1", "partID": "prt_1", "snapshot": "abc", "diff": "diff --git" }
    }));
    assert!(session.is_child());
    assert_eq!(session.time.archived, Some(-1.0));
    assert_eq!(
        session
            .metadata
            .as_ref()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["zeta", "alpha"]
    );

    let minimal: Session = round_trip(json!({
        "id": "ses_1",
        "slug": "calm-moon",
        "projectID": "global",
        "directory": "/",
        "title": "New session - 2023-11-14T22:13:20.123Z",
        "version": "local",
        "time": { "created": 1, "updated": 2 }
    }));
    assert!(minimal.has_default_title());
    assert_eq!(minimal.cost, None);

    let nulls: Session = serde_json::from_value(json!({
        "id": "ses_1", "slug": "s", "projectID": "global", "directory": "", "title": "t", "version": "v",
        "time": { "created": 1, "updated": 2 }, "revert": null, "share": null
    }))
    .unwrap();
    assert_eq!(nulls.revert, None);
}

#[test]
fn numbers_serialise_like_json_stringify() {
    let tokens = Tokens {
        total: Some(1500.0),
        input: 500.0,
        output: -42.0,
        reasoning: 0.5,
        cache: TokenCache {
            read: 0.0,
            write: 1e21,
        },
    };
    assert_eq!(
        serde_json::to_string(&tokens).unwrap(),
        r#"{"total":1500,"input":500,"output":-42,"reasoning":0.5,"cache":{"read":0,"write":1e+21}}"#
    );
    let part = Part {
        id: "prt_1".into(),
        session_id: "ses_1".into(),
        message_id: "msg_1".into(),
        body: PartBody::StepFinish(StepFinishPart {
            reason: "stop".into(),
            snapshot: None,
            cost: 0.0,
            tokens: Tokens::default(),
        }),
    };
    assert_eq!(
        serde_json::to_string(&part).unwrap(),
        concat!(
            r#"{"id":"prt_1","sessionID":"ses_1","messageID":"msg_1","type":"step-finish","reason":"stop","#,
            r#""cost":0,"tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}}"#
        )
    );
}

#[test]
fn user_message_round_trips() {
    let message: Message = round_trip(json!({
        "id": "msg_1",
        "sessionID": "ses_1",
        "role": "user",
        "time": { "created": 1700000000000u64 },
        "format": { "type": "json_schema", "schema": { "type": "object", "properties": {} }, "retryCount": 3 },
        "summary": { "title": "t", "body": "b", "diffs": [{ "file": "a", "additions": 1, "deletions": 0, "status": "added" }] },
        "agent": "build",
        "model": { "providerID": "test", "modelID": "test", "variant": "max" },
        "system": "be brief",
        "tools": { "bash": false, "read": true }
    }));
    assert_eq!(message.role(), "user");
    assert_eq!(message.id(), "msg_1");
    assert_eq!(message.session_id(), "ses_1");
    assert_eq!(message.time_created(), 1_700_000_000_000);

    // Literal from opencode/test/server/negative-tokens-regression.test.ts (ids substituted).
    round_trip::<Message>(json!({
        "id": "msg_2",
        "role": "user",
        "sessionID": "ses_1",
        "agent": "build",
        "model": { "providerID": "test", "modelID": "test" },
        "time": { "created": 1700000000000u64 }
    }));
    round_trip::<OutputFormat>(json!({ "type": "text" }));
    let defaulted: OutputFormat =
        serde_json::from_value(json!({ "type": "json_schema", "schema": {} })).unwrap();
    assert_eq!(
        serde_json::to_value(&defaulted).unwrap(),
        json!({ "type": "json_schema", "schema": {}, "retryCount": 2 })
    );
}

#[test]
fn assistant_message_round_trips_with_every_error() {
    let errors = [
        json!({ "name": "ProviderAuthError", "data": { "providerID": "p", "message": "m" } }),
        json!({ "name": "UnknownError", "data": { "message": "m", "ref": "r" } }),
        json!({ "name": "UnknownError", "data": { "message": "m" } }),
        json!({ "name": "MessageOutputLengthError", "data": {} }),
        json!({ "name": "MessageAbortedError", "data": { "message": "Aborted" } }),
        json!({ "name": "StructuredOutputError", "data": { "message": "m", "retries": 2 } }),
        json!({ "name": "ContextOverflowError", "data": { "message": "m", "responseBody": "{}" } }),
        json!({ "name": "ContentFilterError", "data": { "message": "m" } }),
        json!({ "name": "APIError", "data": {
            "message": "Connection reset by server", "statusCode": 529, "isRetryable": true,
            "responseHeaders": { "retry-after": "1" }, "responseBody": "overloaded",
            "metadata": { "code": "ECONNRESET" }
        } }),
        json!({ "name": "APIError", "data": { "message": "m", "isRetryable": false } }),
    ];
    for error in errors {
        round_trip::<AssistantError>(error.clone());
        let message: Message = round_trip(json!({
            "id": "msg_2",
            "sessionID": "ses_1",
            "role": "assistant",
            "time": { "created": 1700000000000u64, "completed": 1700000000900u64 },
            "error": error,
            "parentID": "msg_1",
            "modelID": "model-a",
            "providerID": "provider-a",
            "mode": "build",
            "agent": "build",
            "path": { "cwd": "/repo/pkg", "root": "/repo" },
            "summary": true,
            "cost": 0.0123,
            "tokens": { "total": 1500, "input": 500, "output": 800, "reasoning": 200, "cache": { "read": 100, "write": 50 } },
            "structured": { "answer": [1, 2] },
            "variant": "high",
            "finish": "stop"
        }));
        assert!(message.as_assistant().unwrap().error.is_some());
    }
    let minimal: Message = round_trip(json!({
        "id": "msg_2",
        "sessionID": "ses_1",
        "role": "assistant",
        "time": { "created": 1 },
        "parentID": "msg_1",
        "modelID": "m",
        "providerID": "p",
        "mode": "build",
        "agent": "build",
        "path": { "cwd": "/", "root": "/" },
        "cost": 0,
        "tokens": { "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } }
    }));
    assert_eq!(minimal.role(), "assistant");
    assert_eq!(
        serde_json::to_value(AssistantError::aborted()).unwrap(),
        json!({ "name": "MessageAbortedError", "data": { "message": "Aborted" } })
    );
}

#[test]
fn every_part_variant_round_trips() {
    let source_text = json!({ "value": "@a.rs", "start": 0, "end": 5 });
    let bodies = [
        json!({ "type": "text", "text": "hello" }),
        json!({ "type": "text", "text": "hi", "synthetic": true, "ignored": false,
                "time": { "start": 1, "end": 2 }, "metadata": { "openai": { "itemId": "x" } } }),
        json!({ "type": "reasoning", "text": "thinking", "time": { "start": 1 } }),
        json!({ "type": "reasoning", "text": "thinking", "metadata": { "anthropic": { "signature": "s" } },
                "time": { "start": 1, "end": 2 } }),
        json!({ "type": "file", "mime": "image/png", "url": "data:image/png;base64,AAAA" }),
        json!({ "type": "file", "mime": "text/plain", "filename": "a.rs", "url": "file:///repo/a.rs",
                "source": { "type": "file", "text": source_text, "path": "/repo/a.rs" } }),
        json!({ "type": "file", "mime": "text/plain", "url": "file:///repo/a.rs",
                "source": { "type": "symbol", "text": source_text, "path": "/repo/a.rs",
                            "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 3, "character": 1 } },
                            "name": "main", "kind": 12 } }),
        json!({ "type": "file", "mime": "text/plain", "url": "mcp://r",
                "source": { "type": "resource", "text": source_text, "clientName": "docs", "uri": "mcp://r" } }),
        json!({ "type": "step-start" }),
        json!({ "type": "step-start", "snapshot": "abc123" }),
        // Literal from opencode/test/session/session.test.ts "step-finish token propagation".
        json!({ "type": "step-finish", "reason": "stop", "cost": 0.005,
                "tokens": { "total": 1500, "input": 500, "output": 800, "reasoning": 200, "cache": { "read": 100, "write": 50 } } }),
        // Literal from opencode/test/server/negative-tokens-regression.test.ts.
        json!({ "type": "step-finish", "reason": "stop", "cost": 0,
                "tokens": { "input": 0, "output": -42, "reasoning": 0, "cache": { "read": 0, "write": 0 } } }),
        json!({ "type": "step-finish", "reason": "tool-calls", "snapshot": "abc", "cost": 1,
                "tokens": { "input": 1, "output": 2, "reasoning": 3, "cache": { "read": 4, "write": 5 } } }),
        json!({ "type": "snapshot", "snapshot": "abc123" }),
        json!({ "type": "patch", "hash": "abc123", "files": ["/repo/a.rs", "/repo/b.rs"] }),
        json!({ "type": "agent", "name": "explore" }),
        json!({ "type": "agent", "name": "explore", "source": { "value": "@explore", "start": 0, "end": 8 } }),
        json!({ "type": "subtask", "prompt": "p", "description": "d", "agent": "general" }),
        json!({ "type": "subtask", "prompt": "p", "description": "d", "agent": "general",
                "model": { "providerID": "p", "modelID": "m" }, "command": "review" }),
        json!({ "type": "retry", "attempt": 2,
                "error": { "name": "APIError", "data": { "message": "Overloaded", "statusCode": 529, "isRetryable": true } },
                "time": { "created": 1700000000000u64 } }),
        json!({ "type": "compaction", "auto": true }),
        json!({ "type": "compaction", "auto": false, "overflow": true, "tail_start_id": "msg_9" }),
    ];
    let mut kinds = std::collections::BTreeSet::new();
    for body in bodies {
        let part: Part = round_trip(part_base(body.clone()));
        assert_eq!(part.kind(), body["type"]);
        assert_eq!(
            (
                part.id.as_str(),
                part.session_id.as_str(),
                part.message_id.as_str()
            ),
            ("prt_1", "ses_1", "msg_1")
        );
        kinds.insert(part.kind());
    }
    assert_eq!(kinds.len(), 11);
    assert!(serde_json::from_value::<Part>(part_base(json!({ "type": "nope" }))).is_err());
}

#[test]
fn tool_part_round_trips_through_every_state() {
    let states = [
        json!({ "status": "pending", "input": {}, "raw": "{\"comm" }),
        json!({ "status": "running", "input": { "command": "ls" }, "time": { "start": 10 } }),
        json!({ "status": "running", "input": { "command": "ls" }, "title": "ls", "metadata": { "output": "a" },
                "time": { "start": 10 } }),
        json!({ "status": "completed", "input": { "command": "ls" }, "output": "a\nb", "title": "ls",
                "metadata": { "exit": 0, "truncated": false }, "time": { "start": 10, "end": 20 } }),
        json!({ "status": "completed", "input": { "filePath": "/a.png" }, "output": "Image read", "title": "a.png",
                "metadata": {}, "time": { "start": 10, "end": 20, "compacted": 30 },
                "attachments": [{ "id": "prt_2", "sessionID": "ses_1", "messageID": "msg_1", "type": "file",
                                  "mime": "image/png", "filename": "a.png", "url": "data:image/png;base64,AAAA" }] }),
        json!({ "status": "error", "input": { "command": "ls" }, "error": "boom", "time": { "start": 10, "end": 20 } }),
        json!({ "status": "error", "input": {}, "error": "Tool execution aborted",
                "metadata": { "interrupted": true, "output": "partial" }, "time": { "start": 10, "end": 20 } }),
    ];
    for state in states {
        let status = state["status"].clone();
        let part: Part = round_trip(part_base(json!({
            "type": "tool", "callID": "call_1", "tool": "bash", "state": state,
            "metadata": { "providerExecuted": true }
        })));
        let PartBody::Tool(tool) = &part.body else {
            panic!("expected tool part")
        };
        assert_eq!(tool.state.status(), status);
    }
    round_trip::<Part>(part_base(json!({
        "type": "tool", "callID": "call_1", "tool": "bash",
        "state": { "status": "pending", "input": {}, "raw": "" }
    })));
}

#[test]
fn tool_state_machine_carries_input_and_start() {
    let input = json!({ "command": "ls" }).as_object().unwrap().clone();
    let pending = ToolState::pending(JsonMap::new(), "");
    assert!(!pending.is_terminal());
    let running = pending.to_running(input.clone(), 10);
    assert_eq!((running.status(), running.start()), ("running", Some(10)));
    let completed = running.to_completed("out", "ls", JsonMap::new(), None, 20);
    let ToolState::Completed(done) = &completed else {
        panic!("expected completed")
    };
    assert_eq!(
        (done.time.start, done.time.end, &done.input),
        (10, 20, &input)
    );
    let failed = running.to_error("boom", None, 25);
    assert_eq!(
        serde_json::to_value(&failed).unwrap(),
        json!({ "status": "error", "input": { "command": "ls" }, "error": "boom", "time": { "start": 10, "end": 25 } })
    );
    assert!(completed.is_terminal() && failed.is_terminal());
}

#[test]
fn project_and_with_parts_round_trip() {
    round_trip::<Project>(json!({
        "id": "476fbfe6befa9e63ddd7f9ae71655999a0104256",
        "worktree": "/repo",
        "vcs": "git",
        "name": "repo",
        "icon": { "url": "data:image/png;base64,AA", "override": "x", "color": "#fff" },
        "commands": { "start": "bun dev" },
        "time": { "created": 1, "updated": 2, "initialized": 3 },
        "sandboxes": ["/repo-wt"]
    }));
    round_trip::<Project>(
        json!({ "id": "global", "worktree": "/", "time": { "created": 1, "updated": 2 }, "sandboxes": [] }),
    );
    round_trip::<MessageWithParts>(json!({
        "info": { "id": "msg_1", "sessionID": "ses_1", "role": "user", "time": { "created": 1 }, "agent": "build",
                  "model": { "providerID": "p", "modelID": "m" } },
        "parts": [{ "id": "prt_1", "sessionID": "ses_1", "messageID": "msg_1", "type": "text", "text": "hi" }]
    }));
}

#[test]
fn titles_slugs_and_timestamps() {
    assert_eq!(iso_timestamp(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
    assert_eq!(iso_timestamp(951_782_400_000), "2000-02-29T00:00:00.000Z");
    assert_eq!(iso_timestamp(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(
        default_title(false, 1_700_000_000_123),
        "New session - 2023-11-14T22:13:20.123Z"
    );
    assert_eq!(
        default_title(true, 0),
        "Child session - 1970-01-01T00:00:00.000Z"
    );
    assert!(is_default_title("Child session - 1970-01-01T00:00:00.000Z"));
    assert!(!is_default_title("New session - 1970-01-01T00:00:00Z"));
    assert!(!is_default_title("Fix the bug"));
    let slug = slug();
    let (adjective, noun) = slug.split_once('-').unwrap();
    assert!(!adjective.is_empty() && !noun.is_empty());
}

#[test]
fn new_session_defaults() {
    let session = Session::new(NewSession {
        project_id: "global".into(),
        directory: "/work".into(),
        version: "0.1.0".into(),
        ..NewSession::default()
    })
    .unwrap();
    assert!(session.id.starts_with("ses_") && session.id.len() == 30);
    assert!(session.has_default_title() && session.title.starts_with("New session - "));
    assert_eq!(
        (session.cost, session.tokens),
        (Some(0.0), Some(SessionTokens::default()))
    );
    assert_eq!(session.time.created, session.time.updated);

    let child = Session::new(NewSession {
        project_id: "global".into(),
        parent_id: Some(session.id.clone()),
        ..NewSession::default()
    })
    .unwrap();
    assert!(child.title.starts_with("Child session - "));
    assert!(child.id < session.id);
    assert!(
        Session::new(NewSession {
            id: Some("msg_x".into()),
            ..NewSession::default()
        })
        .is_err()
    );
}

fn project(id: &str) -> Project {
    Project {
        id: id.into(),
        worktree: "/repo".into(),
        vcs: Some(Vcs::Git),
        name: None,
        icon: None,
        commands: None,
        time: ProjectTime {
            created: 1,
            updated: 1,
            initialized: None,
        },
        sandboxes: vec![],
    }
}

fn session(store: &Store, project_id: &str, parent: Option<&str>) -> Session {
    let session = Session::new(NewSession {
        project_id: project_id.into(),
        directory: "/repo".into(),
        version: "test".into(),
        parent_id: parent.map(str::to_string),
        ..NewSession::default()
    })
    .unwrap();
    store.create_session(&session).unwrap();
    session
}

fn user(session_id: &str) -> Message {
    Message::User(UserMessage {
        id: id::message(),
        session_id: session_id.into(),
        time: UserTime {
            created: id::now_ms(),
        },
        format: None,
        summary: None,
        agent: "build".into(),
        model: UserModel {
            provider_id: "test".into(),
            model_id: "test".into(),
            variant: None,
        },
        system: None,
        tools: None,
    })
}

fn text(message: &Message, value: &str) -> Part {
    Part::new(
        message.session_id(),
        message.id(),
        PartBody::Text(TextPart {
            text: value.into(),
            ..TextPart::default()
        }),
    )
}

fn step_finish(message: &Message, cost: f64, input: f64) -> Part {
    Part::new(
        message.session_id(),
        message.id(),
        PartBody::StepFinish(StepFinishPart {
            reason: "stop".into(),
            snapshot: None,
            cost,
            tokens: Tokens {
                total: Some(9999.0),
                input,
                output: 2.0,
                reasoning: 3.0,
                cache: TokenCache {
                    read: 4.0,
                    write: 5.0,
                },
            },
        }),
    )
}

fn totals(store: &Store, session_id: &str) -> (f64, SessionTokens) {
    let session = store.get_session(session_id).unwrap().unwrap();
    (session.cost.unwrap(), session.tokens.unwrap())
}

#[test]
fn store_is_shareable_across_threads() {
    fn assert_send_sync<T: Send + Sync + Clone + 'static>() {}
    assert_send_sync::<Store>();
}

#[test]
fn open_creates_schema_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = Store::default_path(dir.path());
    assert_eq!(path.file_name().unwrap(), "opencode-rs.db");
    {
        let store = Store::open(&path).unwrap();
        assert_eq!(store.applied_migrations().unwrap(), MIGRATION_IDS);
        store.upsert_project(&project("p1")).unwrap();
        session(&store, "p1", None);
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.applied_migrations().unwrap(), MIGRATION_IDS);
    assert_eq!(store.list_sessions("p1").unwrap().len(), 1);
    assert!(matches!(
        Store::open(dir.path().join("opencode.db")),
        Err(StoreError::ForeignDatabase(_))
    ));
    assert!(!dir.path().join("opencode.db").exists());
}

#[test]
fn project_upsert_and_cascade() {
    let store = Store::open_in_memory().unwrap();
    let mut info = project("p1");
    store.upsert_project(&info).unwrap();
    info.name = Some("renamed".into());
    info.time.updated = 5;
    store.upsert_project(&info).unwrap();
    assert_eq!(store.get_project("p1").unwrap(), Some(info));
    assert_eq!(store.get_project("missing").unwrap(), None);
    assert_eq!(store.list_projects().unwrap().len(), 1);

    let created = session(&store, "p1", None);
    assert!(store.delete_project("p1").unwrap());
    assert_eq!(store.get_session(&created.id).unwrap(), None);
}

#[test]
fn session_create_update_list_and_children() {
    let store = Store::open_in_memory().unwrap();
    store.upsert_project(&project("p1")).unwrap();
    store.upsert_project(&project("p2")).unwrap();

    let orphan = Session::new(NewSession {
        project_id: "missing".into(),
        ..NewSession::default()
    })
    .unwrap();
    assert!(matches!(
        store.create_session(&orphan),
        Err(StoreError::Sqlite(_))
    ));

    let mut first = session(&store, "p1", None);
    let second = session(&store, "p1", None);
    let other = session(&store, "p2", None);
    let child = session(&store, "p1", Some(&first.id));
    assert!(matches!(
        store.create_session(&first),
        Err(StoreError::AlreadyExists {
            kind: "session",
            ..
        })
    ));
    assert_eq!(store.get_session(&first.id).unwrap().as_ref(), Some(&first));
    assert_eq!(store.get_session("ses_missing").unwrap(), None);

    first.title = "Renamed".into();
    first.time.updated += 10_000;
    first.share = Some(SessionShare {
        url: "https://share.example/1".into(),
    });
    first.revert = Some(SessionRevert {
        message_id: "msg_1".into(),
        part_id: None,
        snapshot: Some("abc".into()),
        diff: None,
    });
    first.summary = Some(SessionSummary {
        additions: 1.0,
        deletions: 2.0,
        files: 1.0,
        diffs: None,
    });
    store.update_session(&first).unwrap();
    assert_eq!(store.get_session(&first.id).unwrap().as_ref(), Some(&first));

    let listed: Vec<String> = store
        .list_sessions("p1")
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0], first.id);
    assert!(
        listed.contains(&second.id) && listed.contains(&child.id) && !listed.contains(&other.id)
    );
    assert_eq!(store.list_children(&first.id).unwrap(), vec![child.clone()]);
    assert!(store.list_children(&second.id).unwrap().is_empty());

    let patched = store
        .patch_session(&second.id, |session| {
            session.title = "Patched".into();
            session.revert = None;
        })
        .unwrap();
    assert_eq!(patched.title, "Patched");
    assert_eq!(store.get_session(&second.id).unwrap(), Some(patched));

    let mut ghost = other.clone();
    ghost.id = "ses_ghost".into();
    assert!(matches!(
        store.update_session(&ghost),
        Err(StoreError::NotFound { .. })
    ));
    assert!(matches!(
        store.patch_session("ses_ghost", |_| {}),
        Err(StoreError::NotFound { .. })
    ));
}

#[test]
fn equal_update_times_list_by_id_descending() {
    let store = Store::open_in_memory().unwrap();
    store.upsert_project(&project("p1")).unwrap();
    let mut ids = Vec::new();
    for index in 0..3 {
        let created = session(&store, "p1", None);
        store
            .patch_session(&created.id, |session| session.time.updated = 100)
            .unwrap();
        ids.push((index, created.id));
    }
    let listed: Vec<String> = store
        .list_sessions("p1")
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    let mut expected: Vec<String> = ids.into_iter().map(|(_, id)| id).collect();
    expected.sort_by(|left, right| right.cmp(left));
    assert_eq!(listed, expected);
}

#[test]
fn messages_and_parts_list_in_id_order() {
    let store = Store::open_in_memory().unwrap();
    store.upsert_project(&project("p1")).unwrap();
    let info = session(&store, "p1", None);
    let other = session(&store, "p1", None);

    let first = user(&info.id);
    let second = user(&info.id);
    let foreign = user(&other.id);
    let first_parts = [text(&first, "a"), text(&first, "b"), text(&first, "c")];
    let second_part = text(&second, "d");

    store.upsert_message(&second).unwrap();
    store.upsert_message(&foreign).unwrap();
    store.upsert_message(&first).unwrap();
    store.upsert_part(&second_part).unwrap();
    for part in first_parts.iter().rev() {
        store.upsert_part(part).unwrap();
    }
    store.upsert_part(&text(&foreign, "x")).unwrap();

    let listed = store.list_messages(&info.id).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0], (first.clone(), first_parts.to_vec()));
    assert_eq!(listed[1], (second.clone(), vec![second_part.clone()]));
    assert_eq!(
        store.get_message(first.id()).unwrap(),
        Some(listed[0].clone())
    );
    assert_eq!(store.get_message("msg_missing").unwrap(), None);
    assert!(store.list_messages("ses_missing").unwrap().is_empty());

    let mut edited = first_parts[1].clone();
    edited.body = PartBody::Text(TextPart {
        text: "B".into(),
        ..TextPart::default()
    });
    store.upsert_part(&edited).unwrap();
    assert_eq!(store.get_part(&edited.id).unwrap(), Some(edited.clone()));
    assert_eq!(store.list_messages(&info.id).unwrap()[0].1[1], edited);

    assert!(store.delete_part(&first_parts[0].id).unwrap());
    assert!(!store.delete_part(&first_parts[0].id).unwrap());
    assert_eq!(store.list_messages(&info.id).unwrap()[0].1.len(), 2);

    assert!(store.delete_message(first.id()).unwrap());
    assert!(!store.delete_message(first.id()).unwrap());
    assert_eq!(store.get_part(&edited.id).unwrap(), None);
    assert_eq!(store.list_messages(&info.id).unwrap().len(), 1);
    assert_eq!(store.list_messages(&other.id).unwrap().len(), 1);

    let dangling = Part::new(
        &info.id,
        "msg_missing",
        PartBody::StepStart(StepStartPart::default()),
    );
    assert!(matches!(
        store.upsert_part(&dangling),
        Err(StoreError::Sqlite(_))
    ));
    assert!(matches!(
        store.upsert_message(&user("ses_missing")),
        Err(StoreError::Sqlite(_))
    ));
}

#[test]
fn step_finish_parts_accumulate_into_session_totals() {
    let store = Store::open_in_memory().unwrap();
    store.upsert_project(&project("p1")).unwrap();
    let info = session(&store, "p1", None);
    let message = user(&info.id);
    let other = user(&info.id);
    store.upsert_message(&message).unwrap();
    store.upsert_message(&other).unwrap();
    let tokens = |input: f64, steps: f64| SessionTokens {
        input,
        output: 2.0 * steps,
        reasoning: 3.0 * steps,
        cache: TokenCache {
            read: 4.0 * steps,
            write: 5.0 * steps,
        },
    };

    let mut step = step_finish(&message, 0.25, 100.0);
    store.upsert_part(&step).unwrap();
    store.upsert_part(&text(&message, "ignored")).unwrap();
    assert_eq!(totals(&store, &info.id), (0.25, tokens(100.0, 1.0)));

    let PartBody::StepFinish(body) = &mut step.body else {
        unreachable!()
    };
    body.cost = 0.5;
    body.tokens.input = 40.0;
    store.upsert_part(&step).unwrap();
    assert_eq!(totals(&store, &info.id), (0.5, tokens(40.0, 1.0)));

    let second = step_finish(&other, 0.25, 10.0);
    let third = step_finish(&other, 1.0, 1.0);
    store.upsert_part(&second).unwrap();
    store.upsert_part(&third).unwrap();
    assert_eq!(totals(&store, &info.id), (1.75, tokens(51.0, 3.0)));
    assert_eq!(
        store.get_session(&info.id).unwrap().unwrap().time.updated,
        info.time.updated
    );

    assert!(store.delete_part(&third.id).unwrap());
    assert_eq!(totals(&store, &info.id), (0.75, tokens(50.0, 2.0)));
    assert!(store.delete_message(other.id()).unwrap());
    assert_eq!(totals(&store, &info.id), (0.5, tokens(40.0, 1.0)));

    let patched = store
        .patch_session(&info.id, |session| session.title = "kept totals".into())
        .unwrap();
    assert_eq!(patched.cost, Some(0.5));
    assert_eq!(totals(&store, &info.id), (0.5, tokens(40.0, 1.0)));
}

#[test]
fn delete_session_cascades_to_children_messages_and_parts() {
    let store = Store::open_in_memory().unwrap();
    store.upsert_project(&project("p1")).unwrap();
    let root = session(&store, "p1", None);
    let child = session(&store, "p1", Some(&root.id));
    let grandchild = session(&store, "p1", Some(&child.id));
    let sibling = session(&store, "p1", None);
    let mut part_ids = Vec::new();
    for owner in [&root, &child, &grandchild, &sibling] {
        let message = user(&owner.id);
        let part = text(&message, "t");
        store.upsert_message(&message).unwrap();
        store.upsert_part(&part).unwrap();
        part_ids.push(part.id);
    }

    assert!(store.delete_session(&root.id).unwrap());
    assert!(!store.delete_session(&root.id).unwrap());
    for gone in [&root, &child, &grandchild] {
        assert_eq!(store.get_session(&gone.id).unwrap(), None);
        assert!(store.list_messages(&gone.id).unwrap().is_empty());
    }
    for id in &part_ids[..3] {
        assert_eq!(store.get_part(id).unwrap(), None);
    }
    assert!(store.get_part(&part_ids[3]).unwrap().is_some());
    assert_eq!(store.list_sessions("p1").unwrap(), vec![sibling]);
}

#[test]
fn store_works_from_concurrent_threads() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(Store::default_path(dir.path())).unwrap();
    store.upsert_project(&project("p1")).unwrap();
    let info = session(&store, "p1", None);
    let message = user(&info.id);
    store.upsert_message(&message).unwrap();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            let message = message.clone();
            std::thread::spawn(move || {
                for _ in 0..10 {
                    store.upsert_part(&step_finish(&message, 1.0, 1.0)).unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let parts = &store.list_messages(&info.id).unwrap()[0].1;
    assert_eq!(parts.len(), 80);
    assert!(parts.windows(2).all(|pair| pair[0].id < pair[1].id));
    assert_eq!(totals(&store, &info.id).0, 80.0);
}

#[test]
fn project_identity_feeds_the_store() {
    let dir = tempfile::tempdir().unwrap();
    if identity::discover(dir.path()).is_some() {
        return;
    }
    let resolved = identity::resolve(dir.path());
    let store = Store::open_in_memory().unwrap();
    let mut info = project(&resolved.id);
    info.vcs = None;
    info.worktree = "/".into();
    store.upsert_project(&info).unwrap();
    assert_eq!(store.get_project(GLOBAL_PROJECT_ID).unwrap(), Some(info));
}

#[test]
fn todos_replace_and_cascade() {
    let store = Store::open_in_memory().unwrap();
    let now = crate::id::now_ms();
    store
        .upsert_project(&Project {
            id: "p".into(),
            worktree: "/".into(),
            vcs: None,
            name: None,
            icon: None,
            commands: None,
            time: ProjectTime { created: now, updated: now, initialized: None },
            sandboxes: Vec::new(),
        })
        .unwrap();
    let session = Session::new(NewSession { project_id: "p".into(), directory: "/".into(), version: "0".into(), ..Default::default() }).unwrap();
    store.create_session(&session).unwrap();
    let todo = |content: &str| Todo { content: content.into(), status: "pending".into(), priority: "low".into() };
    store.set_todos(&session.id, &[todo("a"), todo("b")]).unwrap();
    store.set_todos(&session.id, &[todo("c"), todo("a")]).unwrap();
    assert_eq!(store.get_todos(&session.id).unwrap(), [todo("c"), todo("a")]);
    store.delete_session(&session.id).unwrap();
    assert!(store.get_todos(&session.id).unwrap().is_empty());
}

const MIGRATION_IDS: [&str; 6] = ["0001_init", "0002_todo", "0003_event", "0004_workspace", "0005_project_directory", "0006_account"];

fn kinds(events: &[crate::StoredEvent]) -> Vec<&str> {
    events.iter().map(|event| event.kind.as_str()).collect()
}

#[test]
fn row_writes_log_durable_session_events() {
    let store = Store::open_in_memory().unwrap();
    store.upsert_project(&project("p")).unwrap();
    let session = session(&store, "p", None);
    let message = user(&session.id);
    store.upsert_message(&message).unwrap();
    let part = text(&message, "hi");
    store.upsert_part(&part).unwrap();
    store.patch_session(&session.id, |s| s.title = "Renamed".into()).unwrap();
    store.project_session(&session.id, |s| s.agent = Some("plan".into())).unwrap();
    store.delete_part(&part.id).unwrap();
    store.delete_message(message.id()).unwrap();
    let id = id::ascending(id::Prefix::Event);
    let data = json!({"sessionID": session.id, "agent": "plan"});
    let seq = store.append_event(&id, &session.id, "session.next.agent.switched.1", &data).unwrap();

    let events = store.list_events(&session.id, None, None, None).unwrap();
    let expected = [
        "session.created.1",
        "message.updated.1",
        "message.part.updated.1",
        "session.updated.1",
        "message.part.removed.1",
        "message.removed.1",
        "session.next.agent.switched.1",
    ];
    assert_eq!(kinds(&events), expected);
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), (0..7).collect::<Vec<u64>>());
    assert_eq!((seq, store.latest_sequence(&session.id).unwrap()), (6, Some(6)));
    assert_eq!(events[0].data["info"]["id"], json!(session.id));
    assert_eq!(events[2].data["part"]["text"], "hi");
    assert_eq!(events[3].data["info"]["title"], "Renamed");
    assert_eq!(events[4].data, json!({"sessionID": session.id, "messageID": message.id(), "partID": part.id}));
    assert!(matches!(store.append_event(&id, &session.id, "x", &json!({})), Err(StoreError::InvalidEvent { .. })));

    let only = ["session.next.agent.switched.1"];
    assert_eq!(store.list_events(&session.id, Some(2), Some(&only), None).unwrap().len(), 1);
    assert_eq!(kinds(&store.list_events(&session.id, Some(4), None, Some(1)).unwrap()), ["message.removed.1"]);

    let other = self::session(&store, "p", None);
    let history = store.event_history(&[(session.id.clone(), 5)]).unwrap();
    let rows: Vec<(&str, u64)> = history.iter().map(|e| (e.aggregate_id.as_str(), e.seq)).collect();
    assert_eq!(rows, [(other.id.as_str(), 0), (session.id.as_str(), 6)]);

    // A deleted session takes its log with it.
    store.delete_session(&session.id).unwrap();
    assert!(store.list_events(&session.id, None, None, None).unwrap().is_empty());
    assert_eq!(store.latest_sequence(&session.id).unwrap(), None);
    store.delete_project("p").unwrap();
    assert!(store.event_history(&[]).unwrap().is_empty());
}

#[test]
fn replay_rebuilds_rows_and_guards_the_log() {
    let source = Store::open_in_memory().unwrap();
    source.upsert_project(&project("p")).unwrap();
    let session = session(&source, "p", None);
    let message = user(&session.id);
    source.upsert_message(&message).unwrap();
    source.upsert_part(&step_finish(&message, 0.5, 10.0)).unwrap();
    let log = source.list_events(&session.id, None, None, None).unwrap();

    let target = Store::open_in_memory().unwrap();
    target.upsert_project(&project("p")).unwrap();
    assert_eq!(target.replay_events(&log, Some("wrk_1"), true).unwrap(), Some(session.id.clone()));
    assert_eq!(target.get_session(&session.id).unwrap().unwrap().title, session.title);
    assert_eq!(target.list_messages(&session.id).unwrap()[0].1.len(), 1);
    assert_eq!(totals(&target, &session.id).0, 0.5);
    assert_eq!(target.list_events(&session.id, None, None, None).unwrap(), log);
    // The same history again changes nothing.
    assert!(!target.replay_event(&log[1], Some("wrk_1"), true).unwrap());
    assert_eq!(totals(&target, &session.id).0, 0.5);

    let mut diverged = log[2].clone();
    diverged.data["time"] = json!(1);
    let error = target.replay_event(&diverged, Some("wrk_1"), true).unwrap_err().to_string();
    assert_eq!(error, format!("Replay diverged at aggregate {} sequence 2", session.id));
    let mut gap = log[2].clone();
    gap.seq = 5;
    gap.id = id::ascending(id::Prefix::Event);
    let error = target.replay_event(&gap, Some("wrk_1"), true).unwrap_err().to_string();
    assert_eq!(error, format!("Sequence mismatch for aggregate {}: expected 3, got 5", session.id));
    assert!(target.replay_event(&gap, Some("wrk_2"), true).unwrap_err().to_string().starts_with("Replay owner mismatch"));
    // Another owner's events are ignored without strict ownership; a claim hands the aggregate over.
    assert!(!target.replay_event(&gap, Some("wrk_2"), false).unwrap());
    target.claim_events(&session.id, "wrk_2").unwrap();
    assert!(target.replay_event(&log[1], Some("wrk_1"), true).is_err());
    let unknown = crate::StoredEvent { kind: "nope.1".into(), ..log[0].clone() };
    assert_eq!(target.replay_event(&unknown, None, false).unwrap_err().to_string(), "Unknown durable event type nope.1");
    let shuffled = [log[1].clone(), log[0].clone()];
    assert!(target.replay_events(&shuffled, None, false).unwrap_err().to_string().starts_with("Replay sequence mismatch at index 1"));
    assert_eq!(target.replay_events(&[], None, false).unwrap(), None);

    target.remove_events(&session.id).unwrap();
    assert!(target.list_events(&session.id, None, None, None).unwrap().is_empty());
}

#[test]
fn workspaces_and_project_directories() {
    let store = Store::open_in_memory().unwrap();
    store.upsert_project(&project("p")).unwrap();
    let workspace = |id: &str| crate::WorkspaceInfo {
        id: id.into(),
        kind: "worktree".into(),
        name: "calm-moon".into(),
        branch: None,
        directory: Some("/w".into()),
        extra: None,
        project_id: "p".into(),
        time_used: 5,
    };
    store.insert_workspace(&workspace("wrk_2")).unwrap();
    store.insert_workspace(&crate::WorkspaceInfo { extra: Some(json!({"a": 1})), ..workspace("wrk_1") }).unwrap();
    let listed = store.list_workspaces("p").unwrap();
    assert_eq!(listed.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), ["wrk_1", "wrk_2"]);
    let shown = json!({"id": "wrk_2", "type": "worktree", "name": "calm-moon", "branch": null, "directory": "/w", "extra": null, "projectID": "p", "timeUsed": 5});
    assert_eq!(serde_json::to_value(&listed[1]).unwrap(), shown);
    assert_eq!(store.get_workspace("wrk_1").unwrap().unwrap().extra, Some(json!({"a": 1})));

    let inside = NewSession { project_id: "p".into(), directory: "/w".into(), version: "t".into(), workspace_id: Some("wrk_1".into()), ..Default::default() };
    let inside = Session::new(inside).unwrap();
    store.create_session(&inside).unwrap();
    session(&store, "p", None);
    assert_eq!(store.list_workspace_sessions("wrk_1").unwrap().iter().map(|s| &s.id).collect::<Vec<_>>(), [&inside.id]);
    assert!(store.get_workspace("wrk_1").unwrap().unwrap().time_used > 5);
    assert!(store.delete_workspace("wrk_1").unwrap() && !store.delete_workspace("wrk_1").unwrap());

    assert!(store.add_project_directory("p", "/a", None, false).unwrap());
    assert!(!store.add_project_directory("p", "/a", None, false).unwrap());
    assert!(store.add_project_directory("p", "/a", Some("git_worktree"), true).unwrap());
    assert!(!store.add_project_directory("p", "/a", Some("git_worktree"), true).unwrap());
    assert!(store.add_project_directory("p", "/a", None, true).unwrap());
    assert!(!store.add_project_directory("p", "/a", None, true).unwrap());
    store.add_project_directory("p", "/b", Some("git_worktree"), false).unwrap();
    assert_eq!(store.list_project_directories("p").unwrap().len(), 2);
    assert_eq!(store.get_project_directory("p", "/b").unwrap().unwrap().strategy.as_deref(), Some("git_worktree"));
    assert_eq!(serde_json::to_value(store.get_project_directory("p", "/a").unwrap().unwrap()).unwrap(), json!({"directory": "/a"}));
    assert!(store.remove_project_directory("p", "/a").unwrap() && !store.remove_project_directory("p", "/a").unwrap());
    store.delete_project("p").unwrap();
    assert!(store.list_project_directories("p").unwrap().is_empty() && store.list_workspaces("p").unwrap().is_empty());
}
