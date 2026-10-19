"""Reference Smart Turn v3 features and probabilities for the Rust parity tests.

Runs the upstream Python path (pipecat-ai/smart-turn `inference.py` and `audio_utils.py`,
copied below without changes to the logic) on 16 kHz mono 16-bit WAV files and writes,
into the output directory:

  reference.json   one entry per WAV: sample count, probability, prediction, global
                   feature statistics and the indices of the frames stored in full
  <name>.f32       little-endian float32: per-frame mean[800], min[800], max[800] of the
                   80 x 800 input tensor, then the 80 values of each sampled frame

The full tensors are not stored, to keep the fixtures small (src-tauri/tests-data).

Usage (offline; needs numpy, soundfile, onnxruntime and transformers):
  python -I scripts/smart_turn_reference.py --model smart-turn-v3.2-cpu.onnx \
      --out src-tauri/tests-data/smart_turn src-tauri/tests-data/smart_turn/*.wav

See docs/assistant/turn-detection.md.
"""

import argparse
import json
import os
import sys

import numpy as np
import onnxruntime as ort
import soundfile as sf
from transformers import WhisperFeatureExtractor

SAMPLE_RATE = 16000
N_MELS = 80
N_FRAMES = 800
# Frames stored in full: both ends (the padding side and the most recent audio) and a
# regular stride through the middle.
SAMPLED_FRAMES = [0, 1, 2] + list(range(5, N_FRAMES, 15)) + [797, 798, 799]


# --- upstream audio_utils.py (verbatim) ---------------------------------------------
def truncate_audio_to_last_n_seconds(audio_array, n_seconds=8, sample_rate=16000):
    """Truncate audio to last n seconds or pad with zeros to meet n seconds."""
    max_samples = n_seconds * sample_rate
    if len(audio_array) > max_samples:
        return audio_array[-max_samples:]
    elif len(audio_array) < max_samples:
        # Pad with zeros at the beginning
        padding = max_samples - len(audio_array)
        return np.pad(audio_array, (padding, 0), mode='constant', constant_values=0)
    return audio_array


# --- upstream inference.py (verbatim apart from returning the features too) ----------
def build_session(onnx_path):
    so = ort.SessionOptions()
    so.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
    so.inter_op_num_threads = 1
    so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
    return ort.InferenceSession(onnx_path, sess_options=so)


feature_extractor = WhisperFeatureExtractor(chunk_length=8)


def predict_endpoint(session, audio_array):
    audio_array = truncate_audio_to_last_n_seconds(audio_array, n_seconds=8)
    inputs = feature_extractor(
        audio_array,
        sampling_rate=16000,
        return_tensors="np",
        padding="max_length",
        max_length=8 * 16000,
        truncation=True,
        do_normalize=True,
    )
    input_features = inputs.input_features.squeeze(0).astype(np.float32)
    input_features = np.expand_dims(input_features, axis=0)
    outputs = session.run(None, {"input_features": input_features})
    probability = outputs[0][0].item()
    prediction = 1 if probability > 0.5 else 0
    return input_features[0], probability, prediction


# --- fixture writing ------------------------------------------------------------------
def load_wav(path):
    """16-bit PCM to float32 as sample / 32768, the same conversion the Rust test uses."""
    data, rate = sf.read(path, dtype="int16", always_2d=False)
    if rate != SAMPLE_RATE:
        sys.exit(f"{path}: expected {SAMPLE_RATE} Hz, got {rate}")
    if data.ndim != 1:
        sys.exit(f"{path}: expected mono audio")
    return data.astype(np.float32) / np.float32(32768.0)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--model", required=True, help="smart-turn-v3.x ONNX file")
    parser.add_argument("--out", required=True, help="directory for reference.json and the .f32 files")
    parser.add_argument("wavs", nargs="+")
    args = parser.parse_args()

    os.makedirs(args.out, exist_ok=True)
    session = build_session(args.model)
    entries = []
    for path in sorted(args.wavs):
        name = os.path.splitext(os.path.basename(path))[0]
        audio = load_wav(path)
        features, probability, prediction = predict_endpoint(session, audio)
        assert features.shape == (N_MELS, N_FRAMES), features.shape
        blob = np.concatenate(
            [features.mean(axis=0), features.min(axis=0), features.max(axis=0)]
            + [features[:, frame] for frame in SAMPLED_FRAMES]
        ).astype("<f4")
        blob.tofile(os.path.join(args.out, f"{name}.f32"))
        entries.append({
            "name": name,
            "wav": os.path.basename(path),
            "samples": int(audio.shape[0]),
            "probability": probability,
            "prediction": prediction,
            "feature_min": float(features.min()),
            "feature_max": float(features.max()),
            "feature_mean": float(features.mean()),
        })
        print(f"{name}: {audio.shape[0] / SAMPLE_RATE:.2f} s, probability {probability:.4f}, prediction {prediction}")

    reference = {
        "source": "pipecat-ai/smart-turn inference.py + transformers WhisperFeatureExtractor (numpy path)",
        "transformers": __import__("transformers").__version__,
        "onnxruntime": ort.__version__,
        "numpy": np.__version__,
        "model": os.path.basename(args.model),
        "n_mels": N_MELS,
        "n_frames": N_FRAMES,
        "sampled_frames": SAMPLED_FRAMES,
        "layout": "float32 LE: frame_mean[n_frames], frame_min[n_frames], frame_max[n_frames], then n_mels values per sampled frame",
        "fixtures": entries,
    }
    with open(os.path.join(args.out, "reference.json"), "w") as handle:
        json.dump(reference, handle, indent=2)
        handle.write("\n")


if __name__ == "__main__":
    main()
