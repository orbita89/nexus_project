#!/usr/bin/env python3
"""Идемпотентный сид-скрипт для заполнения фрагментов полотен и разбора картин.

Читает файлы JSON из seeds/paintings/ (по одному на картину, со slug в имени/содержимом),
валидирует структуру данных (размеры кропов, лимиты символов, количество фрагментов и тезисов)
и обновляет `entities.metadata` и `entity_fragments` в PostgreSQL.

Использование:
    scripts/seed_painting_fragments.py
    scripts/seed_painting_fragments.py --slug the-night-watch
    scripts/seed_painting_fragments.py --dir seeds/paintings --check-only
"""

import argparse
import glob
import json
import os
import sys

try:
    import psycopg2
    from psycopg2.extras import Json
except ImportError:
    # Попробуем использовать venv проекта, если доступен
    venv_site = os.path.join(
        os.path.dirname(__file__), "catalog-import", ".venv", "lib"
    )
    for root, dirs, files in os.walk(venv_site):
        if "site-packages" in root:
            sys.path.insert(0, root)
            break
    try:
        import psycopg2
        from psycopg2.extras import Json
    except ImportError:
        psycopg2 = None
        Json = None


MAX_FRAGMENTS = 20
MAX_FRAGMENT_TITLE = 120
MAX_FRAGMENT_DESCRIPTION = 600
MIN_FRAGMENT_AREA = 0.025  # ~3% площади полотна во избежание замыливания

MAX_ANALYSIS_TEXT = 10000
MAX_ANALYSIS_POINTS = 10
MAX_POINT_TITLE = 120
MAX_POINT_TEXT = 600


def validate_crop(crop, title):
    if not isinstance(crop, dict):
        raise ValueError(f"[{title}] crop must be a dict")
    for key in ("x", "y", "width", "height"):
        if key not in crop:
            raise ValueError(f"[{title}] crop is missing '{key}'")
        val = crop[key]
        if not isinstance(val, (int, float)):
            raise ValueError(f"[{title}] crop.{key} must be a number, got {val!r}")

    x = float(crop["x"])
    y = float(crop["y"])
    w = float(crop["width"])
    h = float(crop["height"])

    if not (0.0 <= x <= 1.0):
        raise ValueError(f"[{title}] crop.x must be between 0 and 1, got {x}")
    if not (0.0 <= y <= 1.0):
        raise ValueError(f"[{title}] crop.y must be between 0 and 1, got {y}")
    if w <= 0.0 or w > 1.0:
        raise ValueError(f"[{title}] crop.width must be > 0 and <= 1, got {w}")
    if h <= 0.0 or h > 1.0:
        raise ValueError(f"[{title}] crop.height must be > 0 and <= 1, got {h}")

    eps = 1e-6
    if x + w > 1.0 + eps:
        raise ValueError(f"[{title}] crop exceeds right edge: x ({x}) + width ({w}) = {x + w} > 1")
    if y + h > 1.0 + eps:
        raise ValueError(f"[{title}] crop exceeds bottom edge: y ({y}) + height ({h}) = {y + h} > 1")

    area = w * h
    if area < MIN_FRAGMENT_AREA:
        raise ValueError(
            f"[{title}] crop area ({area:.4f}) is smaller than minimum ~3% ({MIN_FRAGMENT_AREA})"
        )


