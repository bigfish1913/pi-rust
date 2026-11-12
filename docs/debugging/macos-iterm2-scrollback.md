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
- **后续修复改为不可变的原生历史快照。** regular 不再为了旧行变化发送 `ED 3`，也不将整个逻辑文档再次追加。已经滚出的行保留输出时的版本；当前组件树和会话存储仍是最新内容。fullscreen 继续提供可重排的完整当前视图。原生历史不是可回写的第二份当前文档，不能同时承诺追改旧行和保留其浏览锚点。
- regular 中 todo/plan 默认显示紧凑的当前摘要，避免几十行面板吞掉回复和输入区；Ctrl+O（plan 也可用 `/plan show`）仍可显式展开完整内容。展开时将当前状态放在面板尾部，使完成状态不会只留在不可回写的旧标题处。fullscreen 的原有详细预览不变。
- 晚到的更新卡片仍保留在逻辑 transcript 的启动槽，同时增加 regular 专用的单行状态和更新命令，位于面板后、输入区前。它不会因历史结算、翻页或 remount 重复，也不会挤占 fullscreen 的输出。

## v0.22.1 后续报告：流式内容的中间帧

v0.22.1 已包含 PR #303 的写入时序修复，但等待 `drain_session()` 并不能保证
渲染线程读到完整组件树。后续排查找到两个发布窗口：

- `AssistantMessageComponent` 更新时先清空正在展示的 container，再逐块进行
  Markdown 转换、创建组件并加入 container。独立的定时渲染线程可在两块之间读到
  一条长回复仅剩第一块的中间状态，误判为大幅缩短的文档并进入 `ED 3` 重建。
- 历史结算先清理 live reply、再清空旧 page、最后挂载持久化 page。这些步骤原先
  不受 renderer 的 frame lock 保护，写入已完成仍可能绘制到空/不完整的 transcript。

修复采用离屏构建、完整发布：assistant 的正常块和错误内容在临时 container 中
构建完成后一次替换 children；历史 page/live 槽的同步替换使用同一个 frame lock，
使 regular 和 fullscreen 渲染只能发生在替换前或替换后。耗时的页面读取和组件构建
不放在 frame lock 内；锁内不 await、不请求重绘。

确定性回归在第二个 Markdown 块的转换处使用 channel 暂停更新，同时请求渲染。
原生产实现发送一次 `ED 3`（期望 0、实际 1）；修复后旧的完整回复、原生回看锚点、
既有消息顺序和数量均保持，放行转换后新的回复尾部正常出现。不使用 sleep 猜测竞态。
另一个测试让两种屏幕模式的 renderer 与批量组件替换并发，验证只绘制完整新树。

单独原子发布仍不足以解决晚到更新提示、todo 完成和 plan active→ready：三条真实
旧行变化路径的诊断均为 `ED3 delta=1`，原生回看锚点丢失。因此后续同时修改 regular
的渲染契约，而不是只隐藏中间帧：

1. 将逻辑行号与物理 saved-row 数量分开。未改变的前缀/后缀及唯一的 live 三行锚点，
   用来在插入旧提示、页面淘汰、重排后重新定位当前可写区域。
2. 若旧 live 行全部被移除，使用仍存活的唯一 saved 三行块确定已输出边界，防止
   折叠或“大幅缩短 + 更早行编辑”把旧消息再次拉回 live screen。重复/空白块不作身份锚点。
3. 在当前可写后缀内增量更新；需要重绘时，只清理 live screen（`ED 2`），不清 saved
   scrollback（不发 `ED 3`）。可靠锚点之后的新增输出全部写出，不能仅截最后一屏：
   resize/强制重绘和大量输出可以被合并进同一帧。
4. 已初始化后变为空文档不能重新走首次全量输出。零行列窗口推迟绘制，保留重绘义务；
   窗口回到相同行列的 resize 通知也必须修复 live cells。移除逻辑页只删除 live 的 Kitty
   placement，不删除已归档图像的 placement。

这是明确的行为改变：两个旧测试原先要求“编辑第 50 行后，终端保存的第 50 行必须变成
最新版本”，该要求与保留原生历史不可兼得。现在明确断言“保存的原版本不变、当前逻辑
版本已更新、不清历史、不增长/重复整个历史、最新输入区仍正确”；不是无说明删除断言。

