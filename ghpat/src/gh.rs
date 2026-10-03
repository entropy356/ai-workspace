//! gh 子命令实现（规格 §7.3）：daemon 内执行，调用 GitHub REST API

use serde_json::{json, Value};

pub struct GhResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl GhResult {
    fn ok(stdout: String) -> Self {
        GhResult { stdout, stderr: String::new(), exit_code: 0 }
    }
    fn fail(stderr: String, code: i32) -> Self {
        GhResult { stdout: String::new(), stderr, exit_code: code }
    }
}

pub struct ApiCtx<'a> {
    pub client: &'a reqwest::Client,
    pub pat: &'a str,
}

const API_BASE: &str = "https://api.github.com";

fn headers(pat: &str) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {pat}")) {
        h.insert(reqwest::header::AUTHORIZATION, v);
    }
    h.insert(reqwest::header::ACCEPT, "application/vnd.github+json".parse().unwrap());
    h.insert(reqwest::header::USER_AGENT, "ghpat".parse().unwrap());
    h.insert("X-GitHub-Api-Version", "2022-11-28".parse().unwrap());
    h
}

/// 单次 API 调用：返回 (status, json)
async fn api_call(
    ctx: &ApiCtx<'_>,
    method: &str,
    url: &str,
    body: Option<&Value>,
) -> Result<(u16, Value), String> {
    let m = method.to_uppercase();
    let mut req = match m.as_str() {
        "GET" => ctx.client.get(url),
        "POST" => ctx.client.post(url),
        "PUT" => ctx.client.put(url),
        "PATCH" => ctx.client.patch(url),
        "DELETE" => ctx.client.delete(url),
        other => return Err(format!("不支持的 HTTP 方法: {other}")),
    };
    req = req.headers(headers(ctx.pat));
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req.send().await.map_err(|e| format!("网络请求失败: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    let val = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text.clone()))
    };
    Ok((status, val))
}

/// 带分页的列表拉取：per_page=min(limit,100)，超出自动翻页（§7.3）
async fn api_list(
    ctx: &ApiCtx<'_>,
    endpoint: &str,
    mut query: Vec<(String, String)>,
    limit: usize,
) -> Result<Vec<Value>, String> {
    let per_page = limit.min(100);
    query.push(("per_page".into(), per_page.to_string()));
    let mut items: Vec<Value> = Vec::new();
    let mut page = 1;
    loop {
        let mut q = query.clone();
        q.push(("page".into(), page.to_string()));
        let qs: Vec<String> = q.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let url = format!("{API_BASE}{endpoint}?{}", qs.join("&"));
        let (status, val) = api_call(ctx, "GET", &url, None).await?;
        if status != 200 {
            return Err(api_err(status, &val));
        }
        let arr = val.as_array().cloned().unwrap_or_default();
        let got = arr.len();
        items.extend(arr);
        if items.len() >= limit || got < per_page {
            break;
        }
        page += 1;
    }
    items.truncate(limit);
    Ok(items)
}

fn api_err(status: u16, val: &Value) -> String {
    let msg = val
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("(无 message)");
    if status == 401 {
        format!("GitHub API 401: {msg}")
    } else {
        format!("GitHub API {status}: {msg}")
    }
}

/// 宽度对齐（按字符计；CJK 字符按 2 列计）
fn display_width(s: &str) -> usize {
    s.chars().map(|c| if (c as u32) > 0x2E7F { 2 } else { 1 }).sum()
}

fn pad(s: &str, w: usize) -> String {
    let mut out = s.to_string();
    for _ in display_width(s)..w {
        out.push(' ');
    }
    out
}

fn render_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| display_width(h)).collect();
    for r in rows {
        for (i, cell) in r.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(display_width(cell));
            }
        }
    }
    let mut out = String::new();
    let hdr: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| pad(h, widths[i]))
        .collect();
    out.push_str(hdr.join("  ").trim_end());
    out.push('\n');
    for r in rows {
        let line: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, c)| pad(c, widths.get(i).copied().unwrap_or(0)))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// gh 表格列定义（验收基准 §11.3）：pr list → NUMBER/TITLE/BRANCH/STATE
fn pr_row(p: &Value) -> Vec<String> {
    vec![
        format!("#{}", p.get("number").and_then(|v| v.as_u64()).unwrap_or(0)),
        p.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        p.pointer("/head/ref").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        p.get("state").and_then(|v| v.as_str()).unwrap_or("").to_uppercase(),
    ]
}

