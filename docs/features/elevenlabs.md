# ElevenLabs transcription (Labs, macOS, this fork)

A cloud speech model for press-to-paste dictation. Off by default. It sends
dictation audio to ElevenLabs, so everything else stays on this Mac.

```ts
Labs > ElevenLabs transcription (off)      // labs.elevenlabs; hex labs on elevenlabs
├── Off -> the model is unavailable everywhere; a selected one returns to the last local model
│         (window, hex labs, hex settings, and loading settings all do this)
└── On -> Speech model picker shows "ElevenLabs Scribe v2 (cloud)", last among the models that support the language

Picker card                                // rune_ui/picker.rs
├── No key -> masked field + Save; nothing to choose yet
├── Key saved -> "Key saved" + Remove + Use
├── ELEVENLABS_API_KEY set -> "From ELEVENLABS_API_KEY" + Use
└── Selected -> General shows "Audio is sent to ElevenLabs"

Dictation with the model selected          // transcription::WarmTranscriber::transcribe_dictation
├── Near-silent or speechless capture -> discarded before upload (speech detector, level check)
├── Upload -> text is processed and pasted like any dictation
├── Escape, job deadline, or quit -> curl is killed at once; audio already sent can't be recalled
├── No connection, timeout, 429, 5xx -> last local model, loaded for this clip, no error tone
│   ├── labs.elevenlabs_fallback off or no local model downloaded -> the dictation fails visibly
│   └── The local model fails too -> the dictation fails visibly
└── 401, 403, 4xx -> the dictation fails visibly and its clip is lost, so a bad key can't quietly run locally

Stays on this Mac whatever is selected     // app_settings::local_transcription_selection
├── Meeting transcription, including the final pass and recovery (speaker labels below are a separate opt-in)
├── Annotate captures (each one loads the local model into the engine, evicting the cloud model)
├── /dictations from other apps
├── /transcriptions and /models/<id>/prepare -> the model is rejected; /models leaves it out
└── /streams -> 503 model-not-loaded while the cloud model is the engine's
```

Without a local model downloaded, those features report that their model isn't
installed rather than uploading.

## Meeting speaker labels

```ts
Labs > Meeting speaker labels (off)        // labs.meeting_speakers; hex labs on meeting-speakers; meeting_speakers.rs
├── Off -> nothing is uploaded; other participants are all Them
└── On, as a meeting publishes, whichever speech model it used
    ├── Nobody else spoke -> nothing is uploaded
    ├── system.wav -> a temporary owner-only 16 kHz file -> ElevenLabs once with diarize=true -> file removed. Never the microphone track
    ├── Each Them line gets the speaker with the most overlapping words -> Speaker 1, Speaker 2, by who spoke first
    │   ├── One speaker found -> stays Them
    │   └── A line no words overlap -> stays Them
    └── No key, no connection, a rejected request, or quitting -> publishes with Them; speakers_error shows "Speakers weren't labeled."
```

Hex's local transcript stays the text. ElevenLabs' words are only used for timing,
so a line where two people spoke goes to whoever spoke more of it. Labels appear
in the Meetings pane, the Markdown copy, `hex meetings`, subtitles, the notes and
chapter prompts, and `hex mcp`. Publication waits for the upload, at most 120 s
plus the meeting's length; the rolling transcript stays readable meanwhile. Labels
are per meeting, so Speaker 1 in one isn't the same person in another. Collect
participants' consent first: it uploads their voices, with ElevenLabs' default
logging. Check: `meeting_speakers.rs`; `live_diarization_tells_two_voices_apart`
uses `HEX_LIVE_DIARIZE_WAV` and a little credit.

### Speaker names

```ts
Labs > Meeting speaker names (off)         // labs.meeting_speaker_names; hex labs on meeting-speaker-names; meeting_speakers.rs
├── Needs Meeting speaker labels on, Summarize with OpenCode on, and OpenCode running; otherwise "Speakers weren't named."
└── On, once speakers are numbered, before publishing
    ├── Sends the calendar's attendees and up to 12,000 characters of transcript (start and end, your lines included) to the OpenCode notes model, at most 60 s
    ├── Each speaker gets a name or null -> a name must be well formed, found in the attendees or that transcript, and given to one speaker only
    └── Anything else, or any failure -> stays Speaker N
```

Names are inferences and can be wrong, and nothing in a transcript marks them as
guesses. Without a connected calendar it only has what people say in the call.
Check: `meeting_speakers.rs`; `live_opencode_names_speakers_and_leaves_the_unsure_one`
uses `HEX_LIVE_OPENCODE_MODEL`.

## Talk and click

