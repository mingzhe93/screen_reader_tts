# Real CPython WASI sandbox checks

Production pins (official metadata checked 2026-10-19):

- [CPython WASI 3.14.7 / SDK24 release](https://api.github.com/repos/brettcannon/cpython-wasi-build/releases/tags/v3.14.7): `python-3.14.7-wasi_sdk-24.zip`, 14,291,017 bytes, SHA256 `2e064d3fb8172471d39d741348efa722349c40b96301f69968dff714999c584b`.
- [tzdata2026.5 PyPI metadata](https://pypi.org/pypi/tzdata/2026.5/json): `tzdata-2026.5-py2.py3-none-any.whl`, 347,996 bytes, SHA256 `b683bd1b6659ddcd810ff02ad09ba821d4bf1065072805063eb35c49617905ac`.
- [Wasmtime36.0.18 source](https://github.com/bytecodealliance/wasmtime/tree/v36.0.18): the pinned LTS patch; async Preview1 functions and epoch deadlines are both required.

Download the two archives from these sources; set `VOICEREADER_SANDBOX_PYTHON_ARCHIVE` and `VOICEREADER_SANDBOX_TZDATA_ARCHIVE` to their absolute paths. Or, if the app has already installed the runtime, set `VOICEREADER_SANDBOX_INSTALLED_DIR` to its `assistant/sandbox` folder (on macOS, `~/Library/Application Support/com.voicereader.desktop/data/assistant/sandbox`); the test copies it into a temporary folder and never changes it. Run:

```sh
cargo test --manifest-path src-tauri/Cargo.toml --features build-base real_runtime_isolation_limits_cancellation_and_lifecycle -- --ignored --nocapture
```

The test checks both archive hashes, uses the production Wasmtime implementation, compiles once, records compile/interpreter timings, and verifies timezone data, standard-library arithmetic, host filesystem/write/network/environment isolation, CPU/sleep timeout and cancellation, lifecycle leases, memory/output/descriptor caps, and recovery using a fresh store after traps. No host Python process executes guest code.

The installer retains the complete CPython `LICENSE` and the wheel's `tzdata-2026.5.dist-info/licenses/` (Apache2.0 and database public-domain notice). These are inside the runtime directory beside the exact installed artifacts; deletion removes them with the runtime. No compiled native module is written to disk.

Measured on the M2 Max (2026-10-19), debug app with the Cargo dev profiles optimizing the runtime compiler: five tests passed, including real network installation, in 4.76 seconds. Session compilation 1.019 seconds; first compile plus interpreter 1.207 seconds; warm fresh interpreter 0.102 seconds. Installed files total 41,380,140 bytes. CPU and sleep deadlines stopped at 0.512/0.504 seconds for a 0.5-second limit; cancellation cases returned at 0.310/0.309 seconds after a flag scheduled at 0.3 seconds. `/usr/bin/time -l` reported maximum process RSS 564,215,808 bytes while the test progressively filled guest memory to its limit (the 256 MiB limit covers guest linear memory, not compiler/module/host allocations). Binary-size impact and Windows execution remain unmeasured.

The first unoptimized, serial debug compile took 55.408 seconds; parallel compilation and optimizing the compiler's dev dependencies reduce that to about one second without persisting executable native caches. Initial compilation cannot be interrupted. Its worker retains the operation lease, caches the completed module once, and checks Stop before starting guest code. An executing guest yields through epoch ticks and is cancelled by dropping the asynchronous run, including blocked WASI sleep calls. The WASI host resource table is independently capped at 128 entries; guest Wasm tables, instances, memory, stdout and stderr also have explicit bounds.
