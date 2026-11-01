# 第四期：显式构造的实验原生 SessionStorage/Repo

## 状态与启用边界

工作基于第三期提交 `79606147`。本阶段引入默认关闭的 harness feature
`experimental-native-store`，导出 `session::experimental_native::{ExperimentalNativeRepo,
ExperimentalNativeStorage}`。调用者必须显式构造专用目录并调用 repo trait；**没有 CLI/Web
路由、默认 backend 替换、旧 session discovery 或迁移**。仅合成 fixture 验收，非生产交付。

```bash
cargo test -p rpi-harness --features experimental-native-store --test experimental_native --locked --offline -- --test-threads=1
cargo test -p rpi-harness --features experimental-native-store --lib --locked --offline -- --test-threads=1
cargo test -p rpi-harness --test native_recovery_store --locked --offline -- --test-threads=1
```

## 共享事实引擎与格式

第三期事实引擎移动到 `src/session/experimental_native/engine.rs`；原 integration test support
通过 path wrapper 使用同一实现，避免复制两套 reducer。**因此当前共享源码不再创建第三期
v1 格式**：采用独立 `rpi-native-fact-experimental-v2`、application_id `0x5250494f`、user_version=1。
第三期文档的 v1 标识及数值是该提交的历史证据，不是新格式兼容承诺。无自动升级。

专用 artifact 后缀 `.rpinative`，safe ASCII ID 映射路径；拒绝明显 symlink/non-regular 文件。
这不是针对恶意目录替换的 sandbox，也不防同 identity 的整库旧备份回滚。
checkpoint、固定预算 tail 与 lazy body 校验继承事实引擎；auxiliary 表中的 metadata/progress
各自带 identity-bound 摘要及 4 MiB 实验预算。摘要不是安全签名。

## 事务、异步与取消

- SQLite 工作在 `spawn_blocking`，history 写使用 BEGIN IMMEDIATE 后的 fresh projection。
- 批写、record+entry、record+entry+record、取消批次均单事务，失败不消耗 sequence。
- expected history revision 在事务内比较，不是先读后写。
- admitted blocking owner 持有 owned gate；调用者取消不取消已启动 mutation。
  `drain` 等待本 handle 组 gate，**不是跨进程 drain**。
- Repo create/open/list 的 owner 也持有生命周期 gate。create/fork 在同事务初始化 schema、
  metadata、progress 与初始 facts，失败可留下不可打开的 artifact；不会自动删除或覆盖残留。
- 同进程、同路径 repo open 共用 inner，拒绝活动 handle 的不同 generation。
  全局 registry 只弱引用，不持有历史正文；不是跨进程 fencing。
- fork 显式全历史 materialize，使用真实 SessionState 生成 fork mutations；不复制 progress。
- delete 始终 fail closed：活动 handles 明确拒绝，其他情形也因尚无跨进程 lifecycle claim
  而报错。没有安全删除实现，不能宣称完整 Repo 生命周期已完成。

## 查询及 progress

支持 lanes、get_entry、entry/branch/record 查询、record_page、open operations、log、name/label、stats。
metadata 保存 query selection keys（type/run/tool/outbox 等），先选择 keys 再读取所选正文。
**一般历史查询当前扫描并收集全部 metadata**，不是完整索引化查询或有界峰值内存保证；branch
扫描随路径增长。pending 查询使用 active projection，checkpoint 恢复不读取 retired bodies。
context 使用 trait fallback；message_summaries/message_window 保持 unsupported，未实现 UI 首屏优化。

progress 与 history 使用独立 sequence，存同一数据库但不进入 immutable history；CommitStream
只允许历史 record，不可写入 transient progress。cleanup 仅按同 lane/run 的 durable finish
退休活动 payload。当前 progress 整体读写且受预算约束，不声称任意大活动状态可扩展。

## 验证范围与未完成工作

接口 fixture 对照 Memory/Session facade，覆盖条件并发写、跨 repo handles、重开、批次回滚、
取消批次 pending/run 校验、progress 序号隔离及 terminal proof、显式 fork 与 legacy 文件不变。
增加 branch bounds/order/cursor/limit 矩阵、context/facts、tool/outbox、全部 record 变种过滤与
record_page 边界、公开接口 typed errors 对照；一般查询扫描 metadata 的限制仍然存在。
私有 gate/channel 测试通过直接 poll 证明 admission 前取消不写、admission 后 drain 必须等待；
Repo caller 取消后 list 必须等待初始化结束。另测试独立 SQLite connections 的事务序号、
初始化失败整体回滚、catalog 误命名/外来文件拒绝、open limit 不读未选旧正文、entry lookup
不误读同 ID record 的损坏正文，以及损坏 terminal metadata 不能删除 progress。
共享事实引擎的原 14 项回归继续覆盖 checkpoint/reference/body/ACK 等协议边界。
可逆负向测试移除取消批次的匹配校验后，新接口回归确实失败；逐字节恢复后重跑绿色。

生产仍缺：完整错误分类及等价性矩阵、独立进程 crash/锁/取消测试、外部 generation fencing、
稳定格式 registry 与旧 writer 拒绝、request-ID 不确定提交对账封装、安全 delete、summary/window
与完整索引、实际宿主切换、隔离旧数据测量及 idle/独占/备份/回滚迁移。当前 Storage 返回的
实验协议错误部分统一映射 StorageError；不能当作生产错误码兼容。

**生产 v4 JSONL 初次扫描未改变；总体目标尚未完成。**
