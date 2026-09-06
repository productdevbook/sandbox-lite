#!/usr/bin/env bash
# Measures daemon RSS as tenants are created, edited and previewed.
#   bench/mem.sh [tenants] [port]
set -euo pipefail
N="${1:-200}"
PORT="${2:-4399}"
root="$(cd "$(dirname "$0")/.." && pwd)"
bin="$root/target/release/sandbox-lite"
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 1; }
data="$(mktemp -d)"
trap 'kill $pid 2>/dev/null || true; rm -rf "$data"' EXIT
"$bin" --listen "127.0.0.1:$PORT" --bases "$root/examples" --data-dir "$data" >/dev/null 2>&1 &
pid=$!
for _ in $(seq 50); do curl -sf "http://127.0.0.1:$PORT/health" >/dev/null && break; sleep 0.1; done

rss() { awk '/VmRSS/ {print $2}' "/proc/$pid/status"; }
modules=(src/pages/index.astro src/layouts/Layout.astro src/components/Header.astro src/components/Footer.astro src/components/Card.astro src/data/products.ts src/styles/global.css)
load_preview() {
  local id="$1"
  curl -sf -H "Host: $id.localhost" "http://127.0.0.1:$PORT/" >/dev/null
  curl -sf -H "Host: $id.localhost" "http://127.0.0.1:$PORT/__sl/routes.json" >/dev/null
  for m in "${modules[@]}"; do curl -sf -H "Host: $id.localhost" "http://127.0.0.1:$PORT/__sl/m/$m?v=1" >/dev/null; done
  curl -sf -H "Host: $id.localhost" "http://127.0.0.1:$PORT/__sl/m/src/pages/index.astro?astro&type=style&index=0&lang.css" >/dev/null
  curl -sf -H "Host: $id.localhost" "http://127.0.0.1:$PORT/__sl/content/posts" >/dev/null
}

base=$(rss)
printf "idle daemon:            %6d kB\n" "$base"
t0=$(date +%s%N)
for i in $(seq 1 "$N"); do
  id="t$i"
  curl -sf -X POST -H 'content-type: application/json' -d "{\"id\":\"$id\",\"base\":\"starter\"}" "http://127.0.0.1:$PORT/api/tenants" >/dev/null
  # every tenant edits one file so its module output is unique
  printf -- '---\nimport Layout from "../layouts/Layout.astro";\n---\n<Layout title="About %s">\n  <h1>Tenant %s</h1>\n  <p>Edited copy for tenant %s.</p>\n</Layout>\n' "$id" "$id" "$id" \
    | curl -sf -X PUT --data-binary @- "http://127.0.0.1:$PORT/api/t/$id/file/src/pages/about.astro" >/dev/null
  load_preview "$id"
  curl -sf -H "Host: $id.localhost" "http://127.0.0.1:$PORT/__sl/m/src/pages/about.astro?v=1" >/dev/null
done
t1=$(date +%s%N)
after=$(rss)
printf "after %4d tenants:      %6d kB   (+%d kB, %.1f kB per tenant, %d ms per tenant incl. preview load)\n" \
  "$N" "$after" $((after-base)) "$(awk -v a="$after" -v b="$base" -v n="$N" 'BEGIN { printf "%.1f", (a-b)/n }')" $(( (t1-t0)/1000000/N ))
curl -s "http://127.0.0.1:$PORT/api/stats" | python3 -c "import json,sys; s=json.load(sys.stdin)['cache']; print('transform cache:        %d kB in %d entries, %d hits / %d misses' % (s['bytes']//1024, s['entries'], s['hits'], s['misses']))"
