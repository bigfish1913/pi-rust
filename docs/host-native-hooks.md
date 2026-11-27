# 生产 Host 原生 tool-result 路径：#46 阶段验收

## 状态与边界

本阶段已接通 **实际 `rpi host serve` → 私有受信 profile → 冻结/staging 的原生 DLL → 原生工具结果 transform → HTTP Provider**，不是测试 binder 或 SDK 单独加载实验。**#46 仍为 in progress**：这里仅支持首个 tool-result 路径；生产 Provider hook 桥、#14/#23 整体收口、默认 CLI 薄 Host、真正后台 daemon/整组恢复、watcher/helper/受控 OS 资源全域结算仍未完成。

现有 Provider hook trait 是同步接口，Harness/Agent loop 的实际调用点处于异步路径。不能仅包装一层 custody 后直接执行潜在阻塞的原生回调。本阶段没有开放该注册能力，也没有改写 Agent loop、AgentLane 或 SDK ABI。后续必须用可等待的真实 worker 结算桥接，并保留逐 Agent 身份，不能靠删掉拒绝分支冒充实现。

所有源码修改仅在 `/Users/bigfish/work/rpi-projects/pi-rust-wt-host`；extension worktree 全部 823 个冻结输入文件保持不变。没有提交、安装、发布、部署、合并、注册系统服务、新增 workspace crate/锁定包或重跑内存 benchmark。旧 #56/#59/#60/#61 及 20-Agent 证据不改写。

## 私有 profile 的显式批准

以下是已有完整 Agent profile 的**资源片段**，不是可独立启动的完整配置；路径均需由 Host owner 显式提供：

```json
{
  "resources": {
    "version": 1,
    "tools": {"mode": "only", "names": ["read"]},
    "instructions": [],
    "extensions": ["/trusted/native/libresult.dylib"]
  },
  "nativeHooks": {
    "stagingRoot": "/private/owned/host-staging",
    "registrations": [{"toolResult": 1}]
  }
}
```

- `nativeHooks` 必须与非空、有序的显式 DLL selection 配对，逐文件批准数量须完全一致；最多沿用既有 8 DLL/单文件 64 MiB/总 128 MiB 的 artifact 限制，每 DLL 的 tool-result 数不超过 16。
- staging root 必须是现存、绝对、当前 UID 所有且 group/other 无权限的真实目录；不自动采用 ambient TMPDIR 或创建公共目录。这里没有宣称 inode/TOCTOU 沙箱。
- 冻结读取 DLL 字节、规范 source provenance、顺序、instructions 和批准表；后续 source 替换不改变已冻结的批次。内容 hash 是身份校验，不是权限本身；批准来自私有 profile。
- 未批准原生扩展时保留 instruction-only 的拒绝行为。精确 tools 与原生内置工具授权须同时匹配，模型/IPC 只能引用既有受信 profile，不能提供新凭据或自行批准插件。
- 比较 loader 在 registry 合并前提供的完整 `RegistrationInventory`：除批准的 tool-result 数之外，其他注册/声明必须是默认值。包括不支持的 Provider observer、prompt transformer、ActionBridge/UI、插件工具、priority/platform/provider metadata 等，不能在合并中悄悄丢掉。
- 私有 profile 允许执行受信原生代码，**不是对 DLL 内部任意系统调用的隔离**。

## 生产接线与所有权

新增闭合的 `NativeBindingResources` 变体区分 instruction-only 与批准的 native hooks；历史公开字段 `instruction_resources` 保留名称，但类型现在承载这个闭合资源集合。不是通用可执行 preflight 回调。

