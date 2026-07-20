#!/usr/bin/env python3
"""Validate the fixtures in ``fixtures/`` against the Aether engine contract.

Stdlib only — no ``jsonschema`` dependency. Two kinds of checks:

  (a) Structural checks of every ``fixtures/jobs/*.json`` (and the CSV/waypoint
      companions) against the required fields / enums documented in
      ``docs/CONTRACT.md`` and mirrored in ``schemas/``.
  (b) Byte-by-byte parsing of the binary fixtures in ``fixtures/formats/``
      (magic, header offsets, index/footer widths, row strides) per §6/§7.

Exit code 0 means every check passed. Any mismatch is reported loudly and the
script exits 1. Nothing here is wired into CI yet.
"""

import json
import os
import struct
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
JOBS_DIR = os.path.join(REPO_ROOT, "fixtures", "jobs")
FORMATS_DIR = os.path.join(REPO_ROOT, "fixtures", "formats")

ERRORS = []


def check(cond, msg):
    if not cond:
        ERRORS.append(msg)
    return cond


def _req(obj, key, ctx):
    return check(isinstance(obj, dict) and key in obj, f"{ctx}: missing required key '{key}'")


def _is_num(v):
    return isinstance(v, (int, float)) and not isinstance(v, bool)


def _enum(obj, key, allowed, ctx):
    if isinstance(obj, dict) and key in obj:
        check(obj[key] in allowed, f"{ctx}: '{key}'={obj[key]!r} not in {sorted(allowed)}")


# ── (a) JSON job validators ──────────────────────────────────────────────────

TASK_TYPES = {"SINGLE", "P2P", "BATCH_P2P", "PATH"}
PROP_MODELS = {"LOS", "SIMPLE_LOSS", "ITM", "MIN_ALT"}
BACKENDS = {"AUTO", "GPU", "CPU", "GPU_ONLY"}
MODES = {"AGL", "AMSL"}


def validate_job(obj, ctx):
    check(isinstance(obj, dict), f"{ctx}: job must be an object")
    if not isinstance(obj, dict):
        return

    # tx
    if _req(obj, "tx", ctx):
        tx = obj["tx"]
        check(isinstance(tx, dict), f"{ctx}.tx: must be object")
        if isinstance(tx, dict):
            if _req(tx, "freq_mhz", f"{ctx}.tx"):
                check(_is_num(tx["freq_mhz"]) and tx["freq_mhz"] > 0,
                      f"{ctx}.tx.freq_mhz: must be a positive number")
            _enum(tx, "mode", MODES, f"{ctx}.tx")

    # rx (optional)
    if isinstance(obj.get("rx"), dict):
        _enum(obj["rx"], "mode", MODES, f"{ctx}.rx")

    # analysis
    if _req(obj, "analysis", ctx):
        an = obj["analysis"]
        check(isinstance(an, dict), f"{ctx}.analysis: must be object")
        if isinstance(an, dict):
            if _req(an, "propagation_model", f"{ctx}.analysis"):
                check(an["propagation_model"] in PROP_MODELS,
                      f"{ctx}.analysis.propagation_model={an['propagation_model']!r} not in {sorted(PROP_MODELS)}")
            if _req(an, "max_range_km", f"{ctx}.analysis"):
                check(isinstance(an["max_range_km"], int) and not isinstance(an["max_range_km"], bool)
                      and an["max_range_km"] >= 0, f"{ctx}.analysis.max_range_km: must be a non-negative integer")
            if _req(an, "resolution_m", f"{ctx}.analysis"):
                check(_is_num(an["resolution_m"]) and an["resolution_m"] > 0,
                      f"{ctx}.analysis.resolution_m: must be a positive number")
            _enum(an, "task_type", TASK_TYPES, f"{ctx}.analysis")
            _enum(an, "compute_backend", BACKENDS, f"{ctx}.analysis")

            task = an.get("task_type", "SINGLE")
            if task == "PATH":
                check(isinstance(an.get("path_file"), str),
                      f"{ctx}.analysis: task_type PATH requires string 'path_file'")
            if task in ("BATCH_P2P", "P2P"):
                check(isinstance(an.get("batch_file"), str),
                      f"{ctx}.analysis: task_type {task} requires string 'batch_file'")

    # output
    if _req(obj, "output", ctx):
        out = obj["output"]
        if isinstance(out, dict):
            check(isinstance(out.get("directory"), str), f"{ctx}.output.directory: required string")
            check(isinstance(out.get("filename"), str), f"{ctx}.output.filename: required string")

    # propagation (optional)
    if isinstance(obj.get("propagation"), dict):
        pr = obj["propagation"]
        _enum(pr, "earth_radius_mode", {"FOUR_THIRDS", "ADVANCED"}, f"{ctx}.propagation")
        _enum(pr, "pol", {0, 1}, f"{ctx}.propagation")
        if "radio_climate" in pr:
            check(pr["radio_climate"] in range(1, 8), f"{ctx}.propagation.radio_climate: expected 1..7")


