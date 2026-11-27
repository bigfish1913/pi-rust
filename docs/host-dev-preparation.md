# #56：初始 dev preparation / Darwin compiler wrapper / IPC 帮助收口

## 结论与范围

本轮限定 review 收口已完成，接住既有 `prepare_initial`、app 接线及 `cc` build-script WIP，补齐清理拒绝、真实 DLL 拷贝失败、帮助与验收。未提交、安装、发布、部署或合并。未新增 workspace crate 或 Cargo 锁定包；扩展工作树和既有 #59/#60/#61 报告保持不变。

**定向验收通过，不代表全库或 workspace 全绿。** 未过滤全库仍有四项已记录的预构建扩展 codesign 夹具失败。

## 行为与所有权

- `DevExtension::prepare_initial(self: Arc<Self>, ...)` 使用 `Arc::get_mut` 拒绝其他 strong owner **及 Weak 观察者**；generation 非零或已有 current stage 时拒绝初始准备。不把普通 reload/retirement 改成此清理入口。
- 在初始独占 namespace 的 Cargo build / staging / 参数应用失败后回收自己的未加载目录。不启动 detached cleanup helper。保留原始错误，清理失败时追加剩余路径和清理诊断。
- `discard_unloaded` 对缺失路径幂等；通过 `symlink_metadata` 拒绝当前观察到的 regular file/symlink 替换，保留替换项和外部 target。**这不是目录 inode 身份证明或抗并发目录替换的删除协议**；安全前提仍是调用方证明独占、未加载。真实目录被外部替换及检查与删除间竞态不在本轮 OS 沙箱保证内。
- 新增真实 Cargo DLL **copy** 失败测试：先实际编译并发现 DLL 文件名，在 generation 目录内用同名目录阻塞 copy。验证原错误是 `could not stage extension`、本 namespace 消失、另一个实例与 legacy 目录中的 sentinel 仍在、Cargo DLL 逐字不变。既有 generation mkdir 失败用例继续保留，不能把 mkdir 失败冒充 copy 失败。
- 其他覆盖包括编译失败、清理错误保原诊断、strong/weak 共享拒绝、成功后再次 prepare 拒绝且 artifact 保留、namespace 碰撞与隔离。

## Darwin 构建与非 mac 边界

`crates/rpi-cli/build.rs` 在 target OS 为 macOS 时才通过锁中已有的 `cc = 1.4.7` 编译 census C11 静态库，保留 `_DARWIN_C_SOURCE`、warnings-as-errors。`tool_processes` 模块也仅在 macOS 编译。

两类实际实验：

1. **真实 Cargo build** 设置记录参数的 `CC`/`AR` wrapper。构建成功，记录明确包含 `src/host/tool_processes/census.c` 与 `librpi_host_process_census.a` 的编译/归档，不能用 ring/sqlite 的 wrapper 调用代替本模块证据。上轮工具调用中断后检查确认构建已完成，未盲目重启。
2. 执行 Cargo 已编译的真实 `build_script_build`，在正确的 crate cwd、隔离 OUT_DIR 下设置 target-specific CC/AR；global CC/AR 故意指向 `/usr/bin/false`。自定义**多词** compiler wrapper 经 `CC_KNOWN_WRAPPER_CUSTOM` 识别，实际编译、归档成功；参数包含 deployment target `14.0`。这是继承 `cc` 约定的验证，不是自己解析 shell 命令。

同一真实脚本在仅设置 `CARGO_CFG_TARGET_OS=linux`、poisoned CC/AR 且无 OUT_DIR 的环境中成功返回，只输出 rerun-if-changed，未增加 wrapper 日志。**这只证明非 mac early-return，不证明 Linux Rust/C 交叉编译或全平台门禁。**

第一次直接脚本试验因 cwd 错用 workspace root 而找不到 C 文件，保留为诊断记录，不作通过证据；最终独立 `script-final/` 使用 crate cwd。未覆盖旧日志或修改生产源码来绕过错误。

## 帮助与实际 binary 回归

顶层帮助使用 `host <command>`，描述 foreground serve、查询、submit/receipt 和可选行政操作，明确 **authenticated IPC**，不再称 read-only；不新增 `rpi host result` CLI 子命令。

首次完整 `host_control` 为 **11 passed / 21 failed**：公共 fixture 的精确能力数组遗漏 #60 已开放的 `results`，另一处帮助断言仍要求旧命令串。仅同步这两处断言，保留完整数组、顺序和其它行为断言，不过滤、不放宽能力校验。最终全部 **32 passed / 0 failed / 0 ignored**，包含真实 binary、HTTP Provider、取消/关闭/Stop、重启及负向文件检查。

