pub mod audio;
pub mod constants;
pub mod lang_id;
pub mod text;
pub mod utils;
pub mod vad;

pub use audio::{
    classify_speech, is_microphone_access_denied, is_no_input_device_error, list_input_devices,
    list_output_devices, read_wav_samples, save_wav_file, verify_wav_file, whisper_autogain,
    AudioRecorder, CaptureDebug, CpalDeviceInfo, NoSpeechReason, SegmentClose, SpeechVerdict,
    StreamSegmenter, StreamSegmenterConfig, VadPolicy,
};
pub use constants::WHISPER_SAMPLE_RATE;
pub use lang_id::detect_output_language;
pub use text::{
    apply_custom_words, normalize_transcription_output, remove_filler_words, OutputLanguageEvidence,
};
pub use utils::get_cpal_host;
pub use vad::{EarshotVad, SileroVad, VoiceActivityDetector};
