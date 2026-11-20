# Kitty 图片重绘：回归与真实终端验收

对应 PR #384。修改仅在终端渲染层：不改变 provider 请求、图片归一化或 JSONL 会话内容。
Kitty（例如 Ghostty）的已上传图片可以用 `a=p` 放置；iTerm2 内联协议不使用这条优化路径。

## 缓存安全规则

- 普通空闲帧和 main-screen 的 live-suffix 修复复用图片数据，不反复搬运 PNG payload。
- 全屏清屏、窗口/像素几何变化、同尺寸 resize 通知、显式强制重绘：**先撤销上传信任，再布局**，使本帧重新产生 `a=T`。
- 返回外部 UI 会强制重绘，两种模式都重传当前需要绘制的图片。
- fullscreen 只信任上一帧仍显示的图片；图片离屏后再返回会重新上传，不承诺“一生只传一次”。
- ScrollView 的复用文档同时校验上传作用域，不能在图片已被忘记后继续使用缓存的 `a=p` 行。
- main-screen 已进入原生 scrollback 的内容是不可变快照；强制重绘只修复 live 区域，不保证恢复终端已淘汰的历史图片。

核心兼容性假设：普通 main-screen 修复中的 `EL 2`（`\x1b[2K`）不释放终端图片数据。
如果真实终端不满足这一假设，不能仅凭测试通过就合并；需要针对该终端采用更保守的重传策略。
终端自行回收图片而应用没有收到事件时，应用也无法保证即时发现。

## 自动回归

```bash
cargo test -p rpi-tui --test image_repaint_cost
cargo test -p rpi-tui --test main_screen_scroll_up
cargo test -p rpi-cli --lib live_images
cargo fmt --all -- --check
RUSTFLAGS="-D warnings" cargo check --workspace --all-targets --locked
```

`image_repaint_cost` 覆盖：首次上传、placement 稳态、空闲零写出、不同 payload 大小、
全屏强制清屏、行列变化、像素几何变化、同尺寸 resize、main-screen 强制恢复、
缓存滚动的离屏返回、外部 UI 返回。

新增的全屏强制清屏/几何变化、main-screen 强制恢复、缓存滚动离屏返回用例在
失效顺序修复前失败，修复后通过。测试终端记录协议输出，并不解码 PNG 或模拟真实终端的图片内存。

完整 `cargo test -p rpi-tui --no-fail-fast` 在本机仍有两个已在干净基线复现的计时失败：
`completed_panel_reports_total_elapsed`、`streaming_burst_paints_at_most_one_frame_per_interval`。
应保留这个结果，不把跳过测试后的结果描述成完整套件通过。

## 合成性能探针

```bash
cargo run --release -p rpi-tui --example probe_image_transcript -- 38 2.4
```

一次本地测量（38 张图，120×50 终端，未设置性能阈值）：

| 路径 | 固定 payload 行 | 实际 Image 组件稳态 | 稳态终端写出 |
| --- | ---: | ---: | ---: |
| regular/main-screen | 1027.816 ms/帧 | 1.582 ms/帧 | 0 B/帧 |
| fullscreen | 46.284 ms/帧 | 0.077 ms/帧 | 22 B/帧（协议开销） |

这不是旧二进制与新二进制的同输入 A/B，也不是和 TypeScript Pi 的性能比较。
固定行直接填充约 91 MiB 的字符串，Image 的输入是约 91 MiB 原始字节，编码后首次写出约 121 MiB；
两种表示的体积不同。探针用于说明稳态成本不再随图片 payload 搬运增长，不能代替现场 CPU/显示验收。
原报告中持续触发空闲重绘的来源尚未最终确认。

## 真实 Ghostty 验收（尚待人工完成）

在 PR 的独立 worktree 中启动构建，不安装、不覆盖当前正式版。使用有多张 PNG 的测试会话，
优先用无敏感信息的合成截图。如果必须使用原始报告会话，先复制到临时目录，勿直接操作原 JSONL：

```bash
cd /Users/bigfish/work/rpi-projects/pi-rust-wt-image-repaint
fixture_dir=$(mktemp -d)
cp /absolute/path/to/image-session.jsonl "$fixture_dir/images.jsonl"
cargo run --release -p rpi-cli -- \
  --offline --no-extensions --tui-mode regular --session "$fixture_dir/images.jsonl"
# 退出后用相同命令、--tui-mode fullscreen 复测。
```

验收不需要提交新 prompt。`--offline` 只禁用启动网络检查，**不是网络沙箱，也不禁止向模型发送请求**；
不要为了验证显示而把私有图片再次提交给模型。复制的会话仍含私有内容，不上传到 PR/CI，验收后自行清理。

记录 rpi commit、Ghostty/macOS 版本、窗口大小、字体大小、图片数量与大致体积，再逐项填写：

- [ ] 首次显示后图片完整，文字不被遮盖；保持空闲至少一分钟，记录进程 CPU（不要把首次加载算进稳态）。
- [ ] 反复缩放窗口；改字体大小，使字符网格和像素几何分别发生变化；没有空白、错位或重复图。
- [ ] fullscreen 反复滚动，让图片完全离开再返回；图像仍能显示，后续空闲无持续 payload 重传。
- [ ] regular 原生回看后回到底部；没有重复历史、异常清空 scrollback 或输入区归档。
- [ ] 打开并关闭现有对话框/自定义 UI；恢复后 live 区图片正常。启用扩展的场景另行复测可信扩展。
- [ ] 持续文字输出/输入时图片不闪烁、不残留。若通过真实模型产生流式输出，只使用非敏感测试图片。
- [ ] 另一个宣称支持 Kitty 的终端重复核心步骤；不要把 iTerm2 协议的结果当作 Kitty 兼容性证据。

**合并条件：**新增协议回归通过、CI 失败有明确处置、至少报告中的 Ghostty 场景人工验收通过。
尚未完成上述视觉验收时，不将 PR 描述为已验证所有终端；本次修复不顺带引入图片压缩或模型输入策略变化。
