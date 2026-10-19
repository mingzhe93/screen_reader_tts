//! Pinned llama.cpp backend build and the curated Assistant model catalogue
//! (plan §4.1, §4.2, §5.1). Everything here is compiled in; nothing looks up "latest".

use serde::Serialize;

/// The llama.cpp build tag this release installs.
pub(crate) const BACKEND_VERSION: &str = "b11718";

/// Populated only after the pinned 12-case/three-seed tool evaluation passes.
/// A model remains usable with tools after the explicit reliability warning.
pub(crate) const TOOLS_VERIFIED_MODELS: &[&str] = &["gemma-4-e4b", "gemma-4-12b"];

const BACKEND_RELEASE_URL: &str = "https://github.com/ggml-org/llama.cpp/releases/download";

/// One downloadable file with its pinned size and SHA-256 (lowercase hex).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PinnedFile {
    pub file: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

impl PinnedFile {
    /// Download URL of a backend release asset.
    pub(crate) fn url(&self) -> String {
        format!("{BACKEND_RELEASE_URL}/{BACKEND_VERSION}/{}", self.file)
    }
}

const BACKEND_WINDOWS_X64: PinnedFile = PinnedFile {
    file: "llama-b11718-bin-win-vulkan-x64.zip",
    size: 32_307_966,
    sha256: "237e5736031a9cdcf9eff54e1f4e9bd871945de1d56ec542ecb9ebaf8d45bdb6",
};

const BACKEND_MACOS_ARM64: PinnedFile = PinnedFile {
    file: "llama-b11718-bin-macos-arm64.tar.gz",
    size: 12_292_559,
    sha256: "a6f2574e4996d74f93bdcc32e2139539b276e14edfba3388cda5a5e9ebbc445a",
};

/// Backend archive for an `std::env::consts::{OS, ARCH}` pair; `None` when unsupported
/// (Intel Mac, Windows arm64, Linux).
pub(crate) fn backend_asset_for(os: &str, arch: &str) -> Option<&'static PinnedFile> {
    match (os, arch) {
        ("windows", "x86_64") => Some(&BACKEND_WINDOWS_X64),
        ("macos", "aarch64") => Some(&BACKEND_MACOS_ARM64),
        _ => None,
    }
}

/// Backend archive for the running platform.
pub(crate) fn backend_asset() -> Option<&'static PinnedFile> {
    backend_asset_for(std::env::consts::OS, std::env::consts::ARCH)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ModelTier {
    Small,
    Mid,
    /// Serialised as "mid-high".
    MidHigh,
    Large,
}

/// How a model's thinking is switched on and off. Kept as data so a future model can use a
/// different mechanism (plan §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingControl {
    /// A boolean chat-template kwarg, passed as `chat_template_kwargs: {<name>: bool}`.
    ChatTemplateKwarg(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CatalogueModel {
    pub id: &'static str,
    pub label: &'static str,
    pub tier: ModelTier,
    pub repo: &'static str,
    /// Hugging Face commit the files are downloaded from, never `main`.
    pub revision: &'static str,
    pub weights: PinnedFile,
    /// Vision projector, passed to llama-server as `--mmproj`.
    pub mmproj: PinnedFile,
    /// Tokens one image is assumed to cost when fitting the context.
    pub image_token_estimate: u32,
    pub thinking: ThinkingControl,
    /// The context length the model was trained for (`<arch>.context_length` in its GGUF).
    pub max_context: u32,
    pub memory: MemoryEstimate,
    /// The sampling settings the model's publisher recommends; the panel's defaults.
    pub sampling: SamplingDefaults,
}

/// Sampling settings sent with each request. llama.cpp's own defaults (temperature 0.8,
/// top-k 40, min-p 0.05) are generic, not what these models were tuned for.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub(crate) struct SamplingDefaults {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
    pub min_p: f32,
    pub repeat_penalty: f32,
}

/// From the Gemma 4 model cards: "temperature=1.0, top_p=0.95, top_k=64" for all uses.
const GEMMA4_SAMPLING: SamplingDefaults =
    SamplingDefaults { temperature: 1.0, top_p: 0.95, top_k: 64, min_p: 0.0, repeat_penalty: 1.0 };

