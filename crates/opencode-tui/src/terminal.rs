// Terminal capabilities: colour queries, the generated `system` theme, colour-depth and glyph fallbacks, the
// clipboard and the external editor.
use crate::theme::{Mode, Rgba, ansi, tint};
use opencode_core::flag::Lookup;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use serde_json::{Map, Value, json};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

// `renderer.waitForThemeMode(1000)`
pub const QUERY_TIMEOUT: Duration = Duration::from_millis(1000);
pub const PALETTE_SIZE: usize = 16;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TerminalColors {
    pub background: Option<Rgba>,
    pub foreground: Option<Rgba>,
    pub palette: Vec<Option<Rgba>>,
}

// OSC 10/11 and OSC 4 queries, closed by a device-attributes request every terminal answers.
pub fn query() -> String {
    let mut out = String::from("\u{1b}]10;?\u{7}\u{1b}]11;?\u{7}");
    for index in 0..PALETTE_SIZE {
        out.push_str(&format!("\u{1b}]4;{index};?\u{7}"));
    }
    out.push_str("\u{1b}[c");
    out
}

// "rgb:RRRR/GGGG/BBBB" with 1-4 hex digits per channel, or "#RRGGBB".
fn parse_color(spec: &str) -> Option<Rgba> {
    if spec.starts_with('#') {
        return Rgba::from_hex(spec);
    }
    let body = spec.strip_prefix("rgb:").or_else(|| spec.strip_prefix("rgba:"))?;
    let mut channels = body.split('/').map(|part| {
        let value = u32::from_str_radix(part, 16).ok()?;
        let max = (1u32 << (4 * part.len().clamp(1, 4) as u32)) - 1;
        Some((value * 255 / max) as u8)
    });
    Some(Rgba::rgb(channels.next()??, channels.next()??, channels.next()??))
}

pub fn parse_responses(text: &str) -> TerminalColors {
    let mut colors = TerminalColors { palette: vec![None; PALETTE_SIZE], ..Default::default() };
    for chunk in text.split("\u{1b}]").skip(1) {
        let end = chunk.find(['\u{7}', '\u{1b}']).unwrap_or(chunk.len());
        let mut fields = chunk[..end].split(';');
        match (fields.next(), fields.next(), fields.next()) {
            (Some("10"), Some(spec), _) => colors.foreground = parse_color(spec),
            (Some("11"), Some(spec), _) => colors.background = parse_color(spec),
            (Some("4"), Some(index), Some(spec)) => {
                if let Some(slot) = index.parse::<usize>().ok().and_then(|i| colors.palette.get_mut(i)) {
                    *slot = parse_color(spec);
                }
            }
            _ => {}
        }
    }
    colors
}

// True once the device-attributes reply (`ESC [ ? ... c`) has arrived.
pub fn answered(text: &str) -> bool {
    text.rfind("\u{1b}[?").is_some_and(|at| text[at..].ends_with('c'))
}

// `terminalMode`
pub fn mode(colors: &TerminalColors) -> Option<Mode> {
    colors.background.map(|bg| if bg.luminance() > 0.5 { Mode::Light } else { Mode::Dark })
}

// Sends the queries and collects replies from stdin. Must run in raw mode before any other reader of stdin starts.
pub fn detect(timeout: Duration) -> TerminalColors {
    let mut out = std::io::stdout();
    if out.write_all(query().as_bytes()).and_then(|_| out.flush()).is_err() {
        return TerminalColors::default();
    }
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut all = Vec::new();
        let mut chunk = [0u8; 256];
        while let Ok(read) = stdin.read(&mut chunk) {
            if read == 0 {
                break;
            }
            all.extend_from_slice(&chunk[..read]);
            let done = answered(&String::from_utf8_lossy(&all));
            if tx.send(all.clone()).is_err() || done {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + timeout;
    let mut latest = Vec::new();
    while let Ok(bytes) = rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
        latest = bytes;
        if answered(&String::from_utf8_lossy(&latest)) {
            break;
        }
    }
    parse_responses(&String::from_utf8_lossy(&latest))
}

fn hex(color: Rgba) -> Value {
    Value::String(format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b))
}

