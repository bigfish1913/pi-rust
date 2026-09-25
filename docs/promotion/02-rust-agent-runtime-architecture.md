# 把 coding agent 做成库而不是 CLI：一个 Rust runtime 的分层实践

> 投放渠道：掘金（主投）/ OSCHINA / 知乎（改写成问答）/ RustCC
> 角度：技术实践为主，项目介绍放最后，避免被判定为纯广告

## 为什么值得重做一遍

大多数 coding agent 是一个 CLI：装上、配好 key、在终端里用。这没问题，但如果想把 agent
放进自己的进程——编辑器插件、CI 任务、一个内部工具——就只能去调用它的 CLI，然后解析 stdout。
这条路能走，但很脆：输出格式是给人看的、错误处理靠字符串匹配、也没法订阅中间事件。

所以 rpi 的出发点是**先做库，CLI 只是第一个使用者**。这个决定一路推导出了后面所有的设计。

## 一、分层：让依赖方向不可逆

九个 crate，单向依赖：

```
rpi-telemetry → rpi-ai → rpi-agent → rpi-tools → rpi-harness → rpi-cli
                  ↑                                        ↑
            rpi-tui                          rpi-plugin-sdk → rpi-extensions
```

每一层只解决一件事：

| crate | 职责 | 关键约束 |
| --- | --- | --- |
| `rpi-ai` | provider 无关的消息/流式类型 + 各 provider 实现 | 默认构建**不含 HTTP 依赖** |
| `rpi-agent` | agent loop、`AgentTool`、事件、hooks、队列、取消 | 不认识任何 provider 的线格式 |
| `rpi-tools` | `read`/`write`/`edit`/`bash` + `ExecutionEnv` 抽象 | 工具只依赖 `ExecutionEnv`，不依赖真实文件系统 |
| `rpi-harness` | 会话树、JSONL 持久化、压缩、崩溃恢复 | 唯一负责"状态活过进程"的层 |
| `rpi-cli` | TUI、配置、插件加载 | 只做编排，不含领域逻辑 |
| `rpi-plugin-sdk` | 稳定 `#[repr(C)]` ABI | **零依赖叶子**，插件不该拖进整个 runtime |

两条约束是硬性的，代码评审会直接拒：

1. **不能有反向依赖。** `rpi-agent` 依赖 `rpi-cli` 这类改动一出现就说明分层错了。
2. **插件 SDK 必须是零依赖叶子。** 如果它依赖 runtime，插件就会链接宿主，`cdylib` 加载立刻出问题。

第二条尤其容易被忽略：插件是宿主**加载**进来的动态库，宿主链接插件，反过来不行。

## 二、把 LLM 边界收成一个 trait

agent loop 里唯一接触模型的地方是 `StreamFn`——一个**同步**返回事件流消费端的函数：

```rust
use rpi_ai::model::Model;
use rpi_ai::provider::SimpleStreamOptions;
use rpi_ai::types::Context;
use rpi_ai::AssistantMessageEventStream;

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn models(&self) -> &[Model];
    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream;
}
```

三个决定值得说明：

**返回流而不是 `Result<Vec<Event>>`。** provider 的失败（网络断开、帧被截断、请求被取消）
统一表达成流里的 `Error` 事件，而不是 `Err`。这样 harness 和 loop 只有一条代码路径处理
"模型这一轮出问题了"，不用区分三种失败来源。

**`stream_simple` 是 async，但 `StreamFn` 是同步的。** 因为 `StreamFn` 必须在 loop 里同步
返回（loop 需要立刻拿到流继续推进），而 producer 已经在返回前就 spawn 好了。这个
sync/async 的缝是有意留的：它让 loop 不需要是 async，也让它更容易测试。

**边界收在一个 trait，直接换来可测试性。** provider、测试替身、录制的回放三者可互换：

```rust
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_ai::Provider;

let provider = FauxProvider::new(
    FauxScript::new()
        .with_tool_call("write", serde_json::json!({ "path": "out.txt", "content": "hi" }))
        .with_text("Done."),
);
```

一个 `faux` provider 加一次工具调用，就能离线跑通"工具调用 → 结果回灌 → 下一轮"的完整循环。
整套测试套件不联网，`rpi-ai` 的 HTTP provider 在 feature flag 后面——**默认构建里没有网络栈，
所以测试不可能偷偷依赖网络**。

## 三、工具的输入有两个来源，都要处理

工具参数有两个来源：类型派生的 schema，和模型实际吐出来的 JSON。两者经常不一致。

- schema 用 `schemars` 从 Rust 类型派生，而不是手写 JSON Schema——手写的 schema 和实现会漂移。
- 但模型会写出"差不多对"的 JSON（尾随逗号、单引号、布尔值写成字符串）。
  所以校验前有一层 coercion，把常见的略有偏差修回来，而不是直接拒绝。

工具执行侧的关键抽象是 `ExecutionEnv`：

| 后端 | 用途 |
| --- | --- |
| `OsExecutionEnv` | 真实文件系统 + 真实 shell |
| `InMemoryExecutionEnv` | 假文件系统 + 脚本化 shell 响应 |

同一个 `read` 工具，跑在真机上和跑在内存里，是同一份代码。所以测试断言的是**工具行为**，
而不是"我机器上那个目录里碰巧有什么"，并且可以和其他测试并行跑。

这也解决了一个更隐蔽的问题：**测试之间的文件系统竞争**。多个测试同时写同一个相对路径，
失败是偶发且难查的。内存后端把这个类别整个消掉。

## 四、崩溃恢复：为什么必须按"帧"而不是按"轮"持久化

这是我最想讲的部分，因为它是一个反直觉的设计选择。

