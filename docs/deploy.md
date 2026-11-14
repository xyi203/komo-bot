# 部署说明：在远程虚拟机上跑 komo（单人、自己的服务器）

> 状态：说明书，2026-11-14 写。**只写说明，不替操作者配**；本文所有步骤都**没有实测**（写的时候没有登录过任何远程机器，也没有改过 Cloudflare / Tailscale / Pocket ID 的任何真实配置）。第一次照着做要边做边核对，核对出入请回来改本文。涉及的设计见 `docs/redesign.md` §2.9、`docs/clients.md` §5 / §9。

## 0. 拓扑

```text
浏览器 / 手机 PWA / 桌面端
        │ https://komo.example.com          （同一个域名）
        ▼
   Cloudflare ── 按路径分流 ─┬─ /v1/*  /auth/*  /healthz ──► cloudflared 隧道 ──► Gateway（虚拟机上，127.0.0.1:7777）
                            └─ 其余（/、/assets/…、/chats/…）──► 静态网页（Cloudflare Pages，或同一台机器上的静态目录）

   你的电脑 ──Tailscale──► 虚拟机（ssh、直连 Gateway 调试、命令行 komo login）
   Pocket ID（身份提供方）── Gateway 走标准 OIDC 接它
```

要点：

- **同域名、按路径分流，所以不做 CORS**。网页与接口永远同源，`komo_session` cookie（`HttpOnly; SameSite=Lax; Path=/`，公开地址是 https 时带 `Secure`）、`Origin` 核对、登录回跳都按「同一个域名」成立（读代码核对过，未实测）。
- Gateway 只认三种凭据：本机令牌（虚拟机上的命令行）、设备令牌（桌面端 / `komo login`）、登录 cookie；`/healthz`、`/auth/*` 不要登录。**公网这条路上登录是唯一的墙**（命令执行没有别的限制），身份提供方要可靠。
- 推送（Web Push）要求页面是 **https**；iOS 要先「添加到主屏幕」才收得到。

## 1. 虚拟机上跑 Gateway

### 1.1 装二进制

用发布包（`komo-linux-amd64.tar.gz` / `-arm64`，包里是 `komo` 与网页壳目录 `web/`）或 `scripts/install.sh`。装好后：

```bash
komo --version
komo init            # 写最小的 config.toml 与 0600 的 .env（不会覆盖已有的）
```

数据目录默认 `~/.komo`（或 `KOMO_HOME`）。**状态都在这里**：`durable.db*`、`aux.db*`、`sessions/`、`runtime/`（含 `vapid.json`）、`mcp/<name>/credentials.json`、`.env`。

### 1.2 config.toml 要点

```toml
[gateway]
listen = "127.0.0.1:7777"          # 只听回环，外面的人只能经隧道进来
trusted_hosts = ["komo.example.com"]   # public_url 的主机会自动放行，这里可留空；经 Tailscale 直连时再加 tailnet 主机名

[auth.oidc]                         # 见 §4
issuer = "https://id.example.com"
client_id = "<Pocket ID 的 Client ID>"
public_url = "https://komo.example.com"
allowed_subjects = ["<你的 sub>"]   # 或 allowed_emails = ["me@example.com"]

[web]
serve = true                        # 网页由 Gateway 托管（同机静态目录的做法，见 §3.B）；单独发布到 Pages 时改 false（§3.A）
# dir = "/opt/komo/web"             # 没写就读二进制旁的 web/

[push]
enabled = true                      # 默认开
# subject = "mailto:me@example.com" # 省略时用 [auth.oidc] public_url
```

`.env` 里放模型 API key 与 `KOMO_OIDC_CLIENT_SECRET`（Pocket ID 的客户端密钥；公开客户端可不放）。改完跑 `komo config check`，**`[web]`、`[gateway]`、`[auth.oidc]`、`[push]` 都是启动期键**，改了要 `komo gateway restart`。

### 1.3 systemd 示例（用户级单元，未实测）

`komo gateway` 在 Linux 上本身支持 `systemd --user`（见 `crates/komo/src/connect.rs`）；下面是手写单元，想完全自己管进程时用。放 `~/.config/systemd/user/komo.service`：

```ini
[Unit]
Description=komo gateway
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
Environment=KOMO_HOME=%h/.komo
ExecStart=%h/.local/bin/komo gateway --foreground
Restart=on-failure
RestartSec=3
# 退出时给 Gateway 时间收尾（durable.db 靠提交线，被杀也能恢复，但干净退出更省事）
TimeoutStopSec=30

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now komo
loginctl enable-linger "$USER"      # 没登录也保持运行；否则用户级服务随登出被停
journalctl --user -u komo -f
curl -s http://127.0.0.1:7777/healthz   # 应有 instance_id / version / protocol_version
```