fn luminance_255(bg: Rgba) -> f64 {
    0.299 * f64::from(bg.r) + 0.587 * f64::from(bg.g) + 0.114 * f64::from(bg.b)
}

// `generateGrayScale`: index 1..=12.
fn gray_scale(bg: Rgba, dark: bool) -> Vec<Rgba> {
    let lum = luminance_255(bg);
    let (r, g, b) = (f64::from(bg.r), f64::from(bg.g), f64::from(bg.b));
    let flat = |value: f64| {
        let v = value.floor().clamp(0.0, 255.0) as u8;
        Rgba::rgb(v, v, v)
    };
    let scaled = |ratio: f64| Rgba::rgb((r * ratio).clamp(0.0, 255.0).floor() as u8, (g * ratio).clamp(0.0, 255.0).floor() as u8, (b * ratio).clamp(0.0, 255.0).floor() as u8);
    let mut grays = vec![bg];
    for i in 1..=12 {
        let factor = f64::from(i) / 12.0;
        grays.push(match (dark, lum) {
            (true, lum) if lum < 10.0 => flat(factor * 0.4 * 255.0),
            (true, lum) => scaled((lum + (255.0 - lum) * factor * 0.4) / lum),
            (false, lum) if lum > 245.0 => flat(255.0 - factor * 0.4 * 255.0),
            (false, lum) => scaled(lum * (1.0 - factor * 0.4) / lum),
        });
    }
    grays
}

// `generateMutedTextColor`
fn muted_text(bg: Rgba, dark: bool) -> Rgba {
    let lum = luminance_255(bg);
    let gray = match (dark, lum) {
        (true, lum) if lum < 10.0 => 180.0,
        (true, lum) => (160.0 + lum * 0.3).floor().min(200.0),
        (false, lum) if lum > 245.0 => 75.0,
        (false, lum) => (100.0 - (255.0 - lum) * 0.2).floor().max(60.0),
    } as u8;
    Rgba::rgb(gray, gray, gray)
}