```ts
Labs > Talk and click (off)                // labs.talk_and_click; hex labs on talk-and-click; talk_and_click.rs
├── Off -> the port is closed; pages with the script do nothing
└── On -> Hex listens on 127.0.0.1:47489 for pages with <script src="http://127.0.0.1:47489/client.js">
    ├── A dictation records, from its press until its clip is submitted
    │   └── The page highlights what's under the pointer; a click marks it instead of pressing it. Agentation's toolbar still clicks normally
    ├── It finishes with no clicks -> an ordinary dictation, on the selected model, pasted
    ├── It finishes with clicks -> nothing is pasted or kept in History
    │   ├── Agentation's server isn't running -> fails before any upload, naming the command to start it; the words are lost
    │   ├── Audio -> ElevenLabs with word timing, whichever model is selected, and with no local fallback
    │   ├── Each sentence -> the click made while saying it, else the nearest; a click with no words says so
    │   │   └── Several clicks in one sentence -> divided at its commas, pauses, and "and", "but", or "then", then by word
    │   └── One note per click -> Agentation's server on 4747, in the toolbar's session when the page has one
    │       └── With several clicks, each note's attributes carry hexClick and hexFullRecording: everything said, with [click N] markers, up to 8,000 characters
    └── Escape -> nothing is sent; its clicks are dropped by the next dictation
```

The page's clicks are refused unless a dictation records, and the listener refuses
other `Host` headers and any `Origin` but `localhost`, `127.0.0.1`, `[::1]`,
`*.localhost`, `*.local`, and `*.test`. Notes carry what the page sends, and an agent
reads them, so only add the script to pages you trust. Selectors and React component
names are this script's own, simpler than Agentation's; source files aren't captured.
A sentence is found by Scribe's punctuation or a 1.5 s pause, and goes to one click
unless several fall inside it, so a remark about two things needs a click during each.

Check: `talk_and_click.rs`; `live_a_real_page_click_becomes_an_agentation_note` drives
`prototypes/talk-and-click-demo` with a browser and `HEX_LIVE_WAV`.

## The key

`ELEVENLABS_API_KEY` wins, then the owner-only `<support>/elevenlabs-api-key`
(`~/Library/Application Support/hex/elevenlabs-api-key`). It is not in
`settings.json`, so `hex settings` never shows it. Apps opened from the Dock
don't inherit a shell's environment, so the file is the normal path. It is read
for every upload, so Remove takes effect at once. A missing key fails the model's
load with no network call.

## The request

[`src/elevenlabs.rs`](../../src/elevenlabs.rs) runs `/usr/bin/curl`, with no new
dependency. The key reaches curl through its environment and the audio through
stdin as an in-memory 16 kHz mono 16-bit WAV, so neither shows in `ps` nor touches
the disk. `-q` ignores `~/.curlrc`, `--proto =https` refuses anything else, and
there is no `--location`. It sends `model_id=scribe_v2`,
`tag_audio_events=false`, `timestamps_granularity=none`, and `language_code`
unless the language is Auto; it does not send `enable_logging`, so ElevenLabs'
default logging applies. The Hex UI doesn't claim audio is deleted. Connect
timeout is 5 s; the whole upload gets 20 s plus the clip's length.

Raw PCM (`file_format=pcm_s16le_16`) was measured against WAV on a 4 s clip (three
runs each: 0.45-0.68 s against 0.51-0.58 s) and showed no gain, so it stays WAV.

## Checks

- `cargo nextest run elevenlabs::` runs real curl against a loopback server:
  success, 401, 500, refused connection, hang until timeout, cancel, and large
  responses.
- `other_apps_never_see_or_reach_the_cloud_model` in `local_api.rs`,
  `audio_that_stays_local_never_follows_the_selection_to_the_cloud` and the
  `turning_elevenlabs_off_*` tests in `app_settings.rs`,
  `only_a_dictation_can_reach_the_cloud_model` in `transcription.rs`,
  `an_elevenlabs_upload_always_ends_inside_the_job_deadline` in
  `transcription_engine.rs`, and
  `the_cloud_card_warns_and_takes_a_key_before_it_can_be_chosen` in
  `rune_ui/tests.rs`.
- Ignored live tests in `transcription.rs` (`--run-ignored only live_`) use
  `HEX_LIVE_WAV`, a key, and a little ElevenLabs credit:
  `live_dictation_reaches_elevenlabs`,
  `live_cancel_kills_curl_and_ps_never_shows_the_key` (`HEX_LIVE_KEY_PREFIX`), and
  `live_fallback_transcribes_locally_on_metal` (a closed `HTTPS_PROXY`).

## Known limits

- The first local fallback waits for the model to load: 14.7 s for a 4 s clip in a
  debug build.
- An upstream Hex can't parse `settings.json` while this model is selected. Switch
  to a local model before going back to one.
- macOS only. Goldens haven't been regenerated, since they only run on Linux; the
  row is macOS-only and the model is unavailable there, so no change is expected.
