//! A/B harness — does the whisper style primer actually help on Martin's own
//! recordings? NOT part of the app. Run from `src-tauri/`:
//!
//!   cargo run --release --example ab_primer
//!
//! For every saved recording it transcribes THREE ways, changing only the
//! initial prompt (everything else — task, auto language, condition_on_prev
//! _tokens=false — is held identical), so any difference is the primer alone:
//!   1. BASELINE  — no primer
//!   2. RU primer — the fork's Russian style primer
//!   3. EN primer — the fork's English style primer
//! It prints the detected language + wall-clock per run so we can eyeball
//! punctuation gains, hallucination/leak, and any language damage.

use anyhow::Result;
use std::path::Path;
use std::time::Instant;
use transcribe_cpp::{
    Backend, Model, ModelOptions, RunExtension, RunOptions, Task, WhisperRunOptions,
};

const MODEL_PATH: &str = "/Users/martinmourzenkov/.cache/huggingface/hub/models--handy-computer--whisper-large-v3-gguf/snapshots/e3e29bee6389c7da4a141406f07bb80ddac5337c/whisper-large-v3-Q5_K_M.gguf";
const RECORDINGS_DIR: &str =
    "/Users/martinmourzenkov/Library/Application Support/com.pais.handy/recordings";

const PRIMER_RU: &str = "Привет! Давай обсудим план на сегодня. \
     Нужно закоммитить изменения, открыть pull request и смержить его в main. \
     Потом проверю deployment и logs на staging. В целом всё выглядит неплохо, \
     так что go for it.";

const PRIMER_EN: &str = "Hey, let's go over the plan for today. \
     I'll commit the changes, open a pull request, and merge it into main. Then \
     I'll check the deployment and the logs on staging. Overall it looks solid, \
     so let's ship it.";

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
        hound::SampleFormat::Float => {
            reader.samples::<f32>().collect::<std::result::Result<_, _>>()?
        }
    };
    // App records 16k mono; if a file is stereo, downmix to the left channel.
    if spec.channels == 2 {
        Ok(raw.iter().step_by(2).copied().collect())
    } else {
        Ok(raw)
    }
}

fn whisper_opts(primer: Option<&str>) -> RunOptions {
    RunOptions {
        task: Task::Transcribe,
        language: None,        // auto-detect, held constant across the 3 runs
        target_language: None,
        family: Some(RunExtension::Whisper(WhisperRunOptions {
            initial_prompt: primer.map(|s| s.to_string()),
            condition_on_prev_tokens: Some(false),
            ..Default::default()
        })),
        ..Default::default()
    }
}

fn main() -> Result<()> {
    eprintln!("loading model (metal)…");
    let model = Model::load_with(
        Path::new(MODEL_PATH),
        &ModelOptions {
            backend: Backend::Metal,
            gpu_device: 0,
        },
    )?;
    let mut session = model.session()?;

    let mut wavs: Vec<_> = std::fs::read_dir(RECORDINGS_DIR)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "wav").unwrap_or(false))
        .collect();
    wavs.sort();
    eprintln!("{} recordings\n", wavs.len());

    let conditions: [(&str, Option<&str>); 3] = [
        ("BASELINE (no primer)", None),
        ("RU primer", Some(PRIMER_RU)),
        ("EN primer", Some(PRIMER_EN)),
    ];

    for wav in &wavs {
        let audio = read_wav_16k_mono(wav)?;
        let secs = audio.len() as f32 / 16_000.0;
        println!("\n════════════════════════════════════════════════════════════");
        println!(
            "FILE {}   ({:.1}s)",
            wav.file_name().unwrap().to_string_lossy(),
            secs
        );
        for (label, primer) in conditions {
            let t0 = Instant::now();
            let tr = session.run(&audio, &whisper_opts(primer))?;
            println!(
                "\n── {label}  [{:?}, lang={}]",
                t0.elapsed(),
                tr.language.as_deref().unwrap_or("?")
            );
            println!("{}", tr.text.trim());
        }
    }
    Ok(())
}
