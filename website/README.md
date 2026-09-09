# rpi 官网

这是一个零依赖的静态官网原型。首页数据来自 `data/site.json`，Packages 目录数据来自 `data/packages.json`，Documentation 数据来自 `data/docs.json`；页面逻辑分别在 `app.js`、`packages.js` 和 `docs.js`，共享 `styles.css`。

在仓库根目录启动：

```powershell
python -m http.server 4173 --directory website
```

然后打开 <http://localhost:4173>、<http://localhost:4173/docs.html> 或 <http://localhost:4173/packages.html>。文档页支持“使用文档 / SDK 文档”切换和章节搜索。后续把页面里的 `fetch(...)` 替换为 API 请求即可接入真实后台。
