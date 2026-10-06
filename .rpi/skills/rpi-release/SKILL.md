---
name: rpi-release
description: 发布 rpi Rust workspace 的版本，包含预检、版本递增、Git tag、GitHub Release、crates.io、安装渠道、官网部署，以及发布后合并 main 和创建下一个版本分支。用于用户请求发布新版本、打 tag、发布 crates 或整理 release 流程时。
---

# rpi 发布流程

## 目标

本项目的标准发布入口是根目录的 `Taskfile.yml`：

```bash
task publish
```

它会按依赖顺序完成版本更新、测试/校验、提交、tag、推送、GitHub Release 资产、安装渠道、crates.io 和官网部署。

## 发布前检查

1. 确认当前分支、远端和工作区：

   ```bash
   git status --short --branch
   git remote -v
   git log -5 --oneline --decorate
   ```

2. 不要擅自丢弃未提交修改。若用户要求“全部提交并发布”，先检查 diff，再运行测试，最后统一提交。
3. 确认版本和 tag：

   ```bash
   grep '^version' Cargo.toml | head -1
   git tag --sort=-version:refname | head
   ```

4. 预览发布计划，不产生修改：

   ```bash
   task publish -- --dry-run
   ```

## 常用发布命令

自动递增 patch 版本：

```bash
task publish
```

指定版本：

```bash
task publish RELEASE_VERSION=0.3.16
```

递增 minor/major：

```bash
task publish RELEASE_BUMP=minor
task publish RELEASE_BUMP=major
```

跳过交互确认（只有用户明确授权时使用）：

```bash
task publish RELEASE_VERSION=0.3.16 -- --yes
```

`--` 后的参数会转发给 `scripts/release.sh`。可用阶段参数：

- `--dry-run`：只打印计划
- `--yes`：跳过 crates/site 确认
- `--only a,b`：只执行指定阶段
- `--from phase`：从指定阶段恢复
- `--skip a,b`：跳过阶段
- `--date YYYY-MM-DD`：指定 changelog/release 日期

## 标准阶段

发布脚本通常按以下顺序执行：

1. `preflight`：要求工作区干净、版本/tag 未占用
2. `bump`：更新 workspace 版本和内部依赖
3. `changelog`：切分 Unreleased changelog
4. `docs`：同步 embedded docs
5. `validate`：校验所有 crate 版本
6. `commit`：提交 release commit
7. `tag`：创建 `vX.Y.Z`
8. `push`：推送分支和 tag
9. `wait`：等待 GitHub Release 二进制资产及 checksum
10. `channels`：刷新 Homebrew/Scoop/winget/site manifest
11. `commit-channels`：提交渠道更新
12. `taps`：推送 Homebrew/Scoop 渠道
13. `winget`：创建 winget PR
14. `crates`：按依赖顺序发布 crates.io
15. `site`：部署官网

## 发布失败恢复

不要重复 bump 版本；使用当前已经准备好的版本从失败阶段继续。并非所有阶段都能直接重跑：`tag` 已存在时不可重建，winget 的文件 PUT 重跑需要现有文件的 SHA。

```bash
task publish RELEASE_BUMP=none -- --from push --yes
```

或：

```bash
task publish RELEASE_VERSION=0.3.16 -- --from channels --yes
task publish RELEASE_VERSION=0.3.16 -- --only crates,site --yes
```

## 本次发布经验（0.3.16）

- 长时间发布建议输出到 `.deploy/release-X.Y.Z-resume.log`，完成后检查退出码和各阶段日志；不要把命令超时当成测试通过。
- `winget-submit.sh` 可能因 fork 尚未包含 upstream master 的提交而创建分支失败，并被误报为“branch may already exist”。先用 `gh api repos/<owner>/winget-pkgs/git/refs/heads/rpi-X.Y.Z` 确认分支真的存在；更新 fork 后使用有效的 commit SHA 创建分支。必要时从 fork 的 master 创建，但必须检查 PR diff 仅包含预期 manifests。
- 已存在的 winget manifest 再次 PUT 时需要 SHA；已存在的 PR 不要重复创建。winget 成功后可从 `crates` 阶段继续。
- `commit-channels` 之后脚本没有再次推送主仓库分支。发布结束必须执行 `git push origin HEAD`，确保安装渠道提交也在远端。
- 发布 skill 中的版本号是示例；每次按当前 workspace、tag 和用户要求重新确认版本，不要机械复用。

## 发布后合并与创建下一分支

假设发布分支是 `feature-0.3.16`，发布成功后：

```bash
# 更新本地远端引用
git fetch origin

# 切换 main 并快进到远端最新状态
git switch main
git pull --ff-only origin main

# 合并发布分支
git merge --no-ff feature-0.3.16 -m "merge release 0.3.16"
git push origin main

# 从 main 创建下一个版本分支
git switch -c feature-0.3.17
git push -u origin feature-0.3.17
```

如果仓库约定使用 fast-forward 合并，先确认后再省略 `--no-ff`。若 main 有新的远端提交，先停止并处理冲突，不要强推。

## 权限和凭据

发布需要：

- GitHub `gh auth status` 可写仓库权限
- git push 权限
- crates.io 的 `cargo login` token
- 官网部署所需 SSH 权限
- Homebrew/Scoop/winget 渠道操作权限

检查：

```bash
gh auth status
cargo login --help
```

不要把 token、API key 或 SSH 私钥写入仓库。

## 交付检查

发布结束后确认：

```bash
git status --short --branch
git tag --list 'v0.3.16'
git ls-remote --tags origin v0.3.16
git ls-remote --heads origin main feature-0.3.17
cargo install rpi-cli --version 0.3.16
rpi --version
```

如果 winget 只是提交 PR，不要声称它已经进入 winget 主仓库；需要等待微软仓库审核合并。
