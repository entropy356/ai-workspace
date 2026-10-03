//! daemon 形态（规格 §5、§6、§8）

use crate::err::Code;
use crate::gh::{ApiCtx, GhResult};
use crate::ipc::{Request, Response, LOG_FILE};
use crate::page::{fingerprint, SensitivePage};
use serde_json::json;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

pub struct Meta {
    pub pubkey: String,
    pub login: Option<String>,
    pub scopes: Vec<String>,
    pub fingerprint: Option<String>,
}

pub struct DaemonState {
    pub page: Mutex<SensitivePage>,
    pub meta: Mutex<Meta>,
    pub client: reqwest::Client,
    pub sock_path: PathBuf,
}

/// 日志（§7.2）：仅错误与状态变更；超 1MB 截断保留后半
pub fn log_line(sock_path: &PathBuf, msg: &str) {
    let Some(dir) = sock_path.parent() else { return };
    let path = dir.join(LOG_FILE);
    let _ = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|f| {
            use std::os::unix::fs::MetadataExt;
            use std::os::unix::fs::PermissionsExt;
            if let Ok(md) = f.metadata() {
                if md.size() > 1024 * 1024 {
                    drop(f);
                    // 截断保留后半
                    if let Ok(all) = std::fs::read(&path) {
                        let keep = &all[all.len() / 2..];
                        let _ = std::fs::write(&path, keep);
                    }
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                    return std::fs::File::options().create(true).append(true).open(&path);
                }
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
            Ok(f)
        })
        .and_then(|mut f| {
            use std::time::{SystemTime, UNIX_EPOCH};
            let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            writeln!(f, "[{ts}] {msg}")
        });
}

/// daemon 子进程入口（argv[1] == "--daemon-internal"，§5.3.4）
/// 返回退出码；stdout 为与父进程通信的管道（OK <公钥> / ERR <原因>）
pub fn run_internal() -> i32 {
    let sock_path: PathBuf = match std::env::var("GHPAT_SOCK") {
        Ok(s) if !s.is_empty() => s.into(),
        _ => {
            eprintln!("ERR missing GHPAT_SOCK");
            return 1;
        }
    };

    // a. umask 先行，bind 创建即 0600（消除 bind→chmod TOCTOU）
    unsafe { libc::umask(0o077) };

    // c/d. 关 core dump + 分配敏感页（在 bind 前完成敏感初始化次序无影响，
    // 但 PR_SET_DUMPABLE 要在创建任何敏感数据前设置）
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };

    let page = match SensitivePage::new() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ERR mmap/mlock 失败: {e}");
            return 1;
        }
    };

    // 生成 age 密钥对；私钥原始字节写入 SensitivePage
    let (_identity, raw_identity, pubkey0) = match crate::agekey::generate() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ERR identity 生成失败: {e}");
            return 1;
        }
    };
    page.set_identity(&raw_identity);
    let pubkey = pubkey0;

    // b. tokio 运行时
    let rt = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ERR tokio 初始化失败: {e}");
            return 1;
        }
    };

    rt.block_on(async move {
        // e. bind + listen
        // 先清理残留 socket 文件（父进程已做过一次，此处兜底）
        let _ = std::fs::remove_file(&sock_path);
        let listener = match UnixListener::bind(&sock_path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("ERR bind 失败: {e}");
                return 1;
            }
        };

        // f. 就绪消息
        let mut out = std::io::stdout();
        let _ = writeln!(out, "OK {pubkey}");
        let _ = out.flush();

        // 后台日志重定向：daemon 后续错误写 ghpat.log
        log_line(&sock_path, "daemon started (READY)");

        let client = reqwest::Client::builder()
            .user_agent("ghpat")
            .build()
            .expect("reqwest client");
        let state = Arc::new(DaemonState {
            page: Mutex::new(page),
            meta: Mutex::new(Meta { pubkey, login: None, scopes: Vec::new(), fingerprint: None }),
            client,
            sock_path: sock_path.clone(),
        });

        // 信号处理：SIGINT/SIGTERM → 销毁退出（§5.4）
        {
            let state = state.clone();
            tokio::spawn(async move {
                use tokio::signal::unix::{signal, SignalKind};
                let mut term = signal(SignalKind::terminate()).expect("sigterm");
                let mut int = signal(SignalKind::interrupt()).expect("sigint");
                tokio::select! {
                    _ = term.recv() => {},
                    _ = int.recv() => {},
                }
                log_line(&state.sock_path, "signal received, destroying");
                destroy(&state);
            });
        }

        // g. 事件循环
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let state = state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(stream, state).await {
                            if e.kind() != std::io::ErrorKind::UnexpectedEof {
                                // 记录但不中断
                            }
                        }
                    });
                }
                Err(e) => {
                    log_line(&sock_path, &format!("accept 失败: {e}"));
                }
            }
        }
    })
}

