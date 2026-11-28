# Mini 显式配置与 raw Agent 装配（#66 进行中）

本轮推进实现，不将 raw binder 测试计作生产 Mini 验收。

## 已实现

- `agent.mode` 缺省为 `persistent`，旧 profile 仍必须提供有效 Session。
  显式 `mini` 只接受缺省或 `null` Session，拒绝 Session 引用和未知 mode。
- 普通和 Mini 装配复用闭合 Provider/model、工具批准、cwd/shell、独立工具环境与
  冻结 resource 校验。Mini 使用原 `rpi_agent::Agent`、真实 async Provider factory 和
  原工具，不构造 Harness、AgentLane、内存/文件 Session、journal 或 writer。
- Mini prompt/resources 使用已有 Session-independent 装配入口。受信 DLL callbacks
  仍通过同一个 `NativeExtensionCustody` loader；装配调用者必须先在真正 owner 中
  保留 custody，失败或等待者丢弃不代表 native cleanup。此轮未新增 DLL 实测。
- 现有 daemon 尚无 Mini owner/volatile engine，故在任何 startup/catalog 文件副作用
  之前拒绝 Mini；dynamic-only Mini 由 trusted-profile freeze 拒绝。不能误走 persistent。

## 新增测试与边界

- raw Mini + HTTP Provider：prompt 保留 `{{literal}}`，Agent 对话完成，工作目录无文件。
  此测试直接注册清单行并调用 raw binder，**不证明 service owner/resident 生命周期**。
- raw Mini + 真实 bash + HTTP 两轮：仅授予 bash；冻结 instruction 保留
  `{{instruction}}`，冻结后替换源文件不影响 prompt；shell 不得到 Provider 凭据，
  工具结果保留模板文本且进入下一轮 HTTP，工作目录没有 Session/其他文件。
- 错误 mode、Mini 带 Session、child execution、未冻结 resource、未批准/未知工具、
  缺失 Provider key/HOME，全部在请求/文件副作用之前拒绝。
- profile 默认/显式 persistent、显式 mini、缺失/null/冲突 Session、未知 mode；
  daemon 含先行 persistent 的混合集合，以及 dynamic-only Mini，均有拒绝副作用检查。

## 本轮门禁

所有命令在 core worktree 使用既定 CARGO_HOME、worktree target、`TMPDIR=/private/tmp`，
Cargo `--locked --offline -j 2`。每个门禁保留独立 `.log/.exit`。

| 门禁 | 结果 |
| --- | --- |
| bindings/profile/Mini daemon 拒绝定向 | 23 passed / 0 failed / 0 ignored |
| 未过滤 CLI lib（两线程） | 1544 passed / 0 failed / 3 ignored |
| 完整 ordinary `host_control` binary（两线程） | 39 passed / 0 failed / 0 ignored |
| CLI lib/bins/whole host_control no-deps strict Clippy | exit 0 |
| 定点 rustfmt、两树 diff check、四合同逐字一致 | 见机器证据 |

测试链接阶段出现既有大型 `__eh_frame` linker warning；没有隐藏该诊断。
此前 Agent strict Clippy 未通过、SDK lint 与默认并行 CLI 的历史问题不由本轮修复。

[机器证据](host-mini-raw-binding-validation.json) 包含本轮快照/文件与日志 SHA、旧证据保全。
快照是在 raw binder 初始实现之后、显式 mode 修改之前生成；不声称它是 raw binder 的修改前快照。
`host-mini-raw-agent-focused-2.exit` 是超时后根据完整测试成功日志人工补记，不能作为原 cargo
退出码实测；本轮只采用重新运行并真实捕获退出码的 `focused-final` 和 production-profile 门禁。

## 尚未完成

- 唯一 HostRuntime/HostService 的 Mini owner/启动失败 residue、run/cancel/close/drain 接入；
  raw `NativeMiniAgent` 目前只是内部装配产物，不是已完成的生产生命周期 facade。
- 有界 epoch/owner 范围 volatile admission/result，daemon/control wire 和管理扩展接入。
- 实际 binary + **真实管理 DLL** + HTTP 的 Mini 单链路；`sessionId: null`、取消/断连、
  迟到 callback/finish、真实卸载、重启 Lost/不重放，均需独立验收。
- 不称 durable Mini 回执，不复用普通 Session admission，不宣称 workspace/MSRV/跨平台全绿；
  #14、#65、#66、默认薄 Host/后台 daemon/整组恢复保持未完成。
