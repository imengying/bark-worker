# bark-worker

极简、高性能、自用的 Bark 苹果消息推送服务端，使用 **Rust (Edition 2024) + WebAssembly** 在 Cloudflare Workers 上运行。

参考 [`cwxiaos/bark-worker`](https://github.com/cwxiaos/bark-worker) 的协议，但完全重写以去除 D1、MCP、Basic Auth、表单解析、批量推送、开放注册等非必要功能，保持低冷启动开销。

## 特性

- **白名单单设备**：只有 `DEVICE_KEY` 匹配时才能注册/推送
- **无状态优先**：不绑定任何存储也能完整运行；按需绑定 KV 可支持 App 重装后自动刷新 Token
- **JWT 复用**：Provider Token 缓存 50 分钟，避免触发 APNs `429 TooManyProviderTokenUpdates`
- **零 `.unwrap()`**：所有错误统一经 `AppError` 转换为标准 JSON 响应
- **剔除 `regex`**：依赖仅 `worker` / `serde` / `serde_json` / `p256` / `sha2` / `base64` / `urlencoding`

## 在线部署（网页 + GitHub，推荐）

1. 在 Cloudflare Dashboard → **Workers & Pages** → **Create** → **Workers** → **Connect to Git**，选中本仓库。

2. 构建设置：

   | 配置项 | 值 |
   | --- | --- |
   | Build command | `bun run build` |
   | Deploy command | `bunx wrangler deploy` |
   | Root directory | `/` |

3. 在 **Settings → Variables and Secrets** 添加：

   | 变量 | 类型 | 说明 |
   | --- | --- | --- |
   | `BUN_VERSION` | 明文 | `1.2.15`（钉住构建镜像的 Bun 版本） |
   | `DEVICE_KEY` | Secret | 自用推送密钥，高强度随机串 |
   | `DEVICE_TOKEN` | Secret | 默认 APNs Device Token（64 位十六进制） |
   | `APNS_PRIVATE_KEY` | Secret | 可选，覆盖内置 Bark 官方 `.p8` 私钥 |

### 为什么需要 `scripts/build-ci.sh`

Cloudflare 构建镜像（Ubuntu 24.04）预装了 `libssl-dev`、`build-essential`，但**没有 Rust 工具链，也没有 `pkg-config`**。

- 缺 Rust → 脚本自动用 rustup 安装（工具链版本由 `rust-toolchain.toml` 固定，含 `wasm32-unknown-unknown` 目标）。
- 缺 `pkg-config` → `worker-build` 经 `ureq`/`native-tls` 依赖 `openssl-sys`，后者默认靠 `pkg-config` 探测 OpenSSL，缺失即 panic。脚本显式导出 `OPENSSL_LIB_DIR` / `OPENSSL_INCLUDE_DIR`，让 `openssl-sys` 走直接链接路径。

两者结合后云端无需 `sudo apt install` 也能完整构建（已实测）。

> ⚠️ 首次云端构建要安装 Rust 并编译 `worker-build`，约 **5–8 分钟**；脚本用版本戳缓存，后续构建快很多。

## 本地开发

```bash
bun install
bun run dev          # 本地起服务
bun run deploy       # 部署
bun run lint         # clippy -D warnings
```

`bun run build` 会调用 `scripts/build-ci.sh`：检查/安装 Rust 工具链（由 `rust-toolchain.toml` 声明）、
安装 `worker-build`、然后编译。本地首次运行同样需要这几分钟，之后走缓存。

> 本地若 OpenSSL 头文件缺失（`cargo install worker-build` 报 `openssl-sys` 失败），
> Debian/Ubuntu 上装一下即可：`sudo apt install pkg-config libssl-dev`。
> 在线构建不需要这步——`scripts/build-ci.sh` 会自动定位 OpenSSL 绕开 `pkg-config`。

## 配置

| 变量 | 必需 | 说明 |
| --- | --- | --- |
| `DEVICE_KEY` | 是 | 白名单密钥；不匹配一律 `403` |
| `DEVICE_TOKEN` | 是 | 默认 APNs Device Token，无 KV 时的直接回退 |
| `APNS_PRIVATE_KEY` | 否 | PKCS#8 PEM；支持多行或单行字面量 `\n` 两种粘贴形态 |

### 可选 KV 绑定

不绑定即 100% 无状态运行。绑定后 `/register` 写入的 token 会被持久化，App 重装后可自动刷新：

```bash
bun run kv:create
```

把返回的 namespace id 填进 `wrangler.jsonc` 的 `kv_namespaces`（binding 必须是 `database`）。

> ⚠️ 注意：本项目**没有**任何鉴权中间件保护 KV 读取路径。任何持有正确 `DEVICE_KEY` 的人都能改写 token —— 这正是白名单设计的边界。

## API

统一返回 `{"code": u16, "message": String, "timestamp": u64, ...}`，`Content-Type: application/json; charset=utf-8`。

### `GET /ping`

```json
{"code": 200, "message": "pong", "timestamp": 1792347102}
```

### `GET|POST /register`

从 Query（`?key=...&devicetoken=...`）或 JSON Body（`{"device_key": "...", "device_token": "..."}`）提取参数。
`key` 必须严格等于 `DEVICE_KEY`，否则 `403`。

```json
{
  "code": 200,
  "message": "success",
  "timestamp": 1792347102,
  "data": {"key": "...", "device_key": "...", "device_token": "..."}
}
```

### 推送

支持的路由：

| 方法 | 路径 |
| --- | --- |
| `GET`/`POST` | `/push`（JSON Body 带 `device_key`） |
| `GET`/`POST` | `/:device_key` |
| `GET`/`POST` | `/:device_key/:body` |
| `GET`/`POST` | `/:device_key/:title/:body` |
| `GET`/`POST` | `/:device_key/:title/:subtitle/:body` |

```bash
# 最简单的一条
curl "https://<your-worker>.workers.dev/<DEVICE_KEY>/hello"

# 带标题与扩展参数
curl "https://<your-worker>.workers.dev/<DEVICE_KEY>/告警/磁盘满了?group=ops&level=timeSensitive&badge=1"

# JSON Body（V2）
curl -X POST "https://<your-worker>.workers.dev/push" \
  -H 'content-type: application/json' \
  -d '{"device_key":"<KEY>","title":"告警","body":"磁盘满了","icon":"https://example.com/i.png"}'
```

**参数合并优先级**：Query → 路径段（`title`/`subtitle`/`body`）→ JSON Body，而路径 `device_key` 优先级最高。

支持的 Query/JSON 参数：`title`、`subtitle`、`body`、`sound`（默认 `1107`）、`badge`（数字）、`level`，以及任意扩展字段（`group`、`icon`、`url`、`copy`、`autoCopy`、`call`、`isArchive` 等）——扩展字段通过 `#[serde(flatten)]` 捕获并**原样平铺**到 APNs 顶层 JSON。

`level` 映射为 Apple 标准格式：

| 传入 | `aps.interruption-level` | `apns-priority` |
| --- | --- | --- |
| `active`（默认） | `active` | `10` |
| `timeSensitive` | `time-sensitive` | `10` |
| `critical` | `critical` | `10` |
| `passive` | `passive` | `5` |

## 发往 APNs 的载荷

```json
{
  "aps": {
    "alert": {"title": "告警", "body": "磁盘满了"},
    "sound": "minuet",
    "badge": 1,
    "mutable-content": 1,
    "interruption-level": "time-sensitive"
  },
  "group": "ops",
  "url": "https://example.com"
}
```

`badge` 始终序列化为 JSON **数字**；`mutable-content` 固定为 `1`，供 iOS Notification Service Extension 拦截处理。

## 未实现（刻意排除）

MCP / D1 / `/info` / `/healthz` / HTTP Basic Auth / `multipart` 与 `x-www-form-urlencoded` 解析 / 批量推送（`device_keys`）/ 开放式设备注册。

## 已知限制

- **仅单设备**：`DEVICE_TOKEN` 与 KV 均只存一个 token。多设备需求不在设计范围内。
- **APNs 投递未做端到端验证**（开发环境出站受限），请部署后用真实 token 实测一次。

## License

MIT
