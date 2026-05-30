#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BITNET_DIR="${BITNET_DIR:-$REPO_ROOT/third_party/bitnet}"
BUILD_DIR="${BUILD_DIR:-$REPO_ROOT/target/bitnet-hermit-static}"
HERMIT_PREFIX="${HERMIT_PREFIX:-}"
CC_BIN="${CC_BIN:-$HERMIT_PREFIX/bin/x86_64-hermit-gcc}"
CXX_BIN="${CXX_BIN:-$HERMIT_PREFIX/bin/x86_64-hermit-g++}"
TOOLCHAIN_FILE="$REPO_ROOT/target/bitnet-hermit-toolchain.cmake"

if [ -z "$HERMIT_PREFIX" ]; then
    echo "HERMIT_PREFIX is required and must point at the Hermit cross-toolchain root." >&2
    exit 1
fi

if [ ! -f "$BITNET_DIR/CMakeLists.txt" ]; then
    echo "BitNet checkout not found at $BITNET_DIR"
    exit 1
fi

if [ ! -f "$BITNET_DIR/3rdparty/llama.cpp/CMakeLists.txt" ]; then
    echo "BitNet llama.cpp submodule is missing."
    exit 1
fi

if [ ! -f "$BITNET_DIR/include/bitnet-lut-kernels.h" ]; then
    cp "$BITNET_DIR/preset_kernels/bitnet_b1_58-3B/bitnet-lut-kernels-tl2.h" \
        "$BITNET_DIR/include/bitnet-lut-kernels.h"
fi

if [ ! -x "$CC_BIN" ]; then
    echo "Missing Hermit C compiler: $CC_BIN"
    exit 1
fi

if [ ! -x "$CXX_BIN" ]; then
    echo "Missing Hermit C++ compiler: $CXX_BIN"
    exit 1
fi

mkdir -p "$REPO_ROOT/target"
cat > "$TOOLCHAIN_FILE" <<EOF
set(CMAKE_SYSTEM_NAME Hermit)
set(CMAKE_SYSTEM_PROCESSOR x86_64)
set(CMAKE_C_COMPILER "$CC_BIN")
set(CMAKE_CXX_COMPILER "$CXX_BIN")
set(CMAKE_FIND_ROOT_PATH "$HERMIT_PREFIX/x86_64-hermit")
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
EOF

cmake --log-level=ERROR -S "$BITNET_DIR" -B "$BUILD_DIR" \
    -DCMAKE_TOOLCHAIN_FILE="$TOOLCHAIN_FILE" \
    -DCMAKE_BUILD_TYPE=Release \
    -DGIT_EXECUTABLE=/usr/bin/false \
    -DBUILD_SHARED_LIBS=OFF \
    -DLLAMA_BUILD_SERVER=OFF \
    -DLLAMA_BUILD_TESTS=OFF \
    -DLLAMA_BUILD_EXAMPLES=OFF \
    -DLLAMA_CURL=OFF \
    -DGGML_OPENMP=OFF \
    -DGGML_ALL_WARNINGS=OFF \
    -DGGML_FATAL_WARNINGS=OFF \
    -DLLAMA_ALL_WARNINGS=OFF \
    -DLLAMA_FATAL_WARNINGS=OFF \
    -DGGML_NATIVE=OFF \
    -DGGML_AVX=ON \
    -DGGML_AVX2=ON \
    -DGGML_FMA=ON \
    -DGGML_F16C=ON \
    -DGGML_CUDA=OFF \
    -DGGML_METAL=OFF \
    -DGGML_VULKAN=OFF \
    -DGGML_BLAS=OFF \
    -DBITNET_X86_TL2=OFF \
    -DCMAKE_C_FLAGS_RELEASE="-O3 -DNDEBUG -D_POSIX_C_SOURCE=199309L -DCLOCK_MONOTONIC=1 -static -ffreestanding -fno-stack-protector -mno-red-zone -w" \
    -DCMAKE_CXX_FLAGS_RELEASE="-O3 -DNDEBUG -D_POSIX_C_SOURCE=199309L -DCLOCK_MONOTONIC=1 -DPATH_MAX=4096 -static -ffreestanding -fno-stack-protector -mno-red-zone -w" \
    -DCMAKE_EXE_LINKER_FLAGS="-static"

cmake --build "$BUILD_DIR" --target llama ggml --parallel "${JOBS:-$(nproc)}"

echo "Hermit static probe build directory: $BUILD_DIR"
