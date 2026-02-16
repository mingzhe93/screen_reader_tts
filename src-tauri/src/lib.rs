pub fn run() {
    voicereader_core::run_app();
}

mod voicereader_core;
#[cfg(feature = "build-base")]
mod kyutai_local;
