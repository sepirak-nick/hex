//! Labs › Talk and click (macOS): talk and point at a page in development.
//!
//! A one-line script served from here (`client.js`, a dev page's
//! `<script src="http://127.0.0.1:47489/client.js">`) asks whether a dictation
//! is recording. While one is, it takes over clicks on the page and reports what
//! was clicked and when. A dictation that collected clicks doesn't paste: its
//! audio goes to ElevenLabs for word timings, each click gets the words nearest
//! it, and each becomes a note in the local Agentation server, where a coding
//! agent picks it up. A dictation without clicks is an ordinary dictation.
//!
//! The listener binds only to loopback and only while the Labs switch is on. It
//! holds no secrets and starts nothing, but any page could reach it, so it
//! refuses other Host headers (DNS rebinding) and other Origins than development
//! ones, and takes clicks only while a dictation records.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use color_eyre::eyre::{Result, bail, eyre};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::elevenlabs::TimedWord;

pub const PORT: u16 = 47_489;
/// Agentation listens on `localhost`, which is IPv6 on some Macs, and another
/// program may hold the same port on the other address.
const AGENTATION_ADDRESSES: [&str; 2] = ["[::1]:4747", "127.0.0.1:4747"];
const CLIENT: &str = include_str!("talk_and_click_client.js");
const MAX_CLICKS: usize = 200;
const MAX_HEAD_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_FIELD_CHARS: usize = 2_000;
const POLL: Duration = Duration::from_millis(50);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(2);
const AGENTATION_TIMEOUT: Duration = Duration::from_secs(5);
/// A click just before the audio starts still belongs to this recording.
const CLICK_LEAD_MS: u64 = 1_000;
const NO_WORDS: &str = "(Pointed at this without saying anything.)";
/// A silence this long ends a sentence Scribe didn't punctuate.
const SENTENCE_PAUSE_MS: u64 = 1_500;
/// A silence this long divides a sentence with several clicks in it.
const CLAUSE_PAUSE_MS: u64 = 400;

static ENABLED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static CLICKS: Mutex<Vec<Click>> = Mutex::new(Vec::new());

/// One element the page reported clicked. Field names are the page's.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Click {
    pub element: String,
    pub element_path: String,
    pub url: String,
    pub timestamp: u64,
    pub x: f64,
    pub y: f64,
    /// The Agentation toolbar's session on that page, so notes appear there.
    #[serde(skip_serializing)]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub css_classes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nearby_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub react_components: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounding_box: Option<BoundingBox>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct BoundingBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Follows the Labs switch; call it from the listener's loop. Binds lazily and
/// releases the port while off.
pub fn sync(enabled: bool) {
    ENABLED.store(enabled, Ordering::Release);
    if enabled && !STARTED.swap(true, Ordering::AcqRel) {
        let started = thread::Builder::new()
            .name("talk-and-click".into())
            .spawn(serve_while_enabled);
        if let Err(error) = started {
            tracing::error!(%error, "could not start talk and click");
            STARTED.store(false, Ordering::Release);
        }
    }
}

fn serve_while_enabled() {
    let mut listener = None;
    let mut warned = false;
    while !crate::process_exit::quitting() {
        if !ENABLED.load(Ordering::Acquire) {
            listener = None;
            clicks().clear();
            thread::sleep(POLL * 4);
            continue;
        }
        if listener.is_none() {
            match bind() {
                Ok(bound) => {
                    tracing::info!(port = PORT, "talk and click is listening");
                    listener = Some(bound);
                    warned = false;
                }
                Err(error) => {
                    if !warned {
                        tracing::warn!(%error, port = PORT, "talk and click could not listen");
                        warned = true;
                    }
                    thread::sleep(POLL * 20);
                    continue;
                }
            }
        }
        match listener.as_ref().expect("bound above").accept() {
            Ok((stream, _)) => {
                if let Err(error) = respond(stream, crate::recording_environment::capture_active())
                {
                    tracing::debug!(%error, "talk and click request failed");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(POLL),
            Err(error) => {
                tracing::debug!(%error, "talk and click accept failed");
                thread::sleep(POLL);
            }
        }
    }
}

fn bind() -> std::io::Result<TcpListener> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], PORT)))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn clicks() -> std::sync::MutexGuard<'static, Vec<Click>> {
    CLICKS.lock().unwrap_or_else(|error| error.into_inner())
}

