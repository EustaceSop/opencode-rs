// Sounds and system notifications. The view talks to a trait so tests and smoke runs record or stay silent.
use crate::model::attention::{Notification, Sound};
use gpui_kit::{App, SystemNotification};
use std::io::Cursor;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

pub trait Alerts {
    fn play(&self, sound: &'static Sound);
    // The settings picker's preview; a new one cuts the previous one off, None only stops it.
    fn preview(&self, sound: Option<&'static Sound>);
    fn notify(&self, notification: &Notification, cx: &mut App);
}

// The output device is released once nothing has played for this long.
const IDLE: Duration = Duration::from_secs(10);

enum Audio {
    Play(&'static [u8]),
    Preview(Option<&'static [u8]>),
}

// Windows toasts through GPUI (silent, the app plays its own sound) and the original's AAC files through rodio.
pub struct System {
    audio: Sender<Audio>,
}

impl System {
    pub fn new() -> Self {
        let (audio, receiver) = channel();
        if let Err(error) = std::thread::Builder::new().name("opencode-audio".into()).spawn(move || play_loop(receiver)) {
            eprintln!("opencode-desktop: no audio thread: {error}");
        }
        Self { audio }
    }
}

impl Alerts for System {
    fn play(&self, sound: &'static Sound) {
        let _ = self.audio.send(Audio::Play(sound.bytes));
    }

    fn preview(&self, sound: Option<&'static Sound>) {
        let _ = self.audio.send(Audio::Preview(sound.map(|s| s.bytes)));
    }

    fn notify(&self, notification: &Notification, cx: &mut App) {
        cx.show_system_notification(SystemNotification {
            tag: notification.target.tag(notification.kind).into(),
            title: notification.title.clone().into(),
            body: notification.body.clone().into(),
            actions: Vec::new(),
        });
    }
}

fn stop(preview: &mut Option<rodio::Player>) {
    if let Some(player) = preview.take() {
        player.stop();
    }
}

fn play_loop(receiver: Receiver<Audio>) {
    let mut device: Option<rodio::MixerDeviceSink> = None;
    let mut preview: Option<rodio::Player> = None;
    loop {
        let next = if device.is_some() { receiver.recv_timeout(IDLE) } else { receiver.recv().map_err(|_| RecvTimeoutError::Disconnected) };
        let audio = match next {
            Ok(audio) => audio,
            Err(RecvTimeoutError::Timeout) => {
                stop(&mut preview);
                device = None;
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if let Audio::Preview(None) = audio {
            stop(&mut preview);
            continue;
        }
        if device.is_none() {
            device = rodio::DeviceSinkBuilder::open_default_sink()
                .map(|mut sink| {
                    sink.log_on_drop(false);
                    sink
                })
                .map_err(|error| eprintln!("opencode-desktop: no audio output: {error}"))
                .ok();
        }
        let Some(sink) = &device else { continue };
        match audio {
            Audio::Play(bytes) => {
                if let Some(source) = decode(bytes) {
                    sink.mixer().add(source);
                }
            }
            Audio::Preview(Some(bytes)) => {
                stop(&mut preview);
                preview = decode(bytes).map(|source| {
                    let player = rodio::Player::connect_new(sink.mixer());
                    player.append(source);
                    player
                });
            }
            Audio::Preview(None) => {}
        }
    }
}

pub(crate) fn decode(bytes: &'static [u8]) -> Option<rodio::Decoder<Cursor<&'static [u8]>>> {
    rodio::Decoder::new_aac(Cursor::new(bytes)).map_err(|error| eprintln!("opencode-desktop: cannot decode a sound: {error}")).ok()
}

// Smoke runs: the window must not make noise or post anything.
pub struct Silent;

impl Alerts for Silent {
    fn play(&self, _: &'static Sound) {}
    fn preview(&self, _: Option<&'static Sound>) {}
    fn notify(&self, _: &Notification, _: &mut App) {}
}

#[cfg(test)]
#[derive(Default)]
pub struct Recorder {
    pub played: std::cell::RefCell<Vec<&'static str>>,
    pub previews: std::cell::RefCell<Vec<Option<&'static str>>>,
    pub notified: std::cell::RefCell<Vec<Notification>>,
}

#[cfg(test)]
impl Alerts for std::rc::Rc<Recorder> {
    fn play(&self, sound: &'static Sound) {
        self.played.borrow_mut().push(sound.id);
    }

    fn preview(&self, sound: Option<&'static Sound>) {
        self.previews.borrow_mut().push(sound.map(|s| s.id));
    }

    fn notify(&self, notification: &Notification, _: &mut App) {
        self.notified.borrow_mut().push(notification.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::attention::SOUNDS;
    use rodio::Source as _;

    // Decoding needs no output device; every embedded sound must come out as audio of about a second.
    #[test]
    fn every_embedded_sound_decodes() {
        for sound in SOUNDS {
            let source = decode(sound.bytes).unwrap_or_else(|| panic!("{} does not decode", sound.id));
            let (rate, channels) = (source.sample_rate().get(), source.channels().get());
            let samples = source.count();
            let seconds = samples as f64 / (rate as f64 * channels as f64);
            assert!((0.05..10.0).contains(&seconds), "{} lasts {seconds:.2}s", sound.id);
        }
    }
}
