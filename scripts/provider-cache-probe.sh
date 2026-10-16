#!/bin/sh
# 探一个 provider 的 **prompt 缓存**：同一条大前缀连发两次，看第二次的 usage 报不报命中。
#
# 为什么要有它：缓存是否生效决定了「前缀瘦身值多少钱」—— 命中则前缀边际成本≈0，
# 不命中则是每轮实打实的 prefill。而这件事**曾经因为适配器只认一家字段方言**
# 被误判成「provider 不上报」（见 `docs/arch/explore-speed/cost-model.md` §10）。
# 这把尺子直接打 provider，不经我们的解析，故结论不受适配器影响。
#
# 实测（2026-10-16）：
#   deepseek     → 第 2 次 `prompt_cache_hit_tokens: 9600/9735`
#   opencode-go  → 第 2 次 `prompt_tokens_details.cached_tokens: 9728/9760`
#   **两家字段名不同** —— 这正是当初读不到数的原因。
#
# 凭证取自 `~/.yushan/auth.json`，**不打印**。
# 用法：sh scripts/provider-cache-probe.sh [provider 名]
set -u
AUTH="$HOME/.yushan/auth.json"
P="${1:-deepseek}"

BASE=$(python3 -c "import json;print(json.load(open('$AUTH'))['$P']['api_base'])")
KEY=$(python3 -c "import json;print(json.load(open('$AUTH'))['$P']['api_key'])")
MODEL=$(python3 -c "import json;print(json.load(open('$AUTH'))['$P'].get('model',''))")
# opencode-go 强制要求会话头（网关拒绝空会话 id）；其余 provider 无此要求。
SESSION=""
[ "$P" = "opencode-go" ] && SESSION="x-opencode-session: probe-cache-0001"

# 约 9.7k token 的固定前缀：纯 ASCII 重复段落，两次调用逐字相同。
python3 - "$MODEL" > /tmp/provider-cache-body.json <<'PY'
import json, sys
filler = ("The quick brown fox jumps over the lazy dog. " * 8 + "\n") * 120
json.dump({
    "model": sys.argv[1],
    "messages": [
        {"role": "system", "content": "You are a helper. " + filler},
        {"role": "user", "content": "只回复一个字：好"},
    ],
    "max_tokens": 4,
}, sys.stdout, ensure_ascii=False)
PY

echo "=== $P  $BASE  model=$MODEL ==="
for i in 1 2; do
  curl -s --max-time 90 -X POST "$BASE/chat/completions" \
    -H "Content-Type: application/json" -H "Authorization: Bearer $KEY" \
    ${SESSION:+-H "$SESSION"} \
    --data-binary @/tmp/provider-cache-body.json > /tmp/provider-cache-resp.json
  python3 -c "
import json
d = json.load(open('/tmp/provider-cache-resp.json'))
u = d.get('usage')
print('  第 $i 次 usage:', json.dumps(u, ensure_ascii=False) if u else d)
"
done
echo "  读法：第 2 次的「命中」侧应显著 > 0；两家的字段名可能不同（见文件头）。"