1. `AgentProfileFile::into_startup` 只冻结和验证资源，不 dlopen、不注册、不启动 worker、不 staging。
2. `NativeBindingProfile::bind` 保留原生 Provider/工具/显式环境的现有验证；普通无 DLL 路径仍使用原 immediate binder。仅 hook 变体使用既有 deferred assembly，且在任何 native 注册前验证严格只读 Session facts、声明及完整保存 inventory。
3. 真正 owned assembly 才在 blocking worker 创建每 incarnation 的独立 staging，并调用既有 `NativeExtensionCustody::load_owned_checked`。不改变 SDK 的 owned strict all-or-nothing 语义、失败注册 custody 或 cleanup receipts。
4. 首次 Provider/工具前，在真实 writer 下原子持久化 `rpi.resource-selection.v1` 的完整声明、artifact sha256、完整批准表和 tool-schema digest，严格同步历史。重开不同声明/inventory 不能覆盖原 authority。
5. Harness options 中的 `AfterToolCall` 只保留 Weak owner，避免 owner/Harness/DLL 循环。实际 `spawn_blocking` worker 内才取得 call scope；scope 覆盖 callback 和 plugin-side string reclaim。丢掉/取消等待者不会卸载正在执行的代码。
6. 逐 Agent `ToolCallContext::for_agent` 冻结 `cwd/agentId/sessionId`，只装饰插件 envelope 的顶层 `__rpi`，不改模型原参数、原工具结果或原生日志。显式无 Session 不读取 ambient Session。新增 Rust helper 使用类型化 `InactiveToolResultRegistry`；旧 `emit_tool_result_with_status` 的签名/无 context payload 保持兼容，SDK ABI 不变。
7. shutdown/close 复用已有 supervisor、writer 和 SDK custody 结算。迟到 callback/finish 未完成时不可报告释放。finish 返回失败时，实际 worker 即使已经退出，也不能据此推断插件全部资源安全；保守保留 DLL、staging、owner/resident 和已取得的 writer。

## 实际 binary + DLL + HTTP 证据

新增 `crates/rpi-cli/tests/host_control/native_hooks.rs`，由 Cargo 必需 binary 测试直接构建/调用当前 `rpi`。测试在私有临时目录编译 C dylib，不使用已安装程序、测试 binder、management DLL 或生产服务。

| 场景 | 实际验证 |
|---|---|
| 纯冻结/批准拒绝 | 有效 profile 不产生 native registration/staging；缺批准/缺资源/数量不符/Provider 字段/超预算/相对 root/重复 DLL 拒绝 |
| 双 Agent + HTTP | 同源 DLL 装入两份独立 incarnation；同一 tool-call ID 下 `cwd/agentId/sessionId` 不串；真实 read 的结果被 transform，下一次 HTTP 请求收到 `DLL-TRANSFORMED` |
| 冻结与保存 authority | profile 冻结后替换源文件仍加载冻结字节；改保存 inventory 时，在 native 注册前拒绝且原 Session 字节不变 |
| 关闭/重启 | 真实 SDK lifecycle begin/worker-exit/finish 和 C unload destructor 均发生，staging 清空；重启复用同 Session、不重写资源 binding，原请求重复不产生额外 HTTP |
| 整批失败 | 第二 DLL 注册返回 71、accepted priority metadata 不匹配或 tool-result 数不匹配时，已注册的两份实际 worker 均 cleanup/unload，不发布任何 transform；原行政意图只对账，不重新注册 |
| 迟到 AfterToolCall | 门控真实 C callback 与 finish；关闭等待时认证 IPC 仍响应，resident/owner 保留，实际 OS writer lease 被拒；依次释放后才有完成收据、writer 可重新取得 |
| retained | 成功加载后的 close 和失败注册的 cleanup 均让 finish 返回 23；无 unload destructor，staging/owner/resident 保留，已获取 writer 仍不可取得，SIGTERM 也不宣告干净退出 |

原生标记来自 callback、plugin-free、pthread worker、lifecycle finish 和 C unload destructor，并与 owner/resident、实际 OS writer、staging 状态共同断言。没有额外读取 OS VM map，因此**不把 destructor/SDK release receipt 夸大成逐内存页映射审计**。retained 测试最后显式 kill/reap 的仅是私有负向 fixture 子进程；这不是成功 Host cleanup 或生产 recovery 策略。

SDK 新增并发同 registry 的 context 测试还覆盖 sessionless、模型输入中的伪造 `__rpi`、late setter 不重绑定，以及旧 payload 兼容。

## 最终门禁与失败记录

所有 Cargo 命令使用已有 CARGO_HOME、worktree target、`TMPDIR=/private/tmp` 和 `--locked --offline -j 2`。

