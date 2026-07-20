//! Does whisper.cpp's decoder-level anti-hallucination (no_speech_thold /
//! logprob_thold) kill phantom text on boosted non-speech WITHOUT hurting real
//! whispered speech? Gate fixed at 0.3 (best whisper WER). NOT part of the app.
//!   cargo run --release --example whisper_halluc_guard

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
const GATE: f32 = 0.3;
const BOOST_TARGET_DBFS: f32 = -3.0;

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
fn boost(s: &[f32]) -> Vec<f32> {
    let g = 10f32.powf((BOOST_TARGET_DBFS - db(peak(s))) / 20.0);
    s.iter().map(|&x| (x * g).clamp(-1.0, 1.0)).collect()
}
fn vad_gate(vad: &mut Vad, s: &[f32]) -> Vec<f32> {
    const PREFILL: usize = 15; const ONSET: usize = 2; const HANGOVER: usize = 15;
    vad.reset();
    let mut buf: VecDeque<Vec<f32>> = VecDeque::new();
    let mut out = Vec::new();
    let (mut spk, mut hang, mut ons) = (false, 0usize, 0usize);
    let mut fr = [0f32; FRAME];
    for chunk in s.chunks(FRAME) {
        if chunk.len() < FRAME { break; }
        fr.copy_from_slice(chunk);
        buf.push_back(fr.to_vec()); while buf.len() > PREFILL + 1 { buf.pop_front(); }
        let v = vad.compute(&fr).map(|r| r.prob > GATE).unwrap_or(false);
        match (spk, v) {
            (false, true) => { ons += 1; if ons >= ONSET { spk = true; hang = HANGOVER; ons = 0; for b in &buf { out.extend_from_slice(b); } } }
            (true, true) => { hang = HANGOVER; out.extend_from_slice(&fr); }
            (true, false) => { if hang > 0 { hang -= 1; out.extend_from_slice(&fr); } else { spk = false; } }
            (false, false) => ons = 0,
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
    let mut p: Vec<usize> = (0..=h.len()).collect(); let mut c = vec![0usize; h.len() + 1];
    for i in 1..=r.len() { c[0] = i; for j in 1..=h.len() {
        let cost = if r[i-1]==h[j-1] {0} else {1}; c[j]=(p[j]+1).min(c[j-1]+1).min(p[j-1]+cost);
    } std::mem::swap(&mut p, &mut c); }
    p[h.len()] as f32 / r.len() as f32
}

struct Cfg { name: &'static str, no_speech: Option<f32>, logprob: Option<f32> }
fn opts(c: &Cfg) -> RunOptions {
    RunOptions { task: Task::Transcribe, language: None, target_language: None,
        family: Some(RunExtension::Whisper(WhisperRunOptions {
            condition_on_prev_tokens: Some(false),
            no_speech_thold: c.no_speech, logprob_thold: c.logprob, ..Default::default() })),
        ..Default::default() }
}
fn run(sess: &mut transcribe_cpp::Session, vad: &mut Vad, a: &[f32], c: &Cfg) -> String {
    let g = vad_gate(vad, a);
    if g.len() < 1600 { String::new() } else { sess.run(&g, &opts(c)).map(|x| x.text).unwrap_or_default() }
}
fn list(dir: &str, pfx: &str) -> Vec<PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with(pfx)).unwrap_or(false)).collect();
    v.sort(); v
}

fn main() -> Result<()> {
    eprintln!("loading…");
    let model = Model::load_with(Path::new(MODEL_PATH), &ModelOptions { backend: Backend::Metal, gpu_device: 0 })?;
    let mut s = model.session()?;
    let mut vad = Vad::new(SILERO_PATH, SR).map_err(|e| anyhow::anyhow!("vad: {e}"))?;

    let cfgs = [
        Cfg { name: "baseline", no_speech: None, logprob: None },
        Cfg { name: "std(0.6/-1)", no_speech: Some(0.6), logprob: Some(-1.0) },
        Cfg { name: "aggr(0.3/-0.5)", no_speech: Some(0.3), logprob: Some(-0.5) },
    ];

    println!("\n████ NON-SPEECH (boosted) — phantom words per config (want 0) ████");
    print!("{:<32}", "clip");
    for c in &cfgs { print!("{:>16}", c.name); }
    println!();
    let mut nonspeech = list(STRESS, "clean_");
    nonspeech.extend(list(STRESS, "sil_"));
    for p in &nonspeech {
        let a = boost(&read_wav_mono(p)?);
        print!("{:<32}", p.file_name().unwrap().to_string_lossy());
        for c in &cfgs { print!("{:>16}", normalize(&run(&mut s, &mut vad, &a, c)).len()); }
        println!();
    }

    println!("\n████ REAL WHISPER (boosted) — WER per config (want low) ████");
    print!("{:<32}", "clip");
    for c in &cfgs { print!("{:>16}", c.name); }
    println!();
    let mut wc = list(CORPUS, "whisper_");
    wc.retain(|p| { let n = p.file_name().unwrap().to_string_lossy().to_string(); n.contains("_natural_") || n.contains("_faint_") });
    for p in &wc {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let reference = if name.contains("_A_") { REF_A } else { REF_B };
        let a = boost(&read_wav_mono(p)?);
        print!("{:<32}", name);
        for c in &cfgs { print!("{:>15.0}%", wer(reference, &run(&mut s, &mut vad, &a, c)) * 100.0); }
        println!();
    }
    println!("\n(want a config with 0 phantom words on non-speech AND low whisper WER.)");
    Ok(())
}
