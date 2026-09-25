# 🚀 rpi v0.1.28 发布：崩溃恢复 + Langfuse 可观测性，打造生产级 AI Agent

> **核心亮点**：从"崩溃即丢失"到"断点续跑"，从"黑盒运行"到"全链路追踪"——rpi 正式迈入生产就绪阶段。

---

## 📊 一图看懂 v0.1.28

```
┌──────────────────────────────────────────────────────────────────┐
│                     rpi v0.1.28 架构升级                          │
├──────────────────────────────────────────────────────────────────┤
│                                                                  │
│   ┌────────────┐       ┌────────────┐       ┌────────────┐      │
│   │ 用户 Prompt │ ───▶ │ Agent Loop │ ───▶ │  Provider  │      │
│   └────────────┘       └─────┬──────┘       └──────┬─────┘      │
│                              │                     │             │
│                    ┌─────────┴─────────┐           │             │
│                    │                   │           ▼             │
│                    ▼                   ▼    ┌──────────────┐     │
│  ┌─────────────────────────┐  ┌───────────────┐│ 流式响应    │     │
│  │ 🔒 崩溃恢复层            │  │ 📡 Langfuse   ││ 帧/Tool/   │     │
│  │                         │  │   可观测性     ││ Stop       │     │
│  │ • Frame 级进度持久化     │  │               │└──────┬───────┘     │
│  │ • Tool call 结果保证     │  │ • Trace 上报  │       │             │
│  │ • 队列耐久性            │  │ • Generation  │◀──────┘             │
│  │ • 启动自动续跑           │  │ • Tool Span   │                     │
│  │ • Retry intent 保留     │  │ • 评分/Prompt │                     │
│  └─────────────────────────┘  └───────────────┘                     │
│                                                                  │
│               65 files changed, +16,011 / -1,128 lines           │
└──────────────────────────────────────────────────────────────────┘
```

---

## 🎯 为什么这次升级很重要？

### ❌ 之前：崩溃 = 白干

Agent 运行 5 分钟后崩溃，所有进度丢失，只能从头再来。你不知道模型调用了几次、花了多少 token、响应质量如何。

### ✅ 现在：断点续跑 + 全链路追踪

每一帧 assistant 输出、每一个 tool call、每一条排队消息都**实时持久化**。崩溃重启后自动从断点继续。Langfuse 扩展自动追踪全链路——从 prompt 到 response，从 tool call 到最终输出，**一目了然**。

---

## 🔧 Part 1：v0.1.28 核心特性

### 1️⃣ 崩溃恢复 (Crash Recovery)

**端到端的断点续跑**，覆盖所有崩溃场景：

```
正常运行                              崩溃发生              自动恢复
────────                           ─────────           ─────────
                                    
Frame 1 ──▶ 持久化 ✅               Frame 3 写入中       重启 → 读取已提交帧
Frame 2 ──▶ 持久化 ✅               ████ 进程退出 ████    Replay Frame 1-2
Frame 3 ──▶ 持久化 ✅                                    续跑 Frame 3...
Tool call ──▶ 记录策略 ✅                                安全工具自动重放
Queue msg ──▶ 入队即存 ✅                                队列消息重建
```

| 崩溃场景 | 恢复策略 |
|---------|---------|
| 流式输出中断 | 已提交的 frame 前缀 replay 到中断消息 |
| Tool call 无结果 | 安全工具自动重放，其他标记为"结果未知" |
| 重试期间崩溃 | 重试意图保留，不会把失败当作历史 |
| 队列消息丢失 | Steering / follow-up 消息入队即持久化 |

**技术亮点**：

- **Commit-on-settle**：assistant 消息携带 tool call 时立即提交，避免后续崩溃重复执行
- **Resume budget**：续跑有预算限制，防止无限重启
- **63 个测试套件，~1276 个测试**，每个崩溃点都有对应测试覆盖

