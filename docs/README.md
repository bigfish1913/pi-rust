# 项目文档

按功能查阅当前使用说明与实现说明。版本变更统一记录在 [CHANGELOG](../CHANGELOG.md)，后续计划见 [ROADMAP](../ROADMAP.md)。

嵌入式应用接入模型与认证能力，参见 [可嵌入运行时](architecture/reusable-runtime.md)。

| 功能 | 文档 |
| --- | --- |
| 安装、命令、配置与日常使用 | [用户指南](guides/user-guide.md) |
| SDK 分层与 crate 职责 | [架构概览](architecture/overview.md) |
| Provider 边界、独立 package 与迁移 | [Provider packages](architecture/provider-packages.md) |
| Provider 接入、认证、流式与验收规范 | [Provider 接入规范](extensions/provider-integration.md) |
| 创建 Agent、复用工具与嵌入 SDK | [项目结构](agent/project-structure.md) |
| 消息、流式响应与工具执行 | [Agent Loop 链路](agent/loop-walkthrough.md) |
| 长会话与上下文压缩 | [上下文压缩](agent/context-compaction.md) |
| Rust 动态库扩展与 ABI | [扩展开发](extensions/authoring.md) |
| 启动、会话与关闭事件 | [扩展生命周期](extensions/lifecycle.md) |
| 子 agent 配置继承、权限与生命周期 | [宿主管理的子 agent](extensions/subagents.md) |
| 无头服务端与远程 TUI | [远程模式](remote/user-guide.md) |
| Agent 与扩展调试 | [Rust 调试指南](debugging/rust-debugging.md) |
| Windows 上修饰键 + Enter 失效（终端改写 vs 守卫） | [Shift+Enter 排查](debugging/windows-shift-enter.md) |
| 重复思考、取消与崩溃恢复排查 | [问题复盘](debugging/llm-repetition-forensics.md)（历史证据与修复过程） |
| 启动、内存与安装体积测量 | [性能基准](performance/benchmark-vs-pi.md)（含测量日期与边界） |
| 流式消息复制、增量合并与实测 | [消息传递优化](performance/message-copy.md) |
| 历史、HTTP、状态、恢复、RPC 与写入优化 | [内部链路测量](performance/pipeline-costs.md) |
| 同机运行时复测、瓶颈定位与优化验证 | [2026-10-24 Linux 复测](performance/runtime-20261024/README.md) |
| 长历史复制、会话追加与工具修复优化 | [2026-10-24 长历史复测](performance/history-20261024/README.md) |
| 剩余性能差距、调用栈与单项控制实验 | [2026-10-24 性能定位](performance/diagnosis-20261024/README.md) |
| 会话消息、崩溃恢复与真实扩展回归 | [测试流程](maintaining/testing.md) |
| 发版与跨平台分发 | [发布流程](maintaining/releasing.md) |
| 对外介绍与文章草稿 | [推广素材](promotion/README.md) |
| CLI `docs` 工具的模型可读输入 | [内置文档源](rpi-tool/README.md) |

## 来源与许可

项目许可与第三方来源说明见根目录 [NOTICE](../NOTICE) 和 [LICENSE](../LICENSE)。

## 维护约定

- 使用与实现文档放入对应功能目录，修改行为时同步更新说明。
- `rpi-tool/` 是独立的模型可读文档源；通过 `scripts/sync-embedded-docs.sh` 同步到 CLI 的 `embedded-docs/`，不要仅修改生成快照。
- 发布记录维护在根目录 `CHANGELOG.md`，不再新增重复的单版本发布说明。
- 已失效的里程碑问题记录、差距表与旧版本宣传稿已移除；Git 历史仍可查询。
- 调试复盘与基准结果保留日期和适用范围，不作为当前功能缺失清单。
