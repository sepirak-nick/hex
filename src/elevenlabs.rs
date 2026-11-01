//! ElevenLabs Scribe, Hex's one cloud transcription model (Labs, macOS).
//!
//! The upload runs through curl, as model downloads do: it adds no dependency
//! and killing the child ends an upload at once, which is what Escape needs.
//! The key reaches curl through its environment and the audio through stdin,
//! so neither shows in `ps` nor touches the disk. Only dictation calls
//! [`ElevenLabs::transcribe`]; see `Transcriber::transcribe_dictation`. The one
//! other upload is [`diarize`], Labs › Meeting speaker labels' system track,
//! which is sent from its temporary file because a meeting is long.

use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, bail, eyre};

use crate::transcription_models::{AUTO_LANGUAGE, ModelRuntime, TranscriptionSelection, validate};

/// Takes precedence over the key file, mainly for development: apps opened
/// from the Dock don't inherit a shell's environment.
pub const KEY_ENVIRONMENT: &str = "ELEVENLABS_API_KEY";
const KEY_FILE: &str = "elevenlabs-api-key";
const ENDPOINT: &str = "https://api.elevenlabs.io/v1/speech-to-text";
/// macOS's own curl: `PATH` never decides which program receives the key.
const CURL: &str = if cfg!(target_os = "macos") {
    "/usr/bin/curl"
} else {
    "curl"
};
/// Curl reads the key from this variable of its own environment.
const CURL_KEY_VARIABLE: &str = "HEX_XI_KEY";
const SAMPLE_RATE: u32 = 16_000;
const CONNECT_TIMEOUT_SECONDS: u64 = 5;
const BASE_TIMEOUT_SECONDS: u64 = 20;
/// Far larger than any transcript a dictation produces; curl and the reader
/// both hold a response to it.
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 4 * 1024;
const POLL: Duration = Duration::from_millis(25);

/// Why a dictation didn't come back as text.
#[derive(Debug, Eq, PartialEq)]
pub enum UploadError {
    /// Escape, a job deadline, or quitting. The audio already sent can't be
    /// recalled.
    Cancelled,
    /// ElevenLabs may answer later: no connection, a timeout, 429, or 5xx.
    Unreachable(String),
    /// Trying again won't help: a bad key or refused audio.
    Rejected(String),
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("dictation was cancelled"),
            Self::Unreachable(message) | Self::Rejected(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for UploadError {}

/// Where the API key in use comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeySource {
    Environment,
    File,
    Missing,
}

/// A selected ElevenLabs model. Loading it only checks that a key exists; it
/// makes no network call.
pub struct ElevenLabs {
    selection: TranscriptionSelection,
    model_id: &'static str,
}

impl ElevenLabs {
    pub fn load(selection: &TranscriptionSelection) -> Result<Self> {
        let ModelRuntime::ElevenLabs { model_id } = validate(selection)?.runtime else {
            bail!("{} is not an ElevenLabs model", selection.model.as_str());
        };
        if key_source() == KeySource::Missing {
            bail!("Add an ElevenLabs API key to use this model");
        }
        Ok(Self {
            selection: selection.clone(),
            model_id,
        })
    }

    /// The model without its key check, which would read real files.
    #[cfg(test)]
    pub(crate) fn for_test(selection: &TranscriptionSelection) -> Self {
        Self {
            selection: selection.clone(),
            model_id: "scribe_v2",
        }
    }

    pub fn matches_selection(&self, selection: &TranscriptionSelection) -> bool {
        &self.selection == selection
    }

