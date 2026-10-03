//! ghpat 主入口（规格 §4.1）：按 argv 分叉三种形态，不使用环境变量做形态判断

mod agekey;
mod client;
mod cred;
mod daemon;
mod err;
mod gh;
mod ipc;
mod jq;
mod page;
mod wrap;

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "ghpat",
    version,
    about = "面向 AI 智能体的 GitHub PAT 内存代理"
)]
struct Cli {
    /// 覆盖 socket 路径（测试隔离用；默认 ${XDG_RUNTIME_DIR:-/tmp/ghpat-$UID}/ghpat.sock）
    #[arg(long, global = true, value_name = "PATH")]
    sock: Option<PathBuf>,
    /// 详细日志（get_pat 响应以密码掩码形式打印）
    #[arg(long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// 启动 daemon（fork 子进程；--foreground 时当前进程直接进入 daemon 模式）
    Start {
        #[arg(long)]
        foreground: bool,
    },
    /// 注入 token.enc（age 公钥加密的 PAT）
    #[command(name = "set-token")]
    SetToken {
        /// 加密文件路径，或 - 表示 stdin
        file: String,
    },
    /// 销毁 daemon（zeroize 整页 + unlink socket + exit）
    Stop,
    /// 显示运行状态与指纹
    Status,
    /// 打印当前 daemon 的 age 公钥
    Pubkey,
    /// 以注入凭据的环境执行命令
    Wrap {
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// GitHub API 透传
    Api {
        endpoint: String,
        #[arg(long, default_value = "GET")]
        method: String,
        #[arg(long = "field", value_name = "KEY=VALUE")]
        fields: Vec<String>,
        #[arg(long)]
        jq: Option<String>,
    },
    /// 仓库操作
    Repo {
        #[command(subcommand)]
        sub: RepoCmd,
    },
    /// Pull Request 操作
    Pr {
        #[command(subcommand)]
        sub: PrCmd,
    },
    /// Issue 操作
    Issue {
        #[command(subcommand)]
        sub: IssueCmd,
    },
    /// 认证状态
    #[command(name = "auth")]
    Auth {
        #[command(subcommand)]
        sub: AuthCmd,
    },
}

#[derive(clap::Subcommand)]
enum RepoCmd {
    View {
        #[arg(value_name = "REPO")]
        repo: Option<String>,
        #[arg(long = "repo", short = 'R')]
        repo_flag: Option<String>,
    },
    List {
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long = "repo", short = 'R')]
        repo_flag: Option<String>,
    },
}

#[derive(clap::Subcommand)]
enum PrCmd {
    List {
        #[arg(long, default_value = "open")]
        state: String,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long = "repo", short = 'R')]
        repo_flag: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        jq: Option<String>,
    },
    View {
        number: u64,
        #[arg(long = "repo", short = 'R')]
        repo_flag: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        jq: Option<String>,
    },
    Create {
        #[arg(long)]
        title: String,
        #[arg(long)]
        head: String,
        #[arg(long)]
        base: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "repo", short = 'R')]
        repo_flag: Option<String>,
    },
    Merge {
        number: u64,
        #[arg(long)]
        merge: bool,
        #[arg(long)]
        squash: bool,
        #[arg(long)]
        rebase: bool,
        #[arg(long = "repo", short = 'R')]
        repo_flag: Option<String>,
    },
}

#[derive(clap::Subcommand)]
enum IssueCmd {
    List {
        #[arg(long, default_value = "open")]
        state: String,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long = "repo", short = 'R')]
        repo_flag: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        jq: Option<String>,
    },
    View {
        number: u64,
        #[arg(long = "repo", short = 'R')]
        repo_flag: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        jq: Option<String>,
    },
    Create {
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "repo", short = 'R')]
        repo_flag: Option<String>,
    },
}

#[derive(clap::Subcommand)]
enum AuthCmd {
    Status,
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    // §4.1：形态按 argv 分叉
    if argv.get(1).map(|s| s.as_str()) == Some("cred-helper") {
        std::process::exit(cred::run(&argv[1..]));
    }
    if argv.get(1).map(|s| s.as_str()) == Some("--daemon-internal") {
        std::process::exit(daemon::run_internal());
    }