### 2️⃣ Deferred 长轮询 Provider

`resume_deferred` 从 `not_implemented` 变为完整实现：

```
Provider 返回 handle ──▶ 挂起等待 ──▶ 轮询结果 ──▶ 继续执行
                                          │
                                    工具调用自动执行
                                    （单次，避免重复）
```

适用于需要异步等待的 Provider（如某些 API 的长任务）。

### 3️⃣ Settings 对齐

从 native Pi 复制的 `settings.json` 现在**真正生效**：

| 配置项 | 状态 |
|-------|------|
| `retry` / `compaction` | ✅ 已接入 |
| `steeringMode` / `followUpMode` | ✅ 已接入 |
| `sessionDir` / `httpProxy` | ✅ 已接入 |
| `enabledModels` / `httpIdleTimeoutMs` | ✅ 兼容 native 命名 |

### 4️⃣ 更多改进一览

```
┌─────────────────────────────────────────────────────────────┐
│                    v0.1.28 改进全景                           │
├──────────────────┬──────────────────────────────────────────┤
│ 🛡️ 稳定性        │ 崩溃恢复 / 插件 panic 隔离 / 队列持久化    │
│ ⚡ 性能          │ 流式传输避免消息克隆 / CBOR 二进制协议       │
│ 🎨 体验          │ 富 footer 状态栏 / git 分支实时刷新         │
│                  │ 代码块语法高亮 / 文件监听自动刷新            │
│ 📝 文档          │ LLM 重复 forensics / 缺失功能审计更新       │
│ 🔌 扩展          │ Provider hooks 增强 / BeforeAgentStart 事件 │
│ 🖼️ 新功能        │ 图片生成 API (openrouter-images)           │
│                  │ 按键事件追踪 (RPI_DEBUG_KEYS)              │
└──────────────────┴──────────────────────────────────────────┘
```

---

## 🔍 Part 2：Langfuse 扩展 — 让 Agent 运行透明可见

### 什么是 Langfuse？