    /// Uploads normalized 16 kHz audio and returns the transcript. The key is
    /// read for every upload, so removing it takes effect while this model
    /// stays loaded.
    pub fn transcribe(
        &self,
        samples: &[f32],
        cancelled: &AtomicBool,
    ) -> std::result::Result<String, UploadError> {
        if !crate::labs::elevenlabs() {
            return Err(UploadError::Rejected(
                "ElevenLabs transcription is turned off in Labs".into(),
            ));
        }
        let key = read_key()
            .map_err(|error| UploadError::Rejected(error.to_string()))?
            .ok_or_else(|| {
                UploadError::Rejected("Add an ElevenLabs API key to use this model".into())
            })?;
        let audio = encode_wav(samples)
            .map_err(|error| UploadError::Rejected(format!("could not encode audio: {error}")))?;
        let started = Instant::now();
        let request = Request {
            url: ENDPOINT,
            protocols: "=https",
            model_id: self.model_id,
            language: &self.selection.language,
            timeout_seconds: timeout_seconds(samples.len()),
            file: None,
            diarize: false,
            words: false,
        };
        let stop = || {
            cancelled.load(Ordering::Acquire)
                || crate::gguf_session::job_cancel().is_some_and(|cancel| cancel.is_cancelled())
                || crate::process_exit::quitting()
        };
        let result = post(&request, &key, audio, stop).and_then(|body| parse_transcript(&body));
        tracing::info!(
            upload_ms = started.elapsed().as_millis() as u64,
            audio_ms = samples.len() as u64 / 16,
            ok = result.is_ok(),
            "elevenlabs upload"
        );
        result
    }
}

/// One transcribed word, in ms from the clip's start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimedWord {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Uploads a dictation for its words and when each was said, which Labs › Talk
/// and click uses to match words to clicks.
pub fn transcribe_words(
    samples: &[f32],
    language: &str,
    cancelled: &AtomicBool,
) -> std::result::Result<Vec<TimedWord>, UploadError> {
    let key = read_key()
        .map_err(|error| UploadError::Rejected(error.to_string()))?
        .ok_or_else(|| {
            UploadError::Rejected("Add an ElevenLabs API key to use talk and click".into())
        })?;
    let audio = encode_wav(samples)
        .map_err(|error| UploadError::Rejected(format!("could not encode audio: {error}")))?;
    let request = Request {
        url: ENDPOINT,
        protocols: "=https",
        model_id: "scribe_v2",
        language,
        timeout_seconds: timeout_seconds(samples.len()),
        file: None,
        diarize: false,
        words: true,
    };
    let stop = || {
        cancelled.load(Ordering::Acquire)
            || crate::gguf_session::job_cancel().is_some_and(|cancel| cancel.is_cancelled())
            || crate::process_exit::quitting()
    };
    post(&request, &key, audio, stop).and_then(|body| parse_words(&body))
}

/// Who spoke one stretch of a track, in ms from its start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpeakerSpan {
    pub speaker: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Uploads a 16 kHz track of `samples` samples and returns who spoke when.
pub fn diarize(
    track: &Path,
    language: &str,
    samples: usize,
    stop: impl Fn() -> bool,
) -> std::result::Result<Vec<SpeakerSpan>, UploadError> {
    let key = read_key()
        .map_err(|error| UploadError::Rejected(error.to_string()))?
        .ok_or_else(|| {
            UploadError::Rejected("Add an ElevenLabs API key to label meeting speakers".into())
        })?;
    let started = Instant::now();
    let request = Request {
        url: ENDPOINT,
        protocols: "=https",
        model_id: "scribe_v2",
        language,
        timeout_seconds: diarize_timeout_seconds(samples),
        file: Some(track),
        diarize: true,
        words: true,
    };
    let result = post(&request, &key, Vec::new(), stop).and_then(|body| parse_spans(&body));
    tracing::info!(
        upload_ms = started.elapsed().as_millis() as u64,
        audio_ms = samples as u64 / 16,
        ok = result.is_ok(),
        "elevenlabs diarization"
    );
    result
}

/// Sending and processing a whole meeting. Unlike a dictation, nobody is
/// waiting at the keyboard, so it can take about the meeting's own length.
fn diarize_timeout_seconds(samples: usize) -> u64 {
    120 + (samples as u64).div_ceil(u64::from(SAMPLE_RATE))
}

/// The whole upload's limit: a slow connection may need about the clip's own
/// length to send it. Always well inside the engine's job deadline.
pub fn timeout_seconds(samples: usize) -> u64 {
    BASE_TIMEOUT_SECONDS + (samples as u64).div_ceil(u64::from(SAMPLE_RATE))
}

// The key store.

fn key_path() -> Result<PathBuf> {
    Ok(crate::app_paths::support_dir()?.join(KEY_FILE))
}

fn environment_key() -> Option<String> {
    std::env::var(KEY_ENVIRONMENT).ok()
}

/// Which key an upload would use, without reading it into a caller.
pub fn key_source() -> KeySource {
    let file = key_path().ok();
    source_from(environment_key(), file.as_deref())
}

fn source_from(environment: Option<String>, file: Option<&Path>) -> KeySource {
    if environment.as_deref().and_then(usable_key).is_some() {
        return KeySource::Environment;
    }
    let saved = file
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| usable_key(&text).map(str::to_owned));
    if saved.is_some() {
        KeySource::File
    } else {
        KeySource::Missing
    }
}

