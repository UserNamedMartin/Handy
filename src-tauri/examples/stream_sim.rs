//! End-to-end OFFLINE simulation of "whisper-as-its-own-segmenter" streaming
//! (commit-on-sentence-boundary). NOT part of the app — a faithful dry run of
//! the proposed worker so we can validate quality + real-time feasibility on
//! saved audio before touching the live app.
//!
//! Per clip it replays the audio as 30 ms frames and runs the commit loop:
//!   - accumulate frames into an uncommitted buffer;
//!   - at a pause (RMS) / periodically / on overflow, run whisper on the buffer
//!     WITH segment timestamps;
//!   - commit every segment up to the last one whose text ends a sentence
//!     (. ? !) and that ended >= STABILITY_MS before the buffer end (so it won't
//!     change); drop that audio, keep the tail;
//!   - at stop, transcribe the remaining tail.
//! Emits JSONL: streamed text, #runs, total compute vs duration (real-time
//! feasibility), and perceived latency (the final tail run).
//!
//! Env: MODEL_PATH, CORPUS_DIR, OUT_PATH, PRIMER (optional).

use anyhow::Result;
use std::io::Write;
use std::path::Path;
use std::time::Instant;
use transcribe_cpp::{
    Backend, Model, ModelOptions, RunExtension, RunOptions, Task, TimestampKind, WhisperRunOptions,
};

const FRAME: usize = 480; // 30 ms @ 16 kHz
const PAUSE_RMS: f32 = 0.003;
const PAUSE_MS: usize = 500; // contiguous silence that triggers a commit run
const PERIODIC_MS: usize = 8000; // also run this often even without a pause
const MAX_BUFFER_MS: usize = 25000; // force a run if the buffer grows past this
const STABILITY_MS: i64 = 800; // a segment must end this long before buffer-end to commit
const MIN_RUN_MS: usize = 1200; // don't bother running on a tiny buffer

fn read_wav_16k_mono(path: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<std::result::Result<_, _>>()?
        }
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<_, _>>()?,
    };
    if spec.channels == 2 {
        Ok(raw.iter().step_by(2).copied().collect())
    } else {
        Ok(raw)
    }
}

fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

fn ends_sentence(s: &str) -> bool {
    s.trim_end()
        .chars()
        .last()
        .map(|c| ".?!…".contains(c))
        .unwrap_or(false)
}

fn main() -> Result<()> {
    let model_path = std::env::var("MODEL_PATH").expect("MODEL_PATH");
    let corpus = std::env::var("CORPUS_DIR").expect("CORPUS_DIR");
    let out_path = std::env::var("OUT_PATH").expect("OUT_PATH");
    let primer = std::env::var("PRIMER").ok().filter(|s| !s.is_empty());

    eprintln!("loading {model_path} (metal)…");
    let model = Model::load_with(
        Path::new(&model_path),
        &ModelOptions { backend: Backend::Metal, gpu_device: 0 },
    )?;
    let mut session = model.session()?;

    let opts = || RunOptions {
        task: Task::Transcribe,
        language: None,
        target_language: None,
        timestamps: TimestampKind::Segment,
        family: Some(RunExtension::Whisper(WhisperRunOptions {
            initial_prompt: primer.clone(),
            condition_on_prev_tokens: Some(false),
            ..Default::default()
        })),
        ..Default::default()
    };

    let mut clips: Vec<_> = std::fs::read_dir(&corpus)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    clips.sort();
    if let Some(parent) = Path::new(&out_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = std::fs::File::create(&out_path)?;

    for (ci, dir) in clips.iter().enumerate() {
        let cid = dir.file_name().unwrap().to_string_lossy().to_string();
        let wav = dir.join("raw.wav");
        if !wav.exists() {
            continue;
        }
        let audio = read_wav_16k_mono(&wav)?;
        let dur_ms = audio.len() * 1000 / 16000;

        let mut buffer: Vec<f32> = Vec::new();
        let mut committed: Vec<String> = Vec::new();
        let mut trailing_sil = 0usize;
        let mut saw_speech = false;
        let mut since_run = 0usize;
        let mut n_runs = 0usize;
        let mut total_compute_ms = 0u128;

        let mut maybe_commit = |session: &mut transcribe_cpp::Session,
                                buffer: &mut Vec<f32>,
                                committed: &mut Vec<String>,
                                n_runs: &mut usize,
                                total: &mut u128|
         -> Result<()> {
            let t0 = Instant::now();
            let tr = session.run(buffer, &opts())?;
            *total += t0.elapsed().as_millis();
            *n_runs += 1;
            let buf_ms = (buffer.len() * 1000 / 16000) as i64;
            // last stable sentence-ending segment
            let mut cut: Option<(usize, i64)> = None;
            for (i, seg) in tr.segments.iter().enumerate() {
                if ends_sentence(&seg.text) && seg.t1_ms <= buf_ms - STABILITY_MS {
                    cut = Some((i, seg.t1_ms));
                }
            }
            if let Some((k, t1)) = cut {
                let text: String = tr.segments[..=k]
                    .iter()
                    .map(|s| s.text.trim())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !text.trim().is_empty() {
                    committed.push(text.trim().to_string());
                }
                let cut_samples = ((t1 as usize) * 16).min(buffer.len());
                buffer.drain(..cut_samples);
            }
            Ok(())
        };

        for frame in audio.chunks(FRAME) {
            buffer.extend_from_slice(frame);
            let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32).sqrt();
            if rms > PAUSE_RMS {
                saw_speech = true;
                trailing_sil = 0;
            } else {
                trailing_sil += 1;
            }
            since_run += 1;
            let buf_ms = buffer.len() * 1000 / 16000;
            let paused = saw_speech && trailing_sil * 30 >= PAUSE_MS;
            let periodic = since_run * 30 >= PERIODIC_MS;
            let overflow = buf_ms >= MAX_BUFFER_MS;
            if buf_ms >= MIN_RUN_MS && (paused || periodic || overflow) {
                since_run = 0;
                maybe_commit(
                    &mut session,
                    &mut buffer,
                    &mut committed,
                    &mut n_runs,
                    &mut total_compute_ms,
                )?;
            }
        }

        // Flush the remaining tail — this is the only run the user waits on.
        let perceived_ms;
        {
            let t0 = Instant::now();
            let tr = session.run(&buffer, &opts())?;
            perceived_ms = t0.elapsed().as_millis();
            total_compute_ms += perceived_ms;
            n_runs += 1;
            let tail = tr.text.trim();
            if !tail.is_empty() {
                committed.push(tail.to_string());
            }
        }

        let streamed = committed.join(" ");
        writeln!(
            out,
            "{{\"clip_id\":\"{}\",\"dur_ms\":{},\"n_runs\":{},\"total_compute_ms\":{},\"compute_rt\":{:.3},\"perceived_ms\":{},\"n_commits\":{},\"text\":\"{}\"}}",
            cid,
            dur_ms,
            n_runs,
            total_compute_ms,
            total_compute_ms as f64 / dur_ms.max(1) as f64,
            perceived_ms,
            committed.len(),
            json_escape(&streamed)
        )?;
        eprintln!(
            "[{}/{}] {} dur={}s runs={} compute={:.2}xRT perceived={}ms commits={}",
            ci + 1,
            clips.len(),
            cid,
            dur_ms / 1000,
            n_runs,
            total_compute_ms as f64 / dur_ms.max(1) as f64,
            perceived_ms,
            committed.len()
        );
    }
    eprintln!("wrote {out_path}");
    Ok(())
}
