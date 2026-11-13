# macOS 代码签名与"宿主被静默 SIGKILL"

排查 macOS 上 rpi 起不来、新会话秒退、spawn 日志空白时先读这篇。

## 症状

宿主进程启动一两秒内消失，**没有任何可观测错误**：

- 终端可能只有 `Killed: 9`，退出码 **137**
- stderr 一个字节都没有，所以上层（`pi-dingo-dash` 等）看到的是
  「正在重启会话…（等待 Pi 回连）」永远转圈——它只能观察到"宿主没回连"
- 换会话、换目录、换模型、换 cwd 都无效；移走出问题的扩展就恢复正常

## 唯一可靠的证据来源：内核日志

`codesign -v` 和 `spctl` 对这种情况**都看不出问题**（下面「无效的排查手段」一节）。
只有 kernel log 里有真相：

```bash
log show --last 1h --predicate 'eventMessage CONTAINS "cs_mtime" OR eventMessage CONTAINS "CODE SIGNING"'
```

```
CODE SIGNING: process 20087[rpi]: rejecting invalid page at address 0x107434000
  from offset 0x0 in file ".../extensions/librpi_superagent.dylib"
  (cs_mtime:1794052202.55 != mtime:1794056571.77) (signed:1 validated:1 tainted:1 ...)
CODE SIGNING: cs_invalid_page(0x107434000): p=20087[rpi] final status 0x23000200, denying page sending SIGKILL
```

注意杀的是**宿主进程**，不是那个 dylib：内核在映射代码页失败时拒绝该页并把整个进程杀掉。

## 关键事实（这几条是实测结论，和常见说法不一样）

1. **`cs_mtime` 相等也会被杀。** 2026-11-07 实测复现记录里同时存在
   `cs_mtime:… == mtime:…` 和 `!=` 两种，都被 `cs_invalid_page` → SIGKILL。
   所以「mtime 对不上才死」是错的推论；真正的判据是**页内容与 CodeDirectory
   里的哈希不符**。这解释了一个反直觉现象：只 `touch` 文件（sha 不变）无论怎么改
   mtime 都不复现，而改 1 个字节稳定复现。

2. **`rename` 上线是安全的。** 写临时文件再 `std::fs::rename` 到目标路径（新 inode）
   不会触发问题。真正危险的是**同一个 inode 被就地 truncate+write 覆盖**，
   对已经映射过它的进程致命。

3. **纯字节复制通常是安全的**，前提是源文件本身签名有效。`std::fs::copy`／
   `io::copy` 复制出的副本如果签名与字节本来就自洽，加载没问题。
   危险的是把**签名与字节不自洽**的文件落盘或上线。

4. **重签会改变文件字节**（sha256 变、文件略大）。实测同一文件连续两次
   `codesign --force --sign -`，哈希稳定一致，所以它是可重复的，但它会改变
   `inventory`/`receipt` 里记录的摘要——这点在带所有权校验的代码里必须处理。

5. 这条链路**无法在 Linux/Windows CI 复现**，是 macOS 运行期文件系统状态问题。

## 无效的排查手段（别浪费时间）

| 手段 | 为什么没用 |
|---|---|
| `codesign -v` / `--verify --strict` | 对 mtime 漂移的文件照样报 `valid on disk` / `satisfies its Designated Requirement`（返回 0） |
| `spctl -a` | 语义完全不同，报 `rejected` 也不指向这个原因 |
| `cp -p` 保留 mtime | 只在纯 copy 时碰巧成立；`rename` 之后 mtime 由内核给，且它掩盖了"文件被改过"这个真实信号 |
| 在扩展加载器里 try 后重签 | 进程已经被内核杀了，没有"再"的机会 |
| 只在 CI 加测试而不修落盘代码 | 见上面第 5 条 |

## 确定性复现

翻转签名覆盖范围内 1 个字节即可，稳定触发（实测 8/8）：

```bash
cp plugin.dylib /tmp/x.dylib
python3 -c "
p='/tmp/x.dylib'
d=bytearray(open(p,'rb').read()); d[4096]^=0xFF
open(p,'wb').write(d)"
codesign -v /tmp/x.dylib          # → invalid signature (code or signature have been modified)
./load /tmp/x.dylib               # → Killed: 9, exit 137
```

`codesign --force --sign -` 之后立刻恢复。

## 落盘时应该怎么做

**在任何把原生产物落到最终路径之前重签暂存文件**，而不是上线之后再签，
也不是校验一下就算完。

```rust
/// macOS: 重签暂存文件，让内核愿意映射它。
/// 必须在上线（rename）之前做，否则并发加载的进程会看到未签好的中间态。
pub(crate) fn resign_staged_native(path: &Path) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let out = std::process::Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-"])
        .arg(path)
        .output()
        .map_err(|e| format!("could not run codesign on {}: {e}", path.display()))?;
    if !out.status.success() {
        return Err(format!(
            "re-signing {} failed (the kernel will refuse to load it): {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}
```

要点：

- 只对原生库（`std::env::consts::DLL_EXTENSION`）调用，文本资源不要动（省时间，也避免无谓改动）
- 已经装好、可能正被别的 rpi 进程映射着的文件，**不要就地重签**——那会反过来把它们弄成"签名失效"
- 带 `inventory`/`receipt` 摘要校验的路径：重签会改字节，所以**记录必须取重签之后的**，
  否则后续每次所有权校验都会报 "content changed"

rpi 里已经这样处理的三处：`install.rs::stage_native_artifact`、
`packages.rs::link`、`resource_management.rs::copy_snapshot`。

## 后续可做的加载期兜底（尚未实现）

进程已经被内核杀了就没法自救，所以「加载前检测」的**唯一可行形态是子进程探针**：

```
fork → 子进程 dlopen + 触碰入口符号的代码页 → 父进程 waitpid 看 WIFSIGNALED
```

实测可行：子进程拿到 `sig=9` 被 SIGKILL，父进程安然无恙，据此可以跳过该扩展
并给出可读诊断，而不是让宿主静默消失。

**不要**用 `mmap(PROT_READ|PROT_EXEC)` 探测——对未签名的普通库会大量误报
（`/usr/lib`、`/opt/homebrew/lib` 下几乎所有库都 EPERM），没有判别力。

## 参考

- 首次事故：2026-11-07，一次下午 39 次 SIGKILL，全部指向同一个
  `librpi_superagent.dylib`
- 修复 commit 见 `fix(native): resign staged extension artifacts on macOS`
