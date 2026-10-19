//! HTTP 路由、白名单鉴权、设备 Token 存取与统一 JSON 响应。

use std::cell::RefCell;

use serde::Serialize;
use serde_json::Value;
use worker::{
    Env, Error, Headers, KvStore, Request, Response, ResponseBuilder, RouteContext, Router, Url,
};

use crate::apns::{self, unix_now};
use crate::payload::{PushPayload, RegisterData};

/// `device_token` 在 KV 中的键名。
const KV_TOKEN_KEY: &str = "device_token";
/// 设备白名单密钥的环境变量名。
const ENV_DEVICE_KEY: &str = "DEVICE_KEY";
/// KV 命名空间绑定名（必需）。
const KV_BINDING: &str = "database";

thread_local! {
    // Isolate 内设备 Token 缓存（App 重装刷新后避免每次请求都查 KV）。
    static DEVICE_TOKEN_CACHE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// 应用层错误：统一转换为标准 JSON 响应。
#[derive(Debug)]
pub struct AppError(u16, String);

impl AppError {
    fn new(code: u16, message: impl Into<String>) -> Self {
        Self(code, message.into())
    }

    fn message(&self) -> &str {
        &self.1
    }

    /// 转换为 HTTP 响应（`From<&AppError> for Result<..>` 的落点）。
    fn into_response(self) -> Result<Response, Error> {
        json_response(self.0, self.message(), None::<u8>)
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

/// 输出 JSON 响应（`Content-Type: application/json; charset=utf-8`）。
///
/// 注意：不使用 `ResponseBuilder::from_json`，因为它会把 Content-Type
/// 覆盖为不含 charset 的 `application/json`。
fn json_response<T: Serialize>(
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

/// 将 `Result<_, AppError>` 落地为 `Result<Response, worker::Error>`。
fn settle(result: Result<Response, AppError>) -> Result<Response, Error> {
    match result {
        Ok(resp) => Ok(resp),
        Err(err) => err.into_response(),
    }
}

/// 读取环境变量（缺失时返回 `None`）。
fn env_var(env: &Env, name: &str) -> Option<String> {
    env.var(name).ok().map(|v| v.to_string())
}

/// 白名单密钥：环境变量 `DEVICE_KEY`。
fn device_key(env: &Env) -> Result<String, AppError> {
    env_var(env, ENV_DEVICE_KEY)
        .filter(|k| !k.is_empty())
        .ok_or_else(|| {
            AppError::new(500, "server misconfigured: DEVICE_KEY is not set")
        })
}

/// 获取 KV 绑定（`database`）。KV 是必需依赖，未绑定即视为配置错误。
fn kv_store(env: &Env) -> Result<KvStore, AppError> {
    env.kv(KV_BINDING).map_err(|_| {
        AppError::new(
            500,
            "server misconfigured: KV binding `database` is required; \
             create a namespace and add it in wrangler.jsonc",
        )
    })
}

/// 读取设备 Token：内存缓存 -> KV。
///
/// KV 读取不设 `cache_ttl`：`register` 刚写入的新 token 必须立刻生效，
/// 否则 App 重装后立刻推送会打到旧 token 上。
async fn device_token(env: &Env) -> Result<String, AppError> {
    if let Some(token) = DEVICE_TOKEN_CACHE.with(|cache| cache.borrow().clone()) {
        return Ok(token);
    }
    let token = kv_store(env)?
        .get(KV_TOKEN_KEY)
        .text()
        .await
        .map_err(|e| AppError::new(500, format!("KV read failed: {e}")))?;
    let Some(token) = token.filter(|t| !t.trim().is_empty()) else {
        return Err(AppError::new(
            400,
            "device token not registered yet; call /register first",
        ));
    };
    let token = token.trim().to_string();
    DEVICE_TOKEN_CACHE.with(|cache| *cache.borrow_mut() = Some(token.clone()));
    Ok(token)
}

/// 写入设备 Token：内存缓存 + KV。
async fn save_device_token(env: &Env, token: &str) -> Result<(), AppError> {
    DEVICE_TOKEN_CACHE.with(|cache| *cache.borrow_mut() = Some(token.to_string()));
    kv_store(env)?
        .put(KV_TOKEN_KEY, token)
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
    json_response(200, "pong", None::<u8>)
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

    let expected = device_key(env)?;
    if key.as_deref() != Some(expected.as_str()) {
        return Err(AppError::new(403, "forbidden"));
    }

    // 未传 token 时沿用当前已存储的 token；此时若服务端未配置则如实报配置错误，
    // 而不是含糊地提示「token 为空」。
    let current = match token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(passed) => passed.to_string(),
        None => device_token(env).await?,
    };

    if current.len() > 160 {
        return Err(AppError::new(400, "device token is invalid"));
    }

    save_device_token(env, &current).await?;

    Ok(json_response(
        200,
        "success",
        Some(RegisterData {
            key: expected.as_str(),
            device_key: expected.as_str(),
            device_token: current.as_str(),
        }),
    )?)
}

/// 推送处理：合并 Query / JSON / 路径参数，白名单校验后调用 APNs。
async fn push(
    req: &mut Request,
    env: &Env,
    path_key: Option<String>,
    path_title: Option<String>,
    path_subtitle: Option<String>,
    path_body: Option<String>,
) -> Result<Response, AppError> {
    let url = req.url()?;
    // 合并优先级（对齐 Bark 参考实现）：Query -> 路径参数 -> JSON Body；
    // 路径 device_key 无条件优先（URL 即推送目标）。
    let mut payload = push_payload_from_pairs(&query_pairs(&url));
    if let Some(title) = path_title {
        payload.title = Some(title);
    }
    if let Some(subtitle) = path_subtitle {
        payload.subtitle = Some(subtitle);
    }
    if let Some(body) = path_body {
        payload.body = Some(body);
    }
    payload.merge(push_payload_from_json(req).await?);
    if let Some(key) = path_key {
        payload.device_key = Some(key);
    }

    let expected = device_key(env)?;
    let key = payload.device_key.clone().unwrap_or_default();
    if key.is_empty() {
        return Err(AppError::new(400, "device key is empty"));
    }
    if key != expected {
        return Err(AppError::new(403, "forbidden"));
    }

    // 推送目标一律走存储解析：内存缓存 -> KV。
    let device_token = device_token(env).await?;
    // APNs device token 必须是十六进制串；它会被直接拼进请求 URL，
    // 非法字符会产生畸形请求并让失败原因难以定位。
    if !device_token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::new(
            500,
            "stored device token is not a hex string; re-register a valid token",
        ));
    }

    let priority: u8 = if payload.is_passive() { 5 } else { 10 };
    let body = payload.build_apns_body()?;

    let (status, text) = apns::push(env, &device_token, &body, priority).await?;
    if (200..300).contains(&status) {
        return Ok(json_response(200, "success", None::<u8>)?);
    }
    let reason = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("reason")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| {
            if text.is_empty() {
                "unknown error".to_string()
            } else {
                text.clone()
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

/// 提取并解码第 `index` 个路径段。
fn decoded_segment(path: &str, index: usize) -> Option<Result<String, AppError>> {
    segment(path, index).map(|s| decode_path_segment(&s))
}

/// 构建路由表（静态路由优先，动态段路由回退）。
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
            settle(push(&mut req, &ctx.env, None, None, None, None).await)
        })
        .get_async("/push", |mut req, ctx: RouteContext<()>| async move {
            settle(push(&mut req, &ctx.env, None, None, None, None).await)
        })
        .or_else_any_method_async("/:device_key", |mut req, ctx: RouteContext<()>| async move {
            let path = req.path();
            let key = segment(&path, 0).unwrap_or_default();
            settle(push(&mut req, &ctx.env, Some(key), None, None, None).await)
        })
        .or_else_any_method_async(
            "/:device_key/:body",
            |mut req, ctx: RouteContext<()>| async move {
                let path = req.path();
                let key = segment(&path, 0).unwrap_or_default();
                let body = decoded_segment(&path, 1);
                match body {
                    Some(Ok(body)) => {
                        settle(push(&mut req, &ctx.env, Some(key), None, None, Some(body)).await)
                    }
                    Some(Err(e)) => e.into_response(),
                    None => settle(push(&mut req, &ctx.env, Some(key), None, None, None).await),
                }
            },
        )
        .or_else_any_method_async(
            "/:device_key/:title/:body",
            |mut req, ctx: RouteContext<()>| async move {
                let path = req.path();
                let key = segment(&path, 0).unwrap_or_default();
                let title = decoded_segment(&path, 1);
                let body = decoded_segment(&path, 2);
                match (title, body) {
                    (Some(Ok(title)), Some(Ok(body))) => {
                        settle(push(&mut req, &ctx.env, Some(key), Some(title), None, Some(body)).await)
                    }
                    (Some(Err(e)), _) | (None, Some(Err(e))) => e.into_response(),
                    _ => settle(push(&mut req, &ctx.env, Some(key), None, None, None).await),
                }
            },
        )
        .or_else_any_method_async(
            "/:device_key/:title/:subtitle/:body",
            |mut req, ctx: RouteContext<()>| async move {
                let path = req.path();
                let key = segment(&path, 0).unwrap_or_default();
                let title = decoded_segment(&path, 1);
                let subtitle = decoded_segment(&path, 2);
                let body = decoded_segment(&path, 3);
                let mut failure: Option<AppError> = None;
                let mut values: [Option<String>; 3] = [None, None, None];
                for (slot, part) in values.iter_mut().zip([title, subtitle, body]) {
                    match part {
                        Some(Ok(value)) => *slot = Some(value),
                        Some(Err(err)) => failure = Some(err),
                        None => {}
                    }
                }
                if let Some(err) = failure {
                    return err.into_response();
                }
                let [Some(title), Some(subtitle), Some(body)] = values else {
                    return settle(push(&mut req, &ctx.env, Some(key), None, None, None).await);
                };
                settle(
                    push(&mut req, &ctx.env, Some(key), Some(title), Some(subtitle), Some(body))
                        .await,
                )
            },
        )
        // 5 段及以上路径：返回 JSON 404（对齐 Bark 协议错误格式）。
        .or_else_any_method_async(
            "/:a/:b/:c/:d/:e",
            |req, _ctx: RouteContext<()>| async move {
                json_response(
                    404,
                    &format!("Cannot {} {}", req.method(), req.path()),
                    None::<u8>,
                )
            },
        )
        .or_else_any_method_async("/", |_req, _ctx| async { Response::ok("ok") })
}
