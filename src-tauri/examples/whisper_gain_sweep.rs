//! Whisper-mode gain/VAD sweep harness — data-driven tuning for capturing
//! whispered (quiet) speech. NOT part of the app. Run from `src-tauri/`:
//!
//!   cargo run --release --example whisper_gain_sweep
//!
//! For every recording in `examples/whisper_corpus/` (16 kHz mono WAV, named
//! `whisper_<script>_<cond>_take<N>.wav`) it sweeps a set of input-gain
//! strategies and, per strategy, reports:
//!   • WER vs the known reference script (the quality number),
//!   • Silero VAD pass-rate at several thresholds (would the gate pass it?),
//!   • post-gain signal stats (RMS / peak / clipping).
//! Everything upstream of Whisper (gain in dB, Silero threshold) transfers 1:1
//! into Handy, so the winning numbers can be shipped as-is.

use anyhow::Result;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use transcribe_cpp::{
    Backend, Model, ModelOptions, RunExtension, RunOptions, Task, WhisperRunOptions,
};
use vad_rs::Vad;

const MODEL_PATH: &str = "/Users/martinmourzenkov/.cache/huggingface/hub/models--handy-computer--whisper-large-v3-gguf/snapshots/e3e29bee6389c7da4a141406f07bb80ddac5337c/whisper-large-v3-Q5_K_M.gguf";
const CORPUS_DIR: &str = "examples/whisper_corpus";
const SILERO_PATH: &str = "resources/models/silero_vad_v4.onnx";
const SR: usize = 16_000;
const FRAME: usize = 480; // 30 ms @ 16 kHz — same as the app's SileroVad

// Locked reference scripts (must match what was read verbatim).
const REF_A: &str = "Слушай, в какой директории ты сейчас находишься? Чекни, пожалуйста, есть ли у тебя доступ к этому. Так, короче, давай сначала откатим вот эту хуйню, которую ты сделал, а потом уже посмотрим. Окей, погнали. И кстати, увеличь мне, пожалуйста, в настройках размер хранилища до двухсот. Подожди, я нихуя не понял, нахуя мне и это, и это — объясни простыми словами.";
const REF_B: &str = "Окей, изучи, пожалуйста, как у меня сейчас работает сетап для двух компаний в Клоде, и заодно как работают кастомные шрифты. Я вообще нахуй не использую CLI, я пользуюсь только десктопным приложением. Смотри, я хочу запускать это через обычные иконки в док-панели, чтобы у меня было две иконки Клода, и я мог просто запустить какую-то определённую. По сути, это должно выглядеть как два разных приложения, а по факту under the hood, наверное, из-за памяти, это может быть одно приложение — мне похуй, как ты это сделаешь. Можно так сделать или нет?";

/* ── audio ─────────────────────────────────────────────────────────── */

