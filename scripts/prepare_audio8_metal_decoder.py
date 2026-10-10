#!/usr/bin/env python3
"""Prepare a small FP32-compute graph while reusing the original FP16 weight file.

Regeneration needs numpy and onnx; the app and ordinary builds need neither.
"""
from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import urllib.request

import numpy as np
import onnx
from onnx import AttributeProto, TensorProto, helper, numpy_helper

SOURCE_SHA256 = "25379b866ad555b9a55226c46325344d2ebfafea1b474c29de5223a9d01ea533"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-decoder", type=Path, required=True)
    args = parser.parse_args()
    source = args.source_decoder.read_bytes()
    if hashlib.sha256(source).hexdigest() != SOURCE_SHA256:
        raise RuntimeError("Audio8 decoder export changed; review and validate the conversion before updating the pinned hash.")
    model = onnx.load_model_from_string(source)
    graph = model.graph
    casts = []
    for tensor in graph.initializer:
        if tensor.data_type == TensorProto.FLOAT16:
            original_name = tensor.name
            tensor.name += "__stored_fp16"
            casts.append(helper.make_node(
                "Cast", [tensor.name], [original_name],
                name=f"MetalPromoteWeight_{len(casts)}", to=TensorProto.FLOAT,
            ))
    for node in graph.node:
        for attr in node.attribute:
            if node.op_type == "Cast" and attr.name == "to" and attr.i == TensorProto.FLOAT16:
                attr.i = TensorProto.FLOAT
            if attr.type == AttributeProto.TENSOR and attr.t.data_type == TensorProto.FLOAT16:
                attr.t.CopyFrom(numpy_helper.from_array(
                    numpy_helper.to_array(attr.t).astype(np.float32), name=attr.t.name,
                ))
    for value in [*graph.input, *graph.output, *graph.value_info]:
        if value.type.HasField("tensor_type") and value.type.tensor_type.elem_type == TensorProto.FLOAT16:
            value.type.tensor_type.elem_type = TensorProto.FLOAT
    nodes = list(graph.node)
    del graph.node[:]
    graph.node.extend([*casts, *nodes])
    # Weight storage and offsets stay untouched; ORT folds the casts when loading.
    model.doc_string += "\nVoiceReader modification: FP32 codec computation for native WebGPU/Metal; original FP16 weight storage retained."
    output = Path(__file__).resolve().parents[1] / "src-tauri/binaries/audio8-metal"
    output.mkdir(parents=True, exist_ok=True)
    (output / "codec_decoder_fp16.source.onnx").write_bytes(source)
    destination = output / "codec_decoder_fp32.onnx"
    onnx.save_model(model, destination)
    for name in ["LICENSE", "NOTICE"]:
        url = f"https://raw.githubusercontent.com/Audio8-AI/Audio8_TTS/master/{name}"
        (output / name).write_bytes(urllib.request.urlopen(url).read())
    (output / "README.txt").write_text(
        "Audio8 codec decoder graph, modified for VoiceReader macOS Metal acceleration.\n"
        "Source: https://huggingface.co/Edge0/audio8-TTS-0.1B-ONNX-INT8\n"
        f"Original graph SHA-256: {SOURCE_SHA256}\n"
        "License: Apache-2.0; upstream attribution is in NOTICE.\n"
        "Modification: computation uses FP32; initializer weight storage remains FP16.\n"
        "No new weights are included. The original codec_decoder_fp16.onnx.data download is reused.\n"
        "The app verifies the original graph matches before caching this derived graph alongside it.\n"
        "Recipe: scripts/prepare_audio8_metal_decoder.py (numpy + onnx required only for regeneration).\n"
    )
    print(f"Prepared {destination} ({destination.stat().st_size} bytes, {len(casts)} weight casts)")


if __name__ == "__main__":
    main()
