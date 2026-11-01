//! Labs › Meeting speaker labels (macOS): tells the other participants apart.
//!
//! Before a meeting publishes, the system track (never the microphone) goes to
//! ElevenLabs once, resampled to a temporary 16 kHz file. Hex's own local
//! transcript stays the text: ElevenLabs' word timings only decide which
//! speaker each `Them` line gets, by the most overlapping speech. Any failure
//! leaves plain `Them` and records why; it never holds a meeting back.

use std::fs;
use std::path::Path;

use color_eyre::eyre::{Result, eyre};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};

use super::{MeetingManifest, MeetingSource, TranscriptEntry};
use crate::dictation::StreamingResampler;
use crate::elevenlabs::SpeakerSpan;
use crate::transcription_models::TranscriptionSelection;

const UPLOAD_FILE: &str = "system.speakers.wav";
const UPLOAD_RATE: u32 = 16_000;

/// Labels `entries` when the Labs switch is on and anyone else spoke.
pub(super) fn label(
    directory: &Path,
    manifest: &MeetingManifest,
    selection: &TranscriptionSelection,
    entries: &mut [TranscriptEntry],
) -> Result<()> {
    if !cfg!(target_os = "macos") || !crate::labs::meeting_speakers() {
        return Ok(());
    }
    if !entries
        .iter()
        .any(|entry| entry.source == MeetingSource::System)
    {
        return Ok(());
    }
    let upload = directory.join(UPLOAD_FILE);
    let spans = write_upload(&directory.join(&manifest.system.file), &upload).and_then(|samples| {
        crate::elevenlabs::diarize(
            &upload,
            &selection.language,
            samples,
            crate::process_exit::quitting,
        )
        .map_err(|error| eyre!(error))
    });
    // Its only purpose was the upload, and the original track stays.
    let _ = fs::remove_file(&upload);
    assign(entries, &spans?, system_offset_ms(manifest));
    Ok(())
}

/// How far the system track's own time is from the transcript's, which starts
/// at whichever track started first.
fn system_offset_ms(manifest: &MeetingManifest) -> u64 {
    let origin = [
        manifest.system.first_arrival_us,
        manifest.microphone.first_arrival_us,
    ]
    .into_iter()
    .flatten()
    .min()
    .unwrap_or_default();
    manifest
        .system
        .first_arrival_us
        .unwrap_or_default()
        .saturating_sub(origin)
        / 1_000
}

