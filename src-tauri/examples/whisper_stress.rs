//! Stress test for the proposed CONDITIONAL auto-gain (whisper-detect by level).
//! NOT part of the app. Run from `src-tauri/`:
//!   cargo run --release --example whisper_stress
//!
//! Auto-gain rule under test: measure clip RMS; if it's already loud enough
//! (>= WHISPER_LEVEL_DBFS) it's normal voice → passthrough (untouched); else
//! it's a whisper → peak-normalize to BOOST_TARGET_DBFS. Then the app's VAD
//! gate (0.3) + Whisper. Two questions:
//!   1. Silence/room-tone (no speech) → does gained near-silence hallucinate?
//!   2. Whisper + background noise → does WER hold, does level-detect still fire?

use anyhow::Result;
use std::collections::VecDeque;
use std::path::Path;
use transcribe_cpp::{
    Backend, Model, ModelOptions, RunExtension, RunOptions, Task, WhisperRunOptions,
};
use vad_rs::Vad;

const MODEL_PATH: &str = "/Users/martinmourzenkov/.cache/huggingface/hub/models--handy-computer--whisper-large-v3-gguf/snapshots/e3e29bee6389c7da4a141406f07bb80ddac5337c/whisper-large-v3-Q5_K_M.gguf";
const STRESS_DIR: &str = "examples/whisper_stress";
const SILERO_PATH: &str = "resources/models/silero_vad_v4.onnx";
const SR: usize = 16_000;
const FRAME: usize = 480;
const GATE_THRESHOLD: f32 = 0.3;

// The auto-gain design under test:
const WHISPER_LEVEL_DBFS: f32 = -45.0; // below this RMS ⇒ treat as whisper
const BOOST_TARGET_DBFS: f32 = -3.0; // peak-normalize target for whispers

const REF_A: &str = "Слушай, в какой директории ты сейчас находишься? Чекни, пожалуйста, есть ли у тебя доступ к этому. Так, короче, давай сначала откатим вот эту хуйню, которую ты сделал, а потом уже посмотрим. Окей, погнали. И кстати, увеличь мне, пожалуйста, в настройках размер хранилища до двухсот. Подожди, я нихуя не понял, нахуя мне и это, и это — объясни простыми словами.";
const REF_B: &str = "Окей, изучи, пожалуйста, как у меня сейчас работает сетап для двух компаний в Клоде, и заодно как работают кастомные шрифты. Я вообще нахуй не использую CLI, я пользуюсь только десктопным приложением. Смотри, я хочу запускать это через обычные иконки в док-панели, чтобы у меня было две иконки Клода, и я мог просто запустить какую-то определённую. По сути, это должно выглядеть как два разных приложения, а по факту under the hood, наверное, из-за памяти, это может быть одно приложение — мне похуй, как ты это сделаешь. Можно так сделать или нет?";

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
    Ok(raw)
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

/// The conditional auto-gain. Returns (audio, applied_dB, classified_as_whisper).
fn auto_gain(s: &[f32]) -> (Vec<f32>, f32, bool) {
    let lvl = db(rms(s));
    if lvl >= WHISPER_LEVEL_DBFS {
        (s.to_vec(), 0.0, false) // normal/loud → untouched
    } else {
        let g = BOOST_TARGET_DBFS - db(peak(s));
        let gain = 10f32.powf(g / 20.0);
        (s.iter().map(|&x| (x * gain).clamp(-1.0, 1.0)).collect(), g, true)
    }
}

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

fn transcribe_gated(session: &mut transcribe_cpp::Session, vad: &mut Vad, audio: &[f32]) -> String {
    let gated = vad_gate(vad, audio, GATE_THRESHOLD);
    if gated.len() < 1600 {
        String::new()
    } else {
        session.run(&gated, &whisper_opts()).map(|t| t.text).unwrap_or_default()
    }
}

fn list(dir: &str, prefix: &str) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with(prefix))
                .unwrap_or(false)
        })
        .collect();
    v.sort();
    v
}

fn main() -> Result<()> {
    eprintln!("loading whisper…");
    let model = Model::load_with(
        Path::new(MODEL_PATH),
        &ModelOptions {
            backend: Backend::Metal,
            gpu_device: 0,
        },
    )?;
    let mut session = model.session()?;
    let mut vad = Vad::new(SILERO_PATH, SR).map_err(|e| anyhow::anyhow!("vad: {e}"))?;

    // ── TEST 1: hallucination on silence / room tone ──
    println!("\n████ TEST 1 — hallucination on non-speech (want 0 words) ████");
    println!("{:<34} {:>6} {:>12} {:>12}", "clip", "gain", "raw words", "gained words");
    for p in list(STRESS_DIR, "sil_") {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let audio = read_wav_mono(&p)?;
        let raw_txt = transcribe_gated(&mut session, &mut vad, &audio);
        let (gained, g, _) = auto_gain(&audio);
        let gained_txt = transcribe_gated(&mut session, &mut vad, &gained);
        let rawn = normalize(&raw_txt).len();
        let gn = normalize(&gained_txt).len();
        println!(
            "{:<34} {:>+5.0}dB {:>12} {:>12}",
            name, g, rawn, gn
        );
        if gn > 0 {
            println!("     ⚠ phantom: {:?}", gained_txt.trim());
        }
    }

    // ── TEST 2: whisper + background noise ──
    println!("\n████ TEST 2 — whisper + noise, end-to-end WER (auto-gain → VAD@0.3 → whisper) ████");
    println!(
        "{:<28} {:>7} {:>8} {:>10} {:>10}",
        "clip", "class", "gain", "WER raw", "WER auto"
    );
    for p in list(STRESS_DIR, "noise_") {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let reference = if name.contains("_A_") { REF_A } else { REF_B };
        let audio = read_wav_mono(&p)?;

        let raw_wer = wer(reference, &transcribe_gated(&mut session, &mut vad, &audio));
        let (gained, g, is_whisper) = auto_gain(&audio);
        let auto_wer = wer(reference, &transcribe_gated(&mut session, &mut vad, &gained));
        println!(
            "{:<28} {:>7} {:>+6.0}dB {:>9.0}% {:>9.0}%",
            name,
            if is_whisper { "whisper" } else { "normal" },
            g,
            raw_wer * 100.0,
            auto_wer * 100.0,
        );
    }
    println!("\n(TEST1: gained words should be ~0 — else gain makes Whisper hallucinate on silence.");
    println!(" TEST2: 'class' must say whisper (level-detect fired); WER auto ≪ WER raw = noise survived.)");
    Ok(())
}
