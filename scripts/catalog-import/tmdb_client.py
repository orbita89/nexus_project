"""Общий клиент TMDB для импортёров каталога: лимит запросов, повторы, чекпоинт.

Вынесен из `series_seasons_tmdb.py`, когда тем же понадобилось пользоваться `series_tmdb.py`:
два разных бэкоффа и два разных понимания лимита TMDB — верный способ словить 429 на массовом
прогоне.

    limiter = Limiter(rps=45)
    tmdb = Tmdb(key, limiter, workers=48)
    data = tmdb.get("/tv/1399", language="ru-RU")   # 404 → None, иначе dict

Чекпоинт — простой текстовый файл с id, по строке на штуку: `load_checkpoint` читает его в
множество, `append_checkpoint` дописывает и сбрасывает на диск. Переживает `kill -9` и обрыв
сети, поэтому прогон на сотни тысяч записей можно продолжать с места.
"""

import logging
import random
import signal
import threading
import time
from pathlib import Path

import requests

TMDB = "https://api.themoviedb.org/3"
RETRIES = 7
CONNECT_TIMEOUT = 5
READ_TIMEOUT = 15

log = logging.getLogger("tmdb")


class Limiter:
    """Token bucket: общий потолок запросов в секунду на все потоки.

    Скорость задаётся лимитом, а не числом воркеров, поэтому потоков можно ставить много —
    лишние просто ждут. На 429 лимит снижается (AIMD) и восстанавливается по секунде.
    """

    def __init__(self, rps: float):
        self.base = max(0.1, rps)
        self.rps = self.base
        self.allowance = self.base
        self.last = time.monotonic()
        self.lock = threading.Lock()
        self.resume_at = 0.0  # пауза для всех потоков после 429

    def take(self) -> None:
        while True:
            with self.lock:
                now = time.monotonic()
                if now < self.resume_at:
                    pause = self.resume_at - now
                else:
                    self.allowance = min(self.rps, self.allowance + (now - self.last) * self.rps)
                    self.last = now
                    if self.allowance >= 1.0:
                        self.allowance -= 1.0
                        # Медленное восстановление после снижения.
                        if self.rps < self.base:
                            self.rps = min(self.base, self.rps + self.base * 0.01)
                        return
                    pause = (1.0 - self.allowance) / self.rps
            time.sleep(min(pause, 1.0))

    def back_off(self, retry_after: float) -> None:
        """429: притормозить всех и снизить лимит на 20%."""
        with self.lock:
            self.resume_at = max(self.resume_at, time.monotonic() + retry_after)
            self.rps = max(self.base * 0.1, self.rps * 0.8)


class Tmdb:
    """Клиент TMDB с повторами: v4-токен (`eyJ…`) идёт заголовком, v3-ключ — параметром.

    Ошибки сети, таймауты и SSL-сбои корпоративного прокси транзиентные — повторяются
    экспоненциально с джиттером, а не фиксированной паузой в пять секунд.
    """

    def __init__(self, key: str, limiter: Limiter, workers: int = 16, verify: bool = True):
        self.session = requests.Session()
        pool = max(32, workers * 2)
        adapter = requests.adapters.HTTPAdapter(pool_connections=pool, pool_maxsize=pool)
        self.session.mount("https://", adapter)
        self.session.mount("http://", adapter)
        self.session.verify = verify
        self.limiter = limiter
        self.stats = {"requests": 0, "retry_429": 0, "retry_ssl": 0, "retry_timeout": 0,
                      "retry_other": 0}
        self.lock = threading.Lock()
        self.params = {}
        if key.startswith("eyJ"):
            self.session.headers["Authorization"] = f"Bearer {key}"
        else:
            self.params["api_key"] = key

    def count(self, key: str) -> None:
        with self.lock:
            self.stats[key] += 1

    def get(self, path: str, **params) -> dict | None:
        """Ответ TMDB; 404 — `None` (сущность скрыта или удалена)."""
        for attempt in range(RETRIES + 1):
            self.limiter.take()
            kind = "retry_other"
            try:
                self.count("requests")
                response = self.session.get(
                    f"{TMDB}{path}", timeout=(CONNECT_TIMEOUT, READ_TIMEOUT),
                    params={**self.params, **params})
                if response.status_code == 404:
                    return None
                if response.status_code == 429:
                    retry_after = float(response.headers.get("Retry-After") or 1)
                    self.limiter.back_off(retry_after)
                    kind = "retry_429"
                    raise RuntimeError("HTTP 429")
                if response.status_code >= 500:
                    raise RuntimeError(f"HTTP {response.status_code}")
                response.raise_for_status()
                return response.json()
            except (requests.RequestException, ValueError, RuntimeError) as error:
                if kind != "retry_429":
                    if isinstance(error, requests.exceptions.SSLError):
                        kind = "retry_ssl"
                    elif isinstance(error, requests.exceptions.Timeout):
                        kind = "retry_timeout"
                self.count(kind)
                if attempt == RETRIES:
                    raise RuntimeError(f"{path}: {error}") from error
                # На 429 паузу уже держит лимитер, здесь — только джиттер.
                pause = min(8.0, 0.25 * 2 ** attempt) * (0.5 + random.random())
                log.debug("%s: %s — повтор через %.1f с", path, error, pause)
                time.sleep(pause)
        raise RuntimeError("unreachable")


# ---------------------------------------------------------------- чекпоинт


def load_checkpoint(path: Path | None) -> set[int]:
    """Уже обработанные id. Нет файла — пустое множество, прогон начинается с начала."""
    if not path or not path.exists():
        return set()
    done = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line.isdigit():
            done.add(int(line))
    return done


def append_checkpoint(path: Path | None, ids) -> None:
    """Дописать id и сбросить на диск: прогон должен переживать `kill -9`."""
    ids = [str(item) for item in ids if item is not None]
    if not path or not ids:
        return
    with path.open("a", encoding="utf-8") as handle:
        handle.write("".join(f"{item}\n" for item in ids))
        handle.flush()


def stop_event() -> threading.Event:
    """SIGINT: первый — доработать очередь и выйти, второй — выйти сразу."""
    stop = threading.Event()

    def on_signal(_sig, _frame):
        if stop.is_set():
            raise KeyboardInterrupt
        log.warning("SIGINT: дорабатываю очередь, повтор — выход сразу")
        stop.set()

    signal.signal(signal.SIGINT, on_signal)
    return stop