fn issue_row(i: &Value) -> Vec<String> {
    vec![
        format!("#{}", i.get("number").and_then(|v| v.as_u64()).unwrap_or(0)),
        i.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        i.get("state").and_then(|v| v.as_str()).unwrap_or("").to_uppercase(),
    ]
}

/// --json 核心字段集（§7.3）：REST 原生字段映射
fn map_json_field(item: &Value, field: &str) -> Option<Value> {
    match field {
        "number" => item.get("number").cloned(),
        "title" => item.get("title").cloned(),
        "state" => item.get("state").cloned(),
        "headRefName" => item.pointer("/head/ref").cloned(),
        "baseRefName" => item.pointer("/base/ref").cloned(),
        "url" => item.get("html_url").cloned(),
        "author" => item.pointer("/user/login").cloned(),
        "createdAt" => item.get("created_at").cloned(),
        "isDraft" => item.get("draft").cloned(),
        _ => None,
    }
}

const JSON_FIELDS: &[&str] = &[
    "number", "title", "state", "headRefName", "baseRefName", "url", "author", "createdAt",
    "isDraft",
];

fn apply_jq(expr: &str, input: Value) -> Result<Value, String> {
    let outs = crate::jq::apply(expr, &input)?;
    Ok(Value::Array(outs))
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// 入口：执行 gh 子命令（args 已含子命令名，如 ["pr","list",...], repo 为 client 解析结果）
pub async fn execute(
    ctx: &ApiCtx<'_>,
    args: &[String],
    repo: Option<&str>,
) -> GhResult {
    match execute_inner(ctx, args, repo).await {
        Ok(r) => r,
        Err(msg) => {
            let code = if msg.starts_with("GitHub API 401") {
                // TOKEN_EXPIRED 口径：仅透传错误，不清除 PAT（§9）
                1
            } else {
                1
            };
            GhResult::fail(format!("✘ {msg}\n"), code)
        }
    }
}

async fn execute_inner(
    ctx: &ApiCtx<'_>,
    args: &[String],
    repo: Option<&str>,
) -> Result<GhResult, String> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let rest = &args[1..];

    match sub {
        "api" => cmd_api(ctx, rest).await,
        "auth" => cmd_auth(ctx).await,
        "repo" => cmd_repo(ctx, rest, repo).await,
        "pr" => cmd_pr(ctx, rest, repo).await,
        "issue" => cmd_issue(ctx, rest, repo).await,
        other => Err(format!("未知子命令: {other}")),
    }
}

fn need_repo(repo: Option<&str>) -> Result<String, String> {
    repo.map(|r| r.to_string())
        .ok_or_else(|| "无法确定仓库：使用 --repo / GH_REPO，或在 git 仓库内执行".into())
}

fn parse_fields(fields: &[String]) -> Result<Vec<(String, String)>, String> {
    fields
        .iter()
        .map(|f| {
            f.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| format!("--field 格式应为 k=v，得到: {f}"))
        })
        .collect()
}

// ---- ghpat api <endpoint> [--method M] [--field k=v]... [--jq] ----
async fn cmd_api(ctx: &ApiCtx<'_>, rest: &[String]) -> Result<GhResult, String> {
    let mut endpoint: Option<String> = None;
    let mut method = "GET".to_string();
    let mut fields: Vec<String> = Vec::new();
    let mut jq: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--method" | "-X" => {
                i += 1;
                method = rest.get(i).ok_or("--method 缺参数")?.clone();
            }
            "--field" | "-f" => {
                i += 1;
                fields.push(rest.get(i).ok_or("--field 缺参数")?.clone());
            }
            "--jq" => {
                i += 1;
                jq = Some(rest.get(i).ok_or("--jq 缺参数")?.clone());
            }
            other => {
                if endpoint.is_none() {
                    endpoint = Some(other.trim_start_matches('/').to_string());
                }
            }
        }
        i += 1;
    }
    let endpoint = endpoint.ok_or("api 缺少 endpoint 参数")?;
    let pairs = parse_fields(&fields)?;
    let is_getish = method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("DELETE");
    let url = if is_getish || pairs.is_empty() {
        if pairs.is_empty() {
            format!("{API_BASE}/{endpoint}")
        } else {
            let qs: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("{API_BASE}/{endpoint}?{}", qs.join("&"))
        }
    } else {
        format!("{API_BASE}/{endpoint}")
    };
    let body = if is_getish {
        None
    } else {
        let map: serde_json::Map<String, Value> = pairs
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::from_str(v).unwrap_or(Value::String(v.clone()))))
            .collect();
        Some(Value::Object(map))
    };
    let (status, val) = api_call(ctx, &method, &url, body.as_ref()).await?;
    if !(200..300).contains(&status) {
        return Err(api_err(status, &val));
    }
    let out_val = match jq {
        Some(expr) => apply_jq(&expr, val)?,
        None => val,
    };
    Ok(GhResult::ok(pretty(&out_val)))
}

