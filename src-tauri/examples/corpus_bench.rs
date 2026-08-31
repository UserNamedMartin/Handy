//! Corpus benchmark — run a whisper GGUF over the frozen eval corpus with
//! Handy's exact decode settings, emit JSONL {clip_id,text,ms,lang} for scoring
//! against the cloud-consensus gold set. NOT part of the app.
//!
//! Run from `src-tauri/`, configured via env vars:
//!   MODEL_PATH=/abs/path/to/model.gguf \
//!   CORPUS_DIR=$HOME/tools-for-agents/handy-eval/corpus \
//!   OUT_PATH=$HOME/tools-for-agents/handy-eval/local_runs/<tag>.jsonl \
//!   PRIMER=""            # optional initial prompt (default none)
//!   COND_PREV=false      # condition_on_prev_tokens (default false, matches fork)
//!   cargo run --release --example corpus_bench

use anyhow::Result;
use std::io::Write;
use std::path::Path;
use std::time::Instant;
use transcribe_cpp::{
    Backend, Model, ModelOptions, RunExtension, RunOptions, Task, WhisperRunOptions,
};

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

fn main() -> Result<()> {
    let model_path = std::env::var("MODEL_PATH").expect("MODEL_PATH");
    let corpus_dir = std::env::var("CORPUS_DIR").expect("CORPUS_DIR");
    let out_path = std::env::var("OUT_PATH").expect("OUT_PATH");
    let primer = std::env::var("PRIMER").ok().filter(|s| !s.is_empty());
    let cond_prev = std::env::var("COND_PREV")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

    eprintln!("loading {model_path} (metal)…");
    let model = Model::load_with(
        Path::new(&model_path),
        &ModelOptions { backend: Backend::Metal, device: None },
    )?;
    let mut session = model.session()?;

    // ARCH=whisper (default) attaches the whisper run extension (primer +
    // condition_on_prev). Any other value (voxtral, qwen3_asr, parakeet, ...)
    // runs with family=None, exactly like Handy does for non-whisper archs —
    // the whisper extension is rejected with INVALID_ARG on those.
    let arch = std::env::var("ARCH").unwrap_or_else(|_| "whisper".to_string());
    let is_whisper = arch == "whisper";
    let opts = || RunOptions {
        task: Task::Transcribe,
        language: None,
        target_language: None,
        family: if is_whisper {
            Some(RunExtension::Whisper(WhisperRunOptions {
                initial_prompt: primer.clone(),
                condition_on_prev_tokens: Some(cond_prev),
                ..Default::default()
            }))
        } else {
            None
        },
        ..Default::default()
    };

    // clip dirs sorted
    let mut clips: Vec<_> = std::fs::read_dir(&corpus_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    clips.sort();
    eprintln!("{} clips", clips.len());

    if let Some(parent) = Path::new(&out_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = std::fs::File::create(&out_path)?;

    for (i, dir) in clips.iter().enumerate() {
        let cid = dir.file_name().unwrap().to_string_lossy().to_string();
        let wav = dir.join("raw.wav");
        if !wav.exists() {
            continue;
        }
        let audio = read_wav_16k_mono(&wav)?;
        let t0 = Instant::now();
        // Don't abort the whole run on one clip: some archs (e.g. qwen3_asr)
        // hit a generation budget on long clips and return an error. Record it
        // and continue so the rest of the corpus still gets scored.
        match session.run(&audio, &opts()) {
            Ok(tr) => {
                let ms = t0.elapsed().as_millis();
                let lang = tr.language.as_deref().unwrap_or("?").to_string();
                writeln!(
                    out,
                    "{{\"clip_id\":\"{}\",\"ms\":{},\"lang\":\"{}\",\"text\":\"{}\"}}",
                    cid, ms, lang, json_escape(tr.text.trim())
                )?;
                eprintln!("[{}/{}] {} {}ms lang={}", i + 1, clips.len(), cid, ms, lang);
            }
            Err(e) => {
                let msg = format!("{e}");
                writeln!(
                    out,
                    "{{\"clip_id\":\"{}\",\"ms\":null,\"lang\":\"err\",\"text\":\"\",\"error\":\"{}\"}}",
                    cid, json_escape(&msg)
                )?;
                eprintln!("[{}/{}] {} ERROR: {}", i + 1, clips.len(), cid, msg);
            }
        }
    }
    eprintln!("wrote {out_path}");
    Ok(())
}
