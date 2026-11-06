# rpi 运维速查：skills、诊断、provider、会话

这是一份按**运维任务**组织的速查页，不是完整手册。每条事实都能在上游文档或 CLI 帮助里找到对应实现；需要完整背景时请看 `guide` / `debugging` / `agent`。

## 1. skills（技能）

skills 是运行时可激活的领域知识，存放在资源目录的 `skills/` 下，每个技能一个 `SKILL.md`。

发现顺序（同名资源以更靠前者为准）：

1. 项目 `.rpi/skills/`
2. 兼容的项目 `.pi/skills/`
3. 全局 `~/.rpi/agent/skills/`
4. `settings.json` 的 `skillDirs` 追加目录（相对项目根；全局设置里相对 agent 目录）
5. 项目 package 提供的 skills

常用操作：

```bash
rpi --no-skills                 # 本次运行跳过技能发现
rpi --debug-system-prompt       # 打印最终系统提示词，含 skills 与计数
```

排查清单：

- 技能没生效：先用 `--debug-system-prompt` 确认它是否被发现并计入最终提示词。
- 同名技能被覆盖：项目优先于全局，`.rpi` 优先于 `.pi`；确认是否被上层目录的同名技能遮蔽。
- 只想加载当前项目资源：`rpi --package-only`（等价 `--project-only`），它同时加载项目的 skills/prompts/themes/extensions/packages。
- `--local-only` **不是**同一件事：它只加载通过 `--extensions-dir` / `--extension` 显式指定的扩展。
- 技能文件是普通源码，应当可审查、进版本库；不要依赖扩展运行期生成。

## 2. doctor（诊断与自检）

rpi 没有名为 `doctor` 的子命令；自检能力分散在下面几个明确入口。

```bash
rpi auth check                      # 报告可用认证源（不联网）
rpi auth check --provider gateway --json
rpi auth login | logout
rpi --debug-system-prompt           # 最终系统提示词、skills、prompts、package 资源与计数
RPI_EVENT_LOG=1 rpi                 # 记录所有扩展事件处理器调用
rpi events path | tail              # 事件日志路径 / 跟踪（Ctrl+C 停止）
RPI_AGENT_DIAGNOSTICS=1 RPI_FRAME_DIAGNOSTICS=1 rpi -p "复现问题" 2>rpi-diagnostics.log
```

诊断变量说明：

- `RPI_AGENT_DIAGNOSTICS`：上下文构建、assistant 请求转换/建流、工具执行、会话 append 延迟。
- `RPI_FRAME_DIAGNOSTICS`：durable frame 批处理、flush barrier、writer 失败。
- 两者在未设置或设为 `0` 时关闭，不改变 JSONL 格式。

`auth check` 的边界：它只检查本地未过期凭据，**不访问服务端、不刷新 token**，因此在请求仍可自动刷新时可能报告缺少凭据。

事件日志默认在 `~/.rpi/logs/events.jsonl`（`RPI_EVENT_LOG_PATH` 可覆盖）。若 `BeforeTuiStart` 阶段有扩展 veto（`EVENT_HANDLER_ABORT`），CLI 以退出码 `3` 中止启动并打印原因。

常见定位表：

| 症状 | 入口 |
| --- | --- |
| 找不到模型 / 接口报错 | `rpi auth check`，确认 provider、`models.json`、环境变量、`baseUrl` |
| 资源没出现在系统提示词 | `--debug-system-prompt` |
| 事件处理器没触发 | `RPI_EVENT_LOG=1` + `rpi events tail`，看 `Abort`/`Panic`/`Timeout` |
| 扩展加载失败 | 确认 cdylib 与平台匹配；`--extensions-dir` 指向构建目录 |
| 远程模式连不上 | 服务端 `--server --port`，token 用 `--token` 或 `RPI_SERVER_TOKEN` |

## 3. provider 与 models.json

配置文件默认 `~/.rpi/agent/models.json`；`RPI_CODING_AGENT_DIR` 可重定位 agent 配置目录。

凭据优先级（同时存在时从高到低）：

1. `--api-key`
2. `~/.rpi/auth.json`（`rpi auth login` 写入）
3. `models.json` 中对应 provider 的 `apiKey`
4. 环境变量（Anthropic 用 `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`；OpenAI-compatible 用 `OPENAI_API_KEY`）

最小 provider 配置：

```jsonc
{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "sk-gateway-secret",
      "models": [{ "id": "custom-claude", "name": "Custom Claude" }]
    }
  }
}
```

选择与切换：

```bash
rpi --list-models                        # 列出可用模型（可带模糊搜索）
rpi --model gateway/custom-claude -p hello
rpi --provider gateway --model custom-claude -p hello
```

要点：

- 不传 `--model` 时优先选择已认证模型；只有自定义 gateway 已认证时，选它的第一个可用模型。
- 用完整的 `provider/model` 可避免同名模型歧义。
- 不要把密钥写进 Git 仓库、项目文档或 package manifest。
- Anthropic 兼容网关另外支持 `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN`。

## 4. sessions（会话）

会话默认保存到 agent 配置目录下的 `sessions/`，按 cwd 编码分子目录。

常用参数：

```bash
rpi -c                    # 继续最近会话
rpi -r                    # 选择历史会话
rpi --session <id|path>   # 指定会话
rpi --session-dir <dir>   # 指定会话目录（或 settings.json 的 sessionDir / RPI_CODING_AGENT_SESSION_DIR）
rpi --name <name>         # 设置会话显示名称
rpi --export <file>       # 导出为 HTML
rpi --no-session          # 使用临时会话
```

选择器的 scope：`/session` 与 `-r` 默认只列**当前项目**的会话（按 cwd 编码目录定位），Tab 切到全部项目并显示项目标识；每次打开重置为当前项目。

排查：

- 会话列表为空：确认 `--session-dir` / `settings.json` 的 `sessionDir` / `RPI_CODING_AGENT_SESSION_DIR` 指向的根目录正确，且当前 cwd 对应的编码子目录存在。
- 看到别的项目的会话：那是 All 视图；按 Tab 回到当前项目。
- 恢复后目录不对：会话头部记录创建时的 cwd；该目录不存在时回退到启动 cwd 并给出提示。

## 相关主题

- `guide`：完整使用手册（安装、CLI、资源优先级、插件、发布）。
- `debugging`：诊断变量字段参考、扩展调试、常见失败定位。
- `agent`：用 SDK 建自己的 Agent 项目时的目录布局与 skills/prompts 约定。
