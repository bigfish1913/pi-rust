# rpi 官网

这是一个零依赖的静态官网。首页数据来自 `data/site.json`，Packages 目录数据来自 `data/packages.json`，Documentation 数据来自 `data/docs.json`，Blog 数据来自 `data/blog.json`；页面逻辑分别在 `app.js`、`packages.js`、`docs.js` 和 `blog.js`，共享 `styles.css`。`i18n.js` 和 `data/locales.json` 提供全站中文/英文切换，语言选择会保存在浏览器中。

在仓库根目录启动：

```powershell
python -m http.server 4173 --directory website
```

然后打开 <http://localhost:4173>、<http://localhost:4173/docs.html> 或 <http://localhost:4173/packages.html>。点击右上角 `EN` / `中` 可切换语言；也可以通过 `?lang=en` 或 `?lang=zh` 预设语言。文档页支持“使用文档 / SDK 文档”切换和章节搜索。

生产发布使用仓库根目录的 Taskfile：

```powershell
task rpi-deploy
```

全站六个页面均加载 `https://stat.laofu.online/script.js`，通过
`data-performance="true"` 开启真实用户性能采集，并在 tracker 之后加载
同站点 ID 的 `recorder.js`。回放是否启用、采样率、输入遮罩及最长录制时间
由 Umami 网站设置中的 Replays & Heatmaps 配置控制；添加 recorder 脚本后
仍须在后台开启 Replays。部署后检查两个脚本、公开 recorder 配置接口，以及
浏览器中的 `/api/send`（`type: "performance"`）与 `/api/record` 请求。

扩展目录的发布版本和近 90 天下载量来自 crates.io API；功能说明和当前源码
路径来自 `pi-rust/rpi-package` 的 `main` 分支。`data/packages-meta.json` 记录
核验时间、源码提交和目录数量。源码仓库中尚未发布到 crates.io 的扩展不提供
`rpi install <name>` 安装卡片。扩展页默认展示最近发布的扩展，可通过类型筛选
查看核心 crate。

## 双域名与 CDN

当前两个域名提供同一份静态文件：

- `https://rpi.laofu.online`
- `https://rpi.bigfisharc.cn`（注意不是 `bigsifharc.cn`）

`deploy/rpi.caddy` 是旧域名的配置源。服务器通过 `gen_bigfisharc.py`
从 `sites/*.caddy` 生成 `sites-bigfisharc/mirror.caddy`；不要直接编辑镜像，
也不要再添加一份同名站点块。`task rpi-caddy-upload` 会生成镜像，严格校验
完整 Caddy 配置，再热重载，任一步失败均停止。现有证书策略保持不变。
首页 canonical / sitemap 暂保留旧域名；CDN 接入不等于 SEO 主域名迁移。

### 缓存策略

| 文件 | 浏览器缓存 | CDN / 共享缓存 |
| --- | --- | --- |
| `/`、HTML、JSON、Markdown、XML、TXT | 每次重新验证 | 60 秒 |
| JS、CSS | 300 秒 | 300 秒 |
| SVG、PNG、JPG、JPEG、WebP、ICO | 1 小时 | 1 天 |
| 内部路径、错误响应、文档跳转、其他文件类型 | `no-store` | 不缓存 |

`max-age` 控制浏览器，`s-maxage` 控制 CDN。JS/CSS 的 `?v=` 仍由人工维护，
所以暂不使用 `immutable`。发布时应更新修改过的资源版本及其引用；替换同名
图片后，即使 CDN 已刷新，浏览器仍可能保留旧图 1 小时，要求立即更新时请换 URL。

`.rpi`、`.deploy`、`.git` 路径在源站显式拒绝，`file_server hide` 提供额外保护；
打包排除与部署清理仍然保留。`robots.txt` 不是访问控制。

### CDN 控制台接入清单

仓库改动只让源站具备可控缓存，**不会自动开通 CDN 或修改 DNS**。
两个域名需分别配置加速与边缘 HTTPS 证书：

1. 大陆用户为主时评估国内 CDN / EdgeOne，并先确认备案和产品准入要求；
   海外用户为主时可考虑 Cloudflare，实际效果需按地区测试。
2. 保留现有 Caddy 作为源站。回源地址使用源站 IP 或不经 CDN 的专用源站域名，
   不要回源到已经指向 CDN 的加速域名，以免产生循环。
3. HTTPS 回源使用与源站证书匹配的 SNI，并携带正确的 Host：
   `rpi.laofu.online` 或 `rpi.bigfisharc.cn`；不要关闭回源证书校验。
4. 开启静态 HTML / JSON 边缘缓存并遵循源站 `Cache-Control` / `s-maxage`，
   不要用“全站统一缓存 30 天”之类规则覆盖；错误状态码缓存关闭，内部路径绕过缓存。
5. 保留查询参数，至少确保 JS/CSS/JSON 的 `v` 参与缓存键；初期保留全部查询参数
   最稳妥，勿无差别忽略 `lang`、`doc`、`mode` 等页面参数。
6. 配置完成后再切换 DNS；首次接入时清理两个域名可能已有的历史缓存。
   如果源站曾泄漏内部文件，还须清理对应 CDN 缓存，不能只删除源站文件。
7. 连续请求同一 URL，按供应商文档检查 `Age`、`Cache-Status`、`CF-Cache-Status`
   或 `X-Cache` 等命中信息；`Server: Caddy` 本身不能证明是否经过 CDN。

Google Fonts 和 `stat.laofu.online` 的脚本不在本站 CDN 的加速范围内。
国内首屏若仍慢，应另行评估字体本地化。

### 发布与缓存刷新

`task rpi-deploy` 会更新源站、同步双域名 Caddy 配置，执行可选刷新钩子，
最后从本机验证两个公网域名。刷新钩子通过环境变量指定一个仓库外的 Bash 脚本：

```bash
export RPI_CDN_PURGE_SCRIPT=/absolute/path/to/purge-rpi-cdn.sh
task rpi-deploy
# 也可以单独刷新、验证
task rpi-cdn-purge
task rpi-verify
```

PowerShell 可使用 `$env:RPI_CDN_PURGE_SCRIPT = 'C:/private/purge-rpi-cdn.sh'`。
脚本接收两个参数：`rpi.laofu.online rpi.bigfisharc.cn`；负责调用供应商刷新 API，
刷新两个域名的变更 URL（或目录），等待任务完成，任何 API / 等待失败须非零退出。
访问令牌仅放环境变量或本机安全存储，不写进仓库，也不打印到日志。

未配置钩子时会明确提示跳过刷新，并依赖 TTL 收敛：内容最多约 60 秒，
JS/CSS 约 300 秒，图片约 1 天。已有缓存头发生变更时，也需要刷新旧缓存。
`task rpi-verify` 检查 HTTP 内容、安全路径和缓存头，**不代表已验证 CDN 命中或
内容版本与本次发布完全一致**；更新后还应检查具体变更内容和多地区访问效果。

### 回归验证

```bash
python -m unittest discover -s scripts -p test_website.py -v
# 设置 CADDY_BIN 后额外启动隔离的本地 HTTP 站点，测试真实 Caddy 规则
CADDY_BIN=/path/to/caddy python -m unittest discover -s scripts -p test_website.py -v
# 接入或部署后验证双域名
python scripts/verify-website.py https://rpi.laofu.online https://rpi.bigfisharc.cn
```

集成测试使用临时目录和随机本地端口，不申请证书、不修改生产配置。
公网验证不禁用 TLS 校验，并检查不存在的静态文件同样返回 `404 + no-store`。
