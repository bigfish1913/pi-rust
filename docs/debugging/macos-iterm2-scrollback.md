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
| `streamed_output_growth_preserves_history_commands_and_draft` | 使用真实 BashExecutionComponent 连续追加输出；用户回看历史时不清空 scrollback、不跳位，43 行输出各出现一次，完整工具命令、编辑器草稿和 footer 各出现一次。 |
| `cursor_above_live_viewport_does_not_duplicate_or_drop_command_rows` | 12 条唯一命令、4 行 live screen、光标位于文档第 6 行。首次绘制及相同内容再次刷新后，每条命令必须完整且只出现一次，不得把 06/07 重复绘制到后面并丢失 10/11。 |

这是测试先行的 **RED 阶段**，尚未修改生产渲染逻辑。新增三条中，计时刷新与越界光标两条当前失败，正常输出增长的对照用例通过；连同既有八条共 **9 通过、2 失败、0 ignored**。默认 workspace 测试会运行它们，因此当前补丁不是可合并的全绿修复。后续修复不能添加 ignore、反转断言或删除原有历史重排测试来取得 PASS。

虚拟终端只检查协议与内容契约：不能验证 iTerm2 的 GPU 绘制、同步输出超时、触控板滚动动画或完整 Unicode 字宽兼容性。模型中的浏览锚点是验收模型，不代表应用可从真实 iTerm2 查询该锚点。

## iTerm2 人工验收（修复后仍必需）

1. 使用已知待测 binary，记录版本/commit、iTerm2 版本、是否经过 tmux，以及窗口行列数。在隔离会话中显式使用 `--tui-mode regular`；不读取真实用户会话或调用付费 provider 作为默认测试。
2. 准备远长于一屏的历史，以及仍在运行的长工具结果，使工具标题/耗时行进入 scrollback。记录一条早期消息和一条完整命令作为唯一标记。
3. 向上滚动并停留在早期标记；后台继续刷新耗时和追加工具输出。至少观察多个刷新周期，不能只比较最后一帧。
4. 验收：不闪空屏、不自动跳回底部；原生历史中的标记不移位、不重复、不缺行。返回底部后，新增输出完整，工具命令、输入草稿与 footer 不被覆盖。
5. 覆盖折叠/展开工具结果、多行草稿或较高 footer，使光标可能落在 live viewport 上方。每行命令均应完整且仅出现一次。
6. 窗口缩放与字体变化单独验证，不将必要的重排与纯计时刷新混为同一要求。再用 `--tui-mode fullscreen` 做对照；fullscreen 通过不能代替 regular 验收。

缺少真实 macOS / iTerm2 环境时，人工项记为 **未执行**，不能用虚拟终端 PASS 代替。
