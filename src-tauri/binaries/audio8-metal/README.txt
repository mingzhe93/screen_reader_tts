Audio8 codec decoder graph, modified for VoiceReader macOS Metal acceleration.
Source: https://huggingface.co/Edge0/audio8-TTS-0.1B-ONNX-INT8
Original graph SHA-256: 25379b866ad555b9a55226c46325344d2ebfafea1b474c29de5223a9d01ea533
License: Apache-2.0; upstream attribution is in NOTICE.
Modification: computation uses FP32; initializer weight storage remains FP16.
No new weights are included. The original codec_decoder_fp16.onnx.data download is reused.
The app verifies the original graph matches before caching this derived graph alongside it.
Recipe: scripts/prepare_audio8_metal_decoder.py (numpy + onnx required only for regeneration).
