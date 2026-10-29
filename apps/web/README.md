# komo Web 客户端（W0 原型）

浏览器里的 komo：会话列表、按轮组织的对话、右栏的事项 / 带上的记忆 / 上下文构成 / 后台任务。数据全部来自正在跑的 Gateway，没有样例数据。规格见 [`docs/web.md`](../../docs/web.md)。

纯静态文件，没有构建步骤，也没有依赖。

```bash
node apps/web/serve.mjs            # http://127.0.0.1:5178
node apps/web/serve.mjs --port 5200
KOMO_HOME=/path/to/data node apps/web/serve.mjs
```

`serve.mjs` 同源提供页面，把 `/v1/*` 与 `/healthz` 转给 Gateway；令牌每次请求现读 `<KOMO_HOME>/runtime/gateway.json`，只在服务端用，不发给浏览器。Gateway 没在跑时页面显示出错状态，可以重试。

| 文件 | 内容 |
|---|---|
| `index.html` | 外壳与图标 |
| `app.js` | 接口、事件流 fold、渲染与交互 |
| `styles.css` | 主题 token 与全部样式 |
| `serve.mjs` | 本地代理 |
| `scripts/shot.mjs` | 经 DevTools 协议按真实视口截图：`node apps/web/scripts/shot.mjs <url> <out.png> 390x844 [先执行的 JS] [等待毫秒]` |

这是原型：发消息、停止、新建、分叉都会真的作用到所连的 Gateway。
