//! Build-time preparation for the active-session child barrier.
//!
//! The LD_PRELOAD shim is compiled here, once per target, and embedded into
//! `rpi-tools` with `include_bytes!`. The alternative — invoking `cc` from
//! `child_barrier_env()` at runtime — made every durable-session bash command
//! depend on a C compiler being installed on the user's machine, which is
//! unacceptable for a normal CLI. Build machines always have a host compiler
//! (the release matrix already installs `musl-gcc` where needed); users do not.
//!
//! Non-Linux targets do not compile a shim: the child barrier is Linux-only,
//! and the tool-layer checks are the cross-platform boundary there.

#[cfg(target_os = "linux")]
fn main() {
    use std::path::PathBuf;

    println!("cargo:rerun-if-changed=guard_shim.c");

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let so = out.join("libguard.so");
    let status = std::process::Command::new("cc")
        .arg("-shared")
        .arg("-fPIC")
        .arg("-O2")
        .arg("-o")
        .arg(&so)
        .arg("guard_shim.c")
        .arg("-ldl")
        .status()
        .expect("cc is required to build rpi-tools on Linux");
    assert!(status.success(), "guard shim compilation failed: {status}");

    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=CFLAGS");
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("cargo:rerun-if-changed=guard_shim.c");
}
