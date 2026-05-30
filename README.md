# Hermit localLLM

This repository is a proof of concept for running a local inference engine from
a Rust unikernel on bare metal. The goal is to prove that a Rust unikernel can boot on real hardware, 
bring up the required device stack, ingest a local GGUF model, and run inference without a host operating system.

I wanted to build a full Rust stack but I could not find a inference engine as robust as llama.cpp

The current bare-metal target is a Dell Inspiron 7000-class machine. Other
hardware platforms may need code changes in boot, ACPI/PCI discovery, storage,
USB, networking, interrupt routing, timing, or CPU feature handling before they
boot and run reliably.

The folder structure is:

- `bitapp/`: the Hermit application and BitNet runtime bridge.
- `bitapp/native/bitnet/`: the C++ handoff code for llama.cpp/BitNet.
- `third_party/bitnet/`: the BitNet source checkout used for static library builds.
- `kernel/`, `loader/`, `hermit-rs/`, and `usb-oxide/`: Hermit and device support used by the bare-metal image.

## Requirements

The build needs Rust, the Hermit cross toolchain, and native build tools:

- Rustup with `nightly-2026-02-01` for the Hermit app build.
- Rustup with `nightly-2026-04-01` for the Hermit loader build.
- Hermit cross compilers:
  - `x86_64-hermit-gcc`
  - `x86_64-hermit-g++`
  - `x86_64-hermit-ar`
- Native build tools:
  - `cmake`
  - a host C/C++ compiler
  - `make`
  - standard Unix utilities used by the rebuild script
- A mounted EFI/USB partition if you want the script to deploy boot files.
- A GGUF model on model storage for runtime inference.

## Acknowledgements

This project is built on the Hermit-OS ecosystem. The Hermit kernel, loader,
runtime crates, and related Rust support are from Hermit-OS and retain their
upstream MIT OR Apache-2.0 licensing.

Upstream references:

- Hermit project: https://github.com/hermit-os
- Hermit for Rust: https://github.com/hermit-os/hermit-rs
- Hermit kernel: https://github.com/hermit-os/kernel
- Hermit loader: https://github.com/hermit-os/loader

## Rebuild

Run:

```sh
./rebuild_hermit.sh
```

The script prompts for every machine-specific path:

- `HERMIT_LOCAL_LLM_ROOT` (repository root containing `bitapp/`, `kernel/`, `loader/`, and `third_party/bitnet/`)
- `BITNET_DIR` (BitNet source checkout used to build the llama.cpp/BitNet static libraries)
- `BITNET_HERMIT_BUILD` (build output directory for Hermit-target BitNet static libraries)
- `HERMIT_TOOLCHAIN_ROOT` (Hermit cross-toolchain root containing `bin/x86_64-hermit-gcc`, `g++`, and `ar`)
- `HERMIT_CC` (optional full path to `x86_64-hermit-gcc`; leave blank to derive from `HERMIT_TOOLCHAIN_ROOT` or `PATH`)
- `HERMIT_CXX` (optional full path to `x86_64-hermit-g++`; leave blank to derive from `HERMIT_TOOLCHAIN_ROOT` or `PATH`)
- `HERMIT_AR` (optional full path to `x86_64-hermit-ar`; leave blank to derive from `HERMIT_TOOLCHAIN_ROOT` or `PATH`)
- `MEDIA_PATH` (optional mounted EFI/USB partition where `BOOTX64.EFI` and `hermit-app` will be copied)
- `BITNET_HERMIT_APP` (optional prebuilt Hermit app image to deploy instead of the image built here)

For unattended local rebuilds, set the variables in the environment and add:

```sh
HERMIT_LOCAL_LLM_ASSUME_DEFAULTS=1 ./rebuild_hermit.sh
```

The script builds:

```text
loader/target/x86_64-unknown-uefi/release/hermit-loader.efi
target/x86_64-unknown-hermit/release/bitapp
```

If `MEDIA_PATH` is set and exists, it copies them to:

```text
EFI/BOOT/BOOTX64.EFI
EFI/hermit/hermit-app
```
