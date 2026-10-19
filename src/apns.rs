//! APNs 集成：ES256 JWT 签名（含 50 分钟缓存）与推送请求构造。

use std::cell::RefCell;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdsa::signature::hazmat::PrehashSigner;
use p256::ecdsa::SigningKey;
use p256::pkcs8::DecodePrivateKey;
use sha2::{Digest, Sha256};
use worker::{Date, Error, Headers, Method, Request, RequestInit};

/// APNs 接入点。
const APNS_HOST: &str = "https://api.push.apple.com/3/device/";
/// Bark 官方 App 的 APNs Topic。
const APNS_TOPIC: &str = "me.fin.bark";
/// JWT 头部 `kid`。
const APNS_KEY_ID: &str = "LH4T9V5U4R";
/// JWT 载荷 `iss`。
const APNS_TEAM_ID: &str = "5U8LBRXG3A";

/// JWT 复用窗口：3000 秒（50 分钟）。
/// 频繁刷新会触发 APNs `TooManyProviderTokenUpdates` (429)。
const JWT_TTL_SECS: u64 = 3000;

/// Bark 官方公开的 APNs `.p8` 私钥（PKCS#8 DER）。
const APNS_KEY_DER: &[u8] = &[
    0x30, 0x81, 0x93, 0x02, 0x01, 0x00, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86,
    0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d,
    0x03, 0x01, 0x07, 0x04, 0x79, 0x30, 0x77, 0x02, 0x01, 0x01, 0x04, 0x20,
    0xe2, 0xfb, 0x42, 0xde, 0x0e, 0x4b, 0xe4, 0x78, 0x0a, 0x18, 0x9d, 0xbe,
    0x4f, 0x57, 0x80, 0xd2, 0xd3, 0xa2, 0xbd, 0x11, 0x2f, 0x10, 0x06, 0x36,
    0x83, 0xe8, 0xee, 0x45, 0x72, 0x64, 0x60, 0xbd, 0xa0, 0x0a, 0x06, 0x08,
    0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0xa1, 0x44, 0x03, 0x42,
    0x00, 0x04, 0xa6, 0x3a, 0xcd, 0xc9, 0x91, 0x2c, 0xa8, 0x18, 0x45, 0x99,
    0xb1, 0x41, 0xb1, 0x16, 0xcf, 0xf8, 0xa7, 0x0d, 0x6b, 0x22, 0x54, 0x95,
    0xd8, 0x80, 0xb5, 0xf4, 0xcf, 0x2e, 0xdc, 0x6e, 0x64, 0xab, 0x7e, 0xae,
    0xa4, 0xec, 0x32, 0x15, 0x68, 0xfd, 0x1a, 0x2d, 0xc4, 0x8d, 0xcf, 0x5d,
    0xde, 0xc0, 0xbb, 0xf9, 0x21, 0xd1, 0xa9, 0x7a, 0xc4, 0x00, 0x9a, 0x30,
    0xf3, 0xfe, 0xed, 0x46, 0x95, 0x7e,
];

thread_local! {
    // Isolate 内 JWT 缓存：`(token, 签发时间戳)`。
    static JWT_CACHE: RefCell<Option<(String, u64)>> = const { RefCell::new(None) };
}

/// 当前秒级 Unix 时间戳。
pub fn unix_now() -> u64 {
    Date::now().as_millis() / 1000
}

/// 签发一枚 ES256 JWT（对 `header.payload` 做 SHA-256 后 ECDSA 签名）。
fn mint_jwt(now: u64) -> Result<String, Error> {
    let key = SigningKey::from_pkcs8_der(APNS_KEY_DER)
        .map_err(|e| Error::RustError(format!("APNs key parse failed: {e}")))?;
    let header = format!(r#"{{"alg":"ES256","kid":"{APNS_KEY_ID}"}}"#);
    let claims = format!(r#"{{"iss":"{APNS_TEAM_ID}","iat":{now}}}"#);
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(claims)
    );
    let digest: [u8; 32] = Sha256::digest(signing_input.as_bytes()).into();
    let (signature, _) = key
        .sign_prehash(&digest)
        .map_err(|e| Error::RustError(format!("ES256 signing failed: {e}")))?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

/// 获取（或复用）APNs Provider Token。
///
/// 有效期内的 token 直接复用，避免每次请求重新签名触发 429。
fn auth_token(now: u64) -> Result<String, Error> {
    if let Some((token, issued_at)) = JWT_CACHE.with(|cache| cache.borrow().clone())
        && now.saturating_sub(issued_at) < JWT_TTL_SECS
    {
        return Ok(token);
    }
    let token = mint_jwt(now)?;
    JWT_CACHE.with(|cache| *cache.borrow_mut() = Some((token.clone(), now)));
    Ok(token)
}

/// 向 APNs 推送 JSON Body，返回 `(HTTP 状态码, 失败原因文本)`。
pub async fn push(
    device_token: &str,
    body: &str,
    priority: u8,
) -> Result<(u16, String), Error> {
    let token = auth_token(unix_now())?;
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
