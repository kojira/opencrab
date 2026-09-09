#!/usr/bin/env bash
set -euo pipefail

# #975 opt-in実LLM QC。秘密・実IDはargvへ置かず、呼び出し元のenvだけから読む。
required=(LIVE_AGENT_ID LIVE_OWNER_ID LIVE_GATE_SOCK LIVE_CORE_HTTP OPENCRAB_GATE_OPERATOR_TOKEN)
for key in "${required[@]}"; do
  if [[ -z "${!key:-}" ]]; then
    echo "missing required env: ${key}" >&2
    exit 2
  fi
done

model=$(python3 - <<'PY'
import json, os, urllib.request
base = os.environ["LIVE_CORE_HTTP"].rstrip("/")
agent = os.environ["LIVE_AGENT_ID"]
request = urllib.request.Request(
    f"{base}/api/agents/{agent}",
    headers={"Authorization": f"Bearer {os.environ['OPENCRAB_GATE_OPERATOR_TOKEN']}"},
)
with urllib.request.urlopen(request, timeout=10) as response:
    print(json.load(response).get("model") or "")
PY
)
case "$model" in
  chatgpt:*|hermit:*) ;;
  *) echo "refusing non-subscription model for live QC: ${model:-<unset>}" >&2; exit 2 ;;
esac
echo "subscription model preflight: $model"

profile="${1:-smoke}"
case "$profile" in
  smoke) scenarios=(issue975-success issue975-failure issue975-multi) ;;
  full) scenarios=(issue975-success issue975-failure issue975-multi issue975-large) ;;
  *) echo "usage: $0 [smoke|full]" >&2; exit 2 ;;
esac

export LIVE_MODE=gate
export LIVE_TIMEOUT_SECS="${LIVE_TIMEOUT_SECS:-180}"
for scenario in "${scenarios[@]}"; do
  echo "=== #975 live LLM QC: ${scenario} ==="
  LIVE_SCENARIO="$scenario" \
    cargo test -p opencrab-server --test live_di_smoke live_di_smoke -- \
      --ignored --exact --nocapture --test-threads=1
done