agent 跑一轮可能要几分钟。如果只在回合结束时持久化，那么在第 55 秒崩掉，用户拿到的是
"什么都没有"——他输入的 prompt 都丢了。

但"每一轮都持久化"也不对：一轮里 assistant 消息可能已经流式吐了一半，工具可能正在执行。
如果重启时把这些当成"已发生的历史"，就会**重复执行工具**（对一个会写文件、发请求的工具
是灾难）。

所以 rpi 记的是**帧级进度**（`LaneRecord::AssistantFrame`），并且严格区分两类东西：

- **进度不是历史。** 流式帧写入 record stream，但**永远不进**模型看到的那个分支。
  重启时把已提交的前缀重放成一个"被打断的消息"。
- **提交时机是"落定"（commit-on-settle）。** 带工具调用的 assistant 消息在落定瞬间提交，
  同时记一个 `step_attempt`；工具还没跑，它的 `tool_started` 帧就能先写下去。
- **一个 assistant 消息的 tool call 必须有对应结果。** 没有任何 provider 会接受"有调用没结果"
  的对话。所以重启时，每个未解决的调用要么被重放（**且仅当**记录里的策略和当前工具声明
  都说这个调用是安全的），要么被记成"结果未知"。
- **重试意图要单独记。** 在退避等待时崩掉，应该**重试**，而不是把这次失败的尝试当作历史
  存下来。这需要一条独立记录（`retry_pending`），因为流式帧里没有终止 stop reason。
- **续跑预算。** 一个反复崩溃的 run 不应该无限重启同一份工作，所以每个续跑的入口都受预算约束。

还有一个边界值得强调：**恢复不会静默重跑**。恢复只把会话修回一个合法状态，然后
`has_pending_resume()` 告诉调用方"还有未完成的工作"——**由调用方决定什么时候继续**，
因为"跑起来"必须发生在能渲染输出的地方，而 harness 不知道那在哪。

## 五、插件：为什么手写 C ABI，而不是"都是 Rust 所以直接传类型"

两边都是 Rust，看起来可以直接传 `Vec<Message>`。但插件和宿主是**分开编译**的，可能用不同
编译器版本、不同版本的 SDK。所以任何非 `#[repr(C)]`、或者有 `Drop` 的类型都不能跨界。

SDK 里写死了五条规则，违反任意一条都是 UB：

1. **跨界的每个类型都是 `#[repr(C)]`**，union 里的 enum 显式 `#[repr(u32)]` 钉住判别宽度。
2. **没有 `Drop` 类型跨界。** 不能有 `Vec`/`String`/`serde_json::Value`/它们的 `Option`。
   字符串以 `StbString`（指针+长度）跨界，并附带一个 producer 导出的 `free_string`——
   每块内存**恰好被接收方释放一次**。
3. **结构化数据以 JSON 字符串跨界。** 而且 `serde_json` 必须在宿主和插件两侧配置一致
   （`preserve_order` + `arbitrary_precision`），否则超出 `u64`/`i64` 的整数会丢精度、
   对象 key 顺序会变。
4. **union 只放 `Copy` 载荷**，所以读变体是 `unsafe`，由 tag 判别。
5. **panic 不能跨越边界。** 每一边都用 `catch_unwind` 把 panic 转成状态码——
   从 `extern "C"` 里 unwinding 出去会直接 abort 进程。

第 5 条不是理论问题：插件 panic 曾经会拖垮宿主进程，现在被隔离成一次"这个插件出错了"。

宿主侧还有个异步桥的问题：插件的工具执行是**四个导出函数**（`execute`→handle、`poll`、
`cancel`、`destroy`）驱动的阻塞循环。宿主的适配器不自己持有 runtime，而是取当前的
`tokio::runtime::Handle`，把 `poll` 循环丢进 `spawn_blocking`，用 mpsc 回传部分结果。
这样"跨 ABI 的同步插件"就变成了宿主里的普通 async 工具。

## 六、我踩过的坑和还没做的

写下来是因为这些比"架构多优雅"更有参考价值：

- **会话压缩和两段式 LLM 调用的不变量**很脆弱：压缩时不能把一轮切在中间，否则构造出的
  请求是非法的。这条不变量现在有独立测试守住。
- **`main` 曾经编译不过而我本地是绿的**——因为工作区里有一份未提交的修复。
  教训是"本地测试过了"不等于"仓库是绿的"，CI 必须真的跑。
- **基准测试自己会骗人**：我在计时窗口里每 10 ms 起一个 `tasklist` 采样内存，
  把两个工具都拖慢了约 100 ms，正好抹平了真实差距。测量代码也需要被审查。

还没做的：预编译二进制的 Homebrew/Scoop 分发、Gemini/Bedrock 适配器、
以及把启动初始化从 86 ms 砍下去。缺口清单在
`docs/native-pi-missing-features.md` 里逐条记着，没有藏。

## 项目

rpi 是 Rust 原生、library-first 的 coding-agent runtime，MIT 许可，九个 crate 同版本发布。

- GitHub：https://github.com/bigfish1913/pi-rust
- 官网 / 文档：https://rpi.laofu.online/
- 架构文档：https://github.com/bigfish1913/pi-rust/blob/main/docs/architecture.md
- 自己搭 agent 的项目结构建议：
  https://github.com/bigfish1913/pi-rust/blob/main/docs/agent-project.md
- 与原生 TypeScript 版的实测对比：
  https://github.com/bigfish1913/pi-rust/blob/main/docs/performance-vs-pi.md

```bash
curl -fsSL https://raw.githubusercontent.com/bigfish1913/pi-rust/main/scripts/install.sh | sh
# 或者
cargo install rpi-cli
```

想直接看代码：`cargo run -p minimal` 跑一个不联网、不需要 API key 的 agent。