/// 销毁流程（§5.4）：zeroize 整页 → unlink socket → exit(0)
fn destroy(state: &DaemonState) {
    if let Ok(page) = state.page.lock() {
        page.zeroize_all();
    }
    let _ = std::fs::remove_file(&state.sock_path);
    std::process::exit(0);
}

/// SO_PEERCRED 校验 uid 并取调用方 PID（§8.1）
fn peer_uid_pid(stream: &tokio::net::UnixStream) -> Option<(u32, u32)> {
    unsafe {
        let mut ucred: libc::ucred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let ret = libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut ucred as *mut _ as *mut libc::c_void,
            &mut len,
        );
        if ret != 0 {
            return None;
        }
        Some((ucred.uid, ucred.pid as u32))
    }
}

async fn handle_conn(
    stream: tokio::net::UnixStream,
    state: Arc<DaemonState>,
) -> std::io::Result<()> {
    // §8.1：accept 后校验 uid == daemon uid
    if peer_uid_pid(&stream).map(|(uid, _)| uid) != Some(unsafe { libc::getuid() }) {
        return Ok(()); // UID_MISMATCH：静默断开
    }

    let peer_pid = peer_uid_pid(&stream).map(|(_, pid)| pid);
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader); // tokio::io::BufReader
    let mut buf = String::new();

    loop {
        buf.clear();
        let n = lines.read_line(&mut buf).await?;
        if n == 0 {
            break;
        }
        let req: Request = match serde_json::from_str(buf.trim()) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::err(0, "BAD_REQUEST", format!("请求解析失败: {e}"));
                write_resp(&mut writer, &resp).await?;
                continue;
            }
        };
        let resp = dispatch(&state, &req, peer_pid).await;
        let shutdown = req.cmd == "shutdown" && resp.ok;
        write_resp(&mut writer, &resp).await?;
        if shutdown {
            log_line(&state.sock_path, "shutdown requested, destroying");
            destroy(&state);
        }
    }
    Ok(())
}

async fn write_resp(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    resp: &Response,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(resp)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await
}

async fn dispatch(state: &Arc<DaemonState>, req: &Request, caller_pid: Option<u32>) -> Response {
    match req.cmd.as_str() {
        "status" => cmd_status(state, req).await,
        "pubkey" => cmd_pubkey(state, req),
        "set_token" => cmd_set_token(state, req).await,
        "get_pat" => cmd_get_pat(state, req, caller_pid).await,
        "gh" => cmd_gh(state, req).await,
        "shutdown" => Response::ok(req.id, json!({"ok": true})),
        other => Response::err(req.id, "BAD_REQUEST", format!("未知命令: {other}")),
    }
}

async fn cmd_status(state: &Arc<DaemonState>, req: &Request) -> Response {
    let armed = state.page.lock().unwrap().is_armed();
    let fp = state.meta.lock().unwrap().fingerprint.clone();
    Response::ok(
        req.id,
        json!({
            "state": if armed { "armed" } else { "ready" },
            "fingerprint": fp,
        }),
    )
}