[Langfuse](https://langfuse.com) 是**开源的 LLM 可观测性平台**，提供 tracing、evaluation 和 prompt 管理。你可以自托管，也可以使用云版本。

### rpi-langfuse 做了什么？

rpi-langfuse 是一个 **Rust cdylib 原生扩展**，通过 rpi 的 Provider Hooks 机制，自动拦截 Agent 生命周期事件并上报到 Langfuse。

```
┌─────────────────────────────────────────────────────────────────┐
│                    rpi-langfuse 数据流                            │
│                                                                 │
│  ┌─────────┐    Provider Hooks     ┌──────────────┐             │
│  │  rpi    │ ──────────────────▶  │ rpi-langfuse │             │
│  │  Agent  │                      │   扩展        │             │
│  └─────────┘                      └──────┬───────┘             │
│       │                                  │                     │
│       │  BeforeAgentStart                │  HTTP POST          │
│       │  BeforeProviderRequest           │  (batched)          │
│       │  BeforeProviderHeaders           │                     │
│       │  AfterProviderResponse           ▼                     │
│       │  ToolStart / ToolEnd      ┌──────────────┐             │
│       │  TurnStart / TurnEnd     │   Langfuse    │             │
│       │  SessionStart/End        │   Dashboard   │             │
│       │  SessionCompact          │              │             │
│       │                          │  ┌─────────┐ │             │
│       │                          │  │ Traces  │ │             │
│       │                          │  │ Scores  │ │             │
│       │                          │  │ Prompts │ │             │
│       │                          │  └─────────┘ │             │
│       │                          └──────────────┘             │
│       │                                                        │
└───────┴────────────────────────────────────────────────────────┘
```

### 自动追踪：安装即生效，零代码改动

| Agent 事件 | Langfuse 映射 | 记录内容 |
|-----------|--------------|---------|
| Session 开始/结束 | **Trace** | userId, sessionId, metadata |
| Provider 请求/响应 | **Generation** | model, usage, TTFT, cost |
| Tool 调用/结果 | **Span** | tool name, arguments, result |
| Turn 开始/结束 | **Span** | turn duration |
| 上下文压缩 | **Span** | compaction stats |

### Dashboard 里看到什么？

```
┌────────────────────────────────────────────────────────────┐
│  Langfuse Dashboard                                        │
├────────────────────────────────────────────────────────────┤
│                                                            │
│  📋 Trace: "帮我重构这个模块"                                │
│  ├── 🤖 Agent Start (10:23:15)                            │
│  │   ├── 🧠 Generation: claude-sonnet-4-20250514           │
│  │   │   ├── Input tokens:  2,340                          │
│  │   │   ├── Output tokens: 890                            │
│  │   │   ├── TTFT: 1.2s                                    │
│  │   │   └── Cost: $0.045                                  │
│  │   ├── 🔧 Tool: read (0.3s)                              │
│  │   ├── 🔧 Tool: edit (0.8s)                              │
│  │   ├── 🔧 Tool: bash (1.2s)                              │
│  │   └── 🧠 Generation: claude-sonnet-4-20250514           │
│  │       └── [最终响应]                                      │
│  └── ✅ Agent End (10:23:28)  Total: 13s                   │
│                                                            │
│  ⭐ Score: accuracy = 0.95                                  │
│  💰 Total cost: $0.12                                       │
│                                                            │
└────────────────────────────────────────────────────────────┘
```

### 手动工具：评分、Prompt、Trace 查询

除了自动追踪，rpi-langfuse 还提供 3 个手动工具：

#### 📊 `langfuse_score` — 为 Trace 打分

```json
{
  "tool": "langfuse_score",
  "params": {
    "name": "accuracy",
    "value": 0.95,
    "traceId": "trace-123",
    "comment": "High accuracy response"
  }
}
```

#### 📝 `langfuse_prompt` — Prompt 版本管理

```json
{
  "tool": "langfuse_prompt",
  "params": {
    "action": "create",
    "name": "code-review",
    "prompt": "Review the following code for bugs..."
  }
}
```

#### 🔎 `langfuse_trace` — 查询 Trace 详情

```json
{
  "tool": "langfuse_trace",
  "params": {
    "action": "get",
    "traceId": "trace-123"
  }
}
```

---

## 🚀 Part 3：5 分钟快速上手

### Step 1：安装 rpi CLI

```bash
cargo install rpi-cli --version 0.1.28
```

### Step 2：安装 Langfuse 扩展

```bash
rpi install rpi-langfuse
```

### Step 3：配置 Langfuse

**方式一：环境变量**（优先级最高）

```bash
export LANGFUSE_BASE_URL="https://your-langfuse-instance.com"
export LANGFUSE_PUBLIC_KEY="pk-xxx"
export LANGFUSE_SECRET_KEY="sk-xxx"
```

**方式二：配置文件**

创建 `~/.rpi/agent/langfuse.json`（全局）或 `.rpi/langfuse.json`（项目级）：

```json
{
  "baseUrl": "https://your-langfuse-instance.com",
  "publicKey": "pk-xxx",
  "secretKey": "sk-xxx"
}
```

**方式三：环境变量引用**

```json
{
  "baseUrl": "https://your-langfuse-instance.com",
  "publicKeyEnv": "LANGFUSE_PUBLIC_KEY",
  "secretKeyEnv": "LANGFUSE_SECRET_KEY"
}
```

### Step 4：开始使用！

```bash
rpi -p "帮我写一个 Rust HTTP server"
```

打开 Langfuse Dashboard，你会看到完整的 trace 已经自动上报 🎉

---

## 🏗️ 架构深度解读：Provider Hooks 如何工作

rpi 的扩展系统通过 **稳定 ABI** 的 Provider Hooks 让 Langfuse 这样的观测插件无缝接入：

```
┌───────────────────────────────────────────────────────────────────┐
│                    Provider Hooks 机制                              │
│                                                                   │
│   Agent 发起 Provider 请求                                         │
│        │                                                          │
│        ▼                                                          │
│   ┌─────────────────────────┐                                     │
│   │ ExtensionProviderHooks  │  ← rpi-extensions crate             │
│   │                         │                                     │
│   │ (A) BeforeAgentStart    │  ← 每个新 prompt 触发一次            │
│   │     (按 timestamp 去重)  │    携带 prompt 文本 + 图片数量       │
│   │                         │                                     │
│   │ (B) BeforeProviderReq   │  ← 每次 Provider 调用触发            │
│   │     携带 model/messages  │    包含完整的 model-facing messages  │
│   │     /headers/usage      │                                     │
│   │                         │                                     │
│   │ (C) AfterProviderResp   │  ← 收到完整响应后触发                 │
│   │     携带 assistant msg  │    包含完整的 assistant message       │
│   └───────────┬─────────────┘                                     │
│               │                                                   │
│               ▼                                                   │
│   ┌─────────────────────────┐                                     │
│   │  rpi-langfuse 扩展      │  ← Rust cdylib 插件                 │
│   │                         │                                     │
│   │  接收事件 → 批量上报     │  → Langfuse API                     │
│   └─────────────────────────┘                                     │
│                                                                   │
│   ⚠️ v1 观察模式：插件可以记录/诊断，但不能修改请求参数              │
│   🔮 未来 ABI 扩展将解锁 patch 路径                                │
└───────────────────────────────────────────────────────────────────┘
```

---

## 📈 数据说话

```
┌────────────────────────────────────────────────────────┐
│                 v0.1.28 发布统计                        │
├────────────────────────────────────────────────────────┤
│                                                        │
│  📦 65 个文件变更                                       │
│  📝 +16,011 行新增 / -1,128 行删除                      │
│  🧪 63 个测试套件                                       │
│  ✅ ~1,276 个测试通过，0 失败，0 警告                    │
│  📖 1,872 行 LLM 重复 forensics 文档                    │
│  🏗️ 24 个可扩展软件包                                   │
│                                                        │
└────────────────────────────────────────────────────────┘
```

---

## 🔗 相关链接

| 资源 | 链接 |
|------|------|
| 🏠 官网 | [https://rpi.laofu.online](https://rpi.laofu.online) |
| 📦 crates.io | [rpi-cli](https://crates.io/crates/rpi-cli) |
| 📚 API 文档 | [docs.rs/rpi-cli](https://docs.rs/rpi-cli) |
| 💻 GitHub | [bigfish1913/pi-rust](https://github.com/bigfish1913/pi-rust) |
| 📦 扩展仓库 | [pi-rust/rpi-package](https://github.com/pi-rust/rpi-package) |
| 🔍 Langfuse | [langfuse.com](https://langfuse.com) |
| 📋 Release Notes | [v0.1.28](https://github.com/bigfish1913/pi-rust/releases/latest) |

---

## 💡 总结

rpi v0.1.28 是两个关键能力的落地：

1. **崩溃恢复** — 让 Agent 从"一次性脚本"变成"可靠工具"。无论何时崩溃，进度不丢。
2. **Langfuse 可观测性** — 让 Agent 从"黑盒"变成"白盒"。每一次模型调用、每一个工具执行，都有据可查。

配合 rpi 的 **library-first** 架构和 **24 个可扩展软件包**，你现在可以用 Rust 构建真正生产级的 AI Agent。

```bash
# 立即体验
cargo install rpi-cli --version 0.1.28
rpi install rpi-langfuse
rpi -p "Hello, production-grade agent!"
```

---

*MIT License · Rust 1.78+ · [GitHub](https://github.com/bigfish1913/pi-rust) · [文档](https://rpi.laofu.online)*
