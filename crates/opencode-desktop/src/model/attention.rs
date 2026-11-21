// When the original app plays a sound or posts a system notification, decided per engine signal without I/O.
// Sources: `app/src/context/notification.tsx:338-410` (session idle / error) and `app/src/pages/layout.tsx:375-457`
// (permission / question asked), sound files `ui/src/assets/audio/*.aac` listed in `app/src/utils/sound.ts:13-59`.
use super::i18n::I18n;
use super::settings::Settings;
use super::state::project_name;
use std::collections::HashMap;
use std::path::PathBuf;

// `layout.tsx:379`: one permission or question alert per session in this window.
pub const COOLDOWN_MS: u64 = 5000;

#[derive(Debug, PartialEq, Eq)]
pub struct Sound {
    pub id: &'static str,
    pub label: &'static str,
    pub bytes: &'static [u8],
}

macro_rules! sounds {
    ($($id:literal => $label:literal),* $(,)?) => {
        pub static SOUNDS: &[Sound] = &[$(Sound { id: $id, label: $label, bytes: include_bytes!(concat!("../../assets/sounds/", $id, ".aac")) }),*];
    };
}

sounds! {
    "alert-01" => "sound.option.alert01",
    "alert-02" => "sound.option.alert02",
    "alert-03" => "sound.option.alert03",
    "alert-04" => "sound.option.alert04",
    "alert-05" => "sound.option.alert05",
    "alert-06" => "sound.option.alert06",
    "alert-07" => "sound.option.alert07",
    "alert-08" => "sound.option.alert08",
    "alert-09" => "sound.option.alert09",
    "alert-10" => "sound.option.alert10",
    "bip-bop-01" => "sound.option.bipbop01",
    "bip-bop-02" => "sound.option.bipbop02",
    "bip-bop-03" => "sound.option.bipbop03",
    "bip-bop-04" => "sound.option.bipbop04",
    "bip-bop-05" => "sound.option.bipbop05",
    "bip-bop-06" => "sound.option.bipbop06",
    "bip-bop-07" => "sound.option.bipbop07",
    "bip-bop-08" => "sound.option.bipbop08",
    "bip-bop-09" => "sound.option.bipbop09",
    "bip-bop-10" => "sound.option.bipbop10",
    "staplebops-01" => "sound.option.staplebops01",
    "staplebops-02" => "sound.option.staplebops02",
    "staplebops-03" => "sound.option.staplebops03",
    "staplebops-04" => "sound.option.staplebops04",
    "staplebops-05" => "sound.option.staplebops05",
    "staplebops-06" => "sound.option.staplebops06",
    "staplebops-07" => "sound.option.staplebops07",
    "nope-01" => "sound.option.nope01",
    "nope-02" => "sound.option.nope02",
    "nope-03" => "sound.option.nope03",
    "nope-04" => "sound.option.nope04",
    "nope-05" => "sound.option.nope05",
    "nope-06" => "sound.option.nope06",
    "nope-07" => "sound.option.nope07",
    "nope-08" => "sound.option.nope08",
    "nope-09" => "sound.option.nope09",
    "nope-10" => "sound.option.nope10",
    "nope-11" => "sound.option.nope11",
    "nope-12" => "sound.option.nope12",
    "yup-01" => "sound.option.yup01",
    "yup-02" => "sound.option.yup02",
    "yup-03" => "sound.option.yup03",
    "yup-04" => "sound.option.yup04",
    "yup-05" => "sound.option.yup05",
    "yup-06" => "sound.option.yup06",
}

// The picker entry that turns a sound off (`settings-general.tsx:219`).
pub const NO_SOUND: (&str, &str) = ("none", "sound.option.none");

// An id the table does not know plays nothing, like `soundSrc` in the original.
pub fn sound(id: &str) -> Option<&'static Sound> {
    SOUNDS.iter().find(|sound| sound.id == id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    // `session.idle`: the run of a session ended, however it ended.
    Idle,
    // `session.error`, aborts included.
    Error,
    // `permission.asked` / `question.asked`, with the id of the request the view must still be waiting on.
    Permission(u64),
    Question(u64),
}

