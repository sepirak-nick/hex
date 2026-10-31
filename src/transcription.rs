use std::sync::atomic::AtomicBool;

use color_eyre::eyre::{Result, eyre};

use crate::apple_speech::AppleSpeech;
use crate::elevenlabs::{ElevenLabs, UploadError};
use crate::parakeet::Parakeet;
pub use crate::transcript::{Transcript, TranscriptSegment};
use crate::transcription_models::{
    ModelRuntime, TranscriptionModelId, TranscriptionSelection, validate,
};

const UNIFIED_ENGLISH_TRAILING_SILENCE_SAMPLES: usize = 3_200;

pub enum Transcriber {
    Gguf(Box<Parakeet>),
    AppleSpeech(AppleSpeech),
    ElevenLabs(ElevenLabs),
}

/// What a cloud model can't do: nothing but dictation may upload.
const NOT_DICTATION: &str = "the cloud model only transcribes dictations";

#[derive(Default)]
pub struct WarmTranscriber {
    active: Option<Transcriber>,
}

impl Transcriber {
    pub fn load() -> Result<Self> {
        let (_, selection) = crate::app_settings::transcription_selection();
        Self::load_selection(&selection)
    }

    pub fn load_selection(selection: &TranscriptionSelection) -> Result<Self> {
        match validate(selection)?.runtime {
            ModelRuntime::Gguf(_) => Parakeet::load_selection(selection)
                .map(Box::new)
                .map(Self::Gguf),
            ModelRuntime::AppleSpeech => AppleSpeech::load(selection).map(Self::AppleSpeech),
            ModelRuntime::ElevenLabs { .. } => ElevenLabs::load(selection).map(Self::ElevenLabs),
        }
    }

    pub fn matches_selection(&self, selection: &TranscriptionSelection) -> bool {
        match self {
            Self::Gguf(model) => model.matches_selection(selection),
            Self::AppleSpeech(model) => model.matches_selection(selection),
            Self::ElevenLabs(model) => model.matches_selection(selection),
        }
    }

    /// Brings an idle GGUF model's weights back before audio arrives (see
    /// [`Parakeet::prewarm`]). Apple Speech has no weights of Hex's to warm.
    pub fn prewarm(&mut self) -> Result<()> {
        match self {
            Self::Gguf(model) => model.prewarm(),
            Self::AppleSpeech(_) | Self::ElevenLabs(_) => Ok(()),
        }
    }

    /// Transcribe normalized 16 kHz audio. Backend padding is applied once here,
    /// before GGUF chunking or any hex retranscription.
    pub fn transcribe(&mut self, mut samples: Vec<f32>) -> Result<String> {
        match self {
            Self::Gguf(model) => {
                prepare_gguf_samples(&mut samples, model.model_id());
                model.transcribe(&samples)
            }
            Self::AppleSpeech(model) => model.transcribe(&samples).map(|result| result.text),
            Self::ElevenLabs(_) => Err(eyre!(NOT_DICTATION)),
        }
    }

    /// Transcribe one normalized segment with the same input preparation as
    /// plain dictation. The caller retains ownership of segment offsets.
    pub fn transcribe_segments(&mut self, mut samples: Vec<f32>) -> Result<Transcript> {
        match self {
            Self::Gguf(model) => {
                prepare_gguf_samples(&mut samples, model.model_id());
                model
                    .transcribe_segments(&samples)
                    .map(|result| Transcript {
                        text: result.text,
                        segments: result
                            .segments
                            .into_iter()
                            .map(|segment| TranscriptSegment {
                                start_ms: segment.t0_ms,
                                end_ms: segment.t1_ms,
                                text: segment.text,
                            })
                            .collect(),
                        words: result
                            .words
                            .into_iter()
                            .map(|word| TranscriptSegment {
                                start_ms: word.t0_ms,
                                end_ms: word.t1_ms,
                                text: word.text,
                            })
                            .collect(),
                    })
            }
            Self::AppleSpeech(model) => model.transcribe(&samples),
            Self::ElevenLabs(_) => Err(eyre!(NOT_DICTATION)),
        }
    }
}

