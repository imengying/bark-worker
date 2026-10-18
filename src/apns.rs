//! APNs 集成：ES256 JWT 签名（含 50 分钟缓存）与推送请求构造。

use std::cell::RefCell;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use p256::ecdsa::signature::hazmat::PrehashSigner;
use p256::ecdsa::SigningKey;
use p256::pkcs8::DecodePrivateKey;
use sha2::{Digest, Sha256};
use worker::{Date, Env, Error, Headers, Method, Request, RequestInit};

/// APNs 接入点。
const APNS_HOST: &str = "https://api.push.apple.com/3/device/";
/// Bark 官方 App 的 APNs Topic。
const APNS_TOPIC: &str = "me.fin.bark";
/// JWT 头部 `kid`：Bark 官方公开的 APNs Auth Key ID。
const APNS_KEY_ID: &str = "LH4T9V5U4R";
/// JWT 载荷 `iss`：Bark 官方公开的 Team ID。
const APNS_TEAM_ID: &str = "5U8LBRXG3A";

/// JWT 复用窗口：3000 秒（50 分钟）。
/// 频繁刷新会触发 APNs `TooManyProviderTokenUpdates` (429)。
const JWT_TTL_SECS: u64 = 3000;

/// Bark 官方在开源仓库公开的 APNs `.p8` 私钥（PKCS#8 PEM）。
/// 可通过环境变量 `APNS_PRIVATE_KEY` 覆盖。
const BARK_OFFICIAL_KEY: &str = concat!(
    "-----BEGIN PRIVATE KEY-----\n",
    "MIGTAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBHkwdwIBAQQg4vtC3g5L5HgKGJ2+\n",
    "T1eA0tOivREvEAY2g+juRXJkYL2gCgYIKoZIzj0DAQehRANCAASmOs3JkSyoGEWZ\n",
    "sUGxFs/4pw1rIlSV2IC19M8u3G5kq36upOwyFWj9Gi3Ejc9d3sC7+SHRqXrEAJow\n",
    "8/7tRpV+\n",
    "-----END PRIVATE KEY-----"
);

thread_local! {
    // Isolate 内 JWT 缓存：`(token, 签发时间戳)`。
    static JWT_CACHE: RefCell<Option<(String, u64)>> = const { RefCell::new(None) };
}

/// 当前秒级 Unix 时间戳。
pub fn unix_now() -> u64 {
    Date::now().as_millis() / 1000
}

/// 将 PKCS#8 PEM 还原为 DER 字节。
///
/// 说明：为严控 Wasm 体积，依赖仅启用 `p256` 的 `pkcs8` 特性
/// （不含 `pem` RFC7468 解析链），此处手工剥离头尾与换行。
///
/// 同时兼容两种常见粘贴形态：
/// - 真实多行 PEM；
/// - 单行且用字面量 `\n` 转义的 PEM（`wrangler secret put`、CI 变量里很常见）。
fn pem_to_der(pem: &str) -> Result<Vec<u8>, Error> {
    let normalized = pem.replace("\\n", "\n");
    let body = normalized
        .lines()
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with("-----")
        })
        .fold(String::new(), |mut acc, line| {
            acc.push_str(line.trim());
            acc
        });
    if body.is_empty() {
        return Err(Error::RustError(
            "APNs private key PEM contains no key material (check `APNS_PRIVATE_KEY`)".into(),
        ));
    }
    STANDARD
        .decode(body.as_bytes())
        .map_err(|e| Error::RustError(format!("APNs private key is not valid base64: {e}")))
}

/// 解析 SigningKey：优先环境变量 `APNS_PRIVATE_KEY`，否则使用内置 Bark 官方密钥。
///
/// 环境变量存在但内容不含 PEM 头时直接报错，而不是静默回落到官方密钥 ——
/// 后者会让推送「看起来成功」却由 Bark 官方 App 接收，极难排查。
fn signing_key(env: &Env) -> Result<SigningKey, Error> {
    let pem = match env.var("APNS_PRIVATE_KEY").ok().map(|v| v.to_string()) {
        Some(v) if v.trim().is_empty() => BARK_OFFICIAL_KEY.to_string(),
        Some(v) if v.contains("PRIVATE KEY") => v,
        Some(_) => {
            return Err(Error::RustError(
                "`APNS_PRIVATE_KEY` is set but is not a PKCS#8 PEM private key".into(),
            ));
        }
        None => BARK_OFFICIAL_KEY.to_string(),
    };
    let der = pem_to_der(&pem)?;
    SigningKey::from_pkcs8_der(&der)
        .map_err(|e| Error::RustError(format!("APNs private key parse failed: {e}")))
}

/// 签发一枚 ES256 JWT（对 `header.payload` 做 SHA-256 后 ECDSA 签名）。
fn mint_jwt(env: &Env, now: u64) -> Result<String, Error> {
    let key = signing_key(env)?;
    let header = format!(r#"{{"alg":"ES256","kid":"{APNS_KEY_ID}"}}"#);
    let claims = format!(r#"{{"iss":"{APNS_TEAM_ID}","iat":{now}}}"#);
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(claims)
    );
    let digest: [u8; 32] = Sha256::digest(signing_input.as_bytes()).into();
    let (signature, _) = key.sign_prehash(&digest).map_err(|e| {
        Error::RustError(format!("ES256 signing failed: {e}"))
    })?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

/// 获取（或复用）APNs Provider Token。
///
/// 有效期内的 token 直接复用，避免每次请求重新签名触发 429。
fn auth_token(env: &Env, now: u64) -> Result<String, Error> {
    if let Some((token, issued_at)) = JWT_CACHE.with(|cache| cache.borrow().clone())
        && now.saturating_sub(issued_at) < JWT_TTL_SECS
    {
        return Ok(token);
    }
    let token = mint_jwt(env, now)?;
    JWT_CACHE.with(|cache| *cache.borrow_mut() = Some((token.clone(), now)));
    Ok(token)
}

/// 向 APNs 推送 JSON Body，返回 `(HTTP 状态码, 失败原因文本)`。
///
/// 通过 `worker::Fetch::Request` 走 Workers 原生 fetch，复用 Cloudflare 边缘
/// 到 `api.push.apple.com` 的 HTTP/2 连接池。
pub async fn push(
    env: &Env,
    device_token: &str,
    body: &str,
    priority: u8,
) -> Result<(u16, String), Error> {
    let token = auth_token(env, unix_now())?;
    let uri = format!("{APNS_HOST}{device_token}");

    let headers = Headers::new();
    headers.set("authorization", &format!("bearer {token}"))?;
    headers.set("apns-topic", APNS_TOPIC)?;
    headers.set("apns-push-type", "alert")?;
    headers.set("apns-priority", &priority.to_string())?;
    headers.set("content-type", "application/json")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(body.to_string().into()));

    let req = Request::new_with_init(&uri, &init)?;
    let mut resp = worker::Fetch::Request(req).send().await?;
    let status = resp.status_code();
    // 读不到响应体不应掩盖真实状态码；失败时只丢失 reason 文本。
    let text = resp.text().await.unwrap_or_else(|err| {
        worker::console_error!("failed to read APNs response body: {err}");
        String::new()
    });
    Ok((status, text))
}
