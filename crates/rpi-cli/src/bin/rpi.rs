//! The `pi` binary entry point. Mirrors the TS
//! `packages/coding-agent/src/cli.ts` (the `#!/usr/bin/env node` shebang file
//! whose `bin` entry in `package.json` resolves to `dist/cli.js`, which just
//! calls `main(process.argv.slice(2))`).
//!
//! Rust's analogue: a thin wrapper that boots the multi-thread runtime the
//! harness needs (the harness's `StreamFn` bridge uses `block_in_place`, which
//! requires the multi-thread flavor) and hands control to
//! [`rpi_cli::app::run`], exiting with its returned code.

use std::process::ExitCode;

/// Windows debug 构建下主线程的栈大小。
///
/// Windows 给主线程的默认栈只有 1MB，而 debug 构建下的启动路径
/// （harness 构建 + 首帧全量布局）会把它撑爆，进程直接 stack overflow。
/// release 的栈帧小得多；其他平台的主线程默认就是 8MB。
#[cfg(all(windows, debug_assertions))]
const MAIN_STACK: usize = 8 * 1024 * 1024;

/// 启动入口（Windows debug 构建）。
///
/// 换到一个显式大栈的线程上跑：主线程默认只有 1MB，`app::run()` 展开后的
/// async 状态机会把它撑爆。
#[cfg(all(windows, debug_assertions))]
fn main() -> ExitCode {
    std::thread::Builder::new()
        .name("rpi-main".to_string())
        .stack_size(MAIN_STACK)
        .spawn(run)
        .expect("failed to spawn rpi-main")
        .join()
        .unwrap_or(ExitCode::FAILURE)
}

/// 启动入口（其余平台与 release 构建）。
///
/// 主线程默认 8MB，release 的栈帧也小得多，直接在当前线程跑。
#[cfg(not(all(windows, debug_assertions)))]
fn main() -> ExitCode {
    run()
}

/// 创建多线程 tokio runtime 并驱动整个 CLI，返回进程退出码。
fn run() -> ExitCode {
    // `block_in_place`（provider 流的同步桥）要求多线程 runtime，和原来的
    // `#[tokio::main(flavor = "multi_thread")]` 保持一致。
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    let code = runtime.block_on(rpi_cli::app::run());
    ExitCode::from(code as u8)
}
