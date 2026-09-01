// Re-export all audio components
mod device;
mod gain;
mod recorder;
mod resampler;
mod speech_gate;
mod stream_segmenter;
mod utils;
mod visualizer;

pub use device::{list_input_devices, list_output_devices, CpalDeviceInfo};
pub use gain::{whisper_autogain, whisper_autogain_with_meta};
pub use recorder::CaptureDebug;
pub use recorder::{
    is_microphone_access_denied, is_no_input_device_error, AudioRecorder, VadPolicy,
};
pub use resampler::FrameResampler;
pub use speech_gate::{classify as classify_speech, NoSpeechReason, SpeechVerdict};
pub use stream_segmenter::{SegmentClose, StreamSegmenter, StreamSegmenterConfig};
pub use utils::{read_wav_samples, save_wav_file, verify_wav_file};
pub use visualizer::AudioVisualiser;
