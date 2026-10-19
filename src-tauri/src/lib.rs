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

#[cfg(feature = "build-base")]
mod recording_export;

// The optional Assistant: a managed llama-server and the chat page (docs/assistant/README.md).
#[cfg(feature = "build-base")]
mod assistant_backend;
#[cfg(feature = "build-base")]
mod assistant_catalogue;
#[cfg(feature = "build-base")]
mod assistant_chat;
#[cfg(feature = "build-base")]
mod assistant_commands;
#[cfg(feature = "build-base")]
mod assistant_voice;
#[cfg(feature = "build-base")]
mod assistant_turn;
#[cfg(feature = "build-base")]
mod assistant_phrases;
#[cfg(feature = "build-base")]
mod assistant_conversation;
#[cfg(feature = "build-base")]
mod assistant_tools;
#[cfg(feature = "build-base")]
mod assistant_tools_network;
#[cfg(feature = "build-base")]
mod assistant_sandbox;
#[cfg(feature = "build-base")]
mod assistant_tool_loop;