fn read_key() -> Result<Option<String>> {
    read_key_from(environment_key(), &key_path()?)
}

fn read_key_from(environment: Option<String>, file: &Path) -> Result<Option<String>> {
    if let Some(key) = environment.as_deref().and_then(usable_key) {
        return Ok(Some(key.to_owned()));
    }
    match fs::read_to_string(file) {
        Ok(text) => Ok(usable_key(&text).map(str::to_owned)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        // The error names the path, never the contents.
        Err(error) => Err(eyre!("could not read the ElevenLabs API key: {error}")),
    }
}

/// A key is one line of visible ASCII, so it can't add a header to a request.
fn usable_key(text: &str) -> Option<&str> {
    let key = text.trim();
    (!key.is_empty() && key.bytes().all(|byte| byte.is_ascii_graphic())).then_some(key)
}

/// Saves the key to the owner-only file. Not `settings.json`: `hex settings`
/// would show it, and that file isn't forced to 0600.
pub fn save_key(key: &str) -> Result<()> {
    save_key_at(&key_path()?, key)
}

fn save_key_at(path: &Path, key: &str) -> Result<()> {
    let Some(key) = usable_key(key) else {
        bail!("That doesn't look like an API key");
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    // An older file keeps its mode through `open`; tighten it before writing.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(key.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

pub fn remove_key() -> Result<()> {
    remove_key_at(&key_path()?)
}

fn remove_key_at(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

// The audio.

/// 16 kHz mono 16-bit WAV in memory, about 1.9 MB a minute. It is never
/// written to disk.
fn encode_wav(samples: &[f32]) -> std::result::Result<Vec<u8>, hound::Error> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    // Header plus two bytes a sample.
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    {
        let mut writer = hound::WavWriter::new(Cursor::new(&mut bytes), spec)?;
        for sample in samples {
            let sample = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
            writer.write_sample(sample)?;
        }
        writer.finalize()?;
    }
    Ok(bytes)
}

// The request.

struct Request<'a> {
    url: &'a str,
    protocols: &'a str,
    model_id: &'a str,
    language: &'a str,
    timeout_seconds: u64,
    /// Upload this file instead of stdin.
    file: Option<&'a Path>,
    /// Ask who speaks each word.
    diarize: bool,
    /// Ask when each word was said.
    words: bool,
}

/// Everything curl is told on its command line, which any process on this Mac
/// can read: never the key or the audio.
fn curl_arguments(request: &Request<'_>) -> Vec<String> {
    let mut arguments: Vec<String> = [
        // First, or curl ignores it: `~/.curlrc` could otherwise add tracing
        // or a proxy to a request carrying the key.
        "-q",
        "--silent",
        "--show-error",
        "--proto",
        request.protocols,
        "--connect-timeout",
        &CONNECT_TIMEOUT_SECONDS.to_string(),
        "--max-time",
        &request.timeout_seconds.to_string(),
        "--max-filesize",
        &MAX_RESPONSE_BYTES.to_string(),
        "--variable",
        &format!("%{CURL_KEY_VARIABLE}"),
        "--expand-header",
        &format!("xi-api-key: {{{{{CURL_KEY_VARIABLE}}}}}"),
        "--form-string",
        &format!("model_id={}", request.model_id),
        // Never pasted as "(laughter)".
        "--form-string",
        "tag_audio_events=false",
        "--form-string",
        if request.words {
            "timestamps_granularity=word"
        } else {
            "timestamps_granularity=none"
        },
    ]
    .map(str::to_owned)
    .into();
    if request.language != AUTO_LANGUAGE {
        arguments.push("--form-string".into());
        arguments.push(format!("language_code={}", request.language));
    }
    if request.diarize {
        arguments.push("--form-string".into());
        arguments.push("diarize=true".into());
    }
    let source = request.file.map_or_else(
        || "-".to_owned(),
        |file| {
            // Curl's own quoting, so no path can add form options.
            let path = file.to_string_lossy();
            format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
        },
    );
    arguments.extend(
        [
            "--form",
            &format!("file=@{source};filename=audio.wav;type=audio/wav"),
            "--write-out",
            "\n%{http_code}",
            request.url,
        ]
        .map(str::to_owned),
    );
    arguments
}

/// Runs curl until it answers, returning a successful answer's body, or until
/// `stop` says to give up, which kills it.
/// No `--location`: a redirect can't carry the key anywhere else.
fn post(
    request: &Request<'_>,
    key: &str,
    audio: Vec<u8>,
    stop: impl Fn() -> bool,
) -> std::result::Result<Vec<u8>, UploadError> {
    if stop() {
        return Err(UploadError::Cancelled);
    }
    let unreachable = |error: &dyn std::fmt::Display| UploadError::Unreachable(format!("{error}"));
    let mut command: Command = crate::background_process::command(CURL);
    let mut child = command
        .args(curl_arguments(request))
        .env(CURL_KEY_VARIABLE, key)
        .stdin(if request.file.is_some() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| unreachable(&format_args!("could not start curl: {error}")))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    // Each pipe has its own thread: a full pipe would otherwise hold curl
    // until its timeout. A killed curl closes them all, so these all end.
    let writer = thread::spawn(move || {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(&audio);
        }
    });
    let output = thread::spawn(move || read_bounded(stdout, MAX_RESPONSE_BYTES));
    let errors = thread::spawn(move || read_bounded(stderr, MAX_ERROR_BYTES));
    let status = loop {
        if stop() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = writer.join();
            let _ = output.join();
            let _ = errors.join();
            return Err(UploadError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(POLL),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(unreachable(&format_args!(
                    "could not wait for curl: {error}"
                )));
            }
        }
    };
    let _ = writer.join();
    let output = output.join().unwrap_or_default();
    let errors = errors.join().unwrap_or_default();
    if !status.success() {
        let detail = String::from_utf8_lossy(&errors).replace(key, "[key]");
        return Err(UploadError::Unreachable(format!(
            "could not reach ElevenLabs: {}",
            detail.trim()
        )));
    }
    response_body(&output).map(<[u8]>::to_vec)
}

