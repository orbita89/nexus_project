#!/usr/bin/env bash
# Досылает в поисковый индекс только указанные сущности (по slug) — вместо полной перестройки
# (scripts/reindex-search.sh, десятки минут на весь каталог). Нужно после импортов, которые
# пишут в БД мимо API. Вход — dev login под admin.
#
#   scripts/index-entities.sh mona-lisa guernica
#   scripts/index-entities.sh < slugs.txt      # по одному slug в строке
#
# За раз бэкенд принимает до 1000 slug'ов; длинный список режем на части.
set -euo pipefail

HOST=${HOST:-http://localhost}
BATCH=${BATCH:-500}

slugs=("$@")
if [ ${#slugs[@]} -eq 0 ]; then
  while IFS= read -r line; do
    [ -n "$line" ] && slugs+=("$line")
  done
fi
if [ ${#slugs[@]} -eq 0 ]; then
  echo "укажите slug'и аргументами или на stdin" >&2
  exit 2
fi

token=$(curl -fsS -X POST "$HOST/api/v1/auth/dev/login" \
  -H 'Content-Type: application/json' -d '{"login": "admin"}' | jq -r .access_token)

for ((i = 0; i < ${#slugs[@]}; i += BATCH)); do
  printf '%s\n' "${slugs[@]:i:BATCH}" \
    | jq -R . | jq -s '{slugs: .}' \
    | curl -fsS -X POST "$HOST/api/v1/catalog/admin/search/entities" \
        -H "Authorization: Bearer $token" -H 'Content-Type: application/json' -d @-
  echo
done
