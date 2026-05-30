#!/usr/bin/env bash
# Build and optionally deploy the Hermit local LLM bare-metal image.
set -euo pipefail
unset RUSTFLAGS

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Path variables prompted by this script:
# HERMIT_LOCAL_LLM_ROOT (repository root containing bitapp/, kernel/, loader/, and third_party/bitnet/)
# BITNET_DIR (BitNet source checkout used to build the llama.cpp/BitNet static libraries)
# BITNET_HERMIT_BUILD (build output directory for Hermit-target BitNet static libraries)
# HERMIT_TOOLCHAIN_ROOT (Hermit cross-toolchain root containing bin/x86_64-hermit-gcc, g++, and ar)
# HERMIT_CC (optional full path to x86_64-hermit-gcc; leave blank to derive from HERMIT_TOOLCHAIN_ROOT or PATH)
# HERMIT_CXX (optional full path to x86_64-hermit-g++; leave blank to derive from HERMIT_TOOLCHAIN_ROOT or PATH)
# HERMIT_AR (optional full path to x86_64-hermit-ar; leave blank to derive from HERMIT_TOOLCHAIN_ROOT or PATH)
# MEDIA_PATH (optional mounted EFI/USB partition where BOOTX64.EFI and hermit-app will be copied)
# BITNET_HERMIT_APP (optional prebuilt Hermit app image to deploy instead of the image built here)

prompt_path() {
    local var="$1"
    local default_value="$2"
    local description="$3"
    local required="${4:-required}"
    local current="${!var:-$default_value}"
    local answer

    if [ -t 0 ] && [ "${HERMIT_LOCAL_LLM_ASSUME_DEFAULTS:-0}" != "1" ]; then
        echo "$var ($description)"
        if [ -n "$current" ]; then
            read -r -p "  Path [$current]: " answer
            current="${answer:-$current}"
        else
            read -r -p "  Path: " answer
            current="$answer"
        fi
    fi

    if [ "$required" = "required" ] && [ -z "$current" ]; then
        echo "$var is required. $description" >&2
        exit 1
    fi

    printf -v "$var" '%s' "$current"
    export "$var"
}

prompt_value() {
    local var="$1"
    local default_value="$2"
    local description="$3"
    local current="${!var:-$default_value}"
    local answer

    if [ -t 0 ] && [ "${HERMIT_LOCAL_LLM_ASSUME_DEFAULTS:-0}" != "1" ]; then
        echo "$var ($description)"
        read -r -p "  Value [$current]: " answer
        current="${answer:-$current}"
    fi

    printf -v "$var" '%s' "$current"
    export "$var"
}

prompt_path \
    "HERMIT_LOCAL_LLM_ROOT" \
    "$SCRIPT_DIR" \
    "repository root containing bitapp/, kernel/, loader/, and third_party/bitnet/" \
    "required"

prompt_path \
    "BITNET_DIR" \
    "$HERMIT_LOCAL_LLM_ROOT/third_party/bitnet" \
    "BitNet source checkout used to build the llama.cpp/BitNet static libraries" \
    "required"

prompt_path \
    "BITNET_HERMIT_BUILD" \
    "$HERMIT_LOCAL_LLM_ROOT/target/bitnet-hermit-static" \
    "build output directory for Hermit-target BitNet static libraries" \
    "required"

prompt_path \
    "HERMIT_TOOLCHAIN_ROOT" \
    "${HERMIT_PREFIX:-}" \
    "Hermit cross-toolchain root containing bin/x86_64-hermit-gcc, g++, and ar" \
    "required"

prompt_path \
    "HERMIT_CC" \
    "" \
    "optional full path to x86_64-hermit-gcc; leave blank to derive from HERMIT_TOOLCHAIN_ROOT or PATH" \
    "optional"

prompt_path \
    "HERMIT_CXX" \
    "" \
    "optional full path to x86_64-hermit-g++; leave blank to derive from HERMIT_TOOLCHAIN_ROOT or PATH" \
    "optional"

prompt_path \
    "HERMIT_AR" \
    "" \
    "optional full path to x86_64-hermit-ar; leave blank to derive from HERMIT_TOOLCHAIN_ROOT or PATH" \
    "optional"

prompt_path \
    "MEDIA_PATH" \
    "" \
    "optional mounted EFI/USB partition where BOOTX64.EFI and hermit-app will be copied" \
    "optional"

prompt_path \
    "BITNET_HERMIT_APP" \
    "" \
    "optional prebuilt Hermit app image to deploy instead of the image built here" \
    "optional"

