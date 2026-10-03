//! client 形态：生命周期命令与 gh 子命令转发（§5.3、§7.2、§7.3）

use crate::err::{client_message, Code};
use crate::ipc::{self, Request, Response};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const IPC_TIMEOUT: Duration = Duration::from_secs(120);

fn call_or_exit(sock: &Path, req: Request) -> Response {
    match ipc::call(sock, &req, IPC_TIMEOUT) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("{}", client_message(Code::DaemonNotRunning, None));
            std::process::exit(1);
        }
    }
}

/// ghpat start（§5.3）
pub fn start(sock: &Path, foreground: bool) -> i32 {
    if foreground {
        // §7.2：当前进程直接进入 daemon 模式，公钥打印 stdout，不 fork
        std::env::set_var("GHPAT_SOCK", sock);
        return crate::daemon::run_internal();
    }

    // 1. 校验/创建父目录（回退 /tmp 场景）
    if let Err(e) = ipc::ensure_sock_dir(sock) {
        eprintln!("✘ socket 目录校验失败: {e}");
        return 1;
    }

    // 2. 残留/已运行检测
    if sock.exists() {
        if ipc::probe_connect(sock) {
            eprintln!("{}", client_message(Code::AlreadyRunning, None));
            return 1;
        }
        let _ = std::fs::remove_file(sock); // 残留清理
    }

    // 3. 匿名管道 + fork/exec 自身
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        eprintln!("✘ 创建管道失败");
        return 1;
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("✘ 无法定位自身二进制: {e}");
            return 1;
        }
    };
    let child = unsafe {
        libc::fork()
    };
    if child < 0 {
        eprintln!("✘ fork 失败");
        return 1;
    }
    if child == 0 {
        // 子进程：把管道写端复制到 stdout，exec 自身
        unsafe {
            libc::dup2(fds[1], 1);
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        let mut env_map: Vec<(String, String)> = std::env::vars().collect();
        env_map.push(("GHPAT_SOCK".into(), sock.to_string_lossy().into_owned()));
        for (k, v) in env_map {
            std::env::set_var(&k, &v);
        }
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("--daemon-internal");
        let err = cmd.exec();
        eprintln!("ERR exec 失败: {err}");
        std::process::exit(127);
    }
    // 父进程：关闭写端，阻塞读（5s 超时）
    unsafe { libc::close(fds[1]) };
    use std::os::unix::io::FromRawFd;
    // SAFETY: fds[0] 为本进程独占的管道读端，交给 File 管理
    let file = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let reader = std::io::BufReader::new(file);

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut line = String::new();
        let mut r = reader;
        match r.read_line(&mut line) {
            Ok(_) => {
                let _ = tx.send(line);
            }
            Err(e) => {
                let _ = tx.send(format!("ERR {e}"));
            }
        }
    });
    let msg = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(m) => m,
        Err(_) => {
            eprintln!("{}", client_message(Code::StartTimeout, None));
            return 1;
        }
    };
    let msg = msg.trim().to_string();
    if let Some(pubkey) = msg.strip_prefix("OK ") {
        println!("{pubkey}");
        println!("daemon 已启动 (pid {child}, socket: {})", sock.display());
        println!("状态: READY（等待 token 注入）");
        0
    } else {
        eprintln!("✘ daemon 启动失败: {}", msg.trim_start_matches("ERR "));
        1
    }
}

/// ghpat set-token <token.enc | ->
pub fn set_token(sock: &Path, file: &str) -> i32 {
    let mut bytes = Vec::new();
    if file == "-" {
        if std::io::stdin().read_to_end(&mut bytes).is_err() {
            eprintln!("✘ 读取 stdin 失败");
            return 1;
        }
    } else {
        match std::fs::read(PathBuf::from(file)) {
            Ok(b) => bytes = b,
            Err(e) => {
                eprintln!("✘ 读取 {file} 失败: {e}");
                return 1;
            }
        }
    }
    use base64::Engine;
    let enc_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let req = Request {
        id: ipc::next_id(),
        cmd: "set_token".into(),
        enc_b64: Some(enc_b64),
        args: None,
        repo: None,
        host: None,
        protocol: None,
    };
    let resp = call_or_exit(sock, req);
    print_set_token_result(&resp)
}

