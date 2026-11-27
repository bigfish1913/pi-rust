# Host 收口：预构建扩展 codesign 夹具修复（#62）

四项既有失败已修复，没有跳过测试或修改生产签名逻辑。

- macOS 测试对 arm64 / x86_64 分别用系统 clang 链接真实 dylib，缓存各架构字节，使用生产的固定 `com.rpi.native` 标识签名后打包。
- 新测试在不同 staging 文件名下重新签名，执行 `codesign --verify --strict`，并要求重新签名前后字节完全一致。因此原有六分发目标的文件内容、archive/manifest/library provenance、升级与幂等断言均保留。
- 非 macOS 的原有 PE/ELF/Mach-O 头只用于格式结构测试；本次未声称其它平台原生安装或交叉编译验收。
- 仅修改 `crates/rpi-cli/src/prebuilt_extensions.rs` 的测试区；生产 `resign_staged_native` 未修改。此修复不表示 #46、默认 Host、daemon 或 OS 资源清理已完成。

## 验收

| 门禁 | 结果 |
|---|---|
| 全部 prebuilt 测试 | **26 passed / 0 failed / 0 ignored** |
| 未过滤 rpi-cli 库测试 | **1538 passed / 0 failed / 3 ignored** |
| rpi-cli `--lib --bins --no-deps -D warnings` Clippy | **通过** |
| 修改文件 fmt / 两树 diff check | **通过** |

使用既有 CARGO_HOME、本 worktree target、TMPDIR=/private/tmp，Cargo `--locked --offline -j 2`。全库使用 `--test-threads=2`，无过滤或新忽略。

旧 #56/#61 报告及当时四项失败记录保持原样。任务前完整工作树源码/补丁/索引哈希已存于 `/tmp/rpi-host-integration-checks/host-all-20261124-before`；报告生成时唯一源码增量为上述测试文件，全部 extension 源码未变。未提交、发布、部署或重跑内存 benchmark。

[机器证据](host-codesign-fixtures-validation.json)