/// The local model a dictation falls back to, when allowed and downloaded.
fn fallback_selection(allowed: bool) -> Option<TranscriptionSelection> {
    if !allowed {
        return None;
    }
    let local = crate::app_settings::local_transcription_selection();
    let model = validate(&local).ok()?;
    (matches!(model.runtime, ModelRuntime::Gguf(_))
        && crate::transcription_models::is_installed(model, &local.language))
    .then_some(local)
}

fn prepare_gguf_samples(samples: &mut Vec<f32>, model: Option<TranscriptionModelId>) {
    crate::dictation::pad_for_parakeet(samples);
    if model == Some(TranscriptionModelId::ParakeetUnifiedEnglish) {
        samples.resize(
            samples.len() + UNIFIED_ENGLISH_TRAILING_SILENCE_SAMPLES,
            0.0,
        );
    }
}

impl WarmTranscriber {
    /// A warm transcriber already serving `model`, for opt-in harnesses.
    #[cfg(test)]
    pub(crate) fn serving(model: Transcriber) -> Self {
        Self {
            active: Some(model),
        }
    }

    pub fn load() -> Result<Self> {
        Ok(Self {
            active: Some(Transcriber::load()?),
        })
    }

    /// The selection of the warm model when it is a GGUF model.
    pub(crate) fn gguf_selection(&self) -> Option<&TranscriptionSelection> {
        match &self.active {
            Some(Transcriber::Gguf(model)) => model.selection(),
            _ => None,
        }
    }

    /// Stream options of the warm model, when it can stream on its device.
    pub(crate) fn stream_options(&self) -> Option<&transcribe_cpp::StreamOptions> {
        match &self.active {
            Some(Transcriber::Gguf(model)) => model.stream_options(),
            _ => None,
        }
    }

    pub(crate) fn with_stream<R>(
        &mut self,
        stream_options: &transcribe_cpp::StreamOptions,
        cancel: &transcribe_cpp::CancelToken,
        drive: impl FnOnce(&mut transcribe_cpp::Stream<'_>) -> R,
    ) -> transcribe_cpp::Result<R> {
        match &mut self.active {
            Some(Transcriber::Gguf(model)) => model.with_stream(stream_options, cancel, drive),
            _ => Err(transcribe_cpp::Error::NotImplemented(
                "the active transcriber cannot stream".into(),
            )),
        }
    }

    /// The loaded model, without switching or loading one.
    pub(crate) fn active(&mut self) -> Option<&mut Transcriber> {
        self.active.as_mut()
    }

    /// Transcribes one press-to-paste dictation: the only path that uploads
    /// audio. When ElevenLabs can't be reached it runs on the last local
    /// model, loaded for this clip and dropped, unless Labs turned that off.
    /// A rejected key or clip fails visibly: a bad key must not quietly run
    /// on the local model.
    pub(crate) fn transcribe_dictation(
        &mut self,
        selection: &TranscriptionSelection,
        samples: Vec<f32>,
        cancelled: &AtomicBool,
    ) -> Result<String> {
        let transcriber = self.activate(selection)?;
        let Transcriber::ElevenLabs(cloud) = transcriber else {
            return transcriber.transcribe(samples);
        };
        let reason = match cloud.transcribe(&samples, cancelled) {
            Ok(text) => return Ok(text),
            Err(UploadError::Unreachable(reason)) => reason,
            Err(error) => return Err(error.into()),
        };
        let Some(local) = fallback_selection(crate::labs::elevenlabs_fallback()) else {
            return Err(eyre!(reason));
        };
        tracing::warn!(
            %reason,
            model = local.model.as_str(),
            "ElevenLabs unreachable; transcribing this dictation locally"
        );
        Transcriber::load_selection(&local)
            .and_then(|mut model| model.transcribe(samples))
            .map_err(|error| eyre!("{reason}; the local model failed too: {error:#}"))
    }

    pub fn activate(&mut self, selection: &TranscriptionSelection) -> Result<&mut Transcriber> {
        if !self
            .active
            .as_ref()
            .is_some_and(|model| model.matches_selection(selection))
        {
            let candidate = Transcriber::load_selection(selection)?;
            self.active = Some(candidate);
        }
        Ok(self
            .active
            .as_mut()
            .expect("activated transcriber must be available"))
    }
}

#[cfg(test)]
mod tests {
    use super::{Transcript, UNIFIED_ENGLISH_TRAILING_SILENCE_SAMPLES, prepare_gguf_samples};
    use crate::transcription_models::TranscriptionModelId;

