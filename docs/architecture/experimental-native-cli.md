# 实验原生 session：CLI 显式 print opt-in

## 范围

默认 backend 仍为 JSONL；构建 feature 本身不会选择 native backend。CLI 增加默认关闭 feature
`experimental-native-store`，转发 harness 的同名 feature。显式启用只支持**新建 session 的一次
text/json print 调用**，不接 Web/TUI/RPC、不恢复已有 artifact、不迁移真实 session。

```bash
# 使用专用隔离目录；不是从已有 session 转换/恢复。
cargo run -p rpi-cli --features experimental-native-store --locked --offline -- \
  --experimental-native-store ./native-fixture --print "synthetic prompt"
# JSON 输出仍须显式 --print
rpi --experimental-native-store ./native-fixture --print --mode json "synthetic prompt"
```

第二条仅适用于启用了实验 feature 的 binary。feature-off binary 也识别保留参数并明确拒绝，
不会把它当 unknown extension flag 后悄悄创建旧 backend。两种 `--flag value` / `--flag=value`
写法均支持；缺值、空白值、重复值拒绝。每次启动分配新 ID，不扫描/选择目录内现有 session。
目录按 launch cwd 冻结；native repo/create 失败直接报错，不回退 JSONL 或 memory。

## 必须拒绝的组合

- 未显式 `--print/-p`，包括仅 `--mode json` 或依赖管道推断 print。
- continue/resume/session/session-id/fork/no-session/session-dir。
- RPC/server/Web/connect、export/list-models（server=false 也算存在）。
- auth/events/update/install/uninstall/package/dev/dev-local 等提前分发的命令。
- `RPI_FORCE_TUI=1` 或声明 ownsStdio 的扩展。

早期命令检查在远程连接/子命令分发前；普通参数校验在 config migration/export/list-models/
resume/旧 session peek 前；prepare/build 对库调用再次校验，且拒绝准备后改变 backend/root。
stdio claim 在创建 artifact 前检查，最终 run mode 仍有 Print/Json 兜底。
早期扫描尊重 remote dispatcher 对 token/address 的值消费，不将形似保留 flag 的连接凭据
误当本地 opt-in。

## 单会话宿主能力

native storage -> Session -> AgentHarness 使用现有 prompt/tool/stream 管线，不先建 JSONL 再替换。
name/custom entries/工具结果写当前 native store。runtime new/switch/fork/navigate 在任何
目录/锁/打开/写入前返回策略拒绝；初始 action host、reload 复用和 fallback 重建均保留限制。
ReloadContext 单独保存 startup capability，不从可变 args/settings/env 推断权限；reload 新的
stdio-owning 扩展也拒绝。

这不是扩展或工具 sandbox：本地扩展、bash/write 仍可执行其正常授权的主机操作。不把目录/
环境隐式传给子 agent 当作 native opt-in，也不宣称防御恶意本地代码任意文件 I/O。

## 错误与格式边界

- `.rpinative` 使用 [v3 实验格式](experimental-native-store.md)，拒绝 v1/v2，不升级。
- 正文保持现有 JSONL canonical wire shape。实验 codec 先用既有 schema decoder 验证，再为
  支持的 opaque JSON 字段恢复显式 null 的存在性（tool-result details、custom data、
  compaction/branch-summary details、assistant-frame frame），以保留 Some(null) 的 typed ACK。
  缺字段仍为 None；生产 JSONL parser 不改。其他不可精确 roundtrip 的值仍 fail closed。
- 旧 JSONL loader 误开 native artifact 必须报错；旧 SQLite writer 无条件在 WAL/schema
  初始化前拒绝已保留的 v1/v2/v3 application_id，feature-off 同样生效。不承诺所有崩溃 WAL
  状态的物理零 I/O；合成干净关闭 artifact 验证字节及 sidecar 不变。
- native text/json 遇到已排队 write failure，即使后续 prompt 成功也退出非零。JSON 输出
  `error.kind=session_write_failed`，最终 result.outcome=failed/persistenceFailed=true。
  harness 本身返回 Err 时也有失败 error/result；不把一般运行错误冒充 write-failure 证明。
- `--name` 在 native 模式持久化失败为启动错误，不仅 verbose 警告。

## 验证与非承诺

所有新 fixture 使用临时 HOME/config/cwd/session roots，provider 为本地 loopback mock 或 faux，
不读真实配置/密钥/session。真实 binary 验证 text/json 新建与 reopen、工具结果、name、
失败注入、旧根递归文件列表/字节不变及未 opt-in 时默认仍建 JSONL。
库级验证 stdio claim、prepare/build 绕过、root 冻结、三种 action host 的拒绝及 custom null。
受控 SQLite trigger 让真实 harness write-failure 队列非空，随后恢复 writer 并跑两个成功 prompt，
断言 text/json 仍返回失败；子测试进程验证 JSON error/result 顺序和字段。

本入口**不是生产默认切换或完整 durable 交付**：稳定格式 registry、独立进程 crash/锁测试、
跨进程 generation fencing、安全 delete、request-ID 不确定提交封装、全面 context/branch 优化、
真实数据隔离测量和迁移仍需另行实施/授权。v4 JSONL 初次扫描未变。v3 结构性 metadata/body
读取证据不等于宿主端到端恢复 benchmark、物理 I/O 恒定、fsync/断电原子性或外部 exactly-once。
