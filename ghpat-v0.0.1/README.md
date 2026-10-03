# ghpat v0.0.1 — 面向 AI 智能体的 GitHub PAT 内存代理

单二进制实现（Rust，x86_64 Linux，musl 静态链接），按规格文档完整实现三形态：
`client` / `daemon(--daemon-internal)` / `cred-helper`。

## 产物

| 文件 | 说明 |
|---|---|
| `ghpat-linux-x86_64` | 主二进制，5.87 MB（验收 <8 MB ✓），static-pie linked，stripped |
| `run_tests.sh` | 集成自测脚本（`bash run_tests.sh ./ghpat-linux-x86_64`） |
| `SHA256SUMS` | 校验和：`89e7f24389636dd88572dcd6805c21df422923fdc7afef82cf0ca40228e4489f` |

## 快速上手

```bash
# 1. 启动 daemon（fork 子进程；打印 age 公钥）
./ghpat-linux-x86_64 start
#    可选：./ghpat-linux-x86_64 start --foreground   # 前台运行（调试）

# 2. 用打印出的公钥在本地加密 PAT（PAT 不落盘、不进 argv/env）
age -r age1xxxxxxxx... -o token.enc pat.txt    # pat.txt 加密后请自行粉碎

# 3. 注入 daemon（校验 /user → 通过后写入 mlock 内存页）
./ghpat-linux-x86_64 set-token token.enc

# 4. 日常使用（等价 gh 命令）
./ghpat-linux-x86_64 auth status
./ghpat-linux-x86_64 repo view
./ghpat-linux-x86_64 pr list --limit 10
./ghpat-linux-x86_64 pr view 12
./ghpat-linux-x86_64 issue create --title "..." --body "..."
./ghpat-linux-x86_64 api /repos/{owner}/{repo} --jq '.full_name'

# 5. git 场景：包装任意命令，自动注入 credential helper
./ghpat-linux-x86_64 wrap -- git clone https://github.com/o/r.git
./ghpat-linux-x86_64 wrap -- git push

# 6. 状态与销毁
./ghpat-linux-x86_64 status     # READY/ARMED + 指纹
./ghpat-linux-x86_64 pubkey     # 打印当前公钥
./ghpat-linux-x86_64 stop       # zeroize 整页 + unlink socket + exit(0)
```

socket 路径：`${XDG_RUNTIME_DIR:-/tmp/ghpat-$UID}/ghpat.sock`（目录自动创建并强制 0700）。
测试隔离可用 `--sock <PATH>` 覆盖。

## 安全设计落实情况（规格 §5/§8）

- **单页敏感状态**：identity（32B）+ pat_len（8B）+ pat_buf（256B）位于一页 4KiB
  `mmap(MAP_PRIVATE|MAP_ANONYMOUS)`，整页 `mlock` + `MADV_DONTDUMP`，进程生命周期内不
  munlock/realloc；写入用 volatile store、清除用 volatile zero，防止编译器优化消除。
- **PAT 不落盘**：set-token 全程内存解密（age Recipients 格式），明文临时缓冲
  `Zeroizing` 包裹，用后即毁；仅在 GitHub `/user` 返回 200 后才写入页（401 拒绝并保留原状态）。
- **形态分叉**：按 argv 判定（`cred-helper` / `--daemon-internal`），不使用环境变量做形态判断。
- **IPC**：Unix socket + JSON Lines 一问一答；`SO_PEERCRED` 校验调用方 UID == daemon UID；
  daemon `umask(0o077)` + `PR_SET_DUMPABLE=0`。
- **审计**：get_pat 记录时间 + 调用方 PID（来自 SO_PEERCRED）到 `ghpat.log`（0600，超 1MB 截断保留后半）。
- **cli 输出脱敏**：gh 子命令响应含 PAT 的路径不存在（PAT 永不回传给 client）。

## 测试结果

**单元测试（cargo test）：5/5 通过**
- SensitivePage 写入/覆盖/超长拒绝/zeroize
- age 密钥生成 → bech32 往返 → 页内原始字节重建 identity → 公钥一致
- age 加密→解密完整往返（等价 set-token 解密路径）
- 指纹前缀识别（ghp_ / github_pat_ / 未知前缀）
- --jq 基础过滤与语法错误处理