// ---- ghpat auth status ----
async fn cmd_auth(ctx: &ApiCtx<'_>) -> Result<GhResult, String> {
    let (status, val) = api_call(ctx, "GET", &format!("{API_BASE}/user"), None).await?;
    if status == 401 {
        return Err(api_err(status, &val));
    }
    if status != 200 {
        return Err(api_err(status, &val));
    }
    let login = val.get("login").and_then(|v| v.as_str()).unwrap_or("?");
    Ok(GhResult::ok(format!("已认证为 {login}\n")))
}

// ---- repo view / repo list ----
async fn cmd_repo(ctx: &ApiCtx<'_>, rest: &[String], repo: Option<&str>) -> Result<GhResult, String> {
    match rest.first().map(|s| s.as_str()) {
        Some("view") => {
            let target = rest.get(1).cloned().or_else(|| repo.map(|r| r.to_string()));
            let target = target.ok_or("repo view 缺少仓库参数")?;
            let (status, val) = api_call(ctx, "GET", &format!("{API_BASE}/repos/{target}"), None).await?;
            if status != 200 {
                return Err(api_err(status, &val));
            }
            let mut out = String::new();
            for (k, label) in [
                ("full_name", "名称"),
                ("description", "描述"),
                ("visibility", "可见性"),
                ("default_branch", "默认分支"),
                ("stargazers_count", "Stars"),
                ("updated_at", "更新时间"),
                ("html_url", "URL"),
            ] {
                out.push_str(&format!(
                    "{label}: {}\n",
                    val.get(k).map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    }).unwrap_or_else(|| "-".into())
                ));
            }
            Ok(GhResult::ok(out))
        }
        Some("list") => {
            let mut limit = 30usize;
            let mut i = 1;
            while i < rest.len() {
                if rest[i] == "--limit" {
                    i += 1;
                    limit = rest.get(i).ok_or("--limit 缺参数")?.parse().map_err(|_| "--limit 需要整数")?;
                }
                i += 1;
            }
            let items = api_list(
                ctx,
                "/user/repos",
                vec![("sort".into(), "updated".into())],
                limit,
            )
            .await?;
            let rows: Vec<Vec<String>> = items
                .iter()
                .map(|r| {
                    vec![
                        r.get("full_name").and_then(|v| v.as_str()).unwrap_or("").into(),
                        r.get("description").and_then(|v| v.as_str()).unwrap_or("").into(),
                        r.get("updated_at").and_then(|v| v.as_str()).unwrap_or("").into(),
                    ]
                })
                .collect();
            Ok(GhResult::ok(render_table(&["NAME", "DESCRIPTION", "UPDATED"], &rows)))
        }
        other => Err(format!("未知 repo 子命令: {}", other.unwrap_or(""))),
    }
}

fn common_list_opts(rest: &[String], start: usize) -> Result<(String, usize, Option<String>, Option<String>), String> {
    let mut state = String::new();
    let mut limit = 30usize;
    let mut jsonf: Option<String> = None;
    let mut jq: Option<String> = None;
    let mut i = start;
    while i < rest.len() {
        match rest[i].as_str() {
            "--state" => {
                i += 1;
                state = rest.get(i).ok_or("--state 缺参数")?.clone();
            }
            "--limit" | "-L" => {
                i += 1;
                limit = rest.get(i).ok_or("--limit 缺参数")?.parse().map_err(|_| "--limit 需要整数")?;
            }
            "--json" => {
                i += 1;
                jsonf = Some(rest.get(i).ok_or("--json 缺参数")?.clone());
            }
            "--jq" => {
                i += 1;
                jq = Some(rest.get(i).ok_or("--jq 缺参数")?.clone());
            }
            other => return Err(format!("未知参数: {other}")),
        }
        i += 1;
    }
    Ok((state, limit, jsonf, jq))
}

