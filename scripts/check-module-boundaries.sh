#!/usr/bin/env bash
# Модули (modules/*) не должны зависеть друг от друга — только от libs/*.
# Компилятор не даст импортировать модуль, которого нет в Cargo.toml, а этот скрипт
# не даст его туда добавить.
set -euo pipefail

violations=$(cargo metadata --no-deps --format-version 1 | jq -r '
  [.packages[] | select(.manifest_path | test("/modules/[^/]+/Cargo.toml$")) | .name] as $modules
  | .packages[]
  | select(.name as $n | $modules | index($n))
  | .name as $from
  | .dependencies[]
  | select(.name as $d | $modules | index($d))
  | "\($from) -> \(.name)"
')

if [[ -n "$violations" ]]; then
  echo "Модули не должны зависеть друг от друга:" >&2
  echo "$violations" >&2
  echo "Вынесите общий код в libs/ или оформите явный API через shared (см. README)." >&2
  exit 1
fi

echo "module boundaries ok"