**集成自测：23/23 通过**（详见 `bash run_tests.sh`）
- 生命周期：start / DAEMON_ALREADY_RUNNING / stop / 3 轮循环
- 安全：进程参数中无 PAT 明文、非 age 密文被拒、失败注入保留 READY
- cred-helper：非 github.com host 过滤、未注入输出为空、store/erase 静默成功
- wrap：GIT_CONFIG_COUNT/KEY_0/VALUE_0/GIT_TERMINAL_PROMPT=0/GHPAT_SOCK 注入、退出码透传
- 崩溃恢复：kill -9 后残留 socket 检测、自动清理重启、目录权限 0700

## 真网验证记录（2026-10-03）

使用真实 fine-grained PAT（经 age 加密注入）完成端到端真网验证：

| 能力 | 命令 | 结果 |
|---|---|---|
| 注入校验 | `set-token token.enc` | ✔ age 解密 → `/user` 200 → ARMED，指纹 `github_pat_…RJBE` |
| 认证查询 | `auth status` | ✔ 已认证为 entropy356 |
| 仓库读取 | `repo list` / `repo view` | ✔ 字段映射正确；缺参数时报错并 exit≠0 |
| API 透传 | `api /repos/... --jq` | ✔ contents/commits 查询正常 |
| PR/Issue 读取 | `pr list` / `issue list` / `pr view 999` | ✔ 空列表正常；404 正确透传 exit=1 |
| **写路径** | `issue create` | ✔ 成功创建 [Issue #1](https://github.com/entropy356/ai-workspace/issues/1) 并用后关闭 |
| **cred-helper** | `wrap -- git clone/push` | ✔ 真网 clone + push 本仓库，全程 PAT 不出现在进程参数 |

## 已知限制（如实说明）

1. **GitHub API 401/非 200 错误分支的真网复测**：200 路径已全量真网验证（见上表），
   401 拒绝注入路径已在沙箱实测（错误密文/无效凭据注入被拒且保留 READY），但
   "已注入 token 中途失效后的 401 响应分支"未真网复测。
2. **崩溃恢复的多用户并发测试**（规格 §11.2-7）需两个真实 UID 的环境，沙箱为单用户，
   以"kill -9 + 残留 socket 清理重启"用例覆盖了同等逻辑。
3. **daemon 启动时无父进程校验**：`--daemon-internal` 理论上可被同 UID 进程直接调用
   （威胁模型内已排除同 UID 攻击者，与规格 §8 一致）。
4. 与规格的三处小偏差（均已在开工说明中记录）：IPC 增加 `pubkey` 命令；cred-helper
   兼容 `argv[1]==get` 与 `argv[2]==get` 两种布局；wrap 的 `GIT_CONFIG_COUNT` 采用累加偏移
   以兼容嵌套 wrap。
5. release 构建使用 `lto="thin"`（规格建议 fat，沙箱 2 核 + 网络文件系统无法承受 fat
   链接；产物 5.87MB 仍满足 <8MB 验收）。

## 源码与构建

完整 Rust 工程位于本仓库 [`ghpat/`](../ghpat/) 目录（11 个模块，约 2500 行）：

```bash
cd ghpat
cargo build --release        # 或 cross build --release --target x86_64-unknown-linux-musl
cargo test
bash ../ghpat-v0.0.1/run_tests.sh ./target/release/ghpat
```

- 模块划分：`page.rs`（mlock 敏感页）/ `agekey.rs`（age 密钥与加解密）/ `daemon.rs`（IPC 与
  生命周期）/ `client.rs` / `gh.rs`（gh 等价命令与 REST）/ `cred.rs` + `wrap.rs`（cred-helper
  与 wrap）/ `jq.rs` / `ipc.rs` / `err.rs` / `main.rs`（形态分叉）
- Cargo.toml 依赖与规格 §10 一致，另加 bech32 0.9 用于 age 密钥原始字节与页缓冲的互转
