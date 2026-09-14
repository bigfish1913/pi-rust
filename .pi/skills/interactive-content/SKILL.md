---
name: interactive-content
description: 将知识主题编排成可交互的 React/Web 分镜项目，覆盖策划、时间线、素材、生成、质检和交付。
---

## 目标

交付一个可以在浏览器打开的多镜头互动内容项目，而不是只返回一段说明文字。

## 推荐流程

1. `search` / `fetch` 事实核验。
2. `spec` 或 `storyboard` 产出镜头结构。
3. `timeline` 计算时间和图层。
4. `generate_voiceover` 后把实际或估算时长交给 `timeline_calibration`。
5. 对每个镜头调用 `generate_scene`，手绘路线调用 `whiteboard_raster`。
6. 通过 `search_brand_icon`、`search_real_image`、`generate_ai_image` 补齐素材。
7. `contract_check`、`check_scene`、`build_preview`。
8. `finish_project` 写入交付 manifest。

## 质量要求

- 每个镜头有稳定 id、旁白、duration、视觉重点和 enter/hold/exit 动效节奏。
- 所有外部素材记录来源和授权核验提醒。
- 生成失败时保留可读错误，不要静默降级。