// `generateSystem`: a theme built from the terminal's own palette. None without a palette, as in the original.
pub fn system_theme(colors: &TerminalColors, mode: Mode) -> Option<Value> {
    let first = colors.palette.first().copied().flatten()?;
    let bg = colors.background.unwrap_or(first);
    let fg = colors.foreground.or_else(|| colors.palette.get(7).copied().flatten())?;
    let dark = mode == Mode::Dark;
    let col = |i: usize| colors.palette.get(i).copied().flatten().unwrap_or_else(|| ansi(i as u64));
    let grays = gray_scale(bg, dark);
    let muted = muted_text(bg, dark);
    let (red, green, yellow, blue, magenta, cyan) = (col(1), col(2), col(3), col(4), col(5), col(6));
    let alpha = if dark { 0.22 } else { 0.14 };
    let context_bg = grays[2];
    let pairs: Vec<(&str, Value)> = vec![
        ("primary", hex(cyan)),
        ("secondary", hex(magenta)),
        ("accent", hex(cyan)),
        ("error", hex(red)),
        ("warning", hex(yellow)),
        ("success", hex(green)),
        ("info", hex(cyan)),
        ("text", hex(fg)),
        ("textMuted", hex(muted)),
        ("selectedListItemText", hex(bg)),
        ("background", json!("transparent")),
        ("backgroundPanel", hex(grays[2])),
        ("backgroundElement", hex(grays[3])),
        ("backgroundMenu", hex(grays[3])),
        ("borderSubtle", hex(grays[6])),
        ("border", hex(grays[7])),
        ("borderActive", hex(grays[8])),
        ("diffAdded", hex(green)),
        ("diffRemoved", hex(red)),
        ("diffContext", hex(grays[7])),
        ("diffHunkHeader", hex(grays[7])),
        ("diffHighlightAdded", hex(col(10))),
        ("diffHighlightRemoved", hex(col(9))),
        ("diffAddedBg", hex(tint(bg, green, alpha))),
        ("diffRemovedBg", hex(tint(bg, red, alpha))),
        ("diffContextBg", hex(context_bg)),
        ("diffLineNumber", hex(muted)),
        ("diffAddedLineNumberBg", hex(tint(context_bg, green, alpha))),
        ("diffRemovedLineNumberBg", hex(tint(context_bg, red, alpha))),
        ("markdownText", hex(fg)),
        ("markdownHeading", hex(fg)),
        ("markdownLink", hex(blue)),
        ("markdownLinkText", hex(cyan)),
        ("markdownCode", hex(green)),
        ("markdownBlockQuote", hex(yellow)),
        ("markdownEmph", hex(yellow)),
        ("markdownStrong", hex(fg)),
        ("markdownHorizontalRule", hex(grays[7])),
        ("markdownListItem", hex(blue)),
        ("markdownListEnumeration", hex(cyan)),
        ("markdownImage", hex(blue)),
        ("markdownImageText", hex(cyan)),
        ("markdownCodeBlock", hex(fg)),
        ("syntaxComment", hex(muted)),
        ("syntaxKeyword", hex(magenta)),
        ("syntaxFunction", hex(blue)),
        ("syntaxVariable", hex(fg)),
        ("syntaxString", hex(green)),
        ("syntaxNumber", hex(yellow)),
        ("syntaxType", hex(cyan)),
        ("syntaxOperator", hex(cyan)),
        ("syntaxPunctuation", hex(fg)),
    ];
    let theme: Map<String, Value> = pairs.into_iter().map(|(key, value)| (key.to_string(), value)).collect();
    Some(json!({ "theme": theme }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub truecolor: bool,
    // False on consoles whose fonts lack box-drawing-heavy, braille and arrow glyphs.
    pub unicode: bool,
}

impl Default for Caps {
    fn default() -> Self {
        Self { truecolor: true, unicode: true }
    }
}

impl Caps {
    pub fn detect(env: Lookup, windows: bool) -> Self {
        let has = |key: &str| env(key).is_some_and(|v| !v.is_empty());
        let colorterm = env("COLORTERM").unwrap_or_default().to_ascii_lowercase();
        let term = env("TERM").unwrap_or_default().to_ascii_lowercase();
        let modern = has("WT_SESSION") || has("TERM_PROGRAM") || has("ConEmuPID") || has("ALACRITTY_WINDOW_ID") || has("WEZTERM_PANE") || has("KITTY_WINDOW_ID");
        let truecolor = colorterm.contains("truecolor") || colorterm.contains("24bit") || term.contains("direct") || term.contains("truecolor") || modern || windows;
        let unicode = if windows { modern || !term.is_empty() } else { term != "linux" };
        Self { truecolor, unicode }
    }
}

// Nearest xterm-256 index for an RGB colour (cube or gray ramp).
pub fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |v: u8| LEVELS.iter().enumerate().min_by_key(|(_, l)| (i32::from(**l) - i32::from(v)).abs()).map_or(0, |(i, _)| i as u8);
    let (cr, cg, cb) = (level(r), level(g), level(b));
    let cube = (LEVELS[cr as usize], LEVELS[cg as usize], LEVELS[cb as usize]);
    let gray_index = ((i32::from(r) + i32::from(g) + i32::from(b)) / 3 - 8).clamp(0, 230) / 10;
    let gray = (gray_index * 10 + 8) as u8;
    let distance = |c: (u8, u8, u8)| (i32::from(c.0) - i32::from(r)).pow(2) + (i32::from(c.1) - i32::from(g)).pow(2) + (i32::from(c.2) - i32::from(b)).pow(2);
    if distance((gray, gray, gray)) < distance(cube) { 232 + gray_index as u8 } else { 16 + 36 * cr + 6 * cg + cb }
}

fn ascii_glyph(symbol: &str) -> Option<&'static str> {
    Some(match symbol {
        "⠋" | "⠸" | "⠦" => "|",
        "⠙" | "⠼" | "⠧" => "/",
        "⠹" | "⠴" | "⠇" => "-",
        "⠏" => "\\",
        "┃" | "│" | "╹" => "|",
        "─" => "-",
        "▣" | "⚙" => "#",
        "△" => "!",
        "→" | "▶" => ">",
        "←" => "<",
        "↓" | "▼" => "v",
        "↑" => "^",
        "↳" => "\\",
        "⇆" => "~",
        "✱" | "◈" | "•" | "●" | "★" | "⬖" => "*",
        "✓" => "x",
        "✕" => "X",
        "…" | "⋯" => ".",
        "—" => "-",
        _ => return None,
    })
}