fn cmd_pubkey(state: &Arc<DaemonState>, req: &Request) -> Response {
    let pubkey = state.meta.lock().unwrap().pubkey.clone();
    Response::ok(req.id, json!({"pubkey": pubkey}))
}

/// set_token（§7.2 daemon 侧流程）
async fn cmd_set_token(state: &Arc<DaemonState>, req: &Request) -> Response {
    let Some(enc_b64) = &req.enc_b64 else {
        return Response::err(req.id, "BAD_REQUEST", "缺少 enc_b64");
    };
    use base64::Engine;
    let enc = match base64::engine::general_purpose::STANDARD.decode(enc_b64) {
        Ok(b) => b,
        Err(e) => return Response::err(req.id, "BAD_REQUEST", format!("base64 解码失败: {e}")),
    };

    // 1. 仅接受 Recipients 变体（拒绝 passphrase 加密）
    let decryptor = match age::Decryptor::new(&enc[..]) {
        Ok(age::Decryptor::Recipients(d)) => d,
        Ok(age::Decryptor::Passphrase(_)) => {
            return Response::err(req.id, Code::NotRecipientFormat.as_str(), "仅支持 age -r 公钥加密")
        }
        Err(_) => {
            return Response::err(req.id, Code::DecryptFailed.as_str(), "密文与公钥不匹配或已损坏")
        }
    };

    // 2. 以页内 identity 解密 → 临时缓冲（写入后立即 zeroize）
    let pat_tmp = {
        let page = state.page.lock().unwrap();
        let raw = page.identity_raw();
        let raw_z = zeroize::Zeroizing::new(raw);
        match crate::agekey::identity_from_raw(&raw_z) {
            Ok(id) => {
                let mut buf = Vec::new();
                let mut r = match decryptor.decrypt(std::iter::once(&id as &dyn age::Identity))
                {
                    Ok(r) => r,
                    Err(_) => {
                        return Response::err(
                            req.id,
                            Code::DecryptFailed.as_str(),
                            "密文与公钥不匹配或已损坏",
                        )
                    }
                };
                if std::io::Read::read_to_end(&mut r, &mut buf).is_err() {
                    return Response::err(req.id, Code::DecryptFailed.as_str(), "密文读取失败");
                }
                zeroize::Zeroizing::new(buf)
            }
            Err(e) => return Response::err(req.id, "IO", format!("identity 重建失败: {e}")),
        }
    };

    // PAT 不应包含首尾空白（人类 echo 管道常见尾随换行）
    let pat_str = match std::str::from_utf8(&pat_tmp) {
        Ok(s) => s.trim(),
        Err(_) => return Response::err(req.id, Code::DecryptFailed.as_str(), "明文非 UTF-8"),
    };
    if pat_str.is_empty() {
        return Response::err(req.id, Code::DecryptFailed.as_str(), "明文为空");
    }
    if pat_str.len() > crate::page::PAT_CAP {
        return Response::err(req.id, Code::PatTooLong.as_str(), format!("PAT 超长（>{} 字节）", crate::page::PAT_CAP));
    }

    // 3. GET /user 验证
    let resp = state
        .client
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {pat_str}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => return Response::err(req.id, "API_ERROR", format!("访问 GitHub 失败: {e}")),
    };
    let status = resp.status().as_u16();
    // scopes：Classic PAT 从 X-OAuth-Scopes 头读取；Fine-grained 无此头 → "fine-grained"（§7.2）
    let scopes: Vec<String> = resp
        .headers()
        .get("X-OAuth-Scopes")
        .and_then(|v| v.to_str().ok())
        .map(parse_scopes)
        .unwrap_or_else(|| vec!["fine-grained".to_string()]);
    if status == 401 {
        return Response::err(req.id, Code::TokenInvalid.as_str(), "GitHub 返回 401");
    }
    if status != 200 {
        let msg = resp.json::<serde_json::Value>().await.ok().and_then(|v| {
            v.get("message").and_then(|m| m.as_str()).map(|s| s.to_string())
        }).unwrap_or_default();
        return Response::err(req.id, Code::ApiError.as_str(), format!("GitHub API {status}: {msg}"));
    }
    let user: serde_json::Value = match resp.json().await {
        Ok(u) => u,
        Err(e) => return Response::err(req.id, Code::ApiError.as_str(), format!("响应解析失败: {e}")),
    };
    let login = user.get("login").and_then(|v| v.as_str()).unwrap_or("?").to_string();

    // 4. 验证通过后：原地 zeroize 旧 PAT → 写入新 PAT → 记录指纹
    {
        let page = state.page.lock().unwrap();
        if page.set_pat(pat_str).is_err() {
            return Response::err(req.id, Code::PatTooLong.as_str(), "PAT 超长");
        }
    }
    let fp = fingerprint(pat_str);
    {
        let mut meta = state.meta.lock().unwrap();
        meta.login = Some(login.clone());
        meta.fingerprint = Some(fp.clone());
        meta.scopes = scopes.clone();
    }
    log_line(&state.sock_path, &format!("token set: {login} ({fp})"));
    Response::ok(
        req.id,
        json!({"login": login, "scopes": scopes, "fingerprint": fp}),
    )
}