fn read_wav_mono(path: &Path) -> Result<Vec<f32>> {
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

fn rms(s: &[f32]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
}
fn peak(s: &[f32]) -> f32 {
    s.iter().fold(0.0f32, |m, x| m.max(x.abs()))
}
fn db(x: f32) -> f32 {
    if x > 0.0 {
        20.0 * x.log10()
    } else {
        -99.0
    }
}

/// Apply a linear gain (dB), hard-clip to [-1, 1]. Returns (gained, clip_count).
fn gain_db(s: &[f32], gain_db: f32) -> (Vec<f32>, usize) {
    let g = 10f32.powf(gain_db / 20.0);
    let mut clips = 0;
    let out = s
        .iter()
        .map(|&x| {
            let y = x * g;
            if y > 1.0 || y < -1.0 {
                clips += 1;
            }
            y.clamp(-1.0, 1.0)
        })
        .collect();
    (out, clips)
}

#[derive(Clone)]
enum Strategy {
    Raw,
    FixedDb(f32),
    PeakTo(f32), // peak-normalize to target dBFS (never clips)
    RmsTo(f32),  // RMS-normalize to target dBFS (may clip)
}

impl Strategy {
    fn label(&self) -> String {
        match self {
            Strategy::Raw => "raw (0 dB)".into(),
            Strategy::FixedDb(d) => format!("fixed +{d:.0} dB"),
            Strategy::PeakTo(t) => format!("peak→{t:.0} dBFS"),
            Strategy::RmsTo(t) => format!("rms→{t:.0} dBFS"),
        }
    }
    fn apply(&self, s: &[f32]) -> (Vec<f32>, f32, usize) {
        let g = match self {
            Strategy::Raw => 0.0,
            Strategy::FixedDb(d) => *d,
            Strategy::PeakTo(t) => t - db(peak(s)),
            Strategy::RmsTo(t) => t - db(rms(s)),
        };
        let (out, clips) = gain_db(s, g);
        (out, g, clips)
    }
}

/* ── VAD ───────────────────────────────────────────────────────────── */

/// Fraction of 30 ms frames whose Silero speech-prob exceeds each threshold,
/// plus the mean prob. Processes frames in order (LSTM is stateful).
fn vad_passrate(vad: &mut Vad, s: &[f32], thresholds: &[f32]) -> (Vec<f32>, f32) {
    vad.reset();
    let mut probs = Vec::new();
    let mut frame = [0f32; FRAME];
    for chunk in s.chunks(FRAME) {
        if chunk.len() < FRAME {
            break;
        }
        frame.copy_from_slice(chunk);
        if let Ok(r) = vad.compute(&frame) {
            probs.push(r.prob);
        }
    }
    if probs.is_empty() {
        return (thresholds.iter().map(|_| 0.0).collect(), 0.0);
    }
    let rates = thresholds
        .iter()
        .map(|&t| probs.iter().filter(|&&p| p > t).count() as f32 / probs.len() as f32)
        .collect();
    let mean = probs.iter().sum::<f32>() / probs.len() as f32;
    (rates, mean)
}

/// End-to-end VAD gate: replicates the app's SmoothedVad (offline policy) —
/// threshold 0.3, prefill=15, onset=2, hangover=15 frames. Returns ONLY the
/// audio that survives the gate, i.e. exactly what Whisper would receive in
/// the real app.
fn vad_gate(vad: &mut Vad, s: &[f32], threshold: f32) -> Vec<f32> {
    const PREFILL: usize = 15;
    const ONSET: usize = 2;
    const HANGOVER: usize = 15;
    vad.reset();
    let mut buffer: VecDeque<Vec<f32>> = VecDeque::new();
    let mut out: Vec<f32> = Vec::new();
    let (mut in_speech, mut hangover, mut onset_c) = (false, 0usize, 0usize);
    let mut frame = [0f32; FRAME];
    for chunk in s.chunks(FRAME) {
        if chunk.len() < FRAME {
            break;
        }
        frame.copy_from_slice(chunk);
        buffer.push_back(frame.to_vec());
        while buffer.len() > PREFILL + 1 {
            buffer.pop_front();
        }
        let voice = vad.compute(&frame).map(|r| r.prob > threshold).unwrap_or(false);
        match (in_speech, voice) {
            (false, true) => {
                onset_c += 1;
                if onset_c >= ONSET {
                    in_speech = true;
                    hangover = HANGOVER;
                    onset_c = 0;
                    for b in &buffer {
                        out.extend_from_slice(b);
                    }
                }
            }
            (true, true) => {
                hangover = HANGOVER;
                out.extend_from_slice(&frame);
            }
            (true, false) => {
                if hangover > 0 {
                    hangover -= 1;
                    out.extend_from_slice(&frame);
                } else {
                    in_speech = false;
                }
            }
            (false, false) => onset_c = 0,
        }
    }
    out
}

/* ── WER ───────────────────────────────────────────────────────────── */

fn normalize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(|w| w.to_string())
        .collect()
}

