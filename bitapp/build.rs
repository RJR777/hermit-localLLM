use std::path::{Path, PathBuf};

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn require_file(path: &Path, label: &str) {
    if !path.is_file() {
        panic!("missing {label}: {}", path.display());
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=native/bitapp_runtime.cpp");
    println!("cargo:rerun-if-changed=native/bitapp_pthread_c.c");
    println!("cargo:rerun-if-changed=native/bitnet/hermit_bitnet.cpp");
    println!("cargo:rerun-if-changed=native/bitnet/hermit_bitnet.h");
    println!("cargo:rerun-if-env-changed=BITNET_DIR");
    println!("cargo:rerun-if-env-changed=BITNET_HERMIT_BUILD");
    println!("cargo:rerun-if-env-changed=HERMIT_PREFIX");
    println!("cargo:rerun-if-env-changed=HERMIT_TOOLCHAIN_ROOT");
    println!("cargo:rerun-if-env-changed=BITAPP_ENABLE_SMP_PTHREAD");
    println!("cargo:rerun-if-env-changed=BITNET_N_THREADS");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_is_hermit = target_os == "hermit";
    if !target_is_hermit {
        return;
    }

    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir
        .parent()
        .expect("bitapp must live directly under the workspace root")
        .to_path_buf();

    let bitnet_dir =
        env_path("BITNET_DIR").unwrap_or_else(|| workspace_root.join("third_party/bitnet"));
    let bitnet_build = env_path("BITNET_HERMIT_BUILD")
        .unwrap_or_else(|| workspace_root.join("target/bitnet-hermit-static"));
    let handoff_dir = manifest_dir.join("native/bitnet");
    let ggml_lib_dir = bitnet_build.join("3rdparty/llama.cpp/ggml/src");
    let llama_lib_dir = bitnet_build.join("3rdparty/llama.cpp/src");
    let hermit_toolchain = env_path("HERMIT_TOOLCHAIN_ROOT")
        .or_else(|| env_path("HERMIT_PREFIX"))
        .expect("HERMIT_TOOLCHAIN_ROOT or HERMIT_PREFIX must point at the Hermit toolchain root");
    let hermit_lib_dir = hermit_toolchain.join("x86_64-hermit/lib");
    let hermit_gcc_dir = hermit_toolchain.join("lib/gcc/x86_64-hermit/6.3.0");

    require_file(&bitnet_dir.join("CMakeLists.txt"), "BitNet CMake project");
    require_file(
        &handoff_dir.join("hermit_bitnet.cpp"),
        "BitNet Hermit runtime handoff source",
    );
    require_file(
        &handoff_dir.join("hermit_bitnet.h"),
        "BitNet Hermit runtime handoff header",
    );
    require_file(&llama_lib_dir.join("libllama.a"), "Hermit libllama.a");
    require_file(&ggml_lib_dir.join("libggml.a"), "Hermit libggml.a");
    println!(
        "cargo:rerun-if-changed={}",
        llama_lib_dir.join("libllama.a").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        ggml_lib_dir.join("libggml.a").display()
    );

    let mut native = cc::Build::new();
    native
        .cpp(true)
        .file("native/bitapp_runtime.cpp")
        .file(handoff_dir.join("hermit_bitnet.cpp"))
        .include("native")
        .include(handoff_dir)
        .include(bitnet_dir.join("3rdparty/llama.cpp/include"))
        .include(bitnet_dir.join("3rdparty/llama.cpp/ggml/include"))
        .include(bitnet_dir.join("include"))
        .define("NDEBUG", None)
        .define("_GNU_SOURCE", None)
        .define("_POSIX_C_SOURCE", "199309L")
        .define("CLOCK_MONOTONIC", "1")
        .define("PATH_MAX", "4096")
        .flag_if_supported("-std=c++17")
        .flag_if_supported("-ffreestanding")
        .flag_if_supported("-fno-stack-protector")
        .warnings(false);

    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64") {
        native
            .flag_if_supported("-mavx")
            .flag_if_supported("-mavx2")
            .flag_if_supported("-mfma")
            .flag_if_supported("-mf16c")
            .flag_if_supported("-mno-red-zone");
    }

    native.define("BITAPP_TARGET_HERMIT", None);
    let enable_smp_pthread = std::env::var("BITAPP_ENABLE_SMP_PTHREAD").as_deref() == Ok("1");
    if enable_smp_pthread {
        native.define("BITAPP_ENABLE_SMP_PTHREAD", None);
    }
    println!(
        "cargo:rustc-env=BITAPP_ENABLE_SMP_PTHREAD={}",
        if enable_smp_pthread { "1" } else { "0" }
    );
    println!(
        "cargo:rustc-env=BITNET_N_THREADS={}",
        std::env::var("BITNET_N_THREADS").unwrap_or_else(|_| "1".to_string())
    );

    native.compile("bitapp_native");

    let mut pthread_c = cc::Build::new();
    pthread_c
        .file("native/bitapp_pthread_c.c")
        .include("native")
        .define("_GNU_SOURCE", None)
        .warnings(false);
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64") {
        pthread_c.flag_if_supported("-mno-red-zone");
    }
    pthread_c.compile("bitapp_pthread_c");

    println!("cargo:rustc-link-search=native={}", llama_lib_dir.display());
    println!("cargo:rustc-link-search=native={}", ggml_lib_dir.display());
    println!(
        "cargo:rustc-link-search=native={}",
        hermit_lib_dir.display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        hermit_gcc_dir.display()
    );
    println!("cargo:rustc-link-lib=static=bitapp_native");
    println!("cargo:rustc-link-lib=static=bitapp_pthread_c");
    println!("cargo:rustc-link-lib=static=llama");
    println!("cargo:rustc-link-lib=static=ggml");
    println!("cargo:rustc-link-lib=static=stdc++");
    println!("cargo:rustc-link-lib=static=m");
    println!("cargo:rustc-link-lib=static=c");
    println!("cargo:rustc-link-lib=static=gcc");
}
