#!/usr/bin/env bash
# Замер задержки карточек каталога по уровням кэша и пропускной способности (ab).
# Использование: scripts/bench/cards.sh <метка> <base_url> <команда compose...>
#   scripts/bench/cards.sh dev http://localhost docker compose -f infra/docker-compose.yml
# Подробно и результаты — documents/benchmarks/catalog-cards.md.
# Перезапуск app очищает L1, стартовая перестройка индексов очищает Redis → первые запросы идут в L3.
set -euo pipefail
label=$1; url=$2; shift 2
compose=("$@")
dir=$(dirname "$0")

wait_ready() {
  # Ждём, пока приложение отвечает и стартовая перестройка индексов закончилась.
  local since=$1
  for _ in $(seq 1 180); do
    if "${compose[@]}" logs --since "$since" app 2>&1 | grep -q 'search index rebuilt'; then
      return 0
    fi
    sleep 1
  done
  echo "app did not finish startup reindex" >&2
  return 1
}

echo "=================== $label ($url)"
since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
"${compose[@]}" restart app >/dev/null
wait_ready "$since"
sleep 1

python3 "$dir/cards.py" "$url" cold
echo "... ждём 31 с: L1 истекает, Redis остаётся"
sleep 31
python3 "$dir/cards.py" "$url" l2
python3 "$dir/cards.py" "$url" warm

echo "--- пропускная способность (ab, 20000 запросов, 50 одновременно, keep-alive)"
for path in /health /api/v1/catalog/entities/dune-2021 '/api/v1/catalog/entities?limit=20'; do
  ab -q -k -n 20000 -c 50 "$url$path" 2>&1 \
    | awk -v p="$path" '/Requests per second/ {rps=$4} /Time per request/ && !t {t=$4} /Failed requests/ {f=$3}
        END {printf "%-40s %9.0f req/s   mean %6.2f ms   failed %s\n", p, rps, t, f}'
done
