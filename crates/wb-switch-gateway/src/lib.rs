//! wb-switch-gateway —— workbuddy2api 网关内核（Rust 版）。
//!
//! 目标是替代原先由 Go 编译的 `gateway.exe`，通过 `build.rs` 内嵌进
//! wb-switch 桌面端 / CLI 分发。
//!
//! # 模块与 Go 侧对应关系
//!
//! | 本模块      | Go 源文件                        |
//! |-------------|----------------------------------|
//! | `config`    | `cmd/server/config.go`           |
//! | `auth`      | `internal/auth/auth.go`          |
//! | `pool`      | `internal/pool/pool.go`          |
//! | `upstream`  | `internal/upstream/*.go`         |
//! | `session`   | `internal/session/session.go`    |
//! | `server`    | `internal/server/handler.go`     |
//! | `logging`   | `internal/server/logging.go`     |
//! | `scheduler` | `internal/scheduler/*.go`        |
//!
//! # 迁移策略
//!
//! 逐模块替换，每个模块都保留与 Go 版**完全一致**的对外行为（HTTP 响应体、
//! 状态机语义、日志口径），并用同一套用例做 A/B 对照。

pub mod auth;
pub mod config;
pub mod error;
pub mod pool;
pub mod server;
pub mod upstream;

pub use error::{GatewayError, Result};