/// Memory the server needs for a model, linear in context length. Split into two pools,
/// because on a discrete GPU part of a model stays in system RAM (plan §5.3).
///
/// Measured figures come from llama-server b11718's own buffer report on Metal (weights,
/// KV cache, compute buffers and the projector's worst case) at 8K and 32K context. A model
/// not measured yet is calculated from its GGUF layout and says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) struct MemoryEstimate {
    pub gpu_fixed_mib: u32,
    pub gpu_mib_per_1k_ctx: u32,
    pub host_fixed_mib: u32,
    pub host_mib_per_1k_ctx: u32,
    pub measured: bool,
}

impl MemoryEstimate {
    /// `(gpu_mib, host_mib)` for a context of `ctx_size` tokens.
    pub(crate) fn at(&self, ctx_size: u32) -> (u32, u32) {
        let thousands = ctx_size.div_ceil(1024);
        (
            self.gpu_fixed_mib + self.gpu_mib_per_1k_ctx * thousands,
            self.host_fixed_mib + self.host_mib_per_1k_ctx * thousands,
        )
    }
}

impl CatalogueModel {
    /// Weights first, then the projector: the order files are downloaded in.
    pub(crate) fn files(&self) -> [&PinnedFile; 2] {
        [&self.weights, &self.mmproj]
    }

    pub(crate) fn download_bytes(&self) -> u64 {
        self.weights.size + self.mmproj.size
    }

    pub(crate) fn file_url(&self, file: &PinnedFile) -> String {
        format!("https://huggingface.co/{}/resolve/{}/{}", self.repo, self.revision, file.file)
    }
}

const ENABLE_THINKING: ThinkingControl = ThinkingControl::ChatTemplateKwarg("enable_thinking");
/// Measured with llama-server b11718: a 1024x768 image (the page downscales to at most 1024 px)
/// costs 329 tokens on MiniCPM and 338 on both Gemma 4 models. 512 leaves room for other shapes.
const IMAGE_TOKEN_ESTIMATE: u32 = 512;