fn respond(mut stream: TcpStream, recording: bool) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECTION_TIMEOUT))?;
    let response = match read_request(&mut stream) {
        Some(request) => route(&request, recording),
        None => Response::status(400),
    };
    stream.write_all(&response.bytes())
}

struct Request {
    method: String,
    path: String,
    host: Option<String>,
    origin: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut impl Read) -> Option<Request> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let head_end = loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        if bytes.len() > MAX_HEAD_BYTES {
            return None;
        }
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..read]);
    };
    let head = std::str::from_utf8(&bytes[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_owned();
    let path = request_line.next()?.to_owned();
    let header = |name: &str| {
        head.split("\r\n").skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    };
    let length: usize = header("content-length").map_or(Some(0), |value| value.parse().ok())?;
    if length > MAX_BODY_BYTES {
        return None;
    }
    let mut body = bytes[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        body.extend_from_slice(&buffer[..read]);
    }
    body.truncate(length);
    Some(Request {
        method,
        path,
        host: header("host"),
        origin: header("origin"),
        body,
    })
}

struct Response {
    status: u16,
    content_type: &'static str,
    origin: Option<String>,
    body: Vec<u8>,
}

impl Response {
    fn status(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain",
            origin: None,
            body: Vec::new(),
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let reason = match self.status {
            200 => "OK",
            204 => "No Content",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            409 => "Conflict",
            _ => "Error",
        };
        let mut head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nCross-Origin-Resource-Policy: cross-origin\r\n",
            self.status,
            self.content_type,
            self.body.len()
        );
        if let Some(origin) = &self.origin {
            head.push_str(&format!(
                "Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"
            ));
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

fn route(request: &Request, recording: bool) -> Response {
    if !request.host.as_deref().is_some_and(allowed_host) {
        return Response::status(403);
    }
    if request.method == "GET" && request.path == "/client.js" {
        return Response {
            status: 200,
            content_type: "text/javascript",
            origin: None,
            body: CLIENT.as_bytes().to_vec(),
        };
    }
    let Some(origin) = request.origin.as_deref().filter(|o| allowed_origin(o)) else {
        return Response::status(403);
    };
    let mut response = match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/state") => Response {
            status: 200,
            content_type: "application/json",
            origin: None,
            body: json!({ "recording": recording }).to_string().into_bytes(),
        },
        // Sent as text/plain, so a browser needs no preflight.
        ("POST", "/click") if !recording => Response::status(409),
        ("POST", "/click") => match serde_json::from_slice::<Click>(&request.body) {
            Ok(click) if !click.element.is_empty() && !click.element_path.is_empty() => {
                let mut clicks = clicks();
                if clicks.len() < MAX_CLICKS {
                    clicks.push(bounded(click));
                }
                Response::status(204)
            }
            _ => Response::status(400),
        },
        _ => Response::status(404),
    };
    response.origin = Some(origin.to_owned());
    response
}

fn allowed_host(host: &str) -> bool {
    host == format!("127.0.0.1:{PORT}") || host == format!("localhost:{PORT}")
}

/// A page on this Mac in development, as Agentation's own default allows,
/// without its private-network addresses: this one takes no remote pages.
fn allowed_origin(origin: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    if rest.contains('/') || rest.contains('@') {
        return false;
    }
    // An IPv6 host is bracketed, so its colons aren't a port's.
    let (host, port) = match rest.strip_prefix('[') {
        Some(after) => match after.split_once(']') {
            Some((address, port)) => (&rest[..address.len() + 2], port.strip_prefix(':')),
            None => return false,
        },
        None => match rest.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        },
    };
    if port.is_some_and(|port| port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit())) {
        return false;
    }
    let host = host.to_ascii_lowercase();
    host == "localhost"
        || host == "127.0.0.1"
        || host == "[::1]"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".test")
}

