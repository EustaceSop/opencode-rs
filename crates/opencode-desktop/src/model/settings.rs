// User settings, stored under the original app's `settings.v3` key names inside `desktop-rs.json`.
use serde::{Deserialize, Serialize};

pub const FONT_SIZE_DEFAULT: f32 = 14.0;
pub const FONT_SIZE_RANGE: (f32, f32) = (10.0, 24.0);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    #[default]
    System,
    Light,
    Dark,
}

impl Scheme {
    pub const ALL: [Scheme; 3] = [Scheme::System, Scheme::Light, Scheme::Dark];

    pub fn key(self) -> &'static str {
        match self {
            Scheme::System => "theme.scheme.system",
            Scheme::Light => "theme.scheme.light",
            Scheme::Dark => "theme.scheme.dark",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Scheme::System => "system",
            Scheme::Light => "light",
            Scheme::Dark => "dark",
        }
    }

    pub fn is_dark(self, system_dark: bool) -> bool {
        match self {
            Scheme::System => system_dark,
            Scheme::Light => false,
            Scheme::Dark => true,
        }
    }
}

fn yes() -> bool {
    true
}

fn default_theme() -> String {
    super::theme::DEFAULT_THEME.to_string()
}

fn default_font_size() -> f32 {
    FONT_SIZE_DEFAULT
}

// `app/src/context/settings.tsx:214-221`.
fn agent_sound() -> String {
    "staplebops-01".into()
}

fn permissions_sound() -> String {
    "staplebops-02".into()
}

fn errors_sound() -> String {
    "nope-03".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(rename = "theme.id", default = "default_theme")]
    pub theme: String,
    #[serde(rename = "theme.colorScheme", default)]
    pub scheme: Scheme,
    #[serde(rename = "appearance.fontSize", default = "default_font_size")]
    pub font_size: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(rename = "model.default", default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(rename = "agent.default", default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    // The original hides reasoning by default; this client always had it visible.
    #[serde(rename = "general.showReasoningSummaries", default = "yes")]
    pub show_reasoning: bool,
    #[serde(rename = "general.shellToolPartsExpanded", default)]
    pub shell_expanded: bool,
    #[serde(rename = "general.editToolPartsExpanded", default)]
    pub edit_expanded: bool,
    #[serde(rename = "permissions.autoApprove", default)]
    pub auto_accept: bool,
    #[serde(rename = "notifications.agent", default = "yes")]
    pub notify_agent: bool,
    #[serde(rename = "notifications.permissions", default = "yes")]
    pub notify_permissions: bool,
    #[serde(rename = "notifications.errors", default)]
    pub notify_errors: bool,
    #[serde(rename = "sounds.agentEnabled", default = "yes")]
    pub sound_agent: bool,
    #[serde(rename = "sounds.agent", default = "agent_sound")]
    pub sound_agent_id: String,
    #[serde(rename = "sounds.permissionsEnabled", default = "yes")]
    pub sound_permissions: bool,
    #[serde(rename = "sounds.permissions", default = "permissions_sound")]
    pub sound_permissions_id: String,
    #[serde(rename = "sounds.errorsEnabled", default = "yes")]
    pub sound_errors: bool,
    #[serde(rename = "sounds.errors", default = "errors_sound")]
    pub sound_errors_id: String,
    #[serde(rename = "layout.sidebar.opened", default = "yes")]
    pub sidebar: bool,
}

impl Default for Settings {
    fn default() -> Self {
        serde_json::from_str("{}").expect("every setting has a default")
    }
}

// Switches the settings view offers. A sound toggle is shown as the original's picker, where "None" turns it off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    ShowReasoning,
    ShellExpanded,
    EditExpanded,
    AutoAccept,
    NotifyAgent,
    NotifyPermissions,
    NotifyErrors,
    SoundAgent,
    SoundPermissions,
    SoundErrors,
}

impl Toggle {
    pub const GENERAL: [Toggle; 4] = [Toggle::ShowReasoning, Toggle::ShellExpanded, Toggle::EditExpanded, Toggle::AutoAccept];
    pub const NOTIFICATIONS: [Toggle; 3] = [Toggle::NotifyAgent, Toggle::NotifyPermissions, Toggle::NotifyErrors];
    pub const SOUNDS: [Toggle; 3] = [Toggle::SoundAgent, Toggle::SoundPermissions, Toggle::SoundErrors];