**验收边界：**协议实现不主动清空原生历史，但终端自身的有界缓冲淘汰、缩放重排和
“输出时自动滚动到底部”设置不受应用控制。已滚出区域是历史快照，不保证能看到后来
重写的旧正文；需查看最新完整正文时使用 fullscreen。仍需用户在实际使用的终端复测，
协议测试不能代替实机视觉验收。

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
| `assistant_rebuild_keeps_the_previous_complete_frame_visible` | 在第二块 Markdown 转换时暂停后台更新并同步绘制，旧完整回复和原生回看锚点必须保持、不得发 `ED 3`；完成后新尾部可见。原生产实现确定性失败。 |
| `streamed_output_growth_preserves_history_commands_and_draft` | 使用真实 BashExecutionComponent 连续追加输出；逐帧验证回看锚点不变、当前全部输出各出现一次，以及完整工具命令、编辑器草稿和 footer 各出现一次，不只检查最后一帧。 |
| `cursor_above_live_viewport_does_not_duplicate_or_drop_command_rows` | 12 条唯一命令、4 行 live screen、光标位于文档第 6 行。首次绘制及相同内容再次刷新后，每条命令必须完整且只出现一次，不得把 06/07 重复绘制到后面并丢失 10/11。 |
| `offscreen_cursor_can_return_without_corrupting_the_physical_ledger` | 光标在历史区与可见区之间往返，验证隐藏/恢复、物理行列位置和消息完整性。 |
| `column_zero_cursor_and_hidden_cursor_preference_are_respected` | 逻辑列 0 必须保持在列 0，并遵守硬件光标显示设置。 |
| `forced_repaints_repair_live_rows_without_appending_or_erasing_history` | 三次污染 live 行首再强制重绘，验证修复该单元格、不追加消息、不清历史、浏览锚点不变。 |
| `forced_repaint_clears_line_suffixes_and_unused_live_rows` | 污染短行逻辑尾部以外的单元格和下方空白行，强制重绘后均清除；随后追加仍从真实尾部开始。 |

历史上 PR 初始 RED 提交 `39d5494` 为 **9 通过、2 失败**，最初修复后为 **15 通过**。
本次补齐后的 `main_screen_scroll_up` 为 **29 通过、0 失败、0 ignored**（其中 21 项在
`acceptance` 模块），默认 workspace 测试会运行。除上述明确改变的 saved-row 快照契约，
保留既有光标、命令行、草稿、样式、选择、原子发布和完整输出断言，没有新增 ignore。

新增场景包括：真实 30 项 todo/plan 的状态与折叠展开；晚到提示和清理；页前缀淘汰；
清空后继续输出；同尺寸 resize；旧行重排和 401 行追加同帧；强制/resize 重绘合并
100 行追加；编辑早期行并大幅缩短；大型面板移除；两个紧凑面板、模型输出和输入区同时可见。
逐帧检查保存前缀（包括 padding）、浏览锚点、`ED 3` 计数、输出各一次及当前可写区域内容。
另有单元测试覆盖真实窗口尺寸、字体像素、零尺寸、saved/live 图片删除边界、面板模式缓存和
当前任务优先级。CLI 验证更新提示的模式隔离、单行/窄屏、安全文本、命令、翻页、结算和 reset。

RED/GREEN 与 mutation 证据：在不改变新回归测试的情况下，恢复 `9bf9008` renderer，
`acceptance` 为 **11 通过、10 失败**；生产修复后全部通过。将重绘退化为“只写最后一屏”，
新增输出测试失败（行被丢弃）；禁用 saved 块锚点，编辑加缩短测试失败（旧消息重复）。
工具/Bash 单元测试仍验证 regular 时钟独立性、fullscreen 活跃计时和最终耗时；渲染上下文
测试仍覆盖嵌套、线程隔离和 unwind 恢复。

虚拟终端只检查协议与内容契约：不能验证 iTerm2 的 GPU 绘制、同步输出超时、触控板滚动动画或完整 Unicode 字宽兼容性。模型中的浏览锚点是验收模型，不代表应用可从真实 iTerm2 查询该锚点。

## iTerm2 人工验收（修复后仍必需）

1. 使用已知待测 binary，记录版本/commit、iTerm2 版本、是否经过 tmux，以及窗口行列数。在隔离会话中显式使用 `--tui-mode regular`；不读取真实用户会话或调用付费 provider 作为默认测试。
2. 准备远长于一屏的历史，以及仍在运行的长工具结果，使工具标题/状态行进入 scrollback。记录一条早期消息和一条完整命令作为唯一标记。
3. 向上滚动并停留在早期标记；后台继续刷新输入区工作计时和追加工具输出。regular 的工具面板不再显示活跃计时，完成后应显示总耗时。至少观察多个刷新周期，不能只比较最后一帧。
4. 验收：不闪空屏、不自动跳回底部；原生历史中的标记不移位、不重复、不缺行。返回底部后，新增输出完整，工具命令、输入草稿与 footer 不被覆盖。
5. 覆盖 todo/plan 完成、折叠/展开、晚到更新提示、多行草稿和较高 footer。当前摘要/更新命令与输入区应可见；普通输出不得重复。显式展开是用户请求查看的详细快照，不要求追改已经保存的旧状态。
6. 单独覆盖窗口缩放、字体变化和缩小后还原，并在同时有新输出时检查每条新增行没有丢失。再用 `--tui-mode fullscreen` 做对照，检查完整当前正文与模式缓存不串用；fullscreen 通过不能代替 regular 验收。

缺少真实 macOS / iTerm2 环境时，人工项记为 **未执行**，不能用虚拟终端 PASS 代替。
