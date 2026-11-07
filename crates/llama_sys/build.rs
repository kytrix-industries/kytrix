//! Builds the pinned llama.cpp checkout and generates the provider's FFI.
//!
//! The crate owns the native build so `cargo build` is sufficient: the script
//! initializes the `crates/llama_sys/llama.cpp` submodule (pinned to v0.9.0) with
//! a shallow checkout when it is missing, CMake builds that tree, and the
//! resulting static libraries are linked through this script. Only the llama and
//! ggml targets are built; `llama-common`, its HTTP client and the C++ shims are
//! not needed by the C API surface the provider uses.

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use sha2::{Digest, Sha256};

/// The llama.cpp C API functions the provider calls.
const FUNCTIONS: &[&str] = &[
    "llama_backend_init",
    "llama_log_set",
    "llama_supports_gpu_offload",
    "llama_model_default_params",
    "llama_model_load_from_file",
    "llama_model_free",
    "llama_model_get_vocab",
    "llama_vocab_n_tokens",
    "llama_vocab_is_eog",
    "llama_vocab_get_attr",
    "llama_tokenize",
    "llama_token_to_piece",
    "llama_context_default_params",
    "llama_init_from_model",
    "llama_free",
    "llama_decode",
    "llama_batch_init",
    "llama_batch_free",
    "llama_get_memory",
    "llama_memory_clear",
    "llama_sampler_chain_default_params",
    "llama_sampler_chain_init",
    "llama_sampler_chain_add",
    "llama_sampler_init_penalties",
    "llama_sampler_init_top_k",
    "llama_sampler_init_top_p",
    "llama_sampler_init_temp",
    "llama_sampler_init_dist",
    "llama_sampler_sample",
    "llama_sampler_free",
];

/// The C types referenced by those functions and by the parameter structs.
const TYPES: &[&str] = &[
    "llama_model_params",
    "llama_context_params",
    "llama_batch",
    "llama_sampler_chain_params",
    "llama_token_attr",
    "llama_flash_attn_type",
    "llama_token",
    "llama_pos",
    "llama_seq_id",
    "llama_model",
    "llama_context",
    "llama_vocab",
    "llama_sampler",
    "llama_memory_t",
    "llama_progress_callback",
    "llama_model_kv_override",
    "llama_model_tensor_buft_override",
    "ggml_log_level",
    "ggml_log_callback",
    "ggml_abort_callback",
    "ggml_backend_sched_eval_callback",
];