def validate_ingest(obj, ctx):
    jobs = obj if isinstance(obj, list) else [obj]
    for i, j in enumerate(jobs):
        c = f"{ctx}[{i}]" if isinstance(obj, list) else ctx
        check(isinstance(j, dict), f"{c}: must be object")
        if not isinstance(j, dict):
            continue
        for k in ("output_path", "ul_lat", "ul_lon", "resolution_m", "size_px", "swiss_tifs"):
            _req(j, k, c)
        check(isinstance(j.get("output_path"), str), f"{c}.output_path: string")
        check(_is_num(j.get("ul_lat")), f"{c}.ul_lat: number")
        check(_is_num(j.get("ul_lon")), f"{c}.ul_lon: number")
        check(_is_num(j.get("resolution_m")) and j.get("resolution_m", 0) > 0, f"{c}.resolution_m: positive number")
        check(isinstance(j.get("size_px"), int) and not isinstance(j.get("size_px"), bool), f"{c}.size_px: integer")
        check(isinstance(j.get("swiss_tifs"), list), f"{c}.swiss_tifs: array (may be empty)")
        if "format" in j and j["format"] is not None:
            _enum(j, "format", {"r16sint", "bc6h"}, c)


def validate_download(obj, ctx):
    check(isinstance(obj, dict), f"{ctx}: must be object")
    if not isinstance(obj, dict):
        return
    for k in ("url_template", "encoding", "output_dir", "zoom", "tiles"):
        _req(obj, k, ctx)
    check(isinstance(obj.get("url_template"), str), f"{ctx}.url_template: string")
    _enum(obj, "encoding", {"terrarium", "mapbox"}, ctx)
    check(isinstance(obj.get("output_dir"), str), f"{ctx}.output_dir: string")
    check(isinstance(obj.get("zoom"), int) and not isinstance(obj.get("zoom"), bool), f"{ctx}.zoom: integer")
    tiles = obj.get("tiles")
    check(isinstance(tiles, list) and len(tiles) >= 1, f"{ctx}.tiles: non-empty array")
    if isinstance(tiles, list):
        for i, t in enumerate(tiles):
            c = f"{ctx}.tiles[{i}]"
            for k in ("filename", "ul_lat", "ul_lon", "size_px", "resolution_m"):
                _req(t, k, c)
            if isinstance(t, dict):
                check(isinstance(t.get("filename"), str), f"{c}.filename: string")
                check(_is_num(t.get("ul_lat")), f"{c}.ul_lat: number")
                check(_is_num(t.get("ul_lon")), f"{c}.ul_lon: number")
                check(isinstance(t.get("size_px"), int) and not isinstance(t.get("size_px"), bool), f"{c}.size_px: integer")
                check(_is_num(t.get("resolution_m")) and t.get("resolution_m", 0) > 0, f"{c}.resolution_m: positive number")


