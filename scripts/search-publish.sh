#!/usr/bin/env bash
# Публикация поискового индекса каталога на прод. Индекс собирается ЛОКАЛЬНО (по дампу прод-базы),
# прод только читает его (SEARCH_INDEX_READONLY=1) и ничего не перестраивает.
#
#   scripts/search-publish.sh pack                 # слить в один сегмент и упаковать
#   scripts/search-publish.sh push user@host       # выложить последний архив, перезапустить app
#   scripts/search-publish.sh rollback user@host   # вернуть предыдущий индекс
#
# Перед pack индекс должен быть собран: `nexus search rebuild all` (по изменившимся документам)
# или `--force` (с нуля, после смены схемы).
#
# Переменные:
#   NEXUS      — как вызвать nexus локально (по умолчанию release-сборка в контейнере nexus-app)
#   LOCAL_DIR  — каталог индексов внутри локального контейнера
#   OUT        — куда класть архивы (dist/search)
#   REMOTE_DIR — где на сервере лежит docker-compose.prod.yml (по умолчанию ~/nexus/infra)
#   VOLUME     — том с данными приложения (по умолчанию <проект>_archivedata, см. `docker volume ls`)
set -euo pipefail

NEXUS=${NEXUS:-"docker exec -w /app nexus-app /target/release/nexus"}
CONTAINER=${CONTAINER:-nexus-app}
LOCAL_DIR=${LOCAL_DIR:-/var/lib/nexus/tantivy}
OUT=${OUT:-dist/search}
REMOTE_DIR=${REMOTE_DIR:-nexus/infra}
VOLUME=${VOLUME:-infra_archivedata}
# Внутри тома: /var/lib/nexus/tantivy → tantivy/.
INDEX_IN_VOLUME=tantivy

say() { printf '\033[1m%s\033[0m\n' "$*"; }

pack() {
  mkdir -p "$OUT"
  say "1/3 слияние в один сегмент"
  $NEXUS search merge
  local schema segments stamp name
  schema=$(docker exec "$CONTAINER" cat "$LOCAL_DIR/catalog/nexus-schema")
  segments=$(docker exec "$CONTAINER" grep -c '"segment_id"' "$LOCAL_DIR/catalog/meta.json")
  [ "$segments" = "1" ] || { echo "ожидался один сегмент, а их $segments" >&2; exit 1; }
  stamp=$(date +%Y%m%d-%H%M%S)
  name="$schema-$stamp"
  say "2/3 упаковка $name.tar.zst"
  # Только файлы индекса: то, что ведёт Tantivy (.managed.json — в нём и meta.json), слой Б и
  # версия схемы. Осиротевшие сегменты прошлых слияний и блокировки в архив не идут.
  docker exec "$CONTAINER" sh -euc "cd $LOCAL_DIR
      { grep -o '\"[^\"]*\"' catalog/.managed.json | tr -d '\"' | sed 's|^|catalog/|'
        echo catalog/.managed.json; echo catalog/nexus-schema; ls catalog/sidecar-*; } > /tmp/pack.list
      tar -cf - -T /tmp/pack.list" \
    | zstd -T0 -10 -q -o "$OUT/$name.tar.zst"
  zstd -dc "$OUT/$name.tar.zst" | tar -tf - | sed 's|^|  |' 
  say "3/3 контрольная сумма"
  (cd "$OUT" && sha256sum "$name.tar.zst" > "$name.tar.zst.sha256")
  printf '{"schema":"%s","created":"%s","file":"%s.tar.zst"}\n' "$schema" "$stamp" "$name" \
    > "$OUT/$name.json"
  ln -sfn "$name.tar.zst" "$OUT/latest.tar.zst"
  ln -sfn "$name.tar.zst.sha256" "$OUT/latest.tar.zst.sha256"
  ls -la "$OUT/$name.tar.zst"
  say "готово: scripts/search-publish.sh push user@host"
}

# Команда в томе приложения на сервере (одноразовый контейнер alpine с томом в /data).
remote_volume() {
  local host=$1; shift
  ssh "$host" "docker run --rm -i -v $VOLUME:/data -w /data/$INDEX_IN_VOLUME alpine:3.20 sh -euc '$*'"
}

health() {
  local host=$1
  # Поиск отвечает и что-то находит (индекс открыт, слой Б на месте).
  ssh "$host" "for i in \$(seq 1 30); do
      if curl -fsS 'http://localhost/api/v1/catalog/search?q=%D0%BC%D0%B0%D1%82%D1%80%D0%B8%D1%86%D0%B0&limit=1' | grep -q '\"total\":[1-9]'; then exit 0; fi
      sleep 2
    done; exit 1"
}

restart_app() {
  ssh "$1" "cd $REMOTE_DIR && docker compose -f docker-compose.prod.yml restart app"
}

push() {
  local host=${1:?usage: push user@host}
  local file sum
  file=$(readlink -f "$OUT/latest.tar.zst")
  sum=$(cut -d' ' -f1 "$OUT/latest.tar.zst.sha256")
  say "1/5 копирование $(basename "$file") ($(du -h "$file" | cut -f1))"
  scp -q "$file" "$host:/tmp/nexus-catalog.tar.zst"
  ssh "$host" "echo '$sum  /tmp/nexus-catalog.tar.zst' | sha256sum -c --quiet"
  say "2/5 распаковка в catalog.new"
  ssh "$host" "zstd -dc /tmp/nexus-catalog.tar.zst | docker run --rm -i -v $VOLUME:/data alpine:3.20 sh -euc '
      mkdir -p /data/$INDEX_IN_VOLUME && cd /data/$INDEX_IN_VOLUME
      rm -rf catalog.new catalog.unpack && mkdir catalog.unpack
      tar -C catalog.unpack -xf -
      mv catalog.unpack/catalog catalog.new && rmdir catalog.unpack
      test -s catalog.new/meta.json && test -s catalog.new/sidecar-entities.cols'"
  say "3/5 подмена: catalog → catalog.prev, catalog.new → catalog"
  remote_volume "$host" 'rm -rf catalog.prev; if [ -d catalog ]; then mv catalog catalog.prev; fi; mv catalog.new catalog'
  say "4/5 перезапуск app"
  restart_app "$host"
  say "5/5 проверка поиска"
  if health "$host"; then
    ssh "$host" "rm -f /tmp/nexus-catalog.tar.zst"
    say "индекс выложен; предыдущий — catalog.prev (rollback)"
  else
    echo "поиск не отвечает — откат" >&2
    rollback "$host"
    exit 1
  fi
}

rollback() {
  local host=${1:?usage: rollback user@host}
  remote_volume "$host" 'test -d catalog.prev || { echo "нет catalog.prev" >&2; exit 1; }
    rm -rf catalog.bad; if [ -d catalog ]; then mv catalog catalog.bad; fi; mv catalog.prev catalog'
  restart_app "$host"
  health "$host" && say "откат выполнен (сломанный индекс — catalog.bad)"
}

case "${1:-}" in
  pack) pack ;;
  push) shift; push "$@" ;;
  rollback) shift; rollback "$@" ;;
  *) sed -n '2,20p' "$0"; exit 1 ;;
esac
