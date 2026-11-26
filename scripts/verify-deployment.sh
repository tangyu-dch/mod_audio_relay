#!/usr/bin/env bash
# Verify that mod_audio_relay is correctly deployed inside a running
# FreeSWITCH instance. Run on the FreeSWITCH host or inside its container.
#
# Usage:
#   ./scripts/verify-deployment.sh [fs_cli args...]
#   FS_CLI="fs_cli -p secret" ./scripts/verify-deployment.sh
#
# Exit code 0 = all checks passed.
set -uo pipefail

FS_CLI=${FS_CLI:-fs_cli}
FS_PREFIX=${FS_PREFIX:-/usr/local/freeswitch}
FAIL=0

check() { # check <description> <expected-substring> <actual>
    local desc="$1" want="$2" got="$3"
    if printf '%s' "$got" | grep -q "$want"; then
        printf 'PASS  %s\n' "$desc"
    else
        printf 'FAIL  %s (want "%s", got "%s")\n' "$desc" "$want" "$got"
        FAIL=1
    fi
}

echo "== 1. Module binary =="
if [ -f "$FS_PREFIX/mod/mod_audio_relay.so" ]; then
    printf 'PASS  %s/mod/mod_audio_relay.so exists\n' "$FS_PREFIX"
else
    printf 'FAIL  %s/mod/mod_audio_relay.so missing\n' "$FS_PREFIX"
    FAIL=1
fi

echo "== 2. modules.conf load entry =="
CONF=$($FS_CLI -x "global_getvar freeswitch_conf_dir" 2>/dev/null | tr -d '\r')
CONF=${CONF:-$FS_PREFIX/conf}
if grep -q 'load module="mod_audio_relay"' "$CONF/autoload_configs/modules.conf.xml" 2>/dev/null; then
    printf 'PASS  modules.conf.xml loads mod_audio_relay\n'
else
    printf 'FAIL  modules.conf.xml has no <load module="mod_audio_relay"/>\n'
    FAIL=1
fi

echo "== 3. Module registered in running FreeSWITCH =="
OUT=$($FS_CLI -x "module_exists mod_audio_relay" 2>&1 | tr -d '\r')
check "module_exists mod_audio_relay" "true" "$OUT"

echo "== 4. uuid_audio_relay API registered =="
# Calling the API without arguments must return -USAGE (proves the API
# callback is registered); -ERR or no output means it is not deployed.
OUT=$($FS_CLI -x "api uuid_audio_relay" 2>&1 | tr -d '\r')
check "api uuid_audio_relay returns -USAGE" "USAGE" "$OUT"

echo "== 5. SpeexDSP resampler available =="
if ldconfig -p 2>/dev/null | grep -q speexdsp || [ -f /usr/lib/libspeexdsp.so ] || [ -f /usr/local/lib/libspeexdsp.so ]; then
    printf 'PASS  libspeexdsp present\n'
else
    printf 'FAIL  libspeexdsp not found (required for resampling)\n'
    FAIL=1
fi

if [ "$FAIL" -eq 0 ]; then
    echo "ALL CHECKS PASSED"
else
    echo "SOME CHECKS FAILED"
fi
exit "$FAIL"