/// The enum constants the provider names directly.
const VARIABLES: &[&str] = &[
    "LLAMA_TOKEN_ATTR_UNKNOWN",
    "LLAMA_TOKEN_ATTR_UNUSED",
    "LLAMA_TOKEN_ATTR_NORMAL",
    "LLAMA_TOKEN_ATTR_CONTROL",
    "LLAMA_TOKEN_ATTR_USER_DEFINED",
    "LLAMA_TOKEN_ATTR_BYTE",
    "LLAMA_TOKEN_ATTR_NORMALIZED",
    "LLAMA_TOKEN_ATTR_LSTRIP",
    "LLAMA_TOKEN_ATTR_RSTRIP",
    "LLAMA_TOKEN_ATTR_SINGLE_WORD",
    "LLAMA_FLASH_ATTN_TYPE_AUTO",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for path in [
        "cmake/llama-toolchain.cmake",
        "cmake/llama-msvc-flags.cmake",
        "llama.cpp",
        "llama.cpp/CMakeLists.txt",
        "llama.cpp/include/llama.h",
        "llama.cpp/ggml/CMakeLists.txt",
        "llama.cpp/ggml/src/CMakeLists.txt",
        "llama.cpp/src/CMakeLists.txt",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    for variable in [
        "KYTRIX_CUDA_ARCHITECTURES",
        "CUDA_PATH",
        "CUDA_HOME",
        "VULKAN_SDK",
        "LIBCLANG_PATH",
        "LOCALAPPDATA",
        "CARGO_TARGET_DIR",
        "CC",
        "CXX",
        "CMAKE_C_COMPILER",
        "CMAKE_CXX_COMPILER",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }

    let plan = Plan::resolve();
    let source = source_directory();
    let toolchain = Toolchain::detect(&plan);
    let build = build_directory(&plan, &source, &toolchain);
    configure_and_build(&plan, &source, &build, &toolchain);
    emit_links(&plan, &build, &toolchain);
    generate_bindings(&source);
}

/// What this build needs from llama.cpp and the host.
#[derive(Debug)]
struct Plan {
    target: String,
    openmp: bool,
    cuda: bool,
    vulkan: bool,
    metal: bool,
    cuda_architectures: Option<String>,
}

impl Plan {
    fn resolve() -> Self {
        let target = env::var("TARGET").expect("TARGET");
        let host = env::var("HOST").expect("HOST");
        assert_eq!(
            target, host,
            "llama_sys: cross-compilation is not supported; build on the target host"
        );
        assert!(
            !target.contains("windows") || target.contains("msvc"),
            "llama_sys: Windows builds require the MSVC target"
        );
        assert!(
            !target.contains("windows") || !target.contains("aarch64"),
            "llama_sys: Windows ARM64 is not a supported native target"
        );

        let openmp = feature("openmp");
        let cuda = feature("cuda");
        let vulkan = feature("vulkan");
        let metal = feature("metal");

        if cuda {
            assert!(
                target.contains("windows") || target.contains("linux"),
                "llama_sys: cuda is only supported on Windows and Linux"
            );
        }
        if vulkan {
            assert!(
                target.contains("windows") || target.contains("linux"),
                "llama_sys: vulkan is only supported on Windows and Linux"
            );
        }
        if metal {
            assert!(
                target.contains("apple"),
                "llama_sys: metal is only supported on macOS"
            );
        }

        let cuda_architectures = env::var("KYTRIX_CUDA_ARCHITECTURES")
            .ok()
            .filter(|value| !value.is_empty());
        if let Some(value) = &cuda_architectures {
            validate_cuda_architectures(value);
        }

        Self {
            target,
            openmp,
            cuda,
            vulkan,
            metal,
            cuda_architectures,
        }
    }

    fn feature_names(&self) -> Vec<&'static str> {
        let mut features = Vec::with_capacity(4);
        if self.openmp {
            features.push("openmp");
        }
        if self.cuda {
            features.push("cuda");
        }
        if self.vulkan {
            features.push("vulkan");
        }
        if self.metal {
            features.push("metal");
        }
        features
    }
}

fn feature(name: &str) -> bool {
    env::var_os(format!("CARGO_FEATURE_{}", name.to_uppercase())).is_some()
}

fn validate_cuda_architectures(value: &str) {
    let valid = !value.is_empty() && value.split(';').all(valid_cuda_architecture);
    assert!(
        valid,
        "llama_sys: KYTRIX_CUDA_ARCHITECTURES must be a semicolon-separated list of SM numbers \
         (for example `75;86;120a`) or a CMake keyword (`native`, `all`, `all-major`), got \
         `{value}`"
    );
}

