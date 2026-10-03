//! 错误码定义（规格 §9）

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    DaemonNotRunning,
    SocketStale,
    AlreadyRunning,
    NoToken,
    DecryptFailed,
    TokenInvalid,
    TokenExpired,
    ApiError,
    NotRecipientFormat,
    RemoteNotHttps,
    UidMismatch,
    StartTimeout,
    HostNotAllowed,
    PatTooLong,
    Io,
}

impl Code {
    pub fn as_str(&self) -> &'static str {
        match self {
            Code::DaemonNotRunning => "DAEMON_NOT_RUNNING",
            Code::SocketStale => "SOCKET_STALE",
            Code::AlreadyRunning => "DAEMON_ALREADY_RUNNING",
            Code::NoToken => "NO_TOKEN",
            Code::DecryptFailed => "DECRYPT_FAILED",
            Code::TokenInvalid => "TOKEN_INVALID",
            Code::TokenExpired => "TOKEN_EXPIRED",
            Code::ApiError => "API_ERROR",
            Code::NotRecipientFormat => "NOT_RECIPIENT_FORMAT",
            Code::RemoteNotHttps => "REMOTE_NOT_HTTPS",
            Code::UidMismatch => "UID_MISMATCH",
            Code::StartTimeout => "START_TIMEOUT",
            Code::HostNotAllowed => "HOST_NOT_ALLOWED",
            Code::PatTooLong => "PAT_TOO_LONG",
            Code::Io => "IO_ERROR",
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// client 侧错误呈现（§9 表）
pub fn client_message(code: Code, detail: Option<&str>) -> String {
    match code {
        Code::DaemonNotRunning => "✘ daemon 未运行，请先执行 ghpat start".into(),
        Code::SocketStale => "✘ 残留 socket 已自动清理，请重试".into(),
        Code::AlreadyRunning => "✘ daemon 已在运行（DAEMON_ALREADY_RUNNING）".into(),
        Code::NoToken => "✘ PAT 未注入，请执行 ghpat set-token".into(),
        Code::DecryptFailed => "✘ 解密失败：密文与公钥不匹配或已损坏".into(),
        Code::TokenInvalid => format!(
            "✘ token 无效（401），注入被拒绝；保留原状态{}",
            detail.map(|d| format!("：{d}")).unwrap_or_default()
        ),
        Code::NotRecipientFormat => "✘ 仅支持 age -r 公钥加密（NOT_RECIPIENT_FORMAT）".into(),
        Code::RemoteNotHttps => "✘ ghpat 仅支持 HTTPS remote".into(),
        Code::StartTimeout => "✘ daemon 启动超时，exit(1)".into(),
        _ => format!("✘ {}{}", code, detail.map(|d| format!(": {d}")).unwrap_or_default()),
    }
}