## 验收矩阵

统一环境：既有 `CARGO_HOME`、本 worktree `CARGO_TARGET_DIR=$PWD/target`、`TMPDIR=/private/tmp`，Cargo 使用 `--locked --offline -j 2`；底层继续使用 cargoc 与 `/Volumes/BuildCache`，未 clean。

| 门禁 | 结果 |
|---|---|
| `cargo test -p rpi-cli --lib ... dev_ -- --test-threads=1` | **22 passed / 0 failed / 0 ignored** |
| `cargo test -p rpi-cli --lib ... host:: -- --test-threads=1` | **287 passed / 0 failed / 0 ignored** |
| `cargo test -p rpi-cli --test host_control ... -- --test-threads=1` | **32 passed / 0 failed / 0 ignored** |
| `cargo clippy -p rpi-cli --lib --bins --no-deps ... -- -D warnings` | **exit 0**，无新增 lint 豁免；不外推 tests/all-targets/workspace Clippy |
| 6 个本次关联 Rust 文件 `rustfmt --edition 2021 --check --config skip_children=true` | **exit 0**，未全树 fmt |
| 双工作树 `git diff --check` | **通过** |
| 两仓 protocol/client/client_tests/control-v1 四合同 | **逐字一致且本轮未修改** |
| 真实 Cargo wrappers / target-specific 多词 wrapper 与 AR / 非 mac 分支 | **通过**，上述边界不变 |
| 未过滤 `cargo test -p rpi-cli --lib ... -- --test-threads=2` | **1533 passed / 4 failed / 3 ignored**，exit 101 |

全库失败仍是以下四项，均报告 `object file format unrecognized, invalid, or unsuitable`：

- `prebuilt_extensions::tests::all_six_distributions_install_and_record_exact_provenance`
- `prebuilt_extensions::tests::downloaded_manifest_and_archive_install_into_an_isolated_profile`
- `prebuilt_extensions::tests::legacy_archives_install_only_the_library_and_keep_their_records_unchanged`
- `prebuilt_extensions::tests::prebuilt_packages_upgrade_without_force_and_same_version_is_unchanged`

这些测试及签名实现文件本轮未改，#61 已记录相同四项失败。本轮没有跳过、削弱签名或扩范围修夹具；仍不冒称执行过完整 before-source 基线。

## 输入保全与产物

- Core：`/Users/bigfish/work/rpi-projects/pi-rust-wt-host`，`feat/shared-host-integration`。
- Extension：`/Users/bigfish/work/rpi-projects/rpi-extension-wt-host`，`feat/shared-host-management`。
- 实际 CLI：core worktree `target/debug/rpi`，SHA-256 `3d776138d1c5db2bf9609043eeab3a7cdb40108748ba9c0900e95602249ea64f`。
- 任务前哈希由冻结 #61 input manifest + 其记录的 after-hashes 重建，并注明采样时 args/dev_staging 已修改；逐文件核实当时仅这两处差异。**这不是虚构的任务前现场快照或 before-source 测试执行。** 既有清单未枚举的受追踪 release skill 文件另与 HEAD 逐字核验相同，不把它误计为本轮新增，也未修改该私有资源。
- 最终仅修改 5 个既有文件：`src/args.rs`、`src/dev_extension.rs`、`src/host/dev_staging.rs`、`tests/host_control.rs`（均位于 `crates/rpi-cli/`），以及 `crates/rpi-cli/src/host/README.md`。新增本报告和机器证据；其余 core WIP、全部 extension 文件、旧报告/内存证据不变。`app.rs`、build.rs、Cargo.toml/lock 中本轮接住的旧 WIP 未再修改。
- 精确输入/输出哈希、计数、日志哈希与排除项见 [机器证据](host-dev-preparation-validation.json)。原始日志位于 `/tmp/rpi-host-integration-checks/host-56-*`；没有重跑内存 benchmark 或管理 DLL benchmark。

## 仍未完成

不解决 watcher/延迟 helper 的 Host 托管与结算、Cargo/工具后台后代、跨代 OS env、父死/逃逸进程或全域 shutdown。默认 CLI 未切薄 Host；daemon、自动重启、整组恢复和 #46 的生产 Host 受信 DLL hook 装配仍未完成。#14/#23 的整体收口也不由这些定向测试替代。