fn json_output(items: &[Value], jsonf: Option<&str>, jq: Option<&str>) -> Result<String, String> {
    let val = match jsonf {
        Some(f) => {
            let fields: Vec<&str> = f.split(',').map(|s| s.trim()).collect();
            for fld in &fields {
                if !JSON_FIELDS.contains(fld) {
                    return Err(format!(
                        "未知 --json 字段: {fld}（v0.0.1 支持: {}）",
                        JSON_FIELDS.join(",")
                    ));
                }
            }
            let arr: Vec<Value> = items
                .iter()
                .map(|item| {
                    let mut m = serde_json::Map::new();
                    for fld in &fields {
                        if let Some(v) = map_json_field(item, fld) {
                            m.insert(fld.to_string(), v);
                        }
                    }
                    Value::Object(m)
                })
                .collect();
            Value::Array(arr)
        }
        None => Value::Null,
    };
    match (jsonf, jq) {
        (Some(_), Some(expr)) => Ok(pretty(&apply_jq(expr, val)?)),
        (Some(_), None) => Ok(pretty(&val)),
        (None, Some(_)) => Err("--jq 需与 --json 配合使用".into()),
        (None, None) => unreachable!(),
    }
}

// ---- pr list / view / create / merge ----
async fn cmd_pr(ctx: &ApiCtx<'_>, rest: &[String], repo: Option<&str>) -> Result<GhResult, String> {
    match rest.first().map(|s| s.as_str()) {
        Some("list") => {
            let target = need_repo(repo)?;
            let (state, limit, jsonf, jq) = common_list_opts(rest, 1)?;
            let state = if state.is_empty() { "open".into() } else { state };
            let items = api_list(
                ctx,
                &format!("/repos/{target}/pulls"),
                vec![("state".into(), state)],
                limit,
            )
            .await?;
            if jsonf.is_some() {
                return Ok(GhResult::ok(json_output(&items, jsonf.as_deref(), jq.as_deref())?));
            }
            let rows: Vec<Vec<String>> = items.iter().map(pr_row).collect();
            Ok(GhResult::ok(render_table(&["NUMBER", "TITLE", "BRANCH", "STATE"], &rows)))
        }
        Some("view") => {
            let target = need_repo(repo)?;
            let num = rest.get(1).ok_or("pr view 缺少编号")?;
            let (status, val) =
                api_call(ctx, "GET", &format!("{API_BASE}/repos/{target}/pulls/{num}"), None).await?;
            if status != 200 {
                return Err(api_err(status, &val));
            }
            let mut out = String::new();
            out.push_str(&format!("title: {}\n", val.get("title").and_then(|v| v.as_str()).unwrap_or("")));
            out.push_str(&format!("state: {}\n", val.get("state").and_then(|v| v.as_str()).unwrap_or("")));
            out.push_str(&format!(
                "author: {}\n",
                val.pointer("/user/login").and_then(|v| v.as_str()).unwrap_or("")
            ));
            out.push_str(&format!(
                "branch: {} -> {}\n",
                val.pointer("/head/ref").and_then(|v| v.as_str()).unwrap_or(""),
                val.pointer("/base/ref").and_then(|v| v.as_str()).unwrap_or("")
            ));
            out.push_str(&format!(
                "url: {}\n",
                val.get("html_url").and_then(|v| v.as_str()).unwrap_or("")
            ));
            Ok(GhResult::ok(out))
        }
        Some("create") => {
            let target = need_repo(repo)?;
            let mut title = String::new();
            let mut head = String::new();
            let mut base = String::new();
            let mut body = String::new();
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--title" | "-t" => { i += 1; title = rest.get(i).ok_or("--title 缺参数")?.clone(); }
                    "--head" | "-H" => { i += 1; head = rest.get(i).ok_or("--head 缺参数")?.clone(); }
                    "--base" | "-B" => { i += 1; base = rest.get(i).ok_or("--base 缺参数")?.clone(); }
                    "--body" | "-b" => { i += 1; body = rest.get(i).ok_or("--body 缺参数")?.clone(); }
                    o => return Err(format!("未知参数: {o}")),
                }
                i += 1;
            }
            if title.is_empty() || head.is_empty() || base.is_empty() {
                return Err("pr create 需要 --title --head --base".into());
            }
            let payload = json!({ "title": title, "head": head, "base": base, "body": body });
            let (status, val) = api_call(
                ctx,
                "POST",
                &format!("{API_BASE}/repos/{target}/pulls"),
                Some(&payload),
            )
            .await?;
            if !(200..300).contains(&status) {
                return Err(api_err(status, &val));
            }
            Ok(GhResult::ok(format!(
                "已创建 PR #{}: {}\n",
                val.get("number").and_then(|v| v.as_u64()).unwrap_or(0),
                val.get("html_url").and_then(|v| v.as_str()).unwrap_or("")
            )))
        }
        Some("merge") => {
            let target = need_repo(repo)?;
            let num = rest.get(1).ok_or("pr merge 缺少编号")?;
            let mut method = "merge".to_string();
            let mut i = 2;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--merge" => method = "merge".into(),
                    "--squash" | "-s" => method = "squash".into(),
                    "--rebase" | "-r" => method = "rebase".into(),
                    o => return Err(format!("未知参数: {o}")),
                }
                i += 1;
            }
            let payload = json!({ "merge_method": method });
            let (status, val) = api_call(
                ctx,
                "PUT",
                &format!("{API_BASE}/repos/{target}/pulls/{num}/merge"),
                Some(&payload),
            )
            .await?;
            if !(200..300).contains(&status) {
                return Err(api_err(status, &val));
            }
            Ok(GhResult::ok(format!(
                "已合并 PR #{num}（{method}）: {}\n",
                val.get("message").and_then(|v| v.as_str()).unwrap_or("")
            )))
        }
        other => Err(format!("未知 pr 子命令: {}", other.unwrap_or(""))),
    }
}

