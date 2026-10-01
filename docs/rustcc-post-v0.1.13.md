# rpi 0.1.13 后续开发：让本地扩展和 package 更新更可控

> **这篇是 0.1.13 时期的历史发布稿，命令示例按当时的状态保留。**
> 之后 Pi 兼容层已整体移除：`rpi pi-update`、`rpi package update`、
> `rpi install-pi` / `rpi uninstall-pi` 都不再存在，`rpi self-update`
> 也改名为 `rpi update`。现在的命令面见
> [user-guide](rpi-tool/user-guide.md)，其中与更新相关的只有
> `rpi update`（更新 rpi CLI 自身）、`rpi install <crate>` 和
> `rpi uninstall <crate>`（管理 Rust 扩展）。

rpi 是一个 Rust 原生、library-first 的 Pi agent SDK，同时提供可直接使用的终端 coding-agent CLI。稳定版本目前为 `v0.1.13`。最近在 `v0.1.13` 之后，项目继续完善本地扩展开发和 package 更新流程，目标是让开发者在自己的项目中迭代 Agent 能力时更容易验证、回滚和定位问题。

## 最近的改动

### 1. 增加本地扩展开发模式

现在可以使用：

```bash
rpi dev-local
rpi dev --local-only
```

本地开发模式只加载当前项目的 `.rpi/skills`、`.pi/skills`、项目配置资源，以及当前正在开发的扩展资源，避免全局 package 和其他扩展干扰调试。

`/reload` 也会沿用相同的本地资源范围，适合编写和调试项目内的 skill 或 Rust 原生扩展。

### 2. 拆分不同类型的更新命令

> 已过时：这一节描述的 `rpi pi-update` 和 `rpi self-update` 都已不存在。
> 现在只有 `rpi update`，它更新的是 rpi CLI 自身；Rust 扩展的安装与卸载
> 用 `rpi install <crate>` / `rpi uninstall <crate>`。以下是 0.1.13 当时的原文。

当时更新范围更加明确：

```bash
# 只更新 Rust 原生扩展
rpi update

# 只更新 Pi npm/Git package
rpi pi-update

# 更新 rpi CLI 自身
rpi self-update
```

旧的 `rpi package update` 仍然保留，用于兼容同时更新两类 package 的工作流。

这样可以避免一次更新同时触发 Rust 扩展、Pi package 和 CLI 自更新，也更容易在出现问题时定位是哪一类依赖造成的。

### 3. 项目 package 默认加载，可显式关闭

项目资源现在默认直接加载，不再在每次交互启动时询问确认。对于不希望加载当前项目 package 的场景，可以显式使用：

```bash
rpi --no-approve
```

项目资源可能包含扩展代码和命令，因此建议只在可信项目中启用；需要隔离范围时可以使用 `rpi dev-local` 或 `--no-approve`。

### 4. TUI 工具面板不再显示空状态

交互式 TUI 现在会延迟空的工具进度回调，等到工具真正开始执行后再创建面板，因此首次调用工具时不会先出现没有参数和内容的空面板。

同时，工具选择器会把“空列表代表全部内置工具”的内部状态正确展开为当前启用的工具，首次打开 `/tools` 时不会误显示为全部关闭，也不会因为切换一个工具而意外丢失其他默认工具。

## 稳定能力

rpi 的稳定主线仍然是：

- Rust 原生的异步、流式 Agent loop
- Anthropic、OpenAI-compatible 和 faux provider
- 与 Pi 对齐的 `read`、`write`、`edit`、`bash` 工具
- JSONL session、上下文压缩和 AgentHarness
- `rpi-plugin-sdk` 稳定 ABI
- 可嵌入的多 crate SDK 和 `rpi` 终端 CLI

Node/TypeScript 扩展桥接和 TUI 原生技能调用渲染目前仍属于 Beta，建议仅用于本地评估和兼容性测试，暂不建议用于生产环境。

## 快速开始

安装稳定版 CLI：

```bash
cargo install rpi-cli --version 0.1.13
rpi -p "检查当前项目结构并列出最值得优先修复的问题"
```

不需要 API Key 的本地示例：

```bash
git clone https://github.com/bigfish1913/pi-rust.git
cd pi-rust
cargo run -p minimal
```

项目地址：<https://github.com/bigfish1913/pi-rust>

官网和文档：<https://rpi.laofu.online/>

欢迎反馈本地扩展开发、package 更新和跨平台安装过程中遇到的问题。
