//! 网关内核共享错误类型。
//!
//! 与 Go 侧口径对齐：Go 侧大量使用 `fmt.Errorf` + `errors.As` 做分类判定，
//! Rust 侧统一收敛为一个 thiserror 枚举，避免调用点丢失分类信息。

use thiserror::Error;

/// 网关内核错误。
#[derive(Debug, Error)]
pub enum GatewayError {
    /// 配置文件读取/解析失败。
    #[error("read config: {0}")]
    ConfigRead(String),

    /// 配置解析失败（JSON 语法错误）。
    #[error("parse config: {0}")]
    ConfigParse(String),

    /// 配置项取值非法（例如 duration 字符串不可解析、小时越界）。
    #[error("invalid config: {0}")]
    ConfigInvalid(String),

    /// 凭证文件解析失败。
    #[error("auth parse {path}: {msg}")]
    AuthParse { path: String, msg: String },

    /// 凭证文件写回（原子写）失败。
    #[error("auth save {path}: {msg}")]
    AuthSave { path: String, msg: String },

    /// 上游 HTTP 传输层错误（连接失败、超时、TLS 等）。
    ///
    /// 对应 Go 侧的 transport error：只换号不喂熔断。
    #[error("upstream transport: {0}")]
    Transport(String),

    /// 上游返回了无法解析的响应体。
    #[error("upstream parse: {0}")]
    UpstreamParse(String),

    /// 无可用账号（全冷却/禁用/占满）。
    #[error("all accounts unavailable (cooling/disabled)")]
    NoHealthyAccount,

    /// 状态持久化失败。
    #[error("state persist: {0}")]
    StatePersist(String),

    /// Redis / 远端快照读写失败。v0 阶段 Redis 尚未接入，先预留。
    #[error("redis: {0}")]
    Redis(String),

    /// 其他未分类的内部错误。
    #[error("{0}")]
    Other(String),
}

/// `Result` 别名，内核统一使用。
pub type Result<T> = std::result::Result<T, GatewayError>;

impl GatewayError {
    /// 把任意 `std::io::Error` 包装为 [`GatewayError::Other`]。
    pub fn io(e: std::io::Error) -> Self {
        GatewayError::Other(e.to_string())
    }
}