    fn scribe() -> crate::transcription_models::TranscriptionSelection {
        crate::transcription_models::TranscriptionSelection {
            model: TranscriptionModelId::ElevenLabsScribeV2,
            language: "en".into(),
            recognition_hints: String::new(),
        }
    }

    #[test]
    fn the_cloud_model_does_not_load_with_its_switch_off() {
        assert!(super::Transcriber::load_selection(&scribe()).is_err());
    }

    fn loaded_cloud_model() -> super::Transcriber {
        crate::labs::with_elevenlabs(true, || {
            // Built directly: loading also needs a key, which tests never read.
            super::Transcriber::ElevenLabs(crate::elevenlabs::ElevenLabs::for_test(&scribe()))
        })
    }

    #[test]
    fn only_a_dictation_can_reach_the_cloud_model() {
        let mut model = loaded_cloud_model();
        // Both return before any upload, whatever the switch says.
        crate::labs::with_elevenlabs(true, || {
            let error = model.transcribe(vec![0.0; 16]).unwrap_err().to_string();
            assert!(error.contains("only transcribes dictations"), "{error}");
            let error = model
                .transcribe_segments(vec![0.0; 16])
                .unwrap_err()
                .to_string();
            assert!(error.contains("only transcribes dictations"), "{error}");
        });
    }

    #[test]
    fn a_warm_cloud_model_counts_as_no_loaded_model_for_other_work() {
        let mut warm = super::WarmTranscriber::serving(loaded_cloud_model());
        assert!(warm.gguf_selection().is_none(), "/streams answers 503");
        assert!(warm.stream_options().is_none(), "dictation never streams");
    }

    #[test]
    fn a_dictation_with_the_switch_off_never_uploads() {
        let model = crate::elevenlabs::ElevenLabs::for_test(&scribe());
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let error = model.transcribe(&[0.0; 16], &cancelled).unwrap_err();
        assert!(error.to_string().contains("turned off"), "{error}");
    }

    /// `say` output at `HEX_LIVE_WAV`, as 16 kHz mono 16-bit.
    fn live_samples() -> Vec<f32> {
        let path = std::env::var("HEX_LIVE_WAV").expect("HEX_LIVE_WAV");
        hound::WavReader::open(path)
            .unwrap()
            .samples::<i16>()
            .map(|sample| f32::from(sample.unwrap()) / f32::from(i16::MAX))
            .collect()
    }

    fn auto_scribe() -> crate::transcription_models::TranscriptionSelection {
        crate::transcription_models::TranscriptionSelection {
            language: "auto".into(),
            ..scribe()
        }
    }

    /// Costs a little: uploads to ElevenLabs with the configured key.
    #[test]
    #[ignore = "uploads to ElevenLabs; needs a key and HEX_LIVE_WAV"]
    fn live_dictation_reaches_elevenlabs() {
        crate::labs::with_elevenlabs(true, || {
            let mut warm = super::WarmTranscriber::default();
            let cancelled = std::sync::atomic::AtomicBool::new(false);
            let started = std::time::Instant::now();
            let text = warm
                .transcribe_dictation(&auto_scribe(), live_samples(), &cancelled)
                .unwrap();
            eprintln!("LIVE {:?} text={text:?}", started.elapsed());
            assert!(text.to_lowercase().contains("pickled peppers"), "{text}");
        });
    }

