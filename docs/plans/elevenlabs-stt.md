# Plan: ElevenLabs Scribe as a cloud dictation option in Hex (macOS, first version)

**How far this is checked:** I read the code and today's ElevenLabs docs, and tested the curl command against a loopback server using a fake key. I made no real ElevenLabs calls and compiled nothing, because this host has no Rust toolchain. Nothing has run on macOS.

## What changes from your brief
- **Apple Speech is only a structural pattern.** `available()` returns false for it and `validate` rejects it (`src/transcription_models.rs:196`, test at `:1479`). This would be the only non-GGUF model that can actually run, so expect stale assumptions.
- **The biggest risk is audio going to the cloud by accident.** Meetings, Annotate and the local API all follow the dictation selection (`src/meeting_transcript.rs:736`, `src/capture_host.rs:680`). A simple third variant would upload whole calls.
- **`linux_dictation.rs` and `windows_dictation.rs` don't match on `ModelRuntime`.** They use `LocalTranscriber`, which `validate` already guards.
- **`ureq` is already in `Cargo.lock`.** It only comes in through the optional `onnx-benchmark` feature, so the default build has no HTTP client.

## Runtime design
```rust
// transcription_models.rs
pub enum ModelRuntime { Gguf(&'static GgufArtifact), AppleSpeech, ElevenLabs { model_id: &'static str } }

// transcription.rs (macOS only)
Transcriber::ElevenLabs(ElevenLabs)
fn transcribe(..)            // ElevenLabs: always Err
fn transcribe_segments(..)   // ElevenLabs: always Err
fn transcribe_dictation(&mut self, samples, cancelled: &AtomicBool) // the only path that uploads
```
- I'm using a concrete variant rather than a generic `Remote`, because `AGENTS.md` rules out hypothetical seams.
- The new model id is `elevenlabs_scribe_v2`, added last in `MODELS`.

## 1. Call sites

**The compiler will force these:**

| File | Change |
|---|---|
| `src/transcription.rs:12-97` | New variant and 5 match arms |
| `src/transcription_models.rs` | Enum, `as_str`, `from_str`, `available`, `size_label`, `download_bytes`, `is_installed`, `MODELS` |
| `src/app_settings.rs:1912` | Reject for keyboard switching, same as Apple Speech |
| `src/local_api.rs:756` | Treat as not prepared |
| `src/parakeet.rs:1153` | Empty architecture, same as Apple Speech |

**These compile unchanged but would behave wrongly:**

| File | Change |
|---|---|
| `src/parakeet.rs:417-437` | Call `transcribe_dictation` with `&control.cancelled`; `TranscriptionTarget::Service` uses the local selection |
| `src/meeting.rs:370,890,1162`, `src/meeting_transcript.rs:736,1905`, `src/capture_host.rs:680` | Use a new `local_transcription_selection()` |
| `src/local_api.rs:966,1185`, `src/local_api/streams.rs:173` | Reject the model and leave it out of `/models` |
| `src/app_settings.rs:2422` | When the feature is disabled, restore the last local model instead of the default |
| `src/transcription_models.rs:790,843,992` | Fix the "managed by macOS" error text |
| `src/status_item.rs:249` | Build the menu item regardless of Labs, otherwise it needs a relaunch to appear |
| `src/transcription_preparation.rs:183` | Start the speech detector download, which today only model downloads trigger |
| `src/rune_ui/picker.rs` | See section 7 |

**No change needed (confirm in review):**
- `src/transcription_engine.rs`: `held` and `park` are GGUF-only, and a model without streaming skips live streams.
- `WarmTranscriber`'s stream methods fall through on `_`.
- `src/gguf_session.rs:102`, `src/capture_host.rs:790` and `src/rune_ui/settings.rs:848`.
- `src/meeting_transcript.rs:820`, which is GGUF-only and so loads its own local model.
- `src/transcription_service.rs`, because the local API rejects the model first and `transcribe` returns an error anyway.
- `linux_dictation.rs`, `windows_dictation.rs`, `windows.rs`, `linux_app.rs` and `control/cli.rs`.

