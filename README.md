# bark-worker

自用 Bark 推送服务端，Rust (Edition 2024) + WebAssembly on Cloudflare Workers。

## 特性

- 白名单多设备：`DEVICE_KEY` 逗号分隔配置多个 key，匹配才能注册与推送
- Device Token 按设备存 KV（`token:{key}`），App 重装后重新 `/register` 即可
- JWT 缓存 50 分钟，避免触发 APNs `429 TooManyProviderTokenUpdates`
- 依赖精简：`worker` / `serde` / `serde_json` / `p256` / `sha2` / `base64` / `urlencoding`

## 部署

1. Workers & Pages → Create → Workers → Connect to Git，选中本仓库
2. 构建配置：

   | 项 | 值 |
   | --- | --- |
   | Build command | `bun run build` |
   | Deploy command | `bunx wrangler deploy` |
   | Root directory | `/` |

3. Variables and Secrets：

   | 变量 | 类型 | 说明 |
   | --- | --- | --- |
   | `BUN_VERSION` | 明文 | `1.4.3` |
   | `DEVICE_KEY` | Secret | 推送密钥 |

4. 首次部署会自动创建 KV 命名空间并绑定为 `bark`（无需手动创建或填 id）
5. 部署后写入一次 Device Token：

   ```bash
   curl "https://<worker>.workers.dev/register?key=<DEVICE_KEY>&devicetoken=<64位hex>"
   ```

云端构建镜像没有 Rust 工具链和 `pkg-config`，`bun run build` 会自动安装工具链并绕开 `pkg-config` 探测（`scripts/build-ci.sh`）。

## 本地开发

```bash
bun install
bun run dev      # 本地起服务
bun run deploy   # 部署
bun run lint     # clippy -D warnings
```

## 配置

| 变量 | 必需 | 说明 |
| --- | --- | --- |
| `DEVICE_KEY` | 是 | 白名单密钥，逗号分隔可配多个设备，不匹配返回 `403` |

KV 绑定名固定为 `bark`，键为 `device_token`。KV 无记录时推送返回 `400`，提示先调 `/register`。

## API

响应统一为 `{"code": u16, "message": String, "timestamp": u64, ...}`，`Content-Type: application/json; charset=utf-8`。

### `GET /ping`

```json
{"code": 200, "message": "pong", "timestamp": 1792347102}
```

### `GET|POST /register`

参数来自 Query（`?key=...&devicetoken=...`）或 JSON Body（`{"device_key": "...", "device_token": "..."}`）。`key` 必须等于 `DEVICE_KEY`，否则 `403`。

```json
{
  "code": 200,
  "message": "success",
  "timestamp": 1792347102,
  "data": {"key": "...", "device_key": "...", "device_token": "..."}
}
```

### 推送

| 方法 | 路径 |
| --- | --- |
| `GET`/`POST` | `/push`（JSON Body 带 `device_key`） |
| `GET`/`POST` | `/:device_key` |
| `GET`/`POST` | `/:device_key/:body` |
| `GET`/`POST` | `/:device_key/:title/:body` |
| `GET`/`POST` | `/:device_key/:title/:subtitle/:body` |

批量推送：`/push` 传 `device_keys`（数组或逗号分隔字符串），逐设备返回结果：

```bash
curl -X POST "https://<worker>.workers.dev/push" \
  -H 'content-type: application/json' \
  -d '{"device_keys":["key1","key2"],"title":"批量","body":"hi"}'
```

```json
{
  "code": 200,
  "message": "success",
  "data": [
    {"code": 200, "message": "success", "device_key": "key1"},
    {"code": 400, "message": "device token not registered for `key2`; call /register first", "device_key": "key2"}
  ]
}
```

单个设备失败不影响其它设备；只要有一个 key 不在白名单，整个请求返回 `403`。

```bash
curl "https://<worker>.workers.dev/<DEVICE_KEY>/hello"
curl "https://<worker>.workers.dev/<DEVICE_KEY>/告警/磁盘满了?group=ops&level=timeSensitive&badge=1"
curl -X POST "https://<worker>.workers.dev/push" \
  -H 'content-type: application/json' \
  -d '{"device_key":"<KEY>","title":"告警","body":"磁盘满了","icon":"https://example.com/i.png"}'
```

参数合并优先级：Query → 路径段 → JSON Body；路径 `device_key` 优先级最高。

支持 `title`、`subtitle`、`body`、`sound`（默认 `1107`）、`badge`（数字）、`level`，其余字段（`group`、`icon`、`url`、`copy`、`autoCopy`、`call`、`isArchive` 等）原样平铺到 APNs 顶层 JSON。

`level` 映射：

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

`badge` 恒为 JSON 数字，`mutable-content` 固定为 `1`。

## 未实现

MCP / D1 / `/info` / `/healthz` / HTTP Basic Auth / 表单解析 / 开放式注册。

## 已知限制

- APNs 投递未做端到端验证，部署后请用真实 token 实测

## License

MIT
