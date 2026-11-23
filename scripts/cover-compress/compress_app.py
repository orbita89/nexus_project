#!/usr/bin/env python3
"""Мини-приложение: сжатие обложек в AVIF тем же рецептом, что выбран вручную.

Запуск:  python3 scripts/cover-compress/compress_app.py  → открыть http://127.0.0.1:8765
Перетащите одну или несколько картинок (JPG, PNG, WebP, AVIF…) — готовые .avif ложатся в
~/Downloads/compressed/ (папка — флаг --out).

Рецепт (cover-variants-5/07_crf32_qm_nlm, ~19 КБ вместо ~284 КБ JPEG):
  ширина 600 px (меньшие не увеличиваются) → лёгкое шумоподавление nlmeans=s=2
  → AV1 10 бит, crf 32, tune=ssim, матрицы квантования (enable-qm), libaom still-picture.

Нужен только ffmpeg с libaom-av1. Без сторонних пакетов Python. Слушает лишь 127.0.0.1.
"""

import argparse
import json
import re
import shutil
import subprocess
import tempfile
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, quote, unquote, urlparse

DEFAULT_WIDTH = 600
DEFAULT_CRF = 32
MAX_UPLOAD = 100 * 1024 * 1024


def safe_stem(name: str) -> str:
    stem = Path(name).stem or "image"
    stem = re.sub(r"[^\w\-. ]+", "_", stem, flags=re.UNICODE).strip(" .") or "image"
    return stem[:120]


def compress(src: Path, dst: Path, width: int, crf: int, denoise: bool) -> None:
    filters = [f"scale='min({width},iw)':-2:flags=lanczos"]
    if denoise:
        filters.append("nlmeans=s=2:p=7:r=11")
    filters.append("format=yuv420p10le")
    cmd = [
        "ffmpeg", "-v", "error", "-y", "-i", str(src),
        "-vf", ",".join(filters), "-frames:v", "1",
        "-c:v", "libaom-av1", "-still-picture", "1", "-cpu-used", "3",
        "-crf", str(crf), "-b:v", "0", "-tune", "ssim",
        "-aom-params", "enable-qm=1:qm-min=5:qm-max=15",
        str(dst),
    ]
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=300)
    if result.returncode != 0 or not dst.exists():
        raise RuntimeError(result.stderr.strip() or "ffmpeg failed")


def probe(path: Path) -> str:
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", "v:0",
         "-show_entries", "stream=width,height", "-of", "csv=p=0:s=x", str(path)],
        capture_output=True, text=True,
    ).stdout.strip()
    return out.splitlines()[0] if out else "?"


PAGE = """<!doctype html><html lang="ru"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Сжатие обложек</title>
<style>
:root{--bg:#f6f6f4;--card:#fff;--fg:#1d1d1b;--mute:#6b6b66;--line:#ddd;--acc:#2f6fdf;--ok:#1f8a4c;--err:#c0392b}
@media (prefers-color-scheme:dark){:root{--bg:#161615;--card:#1f1f1d;--fg:#ecece8;--mute:#9a9a94;--line:#333;--acc:#6b9cff;--ok:#4cc27e;--err:#ff7a6b}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.45 system-ui,sans-serif}
main{max-width:1000px;margin:0 auto;padding:24px 16px}
h1{margin:0 0 4px;font-size:22px}.mute{color:var(--mute);font-size:13px}
#drop{margin:18px 0;border:2px dashed var(--line);border-radius:12px;padding:36px 16px;text-align:center;background:var(--card);cursor:pointer}
#drop.on{border-color:var(--acc)}
.opts{display:flex;flex-wrap:wrap;gap:16px;align-items:center;font-size:14px}
.opts input[type=number]{width:72px;padding:4px 6px;background:var(--card);color:var(--fg);border:1px solid var(--line);border-radius:6px}
button{background:var(--acc);color:#fff;border:0;border-radius:8px;padding:8px 14px;font-size:14px;cursor:pointer}
#list{display:grid;grid-template-columns:repeat(auto-fill,minmax(220px,1fr));gap:14px;margin-top:18px}
.item{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:10px;font-size:13px}
.item img{width:100%;aspect-ratio:2/3;object-fit:contain;background:#0001;border-radius:6px}
.name{font-weight:600;word-break:break-all;margin:6px 0 2px}.ok{color:var(--ok)}.err{color:var(--err)}
.sum{margin-top:14px;font-weight:600}
</style></head><body><main>
<h1>Сжатие обложек</h1>
<div class="mute">Рецепт по умолчанию: ширина 600 px, лёгкое шумоподавление, AV1 10 бит, crf 32 (~19 КБ).
Готовые файлы: <code>__OUT__</code></div>
<div id="drop">Перетащите сюда одну или несколько картинок<br><span class="mute">или нажмите, чтобы выбрать файлы</span>
<input id="file" type="file" accept="image/*,.avif" multiple hidden></div>
<div class="opts">
<label>Ширина, px <input id="w" type="number" min="100" max="4000" value="__W__"></label>
<label>crf (больше — меньше файл) <input id="crf" type="number" min="0" max="63" value="__CRF__"></label>
<label><input id="dn" type="checkbox" checked> шумоподавление</label>
<button id="reset" type="button">Сбросить к рецепту</button>
</div>
<div class="sum" id="sum"></div>
<div id="list"></div>
</main><script>
const drop=document.getElementById('drop'),file=document.getElementById('file'),list=document.getElementById('list'),sum=document.getElementById('sum');
let totalIn=0,totalOut=0,done=0;
const kb=b=>(b/1024).toFixed(1)+' КБ';
drop.onclick=()=>file.click();
drop.ondragover=e=>{e.preventDefault();drop.classList.add('on')};
drop.ondragleave=()=>drop.classList.remove('on');
drop.ondrop=e=>{e.preventDefault();drop.classList.remove('on');run([...e.dataTransfer.files])};
file.onchange=()=>{run([...file.files]);file.value=''};
document.getElementById('reset').onclick=()=>{w.value=__W__;crf.value=__CRF__;dn.checked=true};
async function run(files){
  const queue=files.filter(f=>f.type.startsWith('image/')||/\\.avif$/i.test(f.name));
  const workers=Array.from({length:2},async()=>{while(queue.length) await one(queue.shift())});
  await Promise.all(workers);
}
async function one(f){
  const card=document.createElement('div');card.className='item';
  card.innerHTML='<div class="name"></div><div class="mute">Сжимаю…</div>';
  card.querySelector('.name').textContent=f.name;list.prepend(card);
  const q=new URLSearchParams({name:f.name,width:w.value,crf:crf.value,denoise:dn.checked?'1':'0'});
  try{
    const r=await fetch('/compress?'+q,{method:'POST',body:f});
    const j=await r.json();if(!r.ok) throw new Error(j.error||r.status);
    totalIn+=j.in_bytes;totalOut+=j.out_bytes;done++;
    card.innerHTML='<img alt=""><div class="name"></div><div></div><div class="mute"></div>';
    card.querySelector('img').src=j.url+'?t='+Date.now();
    card.querySelector('.name').textContent=j.file;
    card.children[2].innerHTML='<span class="ok">'+kb(j.out_bytes)+'</span> из '+kb(j.in_bytes)+' (−'+Math.round(100-100*j.out_bytes/j.in_bytes)+'%)';
    card.children[3].textContent=j.size+' px · '+j.seconds+' с';
    sum.textContent='Готово: '+done+' · было '+kb(totalIn)+' → стало '+kb(totalOut);
  }catch(e){card.querySelector('.mute').className='err';card.querySelector('.err').textContent='Ошибка: '+e.message}
}
</script></body></html>"""