## 2. How a remote model counts as available
- **`available()`:** drop `const` and return `cfg!(target_os = "macos") && labs::elevenlabs()` for this runtime. Every existing consumer then respects the Labs switch with no edits, and existing tests still see it as unavailable by default.
- **`is_installed()`:** always true on macOS. If it depended on the key, removing the key would reopen the onboarding gate (`src/onboarding.rs:843`) and the button would say "Download".
- **`is_verified()`:** stays false, which is harmless because the local API rejects the model anyway.
- **Missing key:** `ElevenLabs::load` fails, and the picker shows that error through the existing preparation path. It makes no network call.
- **Turning Labs off:** `Feature::set` switches the selection back to the last local model. The upload function also checks the Labs switch itself.

To check during the work: `labs::state()` can read settings while `load_from` is running, so confirm `saved_labs` (`src/app_settings.rs:2102`) can't recurse.

## 3. HTTP: use curl

| | curl | ureq | reqwest |
|---|---|---|---|
| New dependencies | None | One line in `Cargo.toml`, already locked | Many; hyper and reqwest aren't in the lockfile |
| Escape | Kills the upload immediately | Blocking; as far as I know you can only abandon the thread, so upload continues | Needs an async runtime |
| Precedent | Updater and model downloads | None | None |
| Cost | Three pipe threads, one process per dictation | Least code | Largest merge surface |

`src/transcription_models.rs:1089-1126` is the pattern to copy, since it already has the poll-and-kill loop. I expect curl's cost to be small next to the upload, and will measure it in the Mac smoke test. Connection reuse in `ureq` wouldn't help much, since idle gaps between dictations are likely to outlast the connection.

```sh
curl --silent --show-error --proto =https --connect-timeout 5 --max-time <n> \
  --variable %HEX_XI_KEY --expand-header 'xi-api-key: {{HEX_XI_KEY}}' \
  -F model_id=scribe_v2 -F tag_audio_events=false -F timestamps_granularity=none \
  -F language_code=<omitted for auto> -F 'file=@-;filename=audio.wav;type=audio/wav' \
  --max-filesize 1048576 --write-out '\n%{http_code}' https://api.elevenlabs.io/v1/speech-to-text
```
- **Key:** it goes in the child's environment, so it never appears in `ps` arguments.
- **Audio:** it goes through stdin, so `-F file=@-` never writes it to disk, in line with the "no persisted audio" invariant.
- **Redirects:** there's no `--location`, so the key can't follow a redirect.
- **Tested:** the header, multipart fields and 200 KB body all arrived intact on Linux curl 8.18.
- **Still to check:** `--variable` needs curl 8.3 or later. Hex's minimum is macOS 15, and I haven't confirmed which curl it ships.
- **Threads:** stdout needs its own reader thread, or a large response would block on a full pipe until the timeout.

## 4. Audio and API
- **Encoding:** 16 kHz mono 16-bit WAV, written to a `Cursor<Vec<u8>>` with `hound`, about 1.9 MB per minute. Skip `pad_for_parakeet`.
- **Model id:** `scribe_v2` is the current batch model. `scribe_v1` is listed as deprecated, and `scribe_v2_realtime` is the live-transcription model, so it's out of scope.
- **Response:** read only the `text` field from the JSON (`language_code`, `language_probability`, `text`, `words`).
- **`file_format=pcm_s16le_16`:** the docs say it lowers latency compared with an encoded file. Start with WAV as you asked, measure both once, and switch if it helps, since it's a small change.
- **`tag_audio_events=false`:** set this explicitly so "(laughter)" is never pasted.
- **`timestamps_granularity=none`:** confirm it works on the first live call, because `words` is documented as required in the response.
- **Errors:** handle them by HTTP status only, because ElevenLabs' two doc pages disagree on the 429 error codes.

## 5. Key and model choice
**Decision:** read `ELEVENLABS_API_KEY` first, then an owner-only file at `<support>/elevenlabs-api-key`. The model choice stays in the existing `TranscriptionSelection`.
- **Not `settings.json`:**
  - `durable::write` uses plain `File::create`, so the file isn't forced to 0600.
  - The `every_persisted_setting_has_a_cli_key` test would push the key into `hex settings`.
  - `CaptureRouting` already strips its Jev key to avoid exactly this (`src/app_settings.rs:1589`).
