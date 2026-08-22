# pi TUI 完整差距分析报告

## 概述

对比 TypeScript 版本的 pi TUI（`packages/coding-agent/src/modes/interactive/interactive-mode.ts`，6000+ 行）与 Rust 版本（`crates/pi-cli/src/interactive_tui.rs`，591 行），差距非常大。

## 一、库层对比（pi-tui）

### ✅ 已实现（Rust 有对应）
| TypeScript 文件 | Rust 文件 | 状态 |
|----------------|-----------|------|
| alt-screen-search.ts | alt_screen_search.rs | ✅ |
| autocomplete.ts | autocomplete.rs | ✅ |
| box.ts | box_component.rs | ✅ |
| cancellable-loader.ts | loader.rs (CancellableLoader) | ✅ |
| editor.ts | editor.rs | ✅ |
| h-stack.ts | hstack.rs | ✅ |
| image.ts | image.rs | ✅ |
| input.ts | input.rs | ✅ |
| loader.ts | loader.rs | ✅ |
| markdown.ts | markdown.rs | ⚠️ 简化版 |
| scroll-view.ts | scroll_view.rs | ✅ |
| select-list.ts | select_list.rs | ✅ |
| settings-list.ts | settings_list.rs | ✅ |
| spacer.ts | spacer.rs | ✅ |
| stack.ts | vstack.rs (部分) | ⚠️ 缺少抽象基类 |
| text.ts | text.rs | ✅ |
| truncated-text.ts | truncated_text.rs | ✅ |
| v-stack.ts | vstack.rs | ✅ |
| fuzzy.ts | fuzzy.rs | ✅ |
| keybindings.ts | keybindings.rs | ✅ |
| keys.ts | keys.rs | ✅ |
| kill-ring.ts | kill_ring.rs | ✅ |
| latex.ts | latex.rs | ✅ |
| layout.ts | layout.rs | ✅ |
| layout-node.ts | layout_node.rs | ✅ |
| stdin-buffer.ts | stdin_buffer.rs | ✅ |
| terminal.ts | terminal.rs | ✅ |
| terminal-colors.ts | terminal_colors.rs | ✅ |
| terminal-image.ts | terminal_image.rs | ✅ |
| tui.ts | tui.rs | ✅ |
| tui-alt-screen.ts | tui_alt_screen.rs | ✅ |
| tui-main-screen.ts | tui_main_screen.rs | ✅ |
| undo-stack.ts | undo_stack.rs | ✅ |
| utils.ts | utils.rs | ✅ |
| word-navigation.ts | word_navigation.rs | ✅ |

### ❌ 缺失
| TypeScript 文件 | 说明 |
|----------------|------|
| editor-component.ts | EditorComponent 接口/trait，允许自定义编辑器 |
| native-modifiers.ts | 原生修饰键检测（Node 原生模块，Rust 可用 crossterm 替代） |
| components/alt-screen-flash.ts | 临时消息闪现组件（AltScreenFlashContainer） |

## 二、Interactive 组件层对比（37 个缺失）

### ✅ 已实现
| TypeScript 组件 | Rust 实现 | 状态 |
|----------------|-----------|------|
| assistant-message.ts | assistant_message.rs | ⚠️ 简化版（无 thinking 块、无流式） |
| tool-execution.ts | tool_execution.rs | ⚠️ 简化版（未集成） |
| footer.ts | footer.rs | ⚠️ 简化版（无动态状态） |
| custom-editor.ts | editor.rs | ⚠️ 简化版（无自动完成、无斜杠命令提示） |

### ❌ 缺失组件（37 个）
| 组件 | 用途 |
|------|------|
| user-message.ts | 用户消息显示（带头像/样式） |
| bash-execution.ts | Bash 命令执行显示 |
| bordered-loader.ts | 带边框的加载指示器 |
| branch-summary-message.ts | 分支摘要 |
| compaction-summary-message.ts | 压缩摘要 |
| config-selector.ts | 配置选择器（31KB 大文件） |
| countdown-timer.ts | 倒计时器 |
| custom-entry.ts | 自定义条目 |
| custom-message.ts | 自定义消息 |
| diff.ts | 差异显示 |
| dynamic-border.ts | 动态边框 |
| status-indicator.ts | 状态指示器（Working/Loading 等） |
| keybinding-hints.ts | 键绑定提示 |
| markdown-transform.ts | Markdown 转换器 |
| mermaid.ts | Mermaid 图表渲染 |
| model-selector.ts | 模型选择器 |
| session-selector.ts | 会话选择器（33KB） |
| session-selector-search.ts | 会话搜索 |
| settings-selector.ts | 设置选择器 |
| theme-selector.ts | 主题选择器 |
| thinking-selector.ts | 思考级别选择器 |
| tree-selector.ts | 树选择器 |
| trust-selector.ts | 信任选择器 |
| show-images-selector.ts | 图片显示选择器 |
| scoped-models-selector.ts | 作用域模型选择器 |
| oauth-selector.ts | OAuth 选择器 |
| login-dialog.ts | 登录对话框 |
| user-message-selector.ts | 用户消息选择器 |
| visual-truncate.ts | 视觉截断 |
| first-time-setup.ts | 首次设置 |
| earendil-announcement.ts | 公告 |
| extension-editor.ts | 扩展编辑器 |
| extension-input.ts | 扩展输入 |
| extension-selector.ts | 扩展选择器 |
| skill-invocation-message.ts | 技能调用消息 |
| armin.ts / daxnuts.ts | 彩蛋组件 |