    pub fn id(self) -> &'static str {
        match self {
            Toggle::ShowReasoning => "general.showReasoningSummaries",
            Toggle::ShellExpanded => "general.shellToolPartsExpanded",
            Toggle::EditExpanded => "general.editToolPartsExpanded",
            Toggle::AutoAccept => "permissions.autoApprove",
            Toggle::NotifyAgent => "notifications.agent",
            Toggle::NotifyPermissions => "notifications.permissions",
            Toggle::NotifyErrors => "notifications.errors",
            Toggle::SoundAgent => "sounds.agentEnabled",
            Toggle::SoundPermissions => "sounds.permissionsEnabled",
            Toggle::SoundErrors => "sounds.errorsEnabled",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Toggle::ShowReasoning => "settings.general.row.reasoningSummaries.title",
            Toggle::ShellExpanded => "settings.general.row.shellToolPartsExpanded.title",
            Toggle::EditExpanded => "settings.general.row.editToolPartsExpanded.title",
            Toggle::AutoAccept => "command.permissions.autoaccept.enable",
            Toggle::NotifyAgent => "settings.general.notifications.agent.title",
            Toggle::NotifyPermissions => "settings.general.notifications.permissions.title",
            Toggle::NotifyErrors => "settings.general.notifications.errors.title",
            Toggle::SoundAgent => "settings.general.sounds.agent.title",
            Toggle::SoundPermissions => "settings.general.sounds.permissions.title",
            Toggle::SoundErrors => "settings.general.sounds.errors.title",
        }
    }

    pub fn description(self) -> Option<&'static str> {
        match self {
            Toggle::ShowReasoning => Some("settings.general.row.reasoningSummaries.description"),
            Toggle::ShellExpanded => Some("settings.general.row.shellToolPartsExpanded.description"),
            Toggle::EditExpanded => Some("settings.general.row.editToolPartsExpanded.description"),
            Toggle::AutoAccept => Some("toast.permissions.autoaccept.on.description"),
            Toggle::NotifyAgent => Some("settings.general.notifications.agent.description"),
            Toggle::NotifyPermissions => Some("settings.general.notifications.permissions.description"),
            Toggle::NotifyErrors => Some("settings.general.notifications.errors.description"),
            Toggle::SoundAgent => Some("settings.general.sounds.agent.description"),
            Toggle::SoundPermissions => Some("settings.general.sounds.permissions.description"),
            Toggle::SoundErrors => Some("settings.general.sounds.errors.description"),
        }
    }
}

impl Settings {
    pub fn get(&self, toggle: Toggle) -> bool {
        match toggle {
            Toggle::ShowReasoning => self.show_reasoning,
            Toggle::ShellExpanded => self.shell_expanded,
            Toggle::EditExpanded => self.edit_expanded,
            Toggle::AutoAccept => self.auto_accept,
            Toggle::NotifyAgent => self.notify_agent,
            Toggle::NotifyPermissions => self.notify_permissions,
            Toggle::NotifyErrors => self.notify_errors,
            Toggle::SoundAgent => self.sound_agent,
            Toggle::SoundPermissions => self.sound_permissions,
            Toggle::SoundErrors => self.sound_errors,
        }
    }

    pub fn set(&mut self, toggle: Toggle, value: bool) {
        let field = match toggle {
            Toggle::ShowReasoning => &mut self.show_reasoning,
            Toggle::ShellExpanded => &mut self.shell_expanded,
            Toggle::EditExpanded => &mut self.edit_expanded,
            Toggle::AutoAccept => &mut self.auto_accept,
            Toggle::NotifyAgent => &mut self.notify_agent,
            Toggle::NotifyPermissions => &mut self.notify_permissions,
            Toggle::NotifyErrors => &mut self.notify_errors,
            Toggle::SoundAgent => &mut self.sound_agent,
            Toggle::SoundPermissions => &mut self.sound_permissions,
            Toggle::SoundErrors => &mut self.sound_errors,
        };
        *field = value;
    }

    // The chosen sound of a sound toggle; None for the other toggles.
    pub fn sound_id(&self, toggle: Toggle) -> Option<&str> {
        match toggle {
            Toggle::SoundAgent => Some(&self.sound_agent_id),
            Toggle::SoundPermissions => Some(&self.sound_permissions_id),
            Toggle::SoundErrors => Some(&self.sound_errors_id),
            _ => None,
        }
    }

