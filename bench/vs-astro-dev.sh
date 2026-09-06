#!/usr/bin/env bash
# Compares the two ways to give one customer a live preview of the same Astro
# project: a dev server per tenant, and one sandbox-lite daemon for all of them.
#   bench/vs-astro-dev.sh [project-dir] [port]
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
project="${1:-$root/examples/starter}"
port="${2:-4595}"
bin="$root/target/release/sandbox-lite"
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 1; }
command -v npm >/dev/null || { echo "npm is needed for the astro dev half" >&2; exit 1; }

ms() { echo $(( ($2 - $1) / 1000000 )); }
rss() { awk '/VmRSS/ {print $2}' "/proc/$1/status" 2>/dev/null || echo 0; }
listener() { ss -ltnp 2>/dev/null | grep ":$1 " | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2; }

work="$(mktemp -d)"
trap 'pkill -f "astro dev --port $((port+1))" 2>/dev/null || true; kill "${sl_pid:-}" 2>/dev/null || true; rm -rf "$work"' EXIT

echo "project: $project"
echo
echo "== astro dev (one process per tenant)"
cp -r "$project/." "$work/"
t0=$(date +%s%N); npm --prefix "$work" install --no-audit --no-fund --silent; t1=$(date +%s%N)
install_ms=$(ms "$t0" "$t1")
modules_mb=$(du -sm "$work/node_modules" | cut -f1)
t2=$(date +%s%N)
(cd "$work" && npx astro dev --port $((port+1)) >"$work/dev.log" 2>&1 &)
until grep -qE "ready in|localhost:$((port+1))" "$work/dev.log" 2>/dev/null; do sleep 0.02; done
t3=$(date +%s%N); ready_ms=$(ms "$t2" "$t3")
t4=$(date +%s%N); curl -sf -o /dev/null "http://localhost:$((port+1))/"; t5=$(date +%s%N)
first_ms=$(ms "$t4" "$t5")
t6=$(date +%s%N); curl -sf -o /dev/null "http://localhost:$((port+1))/blog"; t7=$(date +%s%N)
second_ms=$(ms "$t6" "$t7")
dev_rss=$(rss "$(listener $((port+1)))")
pkill -f "astro dev --port $((port+1))" || true
printf "  npm install          %6d ms   (%d MB of node_modules, per tenant)\n" "$install_ms" "$modules_mb"
printf "  dev server ready     %6d ms\n" "$ready_ms"
printf "  first page           %6d ms\n" "$first_ms"
printf "  second page          %6d ms\n" "$second_ms"
printf "  RSS                  %6d MB   for this one tenant\n" $((dev_rss / 1024))
printf "  cold -> first page   %6d ms\n" $(( install_ms + ready_ms + first_ms ))

echo
echo "== sandbox-lite (one daemon for every tenant)"
data="$work/data"
t0=$(date +%s%N)
"$bin" --listen "127.0.0.1:$port" --base "bench=$project" --data-dir "$data" >"$work/sl.log" 2>&1 &
sl_pid=$!
until curl -sf -o /dev/null "http://127.0.0.1:$port/health" 2>/dev/null; do sleep 0.005; done
t1=$(date +%s%N); boot_ms=$(ms "$t0" "$t1")
t2=$(date +%s%N)
curl -sf -o /dev/null -X POST -H 'content-type: application/json' -d '{"id":"t1","base":"bench"}' "http://127.0.0.1:$port/api/tenants"
t3=$(date +%s%N); tenant_ms=$(ms "$t2" "$t3")
modules=$(curl -sf -H 'Host: t1.localhost' "http://127.0.0.1:$port/__sl/routes.json" | grep -o '"component":"[^"]*"' | cut -d'"' -f4)
t4=$(date +%s%N)
curl -sf -o /dev/null -H 'Host: t1.localhost' "http://127.0.0.1:$port/"
curl -sf -o /dev/null -H 'Host: t1.localhost' "http://127.0.0.1:$port/__sl/routes.json"
for m in $modules; do curl -sf -o /dev/null -H 'Host: t1.localhost' "http://127.0.0.1:$port/__sl/m/$m?v=1" || true; done
t5=$(date +%s%N); graph_ms=$(ms "$t4" "$t5")
t6=$(date +%s%N); curl -sf -o /dev/null -H 'Host: t1.localhost' "http://127.0.0.1:$port/__sl/m/src/pages/index.astro?v=1"; t7=$(date +%s%N)
cached_ms=$(ms "$t6" "$t7")
sl_rss=$(rss "$sl_pid")
printf "  daemon ready         %6d ms\n" "$boot_ms"
printf "  tenant created       %6d ms   (no install, no node_modules)\n" "$tenant_ms"
printf "  every page module    %6d ms   (%d modules compiled)\n" "$graph_ms" "$(echo "$modules" | wc -w)"
printf "  cached module        %6d ms\n" "$cached_ms"
printf "  RSS                  %6d MB   for the daemon and every tenant in it\n" $((sl_rss / 1024))
printf "  cold -> first page   %6d ms\n" $(( boot_ms + tenant_ms + graph_ms ))
echo
printf "cold path: %d ms vs %d ms   ·   memory: %d MB vs %d MB\n" \
  $(( install_ms + ready_ms + first_ms )) $(( boot_ms + tenant_ms + graph_ms )) $((dev_rss / 1024)) $((sl_rss / 1024))