prompt_value \
    "BITAPP_ENABLE_SMP_PTHREAD" \
    "${BITAPP_ENABLE_SMP_PTHREAD:-1}" \
    "1 enables the SMP pthread shim; 0 disables it"

prompt_value \
    "BITNET_N_THREADS" \
    "${BITNET_N_THREADS:-4}" \
    "number of CPU worker threads requested by the BitNet runtime"

prompt_value \
    "HERMIT_CPU_FREQ_MHZ" \
    "${HERMIT_CPU_FREQ_MHZ:-}" \
    "optional CPU frequency override in MHz for runtime performance logging; leave blank for auto"

APP_TOOLCHAIN="${APP_TOOLCHAIN:-nightly-2026-02-01}"
LOADER_TOOLCHAIN="${LOADER_TOOLCHAIN:-nightly-2026-04-01}"

WORKSPACE_ROOT="$HERMIT_LOCAL_LLM_ROOT"
KERNEL_DIR="$WORKSPACE_ROOT/kernel"
LOADER_DIR="$WORKSPACE_ROOT/loader"
FINAL_LOADER="$LOADER_DIR/target/x86_64-unknown-uefi/release/hermit-loader.efi"
FINAL_BITAPP="$WORKSPACE_ROOT/target/x86_64-unknown-hermit/release/bitapp"
HERMIT_GGML="$BITNET_HERMIT_BUILD/3rdparty/llama.cpp/ggml/src/libggml.a"
HERMIT_LLAMA="$BITNET_HERMIT_BUILD/3rdparty/llama.cpp/src/libllama.a"
BITNET_HERMIT_STATIC_SCRIPT="$WORKSPACE_ROOT/tools/build-bitnet-hermit-static.sh"
BITNET_RUNTIME_HANDOFF="$WORKSPACE_ROOT/bitapp/native/bitnet"

resolve_hermit_tool() {
    local tool="$1"
    local override="$2"
    local candidate

    if [ -n "$override" ]; then
        if [ -x "$override" ]; then
            echo "$override"
            return 0
        fi
        echo "Invalid override for $tool: $override" >&2
        return 1
    fi

    if [ -n "$HERMIT_TOOLCHAIN_ROOT" ]; then
        candidate="${HERMIT_TOOLCHAIN_ROOT%/}/bin/$tool"
        if [ -x "$candidate" ]; then
            echo "$candidate"
            return 0
        fi
    fi

    candidate="$(command -v "$tool" 2>/dev/null || true)"
    if [ -n "$candidate" ] && [ -x "$candidate" ]; then
        echo "$candidate"
        return 0
    fi

    echo "Could not find $tool." >&2
    echo "Set HERMIT_TOOLCHAIN_ROOT or the matching override path shown in the prompts." >&2
    return 1
}

if [ ! -f "$LOADER_DIR/Cargo.toml" ]; then
    echo "Missing Hermit loader checkout: $LOADER_DIR" >&2
    exit 1
fi

if [ ! -f "$KERNEL_DIR/Cargo.toml" ]; then
    echo "Missing Hermit kernel checkout: $KERNEL_DIR" >&2
    exit 1
fi

if [ ! -f "$WORKSPACE_ROOT/bitapp/Cargo.toml" ]; then
    echo "Missing BitNet Hermit app crate: $WORKSPACE_ROOT/bitapp" >&2
    exit 1
fi

if [ ! -f "$BITNET_DIR/CMakeLists.txt" ]; then
    echo "Missing BitNet checkout: $BITNET_DIR" >&2
    exit 1
fi

if [ ! -x "$BITNET_HERMIT_STATIC_SCRIPT" ]; then
    echo "Missing BitNet Hermit static build script: $BITNET_HERMIT_STATIC_SCRIPT" >&2
    exit 1
fi

if [ ! -f "$BITNET_RUNTIME_HANDOFF/hermit_bitnet.cpp" ]; then
    echo "Missing BitNet runtime handoff source: $BITNET_RUNTIME_HANDOFF/hermit_bitnet.cpp" >&2
    exit 1
fi

if [ ! -f "$BITNET_RUNTIME_HANDOFF/hermit_bitnet.h" ]; then
    echo "Missing BitNet runtime handoff header: $BITNET_RUNTIME_HANDOFF/hermit_bitnet.h" >&2
    exit 1
fi

if ! HERMIT_CC_PATH="$(resolve_hermit_tool "x86_64-hermit-gcc" "$HERMIT_CC")"; then
    echo "Missing Hermit C compiler (x86_64-hermit-gcc)." >&2
    exit 1
