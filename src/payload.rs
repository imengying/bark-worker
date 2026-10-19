//! 推送参数模型与 APNs JSON Body 组装。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// 解析 `badge`：兼容 JSON 数字与字符串两种形态（Bark 客户端两种都会发）。
fn parse_badge<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("badge number out of range")),
        Some(Value::String(s)) => s
            .trim()
            .parse::<u32>()
            .map(Some)
            .map_err(|_| serde::de::Error::custom("badge is not a valid number")),
        Some(_) => Err(serde::de::Error::custom("badge must be a number")),
    }
}

/// 推送请求参数。
///
/// - 显式字段对 Bark 常用参数建模；
/// - `extra` 通过 `#[serde(flatten)]` 捕获其余扩展参数
///   （`icon` / `group` / `url` / `copy` / `autoCopy` / `call` / `isArchive` 等），
///   并在组装时原样平铺到 APNs 顶层 JSON。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushPayload {
    #[serde(default, alias = "devicekey", alias = "key")]
    pub device_key: Option<String>,
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub body: Option<String>,
    /// 通知声音，默认 `1107`。
    pub sound: Option<String>,
    /// App 角标数字（序列化为 JSON 数字，兼容字符串传入）。
    #[serde(default, deserialize_with = "parse_badge")]
    pub badge: Option<u32>,
    /// 中断级别：`active` / `timeSensitive` / `critical` / `passive`。
    pub level: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl PushPayload {
    /// 合并覆盖：`other` 中已显式设置的字段优先（用于 Query 先行、JSON Body 覆盖）。
    pub fn merge(&mut self, other: PushPayload) {
        let PushPayload {
            device_key,
            title,
            subtitle,
            body,
            sound,
            badge,
            level,
            extra,
        } = other;
        if device_key.is_some() {
            self.device_key = device_key;
        }
        if title.is_some() {
            self.title = title;
        }
        if subtitle.is_some() {
            self.subtitle = subtitle;
        }
        if body.is_some() {
            self.body = body;
        }
        if sound.is_some() {
            self.sound = sound;
        }
        if badge.is_some() {
            self.badge = badge;
        }
        if level.is_some() {
            self.level = level;
        }
        self.extra.extend(extra);
    }

    /// 将 `level` 参数映射为 Apple 标准的 `interruption-level` 值。
    fn interruption_level(level: Option<&str>) -> &'static str {
        match level.map(str::to_ascii_lowercase).as_deref() {
            Some("passive") => "passive",
            Some("critical") => "critical",
            Some("timesensitive") | Some("time-sensitive") => "time-sensitive",
            _ => "active",
        }
    }

    /// 是否为被动推送（`level == passive`，此时 APNs 优先级降为 5）。
    pub fn is_passive(&self) -> bool {
        matches!(
            self.level.as_deref().map(str::to_ascii_lowercase).as_deref(),
            Some("passive")
        )
    }

    /// 组装发往 APNs 的 JSON Body。
    ///
    /// 扩展参数中的 `ciphertext` / `iv` 会被移动进 `ext`（加密推送载荷）。
    pub fn build_apns_body(&self) -> Result<String, worker::Error> {
        let mut alert = Map::new();
        if let Some(title) = self.title.as_deref().filter(|t| !t.is_empty()) {
            alert.insert("title".into(), json!(title));
        }
        if let Some(subtitle) = self.subtitle.as_deref().filter(|s| !s.is_empty()) {
            alert.insert("subtitle".into(), json!(subtitle));
        }
        alert.insert(
            "body".into(),
            json!(self.body.clone().unwrap_or_default()),
        );

        let sound = self
            .sound
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "1107".to_string());

        let mut aps = Map::new();
        aps.insert("alert".into(), Value::Object(alert));
        aps.insert("sound".into(), json!(sound));
        aps.insert("badge".into(), json!(self.badge.unwrap_or(1)));
        aps.insert("mutable-content".into(), json!(1));
        aps.insert(
            "interruption-level".into(),
            json!(Self::interruption_level(self.level.as_deref())),
        );

        let mut root = Map::new();
        root.insert("aps".into(), Value::Object(aps));

        // 加密推送（端到端加密）载荷放入 ext，其余扩展参数平铺到顶层；
        // 服务端参数（device_key / device_token 及其别名）不下发 APNs。
        // 注意 device_token 在结构体上已无对应字段，会落入 extra，
        // 必须在这里挡掉，否则会随载荷泄漏给 APNs。
        let mut ext = Map::new();
        for (key, value) in &self.extra {
            if key == "ciphertext" || key == "iv" {
                if !value.is_null() {
                    ext.insert(key.clone(), value.clone());
                }
                continue;
            }
            if matches!(
                key.as_str(),
                "device_key" | "devicekey" | "key" | "device_token" | "devicetoken"
            ) {
                continue;
            }
            root.insert(key.clone(), value.clone());
        }
        if !ext.is_empty() {
            root.insert("ext".into(), Value::Object(ext));
        }

        serde_json::to_string(&Value::Object(root))
            .map_err(|e| worker::Error::RustError(format!("APNs body encode failed: {e}")))
    }
}

/// 设备注册响应的 `data` 字段。
#[derive(Debug, Serialize)]
pub struct RegisterData<'a> {
    pub key: &'a str,
    pub device_key: &'a str,
    pub device_token: &'a str,
}