/// The catalogue in display order, smallest first: small, mid, mid-high, large. The tier
/// recommendation walks it from the end, so keep it sorted by memory at 32K context.
pub(crate) const MODELS: [CatalogueModel; 4] = [
    CatalogueModel {
        id: "minicpm-v-4.7-1b",
        label: "MiniCPM-V 4.7 1B",
        tier: ModelTier::Small,
        repo: "openbmb/MiniCPM-V-4.7-1B-gguf",
        revision: "9684c734cfb2e6aaccb0bb52ba58afa9aae92d90",
        weights: PinnedFile {
            file: "MiniCPM-V-4_7-1B-Q4_K_M.gguf",
            size: 672_003_680,
            sha256: "588c6c94c82f756b86dba35c42ed9ba9ec187d65824eb52f039c347df563e63d",
        },
        mmproj: PinnedFile {
            file: "mmproj-model-f16.gguf",
            size: 1_110_117_984,
            sha256: "fb9cc047a3cdad5317fd8433a4aeda2d22f00f02ca4bd93b34ad84165b25a900",
        },
        image_token_estimate: IMAGE_TOKEN_ESTIMATE,
        thinking: ENABLE_THINKING,
        max_context: 262_144,
        // Measured: 1.72 GiB GPU + 0.15 GiB host at 8K, 2.07 + 0.17 at 32K. A hybrid model
        // with only 6 attention layers, so the KV cache is small (384 MiB at 32K).
        memory: MemoryEstimate {
            gpu_fixed_mib: 1643,
            gpu_mib_per_1k_ctx: 15,
            host_fixed_mib: 144,
            host_mib_per_1k_ctx: 1,
            measured: true,
        },
        // From the MiniCPM-V 4.7 model card's "recommended instruct-mode sampling parameters".
        sampling: SamplingDefaults { temperature: 0.7, top_p: 0.95, top_k: 100, min_p: 0.0, repeat_penalty: 1.03 },
    },
    CatalogueModel {
        id: "gemma-4-e4b",
        label: "Gemma 4 E4B",
        tier: ModelTier::Mid,
        repo: "unsloth/gemma-4-E4B-it-GGUF",
        revision: "bfc15c382204943c3a8fff0c750b94ae2364d7a3",
        weights: PinnedFile {
            file: "gemma-4-E4B-it-UD-Q4_K_XL.gguf",
            size: 5_126_306_944,
            sha256: "3cf61de12daa015ee0f7b68e7b7c541405bf220e1e942bad8b47cab827d7df80",
        },
        mmproj: PinnedFile {
            file: "mmproj-F16.gguf",
            size: 990_372_672,
            sha256: "ddf46c21d7078e95338cfc22306b19b276a29a5ad089023449dd54d4b6170a51",
        },
        image_token_estimate: IMAGE_TOKEN_ESTIMATE,
        thinking: ENABLE_THINKING,
        max_context: 131_072,
        // Measured: 4.37 GiB GPU + 2.28 GiB host at 8K, 4.86 + 2.31 at 32K. The per-layer
        // embedding tables (about 2.3 GiB) stay in system RAM.
        memory: MemoryEstimate {
            gpu_fixed_mib: 4310,
            gpu_mib_per_1k_ctx: 21,
            host_fixed_mib: 2330,
            host_mib_per_1k_ctx: 1,
            measured: true,
        },
        sampling: GEMMA4_SAMPLING,
    },
    CatalogueModel {
        id: "mimo-v2.6-9b",
        label: "MiMo-V2.6 9B",
        tier: ModelTier::MidHigh,
        // Xiaomi's agentic fine-tune of Qwen3.5-9B (MIT), quantised by bartowski with an
        // imatrix. No UD-Q4_K_XL build exists, so Q4_K_M.
        repo: "bartowski/MiMo-V2.6-Distill-Qwen-9B-GGUF",
        revision: "4371da10c84fb26da3592d4cf312d24aa82b7b65",
        weights: PinnedFile {
            file: "MiMo-V2.6-Distill-Qwen-9B-Q4_K_M.gguf",
            size: 5_841_049_120,
            sha256: "4bca6f18c73f72270c7a20c2ea2bea581de8246e318714277120369d34048c81",
        },
        mmproj: PinnedFile {
            file: "mmproj-MiMo-V2.6-Distill-Qwen-9B-f16.gguf",
            size: 918_166_048,
            sha256: "ff348f3180a63188aa7285db85f550fe38acb61dd013c599eb8bad08d2cc2576",
        },
        image_token_estimate: IMAGE_TOKEN_ESTIMATE,
        // The MiMo chat template reads `enable_thinking` (an empty <think></think> when false).
        thinking: ENABLE_THINKING,
        max_context: 262_144,
        // Not measured yet: calculated from its GGUF layout (arch `qwen35`, the same hybrid
        // as MiniCPM-V 4.7, whose measured overhead calibrates this). On the GPU: 5014 MiB of
        // weights (token embeddings stay in system RAM), the 876 MiB projector and about
        // 350 MiB of recurrent state and compute buffers. 8 of 32 layers use attention, with
        // 4 KV heads of 256: 32 KiB of f16 KV cache per token, 32 MiB per 1K plus compute.
        memory: MemoryEstimate {
            gpu_fixed_mib: 6240,
            gpu_mib_per_1k_ctx: 36,
            host_fixed_mib: 556,
            host_mib_per_1k_ctx: 1,
            measured: false,
        },
        // From the model's generation_config.json (also in the GGUF's general.sampling.*).
        sampling: SamplingDefaults { temperature: 0.6, top_p: 0.95, top_k: 20, min_p: 0.0, repeat_penalty: 1.0 },
    },
    CatalogueModel {
        id: "gemma-4-12b",
        label: "Gemma 4 12B",
        tier: ModelTier::Large,
        repo: "unsloth/gemma-4-12b-it-GGUF",
        revision: "fc034cfff751157913579611efad8462ac1be606",
        weights: PinnedFile {
            file: "gemma-4-12b-it-UD-Q4_K_XL.gguf",
            size: 7_366_423_360,
            sha256: "90fd944d227e9d9b68e7e2c7d5b57b79d4c66ed521b0919fbbd932cf834f6f8e",
        },
        mmproj: PinnedFile {
            file: "mmproj-F16.gguf",
            size: 175_115_840,
            sha256: "91f086971e56d7a7d8d39e271873fccdb49541bd259d6e02c401a4f1cb7a219e",
        },
        image_token_estimate: IMAGE_TOKEN_ESTIMATE,
        thinking: ENABLE_THINKING,
        max_context: 262_144,
        // Measured: 7.92 GiB GPU + 0.68 GiB host at 8K, 8.37 + 0.71 at 32K. (The earlier
        // estimate from its GGUF layout, 8.5 GiB GPU at 32K, was within 2%.)
        memory: MemoryEstimate {
            gpu_fixed_mib: 7961,
            gpu_mib_per_1k_ctx: 19,
            host_fixed_mib: 692,
            host_mib_per_1k_ctx: 1,
            measured: true,
        },
        sampling: GEMMA4_SAMPLING,
    },
];

