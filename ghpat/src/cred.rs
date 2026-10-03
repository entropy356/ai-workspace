//! cred-helper 形态（规格 §7.5）：git credential helper 协议

use crate::ipc::{self, Request};
use std::io::{BufRead, Write};
use std::time::Duration;

/// 入口（main 在 argv 分叉后调用）。兼容两种调用布局：
///   直接作为 helper：  argv[1] == "get"
///   wrap 配置方式：    argv[1] == "cred-helper"，argv[2] == "get"
/// 返回进程退出码。
pub fn run(argv: &[String]) -> i32 {
    // 1. argv 检查
    let op = argv.get(1).map(|s| s.as_str()).unwrap_or("");
    let op = if op == "cred-helper" { argv.get(2).map(|s| s.as_str()).unwrap_or("") } else { op };
    match op {
        "get" => {}
        "store" | "erase" => return 0, // 静默成功，不落盘任何凭据
        _ => return 1,
    }

    // 2. stdin 解析：读至空行，提取 protocol 与 host
    let stdin = std::io::stdin();
    let mut protocol = String::new();
    let mut host = String::new();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "protocol" => protocol = v.trim().to_string(),
                "host" => host = v.trim().to_string(),
                _ => {}
            }
        }
    }

    // 3. 域过滤：仅 https + github.com；否则输出空响应退出
    if protocol != "https" || !matches!(host.as_str(), "github.com" | "www.github.com") {
        return 0;
    }

    // 4. 经 IPC 取 PAT
    let sock = ipc::resolve_sock(None);
    let req = Request {
        id: ipc::next_id(),
        cmd: "get_pat".into(),
        enc_b64: None,
        args: None,
        repo: None,
        host: Some(host.clone()),
        protocol: Some(protocol.clone()),
    };
    let resp = match ipc::call(&sock, &req, Duration::from_secs(10)) {
        Ok(r) if r.ok => r,
        // NO_TOKEN 或 daemon 不在：输出空响应（git 将走 prompt / 失败）
        _ => return 0,
    };

    // 5. stdout 输出两行键值对 + 空行；响应缓冲含 PAT，写出后立即 zeroize（§7.5.6）
    let password = resp
        .payload
        .as_ref()
        .and_then(|p| p.get("password"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut out = Vec::new();
    out.extend_from_slice(b"username=x-access-token\npassword=");
    out.extend_from_slice(password.as_bytes());
    out.extend_from_slice(b"\n\n");
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(&out);
    let _ = stdout.flush();
    // 6. 缓冲清理
    for b in out.iter_mut() {
        *b = 0;
    }
    let mut pw = password.into_bytes();
    for b in pw.iter_mut() {
        *b = 0;
    }
    0
}