/// Writes `track` as 16 kHz mono 16-bit, returning its samples. Positions stay
/// on the track's own timeline.
fn write_upload(track: &Path, upload: &Path) -> Result<usize> {
    let mut reader = WavReader::open(track)?;
    let source_rate = reader.spec().sample_rate;
    let mut resampler = StreamingResampler::new(source_rate).map_err(|error| eyre!(error))?;
    let mut writer = WavWriter::create(
        upload,
        WavSpec {
            channels: 1,
            sample_rate: UPLOAD_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        },
    )?;
    super::set_owner_only(upload, false)?;
    let mut samples = reader.samples::<f32>();
    let mut block = Vec::with_capacity(source_rate as usize);
    let mut written = 0;
    let mut write = |samples: &[f32]| -> Result<()> {
        for sample in samples {
            writer.write_sample((sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16)?;
        }
        written += samples.len();
        Ok(())
    };
    loop {
        block.clear();
        for sample in samples.by_ref().take(source_rate as usize) {
            block.push(sample?);
        }
        if block.is_empty() {
            break;
        }
        write(&resampler.push(&block))?;
    }
    write(&resampler.finish())?;
    writer.finalize()?;
    Ok(written)
}

/// Gives each `Them` entry the speaker it overlaps most, numbered by who spoke
/// first. One speaker alone adds nothing, so it stays `Them`, as does an entry
/// no speech overlaps.
fn assign(entries: &mut [TranscriptEntry], spans: &[SpeakerSpan], offset_ms: u64) {
    let mut order: Vec<&str> = Vec::new();
    let mut chosen = Vec::with_capacity(entries.len());
    for entry in entries.iter() {
        let speaker = (entry.source == MeetingSource::System)
            .then(|| {
                dominant_speaker(
                    entry.start_ms.saturating_sub(offset_ms),
                    entry.end_ms.saturating_sub(offset_ms),
                    spans,
                )
            })
            .flatten();
        if let Some(speaker) = speaker
            && !order.contains(&speaker)
        {
            order.push(speaker);
        }
        chosen.push(speaker);
    }
    if order.len() < 2 {
        return;
    }
    for (entry, speaker) in entries.iter_mut().zip(chosen) {
        entry.speaker = speaker
            .and_then(|speaker| order.iter().position(|known| *known == speaker))
            .and_then(|index| u16::try_from(index + 1).ok());
    }
}

fn dominant_speaker(start_ms: u64, end_ms: u64, spans: &[SpeakerSpan]) -> Option<&str> {
    let mut totals: Vec<(&str, u64)> = Vec::new();
    for span in spans {
        let overlap = end_ms
            .min(span.end_ms)
            .saturating_sub(start_ms.max(span.start_ms));
        if overlap == 0 {
            continue;
        }
        match totals
            .iter_mut()
            .find(|(speaker, _)| *speaker == span.speaker)
        {
            Some((_, total)) => *total += overlap,
            None => totals.push((&span.speaker, overlap)),
        }
    }
    // The first to reach the highest total wins a tie, so labels are stable.
    totals
        .into_iter()
        .rev()
        .max_by_key(|(_, total)| *total)
        .map(|(speaker, _)| speaker)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn them(start_ms: u64, end_ms: u64) -> TranscriptEntry {
        TranscriptEntry {
            source: MeetingSource::System,
            start_ms,
            end_ms,
            text: "words".into(),
            speaker: None,
        }
    }

    fn span(speaker: &str, start_ms: u64, end_ms: u64) -> SpeakerSpan {
        SpeakerSpan {
            speaker: speaker.into(),
            start_ms,
            end_ms,
        }
    }

    #[test]
    fn speakers_are_numbered_by_who_spoke_first() {
        let mut entries = vec![them(0, 1_000), them(1_000, 2_000), them(2_000, 3_000)];
        let spans = [
            span("speaker_7", 0, 1_000),
            span("speaker_2", 1_000, 2_000),
            span("speaker_7", 2_000, 3_000),
        ];
        assign(&mut entries, &spans, 0);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.speaker)
                .collect::<Vec<_>>(),
            [Some(1), Some(2), Some(1)]
        );
        assert_eq!(entries[1].label(), "Speaker 2");
    }

    #[test]
    fn the_speaker_with_the_most_overlap_wins() {
        let mut entries = vec![them(0, 1_000), them(2_000, 3_000)];
        let spans = [
            span("a", 0, 300),
            span("b", 300, 1_000),
            span("a", 2_000, 3_000),
        ];
        assign(&mut entries, &spans, 0);
        assert_eq!(entries[0].label(), "Speaker 1");
        assert_eq!(entries[1].label(), "Speaker 2");
    }

    #[test]
    fn your_own_lines_are_never_labeled() {
        let mut entries = vec![
            TranscriptEntry {
                source: MeetingSource::Microphone,
                ..them(0, 1_000)
            },
            them(1_000, 2_000),
            them(2_000, 3_000),
        ];
        let spans = [
            span("a", 0, 1_000),
            span("a", 1_000, 2_000),
            span("b", 2_000, 3_000),
        ];
        assign(&mut entries, &spans, 0);
        assert_eq!(entries[0].speaker, None);
        assert_eq!(entries[0].label(), "You");
    }

    #[test]
    fn one_remote_speaker_stays_them() {
        let mut entries = vec![them(0, 1_000), them(1_000, 2_000)];
        assign(&mut entries, &[span("a", 0, 2_000)], 0);
        assert!(entries.iter().all(|entry| entry.speaker.is_none()));
        assert_eq!(entries[0].label(), "Them");
    }

    #[test]
    fn a_line_no_speech_overlaps_stays_them() {
        let mut entries = vec![them(0, 1_000), them(1_000, 2_000), them(9_000, 9_500)];
        let spans = [span("a", 0, 1_000), span("b", 1_000, 2_000)];
        assign(&mut entries, &spans, 0);
        assert_eq!(entries[2].speaker, None);
    }

    #[test]
    fn a_late_system_track_is_matched_on_its_own_timeline() {
        // The system track started 5 s after the microphone.
        let mut entries = vec![them(5_000, 6_000), them(6_000, 7_000)];
        let spans = [span("a", 0, 1_000), span("b", 1_000, 2_000)];
        assign(&mut entries, &spans, 5_000);
        assert_eq!(entries[0].label(), "Speaker 1");
        assert_eq!(entries[1].label(), "Speaker 2");
    }

    #[test]
    fn labels_change_turns_but_not_the_text() {
        let mut entries = vec![them(0, 1_000), them(1_000, 2_000)];
        assign(
            &mut entries,
            &[span("a", 0, 1_000), span("b", 1_000, 2_000)],
            0,
        );
        let turns = super::super::coalesce_transcript(entries);
        assert_eq!(turns.len(), 2, "two speakers are two turns");
        assert!(turns.iter().all(|turn| turn.text == "words"));
    }

    #[test]
    fn the_upload_is_16_khz_mono_and_the_same_length() {
        let directory = std::env::temp_dir().join(format!(
            "hex-speakers-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let track = directory.join("system.wav");
        let mut writer = WavWriter::create(
            &track,
            WavSpec {
                channels: 1,
                sample_rate: 48_000,
                bits_per_sample: 32,
                sample_format: SampleFormat::Float,
            },
        )
        .unwrap();
        for index in 0..96_000 {
            writer
                .write_sample((index as f32 * 0.01).sin() * 0.5)
                .unwrap();
        }
        writer.finalize().unwrap();
        let upload = directory.join(UPLOAD_FILE);
        let samples = write_upload(&track, &upload).unwrap();
        let reader = WavReader::open(&upload).unwrap();
        let spec = reader.spec();
        assert_eq!(
            (spec.channels, spec.sample_rate, spec.bits_per_sample),
            (1, 16_000, 16)
        );
        assert_eq!(reader.duration() as usize, samples);
        assert!((31_000..=33_000).contains(&samples), "{samples}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&upload).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "owner only");
        }
        fs::remove_dir_all(directory).unwrap();
    }

    /// Costs a little: uploads `HEX_LIVE_DIARIZE_WAV` with the configured key.
    #[test]
    #[ignore = "uploads to ElevenLabs; needs a key and HEX_LIVE_DIARIZE_WAV"]
    fn live_diarization_tells_two_voices_apart() {
        let path = std::env::var("HEX_LIVE_DIARIZE_WAV").expect("HEX_LIVE_DIARIZE_WAV");
        let samples = WavReader::open(&path).unwrap().duration() as usize;
        let spans =
            crate::elevenlabs::diarize(Path::new(&path), "auto", samples, || false).unwrap();
        let mut speakers: Vec<&str> = spans.iter().map(|span| span.speaker.as_str()).collect();
        speakers.sort_unstable();
        speakers.dedup();
        eprintln!("DIARIZE {} turns, speakers {speakers:?}", spans.len());
        assert!(speakers.len() >= 2, "{speakers:?}");
    }
}
