//! wrap 形态（规格 §7.4）：注入 GIT_CONFIG_* + GIT_TERMINAL_PROMPT=0 + GHPAT_SOCK

use std::path::Path;
use std::process::Command;

pub fn run(sock: &Path, command: Vec<String>) -> i32 {
    if command.is_empty() {
        eprintln!("✘ wrap 需要 -- 后跟目标命令");
        return 1;
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("✘ 无法定位自身二进制: {e}");
            return 1;
        }
    };
    let helper_value = format!("!{} cred-helper", exe.display());

    // 兼容嵌套 wrap：在已有 GIT_CONFIG_COUNT 上累加，避免覆盖外层注入
    let existing: u32 = std::env::var("GIT_CONFIG_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let idx = existing;

    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .env("GIT_CONFIG_COUNT", (existing + 1).to_string())
        .env(format!("GIT_CONFIG_KEY_{idx}"), "credential.https://github.com.helper")
        .env(format!("GIT_CONFIG_VALUE_{idx}"), &helper_value)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GHPAT_SOCK", sock);

    match cmd.status() {
        Ok(st) => st.code().unwrap_or(1),
        Err(e) => {
            eprintln!("✘ 启动目标命令失败: {e}");
            127
        }
    }
}
