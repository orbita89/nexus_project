"""Замеры задержки карточек каталога: L3 (Meilisearch), L2 (Redis), L1 (память), сравнение с БД.

Запуск: python3 scripts/bench/cards.py <base_url> <phase> (обычно через cards.sh)
  phase = cold  — первый запрос к каждой карточке (кэш пуст → L3)
          l2    — первый запрос после истечения L1 (→ L2, Redis)
          warm  — повторные запросы (L1), /health и список из Postgres
Каждый запрос — keep-alive соединение (как браузер), и отдельно — новое соединение на запрос (как curl).
"""

import http.client
import json
import statistics
import sys
import time
from urllib.parse import urlparse

base = urlparse(sys.argv[1])
phase = sys.argv[2]
HOST, PORT = base.hostname, base.port or 80
API = "/api/v1/catalog"


def conn():
    return http.client.HTTPConnection(HOST, PORT, timeout=10)


def fetch(c, path):
    started = time.perf_counter()
    c.request("GET", path)
    r = c.getresponse()
    body = r.read()
    elapsed = (time.perf_counter() - started) * 1000
    assert r.status == 200, (path, r.status, body[:200])
    return elapsed, body


def stats(name, values):
    values = sorted(values)
    p = lambda q: values[min(len(values) - 1, int(q * len(values)))]
    print(
        f"{name:<38} n={len(values):<5} mean={statistics.mean(values):6.2f}  "
        f"p50={p(0.5):6.2f}  p90={p(0.9):6.2f}  p99={p(0.99):6.2f}  max={values[-1]:6.2f} ms"
    )


def slugs():
    c = conn()
    _, entities = fetch(c, f"{API}/entities?limit=100")
    _, people = fetch(c, f"{API}/people?limit=100")
    return (
        [e["slug"] for e in json.loads(entities)["items"]],
        [p["slug"] for p in json.loads(people)["items"]],
    )


def once_each(label):
    entities, people = slugs()
    c = conn()
    stats(f"{label}: entity card, 1st hit", [fetch(c, f"{API}/entities/{s}")[0] for s in entities])
    stats(f"{label}: person card, 1st hit", [fetch(c, f"{API}/people/{s}")[0] for s in people])


def repeated(name, path, n=500):
    c = conn()
    fetch(c, path)  # прогрев
    stats(f"{name} (keep-alive)", [fetch(c, path)[0] for _ in range(n)])
    stats(f"{name} (new conn)", [fetch(conn(), path)[0] for _ in range(n)])


if phase == "cold":
    once_each("L3 Meilisearch")
elif phase == "l2":
    once_each("L2 Redis")
elif phase == "warm":
    repeated("health (baseline)", "/health")
    repeated("L1 entity card dune-2021", f"{API}/entities/dune-2021")
    repeated("L1 person card", f"{API}/people/denis-villeneuve")
    repeated("Postgres list /entities?limit=20", f"{API}/entities?limit=20")
else:
    sys.exit(f"unknown phase {phase}")