/// A model file for automatic turn-taking (plan §7.4), pinned like everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TurnModelFile {
    pub url: &'static str,
    /// Saved under this name in `<assistant root>/turn/`.
    pub file: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

/// Smart Turn v3.2 (CPU, int8; BSD-2-Clause), from `pipecat-ai/smart-turn-v3`.
pub(crate) const SMART_TURN_MODEL: TurnModelFile = TurnModelFile {
    url: "https://huggingface.co/pipecat-ai/smart-turn-v3/resolve/f766f81d3cfdf7737ac64aad813d91bbfd56bf93/smart-turn-v3.2-cpu.onnx",
    file: "smart-turn-v3.2-cpu.onnx",
    size: 8_679_182,
    sha256: "2bb026316b14a660486a75b1733cd3fbab8c2fd0314dc9af7be49f8cca967e4f",
};

/// Silero VAD (MIT), from `onnx-community/silero-vad`.
pub(crate) const SILERO_VAD_MODEL: TurnModelFile = TurnModelFile {
    url: "https://huggingface.co/onnx-community/silero-vad/resolve/e71cae966052b992a7eca6b17738916ce0eca4ec/onnx/model.onnx",
    file: "silero-vad.onnx",
    size: 2_243_022,
    sha256: "a4a068cd6cf1ea8355b84327595838ca748ec29a25bc91fc82e6c299ccdc5808",
};

pub(crate) fn models() -> &'static [CatalogueModel] {
    &MODELS
}