用 `komo gateway --foreground` 时 `komo gateway stop|restart` 管的是同一个实例；两种托管方式别同时用。

### 1.4 cloudflared 隧道（未实测）

```bash
cloudflared tunnel login
cloudflared tunnel create komo
cloudflared tunnel route dns komo komo.example.com
```

`~/.cloudflared/config.yml`（**网页单独发布时只把 Gateway 的三类路径给隧道**，见 §3）：

```yaml
tunnel: <隧道 ID>
credentials-file: /home/<你>/.cloudflared/<隧道 ID>.json
ingress:
  - hostname: komo.example.com
    path: ^/(v1/|auth/|healthz$)
    service: http://127.0.0.1:7777
  - service: http_status:404
```

注意：

- cloudflared 默认原样转发 `Host`；Gateway 不在乎改不改写（`Origin` 另认 `public_url` 的源，Host 校验认 `public_url` 的主机）。
- **事件流（SSE，`/v1/**/events`）经过 Cloudflare 是否正常，未核实**：要看长连接不被缓冲或截断（Cloudflare 对长时间无数据的连接有空闲超时，Gateway 的 SSE 有心跳，先实测）。
- Gateway 自己托管网页（§3.B）时 ingress 去掉 `path`，整个域名都给 Gateway。

## 2. Tailscale

虚拟机与你的电脑都装 Tailscale，ssh 与调试走 tailnet 名字，不开公网端口。

- 直连 Gateway 调试：Gateway 默认只听回环；要经 tailnet 访问，最省事的是 `ssh -L 7777:127.0.0.1:7777 <虚拟机>` 端口转发，浏览器开 `http://127.0.0.1:7777`（回环地址 Host 校验放行）。要让 Gateway 直接听 tailnet 地址，把 `[gateway] listen` 改成 tailnet IP 并把 tailnet 主机名加进 `trusted_hosts`，但这样 `http` 明文、cookie 无 `Secure`——**不推荐**，只在你确认 tailnet 可信时用。
- 虚拟机上的命令行（`komo session list` 等）永远用本机令牌，不需要登录；别的机器上的命令行用 `komo login <地址>`（设备令牌，存 `remote.json`）。**Pocket ID 不可用时的唯一后路：ssh 到虚拟机处理。**

## 3. 网页放哪

先决条件：`apps/client` 的 SPA 产物（`pnpm build` → `apps/client/dist/client`，发布包里的 `web/` 是同一份）。

### 3.A 单独发布（Cloudflare Pages，未实测）

1. Gateway 的 `config.toml` 设 `[web] serve = false` 并重启：之后 Gateway 只回 `/v1/**`、`/auth/**`、`/healthz`，其余路径 404。
2. 把产物发到 Cloudflare Pages（`wrangler pages deploy apps/client/dist/client --project-name komo-web`，或 Git 集成）。单页应用要让未知路径回落到壳：产物里的 `_shell.html` 需要在 Pages 的 `_redirects` 里写 `/* /_shell.html 200`（**文件名与回落规则要对照实际产物核对**；Gateway 托管时回落的是 `_shell.html` / `index.html`）。
3. 域名分流（二选一，都未实测）：
   - **Pages 自定义域 + 隧道只暴露三类路径**：把 `komo.example.com` 绑到 Pages 项目，再用 Cloudflare 的路径路由（Workers Route / Transform 规则 / 一个极小的 Worker）把 `/v1/*`、`/auth/*`、`/healthz` 转给隧道那条后端；
   - **一个 Worker 做入口**：Worker 对三类路径 `fetch` 隧道的主机名，其余转给 Pages（静态资源）。转发时**不要改写 `Cookie` / `Set-Cookie`，别缓冲 SSE 响应体**。
4. 版本核对：网页启动读 `GET /healthz` 的 `protocol_version`，与自己构建时的不一致就提示刷新或升级（网页一侧是否已做见 `docs/redesign.md` §2.9）。**升级时先升 Gateway 还是先升网页**：协议有不兼容改动（`PROTOCOL_VERSION` 递增）时两端会短暂不配套，个人使用可接受；兼容的改动没有顺序要求。

### 3.B 同机静态目录（Gateway 自己托管，最省事）

保持 `[web] serve = true`（默认），`dir` 指向解出来的 `web/`，隧道 ingress 整个域名给 Gateway。不用分流、不用 Pages。代价是网页与服务端一起发布。

## 4. Pocket ID 与 `[auth.oidc]`

详细步骤与逐项核对清单见 `docs/clients.md` §9.1（同样**未实测**）。摘要：