/// Keeps at most `limit` bytes and goes on draining, so curl never blocks.
fn read_bounded(mut pipe: impl Read, limit: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) | Err(_) => return kept,
            Ok(read) => {
                let room = limit.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..read.min(room)]);
            }
        }
    }
}

/// The body, then the status on the last line (`--write-out`). Errors go by
/// status alone: ElevenLabs' documented error bodies disagree, and they never
/// reach a log or the window.
fn response_body(output: &[u8]) -> std::result::Result<&[u8], UploadError> {
    let split = output.iter().rposition(|byte| *byte == b'\n');
    let (body, status) = match split {
        Some(split) => (&output[..split], &output[split + 1..]),
        None => (&output[..0], output),
    };
    let status = std::str::from_utf8(status)
        .ok()
        .and_then(|status| status.trim().parse::<u16>().ok())
        .ok_or_else(|| UploadError::Unreachable("ElevenLabs sent no status".into()))?;
    match status {
        200..=299 => Ok(body),
        401 => Err(UploadError::Rejected(
            "ElevenLabs didn't accept the API key".into(),
        )),
        403 => Err(UploadError::Rejected(
            "This ElevenLabs API key can't use speech to text".into(),
        )),
        408 | 429 | 500..=599 => Err(UploadError::Unreachable(format!(
            "ElevenLabs answered {status}"
        ))),
        status => Err(UploadError::Rejected(format!(
            "ElevenLabs refused the audio ({status})"
        ))),
    }
}

fn parse_transcript(body: &[u8]) -> std::result::Result<String, UploadError> {
    #[derive(serde::Deserialize)]
    struct Response {
        text: String,
    }
    serde_json::from_slice::<Response>(body)
        .map(|response| response.text.trim().to_owned())
        .map_err(|_| UploadError::Unreachable("ElevenLabs sent an unexpected answer".into()))
}