// Applied to every finished frame: 256-colour fallback and ASCII glyph fallback.
pub fn adapt(buffer: &mut Buffer, caps: Caps) {
    if caps.truecolor && caps.unicode {
        return;
    }
    let down = |color: Color| match color {
        Color::Rgb(r, g, b) => Color::Indexed(nearest_256(r, g, b)),
        other => other,
    };
    for cell in &mut buffer.content {
        if !caps.truecolor {
            cell.fg = down(cell.fg);
            cell.bg = down(cell.bg);
        }
        if !caps.unicode
            && let Some(plain) = ascii_glyph(cell.symbol())
        {
            cell.set_symbol(plain);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClipboardContent {
    Text(String),
    // Base64 PNG.
    Image(String),
}

fn capture(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    (output.status.success() && !output.stdout.is_empty()).then_some(output.stdout)
}

fn base64(bytes: &[u8]) -> String {
    crate::runtime::base64(bytes)
}

const WINDOWS_IMAGE: &str = "Add-Type -AssemblyName System.Windows.Forms; $img = [System.Windows.Forms.Clipboard]::GetImage(); if ($img) { $ms = New-Object System.IO.MemoryStream; $img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); [System.Convert]::ToBase64String($ms.ToArray()) }";

// Port of `clipboard.read`: an image when the clipboard holds one, else text. Blocking.
pub fn clipboard_read() -> Option<ClipboardContent> {
    let os = std::env::consts::OS;
    let text = |bytes: Vec<u8>| Some(ClipboardContent::Text(String::from_utf8_lossy(&bytes).into_owned()));
    match os {
        "macos" => {
            let file = std::env::temp_dir().join("opencode-clipboard.png");
            let open = format!("set fileRef to open for access POSIX file \"{}\" with write permission", file.display());
            let script = ["-e", "set imageData to the clipboard as \"PNGf\"", "-e", &open, "-e", "set eof fileRef to 0", "-e", "write imageData to fileRef", "-e", "close access fileRef"];
            let wrote = Command::new("osascript").args(script).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
            let image = if wrote { std::fs::read(&file).ok() } else { None };
            let _ = std::fs::remove_file(&file);
            match image {
                Some(bytes) => Some(ClipboardContent::Image(base64(&bytes))),
                None => capture("pbpaste", &[]).and_then(text),
            }
        }
        "windows" => {
            if let Some(image) = capture("powershell.exe", &["-NonInteractive", "-NoProfile", "-command", WINDOWS_IMAGE]) {
                let data = String::from_utf8_lossy(&image).trim().to_string();
                if !data.is_empty() {
                    return Some(ClipboardContent::Image(data));
                }
            }
            capture("powershell.exe", &["-NonInteractive", "-NoProfile", "-command", "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; Get-Clipboard -Raw"]).and_then(text)
        }
        _ => {
            if let Some(image) = capture("wl-paste", &["-t", "image/png"]).or_else(|| capture("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"])) {
                return Some(ClipboardContent::Image(base64(&image)));
            }
            capture("wl-paste", &["--no-newline"])
                .or_else(|| capture("xclip", &["-selection", "clipboard", "-o"]))
                .or_else(|| capture("xsel", &["--clipboard", "--output"]))
                .and_then(text)
        }
    }
}

// `copyCommand`
pub fn copy_command(os: &str, wayland: bool, has: &dyn Fn(&str) -> bool) -> Option<Vec<&'static str>> {
    match os {
        "macos" if has("osascript") => Some(vec!["osascript"]),
        "linux" if wayland && has("wl-copy") => Some(vec!["wl-copy"]),
        "linux" if has("xclip") => Some(vec!["xclip", "-selection", "clipboard"]),
        "linux" if has("xsel") => Some(vec!["xsel", "--clipboard", "--input"]),
        "windows" if has("powershell.exe") => {
            Some(vec!["powershell.exe", "-NonInteractive", "-NoProfile", "-Command", "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())"])
        }
        _ => None,
    }
}

fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

// Native clipboard write next to the OSC 52 sequence the caller emits. Blocking.
pub fn clipboard_write(text: &str) {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let Some(command) = copy_command(std::env::consts::OS, wayland, &on_path) else { return };
    if command[0] == "osascript" {
        let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = Command::new("osascript").args(["-e", &format!("set the clipboard to \"{escaped}\"")]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        return;
    }
    let child = Command::new(command[0]).args(&command[1..]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    if let Ok(mut child) = child {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LocalAttachment {
    Svg { filename: String, content: String },
    Binary { filename: String, path: String, mime: &'static str, base64: String },
}

// `pastedFilepath`: quotes stripped, `file://` urls decoded, shell escapes undone outside Windows.
pub fn pasted_filepath(value: &str, windows: bool) -> String {
    let raw = value.trim_matches(['\'', '"']);
    if let Some(rest) = raw.strip_prefix("file://") {
        let source = rest.as_bytes();
        let mut bytes = Vec::with_capacity(source.len());
        let mut at = 0;
        while at < source.len() {
            let escaped = (source[at] == b'%').then(|| rest.get(at + 1..at + 3)).flatten().and_then(|hex| u8::from_str_radix(hex, 16).ok());
            match escaped {
                Some(decoded) => {
                    bytes.push(decoded);
                    at += 3;
                }
                None => {
                    bytes.push(source[at]);
                    at += 1;
                }
            }
        }
        let path = String::from_utf8_lossy(&bytes).into_owned();
        // file:///C:/x -> C:/x
        return if windows { path.trim_start_matches('/').to_string() } else { path };
    }
    if windows {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            other => out.push(other),
        }
    }
    out
}

// `readLocalAttachment`: a pasted path to an image or PDF becomes an attachment, an SVG its source text.
pub fn local_attachment(pasted: &str, windows: bool) -> Option<LocalAttachment> {
    let trimmed = pasted.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let path = pasted_filepath(trimmed, windows);
    if path.starts_with("http://") || path.starts_with("https://") {
        return None;
    }
    let file = Path::new(&path);
    let mime = match file.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "avif" => "image/avif",
        "gif" => "image/gif",
        "jpeg" | "jpg" => "image/jpeg",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        _ => return None,
    };
    let filename = file.file_name()?.to_string_lossy().into_owned();
    if mime == "image/svg+xml" {
        let content = std::fs::read_to_string(file).ok().filter(|text| !text.is_empty())?;
        return Some(LocalAttachment::Svg { filename, content });
    }
    let bytes = std::fs::read(file).ok()?;
    Some(LocalAttachment::Binary { filename, path, mime, base64: base64(&bytes) })
}

// Hands a URL to the desktop's opener without waiting for it.
pub fn open_url(url: &str) -> std::io::Result<()> {
    let mut command = match std::env::consts::OS {
        "windows" => {
            let mut command = Command::new("cmd");
            command.args(["/C", "start", "", url]);
            command
        }
        "macos" => {
            let mut command = Command::new("open");
            command.arg(url);
            command
        }
        _ => {
            let mut command = Command::new("xdg-open");
            command.arg(url);
            command
        }
    };
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|_| ())
}

// `normalizePromptContent`: a single line loses the newline editors append.
pub fn normalize_prompt_content(content: &str) -> &str {
    let body = content.strip_suffix("\r\n").or_else(|| content.strip_suffix('\n'));
    match body {
        Some(body) if !body.contains(['\n', '\r']) => body,
        _ => content,
    }
}

pub fn editor_command(env: Lookup) -> Option<String> {
    env("VISUAL").filter(|v| !v.is_empty()).or_else(|| env("EDITOR").filter(|v| !v.is_empty()))
}

// `openEditor`: writes `value` to a temp file, runs the editor on it and returns the edited text. The caller owns
// suspending and resuming the screen.
pub fn open_editor(editor: &str, value: &str, cwd: &Path, tmp: &Path, now_ms: u64) -> std::io::Result<Option<String>> {
    std::fs::create_dir_all(tmp)?;
    let file = tmp.join(format!("{now_ms}.md"));
    std::fs::write(&file, value)?;
    let mut parts = editor.split(' ').filter(|part| !part.is_empty());
    let program = parts.next().unwrap_or(editor);
    let mut command = if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.arg("/C").arg(program);
        command
    } else {
        Command::new(program)
    };
    command.args(parts).arg(&file);
    if cwd.is_dir() {
        command.current_dir(cwd);
    }
    let status = command.status();
    let content = std::fs::read_to_string(&file);
    let _ = std::fs::remove_file(&file);
    let status = status?;
    if !status.success() {
        return Err(std::io::Error::other(format!("Editor exited with {status}")));
    }
    Ok(content.ok().filter(|text| !text.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    fn replies() -> String {
        let mut out = String::from("\u{1b}]10;rgb:d0d0/d0d0/d0d0\u{7}\u{1b}]11;rgb:1e1e/1e1e/2e2e\u{1b}\\");
        for (index, spec) in ["rgb:00/00/00", "rgb:cd/00/00", "rgb:00/cd/00", "rgb:cd/cd/00", "rgb:00/00/ee", "rgb:cd/00/cd", "rgb:00/cd/cd", "rgb:e5/e5/e5"].iter().enumerate() {
            out.push_str(&format!("\u{1b}]4;{index};{spec}\u{7}"));
        }
        out.push_str("\u{1b}[?62;c");
        out
    }

    #[test]
    fn parses_colour_replies_and_detects_mode() {
        assert!(query().starts_with("\u{1b}]10;?\u{7}\u{1b}]11;?\u{7}\u{1b}]4;0;?\u{7}") && query().ends_with("\u{1b}]4;15;?\u{7}\u{1b}[c"));
        let colors = parse_responses(&replies());
        assert_eq!(colors.background, Some(Rgba::rgb(0x1e, 0x1e, 0x2e)));
        assert_eq!(colors.foreground, Some(Rgba::rgb(0xd0, 0xd0, 0xd0)));
        assert_eq!(colors.palette[1], Some(Rgba::rgb(0xcd, 0, 0)));
        assert_eq!(colors.palette[8], None);
        assert_eq!(mode(&colors), Some(Mode::Dark));
        assert!(answered(&replies()) && !answered("\u{1b}]11;rgb:0/0/0\u{7}"));
        let light = parse_responses("\u{1b}]11;rgb:f/f/f\u{7}");
        assert_eq!((light.background, mode(&light)), (Some(Rgba::rgb(255, 255, 255)), Some(Mode::Light)));
        assert_eq!(mode(&parse_responses("junk")), None);
    }

    #[test]
    fn generates_a_system_theme_that_resolves() {
        let colors = parse_responses(&replies());
        let theme = system_theme(&colors, Mode::Dark).unwrap();
        let resolved = crate::theme::resolve(&theme, Mode::Dark).unwrap();
        assert_eq!(resolved.background.a, 0);
        assert_eq!(resolved.primary, Rgba::rgb(0, 0xcd, 0xcd));
        assert_eq!(resolved.text, Rgba::rgb(0xd0, 0xd0, 0xd0));
        // Bright colours missing from the reply fall back to the ANSI table.
        assert_eq!(resolved.diff_highlight_added, Rgba::rgb(0, 255, 0));
        // luminance 31.8: gray 2 scales the background by (lum + (255 - lum) * 2/12 * 0.4) / lum.
        assert_eq!(resolved.background_panel, Rgba::rgb(44, 44, 67));
        assert_eq!(resolved.text_muted, Rgba::rgb(169, 169, 169));
        assert!(system_theme(&TerminalColors::default(), Mode::Dark).is_none());
        let black = TerminalColors { background: Some(Rgba::rgb(0, 0, 0)), foreground: Some(Rgba::rgb(255, 255, 255)), palette: vec![Some(Rgba::rgb(0, 0, 0)); 16] };
        let resolved = crate::theme::resolve(&system_theme(&black, Mode::Dark).unwrap(), Mode::Dark).unwrap();
        assert_eq!((resolved.background_panel, resolved.text_muted), (Rgba::rgb(17, 17, 17), Rgba::rgb(180, 180, 180)));
    }

    #[test]
    fn detects_capabilities_from_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string());
        assert_eq!(Caps::detect(&env(&[("TERM", "xterm-256color")]), false), Caps { truecolor: false, unicode: true });
        assert_eq!(Caps::detect(&env(&[("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]), false), Caps { truecolor: true, unicode: true });
        assert_eq!(Caps::detect(&env(&[("TERM", "linux")]), false), Caps { truecolor: false, unicode: false });
        assert_eq!(Caps::detect(&env(&[]), true), Caps { truecolor: true, unicode: false });
        assert_eq!(Caps::detect(&env(&[("WT_SESSION", "1")]), true), Caps { truecolor: true, unicode: true });
    }

    #[test]
    fn adapts_frames_for_limited_terminals() {
        assert_eq!(nearest_256(255, 0, 0), 196);
        assert_eq!(nearest_256(0, 0, 0), 16);
        assert_eq!(nearest_256(128, 128, 128), 244);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        buffer.set_string(0, 0, "┃→a", Style::default().fg(Color::Rgb(255, 0, 0)).bg(Color::Reset));
        let mut same = buffer.clone();
        adapt(&mut same, Caps::default());
        assert_eq!(same, buffer);
        adapt(&mut buffer, Caps { truecolor: false, unicode: false });
        assert_eq!(buffer[(0, 0)].symbol(), "|");
        assert_eq!(buffer[(1, 0)].symbol(), ">");
        assert_eq!(buffer[(2, 0)].symbol(), "a");
        assert_eq!((buffer[(0, 0)].fg, buffer[(0, 0)].bg), (Color::Indexed(196), Color::Reset));
    }

    #[test]
    fn pasted_paths_become_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot one.png");
        std::fs::write(&png, b"abc").unwrap();
        std::fs::write(dir.path().join("icon.svg"), "<svg/>").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        let quoted = format!("'{}'", png.display());
        let Some(LocalAttachment::Binary { filename, mime, base64, .. }) = local_attachment(&quoted, cfg!(windows)) else { panic!("no attachment") };
        assert_eq!((filename.as_str(), mime, base64.as_str()), ("shot one.png", "image/png", "YWJj"));
        let svg = local_attachment(&dir.path().join("icon.svg").to_string_lossy(), cfg!(windows));
        assert_eq!(svg, Some(LocalAttachment::Svg { filename: "icon.svg".into(), content: "<svg/>".into() }));
        assert_eq!(local_attachment(&dir.path().join("notes.txt").to_string_lossy(), cfg!(windows)), None);
        assert_eq!(local_attachment(&dir.path().join("missing.png").to_string_lossy(), cfg!(windows)), None);
        assert_eq!(local_attachment("https://example.com/a.png", false), None);
        assert_eq!(local_attachment("two\nlines.png", false), None);
        assert_eq!(pasted_filepath("/tmp/my\\ shot.png", false), "/tmp/my shot.png");
        assert_eq!(pasted_filepath("\"C:\\shots\\a b.png\"", true), "C:\\shots\\a b.png");
        assert_eq!(pasted_filepath("file:///tmp/my%20shot.png", false), "/tmp/my shot.png");
        assert_eq!(pasted_filepath("file:///C:/shots/a.png", true), "C:/shots/a.png");
    }

    #[test]
    fn clipboard_commands_and_editor_round_trip() {
        let all = |_: &str| true;
        assert_eq!(copy_command("linux", true, &all).unwrap(), vec!["wl-copy"]);
        assert_eq!(copy_command("linux", false, &all).unwrap(), vec!["xclip", "-selection", "clipboard"]);
        assert_eq!(copy_command("macos", false, &all).unwrap(), vec!["osascript"]);
        assert_eq!(copy_command("windows", false, &all).unwrap()[0], "powershell.exe");
        assert!(copy_command("linux", false, &|_| false).is_none());
        assert_eq!(normalize_prompt_content("one line\n"), "one line");
        assert_eq!(normalize_prompt_content("one line\r\n"), "one line");
        assert_eq!(normalize_prompt_content("two\nlines\n"), "two\nlines\n");
        assert_eq!(editor_command(&|key| (key == "EDITOR").then(|| "vi".to_string())), Some("vi".into()));
        assert_eq!(editor_command(&|key| Some(if key == "VISUAL" { "code -w" } else { "vi" }.to_string())), Some("code -w".into()));
        assert_eq!(editor_command(&|_| None), None);
    }
}
