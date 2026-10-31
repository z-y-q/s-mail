#!/bin/sh
# 一键本地验证门禁——与 CI 同口径，输出一页结论
#
# 用法：
#   sh scripts/verify.sh            # 标准验证：fmt + clippy(CI 逐字) + 测试 + perf + 体积
#   sh scripts/verify.sh --fast     # 跳过 release 构建（perf/体积），日常改动用
#   sh scripts/verify.sh --e2e      # 额外跑 dovecot 本地 e2e 核心组（m4/m14/m19）
#
# 背景：本轮 CI lint 三连红的教训——增量缓存会跳过未改动文件的重新 lint，
# 本地不带尾参的 clippy 与 CI 的 -D warnings 严格模式不同口径。本脚本
# 用 touch 强制全量重 lint，逐字复刻 CI 命令，杜绝「本地绿 CI 红」。
#
# 任何一步失败即停，结尾输出汇总；退出码 = 失败步骤数

set -u

FAST=0
E2E=0
for arg in "$@"; do
    case "$arg" in
        --fast) FAST=1 ;;
        --e2e) E2E=1 ;;
        *) echo "未知参数：$arg"; exit 2 ;;
    esac
done

FAILS=0
PASSED=""
FAILED=""

run_step() {
    name="$1"
    shift
    printf '▶ %s ...' "$name"
    start=$(date +%s)
    if "$@"; then
        end=$(date +%s)
        printf ' PASS（%ss）\n' "$((end - start))"
        PASSED="$PASSED\n  ✅ $name"
    else
        printf ' FAIL\n'
        FAILED="$FAILED\n  ❌ $name"
        FAILS=$((FAILS + 1))
    fi
}

EXCLUDES="--exclude spike-fts --exclude spike-ime --exclude spike-list --exclude spike-melib"

# 1. 格式检查（含 spikes，零成本，与 CI 同）
run_step "格式检查" cargo fmt --all --check

# 2. clippy 严格门禁：touch 全部源码强制全量重 lint（防增量缓存跳过）
find crates -name "*.rs" -exec touch {} +
run_step "clippy 严格门禁（CI 逐字，touch 防缓存）" \
    cargo clippy --workspace $EXCLUDES --all-targets -- -D warnings

# 3. 全仓测试（与 CI 矩阵同口径）
run_step "全仓测试" cargo test --workspace $EXCLUDES

# 3.5 RustSec 公告门禁（与 CI audit 作业同源；无 cargo-audit 时降级为提示）
if command -v cargo-audit >/dev/null 2>&1; then
    # 公告库本地克隆（本机 github HTTPS 不通时走 SSH 手动克隆到 target/advisory-db）
    if [ -d target/advisory-db ]; then
        run_step "RustSec 公告扫描" cargo audit --db target/advisory-db
    else
        run_step "RustSec 公告扫描" cargo audit
    fi
else
    printf '▶ RustSec 公告扫描 ... SKIP（未装 cargo-audit；CI audit 作业仍会扫）\n'
fi

if [ "$FAST" -eq 0 ]; then
    # 4. 性能基线（release，万封确定性负载）
    if cargo build --release -q -p smail-core --example perf \
        && ./target/release/examples/perf; then
        PASSED="$PASSED\n  ✅ 性能基线"
        printf '▶ 性能基线 ... PASS\n'
    else
        FAILED="$FAILED\n  ❌ 性能基线"
        FAILS=$((FAILS + 1))
        printf '▶ 性能基线 ... FAIL\n'
    fi

    # 5. 二进制体积门禁（strip 后 < 40MB，与 CI 同）
    if [ -f target/release/s-mail ]; then
        cp target/release/s-mail target/release/s-mail.stripcheck
        strip target/release/s-mail.stripcheck 2>/dev/null
        size=$(stat -f%z target/release/s-mail.stripcheck 2>/dev/null || stat -c%s target/release/s-mail.stripcheck)
        rm -f target/release/s-mail.stripcheck
        mb=$((size / 1024 / 1024))
        printf '▶ 二进制体积 ... %sMB' "$mb"
        if [ "$size" -lt $((40 * 1024 * 1024)) ]; then
            printf ' PASS\n'
            PASSED="$PASSED\n  ✅ 二进制体积（${mb}MB）"
        else
            printf ' FAIL\n'
            FAILED="$FAILED\n  ❌ 二进制体积（${mb}MB ≥ 40MB）"
            FAILS=$((FAILS + 1))
        fi
    else
        printf '▶ 二进制体积 ... FAIL（release 二进制不存在）\n'
        FAILED="$FAILED\n  ❌ 二进制体积"
        FAILS=$((FAILS + 1))
    fi
fi

if [ "$E2E" -eq 1 ]; then
    # 6. dovecot 本地 e2e 核心组（HTML 管线/移动/废纸篓三链路；完整矩阵在 CI）
    cargo build -q -p smail-core --examples || exit 1
    sh spikes/spike-melib/imap-test/up.sh >/dev/null 2>&1
    for m in m4 m14 m19; do
        home="/tmp/smail-verify-$m"
        rm -rf "$home"
        [ "$m" = "m4" ] && python3 spikes/spike-melib/imap-test/inject_html_mail.py >/dev/null && sleep 2
        if SMAIL_CONFIG=spikes/spike-melib/config.toml SMAIL_HOME="$home" \
            timeout 150 ./target/debug/examples/$m 60 >/dev/null 2>&1; then
            PASSED="$PASSED\n  ✅ e2e $m"
            printf '▶ e2e %s ... PASS\n' "$m"
        else
            FAILED="$FAILED\n  ❌ e2e $m"
            FAILS=$((FAILS + 1))
            printf '▶ e2e %s ... FAIL\n' "$m"
        fi
        rm -rf "$home"
    done
    sh spikes/spike-melib/imap-test/down.sh >/dev/null 2>&1
fi

printf '\n===== 验证汇总 =====\n'
if [ "$FAILS" -eq 0 ]; then
    printf '全部通过（%s 项）\n' "$(printf '%b' "$PASSED" | grep -c '✅')"
else
    printf '%b' "$PASSED"
    printf '%b' "$FAILED"
    printf '\n失败 %s 项\n' "$FAILS"
fi
exit "$FAILS"