/// Accepts CMake's `NN`, `NNa`, `NNf`, `NN-real`, `NN-virtual` forms and its
/// three aggregate keywords.
fn valid_cuda_architecture(architecture: &str) -> bool {
    if matches!(architecture, "native" | "all" | "all-major") {
        return true;
    }
    let (base, mode) = architecture
        .split_once('-')
        .map_or((architecture, ""), |(base, mode)| (base, mode));
    if !matches!(mode, "" | "real" | "virtual") {
        return false;
    }
    let digits = base
        .strip_suffix('a')
        .or_else(|| base.strip_suffix('f'))
        .unwrap_or(base);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// The checked-out llama.cpp source.
///
/// A missing checkout is initialized from the submodule at the commit the
/// superproject records. The fetch is shallow: compiling needs the sources, not
/// llama.cpp's history.
fn source_directory() -> PathBuf {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let source = manifest.join("llama.cpp");
    if source.join("include").join("llama.h").is_file() {
        return source;
    }

    eprintln!("llama_sys: initializing the pinned llama.cpp submodule (shallow checkout)");
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(manifest)
        // `tools/ui` contains paths over MAX_PATH; without this Git for Windows
        // aborts the checkout and leaves a partially populated tree.
        .args(["-c", "core.longpaths=true", "submodule", "update"])
        .args(["--init", "--depth", "1", "--", "llama.cpp"]);
    run(&mut command, "initialize the pinned llama.cpp submodule");

    assert!(
        source.join("include").join("llama.h").is_file(),
        "llama_sys: the llama.cpp submodule at {} is still incomplete after \
         `git submodule update --init --depth 1`",
        source.display()
    );
    source
}

/// The CMake build directory, keyed so a feature or architecture change never
/// reuses a cache that was configured for another plan.
fn build_directory(plan: &Plan, source: &Path, toolchain: &Toolchain) -> PathBuf {
    let mut hasher = Sha256::new();
    for value in [
        plan.target.as_str(),
        plan.cuda_architectures
            .as_deref()
            .unwrap_or("llama.cpp-default"),
        toolchain.cmake_version.as_str(),
        source.to_string_lossy().as_ref(),
    ] {
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    for feature in plan.feature_names() {
        hasher.update(feature.as_bytes());
        hasher.update([0]);
    }
    hasher.update(if toolchain.ninja.is_some() {
        b"ninja".as_slice()
    } else {
        b"cmake-default".as_slice()
    });
    hasher.update([0]);
    if let Some(cuda) = &toolchain.cuda {
        hasher.update(cuda.root.to_string_lossy().as_bytes());
        hasher.update([0]);
    }
    if let Some(vulkan) = &toolchain.vulkan {
        hasher.update(vulkan.sdk.to_string_lossy().as_bytes());
        hasher.update([0]);
    }
    if let Some(libomp) = &toolchain.libomp {
        hasher.update(libomp.to_string_lossy().as_bytes());
        hasher.update([0]);
    }
    for variable in ["CC", "CXX", "CMAKE_C_COMPILER", "CMAKE_CXX_COMPILER"] {
        if let Some(value) = env::var_os(variable) {
            hasher.update(value.to_string_lossy().as_bytes());
            hasher.update([0]);
        }
    }
    if let Ok(commit) = Command::new("git")
        .args(["-C"])
        .arg(source)
        .args(["rev-parse", "HEAD"])
        .output()
        && commit.status.success()
    {
        hasher.update(&commit.stdout);
    }
    for path in [
        Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"),
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("cmake")
            .join("llama-toolchain.cmake"),
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("cmake")
            .join("llama-msvc-flags.cmake"),
    ] {
        if let Ok(contents) = fs::read(&path) {
            hasher.update(contents);
        }
    }
    let key: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let key = &key[..16];

    if plan.target.contains("windows") {
        let base = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| target_directory().join("llama"));
        base.join("kytrix-llama").join(key)
    } else {
        target_directory().join("llama").join(key)
    }
}

fn target_directory() -> PathBuf {
    let workspace = Path::new(&env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate lives two directories below the workspace root")
        .to_path_buf();
    env::var_os("CARGO_TARGET_DIR").map_or_else(
        || workspace.join("target"),
        |directory| {
            let directory = PathBuf::from(directory);
            if directory.is_absolute() {
                directory
            } else {
                workspace.join(directory)
            }
        },
    )
}

/// Tools and paths the configure step needs.
struct Toolchain {
    cmake: PathBuf,
    ninja: Option<PathBuf>,
    cmake_version: String,
    cuda: Option<Cuda>,
    vulkan: Option<Vulkan>,
    libomp: Option<PathBuf>,
    libclang: Option<PathBuf>,
    cuda_architectures: Option<String>,
}

impl Toolchain {
    fn detect(plan: &Plan) -> Self {
        let cmake = find_in_path("cmake").expect(
            "llama_sys: cmake is required; install CMake and put it on PATH or build on a runner \
             with CMake",
        );
        let ninja = find_in_path("ninja");
        let cmake_version = command_output(&cmake, &["--version"]);
        let cuda = plan.cuda.then(|| Cuda::detect(&plan.target));
        let vulkan = plan.vulkan.then(Vulkan::detect);
        let libomp = plan.openmp.then(Libomp::detect).flatten();
        let libclang = discover_libclang();
        if let Some(path) = &libclang {
            // The build script is single-threaded before bindgen runs.
            unsafe {
                env::set_var("LIBCLANG_PATH", path);
            }
        }

        Self {
            cmake,
            ninja,
            cmake_version,
            cuda,
            vulkan,
            libomp,
            libclang,
            cuda_architectures: plan.cuda_architectures.clone(),
        }
    }
}

/// Finds the CUDA toolkit without requiring a version env var.
///
/// `CUDA_PATH`/`CUDA_HOME` win. Otherwise Windows uses the highest version in
/// the standard NVIDIA toolkit directory, and Unix uses `/usr/local/cuda`.
fn discover_cuda_root(target: &str) -> PathBuf {
    if let Some(root) = env::var_os("CUDA_PATH").or_else(|| env::var_os("CUDA_HOME")) {
        return PathBuf::from(root);
    }
    if !target.contains("windows") {
        return PathBuf::from("/usr/local/cuda");
    }

    let base = env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Program Files"))
        .join("NVIDIA GPU Computing Toolkit")
        .join("CUDA");
    let mut candidates = Vec::new();
    if let Ok(entries) = fs::read_dir(&base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("bin").join("nvcc.exe").is_file() {
                candidates.push(path);
            }
        }
    }
    candidates.sort_by_key(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(version_key)
            .unwrap_or_default()
    });

    candidates.pop().unwrap_or_else(|| {
        panic!(
            "llama_sys: cuda needs a toolkit with nvcc under {}; install it or set CUDA_PATH",
            base.display()
        )
    })
}