def validate_batch_links(path, ctx):
    with open(path, encoding="utf-8") as f:
        rows = 0
        for ln in f:
            s = ln.strip()
            if not s or s.startswith("#"):
                continue
            parts = [p.strip() for p in s.split(",")]
            check(len(parts) >= 5, f"{ctx}: row needs >=5 columns, got {len(parts)}: {s!r}")
            if len(parts) >= 5:
                check(parts[0] in ("S", "T"), f"{ctx}: col0 Type must be S or T, got {parts[0]!r}")
                for idx, name in ((2, "Latitude"), (3, "Longitude"), (4, "Altitude_meters")):
                    try:
                        float(parts[idx])
                    except ValueError:
                        check(False, f"{ctx}: {name} not numeric: {parts[idx]!r}")
                if len(parts) >= 6:
                    check(parts[5].upper() in MODES, f"{ctx}: Mode must be AGL/AMSL, got {parts[5]!r}")
                rows += 1
        check(rows >= 1, f"{ctx}: needs at least one data row")


# ── (b) Binary fixture parsers ───────────────────────────────────────────────

def validate_abt(path, ctx):
    with open(path, "rb") as f:
        data = f.read()
    check(len(data) >= 44, f"{ctx}: shorter than the 44-byte header")
    if len(data) < 44:
        return
    check(data[0:4] == b"AETH", f"{ctx}: bad magic {data[0:4]!r} (expected b'AETH')")
    version = struct.unpack_from("<H", data, 4)[0]
    width = struct.unpack_from("<H", data, 6)[0]
    ul_lat = struct.unpack_from("<d", data, 8)[0]
    ul_lon = struct.unpack_from("<d", data, 16)[0]
    scale_y = struct.unpack_from("<d", data, 24)[0]
    scale_x = struct.unpack_from("<d", data, 32)[0]
    base_elev = struct.unpack_from("<h", data, 40)[0]
    row_stride = struct.unpack_from("<H", data, 42)[0]

    check(version in (1, 2), f"{ctx}: version {version} not in (1,2)")
    check(width == 16, f"{ctx}: expected width 16, got {width}")
    expected_stride = (width * 2 + 255) & ~255
    check(row_stride == expected_stride,
          f"{ctx}: row_stride {row_stride} != 256-aligned expected {expected_stride}")
    check(base_elev == 0, f"{ctx}: R16SINT base_elev should be 0, got {base_elev}")
    check(-90.0 <= ul_lat <= 90.0, f"{ctx}: ul_lat {ul_lat} out of range")
    check(-180.0 <= ul_lon <= 180.0, f"{ctx}: ul_lon {ul_lon} out of range")
    check(scale_x > 0 and scale_y > 0, f"{ctx}: scales must be positive ({scale_x}, {scale_y})")

    payload = data[44:]
    check(len(payload) == row_stride * width,
          f"{ctx}: payload {len(payload)} != row_stride*height {row_stride * width}")

    # Spot-check the deterministic ramp: raw(row,col) == row*16 + col.
    if version == 1:
        for (row, col) in ((0, 0), (5, 3), (15, 15)):
            off = 44 + row * row_stride + col * 2
            raw = struct.unpack_from("<h", data, off)[0]
            check(raw == row * 16 + col, f"{ctx}: elevation@({row},{col}) raw {raw} != {row * 16 + col}")


def _get_bit(buf, row_bytes, x, y):
    return (buf[y * row_bytes + (x >> 3)] >> (7 - (x & 7))) & 1