- **Not the environment variable alone:** macOS apps launched from the Dock or at login generally don't inherit your shell's environment. It's still useful for development.
- **Existing secrets handling:** the repo has no Keychain code, only owner-only files.
- **Writing the file:** create it with `mode(0o600)` as `src/onboarding.rs:832` does, not with `durable::write`.
- **Logging:** never log the key, including in curl's stderr.

## 6. Cancel, timeout and fallback
- **Escape:** the upload loop polls every 25 ms and kills curl if any of these is set:
  - `control.cancelled`
  - `gguf_session::job_cancel()`, which covers deadlines and engine shutdown
  - `process_exit::quitting()`

  Audio already sent can't be recalled.
- **Why the extra wiring is needed:** today Escape only sets a flag, and the worker waits for the result (`src/parakeet.rs:805`). Without the kill, the upload would keep going after Escape.
- **Timeouts:** connect 5 s, total 20 s plus the clip's length. That always stays under the engine's deadline of 180 s plus 8× the clip (`src/transcription_engine.rs:74`). A test will hold this true.
- **While a request is in flight:** the engine thread is busy, so later jobs queue in order and meeting chunks wait. Recording itself isn't affected.
- **Fallback:** on by default through `labs.elevenlabs_fallback`.
  - It runs on connection failures, timeouts, 429 and 5xx.
  - It loads the last local model on the engine thread if one is installed, transcribes, then drops it.
  - It logs a warning but plays no error tone, because `ModelFailed` would sound like a failure after a successful paste.
- **401, 403 and 422:** these fail visibly through `src/recognition.rs:1948`, and that clip is lost. I chose this so a bad key can't silently run on the local model.

## 7. UI
- **Labs:** add a row inside `if cfg!(target_os = "macos")`, following `src/rune_ui/labs.rs:210`: "ElevenLabs transcription: sends dictation audio to ElevenLabs".
- **Picker card (`src/rune_ui/picker.rs`):**
  - Add a "Cloud" badge and a warning line.
  - Show Size and Speed as "Cloud".
  - Replace "System" at `:471` with "Not published".
  - Hide "Installed" at `:336`.
  - Replace "Downloading 0%" at `:307`.
- **Picker title:** change "Local transcription" only while a cloud model is offered.
- **Key field:**
  - With no key, show `TextInput::secure()` (present at the pinned Rune rev) and Save. The card's `on_press` at `:517` must be disabled here, or clicking into the field selects the model.
  - With a saved key, show "Key saved" and Remove.
  - With the environment variable set, show "From ELEVENLABS_API_KEY".
- **General pane:** `transcription_status` keeps showing "Audio is sent to ElevenLabs" while this model is active.
- **Strings:** use plain English rather than `tr()`, following `picker.rs:491`. That keeps the translation table out of the diff.

## Privacy
- **Off by default:** it stays hidden until the Labs switch is on, is never in the recommended lists, and first-run setup never picks it.
- **Labelled everywhere:** the model name is "ElevenLabs Scribe v2 (cloud)", so the label also reaches the menu bar and `hex model list`.
- **Dictation only:** `transcribe_dictation` is the single upload path. Meetings, Annotate and the local API can't reach it, and `/dictations` from other apps stays local.
- **Retention:** zero retention (`enable_logging=false`) is enterprise-only, so the UI must not imply audio is deleted. I didn't research how long ElevenLabs keeps it.
- **Silence:** the speech detector runs before upload (`src/dictation.rs:217`). Until it has downloaded, only the level check protects against sending silence.

## 8. Tests and keeping the diff small

**Automated:**
- **Client (`elevenlabs.rs`, runs on Linux and macOS):**
  - WAV round trip through `hound`
  - The key never appears in curl's arguments
  - Language field handling
  - Parsing the docs' sample response and the 401, 422 and 429 errors
  - Key precedence and 0600 permissions
- **Loopback with real curl:**
  - Success and a 500 response
  - Connection refused
  - A hang ended by the timeout
  - A hang ended by cancel, returning in under 500 ms with the child gone
- **Gating:**
  - Unavailable with Labs off
  - `load_from` migrates a saved cloud selection
  - Turning Labs off restores the local model
  - The local API rejects the model
