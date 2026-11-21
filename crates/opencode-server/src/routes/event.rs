// `/event` and `/global/event`: `data: <json>\n\n` frames, an in-band heartbeat every 10 s, no ids or retry lines.
use crate::bus::{Event, event_id};
use crate::state::{AppState, Instance};
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Instant, Interval, MissedTickBehavior, interval_at};

pub const HEARTBEAT: Duration = Duration::from_secs(10);

fn frame(value: &Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

fn synthetic(kind: &str) -> Value {
    json!({"id": event_id(), "type": kind, "properties": {}})
}

enum Step {
    Frame(Value),
    Skip,
    // Emit, then end the stream.
    Last(Value),
}

struct Feed<F> {
    first: Option<Value>,
    events: UnboundedReceiver<Arc<Event>>,
    heartbeat: Interval,
    map: F,
    wrap: fn(Value) -> Value,
    done: bool,
}

fn respond<F>(beat: Duration, events: UnboundedReceiver<Arc<Event>>, wrap: fn(Value) -> Value, map: F) -> Response
where
    F: FnMut(&Event) -> Step + Send + 'static,
{
    let mut heartbeat = interval_at(Instant::now() + beat, beat);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let feed = Feed { first: Some(wrap(synthetic("server.connected"))), events, heartbeat, map, wrap, done: false };
    let body = stream::unfold(feed, |mut feed| async move {
        if let Some(first) = feed.first.take() {
            return Some((Ok::<_, Infallible>(frame(&first)), feed));
        }
        loop {
            if feed.done {
                return None;
            }
            tokio::select! {
                event = feed.events.recv() => match (feed.map)(event?.as_ref()) {
                    Step::Frame(value) => return Some((Ok(frame(&value)), feed)),
                    Step::Last(value) => {
                        feed.done = true;
                        return Some((Ok(frame(&value)), feed));
                    }
                    Step::Skip => {}
                },
                _ = feed.heartbeat.tick() => {
                    let beat = (feed.wrap)(synthetic("server.heartbeat"));
                    return Some((Ok(frame(&beat)), feed));
                }
            }
        }
    });
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache, no-transform"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        Body::from_stream(body),
    )
        .into_response()
}

// Counts an open `/event` stream of a directory for as long as the stream lives.
struct Open {
    state: AppState,
    directory: String,
}

impl Drop for Open {
    fn drop(&mut self) {
        let mut streams = crate::state::lock(&self.state.streams);
        if let Some(count) = streams.get_mut(&self.directory) {
            *count = count.saturating_sub(1);
        }
        self.state.touch(&self.directory);
    }
}

pub async fn instance(State(state): State<AppState>, instance: Instance) -> Response {
    // Subscribed before the first frame is written, so nothing published after connect is lost.
    let events = state.bus.subscribe();
    let open = Open { state: state.clone(), directory: instance.directory.clone() };
    *crate::state::lock(&state.streams).entry(instance.directory.clone()).or_default() += 1;
    respond(state.heartbeat, events, |payload| payload, move |event| {
        let _open = &open;
        if event.directory.as_deref() != Some(instance.directory.as_str()) {
            return Step::Skip;
        }
        match (event.global_only, event.kind.as_str()) {
            (_, "server.instance.disposed") => Step::Last(event.payload()),
            (true, _) => Step::Skip,
            (false, _) => Step::Frame(event.payload()),
        }
    })
}

pub async fn global(State(state): State<AppState>) -> Response {
    let events = state.bus.subscribe();
    respond(state.heartbeat, events, |payload| json!({"payload": payload}), |event| Step::Frame(event.global()))
}