def validate_painting_data(data, filepath="<memory>"):
    slug = data.get("slug")
    if not slug or not isinstance(slug, str):
        raise ValueError(f"[{filepath}] missing or invalid 'slug'")

    # iiif_base (опционально)
    iiif_base = data.get("iiif_base")
    if iiif_base is not None:
        if not isinstance(iiif_base, str) or not iiif_base.startswith(("http://", "https://")):
            raise ValueError(f"[{slug}] invalid iiif_base URL: {iiif_base!r}")

    # analysis
    analysis = data.get("analysis")
    if not analysis or not isinstance(analysis, dict):
        raise ValueError(f"[{slug}] missing or invalid 'analysis' dict")

    text = analysis.get("text")
    if not text or not isinstance(text, str):
        raise ValueError(f"[{slug}] analysis.text must be non-empty string")
    text = text.strip()
    if len(text) > MAX_ANALYSIS_TEXT:
        raise ValueError(f"[{slug}] analysis.text exceeds {MAX_ANALYSIS_TEXT} chars")

    paragraphs = [p for p in text.split("\n\n") if p.strip()]
    if not (2 <= len(paragraphs) <= 4):
        raise ValueError(f"[{slug}] analysis.text must contain 2-4 paragraphs, got {len(paragraphs)}")

    points = analysis.get("points")
    if not isinstance(points, list):
        raise ValueError(f"[{slug}] analysis.points must be a list")
    if not (2 <= len(points) <= MAX_ANALYSIS_POINTS):
        raise ValueError(
            f"[{slug}] analysis.points count must be between 2 and {MAX_ANALYSIS_POINTS}, got {len(points)}"
        )

    for i, pt in enumerate(points):
        if not isinstance(pt, dict):
            raise ValueError(f"[{slug}] analysis.points[{i}] must be a dict")
        p_title = pt.get("title", "").strip()
        p_text = pt.get("text", "").strip()
        if not p_title:
            raise ValueError(f"[{slug}] analysis.points[{i}].title is empty")
        if len(p_title) > MAX_POINT_TITLE:
            raise ValueError(f"[{slug}] analysis.points[{i}].title exceeds {MAX_POINT_TITLE} chars")
        if not p_text:
            raise ValueError(f"[{slug}] analysis.points[{i}].text is empty")
        if len(p_text) > MAX_POINT_TEXT:
            raise ValueError(f"[{slug}] analysis.points[{i}].text exceeds {MAX_POINT_TEXT} chars")

    # fragments
    fragments = data.get("fragments")
    if not isinstance(fragments, list):
        raise ValueError(f"[{slug}] fragments must be a list")
    if not (3 <= len(fragments) <= MAX_FRAGMENTS):
        raise ValueError(
            f"[{slug}] fragments count must be between 3 and {MAX_FRAGMENTS}, got {len(fragments)}"
        )

    for i, frag in enumerate(fragments):
        if not isinstance(frag, dict):
            raise ValueError(f"[{slug}] fragments[{i}] must be a dict")
        f_title = frag.get("title", "").strip()
        if not f_title:
            raise ValueError(f"[{slug}] fragments[{i}].title is empty")
        if len(f_title) > MAX_FRAGMENT_TITLE:
            raise ValueError(f"[{slug}] fragments[{i}].title exceeds {MAX_FRAGMENT_TITLE} chars")

        f_desc = frag.get("description")
        if f_desc is not None:
            if not isinstance(f_desc, str):
                raise ValueError(f"[{slug}] fragments[{i}].description must be a string")
            f_desc = f_desc.strip()
            if len(f_desc) > MAX_FRAGMENT_DESCRIPTION:
                raise ValueError(
                    f"[{slug}] fragments[{i}].description exceeds {MAX_FRAGMENT_DESCRIPTION} chars"
                )

        validate_crop(frag.get("crop"), f"{slug}:{f_title}")


def load_paintings(data_dir, target_slug=None):
    files = glob.glob(os.path.join(data_dir, "*.json"))
    paintings = []
    for filepath in sorted(files):
        with open(filepath, "r", encoding="utf-8") as f:
            try:
                data = json.load(f)
            except Exception as e:
                raise ValueError(f"Failed to parse JSON {filepath}: {e}")
        validate_painting_data(data, filepath)
        if target_slug and data.get("slug") != target_slug:
            continue
        paintings.append(data)
    return paintings