    // What the picker of a sound toggle selects: a sound turns it on, `None` turns it off and keeps the last sound.
    pub fn choose_sound(&mut self, toggle: Toggle, id: Option<&str>) {
        let slot = match toggle {
            Toggle::SoundAgent => &mut self.sound_agent_id,
            Toggle::SoundPermissions => &mut self.sound_permissions_id,
            Toggle::SoundErrors => &mut self.sound_errors_id,
            _ => return,
        };
        if let Some(id) = id {
            *slot = id.to_string();
        }
        self.set(toggle, id.is_some());
    }

    pub fn font_size(&self) -> f32 {
        if self.font_size.is_finite() { self.font_size.clamp(FONT_SIZE_RANGE.0, FONT_SIZE_RANGE.1) } else { FONT_SIZE_DEFAULT }
    }

    pub fn step_font_size(&mut self, delta: f32) {
        self.font_size = (self.font_size() + delta).clamp(FONT_SIZE_RANGE.0, FONT_SIZE_RANGE.1);
    }

    // A theme id written by another version may no longer exist.
    pub fn theme(&self) -> &'static super::theme::Theme {
        super::theme::find(&self.theme).or_else(|| super::theme::find(super::theme::DEFAULT_THEME)).expect("the default theme is built in")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_original_app() {
        let settings = Settings::default();
        assert_eq!((settings.theme.as_str(), settings.scheme, settings.font_size), ("clay", Scheme::System, 14.0));
        assert!(!settings.auto_accept && settings.notify_agent && !settings.notify_errors && settings.sound_errors);
        assert_eq!(settings.theme().id, "clay");
    }

    #[test]
    fn round_trips_under_the_original_key_names() {
        let mut settings = Settings { theme: "dracula".into(), scheme: Scheme::Dark, ..Default::default() };
        settings.set(Toggle::AutoAccept, true);
        settings.step_font_size(100.0);
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(json["theme.id"], "dracula");
        assert_eq!(json["theme.colorScheme"], "dark");
        assert_eq!(json["permissions.autoApprove"], true);
        assert_eq!(json["appearance.fontSize"], 24.0);
        assert_eq!(serde_json::from_value::<Settings>(json).unwrap(), settings);

        let partial: Settings = serde_json::from_str(r#"{"theme.id": "gone", "appearance.fontSize": 3}"#).unwrap();
        assert_eq!(partial.theme().id, "clay");
        assert_eq!(partial.font_size(), 10.0);
        assert!(partial.show_reasoning);
    }

    #[test]
    fn toggles_cover_their_fields() {
        let mut settings = Settings::default();
        for toggle in Toggle::GENERAL.into_iter().chain(Toggle::NOTIFICATIONS).chain(Toggle::SOUNDS) {
            let before = settings.get(toggle);
            settings.set(toggle, !before);
            assert_eq!(settings.get(toggle), !before, "{}", toggle.id());
            assert_eq!(settings.sound_id(toggle).is_some(), Toggle::SOUNDS.contains(&toggle));
        }
        assert!(Scheme::Dark.is_dark(false) && !Scheme::Light.is_dark(true) && Scheme::System.is_dark(true));
    }

    #[test]
    fn sound_choices_follow_the_original_picker() {
        let mut settings = Settings::default();
        assert_eq!(
            (settings.sound_id(Toggle::SoundAgent), settings.sound_id(Toggle::SoundPermissions), settings.sound_id(Toggle::SoundErrors)),
            (Some("staplebops-01"), Some("staplebops-02"), Some("nope-03"))
        );
        settings.choose_sound(Toggle::SoundErrors, None);
        assert!(!settings.sound_errors);
        assert_eq!(settings.sound_id(Toggle::SoundErrors), Some("nope-03"), "None keeps the last choice");
        settings.choose_sound(Toggle::SoundErrors, Some("yup-02"));
        assert!(settings.sound_errors);
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!((json["sounds.errors"].as_str(), json["sounds.errorsEnabled"].as_bool()), (Some("yup-02"), Some(true)));
        settings.choose_sound(Toggle::AutoAccept, Some("yup-02"));
        assert!(!settings.auto_accept);
    }
}