| 门禁 | 最终结果 |
|---|---|
| `cargo test -p rpi-cli --lib`，未过滤 | **1538 passed / 0 failed / 3 ignored** |
| `cargo test -p rpi-cli --test host_control` | **37 passed / 0 failed / 0 ignored** |
| `cargo test -p rpi-extensions --lib` | **259 passed / 0 failed / 1 ignored** |
| `cargo clippy -p rpi-cli --lib --bins --no-deps -- -D warnings` | **exit 0** |
| 单独 SDK 严格 no-deps Clippy | **exit 101，15 项诊断，不是绿色门禁** |
| 本阶段 Rust 定点 fmt、双树 diff、四合同和输入保全 | 见机器证据 |

日志均在 `/tmp/rpi-host-integration-checks/host-all-hooks-*`，完整命令/结果/hash 见配套 JSON。不是 workspace/all-targets、跨平台或 MSRV 专用工具链验收，也没有重测旧 management DLL/内存数据。

保留如下试跑，不以最后通过覆盖失败：

- 首次 Host 编译在工具 120 秒上限中断；检查完整 cargo/cargoc/rustc 进程树，确认无存活构建后另日志续跑，原有 287 Host 测试通过。
- 首次 binary 新测试 3 过/1 失败：负向 C 夹具把 SDK 无 ActionBridge 时对 `ownsStdio` 的拒绝码当成 abort 条件，导致连接不确定。改用 SDK 能实际接受的 priority metadata 测试 merge 前的严格 inventory 拒绝，最终 binary 37/37。
- 第一次未过滤 CLI：1537/1/3，未改的 `review_pr104_fork_preserves_a_model_pick_before_the_next_turn` 断言 fork 后 ID 未变。源码相对冻结 WIP 输入逐字不变；隔离复跑三次均通过，随后全库通过。**没有完整 before-source 执行，也未查明此 TUI 偶发根因，不能声称已证明为基线问题或已修复。**
- 第二次未过滤 CLI：1537/1/3，dev-copy 测试比较第一次 Cargo artifact 与 preparation 触发的第二次合法重建，发生 108 bytes 差异。解析 Mach-O 确认 cargoc 重键改变 LC_ID_DYLIB 中输出根路径及 LC_UUID；并不是 staging cleanup 改了原 artifact。
- #63 仅给私有 Darwin Rust fixture 写 `.cargo/config.toml`，固定 `@rpath/libstage_fixture.dylib` 和 `-no_uuid`。原整文件字节比较、真实 Cargo build/copy 失败及其他 owner 保全断言全部保留；定向 dev 15/15，随后两次未过滤全库均 1538/0/3。生产 dev 构建参数没有改变，旧 #56 报告没有改写。
- SDK 首次单独严格 Clippy 16 项，新增 helper 的 unit-error 已改为类型化错误；最终剩 15 个输入中已经存在的诊断位置。此为源码位置对照，不冒称已运行完整 SDK before-source Clippy；未添加 allow/expect，也未扩大到 SDK 大清理。
- 最终组合命令在 600 秒工具上限中断时，SDK/CLI/binary 三份 `.exit` 均已为 0，Clippy 尚无结果；确认进程树已退出后，单独在新日志完成核心严格 Clippy，不把超时当完成。

四项 codesign 夹具修复已独立验收在 `docs/host-codesign-fixtures.md` 与相应 JSON；本报告不改写该阶段或更早证据。

## 后续仍需推进

1. 完成不阻塞共享异步执行器的 Provider callback worker/结算桥，再进行逐 Agent Provider 上下文和错误/取消/迟到三方验收，才评估 #46 完成。
2. 对 #14/#23 整体矩阵逐项核对；已有 create/restore/cancel/close/stop/认证 IPC/原请求收据不应描述成完全没有，但本阶段也不能自动代表整体收口。
3. 在具体 watcher/helper、延迟/全代际 dev 与受控 OS 子进程取得真实 cleanup 证明后，才接默认 CLI 薄 Host、默认一按需 N、配置化整组恢复和后台 daemon；原请求与未知工具副作用不重放。
