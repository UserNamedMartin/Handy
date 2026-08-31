//! Find a VAD-gate threshold (applied when auto-gain boosts) that KILLS the
//! silence-hallucination while keeping real whispered speech intact.
//! NOT part of the app. Run from `src-tauri/`:
//!   cargo run --release --example whisper_gate_tune
//!
//! For each threshold T: boosted silence/room-tone → gate(T) → Whisper should
//! yield ~0 words; boosted real whisper → gate(T) → Whisper should keep low WER.
//! The operating point is the lowest T where silence goes quiet and whisper WER
//! is still good.

use anyhow::Result;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use transcribe_cpp::{
    Backend, Model, ModelOptions, RunExtension, RunOptions, Task, WhisperRunOptions,
};
use vad_rs::Vad;

const MODEL_PATH: &str = "/Users/martinmourzenkov/.cache/huggingface/hub/models--handy-computer--whisper-large-v3-gguf/snapshots/e3e29bee6389c7da4a141406f07bb80ddac5337c/whisper-large-v3-Q5_K_M.gguf";
const CORPUS: &str = "examples/whisper_corpus";
const STRESS: &str = "examples/whisper_stress";
const SILERO_PATH: &str = "resources/models/silero_vad_v4.onnx";
const SR: usize = 16_000;
const FRAME: usize = 480;
const BOOST_TARGET_DBFS: f32 = -3.0;
const THRESHOLDS: [f32; 4] = [0.3, 0.5, 0.7, 0.85];

const REF_A: &str = "Слушай, в какой директории ты сейчас находишься? Чекни, пожалуйста, есть ли у тебя доступ к этому. Так, короче, давай сначала откатим вот эту хуйню, которую ты сделал, а потом уже посмотрим. Окей, погнали. И кстати, увеличь мне, пожалуйста, в настройках размер хранилища до двухсот. Подожди, я нихуя не понял, нахуя мне и это, и это — объясни простыми словами.";
const REF_B: &str = "Окей, изучи, пожалуйста, как у меня сейчас работает сетап для двух компаний в Клоде, и заодно как работают кастомные шрифты. Я вообще нахуй не использую CLI, я пользуюсь только десктопным приложением. Смотри, я хочу запускать это через обычные иконки в док-панели, чтобы у меня было две иконки Клода, и я мог просто запустить какую-то определённую. По сути, это должно выглядеть как два разных приложения, а по факту under the hood, наверное, из-за памяти, это может быть одно приложение — мне похуй, как ты это сделаешь. Можно так сделать или нет?";

fn read_wav_mono(path: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    Ok(match spec.sample_format {
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>().map(|s| s.map(|v| v as f32 / max)).collect::<std::result::Result<_, _>>()?
        }
        hound::SampleFormat::Float => r.samples::<f32>().collect::<std::result::Result<_, _>>()?,
    })
}
fn peak(s: &[f32]) -> f32 { s.iter().fold(0.0f32, |m, x| m.max(x.abs())) }
fn db(x: f32) -> f32 { if x > 0.0 { 20.0 * x.log10() } else { -99.0 } }

fn boost_to(s: &[f32], target_dbfs: f32) -> Vec<f32> {
    let g = 10f32.powf((target_dbfs - db(peak(s))) / 20.0);
    s.iter().map(|&x| (x * g).clamp(-1.0, 1.0)).collect()
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
        if chunk.len() < FRAME { break; }
        frame.copy_from_slice(chunk);
        buffer.push_back(frame.to_vec());
        while buffer.len() > PREFILL + 1 { buffer.pop_front(); }
        let voice = vad.compute(&frame).map(|r| r.prob > threshold).unwrap_or(false);
        match (in_speech, voice) {
            (false, true) => { onset_c += 1; if onset_c >= ONSET { in_speech = true; hangover = HANGOVER; onset_c = 0; for b in &buffer { out.extend_from_slice(b); } } }
            (true, true) => { hangover = HANGOVER; out.extend_from_slice(&frame); }
            (true, false) => { if hangover > 0 { hangover -= 1; out.extend_from_slice(&frame); } else { in_speech = false; } }
            (false, false) => onset_c = 0,
        }
    }
    out
}
fn normalize(t: &str) -> Vec<String> {
    t.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect::<String>()
        .split_whitespace().map(|w| w.to_string()).collect()
}
fn wer(r: &str, h: &str) -> f32 {
    let r = normalize(r); let h = normalize(h);
    if r.is_empty() { return if h.is_empty() { 0.0 } else { 1.0 }; }
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    let mut cur = vec![0usize; h.len() + 1];
    for i in 1..=r.len() { cur[0] = i; for j in 1..=h.len() {
        let c = if r[i-1]==h[j-1] {0} else {1}; cur[j]=(prev[j]+1).min(cur[j-1]+1).min(prev[j-1]+c);
    } std::mem::swap(&mut prev, &mut cur); }
    prev[h.len()] as f32 / r.len() as f32
}
fn opts() -> RunOptions {
    RunOptions { task: Task::Transcribe, language: None, target_language: None,
        family: Some(RunExtension::Whisper(WhisperRunOptions { condition_on_prev_tokens: Some(false), ..Default::default() })), ..Default::default() }
}
fn gated_text(sess: &mut transcribe_cpp::Session, vad: &mut Vad, a: &[f32], t: f32) -> String {
    let g = vad_gate(vad, a, t);
    if g.len() < 1600 { String::new() } else { sess.run(&g, &opts()).map(|x| x.text).unwrap_or_default() }
}
fn list(dir: &str, pfx: &str) -> Vec<PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with(pfx)).unwrap_or(false)).collect();
    v.sort(); v
}

fn main() -> Result<()> {
    eprintln!("loading…");
    let model = Model::load_with(Path::new(MODEL_PATH), &ModelOptions { backend: Backend::Metal, device: None })?;
    let mut s = model.session()?;
    let mut vad = Vad::new(SILERO_PATH, SR).map_err(|e| anyhow::anyhow!("vad: {e}"))?;

    // all clips boosted to -3 dBFS first (the whisper-mode boost)
    println!("\n████ SILENCE — phantom words after boost, by gate threshold (want 0) ████");
    print!("{:<34}", "clip");
    for t in THRESHOLDS { print!("{:>8}", format!("T={t}")); }
    println!();
    for p in list(STRESS, "sil_") {
        let a = boost_to(&read_wav_mono(&p)?, BOOST_TARGET_DBFS);
        print!("{:<34}", p.file_name().unwrap().to_string_lossy());
        for t in THRESHOLDS { print!("{:>8}", normalize(&gated_text(&mut s, &mut vad, &a, t)).len()); }
        println!();
    }

    println!("\n████ REAL WHISPER — WER after boost, by gate threshold (want low) ████");
    print!("{:<34}", "clip");
    for t in THRESHOLDS { print!("{:>8}", format!("T={t}")); }
    println!();
    let mut whisper_clips = list(CORPUS, "whisper_");
    whisper_clips.retain(|p| { let n = p.file_name().unwrap().to_string_lossy().to_string(); n.contains("_natural_") || n.contains("_faint_") });
    for p in whisper_clips {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let reference = if name.contains("_A_") { REF_A } else { REF_B };
        let a = boost_to(&read_wav_mono(&p)?, BOOST_TARGET_DBFS);
        print!("{:<34}", name);
        for t in THRESHOLDS { print!("{:>7.0}%", wer(reference, &gated_text(&mut s, &mut vad, &a, t)) * 100.0); }
        println!();
    }
    println!("\n(pick the lowest T where SILENCE column is ~0 and WHISPER WER stays low.)");
    Ok(())
}
