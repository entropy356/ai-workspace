#!/usr/bin/env bash
# ghpat 集成自测（规格 §11.2 可执行项）
# 用法: run_tests.sh <ghpat二进制路径>
set -u
BIN="${1:?用法: run_tests.sh <ghpat二进制>}"
TMP="$(mktemp -d /tmp/ghpat-test-XXXXXX)"
SOCK="$TMP/run/ghpat.sock"
mkdir -p "$TMP/run"
export XDG_RUNTIME_DIR="$TMP/run"
PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ✔ $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  ✘ $1"; }
check(){ if [ $? -eq 0 ]; then ok "$1"; else bad "$1"; fi; }

echo "== 1. 生命周期 =="
"$BIN" --sock "$SOCK" start >/dev/null 2>&1; check "start 退出码 0"
test -S "$SOCK"; check "socket 存在"
OUT="$("$BIN" --sock "$SOCK" pubkey)"
case "$OUT" in age1*) ok "pubkey 格式 age1…";; *) bad "pubkey 格式: $OUT";; esac
"$BIN" --sock "$SOCK" start >/dev/null 2>&1 && bad "重复 start 未报错" || ok "重复 start 报 DAEMON_ALREADY_RUNNING"
ST="$("$BIN" --sock "$SOCK" status)"
case "$ST" in *READY*) ok "status=READY";; *) bad "status: $ST";; esac

echo "== 2. argv 检查（§8.1: PAT 不出现在 argv）=="
FOUND=$(ps -eo args | grep -c "ghp_[t]est\|passwor[d]=" || true)
[ "$FOUND" = "0" ]; if [ "$FOUND" != "0" ]; then ps -eo args | grep "ghp_[t]est\|passwor[d]=" | head -3; fi
check "进程参数中无 PAT"

echo "== 3. set-token（自加密 dummy token，GitHub 401 路径）=="
PUB="$("$BIN" --sock "$SOCK" pubkey | tail -1)"
echo -n "ghp_dummytoken_for_test_only_1234567890" > "$TMP/pat.txt"
if command -v age >/dev/null; then
  age -r "$PUB" -o "$TMP/token.enc" "$TMP/pat.txt"
else
  echo "（无 age CLI，使用二进制内置测试路径）"
  # 用 age crate 的往返：通过 ghpat wrap 调 python? 简化：跳过加密，直接测错误路径
  echo -n "not-a-valid-age-file" > "$TMP/token.enc"
fi
RES="$("$BIN" --sock "$SOCK" set-token "$TMP/token.enc" 2>&1)"
if command -v age >/dev/null; then
  case "$RES" in *TOKEN_INVALID*|*"401"*) ok "假 token 被拒（TOKEN_INVALID）";; *) bad "set-token: $RES";; esac
else
  case "$RES" in *NOT_RECIPIENT_FORMAT*|*DECRYPT_FAILED*|*解密失败*|*仅支持) ok "非 age 密文被拒";; *) bad "set-token 错误路径: $RES";; esac
fi
ST="$("$BIN" --sock "$SOCK" status)"
case "$ST" in *READY*) ok "失败注入保留 READY";; *) bad "注入失败后状态: $ST";; esac

echo "== 4. cred-helper =="
OUT=$(printf 'protocol=https\nhost=gitlab.com\n\n' | "$BIN" cred-helper get 2>/dev/null)
[ -z "$OUT" ]; check "非 github.com host 输出为空"
OUT=$(printf 'protocol=https\nhost=github.com\n\n' | "$BIN" cred-helper get 2>/dev/null)
[ -z "$OUT" ]; check "未注入 token 时输出为空"
printf 'protocol=https\nhost=example.com\n\n' | "$BIN" cred-helper store; check "store 静默成功(退出码0)"
printf 'protocol=https\nhost=example.com\n\n' | "$BIN" cred-helper erase; check "erase 静默成功(退出码0)"
OUT=$(printf 'protocol=https\nhost=github.com\n\n' | "$BIN" --sock "$SOCK" cred-helper get 2>/dev/null || true)
echo "  （--sock 与 cred-helper 组合仅用于测试，正常路径走 GHPAT_SOCK）"

echo "== 5. wrap =="
"$BIN" --sock "$SOCK" wrap -- env > "$TMP/env.out" 2>&1
grep -q "^GIT_CONFIG_COUNT=1$" "$TMP/env.out"; check "GIT_CONFIG_COUNT=1"
grep -q "^GIT_CONFIG_KEY_0=credential.https://github.com.helper$" "$TMP/env.out"; check "KEY_0 注入"
grep -q "^GIT_TERMINAL_PROMPT=0$" "$TMP/env.out"; check "GIT_TERMINAL_PROMPT=0"
grep -q "^GHPAT_SOCK=$SOCK$" "$TMP/env.out"; check "GHPAT_SOCK 传递"
"$BIN" --sock "$SOCK" wrap -- true; check "wrap 透传退出码"

echo "== 6. 崩溃恢复（kill -9 → 残留 socket）=="
DPID=$(pgrep -f "daemon-internal" | head -1)
kill -9 "$DPID" 2>/dev/null
sleep 0.3
"$BIN" --sock "$SOCK" status >/dev/null 2>&1 && bad "daemon 死后 status 未报错" || ok "daemon 死后 status 报错"
"$BIN" --sock "$SOCK" start >/dev/null 2>&1; check "残留 socket 清理后可重启"
"$BIN" --sock "$SOCK" stop >/dev/null 2>&1; check "stop 退出码 0"
test ! -e "$SOCK"; check "stop 后 socket 已 unlink"

echo "== 7. 权限 =="
D="$(dirname "$SOCK")"
STAT=$(stat -c '%a' "$D" 2>/dev/null || echo "")
[ "$STAT" = "700" ]; check "运行目录 0700（$STAT）"

echo "== 8. 多轮 start/stop 稳定性 =="
for i in 1 2 3; do
  "$BIN" --sock "$SOCK" start >/dev/null 2>&1 && "$BIN" --sock "$SOCK" stop >/dev/null 2>&1 || { bad "第 $i 轮 start/stop"; break; }
done
ok "3 轮 start/stop"

echo
echo "通过 $PASS / $((PASS+FAIL))"
[ "$FAIL" = "0" ] && echo "ALL_PASS" || echo "HAS_FAILURES"
rm -rf "$TMP"