fi

if ! HERMIT_CXX_PATH="$(resolve_hermit_tool "x86_64-hermit-g++" "$HERMIT_CXX")"; then
    echo "Missing Hermit C++ compiler (x86_64-hermit-g++)." >&2
    exit 1
fi

if ! HERMIT_AR_PATH="$(resolve_hermit_tool "x86_64-hermit-ar" "$HERMIT_AR")"; then
    echo "Missing Hermit ar (x86_64-hermit-ar)." >&2
    exit 1
fi

echo ">>> Hermit local LLM rebuild <<<"
echo "Repository root:   $WORKSPACE_ROOT"
echo "Kernel source:     $KERNEL_DIR"
echo "Loader source:     $LOADER_DIR"
echo "BitNet source:     $BITNET_DIR"
echo "BitNet build:      $BITNET_HERMIT_BUILD"
echo "USB target:        ${MEDIA_PATH:-not set; build only}"
echo "Hermit toolchain:  ${HERMIT_TOOLCHAIN_ROOT:-resolved from PATH or explicit tool overrides}"
echo "Hermit CC:         $HERMIT_CC_PATH"
echo "Hermit CXX:        $HERMIT_CXX_PATH"
echo "Hermit AR:         $HERMIT_AR_PATH"
echo "BitNet threads:    $BITNET_N_THREADS"
echo "SMP pthread shim:  $BITAPP_ENABLE_SMP_PTHREAD"
if [ -n "$HERMIT_CPU_FREQ_MHZ" ]; then
    echo "CPU freq override: ${HERMIT_CPU_FREQ_MHZ} MHz"
else
    echo "CPU freq override: auto"
fi

echo ">>> Building HermitOS Loader (UEFI) <<<"
cd "$LOADER_DIR"
cargo +"$LOADER_TOOLCHAIN" xtask build --target x86_64-uefi --release

echo ">>> Building BitNet static Hermit libraries <<<"
BITNET_CMAKE_CACHE="$BITNET_HERMIT_BUILD/CMakeCache.txt"
if [ -f "$BITNET_CMAKE_CACHE" ]; then
    BITNET_CACHED_SOURCE="$(sed -n 's/^CMAKE_HOME_DIRECTORY:INTERNAL=//p' "$BITNET_CMAKE_CACHE")"
    if [ -n "$BITNET_CACHED_SOURCE" ] && [ "$BITNET_CACHED_SOURCE" != "$BITNET_DIR" ]; then
        echo ">>> Resetting stale BitNet CMake cache: $BITNET_CACHED_SOURCE -> $BITNET_DIR <<<"
        rm -rf "$BITNET_HERMIT_BUILD"
    fi
fi

BITNET_DIR="$BITNET_DIR" \
BUILD_DIR="$BITNET_HERMIT_BUILD" \
HERMIT_PREFIX="$HERMIT_TOOLCHAIN_ROOT" \
CC_BIN="$HERMIT_CC_PATH" \
CXX_BIN="$HERMIT_CXX_PATH" \
"$BITNET_HERMIT_STATIC_SCRIPT"

echo ">>> Building BitNet Hermit app and linked kernel <<<"
cd "$WORKSPACE_ROOT"
echo ">>> Removing previous BitNet app/link artifacts <<<"
rm -f \
  "$WORKSPACE_ROOT/target/x86_64-unknown-hermit/release/bitapp" \
  "$WORKSPACE_ROOT/target/x86_64-unknown-hermit/release/bitapp.d" \
  "$WORKSPACE_ROOT"/target/x86_64-unknown-hermit/release/deps/bitapp-*
rm -rf "$WORKSPACE_ROOT"/target/x86_64-unknown-hermit/release/.fingerprint/bitapp-*

HERMIT_MANIFEST_DIR="$KERNEL_DIR" \
CC_x86_64_unknown_hermit="$HERMIT_CC_PATH" \
CXX_x86_64_unknown_hermit="$HERMIT_CXX_PATH" \
AR_x86_64_unknown_hermit="$HERMIT_AR_PATH" \
CFLAGS_x86_64_unknown_hermit="-mno-red-zone" \
CXXFLAGS_x86_64_unknown_hermit="-mno-red-zone" \
BITAPP_ENABLE_SMP_PTHREAD="$BITAPP_ENABLE_SMP_PTHREAD" \
BITNET_N_THREADS="$BITNET_N_THREADS" \
BITNET_DIR="$BITNET_DIR" \
BITNET_HERMIT_BUILD="$BITNET_HERMIT_BUILD" \
HERMIT_TOOLCHAIN_ROOT="$HERMIT_TOOLCHAIN_ROOT" \
RUSTFLAGS="-C relocation-model=static" \
cargo +"$APP_TOOLCHAIN" build \
  -Zbuild-std=std,panic_abort \
  -Zbuild-std-features=compiler-builtins-mem \
  --target x86_64-unknown-hermit \
  --no-default-features \
  --features "newlib acpi pci pci-ids smp tcp udp dhcpv4 rtl8152" \
  --release \
  -p bitapp

