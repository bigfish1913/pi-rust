---
name: reactor-web-delivery
description: 把分镜产物整理成可运行的 React/Web 预览，强调组件边界、响应式布局和真实浏览器检查。
---

使用 `generate_scene` 生成独立镜头文件，再用 `build_preview` 汇总。预览必须能在本地静态服务器中打开，移动端不能出现文字遮挡或固定尺寸溢出。修改已有场景时使用 `read`/`grep`/`edit`，不要重新生成覆盖用户调整。

完成交付前检查：scene 文件存在、HTML 根节点存在、无明显运行时错误、storyboard-v1 合约通过、素材来源已列出。