fn version_key(version: &str) -> Vec<u32> {
    version
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

struct Cuda {
    root: PathBuf,
    nvcc: PathBuf,
    library_directories: Vec<PathBuf>,
}

impl Cuda {
    fn detect(target: &str) -> Self {
        let root = discover_cuda_root(target);
        let nvcc = root.join("bin").join(if target.contains("windows") {
            "nvcc.exe"
        } else {
            "nvcc"
        });
        if !nvcc.is_file() {
            panic!(
                "llama_sys: cuda needs nvcc at {}, install the CUDA toolkit or set CUDA_PATH",
                nvcc.display()
            );
        }

        let mut library_directories = Vec::with_capacity(2);
        if target.contains("windows") {
            library_directories.push(root.join("lib").join("x64"));
        } else {
            library_directories.push(root.join("lib64"));
            library_directories.push(root.join("lib64").join("stubs"));
        }
        if !library_directories
            .iter()
            .any(|directory| directory.is_dir())
        {
            panic!(
                "llama_sys: cuda libraries are missing under {}",
                root.display()
            );
        }

        Self {
            root,
            nvcc,
            library_directories,
        }
    }
}

struct Vulkan {
    sdk: PathBuf,
    library_directory: PathBuf,
    glslc: PathBuf,
    spirv_headers: PathBuf,
}

impl Vulkan {
    fn detect() -> Self {
        let sdk = env::var_os("VULKAN_SDK")
            .map(PathBuf::from)
            .or_else(default_vulkan_sdk)
            .expect("llama_sys: vulkan needs VULKAN_SDK set to a Vulkan SDK installation");
        let (library_directory, glslc) = if cfg!(windows) {
            (sdk.join("Lib"), sdk.join("Bin").join("glslc.exe"))
        } else {
            (sdk.join("lib"), sdk.join("bin").join("glslc"))
        };
        if !glslc.is_file() {
            panic!(
                "llama_sys: vulkan needs glslc at {}, install the Vulkan SDK or set VULKAN_SDK",
                glslc.display()
            );
        }
        let spirv_headers = sdk.join("SPIRV-Headers");

        Self {
            sdk,
            library_directory,
            glslc,
            spirv_headers,
        }
    }
}

fn default_vulkan_sdk() -> Option<PathBuf> {
    if cfg!(windows) {
        let base = Path::new("C:\\VulkanSDK");
        let mut versions = fs::read_dir(base)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.join("Bin").join("glslc.exe").is_file())
            .collect::<Vec<_>>();
        versions.sort();
        versions.pop()
    } else {
        None
    }
}

struct Libomp;

impl Libomp {
    fn detect() -> Option<PathBuf> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        ["/opt/homebrew/opt/libomp", "/usr/local/opt/libomp"]
            .into_iter()
            .map(PathBuf::from)
            .find(|prefix| prefix.join("lib").join("libomp.dylib").is_file())
    }
}