fn print_set_token_result(resp: &Response) -> i32 {
    if resp.ok {
        let p = resp.payload.as_ref().unwrap();
        let login = p.get("login").and_then(|v| v.as_str()).unwrap_or("?");
        let scopes: Vec<String> = p
            .get("scopes")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let fp = p.get("fingerprint").and_then(|v| v.as_str()).unwrap_or("?");
        println!("✔ 解密成功");
        println!("✔ 验证通过: {login} (scopes: {})", scopes.join(", "));
        println!("✔ fingerprint: {fp}");
        println!("状态: ARMED");
        0
    } else {
        let err = resp.error.as_ref().unwrap();
        match err.code.as_str() {
            "DECRYPT_FAILED" => {
                println!("{}", client_message(Code::DecryptFailed, None));
                println!("（当前状态保持不变）");
            }
            "TOKEN_INVALID" => {
                println!("{}", client_message(Code::TokenInvalid, Some(&err.message)));
                println!("（当前状态保持不变）");
            }
            "NOT_RECIPIENT_FORMAT" => {
                println!("{}", client_message(Code::NotRecipientFormat, None));
                println!("（当前状态保持不变）");
            }
            _ => {
                eprintln!("✘ {}: {}", err.code, err.message);
                println!("（当前状态保持不变）");
            }
        }
        1
    }
}

pub fn stop(sock: &Path) -> i32 {
    let req = Request { id: ipc::next_id(), cmd: "shutdown".into(), enc_b64: None, args: None, repo: None, host: None, protocol: None };
    match ipc::call(sock, &req, Duration::from_secs(10)) {
        Ok(r) if r.ok => {
            println!("已销毁");
            0
        }
        _ => {
            eprintln!("{}", client_message(Code::DaemonNotRunning, None));
            1
        }
    }
}

pub fn status(sock: &Path) -> i32 {
    let req = Request { id: ipc::next_id(), cmd: "status".into(), enc_b64: None, args: None, repo: None, host: None, protocol: None };
    let resp = call_or_exit(sock, req);
    if resp.ok {
        let p = resp.payload.unwrap();
        let state = p.get("state").and_then(|v| v.as_str()).unwrap_or("?");
        let fp = p.get("fingerprint").and_then(|v| v.as_str());
        match state {
            "armed" => println!("状态: ARMED (fingerprint: {})", fp.unwrap_or("?")),
            _ => println!("状态: READY（等待 token 注入）"),
        }
        0
    } else {
        1
    }
}

pub fn pubkey(sock: &Path) -> i32 {
    let req = Request { id: ipc::next_id(), cmd: "pubkey".into(), enc_b64: None, args: None, repo: None, host: None, protocol: None };
    let resp = call_or_exit(sock, req);
    if resp.ok {
        println!("{}", resp.payload.unwrap().get("pubkey").and_then(|v| v.as_str()).unwrap_or(""));
        0
    } else {
        1
    }
}

/// gh 子命令：IPC gh → daemon REST → 输出透传
pub fn gh(sock: &Path, args: Vec<String>, repo: Option<String>) -> i32 {
    let req = Request { id: ipc::next_id(), cmd: "gh".into(), enc_b64: None, args: Some(args), repo, host: None, protocol: None };
    let resp = call_or_exit(sock, req);
    if resp.ok {
        let p = resp.payload.unwrap();
        if let Some(s) = p.get("stdout").and_then(|v| v.as_str()) {
            print!("{s}");
        }
        if let Some(s) = p.get("stderr").and_then(|v| v.as_str()) {
            eprint!("{s}");
        }
        p.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0) as i32
    } else {
        let err = resp.error.unwrap();
        if err.code == "NO_TOKEN" {
            eprintln!("{}", client_message(Code::NoToken, None));
        } else {
            eprintln!("✘ {}: {}", err.code, err.message);
        }
        1
    }
}

/// 仓库解析（§7.3）：--repo > GH_REPO env > git remote get-url origin
pub fn resolve_repo(explicit: Option<&str>) -> Result<Option<String>, (Code, String)> {
    if let Some(r) = explicit {
        return Ok(Some(normalize_repo(r)));
    }
    if let Ok(r) = std::env::var("GH_REPO") {
        if !r.is_empty() {
            return Ok(Some(normalize_repo(&r)));
        }
    }
    // 本地解析 cwd 的 origin
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let url = String::from_utf8_lossy(&o.stdout).trim().to_string();
            parse_remote_url(&url).map(Some).ok_or((
                Code::RemoteNotHttps,
                url,
            ))
        }
        _ => Ok(None), // 非 git 仓库且命令未指定 repo → 交由 daemon 报错
    }
}

fn normalize_repo(s: &str) -> String {
    s.trim_start_matches("https://github.com/")
        .trim_end_matches(".git")
        .to_string()
}

/// https://github.com/o/r(.git) → o/r；SSH URL → None
fn parse_remote_url(url: &str) -> Option<String> {
    let u = url.trim();
    if let Some(rest) = u.strip_prefix("https://github.com/") {
        let rest = rest.trim_end_matches(".git");
        if rest.split('/').count() >= 2 {
            return Some(rest.to_string());
        }
        return None;
    }
    if u.starts_with("git@") || u.starts_with("ssh://") {
        return None; // REMOTE_NOT_HTTPS
    }
    None
}

#[allow(dead_code)]
fn unused(v: &Value) {
    let _ = v;
}
