//! IPC 协议（规格 §6）：JSON Lines over Unix socket，一问一答

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub const SOCK_FILE: &str = "ghpat.sock";
pub const LOG_FILE: &str = "ghpat.log";

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enc_b64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    pub fn ok(id: u64, payload: Value) -> Self {
        Response { id, ok: true, payload: Some(payload), error: None }
    }
    pub fn err(id: u64, code: &str, message: impl Into<String>) -> Self {
        Response {
            id,
            ok: false,
            payload: None,
            error: Some(ErrorBody { code: code.into(), message: message.into() }),
        }
    }
}

pub fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

/// socket 路径解析（§5.3.1、§6.1）：--sock > GHPAT_SOCK > XDG_RUNTIME_DIR > /tmp/ghpat-$UID
pub fn resolve_sock(override_path: Option<&Path>) -> std::path::PathBuf {
    if let Some(p) = override_path {
        return p.to_path_buf();
    }
    if let Ok(s) = std::env::var("GHPAT_SOCK") {
        if !s.is_empty() {
            return s.into();
        }
    }
    sock_dir_default().join(SOCK_FILE)
}

/// 默认 sock 目录：${XDG_RUNTIME_DIR:-/tmp/ghpat-$UID}
pub fn sock_dir_default() -> std::path::PathBuf {
    if let Ok(x) = std::env::var("XDG_RUNTIME_DIR") {
        if !x.is_empty() {
            return std::path::PathBuf::from(x);
        }
    }
    let uid = unsafe { libc::getuid() };
    std::path::PathBuf::from(format!("/tmp/ghpat-{uid}"))
}

/// 回退到 /tmp 时校验/创建父目录（§5.3.1）：归属当前 UID 且 0700
pub fn ensure_sock_dir(path: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        match std::fs::metadata(dir) {
            Ok(md) => {
                let uid = unsafe { libc::getuid() };
                if md.uid() != uid {
                    return Err(format!("目录 {} 不属于当前 UID", dir.display()));
                }
                if md.permissions().mode() & 0o777 != 0o700 {
                    // 已存在且归属当前 UID：自动收紧为 0700
                    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                        .map_err(|e| format!("收紧 {} 权限失败: {e}", dir.display()))?;
                }
            }
            Err(_) => {
                std::fs::create_dir(dir).map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

/// client 侧 IPC 调用：连接、发送一行请求、读取一行响应
pub fn call(sock: &Path, req: &Request, timeout: Duration) -> Result<Response, std::io::Error> {
    use std::io::{BufRead, BufReader, Write};

    let mut stream = UnixStream::connect(sock)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    if buf.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "daemon 关闭连接",
        ));
    }
    serde_json::from_str(&buf).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("响应解析失败: {e}"))
    })
}

/// 探测 socket 是否可连接（用于 start 的残留检测与 DAEMON_ALREADY_RUNNING）
pub fn probe_connect(sock: &Path) -> bool {
    UnixStream::connect(sock).is_ok()
}