def validate_bit(path, sidecar_path, ctx):
    with open(path, "rb") as f:
        data = f.read()
    with open(sidecar_path, encoding="utf-8") as f:
        meta = json.load(f)

    check(len(data) >= 24, f"{ctx}: shorter than the 24-byte footer")
    if len(data) < 24:
        return
    footer = data[-24:]
    index_offset = struct.unpack_from("<Q", footer, 0)[0]
    tile_count = struct.unpack_from("<Q", footer, 8)[0]
    magic = footer[16:20]
    reserved = struct.unpack_from("<I", footer, 20)[0]

    check(magic == b"ATIL", f"{ctx}: bad footer magic {magic!r} (expected b'ATIL')")
    check(reserved == 0, f"{ctx}: footer reserved should be 0, got {reserved}")
    check(tile_count == 1, f"{ctx}: expected 1 tile, got {tile_count}")
    check(index_offset < len(data), f"{ctx}: index_offset {index_offset} beyond EOF {len(data)}")

    # Sidecar cross-checks.
    check(meta.get("output_format") == "1BIT_LOS", f"{ctx}: sidecar output_format != 1BIT_LOS")
    check(meta.get("tile_format") == "TILED", f"{ctx}: sidecar tile_format != TILED")
    ts = meta.get("tile_size")
    check(ts == 512, f"{ctx}: sidecar tile_size != 512")
    dims = meta.get("dimensions", {})
    width = dims.get("width")
    height = dims.get("height")
    check(width == 512 and height == 512, f"{ctx}: sidecar dimensions != 512x512")
    check(isinstance(meta.get("geotransform"), list) and len(meta["geotransform"]) == 6,
          f"{ctx}: sidecar geotransform must have 6 numbers")
    check(meta.get("projection") == "EPSG:4326", f"{ctx}: sidecar projection != EPSG:4326")

    # Parse the single 20-byte index entry.
    e = index_offset
    tx = struct.unpack_from("<I", data, e)[0]
    ty = struct.unpack_from("<I", data, e + 4)[0]
    offset = struct.unpack_from("<Q", data, e + 8)[0]
    size = struct.unpack_from("<I", data, e + 16)[0]
    check((tx, ty) == (0, 0), f"{ctx}: index entry (tx,ty) {(tx, ty)} != (0,0)")

    tile_bytes = (512 * 512) // 8  # 32768
    row_bytes = 512 // 8           # 64, u32-aligned (512/32*4)
    check(size == tile_bytes, f"{ctx}: tile size {size} != 1BIT_LOS tile bytes {tile_bytes}")
    check(row_bytes == ((512 + 31) // 32) * 4, f"{ctx}: 1-bit row stride {row_bytes} not u32-aligned")
    check(offset + size <= len(data), f"{ctx}: tile block [{offset}:{offset + size}] beyond EOF {len(data)}")
    check(offset + size <= index_offset, f"{ctx}: tile block overlaps the index region")

    # Verify the deterministic pattern (diagonal + top-left 8x8 block).
    tile = data[offset:offset + size]
    for i in (0, 100, 511):
        check(_get_bit(tile, row_bytes, i, i) == 1, f"{ctx}: diagonal bit ({i},{i}) not set")
    check(_get_bit(tile, row_bytes, 3, 3) == 1, f"{ctx}: top-left block bit (3,3) not set")
    check(_get_bit(tile, row_bytes, 20, 5) == 0, f"{ctx}: off-pattern bit (20,5) unexpectedly set")


# ── Driver ───────────────────────────────────────────────────────────────────

def main():
    print("== (a) JSON job fixtures ==")
    job_files = {
        "coverage_los.json": validate_job,
        "coverage_itm.json": validate_job,
        "batch_p2p.json": validate_job,
        "path.json": validate_job,
        "ingest_job.json": validate_ingest,
        "download_job.json": validate_download,
    }
    for name, fn in job_files.items():
        p = os.path.join(JOBS_DIR, name)
        if not check(os.path.exists(p), f"missing fixture {name}"):
            continue
        with open(p, encoding="utf-8") as f:
            try:
                obj = json.load(f)
            except json.JSONDecodeError as ex:
                check(False, f"{name}: invalid JSON: {ex}")
                continue
        fn(obj, name)
        print(f"  checked {name}")

    validate_batch_links(os.path.join(JOBS_DIR, "batch_links.csv"), "batch_links.csv")
    print("  checked batch_links.csv")

    print("== (b) binary format fixtures ==")
    validate_abt(os.path.join(FORMATS_DIR, "tiny_16x16.abt"), "tiny_16x16.abt")
    print("  checked tiny_16x16.abt")
    validate_bit(
        os.path.join(FORMATS_DIR, "tiny_los.bit"),
        os.path.join(FORMATS_DIR, "tiny_los.json"),
        "tiny_los.bit",
    )
    print("  checked tiny_los.bit + tiny_los.json")

    print()
    if ERRORS:
        print(f"FAILED: {len(ERRORS)} problem(s):", file=sys.stderr)
        for e in ERRORS:
            print(f"  - {e}", file=sys.stderr)
        sys.exit(1)
    print("OK: all fixtures conform to the contract.")


if __name__ == "__main__":
    main()
