//! HTTP 路由、白名单鉴权、设备 Token 存取与统一 JSON 响应。

use std::cell::RefCell;
use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;
use worker::{
    Env, Error, Headers, KvStore, Request, Response, ResponseBuilder, RouteContext, Router, Url,
};

use crate::apns::{self, unix_now};
use crate::payload::{PushPayload, RegisterData};

/// KV 中设备 Token 的键前缀，实际键为 `token:{device_key}`。
const KV_TOKEN_PREFIX: &str = "token:";
/// 设备白名单密钥的环境变量名。
const ENV_DEVICE_KEY: &str = "DEVICE_KEY";
/// KV 命名空间绑定名（必需）。
const KV_BINDING: &str = "bark";

thread_local! {
    // Isolate 内设备 Token 缓存：device_key -> token。
    static DEVICE_TOKEN_CACHE: RefCell<BTreeMap<String, String>> =
        const { RefCell::new(BTreeMap::new()) };
}

/// 应用层错误：`(HTTP 状态码, 消息)`，统一转换为标准 JSON 响应。
#[derive(Debug)]
pub struct AppError(u16, String);

/// 批量推送中单个设备的结果。
#[derive(Serialize)]
struct BatchResult<'a> {
    code: u16,
    message: String,
    device_key: &'a str,
}

impl AppError {
    fn new(code: u16, message: impl Into<String>) -> Self {
        Self(code, message.into())
    }

    /// 转换为标准 JSON 错误响应。
    fn into_response(self) -> Result<Response, Error> {
        json_response(self.0, &self.1)
    }
}

impl From<Error> for AppError {
    fn from(err: Error) -> Self {
        Self(500, err.to_string())
    }
}

/// 标准响应体骨架：`{"code","message","timestamp"[,"data"]}`。
#[derive(Serialize)]
struct ApiEnvelope<'a, T> {
    code: u16,
    message: &'a str,
    timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
}

/// 序列化为 JSON 响应（`Content-Type: application/json; charset=utf-8`）。
fn respond<T: Serialize>(
    status: u16,
    message: &str,
    data: Option<T>,
) -> Result<Response, Error> {
    let body = ApiEnvelope {
        code: status,
        message,
        timestamp: unix_now(),
        data,
    };
    let payload = serde_json::to_string(&body)
        .map_err(|e| Error::RustError(format!("response encode failed: {e}")))?;
    let headers = Headers::new();
    headers.set("content-type", "application/json; charset=utf-8")?;
    Ok(ResponseBuilder::new()
        .with_status(status)
        .with_headers(headers)
        .fixed(payload.into_bytes()))
}

/// 不带 `data` 的 JSON 响应。
fn json_response(status: u16, message: &str) -> Result<Response, Error> {
    respond(status, message, None::<()>)
}

/// 将 `Result<_, AppError>` 落地为 `Result<Response, worker::Error>`。
fn settle(result: Result<Response, AppError>) -> Result<Response, Error> {
    match result {
        Ok(resp) => Ok(resp),
        Err(err) => err.into_response(),
    }
}

/// 白名单：环境变量 `DEVICE_KEY`，支持逗号分隔多个设备密钥。
fn device_keys(env: &Env) -> Result<Vec<String>, AppError> {
    let raw = env
        .var(ENV_DEVICE_KEY)
        .ok()
        .map(|v| v.to_string())
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| AppError::new(500, "server misconfigured: DEVICE_KEY is not set"))?;
    Ok(raw
        .split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .collect())
}

/// 校验 `key` 是否在白名单内。
fn is_allowed(allowed: &[String], key: &str) -> bool {
    allowed.iter().any(|k| k == key)
}

/// 获取 KV 绑定（`bark`）。KV 是必需依赖，未绑定即视为配置错误。
fn kv_store(env: &Env) -> Result<KvStore, AppError> {
    env.kv(KV_BINDING).map_err(|_| {
        AppError::new(
            500,
            "server misconfigured: KV binding `bark` is required; \
             add it to kv_namespaces in wrangler.jsonc",
        )
    })
}