// The session as the store had it when the signal was raised, the original's `lookup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub title: String,
    pub child: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    pub kind: Kind,
    pub directory: PathBuf,
    pub session_id: Option<String>,
    pub session: Option<Found>,
}

// Where clicking a notification leads: the session, or the project when there is none.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub directory: PathBuf,
    pub session_id: Option<String>,
}

const TAG_SEPARATOR: char = '\u{1f}';

impl Target {
    // Notification tags carry their target, so a click needs no bookkeeping; one tag per session and kind.
    pub fn tag(&self, kind: &str) -> String {
        format!("{kind}{TAG_SEPARATOR}{}{TAG_SEPARATOR}{}", self.session_id.as_deref().unwrap_or(""), self.directory.to_string_lossy())
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        let mut parts = tag.splitn(3, TAG_SEPARATOR);
        let (_, session, directory) = (parts.next()?, parts.next()?, parts.next()?);
        (!directory.is_empty()).then(|| Target { directory: directory.into(), session_id: (!session.is_empty()).then(|| session.to_string()) })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    // `turn-complete`, `error`, `permission` or `question`; replaces an earlier one of the same kind for the session.
    pub kind: &'static str,
    pub title: String,
    pub body: String,
    pub target: Target,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Alert {
    pub sound: Option<&'static str>,
    pub notification: Option<Notification>,
}

#[derive(Debug, Default)]
pub struct Attention {
    // `${directory}:${sessionID}` -> when it last alerted.
    alerted: HashMap<String, u64>,
}

impl Attention {
    // `focused`: the window has focus. The original's `platform.notify` drops notifications then; sounds still play.
    pub fn decide(&mut self, signal: &Signal, settings: &Settings, i18n: &I18n, focused: bool, now_ms: u64) -> Alert {
        let mut alert = self.decide_any(signal, settings, i18n, now_ms);
        if focused {
            alert.notification = None;
        }
        alert
    }

    fn decide_any(&mut self, signal: &Signal, settings: &Settings, i18n: &I18n, now_ms: u64) -> Alert {
        let target = Target { directory: signal.directory.clone(), session_id: signal.session_id.clone() };
        let notification = |kind: &'static str, title: &str, body: String| Notification { kind, title: i18n.t(title).to_string(), body, target: target.clone() };
        let child = signal.session.as_ref().is_some_and(|found| found.child);
        match &signal.kind {
            Kind::Idle => {
                // A session the store does not know is not announced at all.
                let Some(found) = signal.session.as_ref().filter(|found| !found.child) else { return Alert::default() };
                Alert {
                    sound: settings.sound_agent.then_some(settings.sound_agent_id.as_str()).and_then(sound).map(|s| s.id),
                    notification: settings
                        .notify_agent
                        .then(|| notification("turn-complete", "notification.session.responseReady.title", found.title.clone())),
                }
            }
            Kind::Error if child => Alert::default(),
            Kind::Error => {
                let body = signal.session.as_ref().map_or_else(|| i18n.t("notification.session.error.fallbackDescription").to_string(), |s| s.title.clone());
                Alert {
                    sound: settings.sound_errors.then_some(settings.sound_errors_id.as_str()).and_then(sound).map(|s| s.id),
                    notification: settings.notify_errors.then(|| notification("error", "notification.session.error.title", body)),
                }
            }
            Kind::Permission(_) | Kind::Question(_) => {
                let key = format!("{}:{}", signal.directory.to_string_lossy(), signal.session_id.as_deref().unwrap_or(""));
                if self.alerted.get(&key).is_some_and(|at| now_ms.saturating_sub(*at) < COOLDOWN_MS) {
                    return Alert::default();
                }
                self.alerted.insert(key, now_ms);
                let session_title = signal.session.as_ref().map_or_else(|| i18n.t("command.session.new").to_string(), |s| s.title.clone());
                let project = project_name(&signal.directory);
                let vars = [("sessionTitle", session_title.as_str()), ("projectName", project.as_str())];
                if matches!(signal.kind, Kind::Question(_)) {
                    let body = i18n.tf("notification.question.description", &vars);
                    return Alert { sound: None, notification: settings.notify_agent.then(|| notification("question", "notification.question.title", body)) };
                }
                let body = i18n.tf("notification.permission.description", &vars);
                Alert {
                    sound: settings.sound_permissions.then_some(settings.sound_permissions_id.as_str()).and_then(sound).map(|s| s.id),
                    notification: settings.notify_permissions.then(|| notification("permission", "notification.permission.title", body)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::Toggle;

    fn signal(kind: Kind, session: Option<(&str, bool)>) -> Signal {
        Signal {
            kind,
            directory: PathBuf::from("/work/app"),
            session_id: Some("ses_1".into()),
            session: session.map(|(title, child)| Found { title: title.into(), child }),
        }
    }

    fn english() -> I18n {
        I18n::new("en")
    }

    fn text(alert: &Alert) -> Option<(&str, &str)> {
        alert.notification.as_ref().map(|n| (n.title.as_str(), n.body.as_str()))
    }

    #[test]
    fn every_sound_of_the_original_is_embedded_with_a_label() {
        assert_eq!(SOUNDS.len(), 45);
        let english = english();
        for sound in SOUNDS {
            assert!(sound.bytes.len() > 1000, "{} is suspiciously small", sound.id);
            assert_eq!(&sound.bytes[..2], [0xff, 0xf1], "{} is not ADTS AAC", sound.id);
            assert!(english.has(sound.label), "{}", sound.label);
        }
        assert!(english.has(NO_SOUND.1));
        let defaults = Settings::default();
        for id in [&defaults.sound_agent_id, &defaults.sound_permissions_id, &defaults.sound_errors_id] {
            assert!(sound(id).is_some(), "default sound {id} is missing");
        }
        assert_eq!(sound("gone"), None);
    }

    #[test]
    fn a_finished_turn_plays_the_agent_sound_and_notifies_only_while_unfocused() {
        let (settings, english) = (Settings::default(), english());
        let mut attention = Attention::default();
        let done = signal(Kind::Idle, Some(("Fix the parser", false)));
        let alert = attention.decide(&done, &settings, &english, false, 1);
        assert_eq!(alert.sound, Some("staplebops-01"));
        assert_eq!(text(&alert), Some(("Response ready", "Fix the parser")));
        let target = &alert.notification.as_ref().unwrap().target;
        assert_eq!((target.directory.to_str(), target.session_id.as_deref()), (Some("/work/app"), Some("ses_1")));

        let focused = attention.decide(&done, &settings, &english, true, 2);
        assert_eq!((focused.sound, focused.notification), (Some("staplebops-01"), None));

        // Subagent sessions and sessions the store does not know stay silent.
        assert_eq!(attention.decide(&signal(Kind::Idle, Some(("child", true))), &settings, &english, false, 3), Alert::default());
        assert_eq!(attention.decide(&signal(Kind::Idle, None), &settings, &english, false, 4), Alert::default());
    }

    #[test]
    fn each_toggle_switches_exactly_its_own_alert() {
        let english = english();
        let cases = [
            (Toggle::SoundAgent, Kind::Idle, true),
            (Toggle::NotifyAgent, Kind::Idle, false),
            (Toggle::SoundErrors, Kind::Error, true),
            (Toggle::NotifyErrors, Kind::Error, false),
            (Toggle::SoundPermissions, Kind::Permission(1), true),
            (Toggle::NotifyPermissions, Kind::Permission(1), false),
            (Toggle::NotifyAgent, Kind::Question(1), false),
        ];
        for (toggle, kind, is_sound) in cases {
            let mut on = Settings::default();
            for every in Toggle::NOTIFICATIONS.into_iter().chain(Toggle::SOUNDS) {
                on.set(every, true);
            }
            let mut off = on.clone();
            off.set(toggle, false);
            let found = signal(kind.clone(), Some(("Title", false)));
            let with = Attention::default().decide(&found, &on, &english, false, 0);
            let without = Attention::default().decide(&found, &off, &english, false, 0);
            if is_sound {
                assert!(with.sound.is_some() && without.sound.is_none(), "{toggle:?} on {kind:?}");
                assert_eq!(with.notification, without.notification, "{toggle:?} on {kind:?}");
            } else {
                assert!(with.notification.is_some() && without.notification.is_none(), "{toggle:?} on {kind:?}");
                assert_eq!(with.sound, without.sound, "{toggle:?} on {kind:?}");
            }
        }
    }

    #[test]
    fn errors_follow_the_original_defaults_and_fallback_text() {
        let english = english();
        let mut settings = Settings::default();
        let mut attention = Attention::default();
        // Error notifications are off by default; the error sound is on.
        let alert = attention.decide(&signal(Kind::Error, Some(("Build", false))), &settings, &english, false, 0);
        assert_eq!((alert.sound, alert.notification), (Some("nope-03"), None));

        settings.notify_errors = true;
        let alert = attention.decide(&signal(Kind::Error, Some(("Build", false))), &settings, &english, false, 0);
        assert_eq!(text(&alert), Some(("Session error", "Build")));
        let unknown = attention.decide(&signal(Kind::Error, None), &settings, &english, false, 0);
        assert_eq!(text(&unknown), Some(("Session error", "An error occurred")));
        assert_eq!(attention.decide(&signal(Kind::Error, Some(("child", true))), &settings, &english, false, 0), Alert::default());

        // A sound id the table does not know plays nothing.
        settings.sound_errors_id = "removed-sound".into();
        assert_eq!(attention.decide(&signal(Kind::Error, None), &settings, &english, false, 0).sound, None);
    }

    #[test]
    fn requests_name_session_and_project_and_cool_down_per_session() {
        let (settings, english) = (Settings::default(), english());
        let mut attention = Attention::default();
        let asked = signal(Kind::Permission(1), Some(("Refactor", false)));
        let alert = attention.decide(&asked, &settings, &english, false, 10_000);
        assert_eq!(alert.sound, Some("staplebops-02"));
        assert_eq!(text(&alert), Some(("Permission required", "Refactor in app needs permission")));

        // A question within five seconds of the permission is swallowed, like a second permission.
        let question = signal(Kind::Question(2), Some(("Refactor", false)));
        assert_eq!(attention.decide(&question, &settings, &english, false, 10_000 + COOLDOWN_MS - 1), Alert::default());
        let alert = attention.decide(&question, &settings, &english, false, 10_000 + COOLDOWN_MS);
        assert_eq!(alert.sound, None, "questions have no sound of their own");
        assert_eq!(text(&alert), Some(("Question", "Refactor in app has a question")));

        // Another session is not held back, and an unknown session reads as a new one.
        let mut other = signal(Kind::Permission(3), None);
        other.session_id = Some("ses_2".into());
        let alert = attention.decide(&other, &settings, &english, true, 10_001);
        assert_eq!((alert.sound, alert.notification), (Some("staplebops-02"), None));
        let alert = Attention::default().decide(&other, &settings, &english, false, 0);
        assert_eq!(text(&alert), Some(("Permission required", "New session in app needs permission")));
    }

    #[test]
    fn notification_tags_round_trip_their_target() {
        let target = Target { directory: PathBuf::from(r"C:\work\my app"), session_id: Some("ses_9".into()) };
        assert_eq!(Target::from_tag(&target.tag("permission")), Some(target.clone()));
        assert_ne!(target.tag("permission"), target.tag("question"));
        let project = Target { session_id: None, ..target };
        assert_eq!(Target::from_tag(&project.tag("error")), Some(project));
        assert_eq!(Target::from_tag("garbage"), None);
    }
}
