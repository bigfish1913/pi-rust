fn main() {
    println!("cargo:rerun-if-changed=src/host/tool_processes/census.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    // libc has no Darwin kinfo_proc binding. Compile against the target SDK,
    // using the existing compiler helper's target CC/AR, wrapper, flags, SDK
    // and deployment-target conventions rather than parsing tool commands here.
    cc::Build::new()
        .file("src/host/tool_processes/census.c")
        .std("c11")
        .define("_DARWIN_C_SOURCE", None)
        .warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true)
        .compile("rpi_host_process_census");
}