def seed_paintings(conn, paintings, check_only=False):
    cur = conn.cursor()
    updated_count = 0
    total_fragments = 0

    for p in paintings:
        slug = p["slug"]
        cur.execute(
            "SELECT id, kind, metadata FROM entities WHERE slug = %s",
            (slug,),
        )
        row = cur.fetchone()
        if not row:
            print(f"WARN: Entity with slug '{slug}' not found in database. Skipping.")
            continue

        entity_id, kind, meta = row
        if kind != "painting":
            print(f"WARN: Entity '{slug}' has kind '{kind}', expected 'painting'. Skipping.")
            continue

        if meta is None:
            meta = {}
        elif isinstance(meta, str):
            meta = json.loads(meta)

        # Обновляем analysis и iiif_base в metadata
        meta["analysis"] = p["analysis"]
        if p.get("iiif_base"):
            meta["iiif_base"] = p["iiif_base"].rstrip("/")

        fragments = p.get("fragments", [])

        if not check_only:
            # 1. Обновляем metadata сущности
            cur.execute(
                "UPDATE entities SET metadata = %s, updated_at = now() WHERE id = %s",
                (Json(meta), entity_id),
            )

            # 2. Идемпотентно обновляем entity_fragments
            cur.execute("DELETE FROM entity_fragments WHERE entity_id = %s", (entity_id,))

            for default_pos, frag in enumerate(fragments, start=1):
                pos = frag.get("position", default_pos)
                crop = frag["crop"]
                desc = frag.get("description")
                if desc:
                    desc = desc.strip()
                cur.execute(
                    """
                    INSERT INTO entity_fragments
                        (id, entity_id, title, description, crop_x, crop_y, crop_width, crop_height, position, created_at, updated_at)
                    VALUES
                        (gen_random_uuid(), %s, %s, %s, %s, %s, %s, %s, %s, now(), now())
                    """,
                    (
                        entity_id,
                        frag["title"].strip(),
                        desc,
                        float(crop["x"]),
                        float(crop["y"]),
                        float(crop["width"]),
                        float(crop["height"]),
                        int(pos),
                    ),
                )

        updated_count += 1
        total_fragments += len(fragments)
        print(f"OK: '{slug}' -> {len(fragments)} fragments, analysis saved.")

    if not check_only:
        conn.commit()

    return updated_count, total_fragments


def main():
    parser = argparse.ArgumentParser(
        description="Seed painting fragments and analysis data into database"
    )
    parser.add_argument(
        "--dir",
        default=os.path.join(os.path.dirname(__file__), "..", "seeds", "paintings"),
        help="Directory with painting JSON files (default: seeds/paintings)",
    )
    parser.add_argument(
        "--slug",
        default=None,
        help="Specific painting slug to seed (optional)",
    )
    parser.add_argument(
        "--check-only",
        action="store_true",
        help="Only validate files and database presence without making changes",
    )
    parser.add_argument(
        "--database-url",
        default=os.environ.get(
            "DATABASE_URL",
            "postgres://nexus_user:nexus_password@localhost:5432/nexus_db",
        ),
        help="PostgreSQL connection string",
    )
    args = parser.parse_args()

    if not os.path.isdir(args.dir):
        print(f"Error: directory '{args.dir}' does not exist.", file=sys.stderr)
        sys.exit(1)

    print(f"Loading painting data from {args.dir}...")
    try:
        paintings = load_paintings(args.dir, target_slug=args.slug)
    except Exception as e:
        print(f"Validation error: {e}", file=sys.stderr)
        sys.exit(1)

    print(f"Loaded and validated {len(paintings)} painting(s).")
    if not paintings:
        print("No paintings matched. Exiting.")
        sys.exit(0)

    if psycopg2 is None:
        print("Error: psycopg2 is not installed. Please install psycopg2-binary or use the project venv.", file=sys.stderr)
        sys.exit(1)

    try:
        conn = psycopg2.connect(args.database_url)
    except Exception as e:
        print(f"Database connection error: {e}", file=sys.stderr)
        sys.exit(1)

    try:
        updated, frags = seed_paintings(conn, paintings, check_only=args.check_only)
        action = "Validated" if args.check_only else "Seeded"
        print(f"Done! {action} {updated} paintings with {frags} total fragments.")
    finally:
        conn.close()


if __name__ == "__main__":
    main()
