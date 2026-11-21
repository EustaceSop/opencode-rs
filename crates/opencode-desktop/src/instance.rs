// One app per user, like Electron's `requestSingleInstanceLock` + `second-instance` (`desktop/src/main/index.ts:198-216`):
// a later start hands its directory and links to the running app over a local socket and exits.
use crate::model::deeplink::Request;
use interprocess::local_socket::{GenericNamespaced, Listener, ListenerOptions, Stream, prelude::*};
use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

const ACK: &str = "ok";
// A request is a directory and a few links; anything longer is not one of ours.
const MAX_REQUEST: u64 = 64 * 1024;

pub enum Start {
    // This process is the app; later starts arrive through `serve`. None when the socket could not be made.
    First(Option<Listener>),
    // A running app took the request.
    Forwarded,
}

// Scoped to the state directory, so each user (and each throwaway smoke or test root) gets its own app.
pub fn name(state_dir: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in state_dir.to_string_lossy().to_lowercase().bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("ai.opencode.desktop.rs-{hash:016x}")
}

pub fn start(name: &str, request: &Request) -> Start {
    for attempt in 0..3 {
        if forward(name, request).is_ok() {
            return Start::Forwarded;
        }
        match listen(name) {
            Ok(listener) => return Start::First(Some(listener)),
            // Another start won the race to the name; it accepts connections in a moment.
            Err(_) => std::thread::sleep(Duration::from_millis(100 * (attempt + 1))),
        }
    }
    eprintln!("opencode-desktop: cannot reach or become the running instance; starting alone");
    Start::First(None)
}

fn listen(name: &str) -> io::Result<Listener> {
    ListenerOptions::new().name(name.to_ns_name::<GenericNamespaced>()?).create_sync()
}

fn forward(name: &str, request: &Request) -> io::Result<()> {
    let stream = Stream::connect(name.to_ns_name::<GenericNamespaced>()?)?;
    let mut stream = BufReader::new(stream);
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    stream.get_mut().write_all(line.as_bytes())?;
    let mut answer = String::new();
    stream.read_line(&mut answer)?;
    if answer.trim() == ACK { Ok(()) } else { Err(io::Error::other("the running instance did not take the request")) }
}

pub fn serve(listener: Listener, requests: UnboundedSender<Request>) {
    let spawned = std::thread::Builder::new().name("opencode-instance".into()).spawn(move || {
        for connection in listener.incoming().filter_map(Result::ok) {
            let mut connection = BufReader::new(connection);
            let mut line = String::new();
            if (&mut connection).take(MAX_REQUEST).read_line(&mut line).is_err() {
                continue;
            }
            let Ok(request) = serde_json::from_str::<Request>(&line) else { continue };
            let taken = requests.send(request).is_ok();
            let _ = connection.get_mut().write_all(if taken { b"ok\n" } else { b"closing\n" });
            if !taken {
                return;
            }
        }
    });
    if let Err(error) = spawned {
        eprintln!("opencode-desktop: cannot listen for other starts: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn names_are_stable_per_state_directory() {
        let a = name(Path::new(r"C:\Users\a\.local\state\opencode"));
        assert_eq!(a, name(Path::new(r"c:\users\A\.local\state\opencode")));
        assert_ne!(a, name(Path::new(r"C:\Users\b\.local\state\opencode")));
        assert!(a.starts_with("ai.opencode.desktop.rs-") && a.len() == "ai.opencode.desktop.rs-".len() + 16);
    }

    #[test]
    fn a_second_start_hands_its_request_to_the_first() {
        let root = tempfile::tempdir().unwrap();
        let name = name(root.path());
        let Start::First(Some(listener)) = start(&name, &Request::default()) else { panic!("the first start must own the name") };
        let (sender, mut received) = unbounded_channel();
        serve(listener, sender);

        let request = Request { directory: Some(PathBuf::from(r"C:\work\app")), links: vec!["opencode://open-project?directory=/a".into()] };
        assert!(matches!(start(&name, &request), Start::Forwarded));
        assert_eq!(received.blocking_recv(), Some(request));
        assert!(matches!(start(&name, &Request::default()), Start::Forwarded), "a bare start only brings the window forward");
        assert_eq!(received.blocking_recv(), Some(Request::default()));

        // Once the app stops taking requests, a later start is told so instead of being swallowed.
        drop(received);
        assert!(forward(&name, &Request::default()).is_err());
    }
}
