#!/usr/bin/env bash
# Convert a BitNet GGUF into I2_S weights with Q6_K token embeddings.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BITNET_DIR="${BITNET_DIR:-$REPO_ROOT/third_party/bitnet}"
BITNET_HOST_BUILD="${BITNET_HOST_BUILD:-$BITNET_DIR/build}"

usage() {
    cat >&2 <<EOF
Usage:
  $(basename "$0") <input.gguf> [output.gguf]

Example:
  $(basename "$0") \\
    third_party/bitnet/models/BitNet-b1.58-2B-4T/ggml-model-f32.gguf \\
    third_party/bitnet/models/BitNet-b1.58-2B-4T/ggml-model-i2_s-embed-q6_k.gguf

The input can be the public I2_S GGUF. The output keeps BitNet I2_S layer
weights and changes token embeddings to Q6_K.
EOF
}

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    usage
    exit 2
fi

INPUT_GGUF="$1"
if [ ! -f "$INPUT_GGUF" ]; then
    echo "Input GGUF not found: $INPUT_GGUF" >&2
    exit 1
fi

if [ "$#" -eq 2 ]; then
    OUTPUT_GGUF="$2"
else
    OUTPUT_GGUF="${INPUT_GGUF%.gguf}-i2_s-embed-q6_k.gguf"
fi

if [ ! -f "$BITNET_DIR/CMakeLists.txt" ]; then
    echo "BitNet checkout not found: $BITNET_DIR" >&2
    exit 1
fi

QUANTIZE_BIN="$BITNET_HOST_BUILD/bin/llama-quantize"
if [ ! -x "$QUANTIZE_BIN" ]; then
    RELEASE_QUANTIZE_BIN="$BITNET_HOST_BUILD/bin/Release/llama-quantize"
    if [ -x "$RELEASE_QUANTIZE_BIN" ]; then
        QUANTIZE_BIN="$RELEASE_QUANTIZE_BIN"
    fi
fi

if [ ! -x "$QUANTIZE_BIN" ]; then
    echo ">>> Building host llama-quantize in $BITNET_HOST_BUILD <<<"
    cmake -S "$BITNET_DIR" -B "$BITNET_HOST_BUILD" \
        -DCMAKE_BUILD_TYPE=Release \
        -DBITNET_X86_TL2=OFF
    cmake --build "$BITNET_HOST_BUILD" --target llama-quantize --config Release -j "$(nproc)"

    QUANTIZE_BIN="$BITNET_HOST_BUILD/bin/llama-quantize"
    if [ ! -x "$QUANTIZE_BIN" ]; then
        QUANTIZE_BIN="$BITNET_HOST_BUILD/bin/Release/llama-quantize"
    fi
fi

if [ ! -x "$QUANTIZE_BIN" ]; then
    echo "llama-quantize was not built at the expected path." >&2
    exit 1
fi

mkdir -p "$(dirname "$OUTPUT_GGUF")"

echo ">>> Quantizing BitNet GGUF embeddings to Q6_K <<<"
echo "Input:     $INPUT_GGUF"
echo "Output:    $OUTPUT_GGUF"
echo "Quantizer: $QUANTIZE_BIN"
"$QUANTIZE_BIN" \
    --token-embedding-type Q6_K \
    "$INPUT_GGUF" \
    "$OUTPUT_GGUF" \
    I2_S \
    1 \
    1

ls -lh "$OUTPUT_GGUF"
echo ">>> Done. Boot with this GGUF and verify embedding_q6_k=1 in the RAM shell log. <<<"