fn configure_and_build(plan: &Plan, source: &Path, build: &Path, toolchain: &Toolchain) {
    if toolchain.ninja.is_some() && build.join("CMakeCache.txt").is_file() {
        let cache = fs::read_to_string(build.join("CMakeCache.txt")).unwrap_or_default();
        if !cache.contains("CMAKE_GENERATOR:INTERNAL=Ninja") {
            fs::remove_dir_all(build).unwrap_or_else(|error| {
                panic!(
                    "llama_sys: could not remove the stale CMake tree {}: {error}",
                    build.display()
                )
            });
        }
    }

    fs::create_dir_all(build).unwrap_or_else(|error| {
        panic!(
            "llama_sys: could not create build directory {}: {error}",
            build.display()
        )
    });

    let mut command = Command::new(&toolchain.cmake);
    command
        .arg("-S")
        .arg(source)
        .arg("-B")
        .arg(build)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg("-DBUILD_SHARED_LIBS=OFF")
        .arg("-DLLAMA_BUILD_COMMON=OFF")
        .arg("-DLLAMA_BUILD_TESTS=OFF")
        .arg("-DLLAMA_BUILD_TOOLS=OFF")
        .arg("-DLLAMA_BUILD_EXAMPLES=OFF")
        .arg("-DLLAMA_BUILD_SERVER=OFF")
        .arg("-DLLAMA_BUILD_APP=OFF")
        .arg("-DLLAMA_BUILD_MTMD=OFF")
        .arg("-DLLAMA_OPENSSL=OFF")
        .arg("-DGGML_BUILD_TESTS=OFF")
        .arg("-DGGML_BUILD_EXAMPLES=OFF")
        .arg("-DGGML_NATIVE=OFF")
        .arg(format!("-DGGML_OPENMP={}", on_off(plan.openmp)))
        .arg(format!("-DGGML_CUDA={}", on_off(plan.cuda)))
        .arg(format!("-DGGML_VULKAN={}", on_off(plan.vulkan)))
        .arg(format!("-DGGML_METAL={}", on_off(plan.metal)))
        .arg("-DCMAKE_CUDA_RUNTIME_LIBRARY=Shared")
        .arg("-DCMAKE_EXPORT_COMPILE_COMMANDS=OFF");

    if plan.target.contains("aarch64") {
        command.arg("-DGGML_CPU_ARM_ARCH=armv8-a");
    }

    let toolchain_file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("cmake")
        .join("llama-toolchain.cmake");
    command.arg(format!(
        "-DCMAKE_TOOLCHAIN_FILE={}",
        toolchain_file.display()
    ));

    if let Some(ninja) = &toolchain.ninja {
        command.arg("-G").arg("Ninja");
        command.arg(format!("-DCMAKE_MAKE_PROGRAM={}", ninja.display()));
    }
    if let Some(sccache) = find_in_path("sccache") {
        command
            .arg(format!("-DCMAKE_C_COMPILER_LAUNCHER={}", sccache.display()))
            .arg(format!(
                "-DCMAKE_CXX_COMPILER_LAUNCHER={}",
                sccache.display()
            ));
        if plan.cuda {
            command.arg(format!(
                "-DCMAKE_CUDA_COMPILER_LAUNCHER={}",
                sccache.display()
            ));
        }
    }

    if let Some(cuda) = &toolchain.cuda {
        command.arg(format!("-DCMAKE_CUDA_COMPILER={}", cuda.nvcc.display()));
        if let Some(architectures) = &toolchain.cuda_architectures {
            command.arg(format!("-DCMAKE_CUDA_ARCHITECTURES={architectures}"));
        }
        command
            .env("CUDA_PATH", &cuda.root)
            .env("CUDA_HOME", &cuda.root);
    }
    if let Some(vulkan) = &toolchain.vulkan {
        command
            .env("VULKAN_SDK", &vulkan.sdk)
            .env("SPIRV-Headers_DIR", &vulkan.spirv_headers)
            .env("PATH", prepend_path(&vulkan.glslc));
    }
    if let Some(libomp) = &toolchain.libomp {
        command.arg(format!("-DCMAKE_PREFIX_PATH={}", libomp.display()));
    }
    if cfg!(windows) {
        command.env("CL", msvc_cl());
    }

    run(&mut command, "configure llama.cpp");

    let mut build_command = Command::new(&toolchain.cmake);
    build_command
        .arg("--build")
        .arg(build)
        .arg("--config")
        .arg("Release")
        .arg("--target")
        .arg("llama")
        .arg("--parallel")
        .arg(
            std::thread::available_parallelism()
                .map_or(1, std::num::NonZeroUsize::get)
                .to_string(),
        );
    if cfg!(windows) {
        build_command.env("CL", msvc_cl());
    }
    run(&mut build_command, "build llama.cpp");

    if plan.openmp {
        let cache = fs::read_to_string(build.join("CMakeCache.txt")).unwrap_or_default();
        assert!(
            cache.contains("GGML_OPENMP_ENABLED:INTERNAL=ON"),
            "llama_sys: OpenMP was requested but CMake did not find it; install libomp/openmp \
             and rerun"
        );
    }
}

