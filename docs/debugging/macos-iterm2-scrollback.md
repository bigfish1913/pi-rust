# macOS / iTerm2 长会话原生滚动验收

## 已确认的范围与提交

报告条件：正式版 **v0.16.0**、macOS / iTerm2、长会话向上回看时，后台仍在生成回复或执行工具；出现闪屏、消息重复和命令显示不全。最后正常版本未知。

两条可复现的协议缺陷应分别归因，不应合称某个 v0.16 新 PR 的回归：

| 路径 | 引入提交 / PR | 证据与边界 |
| --- | --- | --- |
| 历史行变化触发整段 scrollback 清空重建 | [`e59a6bb`](https://github.com/bigfish1913/pi-rust/commit/e59a6bb3c29be9f9f832af4267ebebd6b770c954)，`fix(rpi-tui): rebuild scrollback when wrapped rows change`；首次包含于 v0.4.1 | 新增 `changed_in_scrollback` 条件，将普通历史行变化送入 `ED 2 → HOME → ED 3 → 重写整个文档`。运行中工具的耗时标签也可触发。GitHub commit/pulls 查询返回空列表，未发现关联 PR，不推测 PR 号。 |
| 光标在 live viewport 上方时，误用 CSI T 取回历史 | [`af296aa`](https://github.com/bigfish1913/pi-rust/commit/af296aa1f63f1937b2a5166bbbe704cbd8343e72)，经 [PR #33](https://github.com/bigfish1913/pi-rust/pull/33) 合入（merge `31e8f92`）；首次包含于 v0.3.18 | `CSI T` 只向当前 live screen 插入空行，却被当作物理历史边界已上移。12 行 / 4 屏幕行用例可复现中间两行重复、末尾两行被覆盖。尚不能证明用户现场必然进入该分支。 |

`e59a6bb` 原本为修复历史重排后的重复追加，并非可以直接回退的无关改动；修复必须继续保留已有内容正确性测试。

对比 v0.15.0 → v0.16.0，主屏差分算法没有变化。相同的光标重复/缺行用例在 v0.14.0 和 v0.16.0 历史源码上都失败。故目前不能确定“v0.16 新增了这个问题”，也不能只凭现有信息指定唯一回归版本。后续 preview 的 `763c19e` 历史分页改动不在正式版 v0.16.0 中，不能用于解释该版本的报告。

## 修复策略与行为边界

- regular 主屏的运行中工具面板使用稳定状态，不嵌入每 100 ms 变化的耗时。Bash 的取消提示也固定显示，不再到时间阈值才增加一行。实时计时保留在输入区的 working/status dock；fullscreen 工具计时和完成后的总耗时不变。渲染上下文仅限当前帧和线程，嵌套/异常退出会恢复，不引入全局模式开关。
- 光标位于 saved scrollback 时隐藏硬件光标，保留真实 live viewport 和物理光标位置；等逻辑光标重新进入 live screen 再定位/显示，不使用 `CSI T` 改写历史。遵守隐藏光标设置，列 0 不发送会右移一格的 `CSI 0 C`。
- 主屏 `render_now(true)` 保留差分基线和物理账本，只强制重画 live rows，不会把整个会话再次追加在旧光标处。重绘前清行，并清理短文档下方的空白 live rows，以修复外部 UI 留下的行首、行尾和空白区残留；不清 saved scrollback。
- **并未禁止所有历史重建。** 真实历史内容编辑、折叠/展开、主题或几何变化仍可能需要重建；不能用这次修复承诺这些操作都保留原生回看锚点，也不能声称已在 iTerm2 实机证实所有闪屏消失。

## 自动验收

入口：

```bash
cargo test -p rpi-tui --test main_screen_scroll_up acceptance:: -- --nocapture
cargo test -p rpi-tui --test main_screen_scroll_up -- --nocapture
```

测试文件：

- `crates/rpi-tui/tests/main_screen_scroll_up.rs`：共享虚拟终端；分别保存应用 live viewport 和用户原生历史浏览锚点，并统计 `ED 3`。
- `crates/rpi-tui/tests/main_screen_scroll_up/acceptance.rs`：本次验收契约。

| 用例 | 验收要求 |
| --- | --- |
| `timer_only_updates_preserve_native_history_without_erasing_scrollback` | 80 行历史 + 长工具结果 + editor/footer；用户回看第 10 行，仅改变历史区耗时标签三次。不得发 `ED 3`，浏览锚点与可见历史不变，既有消息顺序和数量不变。使用真实 ToolExecutionComponent，在每次最多 2 秒的有界轮询中等待可见耗时变化，不假定固定 sleep 后一定变化；期间不修改工具输出、参数或运行状态。 |
| `streamed_output_growth_preserves_history_commands_and_draft` | 使用真实 BashExecutionComponent 连续追加输出；逐帧验证回看锚点不变、当前全部输出各出现一次，以及完整工具命令、编辑器草稿和 footer 各出现一次，不只检查最后一帧。 |
| `cursor_above_live_viewport_does_not_duplicate_or_drop_command_rows` | 12 条唯一命令、4 行 live screen、光标位于文档第 6 行。首次绘制及相同内容再次刷新后，每条命令必须完整且只出现一次，不得把 06/07 重复绘制到后面并丢失 10/11。 |
| `offscreen_cursor_can_return_without_corrupting_the_physical_ledger` | 光标在历史区与可见区之间往返，验证隐藏/恢复、物理行列位置和消息完整性。 |
| `column_zero_cursor_and_hidden_cursor_preference_are_respected` | 逻辑列 0 必须保持在列 0，并遵守硬件光标显示设置。 |
| `forced_repaints_repair_live_rows_without_appending_or_erasing_history` | 三次污染 live 行首再强制重绘，验证修复该单元格、不追加消息、不清历史、浏览锚点不变。 |
| `forced_repaint_clears_line_suffixes_and_unused_live_rows` | 污染短行逻辑尾部以外的单元格和下方空白行，强制重绘后均清除；随后追加仍从真实尾部开始。 |

PR 初始 RED 提交 `39d5494` 为 **9 通过、2 失败**；补上生产修复和四条验收后，该集成测试目标为 **15 通过、0 失败、0 ignored**。默认 workspace 测试会运行它们，没有添加 ignore、反转断言或删除原有历史重排测试。工具/Bash 单元测试另外验证 regular 的时钟独立性、fullscreen 活跃计时及两种模式下的完成总耗时；渲染上下文测试覆盖嵌套、线程隔离和 unwind 恢复。

虚拟终端只检查协议与内容契约：不能验证 iTerm2 的 GPU 绘制、同步输出超时、触控板滚动动画或完整 Unicode 字宽兼容性。模型中的浏览锚点是验收模型，不代表应用可从真实 iTerm2 查询该锚点。

## iTerm2 人工验收（修复后仍必需）

1. 使用已知待测 binary，记录版本/commit、iTerm2 版本、是否经过 tmux，以及窗口行列数。在隔离会话中显式使用 `--tui-mode regular`；不读取真实用户会话或调用付费 provider 作为默认测试。
2. 准备远长于一屏的历史，以及仍在运行的长工具结果，使工具标题/状态行进入 scrollback。记录一条早期消息和一条完整命令作为唯一标记。
3. 向上滚动并停留在早期标记；后台继续刷新输入区工作计时和追加工具输出。regular 的工具面板不再显示活跃计时，完成后应显示总耗时。至少观察多个刷新周期，不能只比较最后一帧。
4. 验收：不闪空屏、不自动跳回底部；原生历史中的标记不移位、不重复、不缺行。返回底部后，新增输出完整，工具命令、输入草稿与 footer 不被覆盖。
5. 覆盖折叠/展开工具结果、多行草稿或较高 footer，使光标可能落在 live viewport 上方。每行命令均应完整且仅出现一次。
6. 窗口缩放与字体变化单独验证，不将必要的重排与纯计时刷新混为同一要求。再用 `--tui-mode fullscreen` 做对照；fullscreen 通过不能代替 regular 验收。

缺少真实 macOS / iTerm2 环境时，人工项记为 **未执行**，不能用虚拟终端 PASS 代替。
