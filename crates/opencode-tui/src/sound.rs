// Rust build: the desktop app's sounds in the TUI. A reply that finishes, a permission request and an error each play
// the sound chosen in the desktop's settings (`<state>/desktop-rs.json`, read only), with the original app's defaults
// when that file has none. `rs.sounds: false` in kv.json silences the TUI alone.
use serde_json::Value;
use std::io::Cursor;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chime {
    Done,
    Permission,
    Error,
}

macro_rules! sounds {
    ($($id:literal),* $(,)?) => {
        const SOUNDS: &[(&str, &[u8])] = &[$(($id, include_bytes!(concat!("../../opencode-desktop/assets/sounds/", $id, ".aac")))),*];
    };
}

sounds!(
    "alert-01", "alert-02", "alert-03", "alert-04", "alert-05", "alert-06", "alert-07", "alert-08", "alert-09", "alert-10",
    "bip-bop-01", "bip-bop-02", "bip-bop-03", "bip-bop-04", "bip-bop-05", "bip-bop-06", "bip-bop-07", "bip-bop-08", "bip-bop-09",
    "bip-bop-10", "nope-01", "nope-02", "nope-03", "nope-04", "nope-05", "nope-06", "nope-07", "nope-08", "nope-09", "nope-10",
    "nope-11", "nope-12", "staplebops-01", "staplebops-02", "staplebops-03", "staplebops-04", "staplebops-05", "staplebops-06",
    "staplebops-07", "yup-01", "yup-02", "yup-03", "yup-04", "yup-05", "yup-06",
);

fn bytes(id: &str) -> Option<&'static [u8]> {
    SOUNDS.iter().find(|(name, _)| *name == id).map(|(_, data)| *data)
}

// Which sound each event plays, or none; the desktop's settings keys and defaults (`app/src/context/settings.tsx`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Choice {
    pub done: Option<&'static str>,
    pub permission: Option<&'static str>,
    pub error: Option<&'static str>,
}

impl Choice {
    pub fn load(state: &Path) -> Self {
        let file = std::fs::read_to_string(state.join("desktop-rs.json")).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let settings = file.as_ref().and_then(|value| value.get("settings"));
        let pick = |enabled: &str, id: &str, default: &'static str| -> Option<&'static str> {
            let on = settings.and_then(|s| s.get(enabled)).and_then(Value::as_bool).unwrap_or(true);
            let wanted = settings.and_then(|s| s.get(id)).and_then(Value::as_str).unwrap_or(default);
            let id = SOUNDS.iter().map(|(name, _)| *name).find(|name| *name == wanted).unwrap_or(default);
            on.then_some(id)
        };
        Choice {
            done: pick("sounds.agentEnabled", "sounds.agent", "staplebops-01"),
            permission: pick("sounds.permissionsEnabled", "sounds.permissions", "staplebops-02"),
            error: pick("sounds.errorsEnabled", "sounds.errors", "nope-03"),
        }
    }

    pub fn sound(&self, chime: Chime) -> Option<&'static str> {
        match chime {
            Chime::Done => self.done,
            Chime::Permission => self.permission,
            Chime::Error => self.error,
        }
    }
}

// The output device is released once nothing has played for this long.
const IDLE: Duration = Duration::from_secs(10);

// Plays on a thread of its own, so the UI never waits for the audio device.
pub struct Player {
    queue: Sender<&'static [u8]>,
}

impl Player {
    pub fn start() -> Option<Self> {
        let (queue, receiver) = channel();
        std::thread::Builder::new().name("opencode-tui-audio".into()).spawn(move || play_loop(receiver)).ok()?;
        Some(Self { queue })
    }

    pub fn play(&self, id: &str) {
        if let Some(data) = bytes(id) {
            let _ = self.queue.send(data);
        }
    }
}

fn play_loop(receiver: Receiver<&'static [u8]>) {
    let mut device: Option<rodio::MixerDeviceSink> = None;
    loop {
        let next = if device.is_some() { receiver.recv_timeout(IDLE) } else { receiver.recv().map_err(|_| RecvTimeoutError::Disconnected) };
        let data = match next {
            Ok(data) => data,
            Err(RecvTimeoutError::Timeout) => {
                device = None;
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if device.is_none() {
            // The terminal belongs to the UI: a missing device stays silent instead of printing.
            device = rodio::DeviceSinkBuilder::open_default_sink()
                .map(|mut sink| {
                    sink.log_on_drop(false);
                    sink
                })
                .ok();
        }
        if let (Some(sink), Ok(source)) = (&device, rodio::Decoder::new_aac(Cursor::new(data))) {
            sink.mixer().add(source);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_desktop_settings_with_the_original_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Choice::load(dir.path()), Choice { done: Some("staplebops-01"), permission: Some("staplebops-02"), error: Some("nope-03") });
        std::fs::write(
            dir.path().join("desktop-rs.json"),
            r#"{"settings": {"sounds.agent": "yup-02", "sounds.errorsEnabled": false, "sounds.permissions": "gone"}}"#,
        )
        .unwrap();
        let choice = Choice::load(dir.path());
        assert_eq!((choice.done, choice.permission, choice.error), (Some("yup-02"), Some("staplebops-02"), None));
        assert!(SOUNDS.len() == 45 && SOUNDS.iter().all(|(_, data)| data.len() > 100));
        // Every embedded sound decodes without an output device.
        for (id, data) in SOUNDS {
            assert!(rodio::Decoder::new_aac(Cursor::new(*data)).is_ok(), "{id}");
        }
    }
}
