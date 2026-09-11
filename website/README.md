# rpi 官网

这是一个零依赖的静态官网。首页数据来自 `data/site.json`，Packages 目录数据来自 `data/packages.json`，Documentation 数据来自 `data/docs.json`；页面逻辑分别在 `app.js`、`packages.js` 和 `docs.js`，共享 `styles.css`。`i18n.js` 和 `data/locales.json` 提供全站中文/英文切换，语言选择会保存在浏览器中。

在仓库根目录启动：

```powershell
python -m http.server 4173 --directory website
```

然后打开 <http://localhost:4173>、<http://localhost:4173/docs.html> 或 <http://localhost:4173/packages.html>。点击右上角 `EN` / `中` 可切换语言；也可以通过 `?lang=en` 或 `?lang=zh` 预设语言。文档页支持“使用文档 / SDK 文档”切换和章节搜索。

生产发布使用仓库根目录的 Taskfile：

```powershell
task rpi-deploy
```
