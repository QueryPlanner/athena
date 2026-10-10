"""Builds the SQLite file of a health export from the chunks Athena uploaded.

Run once by Athena in the sandbox (src/health/export.rs), as
    python3 load.py WORKDIR DEST
where WORKDIR holds meta.json and chunk-*.ndjson, and DEST is the final path.
Python standard library only: the sandbox image has python3 but no sqlite3
shell.

Each chunk line is a JSON object:
    {"t": data_type, "k": point_key, "s": start_ms|null, "e": end_ms|null,
     "d": civil_date|null, "src": source|null, "v": <projected point JSON>}

The database is built as WORKDIR/health.sqlite.tmp and renamed to DEST in one
step, so DEST is either the previous file or the whole new one. The last line
printed on success is one JSON object: {"rows": {data_type: n}, "bytes": n}.
On any failure nothing is left at DEST that was not there before, WORKDIR is
removed, and the exit status is 1 with the reason on stderr.
"""
import glob
import json
import os
import shutil
import sqlite3
import sys

BATCH = 5000

SCHEMA = """
CREATE TABLE points (
    data_type TEXT NOT NULL,
    point_key TEXT NOT NULL,
    start_ms  INTEGER,
    end_ms    INTEGER,
    civil_date TEXT,
    source    TEXT,
    value     TEXT NOT NULL,
    UNIQUE (data_type, point_key)
);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"""

INDEXES = """
CREATE INDEX points_by_date ON points (data_type, civil_date);
CREATE INDEX points_by_start ON points (data_type, start_ms);
"""

# The values are numbers or numeric strings (Google sends 64-bit integers as
# strings), so each is cast.
VIEWS = """
CREATE VIEW heart_rate AS
    SELECT start_ms, civil_date, source,
           CAST(json_extract(value, '$.beatsPerMinute') AS REAL) AS bpm
    FROM points WHERE data_type = 'heart-rate';
CREATE VIEW steps AS
    SELECT start_ms, end_ms, civil_date, source,
           CAST(json_extract(value, '$.count') AS INTEGER) AS count
    FROM points WHERE data_type = 'steps';
CREATE VIEW weight AS
    SELECT start_ms, civil_date, source,
           CAST(json_extract(value, '$.weightGrams') AS REAL) / 1000.0 AS kg
    FROM points WHERE data_type = 'weight';
CREATE VIEW sleep AS
    SELECT start_ms, end_ms, civil_date, source,
           COALESCE(CAST(json_extract(value, '$.summary.minutesAsleep') AS INTEGER),
                    CAST(ROUND((end_ms - start_ms) / 60000.0) AS INTEGER)) AS minutes
    FROM points WHERE data_type = 'sleep';
CREATE VIEW hrv AS
    SELECT start_ms, civil_date, source,
           CAST(json_extract(value,
                '$.rootMeanSquareOfSuccessiveDifferencesMilliseconds') AS REAL) AS rmssd_ms
    FROM points WHERE data_type = 'heart-rate-variability';
CREATE VIEW spo2 AS
    SELECT start_ms, civil_date, source,
           CAST(json_extract(value, '$.percentage') AS REAL) AS percent
    FROM points WHERE data_type = 'oxygen-saturation';
"""

INSERT = (
    "INSERT OR REPLACE INTO points "
    "(data_type, point_key, start_ms, end_ms, civil_date, source, value) "
    "VALUES (?, ?, ?, ?, ?, ?, ?)"
)


def rows_of(path):
    """The rows of one chunk file, as tuples for INSERT."""
    with open(path, encoding="utf-8") as chunk:
        for number, line in enumerate(chunk, 1):
            if not line.strip():
                continue
            try:
                p = json.loads(line)
                yield (
                    p["t"],
                    p["k"],
                    p["s"],
                    p["e"],
                    p["d"],
                    p["src"],
                    json.dumps(p["v"], separators=(",", ":"), ensure_ascii=False),
                )
            except (ValueError, KeyError, TypeError) as e:
                raise ValueError(f"{os.path.basename(path)} line {number}: {e}")


def build(workdir, dest):
    with open(os.path.join(workdir, "meta.json"), encoding="utf-8") as f:
        meta = json.load(f)
    tmp = os.path.join(workdir, "health.sqlite.tmp")
    if os.path.exists(tmp):
        os.remove(tmp)
    db = sqlite3.connect(tmp)
    try:
        # The file is rebuilt from scratch if this fails, so durability
        # buys nothing here.
        db.execute("PRAGMA journal_mode = OFF")
        db.execute("PRAGMA synchronous = OFF")
        db.executescript(SCHEMA)
        batch = []
        for path in sorted(glob.glob(os.path.join(workdir, "chunk-*.ndjson"))):
            for row in rows_of(path):
                batch.append(row)
                if len(batch) >= BATCH:
                    db.executemany(INSERT, batch)
                    batch = []
        if batch:
            db.executemany(INSERT, batch)
        db.executescript(INDEXES)
        db.executescript(VIEWS)
        counts = dict(
            db.execute("SELECT data_type, COUNT(*) FROM points GROUP BY data_type")
        )
        meta["rows"] = counts
        db.executemany(
            "INSERT INTO meta (key, value) VALUES (?, ?)",
            [
                (k, v if isinstance(v, str) else json.dumps(v))
                for k, v in meta.items()
                if v is not None
            ],
        )
        db.commit()
    finally:
        db.close()
    os.chmod(tmp, 0o600)
    os.makedirs(os.path.dirname(dest), mode=0o700, exist_ok=True)
    os.replace(tmp, dest)
    return counts, os.path.getsize(dest)


def main(argv):
    if len(argv) != 3:
        print("usage: load.py WORKDIR DEST", file=sys.stderr)
        return 2
    workdir, dest = argv[1], argv[2]
    try:
        counts, size = build(workdir, dest)
    except Exception as e:  # report, clean up, fail
        print(f"{type(e).__name__}: {e}", file=sys.stderr)
        shutil.rmtree(workdir, ignore_errors=True)
        return 1
    shutil.rmtree(workdir, ignore_errors=True)
    print(json.dumps({"rows": counts, "bytes": size}))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