fn wer(reference: &str, hypothesis: &str) -> f32 {
    let r = normalize(reference);
    let h = normalize(hypothesis);
    if r.is_empty() {
        return if h.is_empty() { 0.0 } else { 1.0 };
    }
    // word-level Levenshtein
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    let mut cur = vec![0usize; h.len() + 1];
    for i in 1..=r.len() {
        cur[0] = i;
        for j in 1..=h.len() {
            let cost = if r[i - 1] == h[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[h.len()] as f32 / r.len() as f32
}

fn reference_for(name: &str) -> &'static str {
    if name.contains("_A_") {
        REF_A
    } else {
        REF_B
    }
}
fn condition_of(name: &str) -> &'static str {
    if name.contains("_faint_") {
        "faint"
    } else if name.contains("_normal_") {
        "normal"
    } else {
        "natural"
    }
}

fn whisper_opts() -> RunOptions {
    RunOptions {
        task: Task::Transcribe,
        language: None,
        target_language: None,
        family: Some(RunExtension::Whisper(WhisperRunOptions {
            condition_on_prev_tokens: Some(false),
            ..Default::default()
        })),
        ..Default::default()
    }
}

/* ── main ──────────────────────────────────────────────────────────── */

fn main() -> Result<()> {
    const GATE_THRESHOLD: f32 = 0.3; // the app's default VAD threshold
    let strategies = [
        Strategy::Raw,
        Strategy::PeakTo(-3.0),
        Strategy::PeakTo(-1.0),
        Strategy::RmsTo(-23.0),
    ];

    eprintln!("loading whisper (metal)…");
    let model = Model::load_with(
        Path::new(MODEL_PATH),
        &ModelOptions {
            backend: Backend::Metal,
            gpu_device: 0,
        },
    )?;
    let mut session = model.session()?;
    eprintln!("loading silero…");
    let mut vad = Vad::new(SILERO_PATH, SR).map_err(|e| anyhow::anyhow!("vad: {e}"))?;

    let mut wavs: Vec<PathBuf> = std::fs::read_dir(CORPUS_DIR)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "wav").unwrap_or(false))
        .collect();
    wavs.sort();
    eprintln!("{} recordings\n", wavs.len());

    // aggregate: (condition, strategy_idx) -> (full_wer_sum, gated_wer_sum, count)
    let mut agg: HashMap<(String, usize), (f32, f32, usize)> = HashMap::new();

    for wav in &wavs {
        let name = wav.file_name().unwrap().to_string_lossy().to_string();
        let audio = read_wav_mono(wav)?;
        let reference = reference_for(&name);
        let cond = condition_of(&name);
        let total_s = audio.len() as f32 / SR as f32;
        println!("\n════════════════════════════════════════════════════════════");
        println!(
            "{name}   ({total_s:.1}s, raw RMS={:.1} peak={:.1} dBFS)",
            db(rms(&audio)),
            db(peak(&audio))
        );
        println!(
            "{:<14} {:>5} {:>5} {:>4} {:>9} {:>9} {:>10}",
            "strategy", "gain", "clip", "@.3", "WER(full)", "WER(gate)", "kept"
        );
        for (si, strat) in strategies.iter().enumerate() {
            let (gained, applied_db, clips) = strat.apply(&audio);

            // (a) model ceiling: Whisper on the full (ungated) audio
            let full = session.run(&gained, &whisper_opts())?;
            let wer_full = wer(reference, &full.text);

            // (b) end-to-end: gate at 0.3 like the app, then Whisper on survivors
            let gated = vad_gate(&mut vad, &gained, GATE_THRESHOLD);
            let kept_s = gated.len() as f32 / SR as f32;
            let hyp_gated = if gated.len() >= 1600 {
                session.run(&gated, &whisper_opts())?.text
            } else {
                String::new() // gate ate everything → app would output nothing
            };
            let wer_gate = wer(reference, &hyp_gated);

            // pass-rate at the app threshold (diagnostic)
            let (rates, _) = vad_passrate(&mut vad, &gained, &[GATE_THRESHOLD]);

            let e = agg.entry((cond.to_string(), si)).or_insert((0.0, 0.0, 0));
            e.0 += wer_full;
            e.1 += wer_gate;
            e.2 += 1;
            println!(
                "{:<14} {:>+5.0} {:>5} {:>3.0}% {:>8.0}% {:>8.0}% {:>6.1}s/{:.0}%",
                strat.label(),
                applied_db,
                clips,
                rates[0] * 100.0,
                wer_full * 100.0,
                wer_gate * 100.0,
                kept_s,
                kept_s / total_s * 100.0,
            );
        }
    }

    // ── aggregate: mean end-to-end (gated) WER per condition × strategy ──
    println!("\n\n████ MEAN WER (%) — end-to-end, after VAD gate @0.3 (= real app) ████");
    print!("{:<14}", "strategy");
    for c in ["natural", "faint", "normal"] {
        print!("{:>10}", c);
    }
    println!("      (full-audio WER in parens = model ceiling)");
    for (si, strat) in strategies.iter().enumerate() {
        print!("{:<14}", strat.label());
        for c in ["natural", "faint", "normal"] {
            match agg.get(&(c.to_string(), si)) {
                Some((full, gate, n)) if *n > 0 => print!(
                    "{:>6.0}% ({:.0})",
                    gate / *n as f32 * 100.0,
                    full / *n as f32 * 100.0
                ),
                _ => print!("{:>10}", "—"),
            }
        }
        println!();
    }
    println!("\n(lower = better. WER(gate) = what Handy actually produces today.");
    println!(" raw vs gained on the whisper rows is the whole story.)");
    Ok(())
}