- **macOS only:**
  - `transcribe` and `transcribe_segments` return errors without starting curl
  - `/dictations` and meeting selections resolve to a local model
- **Existing tests:** add the new id to `model_ids_have_explicit_stable_wire_names`, and run `HEX_UPDATE_GOLDENS=1` on Linux expecting zero image changes.

**Manual on a Mac with a real key (costs a little):**
- Normal dictation.
- Escape mid-upload, with `pgrep curl` confirming it's gone.
- Airplane mode, for the fallback.
- A bad key, and no key.
- `ps -axww` showing no key.
- A meeting and an Annotate capture with the cloud model selected, watching `nettop` for no ElevenLabs connection.

**Keeping merges easy:**
- All logic goes in a new `src/elevenlabs.rs`.
- `Cargo.toml`, `Cargo.lock` and `nix/package.nix` stay untouched, so Rune pin bumps can't conflict.
- New entries go at the end of enums and lists.
- Docs go in a new `docs/features/elevenlabs.md`, plus one index line and short `AGENTS.md` additions.
- Expect small repeat conflicts in `labs.rs` and `control/settings.rs`, because upstream adds Labs features often.

**Risk when running a fork build:**
- **Updater:** build with `HEX_LOCAL_BUILD=1`, because a self-updating install would replace itself with upstream.
- **Going back to upstream:** an upstream binary fails to parse `settings.json` at `src/app_settings.rs:2131` while this model is selected. Switch to a local model first.

## Ordered tasks

| # | Task | Hours |
|---|---|---|
| 1 | On the Mac: confirm the toolchain and curl version, get a test key, run the curl command once by hand | 0.5 |
| 2 | Catalog, match arms, Labs switch, settings key, migration | 3 |
| 3 | `local_transcription_selection()`, meeting, Annotate and local API changes (before any upload code exists) | 3 |
| 4 | `elevenlabs.rs`: key store, WAV, curl runner, parsing, loopback tests | 5 |
| 5 | `Transcriber` variant, dictation wiring, Escape, speech detector download | 2 |
| 6 | Fallback | 1.5 |
| 7 | Labs row, picker, General status, menu bar | 4 |
| 8 | Docs and feature map | 1 |
| 9 | Mac smoke test and fixes | 3-4 |

**Estimate:** about 23 hours, or 3 working days. That's roughly 300 lines of new module, 200 lines of tests, and 150 lines touched across about 16 files. Add up to a day if macOS compiles have to go through CI.

## Decisions (answered)
1. **Test machine:** the Mac, built and tested locally. No GitHub Actions needed.
2. **Fork only:** no translations, no SDK changes, macOS only.
3. **ElevenLabs default logging is acceptable.**
4. **API key:** a 0600 file is acceptable. Keychain is out of the first version.

## Plan changes from those answers
- **Mac (tasks 1 and 9):** builds and tests run on `nicholass-macbook-pro-1`, so CI on your fork is no longer needed. I still don't know whether it has Rust and Xcode, so task 1 checks that. Add time if they need installing.
- **Physical testing:** I can drive dictation with `hex dictation start` and `hex dictation stop`, but that skips the shortcut. Real hold-to-dictate and Escape need you at the Mac, along with its microphone and Accessibility permissions.
- **Fork-only:** this confirms plain English strings, no translation table changes, no SDK changes, and macOS only. Build with `HEX_LOCAL_BUILD=1` so the updater can't replace your build with upstream's.
- **ElevenLabs logging:** the request won't send `enable_logging`. The UI will say audio goes to ElevenLabs and won't claim it gets deleted.
- **API key:** it stays in a 0600 file at `<support>/elevenlabs-api-key`, with `ELEVENLABS_API_KEY` taking priority. Keychain is out of the first version.

The task order and the estimate (about 23 hours) are unchanged.

## Status
Planned only. The plan was drafted on another machine, which had no Rust toolchain, so nothing in it has been compiled or run. Task 1 verifies the toolchain, curl and the curl command by hand on the Mac. This Mac has curl 8.7.1, which meets the 8.3 minimum for `--variable` and `--expand-header`.