## 三、核心交互逻辑差距（interactive-mode.ts vs interactive_tui.rs）

### TypeScript 核心功能（6000+ 行）
| 功能 | TypeScript | Rust | 优先级 |
|------|-----------|------|--------|
| 多容器布局（documentContainer, chatContainer, statusContainer 等） | ✅ | ❌ 只有一个 chat | 高 |
| 流式响应（streamingComponent + message_update 事件） | ✅ | ❌ | 高 |
| 事件处理（subscribeToAgent + handleEvent） | ✅ | ❌ | 高 |
| 消息渲染（addMessageToChat, renderSessionItems） | ✅ | ❌ | 高 |
| 键盘处理（setupKeyHandlers, 全部快捷键） | ✅ | ❌ 只有 Ctrl+C | 高 |
| 编辑器提交（setupEditorSubmitHandler, 完整斜杠命令） | ✅ | ⚠️ 只有 5 个命令 | 高 |
| Tool 执行显示（pendingTools + ToolExecutionComponent） | ✅ | ❌ 未集成 | 高 |
| Bash 执行（isBashMode + BashExecutionComponent） | ✅ | ❌ | 中 |
| 状态指示（showStatusIndicator, Working...） | ✅ | ❌ | 中 |
| 会话管理（rebindCurrentSession, resume） | ✅ | ❌ | 中 |
| 认证（login, logout, OAuth） | ✅ | ❌ | 低 |
| 主题系统（InteractiveThemeController） | ✅ | ❌ | 中 |
| 扩展系统（extension UI, widgets） | ✅ | ❌ | 低 |
| 自动完成（autocompleteProvider） | ✅ | ❌ | 中 |
| Footer 数据（FooterDataProvider） | ✅ | ❌ | 中 |
| 信号处理（Ctrl+C/D/Z, shutdown） | ✅ | ❌ | 高 |
| 压缩（/compact 命令） | ✅ | ❌ | 低 |
| 导出/导入/分享（/export, /import, /share） | ✅ | ❌ | 低 |
| 模型切换（cycleModel, /model） | ✅ | ❌ | 中 |
| 缓存提示（maybeShowCacheMissNotice） | ✅ | ❌ | 低 |
| 标题更新（updateTerminalTitle） | ✅ | ❌ | 中 |

## 四、建议实现顺序

### 阶段 A：核心可用性（必须）
1. **修复输入/渲染问题** - 用户当前只看到 ">"
2. **多容器布局** - documentContainer / chatContainer / statusContainer / footerContainer
3. **流式响应** - 通过 AgentSession 事件流显示实时内容
4. **消息渲染** - user-message + assistant-message 完整实现
5. **键盘处理** - 完整快捷键（Ctrl+C 中断、Esc 取消等）
6. **信号处理** - Ctrl+C 中断运行、Ctrl+D 退出

### 阶段 B：功能完善
7. **完整斜杠命令** - /model, /session, /clear, /compact 等
8. **Tool 执行显示** - 集成 ToolExecutionComponent
9. **状态指示器** - Working... / Thinking... / Loading
10. **Footer 完整版** - 模型名、状态、提示
11. **自动完成** - 文件路径、斜杠命令、模型名

### 阶段 C：高级功能
12. **Bash 执行** - ! 前缀命令
13. **选择器** - 模型/会话/主题/设置选择器
14. **主题系统** - 颜色主题
15. **认证** - login/logout
16. **扩展系统** - 扩展 UI

## 五、当前已知 bug

1. **输入消息后无响应** - on_submit 回调需要正确触发 API 调用并显示结果
2. **欢迎信息不显示** - chat_container 内容没有渲染（需要调查 layout/paint 问题）
3. **调试信息被隐藏** - alternate screen 模式下 eprintln! 输出不可见