def make_handler(out_dir: Path):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, fmt, *args):
            print("%s" % (fmt % args))

        def send(self, code: int, body: bytes, ctype: str) -> None:
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)

        def json(self, code: int, data: dict) -> None:
            self.send(code, json.dumps(data, ensure_ascii=False).encode(), "application/json")

        def do_GET(self):
            path = urlparse(self.path).path
            if path == "/":
                page = (PAGE.replace("__OUT__", str(out_dir))
                        .replace("__W__", str(DEFAULT_WIDTH)).replace("__CRF__", str(DEFAULT_CRF)))
                return self.send(200, page.encode(), "text/html; charset=utf-8")
            if path.startswith("/out/"):
                name = Path(unquote(path[5:])).name
                target = out_dir / name
                if target.is_file():
                    return self.send(200, target.read_bytes(), "image/avif")
            self.send(404, b"not found", "text/plain")

        def do_POST(self):
            url = urlparse(self.path)
            if url.path != "/compress":
                return self.json(404, {"error": "not found"})
            q = {k: v[0] for k, v in parse_qs(url.query).items()}
            try:
                width = max(100, min(4000, int(q.get("width", DEFAULT_WIDTH))))
                crf = max(0, min(63, int(q.get("crf", DEFAULT_CRF))))
            except ValueError:
                return self.json(400, {"error": "width/crf must be numbers"})
            denoise = q.get("denoise", "1") == "1"
            length = int(self.headers.get("Content-Length") or 0)
            if not 0 < length <= MAX_UPLOAD:
                return self.json(400, {"error": "empty or larger than 100 MB"})
            data = self.rfile.read(length)
            name = safe_stem(q.get("name", "image"))
            started = time.monotonic()
            with tempfile.TemporaryDirectory() as tmp:
                src = Path(tmp) / "src"
                src.write_bytes(data)
                tmp_out = Path(tmp) / "out.avif"
                try:
                    compress(src, tmp_out, width, crf, denoise)
                except Exception as e:  # noqa: BLE001 — ошибку ffmpeg показываем в карточке
                    return self.json(400, {"error": str(e)[:300]})
                target = out_dir / f"{name}.avif"
                shutil.move(str(tmp_out), target)
            return self.json(200, {
                "file": target.name,
                "url": "/out/" + quote(target.name),
                "in_bytes": len(data),
                "out_bytes": target.stat().st_size,
                "size": probe(target),
                "seconds": round(time.monotonic() - started, 1),
            })

    return Handler


def main() -> None:
    parser = argparse.ArgumentParser(description="Сжатие обложек в AVIF (локальная страница)")
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--out", type=Path, default=Path.home() / "Downloads" / "compressed")
    args = parser.parse_args()
    if not shutil.which("ffmpeg"):
        raise SystemExit("ffmpeg не найден: установите ffmpeg с libaom-av1")
    args.out.mkdir(parents=True, exist_ok=True)
    server = ThreadingHTTPServer(("127.0.0.1", args.port), make_handler(args.out))
    print(f"Откройте http://127.0.0.1:{args.port}  · готовые файлы → {args.out}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