    /// Costs little: cancels while uploading, watching `ps` as it does.
    #[test]
    #[ignore = "uploads to ElevenLabs; needs a key and HEX_LIVE_WAV"]
    fn live_cancel_kills_curl_and_ps_never_shows_the_key() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let processes = || {
            String::from_utf8(
                std::process::Command::new("/bin/ps")
                    .args(["-axww", "-o", "command"])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
        };
        let key = std::env::var("HEX_LIVE_KEY_PREFIX").expect("HEX_LIVE_KEY_PREFIX");
        crate::labs::with_elevenlabs(true, || {
            let model = crate::elevenlabs::ElevenLabs::for_test(&auto_scribe());
            let cancelled = std::sync::Arc::new(AtomicBool::new(false));
            let trigger = cancelled.clone();
            let watcher = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(120));
                let seen = processes();
                trigger.store(true, Ordering::Release);
                seen
            });
            let samples = live_samples();
            let error = model.transcribe(&samples, &cancelled).unwrap_err();
            let cancel_ended = std::time::Instant::now();
            let seen = watcher.join().unwrap();
            assert_eq!(error, crate::elevenlabs::UploadError::Cancelled);
            assert!(seen.contains("speech-to-text"), "curl was still running");
            assert!(!seen.contains(&key), "ps showed the key");
            assert!(
                !processes().contains("api.elevenlabs.io"),
                "curl outlived it"
            );
            eprintln!("CANCEL ok, gone after {:?}", cancel_ended.elapsed());
        });
    }

    /// Run with `HTTPS_PROXY` at a closed port, so curl can't connect, and the
    /// real support folder for its downloaded local model.
    #[test]
    #[ignore = "loads the local model on Metal; needs HEX_LIVE_WAV and a closed HTTPS_PROXY"]
    fn live_fallback_transcribes_locally_on_metal() {
        crate::labs::with_elevenlabs(true, || {
            let mut warm = super::WarmTranscriber::default();
            let cancelled = std::sync::atomic::AtomicBool::new(false);
            let started = std::time::Instant::now();
            let text = warm
                .transcribe_dictation(&auto_scribe(), live_samples(), &cancelled)
                .unwrap();
            eprintln!("FALLBACK {:?} text={text:?}", started.elapsed());
            assert!(text.to_lowercase().contains("peppers"), "{text}");
        });
    }

    #[test]
    fn the_fallback_can_be_turned_off() {
        assert_eq!(super::fallback_selection(false), None);
    }

    #[test]
    fn apple_bridge_transcript_uses_the_canonical_wire_shape() {
        let transcript: Transcript = serde_json::from_str(
            r#"{"text":"hello","segments":[{"startMs":10,"endMs":40,"text":"hello"}]}"#,
        )
        .unwrap();

        assert_eq!(transcript.text, "hello");
        assert_eq!(transcript.segments.len(), 1);
        assert_eq!(transcript.segments[0].start_ms, 10);
        assert_eq!(transcript.segments[0].end_ms, 40);
    }

    #[test]
    fn unified_english_gets_trailing_silence_for_final_token_context() {
        let mut samples = vec![0.5; 32_000];

        prepare_gguf_samples(
            &mut samples,
            Some(TranscriptionModelId::ParakeetUnifiedEnglish),
        );

        assert_eq!(
            samples.len(),
            32_000 + UNIFIED_ENGLISH_TRAILING_SILENCE_SAMPLES
        );
        assert!(samples[..32_000].iter().all(|sample| *sample == 0.5));
        assert!(samples[32_000..].iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn other_gguf_models_do_not_get_trailing_silence() {
        let mut samples = vec![0.5; 32_000];

        prepare_gguf_samples(&mut samples, Some(TranscriptionModelId::ParakeetV2));

        assert_eq!(samples.len(), 32_000);
    }

    #[test]
    fn short_unified_input_is_minimum_padded_before_trailing_context() {
        let mut samples = vec![0.5; 1_600];
        prepare_gguf_samples(
            &mut samples,
            Some(TranscriptionModelId::ParakeetUnifiedEnglish),
        );

        // The 200 ms context is additional to the 1.5 s minimum, not part of it.
        assert_eq!(
            samples.len(),
            24_000 + UNIFIED_ENGLISH_TRAILING_SILENCE_SAMPLES
        );
        assert!(samples[..1_600].iter().all(|sample| *sample == 0.5));
        assert!(samples[1_600..].iter().all(|sample| *sample == 0.0));
    }
}
