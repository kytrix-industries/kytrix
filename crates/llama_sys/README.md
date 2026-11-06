# llama_sys

Raw FFI and the native build for the pinned llama.cpp checkout in
`crates/llama_sys/llama.cpp` (v0.9.0).

`build.rs` owns the whole native build: it configures CMake for the selected
Cargo features, builds the `llama` target and its `ggml` dependencies, links the
static libraries into the final binary, and runs bindgen over `llama.h`. There is
no separate `xtask` step and no CMake in `ai_provider`.

Builds are native: cross-compiling llama.cpp is not supported yet. Windows ARM64
is intentionally rejected here; `kytrix` omits the dependency on that target.

## Prerequisites

- the submodule: `git submodule update --init crates/llama_sys/llama.cpp`
- CMake 3.20+ and a C++17 compiler
- Ninja (recommended; CMake's default generator works too)
- libclang for bindgen (`LIBCLANG_PATH` if it is not in a standard location)
- OpenMP: MSVC `vcomp` on Windows, GCC `gomp` on Linux, Homebrew `libomp` on macOS
- on Windows, a Visual Studio developer shell so CMake's Ninja build can find
  `cl.exe`, `INCLUDE` and `LIB`

Additional backends:

- `cuda`: CUDA toolkit. `CUDA_PATH`/`CUDA_HOME` select it explicitly; otherwise
  the highest standard Windows install or `/usr/local/cuda` is used. On Windows
  the runtime DLLs are under `<CUDA_PATH>\bin\x64`, which must be on `PATH` when
  running tests or binaries
- `vulkan`: Vulkan SDK (`VULKAN_SDK`) with `glslc` and SPIRV-Headers
- `metal`: macOS only, enabled by `ai_provider` automatically on that target

## Features

```text
default = openmp
cuda
vulkan
metal
```

`KYTRIX_CUDA_ARCHITECTURES` is optional and controls SM architectures, not the
CUDA toolkit version. The toolkit version comes from `CUDA_PATH`/`CUDA_HOME`
(and its `nvcc`). When the variable is unset, llama.cpp's CMake
chooses its own CUDA architecture list for the installed toolkit; this workspace
keeps `GGML_NATIVE=OFF` for portable binaries, so that default is a mixed
virtual/real list rather than the build machine's GPU. Set it only to override
the choice, for example `120a` for one Blackwell GPU, `75;86;120a` for a
distribution list, or `native` for the local GPU.

## Cache

The CMake tree is keyed by source commit, workspace path, target, features,
CUDA architectures and toolchain files. Locations:

- Windows: `%LOCALAPPDATA%\kytrix-llama\<key>`
- other hosts: `target/llama/<key>`

Deleting that directory forces a clean native build; changing a feature creates a
new key instead of reusing a cache configured for another backend.