/// 读取指定设备的 Token：内存缓存 -> KV。
///
/// KV 读取不设 `cache_ttl`：`register` 刚写入的新 token 必须立刻生效。
async fn device_token(env: &Env, key: &str) -> Result<String, AppError> {
    if let Some(token) = DEVICE_TOKEN_CACHE.with(|c| c.borrow().get(key).cloned()) {
        return Ok(token);
    }
    let stored = kv_store(env)?
        .get(&format!("{KV_TOKEN_PREFIX}{key}"))
        .text()
        .await
        .map_err(|e| AppError::new(500, format!("KV read failed: {e}")))?;
    let Some(token) = stored.filter(|t| !t.trim().is_empty()) else {
        return Err(AppError::new(
            400,
            format!("device token not registered for `{key}`; call /register first"),
        ));
    };
    let token = token.trim().to_string();
    DEVICE_TOKEN_CACHE.with(|c| c.borrow_mut().insert(key.to_string(), token.clone()));
    Ok(token)
}

/// 写入指定设备的 Token：内存缓存 + KV。
async fn save_device_token(env: &Env, key: &str, token: &str) -> Result<(), AppError> {
    DEVICE_TOKEN_CACHE.with(|c| c.borrow_mut().insert(key.to_string(), token.to_string()));
    kv_store(env)?
        .put(&format!("{KV_TOKEN_PREFIX}{key}"), token)
        .map_err(|e| AppError::new(500, format!("KV write failed: {e}")))?
        .execute()
        .await
        .map_err(|e| AppError::new(500, format!("KV write failed: {e}")))
}

/// 解析 URL Query 为小写键名键值对。
fn query_pairs(url: &Url) -> Vec<(String, String)> {
    url.query_pairs()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
        .collect()
}

/// 将键值对解释为推送参数（`badge` 数字化，未知参数进入 `extra`）。
fn push_payload_from_pairs(pairs: &[(String, String)]) -> PushPayload {
    let mut payload = PushPayload::default();
    for (key, value) in pairs {
        if value.is_empty() {
            continue;
        }
        match key.as_str() {
            "device_key" | "devicekey" | "key" => payload.device_key = Some(value.clone()),
            "title" => payload.title = Some(value.clone()),
            "subtitle" => payload.subtitle = Some(value.clone()),
            "body" => payload.body = Some(value.clone()),
            "sound" => payload.sound = Some(value.clone()),
            "level" => payload.level = Some(value.clone()),
            "badge" => match value.parse::<u32>() {
                Ok(badge) => payload.badge = Some(badge),
                Err(_) => {
                    payload.extra.insert(key.clone(), Value::from(value.as_str()));
                }
            },
            _ => {
                payload.extra.insert(key.clone(), Value::from(value.as_str()));
            }
        }
    }
    payload
}

/// 从 POST JSON Body 解析推送参数；Body 缺失或非 JSON 时静默忽略。
///
/// `req.text()` 失败是真正的读取错误（连接中断、编码异常），必须上报，
/// 否则会静默推送一条空通知。
async fn push_payload_from_json(req: &mut Request) -> Result<PushPayload, AppError> {
    if !is_json_request(req) {
        return Ok(PushPayload::default());
    }
    let text = req
        .text()
        .await
        .map_err(|e| AppError::new(400, format!("failed to read request body: {e}")))?;
    if text.trim().is_empty() {
        return Ok(PushPayload::default());
    }
    serde_json::from_str::<PushPayload>(&text)
        .map_err(|e| AppError::new(400, format!("request bind failed: {e}")))
}

/// `GET /ping`
async fn ping() -> Result<Response, Error> {
    json_response(200, "pong")
}

/// 判断请求是否声明了 JSON 内容类型。
fn is_json_request(req: &Request) -> bool {
    req.headers()
        .get("content-type")
        .ok()
        .flatten()
        .is_some_and(|v| v.to_ascii_lowercase().contains("application/json"))
}

/// 把 JSON 对象里的字符串字段按小写键名写回 `(key, token)`。
///
/// JSON 解析失败会如实上报：客户端传了坏 JSON 却收到 `403`/`400 key is empty`
/// 会非常难排查。
fn absorb_register_json(
    text: &str,
    key: &mut Option<String>,
    token: &mut Option<String>,
) -> Result<(), AppError> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let Value::Object(obj) = serde_json::from_str::<Value>(text)
        .map_err(|e| AppError::new(400, format!("request bind failed: {e}")))?
    else {
        return Ok(());
    };
    for (k, v) in obj {
        let Some(s) = v.as_str() else { continue };
        match k.to_ascii_lowercase().as_str() {
            "key" | "device_key" | "devicekey" => *key = Some(s.to_string()),
            "devicetoken" | "device_token" => *token = Some(s.to_string()),
            _ => {}
        }
    }
    Ok(())
}