/// Ensures cl.exe reads UTF-8 source files even under a non-UTF-8 code page.
fn msvc_cl() -> OsString {
    let mut cl = env::var_os("CL").unwrap_or_default();
    if !cl.to_string_lossy().contains("/utf-8") {
        if !cl.is_empty() {
            cl.push(" ");
        }
        cl.push("/utf-8");
    }
    cl
}

fn run(command: &mut Command, action: &str) {
    let display = format!("{command:?}");
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("llama_sys: could not {action}: {error}\n{display}"));
    assert!(
        status.success(),
        "llama_sys: failed to {action} ({status})\n{display}"
    );
}

fn emit_links(plan: &Plan, build: &Path, toolchain: &Toolchain) {
    let mut libraries = vec![find_library(build, "llama"), find_library(build, "ggml")];
    if plan.cuda {
        libraries.push(find_library(build, "ggml-cuda"));
    }
    if plan.vulkan {
        libraries.push(find_library(build, "ggml-vulkan"));
    }
    if plan.metal {
        libraries.push(find_library(build, "ggml-metal"));
    }
    libraries.push(find_library(build, "ggml-cpu"));
    libraries.push(find_library(build, "ggml-base"));

    let mut directories = Vec::new();
    for library in &libraries {
        let directory = library
            .parent()
            .expect("a library path always has a parent")
            .to_path_buf();
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    for directory in &directories {
        println!("cargo:rustc-link-search=native={}", directory.display());
    }
    for library in &libraries {
        println!("cargo:rustc-link-lib=static={}", library_stem(library));
        println!("cargo:rerun-if-changed={}", library.display());
    }

    let target = &plan.target;
    if target.contains("windows") {
        println!("cargo:rustc-link-lib=advapi32");
        if plan.openmp {
            println!("cargo:rustc-link-lib=dylib=vcomp");
        }
    } else if target.contains("linux") {
        println!("cargo:rustc-link-lib=dylib=stdc++");
        println!("cargo:rustc-link-lib=dl");
        if plan.openmp {
            println!("cargo:rustc-link-lib=gomp");
        }
        if plan.vulkan {
            println!("cargo:rustc-link-lib=vulkan");
        }
    } else if target.contains("apple") {
        println!("cargo:rustc-link-lib=dylib=c++");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=framework=Accelerate");
        if plan.metal {
            println!("cargo:rustc-link-lib=framework=Metal");
            println!("cargo:rustc-link-lib=framework=MetalKit");
        }
        if plan.openmp {
            if let Some(libomp) = &toolchain.libomp {
                println!(
                    "cargo:rustc-link-search=native={}",
                    libomp.join("lib").display()
                );
            }
            println!("cargo:rustc-link-lib=dylib=omp");
        }
    }

    if let Some(cuda) = &toolchain.cuda {
        for directory in &cuda.library_directories {
            println!("cargo:rustc-link-search=native={}", directory.display());
        }
        for library in ["cudart", "cublas", "cublasLt", "cuda"] {
            println!("cargo:rustc-link-lib={library}");
        }
    }
    if let Some(vulkan) = &toolchain.vulkan {
        println!(
            "cargo:rustc-link-search=native={}",
            vulkan.library_directory.display()
        );
        if target.contains("windows") {
            println!("cargo:rustc-link-lib=vulkan-1");
        }
    }
    if let Some(libclang) = &toolchain.libclang {
        println!("cargo:rerun-if-changed={}", libclang.display());
    }
}

fn find_library(build: &Path, stem: &str) -> PathBuf {
    let names = if cfg!(windows) {
        vec![format!("{stem}.lib")]
    } else {
        vec![format!("lib{stem}.a"), format!("{stem}.a")]
    };
    let mut candidates = Vec::new();
    collect_libraries(build, &names, &mut candidates);
    candidates.sort_by_key(|path| path.components().count());
    candidates.into_iter().next().unwrap_or_else(|| {
        panic!(
            "llama_sys: could not find the static `{stem}` library under {}",
            build.display()
        )
    })
}

fn collect_libraries(directory: &Path, names: &[String], found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_libraries(&path, names, found);
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| names.iter().any(|candidate| candidate == name))
        {
            found.push(path);
        }
    }
}