if [ ! -f "$FINAL_LOADER" ]; then
    echo "Missing built loader: $FINAL_LOADER" >&2
    exit 1
fi

if [ ! -f "$FINAL_BITAPP" ]; then
    echo "Missing built BitNet app/kernel image: $FINAL_BITAPP" >&2
    exit 1
fi

if [ ! -f "$HERMIT_GGML" ] || [ ! -f "$HERMIT_LLAMA" ]; then
    echo "Missing Hermit static BitNet libraries." >&2
    echo "Expected:" >&2
    echo "  $HERMIT_GGML" >&2
    echo "  $HERMIT_LLAMA" >&2
    exit 1
fi

if [ -n "$BITNET_HERMIT_APP" ] && [ ! -f "$BITNET_HERMIT_APP" ]; then
    echo "BITNET_HERMIT_APP is set but missing: $BITNET_HERMIT_APP" >&2
    exit 1
fi

DEPLOY_BITAPP="${BITNET_HERMIT_APP:-$FINAL_BITAPP}"

if [ -n "$MEDIA_PATH" ] && [ -d "$MEDIA_PATH" ]; then
    echo ">>> Deploying boot files to mounted EFI/USB partition at $MEDIA_PATH <<<"
    mkdir -p "$MEDIA_PATH/EFI/BOOT"
    mkdir -p "$MEDIA_PATH/EFI/hermit"

    cp "$FINAL_LOADER" "$MEDIA_PATH/EFI/BOOT/BOOTX64.EFI"
    cp "$DEPLOY_BITAPP" "$MEDIA_PATH/EFI/hermit/hermit-app"
    if [ -n "$HERMIT_CPU_FREQ_MHZ" ]; then
        printf -- "-freq %s\n" "$HERMIT_CPU_FREQ_MHZ" > "$MEDIA_PATH/EFI/hermit/hermit-bootargs"
    else
        rm -f "$MEDIA_PATH/EFI/hermit/hermit-bootargs"
    fi

    sync

    LOADER_HASH="$(sha256sum "$FINAL_LOADER" | awk '{print $1}')"
    USB_LOADER_HASH="$(sha256sum "$MEDIA_PATH/EFI/BOOT/BOOTX64.EFI" | awk '{print $1}')"

    if [ "$LOADER_HASH" != "$USB_LOADER_HASH" ]; then
        echo "!!! Loader deploy hash mismatch !!!"
        echo "  local: $LOADER_HASH"
        echo "  usb:   $USB_LOADER_HASH"
        exit 1
    fi
    echo ">>> Verified loader hash: $LOADER_HASH <<<"

    APP_HASH="$(sha256sum "$DEPLOY_BITAPP" | awk '{print $1}')"
    USB_APP_HASH="$(sha256sum "$MEDIA_PATH/EFI/hermit/hermit-app" | awk '{print $1}')"
    if [ "$APP_HASH" != "$USB_APP_HASH" ]; then
        echo "!!! BitNet app deploy hash mismatch !!!"
        echo "  local: $APP_HASH"
        echo "  usb:   $USB_APP_HASH"
        exit 1
    fi
    echo ">>> Verified BitNet app/kernel hash: $APP_HASH <<<"
    echo ">>> SUCCESS: EFI/USB partition has the Hermit loader and BitNet app/kernel image. <<<"
elif [ -n "$MEDIA_PATH" ]; then
    echo "!!! MEDIA_PATH was provided but does not exist: $MEDIA_PATH !!!"
    echo "Binaries built successfully at:"
    echo "  Loader:       $FINAL_LOADER"
    echo "  BitNet image: $FINAL_BITAPP"
else
    echo ">>> No MEDIA_PATH provided; build only. <<<"
    echo "Binaries built successfully at:"
    echo "  Loader:       $FINAL_LOADER"
    echo "  BitNet image: $FINAL_BITAPP"
fi