// ---- issue list / view / create ----
async fn cmd_issue(ctx: &ApiCtx<'_>, rest: &[String], repo: Option<&str>) -> Result<GhResult, String> {
    match rest.first().map(|s| s.as_str()) {
        Some("list") => {
            let target = need_repo(repo)?;
            let (state, limit, jsonf, jq) = common_list_opts(rest, 1)?;
            let state = if state.is_empty() { "open".into() } else { state };
            let mut items = api_list(
                ctx,
                &format!("/repos/{target}/issues"),
                vec![("state".into(), state)],
                limit,
            )
            .await?;
            // issues API 会包含 PR，过滤掉（gh 行为一致）
            items.retain(|i| i.get("pull_request").is_none());
            if jsonf.is_some() {
                return Ok(GhResult::ok(json_output(&items, jsonf.as_deref(), jq.as_deref())?));
            }
            let rows: Vec<Vec<String>> = items.iter().map(issue_row).collect();
            Ok(GhResult::ok(render_table(&["NUMBER", "TITLE", "STATE"], &rows)))
        }
        Some("view") => {
            let target = need_repo(repo)?;
            let num = rest.get(1).ok_or("issue view 缺少编号")?;
            let (status, val) =
                api_call(ctx, "GET", &format!("{API_BASE}/repos/{target}/issues/{num}"), None).await?;
            if status != 200 {
                return Err(api_err(status, &val));
            }
            let mut out = String::new();
            out.push_str(&format!("title: {}\n", val.get("title").and_then(|v| v.as_str()).unwrap_or("")));
            out.push_str(&format!("state: {}\n", val.get("state").and_then(|v| v.as_str()).unwrap_or("")));
            out.push_str(&format!(
                "author: {}\n",
                val.pointer("/user/login").and_then(|v| v.as_str()).unwrap_or("")
            ));
            Ok(GhResult::ok(out))
        }
        Some("create") => {
            let target = need_repo(repo)?;
            let mut title = String::new();
            let mut body = String::new();
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--title" | "-t" => { i += 1; title = rest.get(i).ok_or("--title 缺参数")?.clone(); }
                    "--body" | "-b" => { i += 1; body = rest.get(i).ok_or("--body 缺参数")?.clone(); }
                    o => return Err(format!("未知参数: {o}")),
                }
                i += 1;
            }
            if title.is_empty() {
                return Err("issue create 需要 --title".into());
            }
            let payload = json!({ "title": title, "body": body });
            let (status, val) = api_call(
                ctx,
                "POST",
                &format!("{API_BASE}/repos/{target}/issues"),
                Some(&payload),
            )
            .await?;
            if !(200..300).contains(&status) {
                return Err(api_err(status, &val));
            }
            Ok(GhResult::ok(format!(
                "已创建 Issue #{}: {}\n",
                val.get("number").and_then(|v| v.as_u64()).unwrap_or(0),
                val.get("html_url").and_then(|v| v.as_str()).unwrap_or("")
            )))
        }
        other => Err(format!("未知 issue 子命令: {}", other.unwrap_or(""))),
    }
}
