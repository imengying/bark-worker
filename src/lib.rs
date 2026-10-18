//! 极简 Bark 推送服务端（Rust + WebAssembly on Cloudflare Workers）。
//!
//! 模块划分：
//! - [`mod@app`]：HTTP 路由、参数合并、白名单鉴权与统一 JSON 响应（`AppError`）。
//! - [`mod@apns`]：ES256 JWT 签名（50 分钟缓存）与 APNs HTTP/2 推送。
//! - [`mod@payload`]：`PushPayload` 参数模型与 APNs JSON Body 组装。

mod apns;
mod app;
mod payload;

use worker::{event, Context, Env, Request, Response};

/// Worker 入口：`fetch` 事件，分发到 [`app`] 的路由器。
#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response, worker::Error> {
    app::router().run(req, env).await
}