    let cli = Cli::parse();
    let sock = ipc::resolve_sock(cli.sock.as_deref());
    let code = match cli.cmd {
        Cmd::Start { foreground } => client::start(&sock, foreground),
        Cmd::SetToken { file } => client::set_token(&sock, &file),
        Cmd::Stop => client::stop(&sock),
        Cmd::Status => client::status(&sock),
        Cmd::Pubkey => client::pubkey(&sock),
        Cmd::Wrap { command } => wrap::run(&sock, command),
        Cmd::Api { endpoint, method, fields, jq } => {
            let args = build_api_args(&endpoint, &method, &fields, &jq, None);
            client::gh(&sock, args, None)
        }
        Cmd::Repo { sub } => match sub {
            RepoCmd::View { repo, repo_flag } => {
                match client::resolve_repo(repo_flag.as_deref().or(repo.as_deref())) {
                    Err((code, detail)) => {
                        eprintln!("{}", err::client_message(code, Some(&detail)));
                        1
                    }
                    Ok(r) => {
                        let args = vec!["repo".into(), "view".into()];
                        client::gh(&sock, args, r)
                    }
                }
            }
            RepoCmd::List { limit, repo_flag: _ } => {
                let args = vec!["repo".into(), "list".into(), "--limit".into(), limit.to_string()];
                client::gh(&sock, args, None)
            }
        },
        Cmd::Pr { sub } => match sub {
            PrCmd::List { state, limit, repo_flag, json, jq } => {
                match client::resolve_repo(repo_flag.as_deref()) {
                    Err((code, detail)) => {
                        eprintln!("{}", err::client_message(code, Some(&detail)));
                        1
                    }
                    Ok(r) => {
                        let mut args = vec![
                            "pr".into(),
                            "list".into(),
                            "--state".into(),
                            state,
                            "--limit".into(),
                            limit.to_string(),
                        ];
                        if json {
                            args.push("--json".into());
                        }
                        if let Some(q) = jq {
                            args.push("--jq".into());
                            args.push(q.clone());
                        }
                        client::gh(&sock, args, r)
                    }
                }
            }
            PrCmd::View { number, repo_flag, json, jq } => {
                match client::resolve_repo(repo_flag.as_deref()) {
                    Err((code, detail)) => {
                        eprintln!("{}", err::client_message(code, Some(&detail)));
                        1
                    }
                    Ok(r) => {
                        let mut args = vec!["pr".into(), "view".into(), number.to_string()];
                        if json {
                            args.push("--json".into());
                        }
                        if let Some(q) = jq {
                            args.push("--jq".into());
                            args.push(q.clone());
                        }
                        client::gh(&sock, args, r)
                    }
                }
            }
            PrCmd::Create { title, head, base, body, repo_flag } => {
                match client::resolve_repo(repo_flag.as_deref()) {
                    Err((code, detail)) => {
                        eprintln!("{}", err::client_message(code, Some(&detail)));
                        1
                    }
                    Ok(r) => {
                        let mut args = vec![
                            "pr".into(),
                            "create".into(),
                            "--title".into(),
                            title,
                            "--head".into(),
                            head,
                            "--base".into(),
                            base,
                        ];
                        if let Some(b) = body {
                            args.push("--body".into());
                            args.push(b);
                        }
                        client::gh(&sock, args, r)
                    }
                }
            }
            PrCmd::Merge { number, merge, squash, rebase, repo_flag } => {
                match client::resolve_repo(repo_flag.as_deref()) {
                    Err((code, detail)) => {
                        eprintln!("{}", err::client_message(code, Some(&detail)));
                        1
                    }
                    Ok(r) => {
                        let mut args = vec!["pr".into(), "merge".into(), number.to_string()];
                        if squash {
                            args.push("--squash".into());
                        } else if rebase {
                            args.push("--rebase".into());
                        } else if merge {
                            args.push("--merge".into());
                        }
                        client::gh(&sock, args, r)
                    }
                }
            }
        },
        Cmd::Issue { sub } => match sub {
            IssueCmd::List { state, limit, repo_flag, json, jq } => {
                match client::resolve_repo(repo_flag.as_deref()) {
                    Err((code, detail)) => {
                        eprintln!("{}", err::client_message(code, Some(&detail)));
                        1
                    }
                    Ok(r) => {
                        let mut args = vec![
                            "issue".into(),
                            "list".into(),
                            "--state".into(),
                            state,
                            "--limit".into(),
                            limit.to_string(),
                        ];
                        if json {
                            args.push("--json".into());
                        }
                        if let Some(q) = jq {
                            args.push("--jq".into());
                            args.push(q.clone());
                        }
                        client::gh(&sock, args, r)
                    }
                }
            }
            IssueCmd::View { number, repo_flag, json, jq } => {
                match client::resolve_repo(repo_flag.as_deref()) {
                    Err((code, detail)) => {
                        eprintln!("{}", err::client_message(code, Some(&detail)));
                        1
                    }
                    Ok(r) => {
                        let mut args = vec!["issue".into(), "view".into(), number.to_string()];
                        if json {
                            args.push("--json".into());
                        }
                        if let Some(q) = jq {
                            args.push("--jq".into());
                            args.push(q.clone());
                        }
                        client::gh(&sock, args, r)
                    }
                }
            }
            IssueCmd::Create { title, body, repo_flag } => {
                match client::resolve_repo(repo_flag.as_deref()) {
                    Err((code, detail)) => {
                        eprintln!("{}", err::client_message(code, Some(&detail)));
                        1
                    }
                    Ok(r) => {
                        let mut args = vec!["issue".into(), "create".into(), "--title".into(), title];
                        if let Some(b) = body {
                            args.push("--body".into());
                            args.push(b);
                        }
                        client::gh(&sock, args, r)
                    }
                }
            }
        },
        Cmd::Auth { sub } => match sub {
            AuthCmd::Status => {
                client::gh(&sock, vec!["auth".into(), "status".into()], None)
            }
        },
    };
    std::process::exit(code);
}

fn build_api_args(
    endpoint: &str,
    method: &str,
    fields: &[String],
    jq: &Option<String>,
    _extra: Option<()>,
) -> Vec<String> {
    let mut args = vec!["api".to_string(), endpoint.to_string(), "--method".to_string(), method.to_string()];
    for f in fields {
        args.push("--field".to_string());
        args.push(f.clone());
    }
    if let Some(q) = jq {
        args.push("--jq".to_string());
        args.push(q.clone());
    }
    args
}