/// get_pat（§6.3）：仅 cred-helper；host 限定 github.com 域
async fn cmd_get_pat(state: &Arc<DaemonState>, req: &Request, caller_pid: Option<u32>) -> Response {
    let host = req.host.as_deref().unwrap_or("");
    let protocol = req.protocol.as_deref().unwrap_or("");
    if protocol != "https" || !matches!(host, "github.com" | "www.github.com") {
        return Response::err(req.id, Code::HostNotAllowed.as_str(), format!("不为 {host} 代理凭据"));
    }
    let pat = {
        let page = state.page.lock().unwrap();
        page.pat().map(|s| s.to_string()) // IPC 传输必需一次性拷贝（cred-helper 侧 zeroize）
    };
    match pat {
        None => Response::err(req.id, Code::NoToken.as_str(), "PAT 未注入"),
        Some(p) => {
            // 审计日志：记录时间与调用方 PID（§8.1）
            log_line(&state.sock_path, &format!("get_pat for {host} by pid {:?}", caller_pid));
            Response::ok(req.id, json!({"username": "x-access-token", "password": p}))
        }
    }
}

/// gh 子命令（§6.3）：daemon 内执行
async fn cmd_gh(state: &Arc<DaemonState>, req: &Request) -> Response {
    let args = req.args.clone().unwrap_or_default();
    if args.is_empty() {
        return Response::err(req.id, "BAD_REQUEST", "缺少 gh 子命令参数");
    }
    let pat = {
        let page = state.page.lock().unwrap();
        match page.pat() {
            Some(p) => p.to_string(), // gh 模块需要 &str；此处短暂持有
            None => return Response::err(req.id, Code::NoToken.as_str(), "PAT 未注入"),
        }
    };
    let ctx = ApiCtx { client: &state.client, pat: &pat };
    let r: GhResult = gh_exec(&ctx, &args, req.repo.as_deref()).await;
    drop(pat);
    Response::ok(
        req.id,
        json!({"stdout": r.stdout, "stderr": r.stderr, "exit_code": r.exit_code}),
    )
}

fn gh_exec<'a>(
    ctx: &'a ApiCtx<'a>,
    args: &'a [String],
    repo: Option<&'a str>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = GhResult> + Send + 'a>> {
    Box::pin(crate::gh::execute(ctx, args, repo))
}

/// 处理 X-OAuth-Scopes 的辅助（set_token 用；保留以便 Fine-grained 判定）
pub fn parse_scopes(header: &str) -> Vec<String> {
    header.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

