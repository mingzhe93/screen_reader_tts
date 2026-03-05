# VoiceReader TTS Pipeline — Engineering Learnings

Findings from optimising the inter-chunk latency and look-ahead behaviour of the
Rust (Kyutai/pocket-tts) and Python (Qwen/pocket-tts) backends.

---

## 1. The two-path architecture for `rate != 1.0` vs `rate == 1.0`

### Why `generate()` (full batch) must be used when `rate != 1.0`

`SoxTempoStream` wraps a running SoX subprocess that applies pitch-preserving
tempo adjustment.  SoX operates in fixed output frames:

| Rate   | `frame_samples` (output) | Input samples needed per frame |
|--------|--------------------------|-------------------------------|
| < 2×   | 8 192                    | ≈ 8 192 × rate                |
| 2–3×   | 16 384                   | ≈ 32 768                      |
| ≥ 3×   | 24 576                   | ≈ 73 728                      |

`generate_stream()` yields tokens incrementally (~512 raw samples each).  On a
slow CPU (~50 ms/token) the model produces one SoX output frame worth of input
(32 768 samples) in **3.2 s** — but that frame only represents **0.34 s** of
playback.  The frontend drains the frame 10× faster than the model produces it,
causing continuously disjointed audio.

`generate()` returns the entire chunk at once (~50 000 samples in one call).
SoX immediately has enough input to emit multiple full frames, the frontend
receives a contiguous 1–2 s block and playback is smooth.

**Lesson**: use `generate()` for `rate != 1.0`; use `generate_stream()` for
`rate == 1.0` (no SoX involved, tokens are emitted token-by-token directly).

---

## 2. Rust parallel look-ahead generation (implemented)

### Key discovery: `TTSModel::generate()` takes `&self`

The pocket-tts crate's `TTSModel::generate()` takes a **shared reference**
(`&self`), not `&mut self`.  `ModelState` is `HashMap<String, HashMap<String,
Tensor>>` — `Clone + Send`.  The `&mut self` requirement on our
`stream_synthesize` was only because of the voice state cache in
`LocalKyutaiRuntime`, not a pocket-tts limitation.

This means we can share the model via `Arc<TTSModel>` and run **multiple
`generate()` calls in parallel** on different CPU cores.

### Architecture: three CPU cores in parallel

```
Main thread:    [gen C0] [sox+emit C0]   [join C1 → sox+emit C1] [join C2 → sox+emit C2]
Look-ahead:              [gen C1]         [gen C2]                 [gen C3]
SoX process:             [tempo C0]       [tempo C1]               [tempo C2]
```

The implementation:
1. `LocalKyutaiRuntime.model` is stored as `Arc<TTSModel>`
2. Chunk 0 is pre-submitted to a background thread before the loop
3. Each iteration: join the current chunk's thread, immediately spawn the next
   chunk's generation, then push/drain/emit the current chunk via SoX
4. The look-ahead thread runs `model.generate()` on a separate CPU core

### Gap analysis

Old sequential approach:
```
Gap = T_gen(chunk N+1)  — no overlap, entire gen time is dead air
```

New parallel look-ahead:
```
Gap = max(0, T_gen(N+1) − T_sox(N) − T_emit(N))
```

Since T_gen ≈ 2–4 s and T_sox + T_emit ≈ 0.2–0.5 s, the look-ahead thread has
the **full** SoX + emit duration of the current chunk to get a head start on the
next chunk's generation.  For multi-chunk texts, the look-ahead completes most
or all of the next chunk's generation before the main thread needs it.

### `!Send` closure fix (prerequisite)

The original `on_chunk` closure captured `sent_any_chunk: bool` by `&mut`,
making it `FnMut` and `!Send`.  Fixed by returning `had_audio: bool` from
`stream_synthesize` instead.  The closure is now `Fn + Send + 'static`.

---

## 4. Python look-ahead pipelining (implemented)

The Python backend (`tts-engine/src/tts_engine/jobs.py`) was rearchitected to
overlap chunk N+1 synthesis with SoX processing of chunk N.

### Before (sequential)

```
synthesize(C0) → sox(C0) → emit(C0) → synthesize(C1) → sox(C1) → emit(C1) …
```

### After (pipelined)

```
submit synthesize(C0)
for each chunk i:
    await synthesize(C_i)          ← CPU / GPU working
    submit synthesize(C_{i+1})     ← immediately, before awaiting SoX
    await sox(C_i)                 ← SoX subprocess on another core
    emit(C_i)
```

`synthesize_chunk` uses C extensions (pocket-tts) or PyTorch (Qwen), both of
which release the GIL during inference.  `_apply_playback_controls` (SoX) is a
subprocess call.  Both run as genuine OS-level threads on separate CPU cores
under `loop.run_in_executor` / `asyncio.to_thread`.

**Savings per chunk** = min(T_syn, T_sox) — whichever finishes first no longer
adds latency to the chain.  For Qwen GPU (T_syn ≈ 0.5 s, T_sox ≈ 0.3 s) this
eliminates most of the inter-chunk gap.

### GPU compatibility

`QwenCustomVoiceSynthesizer.synthesize_chunk` ends with `.detach().cpu().numpy()`,
which forces a full CUDA stream synchronisation before returning.  The GPU is
completely idle and all model state is released before chunk N+1's synthesis
begins, so there is no concurrent model access and no race condition.

---

## 5. Why the `generate_stream → SoX` combination fails (regression documented)

An earlier attempt unified both paths to always use `generate_stream()` and feed
tokens to SoX incrementally.  This caused continuously disjointed audio at any
`rate > 1.0`.  Root cause: see §1 above.  The fix was to revert to the two-path
structure with `generate()` for `rate_active`.

---

## 6. Summary of architectural constraints

| Concern | Rust (pocket-tts) | Python (pocket-tts / Qwen) |
|---|---|---|
| Parallel chunk generation | ✅ `Arc<TTSModel>` + `thread::spawn` | ❌ Sequential (GIL released, but still one call at a time) |
| SoX parallel to generation | ✅ Background process | ✅ Implemented via `asyncio.to_thread` |
| Look-ahead synthesis | ✅ Next chunk generated on background thread | ✅ Implemented via `loop.run_in_executor` |
| Streaming tokens at rate=1 | ✅ `generate_stream()` | N/A (full chunk returned) |
| Streaming tokens at rate>1 | ❌ SoX starvation (must use `generate()`) | ❌ SoX starvation (same reason) |
