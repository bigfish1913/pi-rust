# RPI Interactive Content Agent

这是一个基于本机 `pi-rust` / RPI SDK 的互动内容生成 agent。直接在当前目录启动即可，RPI 会自动发现：

- `.pi/SYSTEM.md` 和 `.pi/APPEND_SYSTEM.md`
- `.pi/skills/**/SKILL.md`
- `rpi-cli` 默认 coding tools
- 本项目的 interactive-content tools

## 启动

在 PowerShell 中执行：

```powershell
.\start-rpi.ps1
```

或者：

```powershell
cargo run -p rpi-cli --
```

需要联网模型时，先配置 `OPENAI_API_KEY` 或 `ANTHROPIC_API_KEY`。没有 API key 时可以用 `--help` 检查安装，工具本身的本地逻辑仍可通过集成测试运行。

## 典型对话

```text
请为“浏览器如何渲染一帧页面”制作 5 个镜头的互动课程，搜索资料，生成场景、配音时间线、预览并完成质检。
```

生成结果在 `artifacts/`：场景 HTML、React/Vite 应用、SVG/图标/图片素材、音频时长元数据、预览页和最终 manifest。`search` 与 `fetch` 使用公开 HTTP API；真实 TTS/图像服务目前保留 `RPI_TTS_ENDPOINT` / `RPI_IMAGE_ENDPOINT` 适配位，未配置时生成可审阅的本地估算或 SVG 占位。

## 已注册工具

`spec`、`storyboard`、`timeline`、`generate_voiceover`、`timeline_calibration`、`generate_scene`、`whiteboard_raster`、`search_brand_icon`、`search_real_image`、`generate_ai_image`、`check_scene`、`contract_check`、`search`、`fetch`、`build_preview`、`generate_react_app`、`finish_project`，以及 RPI 自带的 `read`、`write`、`edit`、`bash`、`grep`、`find`、`ls`。

`check_scene` 在没有浏览器服务时执行可复现的静态 HTML/JS 检查，并明确报告检查模式；后续可把真实浏览器沙盒接到同一工具接口。
