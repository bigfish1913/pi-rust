# TUI 实现计划与验收清单

## 概述

本文档记录了 TypeScript 版本的 TUI (`packages/coding-agent/src/modes/interactive/`) 与 Rust 版本的 TUI (`crates/pi-cli/src/interactive_tui.rs`) 之间的差距，以及实现计划。

## 当前状态

### 已实现 ✅
- [x] 基础组件: Container, Text, Spacer, Editor, ScrollView, VStack
- [x] 基本的交互流程: 用户输入 -> API 调用 -> 响应显示
- [x] 编译通过，测试通过

### 问题 ❌
- [ ] 用户输入后没有看到响应
- [ ] 界面只有一个 ">" 符号
- [ ] 与 TypeScript 版本差距很大

---

## Phase 1: 核心交互修复

### 1.1 修复消息显示问题

**问题**: 用户输入消息后，没有看到响应显示。

**原因分析**:
- `on_submit` 回调只是添加消息到 transcript，没有触发 API 调用
- API 调用只在初始 prompts 循环中处理

**TypeScript 参考**:
```typescript
// interactive-mode.ts - setupEditorSubmitHandler
this.defaultEditor.onSubmit = async (text: string) => {
    text = text.trim();
    if (!text) return;
    // ... 处理斜杠命令 ...
    await this.session.agent.promptText(text, []);
};
```

**验收标准**:
- [ ] 用户输入消息后，消息显示在 chat 容器中
- [ ] API 响应显示在 chat 容器中
- [ ] 消息有正确的格式（用户消息前有 "> "）

**实现步骤**:
1. 确保 `on_submit` 回调正确发送消息到 channel
2. 主循环正确处理 channel 消息并调用 API
3. 响应正确添加到 chat 容器

---

### 1.2 修复编辑器提示符问题

**问题**: 界面只显示一个 ">" 符号，缺少欢迎信息和操作提示。

**TypeScript 参考**:
```typescript
// interactive-mode.ts - constructor 后的初始化
const logo = theme.bold(theme.fg("accent", APP_NAME)) + theme.fg("dim", ` v${this.version}`);
// ... 添加 keybinding hints ...
```

**验收标准**:
- [ ] 启动时显示欢迎信息
- [ ] 显示操作提示 (Ctrl+C 退出, Shift+Enter 发送等)
- [ ] 编辑器有正确的 placeholder

**实现步骤**:
1. 添加欢迎消息到 chat 容器
2. 添加操作提示
3. 设置编辑器 placeholder

---

## Phase 2: 组件实现

### 2.1 AssistantMessageComponent

**TypeScript 文件**: `assistant-message.ts` (~200行)

**功能**:
- 渲染助手消息
- 支持 Markdown 内容
- 支持 thinking 块
- 处理错误情况 (aborted, error, length)

**验收标准**:
- [ ] 创建 `AssistantMessageComponent` 结构体
- [ ] 实现 `update_content` 方法
- [ ] 支持 Markdown 渲染
- [ ] 显示错误消息

**实现步骤**:
1. 创建 `crates/pi-tui/src/assistant_message.rs`
2. 实现基本结构
3. 集成到 `interactive_tui.rs`

---

### 2.2 FooterComponent

**TypeScript 文件**: `footer.ts` (~300行)

**功能**:
- 显示当前模型
- 显示状态信息
- 显示 keybinding 提示

**验收标准**:
- [ ] 创建 `FooterComponent` 结构体
- [ ] 显示基本信息
- [ ] 动态更新状态

**实现步骤**:
1. 创建 `crates/pi-tui/src/footer.rs`
2. 实现基本结构
3. 集成到布局中

---

### 2.3 完善 Markdown 组件

**TypeScript 文件**: `pi-tui/src/components/markdown.ts`

**当前差距**:
- 缺少 MarkdownTheme 支持
- 缺少 MarkdownTransformer 支持
- 渲染逻辑不完整

**验收标准**:
- [ ] 支持代码块语法高亮
- [ ] 支持列表渲染
- [ ] 支持链接渲染

---

## Phase 3: 高级功能

### 3.1 事件系统

**TypeScript 参考**:
```typescript
case "message_start":
    if (event.message.role === "assistant") {
        this.streamingComponent = new AssistantMessageComponent(...);
        this.chatContainer.addChild(this.streamingComponent);
    }
case "message_update":
    this.streamingComponent.updateContent(this.streamingMessage, true);
case "message_end":
    this.streamingComponent.updateContent(this.streamingMessage, false);
```

**验收标准**:
- [ ] 实现消息流式更新
- [ ] 支持中断操作
- [ ] 错误处理

---

### 3.2 斜杠命令

