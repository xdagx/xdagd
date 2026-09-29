//! Builds the vendored RandomX library with the `cc` crate (no CMake needed).

use std::path::Path;

fn main() {
    let src = Path::new("RandomX/src");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let is_x86 = arch == "x86_64";
    let is_arm = arch == "aarch64";

    let c_files = ["argon2_ref.c", "virtual_memory.c", "argon2_core.c", "reciprocal.c", "blake2/blake2b.c"];
    let cpp_files = [
        "aes_hash.cpp",
        "bytecode_machine.cpp",
        "cpu.cpp",
        "dataset.cpp",
        "soft_aes.cpp",
        "vm_interpreted.cpp",
        "allocator.cpp",
        "assembly_generator_x86.cpp",
        "instruction.cpp",
        "randomx.cpp",
        "superscalar.cpp",
        "vm_compiled.cpp",
        "vm_interpreted_light.cpp",
        "blake2_generator.cpp",
        "instructions_portable.cpp",
        "virtual_machine.cpp",
        "vm_compiled_light.cpp",
    ];

    let mut c = cc::Build::new();
    c.warnings(false).opt_level(3).pic(true);
    for f in c_files {
        c.file(src.join(f));
    }
    if is_x86 {
        c.flag_if_supported("-maes");
        c.file(src.join("jit_compiler_x86_static.S"));
    }
    if is_arm {
        c.flag_if_supported("-march=armv8-a+crypto");
        c.file(src.join("jit_compiler_a64_static.S"));
    }
    c.compile("randomx_c");

    // argon2 SIMD variants need per-file flags
    let mut s = cc::Build::new();
    s.warnings(false).opt_level(3).pic(true).file(src.join("argon2_ssse3.c"));
    if is_x86 {
        s.flag_if_supported("-mssse3");
    }
    s.compile("randomx_ssse3");
    let mut a = cc::Build::new();
    a.warnings(false).opt_level(3).pic(true).file(src.join("argon2_avx2.c"));
    if is_x86 {
        a.flag_if_supported("-mavx2");
    }
    a.compile("randomx_avx2");

    let mut cpp = cc::Build::new();
    cpp.cpp(true).warnings(false).opt_level(3).pic(true).flag_if_supported("-std=c++11");
    for f in cpp_files {
        cpp.file(src.join(f));
    }
    if is_x86 {
        cpp.flag_if_supported("-maes");
        cpp.file(src.join("jit_compiler_x86.cpp"));
    }
    if is_arm {
        cpp.flag_if_supported("-march=armv8-a+crypto");
        cpp.file(src.join("jit_compiler_a64.cpp"));
    }
    cpp.compile("randomx_cpp");

    println!("cargo:rerun-if-changed=RandomX/src");
    println!("cargo:rerun-if-changed=build.rs");
}