pub(crate) fn model_by_id(id: &str) -> Option<&'static CatalogueModel> {
    MODELS.iter().find(|model| model.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_tools_require_current_pins_and_passing_recorded_qualification() {
        use sha2::{Digest, Sha256};
        let reports = [
            ("gemma-4-e4b", include_str!("../../docs/benchmarks/assistant-tools/2026-10-19-gemma-4-e4b-decimal-voice.json")),
            ("gemma-4-12b", include_str!("../../docs/benchmarks/assistant-tools/2026-10-19-gemma-4-12b-decimal-voice.json")),
        ];
        assert_eq!(TOOLS_VERIFIED_MODELS, &reports.map(|(id,_)|id));
        for (id, text) in reports {
        let report: serde_json::Value = serde_json::from_str(text).unwrap();
        let model = model_by_id(id).unwrap();
        assert_eq!(report["qualified"], true);
        assert_eq!(report["model_sha256"], model.weights.sha256);
        assert_eq!(report["model_revision"], model.revision);
        assert_eq!(report["server_build"], BACKEND_VERSION);
        assert_eq!(report["tools_sha256"], format!("{:x}",Sha256::digest(crate::assistant_tools::definitions().to_string().as_bytes())));
        let fixture: serde_json::Value = serde_json::from_str(include_str!("../tests-data/tools/model-cases.json")).unwrap();
        assert_eq!(report["fixture_sha256"], format!("{:x}",Sha256::digest(include_bytes!("../tests-data/tools/model-cases.json"))));
        let time = fixture["policy_time"].as_str().unwrap();
        for (voice, prompt, field) in [(false, crate::assistant_chat::DEFAULT_CHAT_PROMPT, "chat_prompt_sha256"), (true, crate::assistant_chat::DEFAULT_VOICE_PROMPT, "voice_prompt_sha256")] {
            assert_eq!(report[field], format!("{:x}",Sha256::digest(format!("{prompt}{}",crate::assistant_tools::prompt_at(voice,time)).as_bytes())));
            let mut passes = 0;
            for case in fixture["cases"].as_array().unwrap() {
                let records: Vec<_> = report["records"].as_array().unwrap().iter().filter(|r|r["voice"]==voice && r["case"]==case["id"]).collect();
                assert_eq!(records.len(), 3);
                let passed = records.iter().filter(|r|r["passed"]==true).count() >= 2;
                if case["hard"]==true { assert!(passed, "required case {} must pass",case["id"]); }
                passes += usize::from(passed);
            }
            assert!(passes>=11);
        }
        }
    }

    fn is_lower_hex(value: &str, len: usize) -> bool {
        value.len() == len && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    #[test]
    fn assistant_catalogue_memory_estimates_reproduce_the_measurements() {
        // (id, ctx, gpu MiB, host MiB) from llama-server b11718 on Metal.
        for (id, ctx, gpu, host) in [
            ("minicpm-v-4.7-1b", 8192, 1763, 152),
            ("minicpm-v-4.7-1b", 32768, 2123, 176),
            ("gemma-4-e4b", 8192, 4478, 2338),
            ("gemma-4-e4b", 32768, 4982, 2362),
            ("gemma-4-12b", 8192, 8113, 700),
            ("gemma-4-12b", 32768, 8569, 724),
        ] {
            let (g, h) = model_by_id(id).unwrap().memory.at(ctx);
            assert!(g.abs_diff(gpu) <= 8 && h.abs_diff(host) <= 8, "{id} at {ctx}: {g}/{h} vs {gpu}/{host}");
        }
        assert!(models().iter().filter(|model| model.id != "mimo-v2.6-9b").all(|model| model.memory.measured));
        assert_eq!(model_by_id("gemma-4-e4b").unwrap().max_context, 131_072);
    }

    #[test]
    fn assistant_catalogue_lists_tiers_in_stable_order() {
        let ids: Vec<_> = models().iter().map(|model| model.id).collect();
        assert_eq!(ids, ["minicpm-v-4.7-1b", "gemma-4-e4b", "mimo-v2.6-9b", "gemma-4-12b"]);
        let tiers: Vec<_> = models().iter().map(|model| model.tier).collect();
        assert_eq!(tiers, [ModelTier::Small, ModelTier::Mid, ModelTier::MidHigh, ModelTier::Large]);
        assert_eq!(serde_json::to_string(&ModelTier::MidHigh).unwrap(), "\"mid-high\"");
        // The tier recommendation walks the list from the end, so it must be sorted by size.
        let at_32k: Vec<_> = models().iter().map(|model| { let (g, h) = model.memory.at(32_768); g + h }).collect();
        assert!(at_32k.windows(2).all(|pair| pair[0] < pair[1]), "{at_32k:?}");
    }

    #[test]
    fn assistant_catalogue_looks_up_by_id() {
        assert_eq!(model_by_id("gemma-4-e4b").unwrap().label, "Gemma 4 E4B");
        assert_eq!(model_by_id("gemma-4-12b").unwrap().tier, ModelTier::Large);
        assert!(model_by_id("gemma-4").is_none());
        assert!(model_by_id("").is_none());
    }

    #[test]
    fn assistant_catalogue_entries_are_pinned() {
        for model in models() {
            assert!(is_lower_hex(model.revision, 40), "{} revision", model.id);
            for file in model.files() {
                assert!(is_lower_hex(file.sha256, 64), "{} {} sha256", model.id, file.file);
                assert!(file.size > 0);
                let url = model.file_url(file);
                assert!(url.contains(model.revision) && !url.contains("/resolve/main/"), "{url}");
            }
            assert_eq!(model.image_token_estimate, 512);
            assert_eq!(model.thinking, ThinkingControl::ChatTemplateKwarg("enable_thinking"));
        }
        for asset in [&BACKEND_WINDOWS_X64, &BACKEND_MACOS_ARM64] {
            assert!(is_lower_hex(asset.sha256, 64), "{}", asset.file);
            assert!(asset.file.contains(BACKEND_VERSION));
        }
    }

    #[test]
    fn assistant_catalogue_download_totals_match_the_plan() {
        assert_eq!(model_by_id("minicpm-v-4.7-1b").unwrap().download_bytes(), 1_782_121_664);
        assert_eq!(model_by_id("gemma-4-e4b").unwrap().download_bytes(), 6_116_679_616);
        assert_eq!(model_by_id("gemma-4-12b").unwrap().download_bytes(), 7_541_539_200);
    }

    #[test]
    fn assistant_catalogue_model_urls_use_the_pinned_revision() {
        let model = model_by_id("minicpm-v-4.7-1b").unwrap();
        assert_eq!(
            model.file_url(&model.weights),
            "https://huggingface.co/openbmb/MiniCPM-V-4.7-1B-gguf/resolve/9684c734cfb2e6aaccb0bb52ba58afa9aae92d90/MiniCPM-V-4_7-1B-Q4_K_M.gguf"
        );
    }

    #[test]
    fn assistant_catalogue_backend_asset_by_platform() {
        assert_eq!(
            backend_asset_for("windows", "x86_64").unwrap().url(),
            "https://github.com/ggml-org/llama.cpp/releases/download/b11718/llama-b11718-bin-win-vulkan-x64.zip"
        );
        assert_eq!(backend_asset_for("macos", "aarch64").unwrap().size, 12_292_559);
        assert!(backend_asset_for("macos", "x86_64").is_none());
        assert!(backend_asset_for("windows", "aarch64").is_none());
        assert!(backend_asset_for("linux", "x86_64").is_none());
    }
}
