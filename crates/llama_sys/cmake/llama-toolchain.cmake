# Included through CMAKE_TOOLCHAIN_FILE, i.e. before project(). Keep this file
# to settings that do not depend on the detected compiler; compiler-specific
# flags live in llama-msvc-flags.cmake, which project() includes afterwards.
set(CMAKE_PROJECT_INCLUDE
    "${CMAKE_CURRENT_LIST_DIR}/llama-msvc-flags.cmake"
    CACHE FILEPATH "" FORCE)

# ggml otherwise builds a fat 9-10 arch CUDA binary. build.rs supplies a
# distribution list through KYTRIX_CUDA_ARCHITECTURES.
if(DEFINED ENV{KYTRIX_CUDA_ARCHITECTURES} AND NOT "$ENV{KYTRIX_CUDA_ARCHITECTURES}" STREQUAL "")
  set(CMAKE_CUDA_ARCHITECTURES "$ENV{KYTRIX_CUDA_ARCHITECTURES}" CACHE STRING "" FORCE)
endif()

# Keep the CUDA runtime dynamic; llama_sys's build script supplies the link libs.
set(CMAKE_CUDA_RUNTIME_LIBRARY "Shared" CACHE STRING "" FORCE)