/// Holds what a page sent to a size a note can sensibly carry.
fn bounded(mut click: Click) -> Click {
    let clamp = |text: &mut String| {
        if text.chars().count() > MAX_FIELD_CHARS {
            *text = text.chars().take(MAX_FIELD_CHARS).collect();
        }
    };
    clamp(&mut click.element);
    clamp(&mut click.element_path);
    clamp(&mut click.url);
    for field in [
        &mut click.css_classes,
        &mut click.nearby_text,
        &mut click.react_components,
        &mut click.session_id,
    ]
    .into_iter()
    .flatten()
    {
        clamp(field);
    }
    click
}

/// Takes this recording's clicks, in time order, leaving none behind. Clicks
/// from a cancelled recording are older than `start_ms`, so they go too.
pub fn take_clicks(start_ms: u64) -> Vec<Click> {
    let mut taken: Vec<Click> = std::mem::take(&mut *clicks())
        .into_iter()
        .filter(|click| click.timestamp + CLICK_LEAD_MS >= start_ms)
        .collect();
    taken.sort_by_key(|click| click.timestamp);
    taken
}

/// Divides what was said between the clicks, keeping sentences whole. A
/// sentence goes to the click made while saying it, else to the nearest click,
/// so both "this button *click*" and "*click* this button" work. Only a sentence
/// with several clicks in it is divided: at its commas and pauses, then, for a
/// clause that still has several, word by word.
fn split_words(words: &[TimedWord], clicks: &[Click], start_ms: u64) -> Vec<String> {
    let mut notes = vec![Vec::new(); clicks.len()];
    if !clicks.is_empty() {
        for sentence in runs(words, ends_sentence, SENTENCE_PAUSE_MS) {
            assign_run(sentence, clicks, start_ms, Level::Sentence, &mut notes);
        }
    }
    notes
        .into_iter()
        .map(|words| {
            if words.is_empty() {
                NO_WORDS.to_owned()
            } else {
                words.join(" ")
            }
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Level {
    Sentence,
    Clause,
}

fn assign_run<'a>(
    run: &'a [TimedWord],
    clicks: &[Click],
    start_ms: u64,
    level: Level,
    notes: &mut [Vec<&'a str>],
) {
    let (Some(first), Some(last)) = (run.first(), run.last()) else {
        return;
    };
    let (from, to) = (start_ms + first.start_ms, start_ms + last.end_ms);
    let inside = clicks
        .iter()
        .filter(|click| (from..=to).contains(&click.timestamp))
        .count();
    if inside < 2 {
        // The nearest click is the one inside when there is one. On a tie the
        // earlier wins: people usually click first and keep talking.
        let distance = |click: &Click| {
            from.saturating_sub(click.timestamp)
                .max(click.timestamp.saturating_sub(to))
        };
        if let Some((nearest, _)) = clicks
            .iter()
            .enumerate()
            .min_by_key(|(_, click)| distance(click))
        {
            notes[nearest].extend(run.iter().map(|word| word.text.as_str()));
        }
        return;
    }
    match level {
        Level::Sentence => {
            for clause in runs(run, ends_clause, CLAUSE_PAUSE_MS) {
                assign_run(clause, clicks, start_ms, Level::Clause, notes);
            }
        }
        Level::Clause => {
            for word in run {
                let middle = start_ms + (word.start_ms + word.end_ms) / 2;
                if let Some((nearest, _)) = clicks
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, click)| click.timestamp.abs_diff(middle))
                {
                    notes[nearest].push(word.text.as_str());
                }
            }
        }
    }
}

/// Cuts `words` after each word `ends` accepts, and at each silence of `pause_ms`
/// or more, since speech isn't always punctuated.
fn runs(words: &[TimedWord], ends: fn(&str) -> bool, pause_ms: u64) -> Vec<&[TimedWord]> {
    let mut runs = Vec::new();
    let mut start = 0;
    for (index, pair) in words.windows(2).enumerate() {
        if ends(&pair[0].text) || pair[1].start_ms.saturating_sub(pair[0].end_ms) >= pause_ms {
            runs.push(&words[start..=index]);
            start = index + 1;
        }
    }
    if start < words.len() {
        runs.push(&words[start..]);
    }
    runs
}

fn ends_sentence(word: &str) -> bool {
    word.trim_end_matches(['"', '\'', ')', '”', '’'])
        .ends_with(['.', '!', '?', '…'])
}

fn ends_clause(word: &str) -> bool {
    ends_sentence(word) || word.ends_with([',', ';', ':'])
}

/// Sends a finished recording: checks Agentation before spending an upload,
/// gets word timings, and posts one note per click. The answer is what a
/// dictation would have pasted, for the log; nothing is pasted.
pub(crate) fn deliver(
    clicks: &[Click],
    samples: &[f32],
    start_ms: u64,
    language: &str,
    cancelled: &AtomicBool,
) -> Result<String> {
    let Some(agentation) = find_agentation(&AGENTATION_ADDRESSES) else {
        bail!("Agentation's server isn't running. Start it with: npx agentation-mcp server");
    };
    let words = crate::elevenlabs::transcribe_words(samples, language, cancelled)?;
    let notes = split_words(&words, clicks, start_ms);
    post_notes(agentation, clicks, &notes)?;
    Ok(format!("Sent {} notes to Agentation", notes.len()))
}

/// The first address answering as Agentation does, so notes never go to
/// whatever else happens to hold the port.
fn find_agentation<'a>(addresses: &[&'a str]) -> Option<&'a str> {
    addresses.iter().copied().find(|address| {
        request_at(address, "GET", "/health", None)
            .is_ok_and(|health| health["status"] == "ok" && health["mode"].is_string())
    })
}

