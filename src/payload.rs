//! 推送参数模型与 APNs JSON Body 组装。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// 推送请求参数。
///
/// 字段名即 JSON 键名（不接受其它拼写）。`extra` 通过 `#[serde(flatten)]`
/// 捕获未声明的 Bark 扩展参数，并原样平铺到 APNs 顶层 JSON。
#[derive(Debug, Default, Deserialize)]
pub struct PushPayload {
    pub device_key: Option<String>,
    /// 批量推送目标；指定时忽略 `device_key`。
    pub device_keys: Option<Vec<String>>,
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub body: Option<String>,
    /// 通知声音，默认 `1107`。
    pub sound: Option<String>,
    /// App 角标数字（序列化为 JSON 数字）。
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
            device_keys,
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
        if device_keys.is_some() {
            self.device_keys = device_keys;
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

    /// 将 `level` 映射为 Apple 标准的 `interruption-level` 值。
    fn interruption_level(&self) -> &'static str {
        let Some(level) = self.level.as_deref() else {
            return "active";
        };
        if level.eq_ignore_ascii_case("passive") {
            "passive"
        } else if level.eq_ignore_ascii_case("critical") {
            "critical"
        } else if level.eq_ignore_ascii_case("timesensitive") {
            "time-sensitive"
        } else {
            "active"
        }
    }

    /// 是否为被动推送（`level == passive`，此时 APNs 优先级降为 5）。
    pub fn is_passive(&self) -> bool {
        self.level
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case("passive"))
    }

    /// 组装发往 APNs 的 JSON Body。
    ///
    /// 扩展参数中的 `ciphertext` / `iv` 收进 `ext`（加密推送载荷），其余平铺到顶层。
    pub fn build_apns_body(&self) -> Result<String, worker::Error> {
        let mut alert = Map::new();
        if let Some(title) = self.title.as_deref().filter(|t| !t.is_empty()) {
            alert.insert("title".into(), json!(title));
        }
        if let Some(subtitle) = self.subtitle.as_deref().filter(|s| !s.is_empty()) {
            alert.insert("subtitle".into(), json!(subtitle));
        }
        alert.insert("body".into(), json!(self.body.clone().unwrap_or_default()));

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
            json!(self.interruption_level()),
        );

        let mut root = Map::new();
        root.insert("aps".into(), Value::Object(aps));

        // 服务端参数不随载荷下发。
        let mut ext = Map::new();
        for (key, value) in &self.extra {
            if key == "ciphertext" || key == "iv" {
                if !value.is_null() {
                    ext.insert(key.clone(), value.clone());
                }
                continue;
            }
            if matches!(key.as_str(), "device_key" | "device_keys" | "device_token") {
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