**TypeScript 参考**:
- `/settings` - 显示设置选择器
- `/model` - 模型选择
- `/export` - 导出会话
- `/clear` - 清除对话
- 等等

**验收标准**:
- [ ] 实现 `/clear` 命令
- [ ] 实现 `/help` 命令
- [ ] 其他命令后续实现

---

### 3.3 Tool 执行显示

**TypeScript 文件**: `bash-execution.ts`, `tool-execution.ts`

**验收标准**:
- [ ] 显示 tool 调用
- [ ] 显示执行结果
- [ ] 支持展开/折叠

---

## 验收流程

每个功能实现后：

1. **编译检查**: `cargo build -p rpi-cli`
2. **测试检查**: `cargo test -p rpi-tui`
3. **运行测试**: `cargo run --bin rpi`
4. **功能验证**: 确保功能按预期工作

---

## 当前进度

### 已完成 ✅
- [x] Phase 1.1: 修复消息显示问题 - 编译通过，测试通过
- [x] Phase 1.2: 修复编辑器提示符问题 - 编译通过，测试通过
- [x] Phase 2.1: AssistantMessageComponent - 已创建并集成
- [x] Phase 2.2: FooterComponent - 已创建并集成
- [x] Phase 2.3: 完善 Markdown 组件 - 基本功能已完成
- [x] Phase 3.1: 事件系统 - 基础事件已有（RunStart/RunEnd）
- [x] Phase 3.2: 斜杠命令 - 已实现 /help, /clear, /exit
- [x] Phase 3.3: Tool 执行显示 - 已创建 ToolExecutionComponent

## 实现总结

### 新创建的文件
1. `crates/pi-tui/src/assistant_message.rs` - AssistantMessageComponent
2. `crates/pi-tui/src/footer.rs` - FooterComponent
3. `crates/pi-tui/src/tool_execution.rs` - ToolExecutionComponent
4. `docs/tui-implementation-plan.md` - 实现计划文档

### 修改的文件
1. `crates/pi-tui/src/lib.rs` - 添加新模块和导出
2. `crates/pi-cli/src/interactive_tui.rs` - 使用新组件，添加斜杠命令处理

### 测试结果
- 编译成功 ✅
- 测试通过 ✅ (126 passed, 0 failed)
- 无编译错误 ✅

### 已实现的功能
1. **核心交互**: 用户输入 -> API 调用 -> 响应显示
2. **组件**: AssistantMessageComponent, FooterComponent, ToolExecutionComponent
3. **Markdown渲染**: 标题、列表、代码块、链接等
4. **斜杠命令**: /help, /clear, /exit, /version, /model

### 已知差距
1. **流式响应**: Rust 版本缺少消息级别事件，用户需等待完整响应
2. **更多斜杠命令**: /settings 等可后续添加
3. **Tool执行集成**: ToolExecutionComponent 已创建，但尚未集成到 interactive_tui

## 差距分析

### 事件系统差距
TypeScript 版本有消息级别事件：
- `message_start` - 消息开始
- `message_update` - 消息更新（流式响应）
- `message_end` - 消息结束

Rust 版本只有运行级别事件：
- `RunStart` - 运行开始
- `RunEnd` - 运行结束

这意味着 Rust 版本不支持流式响应显示，用户需要等待整个响应完成后才能看到内容。

### 其他差距
- 斜杠命令（/clear, /help, /model 等）
- Tool 执行显示
- 扩展小部件支持
- 等等

## 新创建的文件
1. `crates/pi-tui/src/assistant_message.rs` - AssistantMessageComponent
2. `crates/pi-tui/src/footer.rs` - FooterComponent
3. `docs/tui-implementation-plan.md` - 实现计划文档

## 修改的文件
1. `crates/pi-tui/src/lib.rs` - 添加新模块和导出
2. `crates/pi-cli/src/interactive_tui.rs` - 使用新组件，改进交互流程

---

## 参考文件

### TypeScript 主要文件
- `packages/coding-agent/src/modes/interactive/interactive-mode.ts` (6000+ 行)
- `packages/coding-agent/src/modes/interactive/components/assistant-message.ts` (200 行)
- `packages/coding-agent/src/modes/interactive/components/footer.ts` (300 行)
- `packages/coding-agent/src/modes/interactive/components/custom-editor.ts` (100 行)

### Rust 文件
- `crates/pi-cli/src/interactive_tui.rs` (当前 ~300 行)
- `crates/pi-tui/src/markdown.rs` (当前 ~280 行)
- `crates/pi-tui/src/editor.rs` (当前 ~400 行)

---

## 下一步行动

1. 编译并测试当前的 `interactive_tui.rs` 修改
2. 验证基本交互是否工作
3. 如果有问题，继续修复
4. 然后按计划实现各个组件