fn post_notes(agentation: &str, clicks: &[Click], notes: &[String]) -> Result<()> {
    let mut sessions: Vec<(String, String)> = Vec::new();
    for (click, comment) in clicks.iter().zip(notes) {
        let session = match sessions.iter().find(|(url, _)| *url == click.url) {
            Some((_, session)) => session.clone(),
            None => {
                let session = session_for(agentation, click)?;
                sessions.push((click.url.clone(), session.clone()));
                session
            }
        };
        let mut note = serde_json::to_value(click)?;
        note["comment"] = json!(comment);
        note["kind"] = json!("feedback");
        request_at(
            agentation,
            "POST",
            &format!("/sessions/{session}/annotations"),
            Some(&note),
        )?;
    }
    Ok(())
}

/// The toolbar's session on that page when it still exists, else a new one.
fn session_for(agentation: &str, click: &Click) -> Result<String> {
    if let Some(session) = &click.session_id
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        && request_at(agentation, "GET", &format!("/sessions/{session}"), None).is_ok()
    {
        return Ok(session.clone());
    }
    let created = request_at(
        agentation,
        "POST",
        "/sessions",
        Some(&json!({ "url": click.url })),
    )?;
    created["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| eyre!("Agentation sent no session"))
}

/// One request to a loopback server, over a connection it closes. HTTP/1.0, so
/// the answer isn't chunked, as Agentation's otherwise is.
fn request_at(
    address: &str,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value> {
    let socket: SocketAddr = address.parse()?;
    let mut stream = TcpStream::connect_timeout(&socket, AGENTATION_TIMEOUT)?;
    stream.set_read_timeout(Some(AGENTATION_TIMEOUT))?;
    stream.set_write_timeout(Some(AGENTATION_TIMEOUT))?;
    let body = body.map(ToString::to_string).unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.0\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| eyre!("Agentation sent an unexpected answer"))?;
    let status: u16 = head
        .split(' ')
        .nth(1)
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| eyre!("Agentation sent no status"))?;
    if !(200..300).contains(&status) {
        bail!("Agentation answered {status} to {method} {path}");
    }
    Ok(serde_json::from_str(body).unwrap_or(serde_json::Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, path: &str, origin: Option<&str>, body: &[u8]) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            host: Some(format!("127.0.0.1:{PORT}")),
            origin: origin.map(str::to_owned),
            body: body.to_vec(),
        }
    }

    fn click(timestamp: u64, path: &str) -> Click {
        Click {
            element: "button".into(),
            element_path: path.into(),
            url: "http://localhost:5199/".into(),
            timestamp,
            ..Click::default()
        }
    }

    fn word(text: &str, start_ms: u64, end_ms: u64) -> TimedWord {
        TimedWord {
            text: text.into(),
            start_ms,
            end_ms,
        }
    }

    const DEV: &str = "http://localhost:5199";

    #[test]
    fn each_click_gets_the_words_nearest_it() {
        let words = [
            word("this", 0, 200),
            word("button", 200, 500),
            word("is", 2_000, 2_100),
            word("blue", 2_100, 2_400),
        ];
        let clicks = [click(10_400, ".a"), click(12_000, ".b")];
        assert_eq!(
            split_words(&words, &clicks, 10_000),
            ["this button", "is blue"]
        );
    }

    #[test]
    fn a_click_before_or_after_its_words_still_gets_them() {
        let words = [word("before", 900, 1_000), word("after", 5_100, 5_200)];
        let clicks = [click(1_500, ".before"), click(4_000, ".after")];
        assert_eq!(split_words(&words, &clicks, 0), ["before", "after"]);
    }

    /// Words spoken one after another from `start_ms`, at 300 ms each.
    fn spoken(start_ms: u64, sentence: &str) -> Vec<TimedWord> {
        sentence
            .split_whitespace()
            .enumerate()
            .map(|(index, text)| {
                let at = start_ms + index as u64 * 300;
                word(text, at, at + 280)
            })
            .collect()
    }

    /// The recording that cut "Talk and Fix Demo" in half. Its real clicks were
    /// 5.3 s apart, with the midpoint between "Talk" and "and". Timings around
    /// those are reconstructed, since Hex keeps no audio.
    #[test]
    fn a_sentence_running_past_the_midpoint_between_clicks_stays_whole() {
        let mut words = spoken(0, "Testing dictation and being able to click around.");
        words.extend(spoken(
            2_500,
            "Uh, change this header from Talk and Click Demo to Talk and Fix Demo.",
        ));
        words.extend(spoken(7_000, "Change this price to $5"));
        let clicks = [click(3_300, "h1"), click(8_620, "p")];
        // Word by word, the midpoint at 5.96 s fell inside the header sentence.
        assert_eq!(
            split_words(&words, &clicks, 0),
            [
                "Testing dictation and being able to click around. Uh, change this header \
                 from Talk and Click Demo to Talk and Fix Demo.",
                "Change this price to $5",
            ]
        );
    }

    #[test]
    fn a_click_at_the_end_of_its_sentence_gets_all_of_it() {
        let mut words = spoken(0, "This price should be five.");
        words.extend(spoken(1_600, "This heading should be blue."));
        let clicks = [click(1_400, ".price"), click(3_000, ".heading")];
        assert_eq!(
            split_words(&words, &clicks, 0),
            ["This price should be five.", "This heading should be blue."]
        );
    }

    #[test]
    fn a_sentence_with_no_click_in_it_goes_to_the_nearest() {
        let mut words = spoken(0, "First thing here.");
        words.extend(spoken(3_000, "More about the first thing."));
        words.extend(spoken(10_000, "Second thing here."));
        let clicks = [click(500, ".first"), click(10_400, ".second")];
        assert_eq!(
            split_words(&words, &clicks, 0),
            [
                "First thing here. More about the first thing.",
                "Second thing here."
            ]
        );
    }

    #[test]
    fn clicks_inside_one_sentence_divide_it_at_its_comma() {
        let words = spoken(0, "This button is too big, and this heading is too small.");
        let clicks = [click(600, ".button"), click(2_100, ".heading")];
        assert_eq!(
            split_words(&words, &clicks, 0),
            ["This button is too big,", "and this heading is too small."]
        );
    }

    #[test]
    fn clicks_inside_one_unbroken_clause_divide_it_by_word() {
        let words = spoken(0, "this button and this heading");
        let clicks = [click(150, ".button"), click(1_350, ".heading")];
        assert_eq!(
            split_words(&words, &clicks, 0),
            ["this button and", "this heading"]
        );
    }

    #[test]
    fn a_click_during_a_sentence_doesnt_take_the_next_one() {
        let mut words = spoken(0, "Make this larger.");
        words.extend(spoken(900, "Also make that smaller."));
        let clicks = [click(400, ".this"), click(1_500, ".that")];
        assert_eq!(
            split_words(&words, &clicks, 0),
            ["Make this larger.", "Also make that smaller."]
        );
    }

    #[test]
    fn closing_punctuation_still_ends_a_sentence() {
        assert!(ends_sentence("done.\""));
        assert!(ends_sentence("really?)"));
        assert!(!ends_sentence("Dr"));
        assert!(!ends_sentence("big,"));
        assert!(ends_clause("big,"));
        assert!(ends_clause("done."));
    }

    #[test]
    fn a_silent_click_says_so_rather_than_being_lost() {
        let notes = split_words(
            &[word("hello", 0, 100)],
            &[click(0, ".a"), click(60_000, ".b")],
            0,
        );
        assert_eq!(notes, ["hello", NO_WORDS]);
    }

    #[test]
    fn only_a_development_page_is_answered() {
        for origin in [
            DEV,
            "http://127.0.0.1:3000",
            "https://app.localhost",
            "http://shop.test:8080",
            "http://mac.local",
            "http://[::1]:5173",
        ] {
            assert!(allowed_origin(origin), "{origin}");
        }
        for origin in [
            "https://example.com",
            "http://localhost.example.com",
            "http://evil.com/localhost",
            "http://localhost@evil.com",
            "http://192.168.1.5:3000",
            "http://localhost:evil",
            "http://[::1",
            "null",
            "file://",
            "",
        ] {
            assert!(!allowed_origin(origin), "{origin}");
        }
        assert_eq!(
            route(&request("GET", "/state", None, b""), true).status,
            403
        );
        assert_eq!(
            route(
                &request("GET", "/state", Some("https://example.com"), b""),
                true
            )
            .status,
            403
        );
        let state = route(&request("GET", "/state", Some(DEV), b""), false);
        assert_eq!(state.status, 200);
        assert_eq!(state.origin.as_deref(), Some(DEV));
        assert_eq!(state.body, br#"{"recording":false}"#);
    }

    #[test]
    fn another_host_header_is_refused_against_dns_rebinding() {
        let mut rebound = request("GET", "/client.js", None, b"");
        rebound.host = Some(format!("evil.example:{PORT}"));
        assert_eq!(route(&rebound, true).status, 403);
        let mut missing = request("GET", "/client.js", None, b"");
        missing.host = None;
        assert_eq!(route(&missing, true).status, 403);
    }

    #[test]
    fn the_script_is_served_to_any_page_on_a_loopback_host() {
        let script = route(&request("GET", "/client.js", None, b""), false);
        assert_eq!(
            (script.status, script.content_type),
            (200, "text/javascript")
        );
        assert!(script.body.starts_with(b"//"));
    }

    #[test]
    fn clicks_are_refused_unless_a_dictation_records() {
        let body = serde_json::to_vec(&click(1, "body > button.only-when-idle")).unwrap();
        let idle = route(&request("POST", "/click", Some(DEV), &body), false);
        assert_eq!(idle.status, 409);
        assert!(
            clicks()
                .iter()
                .all(|click| click.element_path != "body > button.only-when-idle")
        );
    }

    #[test]
    fn a_click_needs_an_element_and_a_malformed_one_is_refused() {
        for body in [&b"not json"[..], br#"{"element":"","elementPath":""}"#] {
            let response = route(&request("POST", "/click", Some(DEV), body), true);
            assert_eq!(response.status, 400);
        }
    }

    #[test]
    fn page_fields_are_held_to_a_size() {
        let long = "x".repeat(MAX_FIELD_CHARS * 3);
        let held = bounded(Click {
            element_path: long.clone(),
            nearby_text: Some(long),
            ..click(1, "a")
        });
        assert_eq!(held.element_path.chars().count(), MAX_FIELD_CHARS);
        assert_eq!(held.nearby_text.unwrap().chars().count(), MAX_FIELD_CHARS);
    }

    #[test]
    fn an_oversized_or_cut_off_request_is_refused() {
        let oversized = format!(
            "POST /click HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert!(read_request(&mut oversized.as_bytes()).is_none());
        assert!(read_request(&mut b"GET / HTTP/1.1\r\nHost: x".as_slice()).is_none());
        let cut = "POST /click HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc";
        assert!(read_request(&mut cut.as_bytes()).is_none());
        let huge_head = format!("GET / HTTP/1.1\r\nX: {}", "y".repeat(MAX_HEAD_BYTES * 2));
        assert!(read_request(&mut huge_head.as_bytes()).is_none());
        let whole = "POST /click HTTP/1.1\r\nHOST: h\r\nOrigin: o\r\nContent-Length: 3\r\n\r\nabc";
        let request = read_request(&mut whole.as_bytes()).unwrap();
        assert_eq!(request.host.as_deref(), Some("h"));
        assert_eq!(request.body, b"abc");
    }

    #[test]
    fn the_session_id_never_reaches_a_note_and_page_field_names_are_kept() {
        let parsed: Click = serde_json::from_str(
            r#"{"element":"button","elementPath":".a","sessionId":"s1","cssClasses":"a b",
                "reactComponents":"App > Card","boundingBox":{"x":1,"y":2,"width":3,"height":4}}"#,
        )
        .unwrap();
        assert_eq!(parsed.session_id.as_deref(), Some("s1"));
        let note = serde_json::to_value(&parsed).unwrap();
        assert!(note.get("sessionId").is_none());
        assert_eq!(note["elementPath"], ".a");
        assert_eq!(note["cssClasses"], "a b");
        assert_eq!(note["reactComponents"], "App > Card");
        assert_eq!(note["boundingBox"]["width"], 3.0);
    }

    /// A fake server that answers each request by path, and records them.
    fn fake_server(
        routes: &'static [(&'static str, u16, &'static str)],
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (send, received) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let request = read_request(&mut stream).unwrap();
                let line = format!("{} {}", request.method, request.path);
                let (status, body) = routes
                    .iter()
                    .find(|(route, ..)| *route == line)
                    .map_or((404, "Not found"), |(_, status, body)| (*status, *body));
                let _ = send.send(format!("{line} {}", String::from_utf8_lossy(&request.body)));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (address, received)
    }

    const HEALTHY: &str = r#"{"status":"ok","mode":"local"}"#;

    #[test]
    fn another_program_on_the_port_isnt_mistaken_for_agentation() {
        let (not_found, _) = fake_server(&[]);
        let (other_json, _) = fake_server(&[("GET /health", 200, r#"{"status":"ok"}"#)]);
        let (agentation, _) = fake_server(&[("GET /health", 200, HEALTHY)]);
        let closed = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().to_string()
        };
        assert_eq!(
            find_agentation(&[
                closed.as_str(),
                not_found.as_str(),
                other_json.as_str(),
                agentation.as_str()
            ]),
            Some(agentation.as_str())
        );
        assert_eq!(
            find_agentation(&[closed.as_str(), not_found.as_str()]),
            None
        );
    }

    #[test]
    fn notes_join_the_toolbars_session_and_carry_the_words() {
        let (address, received) = fake_server(&[
            ("GET /sessions/toolbar-1", 200, "{}"),
            ("POST /sessions/toolbar-1/annotations", 201, "{}"),
        ]);
        let clicks = [
            Click {
                session_id: Some("toolbar-1".into()),
                ..click(1, ".a")
            },
            Click {
                session_id: Some("toolbar-1".into()),
                ..click(2, ".b")
            },
        ];
        post_notes(
            &address,
            &clicks,
            &["first".to_owned(), "second".to_owned()],
        )
        .unwrap();
        let sent: Vec<String> = received.try_iter().collect();
        assert_eq!(sent.len(), 3, "the session is checked once: {sent:?}");
        assert!(sent[1].contains(r#""comment":"first""#), "{sent:?}");
        assert!(sent[1].contains(r#""elementPath":".a""#), "{sent:?}");
        assert!(sent[2].contains(r#""comment":"second""#), "{sent:?}");
        assert!(
            sent.iter().all(|line| !line.contains("sessionId")),
            "{sent:?}"
        );
    }

    #[test]
    fn a_stale_or_strange_session_gets_a_new_one_and_failures_surface() {
        let (address, received) = fake_server(&[
            ("POST /sessions", 201, r#"{"id":"new-1"}"#),
            ("POST /sessions/new-1/annotations", 201, "{}"),
        ]);
        let stale = Click {
            session_id: Some("gone".into()),
            ..click(1, ".a")
        };
        post_notes(&address, &[stale], &["words".to_owned()]).unwrap();
        let strange = Click {
            session_id: Some("../../evil".into()),
            ..click(1, ".a")
        };
        post_notes(&address, &[strange], &["words".to_owned()]).unwrap();
        let sent: Vec<String> = received.try_iter().collect();
        assert!(sent.iter().all(|line| !line.contains("evil")), "{sent:?}");
        assert!(
            sent.iter()
                .any(|line| line.starts_with("POST /sessions/new-1/annotations"))
        );
        let (refusing, _) = fake_server(&[("POST /sessions", 500, "{}")]);
        assert!(post_notes(&refusing, &[click(1, ".a")], &["words".to_owned()]).is_err());
    }

    /// The whole path against real parts: a real browser clicks the demo page
    /// (`prototypes/talk-and-click-demo`) while this holds a capture open, then
    /// `HEX_LIVE_WAV` goes to ElevenLabs and notes to a running Agentation server.
    /// The recording is a file rather than a microphone.
    #[test]
    #[ignore = "uploads to ElevenLabs; needs Agentation, a key, HEX_LIVE_WAV, and a browser clicking"]
    fn live_a_real_page_click_becomes_an_agentation_note() {
        let path = std::env::var("HEX_LIVE_WAV").expect("HEX_LIVE_WAV");
        let samples: Vec<f32> = hound::WavReader::open(path)
            .unwrap()
            .samples::<i16>()
            .map(|sample| f32::from(sample.unwrap()) / f32::from(i16::MAX))
            .collect();
        sync(true);
        let capture = crate::recording_environment::CaptureMarker::new();
        eprintln!("LIVE recording; click twice");
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        while clicks().len() < 2 {
            assert!(std::time::Instant::now() < deadline, "no clicks arrived");
            thread::sleep(POLL);
        }
        drop(capture);
        thread::sleep(Duration::from_millis(400));
        sync(false);
        let mut taken = take_clicks(0);
        assert_eq!(taken.len(), 2);
        eprintln!(
            "LIVE clicks {:?}",
            taken.iter().map(|c| &c.element_path).collect::<Vec<_>>()
        );
        eprintln!(
            "LIVE details {:?} | {:?}",
            taken[0].react_components, taken[0].session_id
        );
        // A browser driven by a script can't click in time with a file, so the
        // two clicks are placed where the file's two sentences are.
        let audio_ms = samples.len() as u64 / 16;
        let start_ms = crate::events::now_ms();
        taken[0].timestamp = start_ms + audio_ms / 4;
        taken[1].timestamp = start_ms + audio_ms * 3 / 4;
        let answer = deliver(&taken, &samples, start_ms, "auto", &AtomicBool::new(false)).unwrap();
        eprintln!("LIVE {answer}");
    }
}