fn library_stem(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("library");
    let name = name.strip_prefix("lib").unwrap_or(name);
    name.split('.').next().unwrap_or(name).to_owned()
}

fn generate_bindings(source: &Path) {
    let include = source.join("include");
    let ggml_include = source.join("ggml").join("include");
    let mut builder = bindgen::Builder::default()
        .header_contents("wrapper.h", "#include \"llama.h\"")
        .clang_arg(format!("-I{}", include.display()))
        .clang_arg(format!("-I{}", ggml_include.display()))
        .use_core()
        .layout_tests(true)
        .prepend_enum_name(false);

    for function in FUNCTIONS {
        builder = builder.allowlist_function(function);
    }
    for ty in TYPES {
        builder = builder.allowlist_type(ty);
    }
    for variable in VARIABLES {
        builder = builder.allowlist_var(variable);
    }

    let bindings = builder.generate().unwrap_or_else(|error| {
        panic!(
            "llama_sys: bindgen failed: {error}; install libclang and set LIBCLANG_PATH if needed"
        )
    });
    let output = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("bindings.rs");
    bindings
        .write_to_file(&output)
        .unwrap_or_else(|error| panic!("llama_sys: could not write {}: {error}", output.display()));
}

fn find_in_path(program: &str) -> Option<PathBuf> {
    let extensions: Vec<&str> = if cfg!(windows) {
        vec!["exe", "cmd", "bat"]
    } else {
        vec![""]
    };
    for directory in env::split_paths(&env::var_os("PATH")?) {
        for extension in &extensions {
            let candidate = if extension.is_empty() {
                directory.join(program)
            } else {
                directory.join(format!("{program}.{extension}"))
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn discover_libclang() -> Option<PathBuf> {
    if let Some(path) = env::var_os("LIBCLANG_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
        if contains_libclang(&path) {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();
    if cfg!(windows) {
        for base in [
            env::var_os("ProgramFiles").map(PathBuf::from),
            env::var_os("ProgramFiles(x86)").map(PathBuf::from),
            env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("Programs")),
        ]
        .into_iter()
        .flatten()
        {
            candidates.push(base.join("LLVM").join("bin"));
        }
    } else if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from("/opt/homebrew/opt/llvm/lib"));
        candidates.push(PathBuf::from("/usr/local/opt/llvm/lib"));
        candidates.push(PathBuf::from("/Library/Developer/CommandLineTools/usr/lib"));
        let developer_dir = command_output(Path::new("xcode-select"), &["-p"]);
        if !developer_dir.is_empty() {
            candidates.push(
                PathBuf::from(developer_dir)
                    .join("Toolchains")
                    .join("XcodeDefault.xctoolchain")
                    .join("usr")
                    .join("lib"),
            );
        }
    } else {
        candidates.push(PathBuf::from("/usr/lib/x86_64-linux-gnu"));
        if let Ok(entries) = fs::read_dir("/usr/lib") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("llvm-") {
                    candidates.push(entry.path().join("lib"));
                }
            }
        }
    }
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path) {
            if directory.join("libclang.dll").is_file()
                || directory.join("libclang.so").is_file()
                || directory.join("libclang.dylib").is_file()
            {
                candidates.push(directory);
            }
        }
    }

    candidates
        .into_iter()
        .find(|directory| contains_libclang(directory))
}

/// Whether `directory` holds libclang under any of the platform's names.
fn contains_libclang(directory: &Path) -> bool {
    if !directory.is_dir() {
        return false;
    }
    for name in ["libclang.dll", "libclang.so", "libclang.dylib"] {
        if directory.join(name).is_file() {
            return true;
        }
    }
    fs::read_dir(directory).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with("libclang"))
    })
}

fn prepend_path(program: &Path) -> OsString {
    let mut paths = Vec::new();
    if let Some(parent) = program.parent() {
        paths.push(parent.to_path_buf());
    }
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    env::join_paths(paths).unwrap_or_default()
}

fn command_output(program: &Path, arguments: &[&str]) -> String {
    Command::new(program)
        .args(arguments)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn on_off(value: bool) -> &'static str {
    if value { "ON" } else { "OFF" }
}
