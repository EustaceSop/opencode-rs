// `opencode://` links, parsed as `app/src/pages/layout/deep-links.ts` does, and what a launch asks of the window.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const PREFIX: &str = "opencode://";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    OpenProject { directory: String },
    // The prompt prefills the new session's input; it is not sent.
    NewSession { directory: String, prompt: Option<String> },
}

pub fn parse(input: &str) -> Option<Link> {
    if !input.starts_with(PREFIX) {
        return None;
    }
    let url = url::Url::parse(input).ok()?;
    // `URLSearchParams.get`: the first value, and an empty one counts as missing.
    let param = |name: &str| url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned()).filter(|value| !value.is_empty());
    match url.host_str()? {
        "open-project" => Some(Link::OpenProject { directory: param("directory")? }),
        "new-session" => Some(Link::NewSession { directory: param("directory")?, prompt: param("prompt") }),
        _ => None,
    }
}

// A start of the app: its own, or a second one handing its arguments to the running window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    // An explicit project directory, already absolute.
    pub directory: Option<PathBuf>,
    pub links: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(directory: &str) -> Option<Link> {
        Some(Link::OpenProject { directory: directory.into() })
    }

    // The cases of `app/src/pages/layout/helpers.test.ts:41-101`.
    #[test]
    fn parses_the_links_the_original_accepts() {
        assert_eq!(parse("opencode://open-project?directory=/tmp/demo"), open("/tmp/demo"));
        assert_eq!(parse("opencode://other?directory=/tmp/demo"), None);
        assert_eq!(parse("https://example.com"), None);
        assert_eq!(parse("opencode://open-project/%E0%A4%A%"), None);
        assert_eq!(parse("opencode://open-project"), None);
        assert_eq!(parse("opencode://open-project?directory="), None);
        let links = ["opencode://open-project?directory=/a", "opencode://other?directory=/b", "opencode://open-project?directory=/c"];
        assert_eq!(links.iter().filter_map(|l| parse(l)).collect::<Vec<_>>(), [open("/a").unwrap(), open("/c").unwrap()]);

        assert_eq!(parse("opencode://new-session?directory=/tmp/demo"), Some(Link::NewSession { directory: "/tmp/demo".into(), prompt: None }));
        assert_eq!(
            parse("opencode://new-session?directory=/tmp/demo&prompt=hello%20world"),
            Some(Link::NewSession { directory: "/tmp/demo".into(), prompt: Some("hello world".into()) })
        );
        assert_eq!(parse("opencode://new-session"), None);
        assert_eq!(parse("opencode://new-session?directory="), None);
        assert_eq!(parse("opencode://new-session?directory=/c&prompt="), Some(Link::NewSession { directory: "/c".into(), prompt: None }));
    }

    #[test]
    fn windows_paths_and_form_encoding_survive() {
        assert_eq!(parse(r"opencode://open-project?directory=C:\work\my%20app"), open(r"C:\work\my app"));
        assert_eq!(parse("opencode://open-project?directory=C%3A%5Cwork&directory=ignored"), open(r"C:\work"));
        assert_eq!(
            parse("opencode://new-session?prompt=ship+it&directory=/x"),
            Some(Link::NewSession { directory: "/x".into(), prompt: Some("ship it".into()) })
        );
    }

    #[test]
    fn requests_round_trip_as_json() {
        let request = Request { directory: Some(PathBuf::from(r"C:\work")), links: vec!["opencode://open-project?directory=/a".into()] };
        let line = serde_json::to_string(&request).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
    }
}