/// `GET|POST /register`：仅白名单 `DEVICE_KEY` 允许注册/刷新设备 Token。
async fn register(req: &mut Request, env: &Env) -> Result<Response, AppError> {
    let url = req.url()?;
    let mut key: Option<String> = None;
    let mut token: Option<String> = None;

    for (k, v) in query_pairs(&url) {
        if v.is_empty() {
            continue;
        }
        match k.as_str() {
            "key" | "device_key" | "devicekey" => key = Some(v),
            "devicetoken" | "device_token" => token = Some(v),
            _ => {}
        }
    }

    if is_json_request(req) {
        let text = req
            .text()
            .await
            .map_err(|e| AppError::new(400, format!("failed to read request body: {e}")))?;
        absorb_register_json(&text, &mut key, &mut token)?;
    }

    let allowed = device_keys(env)?;
    let Some(key) = key.filter(|k| is_allowed(&allowed, k)) else {
        return Err(AppError::new(403, "forbidden"));
    };

    // 未传 token 时沿用该设备已存的 token。
    let current = match token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(passed) => passed.to_string(),
        None => device_token(env, &key).await?,
    };

    if current.len() > 160 || !current.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::new(400, "device token is invalid"));
    }

    save_device_token(env, &key, &current).await?;

    Ok(respond(
        200,
        "success",
        Some(RegisterData {
            key: &key,
            device_key: &key,
            device_token: &current,
        }),
    )?)
}

/// 推送处理：合并 Query / JSON / 路径参数，白名单校验后调用 APNs。
async fn push(
    req: &mut Request,
    env: &Env,
    params: PushParams,
) -> Result<Response, AppError> {
    let url = req.url()?;
    // 合并优先级：Query -> 路径参数 -> JSON Body；路径 device_key 最高。
    let mut payload = push_payload_from_pairs(&query_pairs(&url));
    if let Some(title) = params.title {
        payload.title = Some(title);
    }
    if let Some(subtitle) = params.subtitle {
        payload.subtitle = Some(subtitle);
    }
    if let Some(body) = params.body {
        payload.body = Some(body);
    }
    payload.merge(push_payload_from_json(req).await?);
    if !params.key.is_empty() {
        payload.device_key = Some(params.key);
    }

    let allowed = device_keys(env)?;
    // 目标设备：显式 device_keys 优先，否则用 device_key / 路径 key。
    let batch = payload.take_device_keys();
    let targets: Vec<String> = if !batch.is_empty() {
        batch
    } else {
        let key = payload.device_key.clone().unwrap_or_default();
        if key.is_empty() {
            return Err(AppError::new(400, "device key is empty"));
        }
        vec![key]
    };
    if targets.iter().any(|k| !is_allowed(&allowed, k)) {
        return Err(AppError::new(403, "forbidden"));
    }

    let priority: u8 = if payload.is_passive() { 5 } else { 10 };
    let body = payload.build_apns_body()?;

    // 单设备：直接返回 APNs 结果，保持原有响应形态。
    if let [only] = targets.as_slice() {
        send_one(env, only, &body, priority).await?;
        return Ok(json_response(200, "success")?);
    }

    // 多设备：逐个发送并汇总，失败不影响其它设备。
    let mut results = Vec::with_capacity(targets.len());
    for key in &targets {
        let (code, message) = match send_one(env, key, &body, priority).await {
            Ok(()) => (200, "success".to_string()),
            Err(AppError(code, msg)) => (code, msg),
        };
        results.push(BatchResult {
            code,
            message,
            device_key: key.as_str(),
        });
    }
    Ok(respond(200, "success", Some(results))?)
}