1. Pocket ID 管理界面 → OIDC Clients → 新建：回调地址 **只填** `https://komo.example.com/auth/callback`；机密客户端记下 Client ID / Secret（或公开客户端 + PKCE）。
2. 记下 Pocket ID 地址作 `issuer`，先 `curl <issuer>/.well-known/openid-configuration` 确认 `issuer` 字段一致、`jwks_uri` 可打开；ID token 算法只认 RS256 / ES256 / EdDSA。
3. 查你自己的 `sub`（或确认邮箱已验证）写进 `allowed_subjects` / `allowed_emails`；不在名单里一律拒绝。
4. `[auth.oidc]` 照 §1.2；Secret 放 `.env` 的 `KOMO_OIDC_CLIENT_SECRET`。
5. 验证：`komo gateway restart` → 浏览器开 `https://komo.example.com/auth/login` → Pocket ID 通行密钥 → 回到网页；`GET /auth/me` 应 `authenticated: true`。

MCP 授权（`oauth = true` 的服务器）走同一个公开地址：网页里点「登录」→ Gateway 返回授权地址 → 浏览器授权 → 回调 `https://komo.example.com/v1/mcp/<名字>/oauth/callback`（要求浏览器仍带着登录 cookie，且是发起授权的同一个登录状态）。回调路径在 `/v1/*` 下，已经在 §1.4 的分流里。**未对真实授权服务器实测。**

## 5. 推送

- 要求页面 **https**（Cloudflare 终止 TLS 即可）；Gateway 需要能出站访问推送服务（FCM / Mozilla autopush / Apple）。
- 首次有人订阅时 Gateway 生成 VAPID 密钥对，存 `<数据目录>/runtime/vapid.json`（0600）。**备份 `~/.komo` 时带上它**：丢了就换一对新密钥，旧订阅在下一次发送时被推送服务拒绝（410 / 4xx）而清掉，各设备在网页里重新打开通知开关即可。
- 总开关 `[push] enabled`；`[push] subject` 或 `public_url` 至少有一个，否则不发（日志 debug 一行）。
- iOS：「添加到主屏幕」后用那个图标打开再开通知（iOS 16.4+）；那个独立应用与 Safari 的登录状态是分开的，要在应用里再登一次。**未验证**。

## 6. 升级与回滚

升级前先备份（Gateway 可以不停；但冷备更稳）：

```bash
systemctl --user stop komo                 # 或 komo gateway stop
cp -a ~/.komo ~/.komo.bak-$(date +%Y%m%d)  # 含 durable.db* aux.db* sessions/ runtime/ mcp/ .env
```

升级：

```bash
komo update        # 从 GitHub release 换二进制（§13.6；tag 必须等于 workspace 版本，包里的 web/ 一并换）
systemctl --user start komo
komo config check
curl -s http://127.0.0.1:7777/healthz      # 看 version 与 protocol_version
```

- **首次升到包含「定时并进目标」的版本时**，Gateway 启动会跑一次存量定时任务的回填（`komo_store::backfill_cron_into_goals`，可重入，跑过了什么都不改），日志有「存量定时任务已并入目标」；回填失败 Gateway 不起（宁可不起也不带着没迁完的定时起服务）。这一步对真实数据的演练**未做**——所以升级前一定备份。
- aux.db 的新表（`push_subscriptions` 等）与新列启动时自动建 / 补，只增不改。
- 网页单独发布时：先发 Gateway、再发网页（或反过来），`protocol_version` 不一致时网页会提示；版本兼容就无所谓顺序。

回滚：

```bash
systemctl --user stop komo
cp ~/.local/bin/komo ~/.local/bin/komo.new      # 想留着新版就先存一份
# 换回旧二进制（旧的发布包 / 上一次 komo update 前备份的二进制）
rm -rf ~/.komo && cp -a ~/.komo.bak-<日期> ~/.komo   # 数据也退回升级前（升级后的对话会丢）
systemctl --user start komo
```

只回退二进制、不回退数据是否可行取决于那次升级有没有改库结构：新版只追加表与列，旧二进制通常能读，**但回填过的定时任务（`cron_jobs` → `matters`）旧版不认**，所以涉及回填的那次升级回滚要连数据一起退。网页那边回滚就是 Pages 上回到上一个部署。

## 7. 自检清单（都未实测）

- [ ] `curl https://komo.example.com/healthz` 有响应，`/v1/chats` 无登录 401。
- [ ] 浏览器登录一圈回到网页；刷新不掉登录；`POST /auth/logout` 后失效。
- [ ] 网页对话的事件流（SSE）经 Cloudflare 能持续收到，长时间空闲不断。
- [ ] 订阅推送后，让一个定时任务停住提问，手机收到通知，点开落在对应对话。
- [ ] `[web] serve = false` 时 `curl https://<Gateway 直连>/` 是 404，`/v1`、`/auth`、`/healthz` 正常。
- [ ] 重启 Gateway 后登录状态与推送订阅都还在。
