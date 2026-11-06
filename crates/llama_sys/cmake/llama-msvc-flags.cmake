# Included by llama-toolchain.cmake through CMAKE_PROJECT_INCLUDE, i.e. after
# project() has detected the compiler. A toolchain file is read before compiler
# detection, so an `if(MSVC)` check there is always false.
#
# cl.exe ignores -O3 (D9002) and would compile at /Od, so force the MSVC
# spelling here. The Release runtime is the dynamic CRT, matching rustc's
# default MSVC target.
if(MSVC)
  set(CMAKE_C_FLAGS_RELEASE "/O2 /Ob3 /DNDEBUG" CACHE STRING "" FORCE)
  set(CMAKE_CXX_FLAGS_RELEASE "/O2 /Ob3 /DNDEBUG" CACHE STRING "" FORCE)
endif()