/// Spoken words with their times, ignoring spacing and events.
fn parse_words(body: &[u8]) -> std::result::Result<Vec<TimedWord>, UploadError> {
    #[derive(serde::Deserialize)]
    struct Word {
        #[serde(rename = "type")]
        kind: String,
        text: String,
        start: Option<f64>,
        end: Option<f64>,
    }
    #[derive(serde::Deserialize)]
    struct Response {
        words: Vec<Word>,
    }
    let response: Response = serde_json::from_slice(body)
        .map_err(|_| UploadError::Unreachable("ElevenLabs sent an unexpected answer".into()))?;
    Ok(response
        .words
        .into_iter()
        .filter(|word| word.kind == "word")
        .filter_map(|word| {
            Some(TimedWord {
                text: word.text,
                start_ms: (word.start? * 1000.0) as u64,
                end_ms: (word.end? * 1000.0) as u64,
            })
        })
        .collect())
}

/// Word timings into stretches of one speaker, ignoring spacing and events.
fn parse_spans(body: &[u8]) -> std::result::Result<Vec<SpeakerSpan>, UploadError> {
    #[derive(serde::Deserialize)]
    struct Word {
        #[serde(rename = "type")]
        kind: String,
        start: Option<f64>,
        end: Option<f64>,
        speaker_id: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Response {
        words: Vec<Word>,
    }
    let response: Response = serde_json::from_slice(body)
        .map_err(|_| UploadError::Unreachable("ElevenLabs sent an unexpected answer".into()))?;
    let mut spans: Vec<SpeakerSpan> = Vec::new();
    for word in response.words {
        let (true, Some(start), Some(end), Some(speaker)) =
            (word.kind == "word", word.start, word.end, word.speaker_id)
        else {
            continue;
        };
        let (start_ms, end_ms) = ((start * 1000.0) as u64, (end * 1000.0) as u64);
        match spans.last_mut() {
            Some(last) if last.speaker == speaker => last.end_ms = last.end_ms.max(end_ms),
            _ => spans.push(SpeakerSpan {
                speaker,
                start_ms,
                end_ms,
            }),
        }
    }
    Ok(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    const FAKE_KEY: &str = "fake-test-key-value";

    fn request<'a>(url: &'a str, language: &'a str, timeout_seconds: u64) -> Request<'a> {
        Request {
            url,
            protocols: "=http",
            model_id: "scribe_v2",
            language,
            timeout_seconds,
            file: None,
            diarize: false,
            words: false,
        }
    }

    fn parse_response(output: &[u8]) -> std::result::Result<String, UploadError> {
        response_body(output).and_then(parse_transcript)
    }

    fn post_text(
        request: &Request<'_>,
        audio: Vec<u8>,
        stop: impl Fn() -> bool,
    ) -> std::result::Result<String, UploadError> {
        post(request, FAKE_KEY, audio, stop).and_then(|body| parse_transcript(&body))
    }

    /// What the loopback server read.
    struct Received {
        head: String,
        body: Vec<u8>,
    }

    fn read_request(stream: &mut TcpStream) -> Received {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 64 * 1024];
        let head_end = loop {
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "the request ended early");
            bytes.extend_from_slice(&buffer[..read]);
        };
        let head = String::from_utf8_lossy(&bytes[..head_end]).into_owned();
        let length: usize = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .expect("curl sends a length for stdin");
        if head.to_ascii_lowercase().contains("expect: 100-continue") {
            stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").unwrap();
        }
        while bytes.len() < head_end + length {
            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "the body ended early");
            bytes.extend_from_slice(&buffer[..read]);
        }
        Received {
            head,
            body: bytes[head_end..].to_vec(),
        }
    }

    /// Answers one request with `status` and `body`, and reports what it read.
    fn server(status: u16, body: String) -> (String, mpsc::Receiver<Received>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/speech-to-text",
            listener.local_addr().unwrap()
        );
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let received = read_request(&mut stream);
            let answer = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(answer.as_bytes());
            let _ = send.send(received);
        });
        (url, receive)
    }

    /// Accepts and never answers. The returned listener keeps the port open.
    fn hanging_server() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 4096];
            while matches!(stream.read(&mut buffer), Ok(read) if read > 0) {}
        });
        (url, handle)
    }

    fn never() -> bool {
        false
    }

    #[test]
    fn audio_round_trips_as_16_khz_mono_16_bit_wav() {
        let samples = [0.0, 0.5, -0.5, 1.0, -1.0, 2.0, -2.0];
        let bytes = encode_wav(&samples).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        let mut reader = hound::WavReader::new(Cursor::new(bytes)).unwrap();
        let spec = reader.spec();
        assert_eq!(
            (spec.channels, spec.sample_rate, spec.bits_per_sample),
            (1, 16_000, 16)
        );
        let decoded: Vec<i16> = reader.samples::<i16>().map(Result::unwrap).collect();
        assert_eq!(
            decoded,
            [0, 16383, -16383, 32767, -32767, 32767, -32767],
            "clipped, not wrapped"
        );
    }

    #[test]
    fn the_command_line_carries_neither_the_key_nor_a_redirect() {
        let arguments = curl_arguments(&request("https://example.test/", "en", 30));
        let joined = arguments.join(" ");
        assert!(!joined.contains(FAKE_KEY));
        assert_eq!(arguments[0], "-q", "only honored as the first argument");
        assert!(joined.contains("xi-api-key: {{HEX_XI_KEY}}"));
        assert!(!joined.contains("--location"));
        assert!(!joined.contains(" -L"));
        assert!(!joined.contains("enable_logging"));
    }

    #[test]
    fn the_language_field_is_left_out_for_auto_detection() {
        let field = |language| {
            curl_arguments(&request("https://example.test/", language, 30))
                .iter()
                .any(|argument| argument.starts_with("language_code="))
        };
        assert!(field("de"));
        assert!(!field(AUTO_LANGUAGE));
    }

    #[test]
    fn the_timeout_grows_with_the_clip() {
        assert_eq!(timeout_seconds(0), 20);
        assert_eq!(timeout_seconds(1), 21);
        assert_eq!(timeout_seconds(16_000 * 60), 80);
    }

    #[test]
    fn a_response_parses_by_its_status_and_text_field() {
        let sample = br#"{"language_code":"eng","language_probability":0.98,"text":" Hello world. ","words":[]}
200"#;
        assert_eq!(parse_response(sample).unwrap(), "Hello world.");
        let unreachable =
            |text: &[u8]| matches!(parse_response(text), Err(UploadError::Unreachable(_)));
        let rejected = |text: &[u8]| matches!(parse_response(text), Err(UploadError::Rejected(_)));
        assert!(rejected(b"{\"detail\":\"secret\"}\n401"));
        assert!(rejected(b"{}\n403"));
        assert!(rejected(b"{}\n422"));
        assert!(rejected(b"{}\n400"));
        assert!(unreachable(b"{}\n429"));
        assert!(unreachable(b"{}\n500"));
        assert!(unreachable(b"{}\n503"));
        assert!(unreachable(b"not json\n200"));
        assert!(unreachable(b""));
        assert!(
            !parse_response(b"{\"detail\":\"secret\"}\n401")
                .unwrap_err()
                .to_string()
                .contains("secret"),
            "error bodies stay out of messages"
        );
    }

    #[test]
    fn the_environment_key_wins_over_the_file() {
        let directory = tempfile_dir("precedence");
        let path = directory.join(KEY_FILE);
        assert_eq!(source_from(None, Some(&path)), KeySource::Missing);
        assert_eq!(read_key_from(None, &path).unwrap(), None);
        save_key_at(&path, "from-file").unwrap();
        assert_eq!(source_from(None, Some(&path)), KeySource::File);
        assert_eq!(
            read_key_from(None, &path).unwrap().as_deref(),
            Some("from-file")
        );
        let environment = || Some("from-environment".to_owned());
        assert_eq!(
            source_from(environment(), Some(&path)),
            KeySource::Environment
        );
        assert_eq!(
            read_key_from(environment(), &path).unwrap().as_deref(),
            Some("from-environment")
        );
        // An empty variable counts as unset.
        assert_eq!(source_from(Some("  ".into()), Some(&path)), KeySource::File);
        remove_key_at(&path).unwrap();
        remove_key_at(&path).unwrap();
        assert_eq!(source_from(None, Some(&path)), KeySource::Missing);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_owner_only_even_over_an_older_file() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile_dir("permissions");
        let path = directory.join(KEY_FILE);
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        save_key_at(&path, " new-key\n").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&path).unwrap(), "new-key");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_key_with_control_characters_or_spaces_is_refused() {
        let directory = tempfile_dir("refused");
        let path = directory.join(KEY_FILE);
        for key in ["", "   ", "two\nlines", "with space", "tab\tbed", "é"] {
            assert!(save_key_at(&path, key).is_err(), "{key:?}");
        }
        assert!(!path.exists());
        fs::write(&path, "a\nb").unwrap();
        assert_eq!(read_key_from(None, &path).unwrap(), None);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_successful_upload_sends_the_key_fields_and_audio() {
        let (url, received) = server(200, r#"{"text":"hello there","words":[]}"#.into());
        let samples = vec![0.25_f32; 100_000];
        let audio = encode_wav(&samples).unwrap();
        let text = post_text(&request(&url, "de", 10), audio.clone(), never).unwrap();
        assert_eq!(text, "hello there");
        let received = received.recv().unwrap();
        assert!(received.head.contains(&format!("xi-api-key: {FAKE_KEY}")));
        let body = String::from_utf8_lossy(&received.body);
        for field in [
            "name=\"model_id\"",
            "scribe_v2",
            "name=\"tag_audio_events\"",
            "name=\"timestamps_granularity\"",
            "name=\"language_code\"",
            "filename=\"audio.wav\"",
            "audio/wav",
        ] {
            assert!(body.contains(field), "{field}");
        }
        assert!(
            received
                .body
                .windows(audio.len())
                .any(|window| window == audio),
            "the audio arrived intact"
        );
    }

    #[test]
    fn a_diarization_uploads_the_file_and_never_stdin() {
        let directory = tempfile_dir("diarize upload");
        let track = directory.join("system.speakers.wav");
        fs::write(&track, b"RIFF-track-bytes").unwrap();
        let answer = serde_json::json!({ "words": [
            {"type": "word", "text": "Hi", "start": 0.0, "end": 0.5, "speaker_id": "speaker_0"},
            {"type": "spacing", "text": " ", "start": 0.5, "end": 0.6, "speaker_id": "speaker_0"},
            {"type": "word", "text": "there", "start": 0.6, "end": 1.0, "speaker_id": "speaker_0"},
            {"type": "word", "text": "Hello", "start": 2.0, "end": 2.5, "speaker_id": "speaker_1"},
            {"type": "audio_event", "text": "(cough)", "start": 3.0, "end": 3.1, "speaker_id": "speaker_2"},
        ]});
        let (url, received) = server(200, answer.to_string());
        let mut upload = request(&url, "auto", 10);
        upload.file = Some(&track);
        upload.diarize = true;
        upload.words = true;
        let body = post(&upload, FAKE_KEY, Vec::new(), never).unwrap();
        let spans = parse_spans(&body).unwrap();
        assert_eq!(
            spans,
            [
                SpeakerSpan {
                    speaker: "speaker_0".into(),
                    start_ms: 0,
                    end_ms: 1_000
                },
                SpeakerSpan {
                    speaker: "speaker_1".into(),
                    start_ms: 2_000,
                    end_ms: 2_500
                },
            ]
        );
        let received = received.recv().unwrap();
        let sent = String::from_utf8_lossy(&received.body);
        for field in [
            "name=\"diarize\"",
            "word",
            "RIFF-track-bytes",
            "filename=\"audio.wav\"",
        ] {
            assert!(sent.contains(field), "{field}");
        }
        assert!(!sent.contains("language_code"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn word_timings_keep_words_and_skip_spacing_and_events() {
        let body = serde_json::json!({ "words": [
            {"type": "word", "text": "Hello,", "start": 0.25, "end": 0.5},
            {"type": "spacing", "text": " ", "start": 0.5, "end": 0.6},
            {"type": "audio_event", "text": "(cough)", "start": 0.6, "end": 0.7},
            {"type": "word", "text": "no-times"},
            {"type": "word", "text": "there", "start": 0.75, "end": 1.0},
        ]})
        .to_string();
        let words = parse_words(body.as_bytes()).unwrap();
        assert_eq!(
            words,
            [
                TimedWord {
                    text: "Hello,".into(),
                    start_ms: 250,
                    end_ms: 500
                },
                TimedWord {
                    text: "there".into(),
                    start_ms: 750,
                    end_ms: 1_000
                },
            ]
        );
        assert!(matches!(
            parse_words(b"nope"),
            Err(UploadError::Unreachable(_))
        ));
    }

    #[test]
    fn asking_for_words_changes_only_the_timing_field() {
        let mut upload = request("https://example.test/", "en", 30);
        upload.words = true;
        let arguments = curl_arguments(&upload);
        assert!(arguments.contains(&"timestamps_granularity=word".to_owned()));
        assert!(!arguments.contains(&"diarize=true".to_owned()));
        assert!(
            arguments
                .iter()
                .any(|argument| argument.starts_with("file=@-;"))
        );
    }

    #[test]
    fn a_form_path_cant_add_options() {
        let path = Path::new("/tmp/a;type=text/plain\\\"b.wav");
        let mut upload = request("https://example.test/", "en", 30);
        upload.file = Some(path);
        let arguments = curl_arguments(&upload);
        assert!(
            arguments.contains(
                &"file=@\"/tmp/a;type=text/plain\\\\\\\"b.wav\";filename=audio.wav;type=audio/wav"
                    .to_owned()
            )
        );
    }

    #[test]
    fn a_meeting_has_about_its_own_length_to_upload() {
        assert_eq!(diarize_timeout_seconds(16_000 * 3_600), 3_720);
    }

    #[test]
    fn a_response_larger_than_a_pipe_buffer_still_arrives() {
        let text = "word ".repeat(100_000);
        let (url, _received) = server(200, serde_json::json!({ "text": text }).to_string());
        let answer = post_text(&request(&url, "en", 10), vec![0; 16], never).unwrap();
        assert_eq!(answer, text.trim());
    }

    #[test]
    fn server_errors_are_unreachable_and_a_bad_key_is_rejected() {
        let (url, _received) = server(500, "{}".into());
        assert!(matches!(
            post_text(&request(&url, "en", 10), vec![0; 16], never),
            Err(UploadError::Unreachable(_))
        ));
        let (url, _received) = server(401, "{}".into());
        assert!(matches!(
            post_text(&request(&url, "en", 10), vec![0; 16], never),
            Err(UploadError::Rejected(_))
        ));
    }

    #[test]
    fn a_refused_connection_is_unreachable_and_never_names_the_key() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let error = post_text(&request(&url, "en", 10), vec![0; 16], never).unwrap_err();
        assert!(matches!(error, UploadError::Unreachable(_)), "{error:?}");
        assert!(!error.to_string().contains(FAKE_KEY));
    }

    #[test]
    fn plain_http_is_refused_outside_tests() {
        let (url, _handle) = hanging_server();
        let mut refused = request(&url, "en", 10);
        refused.protocols = "=https";
        let error = post_text(&refused, vec![0; 16], never).unwrap_err();
        assert!(matches!(error, UploadError::Unreachable(_)), "{error:?}");
    }

    #[test]
    fn a_hung_server_ends_at_the_timeout() {
        let (url, _handle) = hanging_server();
        let started = Instant::now();
        let error = post_text(&request(&url, "en", 1), vec![0; 16], never).unwrap_err();
        assert!(matches!(error, UploadError::Unreachable(_)), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn cancelling_ends_the_upload_at_once() {
        let (url, _handle) = hanging_server();
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let trigger = cancelled.clone();
        let (armed, arm) = mpsc::channel();
        thread::spawn(move || {
            arm.recv().unwrap();
            thread::sleep(Duration::from_millis(200));
            trigger.store(true, Ordering::Release);
        });
        armed.send(()).unwrap();
        let started = Instant::now();
        let error = post_text(&request(&url, "en", 60), vec![0; 16], || {
            cancelled.load(Ordering::Acquire)
        })
        .unwrap_err();
        assert_eq!(error, UploadError::Cancelled);
        assert!(
            started.elapsed() < Duration::from_millis(700),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_cancelled_dictation_never_starts_curl() {
        // The URL can't be reached, so only an early return is this fast.
        let started = Instant::now();
        let error = post(
            &request("http://127.0.0.1:1/", "en", 60),
            FAKE_KEY,
            vec![0; 16],
            || true,
        )
        .unwrap_err();
        assert_eq!(error, UploadError::Cancelled);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    fn tempfile_dir(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "hex-elevenlabs-{name}-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }
}
