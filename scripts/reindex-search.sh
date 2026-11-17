#!/usr/bin/env bash
# Перестраивает поисковый индекс каталога в поднятом dev-окружении (make up).
# Нужен после загрузки данных в БД в обход API (make seed). Вход — dev login под admin (другой админ: LOGIN=admin1).
set -euo pipefail

HOST=${HOST:-http://localhost}
LOGIN=${LOGIN:-admin}

token=$(curl -fsS -X POST "$HOST/api/v1/auth/dev/login" \
  -H 'Content-Type: application/json' -d "{\"login\": \"$LOGIN\"}" | jq -r .access_token)
curl -fsS -X POST "$HOST/api/v1/catalog/admin/search/reindex" -H "Authorization: Bearer $token"
echo
