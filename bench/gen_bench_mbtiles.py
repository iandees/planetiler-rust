"""Deterministically generate the BenchmarkMbtilesRead input: 500,000 z14 tiles of 800 random bytes each
(columns 0-499 x rows 0-999, TMS row numbering, plain `tiles` table, no dedup), seeded so every run produces an
identical file.

Usage: python3 -I bench/gen_bench_mbtiles.py /tmp/ptbench/bench.mbtiles
"""
import os
import random
import sqlite3
import sys

out = sys.argv[1]
if os.path.exists(out):
    os.remove(out)
rng = random.Random(20261017)
db = sqlite3.connect(out)
db.execute("PRAGMA page_size=4096")
db.execute("CREATE TABLE metadata (name text, value text)")
db.execute(
    "CREATE TABLE tiles (zoom_level integer, tile_column integer, tile_row integer, tile_data blob, "
    "primary key(zoom_level, tile_column, tile_row))"
)
db.executemany(
    "INSERT INTO tiles VALUES (14, ?, ?, ?)",
    ((x, y, rng.randbytes(800)) for x in range(500) for y in range(1000)),
)
db.commit()
db.close()