/// 向单个设备发送，成功返回 `Ok(())`，失败带上 APNs 状态码与原因。
async fn send_one(env: &Env, key: &str, body: &str, priority: u8) -> Result<(), AppError> {
    let device_token = device_token(env, key).await?;
    // APNs device token 必须是十六进制串，否则会拼出畸形 URL。
    if !device_token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::new(
            500,
            "stored device token is not a hex string; re-register a valid token",
        ));
    }

    let (status, text) = apns::push(&device_token, body, priority).await?;
    if (200..300).contains(&status) {
        return Ok(());
    }
    let reason = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("reason").and_then(Value::as_str).map(str::to_string))
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| {
            if text.is_empty() {
                "unknown error".to_string()
            } else {
                text
            }
        });
    Err(AppError::new(status, format!("push failed: {reason}")))
}

/// 对路径段做 URL 解码：`+` 视作空格，`%xx` 还原。
fn decode_path_segment(segment: &str) -> Result<String, AppError> {
    urlencoding::decode(&segment.replace('+', " "))
        .map(|cow| cow.into_owned())
        .map_err(|_| AppError::new(400, "url path parse failed"))
}

/// 提取非空路径段。
fn segment(path: &str, index: usize) -> Option<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .nth(index)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 解析路径段为 `(key, title, subtitle, body)`，按已有段数决定哪些字段生效。
///
/// 路径形如 `/:key[/:body[/:title/:body...]]`；`populated` 是有意义的段数
/// （已排除前导空段），解码失败返回 400。
fn route_segments(path: &str, populated: usize) -> Result<PushParams, AppError> {
    let decoded = |i: usize| -> Result<Option<String>, AppError> {
        match segment(path, i) {
            Some(raw) => decode_path_segment(&raw).map(Some),
            None => Ok(None),
        }
    };
    let key = segment(path, 0).unwrap_or_default();
    let (title, subtitle, body) = match populated {
        0 | 1 => (None, None, None),
        2 => (None, None, decoded(1)?),
        3 => (decoded(1)?, None, decoded(2)?),
        _ => (decoded(1)?, decoded(2)?, decoded(3)?),
    };
    Ok(PushParams { key, title, subtitle, body })
}

/// 路径参数。
#[derive(Default)]
struct PushParams {
    key: String,
    title: Option<String>,
    subtitle: Option<String>,
    body: Option<String>,
}

/// 构建路由表。
pub fn router() -> Router<'static, ()> {
    Router::new()
        .get_async("/ping", |_req, _ctx| ping())
        .get_async("/register", |mut req, ctx: RouteContext<()>| async move {
            settle(register(&mut req, &ctx.env).await)
        })
        .post_async("/register", |mut req, ctx: RouteContext<()>| async move {
            settle(register(&mut req, &ctx.env).await)
        })
        .post_async("/push", |mut req, ctx: RouteContext<()>| async move {
            settle(push(&mut req, &ctx.env, PushParams::default()).await)
        })
        .get_async("/push", |mut req, ctx: RouteContext<()>| async move {
            settle(push(&mut req, &ctx.env, PushParams::default()).await)
        })
        .or_else_any_method_async("/:device_key", |mut req, ctx: RouteContext<()>| async move {
            let path = req.path();
            settle(dispatch(&mut req, &ctx.env, &path, 1).await)
        })
        .or_else_any_method_async(
            "/:device_key/:body",
            |mut req, ctx: RouteContext<()>| async move {
                let path = req.path();
                settle(dispatch(&mut req, &ctx.env, &path, 2).await)
            },
        )
        .or_else_any_method_async(
            "/:device_key/:title/:body",
            |mut req, ctx: RouteContext<()>| async move {
                let path = req.path();
                settle(dispatch(&mut req, &ctx.env, &path, 3).await)
            },
        )
        .or_else_any_method_async(
            "/:device_key/:title/:subtitle/:body",
            |mut req, ctx: RouteContext<()>| async move {
                let path = req.path();
                settle(dispatch(&mut req, &ctx.env, &path, 4).await)
            },
        )
        // 5 段及以上路径：JSON 404。
        .or_else_any_method_async(
            "/:a/:b/:c/:d/:e",
            |req, _ctx: RouteContext<()>| async move {
                json_response(404, &format!("Cannot {} {}", req.method(), req.path()))
            },
        )
        .or_else_any_method_async("/", |_req, _ctx| async { Response::ok("ok") })
}

/// 按路径段数解析参数并执行推送。
async fn dispatch(
    req: &mut Request,
    env: &Env,
    path: &str,
    populated: usize,
) -> Result<Response, AppError> {
    let params = route_segments(path, populated)?;
    push(req, env, params).await
}
