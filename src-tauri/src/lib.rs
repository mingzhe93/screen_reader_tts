pub fn run() {
    voicereader_core::run_app();
}

mod voicereader_core;
mod selection;
mod settings;
#[cfg(feature = "build-base")]
mod asr_local;
#[cfg(feature = "build-base")]
mod audio_capture;
#[cfg(feature = "build-base")]
mod audio_decode;
#[cfg(feature = "build-base")]
mod audio8_local;
#[cfg(feature = "build-base")]
mod audio8_model;
#[cfg(feature = "build-base")]
mod audio_pipeline;
#[cfg(feature = "build-base")]
mod bundled_paths;
#[cfg(feature = "build-base")]
mod kyutai_local;
#[cfg(feature = "build-base")]
mod model_download;
#[cfg(feature = "build-base")]
mod text_chunking;
