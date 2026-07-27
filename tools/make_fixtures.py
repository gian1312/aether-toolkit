#!/usr/bin/env python3
"""Generate the synthetic binary golden fixtures in ``fixtures/formats/``.

Stdlib-only and fully deterministic: no timestamps, no randomness, no
environment lookups. Re-running this script must reproduce byte-identical
files, so the committed fixtures can be regenerated and diffed in review.

Every byte is laid out exactly as documented in ``docs/CONTRACT.md``:

  * ``tiny_16x16.abt`` — a 44-byte-header ``.abt`` terrain tile (see §6),
    version 1 (R16SINT), 16x16 px, 256-byte-aligned rows (matching the
    converter's ``AbtWriter``).
  * ``tiny_los.bit`` + ``tiny_los.json`` — a single-tile ATIL container (see
    §7), 1BIT_LOS, 512x512, with a matching sidecar (see §5a).
  * ``keys/v1_unsigned.blob`` / ``keys/v2_nodelock_unsigned.blob`` /
    ``keys/v2_unlocked_unsigned.blob`` — *decoded* license-key blobs (see §10)
    exercising the 84-vs-116 length discriminator and the all-zero-fingerprint
    rule. **Their 64-byte signature region is zero-filled — they are NOT valid
    keys** (a real key needs the vendor private seed, by design absent here).
    See ``fixtures/keys/README.md``.

These are *spec-derived* goldens, not engine-generated. See
``fixtures/README.md``.
"""

import hashlib
import json
import os
import struct

# ── Paths (resolved relative to this file, not the CWD) ──────────────────────
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FORMATS_DIR = os.path.join(REPO_ROOT, "fixtures", "formats")
KEYS_DIR = os.path.join(REPO_ROOT, "fixtures", "keys")

# ── Shared geo constants (kept identical between .abt and sidecar) ───────────
RESOLUTION_M = 10.0
DEG_PER_M = 1.0 / 111_111.0
SCALE = RESOLUTION_M * DEG_PER_M  # degrees per pixel
UL_LAT = 47.5
UL_LON = 8.0

# ── .abt constants (contract §6) ─────────────────────────────────────────────
ABT_MAGIC = b"AETH"
ABT_VERSION_R16SINT = 1
ABT_HEADER_SIZE = 44
ABT_WIDTH = 16


def _abt_row_stride(width_px: int) -> int:
    """256-byte-aligned row stride, exactly as AbtWriter::create computes it:
    ``stride = (width*2 + 255) & !255``."""
    bytes_per_row = width_px * 2
    return (bytes_per_row + 255) & ~255


def build_abt() -> bytes:
    """A 16x16, version-1 (R16SINT) .abt tile with a deterministic ramp."""
    stride = _abt_row_stride(ABT_WIDTH)  # 256 for width 16

    header = bytearray()
    header += ABT_MAGIC                                    # [0:4]  magic
    header += struct.pack("<H", ABT_VERSION_R16SINT)      # [4:6]  version u16
    header += struct.pack("<H", ABT_WIDTH)                # [6:8]  width u16
    header += struct.pack("<d", UL_LAT)                   # [8:16] ul_lat f64
    header += struct.pack("<d", UL_LON)                   # [16:24] ul_lon f64
    header += struct.pack("<d", SCALE)                    # [24:32] scale_y f64
    header += struct.pack("<d", SCALE)                    # [32:40] scale_x f64
    header += struct.pack("<h", 0)                        # [40:42] base_elev i16 (0 for R16SINT)
    header += struct.pack("<H", stride)                   # [42:44] row_stride u16
    assert len(header) == ABT_HEADER_SIZE, len(header)

    payload = bytearray()
    for row in range(ABT_WIDTH):
        row_bytes = bytearray()
        for col in range(ABT_WIDTH):
            raw = row * ABT_WIDTH + col  # deterministic 0..255 ramp; elev = raw*0.5 m
            row_bytes += struct.pack("<h", raw)
        # Pad the row out to the aligned stride with zeros.
        row_bytes += b"\x00" * (stride - len(row_bytes))
        assert len(row_bytes) == stride
        payload += row_bytes

    return bytes(header) + bytes(payload)


# ── ATIL .bit constants (contract §7) ────────────────────────────────────────
ATIL_MAGIC = b"ATIL"
OUTPUT_TILE_SIZE = 512


def _los_tile_bytes() -> bytes:
    """A 512x512 1-bit LOS tile (u32-aligned 64-byte rows) with a deterministic
    non-empty pattern: the main diagonal plus a solid 8x8 top-left block.
    Bits are MSB-first, matching aether_export's get_bit()."""
    ts = OUTPUT_TILE_SIZE
    row_bytes = ts // 8  # 64, and 512/32*4 == 64 (u32-aligned)
    buf = bytearray(row_bytes * ts)

    def set_bit(x: int, y: int) -> None:
        buf[y * row_bytes + (x >> 3)] |= 1 << (7 - (x & 7))

    for i in range(ts):          # main diagonal
        set_bit(i, i)
    for y in range(8):           # solid 8x8 block at the origin
        for x in range(8):
            set_bit(x, y)
    return bytes(buf)


def build_bit() -> bytes:
    """A single-tile ATIL container: tile block, one 20-byte index entry,
    24-byte footer."""
    tile = _los_tile_bytes()
    out = bytearray()

    # Tile data block at offset 0.
    tile_offset = 0
    out += tile

    # Binary index (one entry): tx u32, ty u32, offset u64, size u32.
    index_offset = len(out)
    out += struct.pack("<I", 0)                 # tx
    out += struct.pack("<I", 0)                 # ty
    out += struct.pack("<Q", tile_offset)       # offset
    out += struct.pack("<I", len(tile))         # size

    # Footer: index_offset u64, tile_count u64, magic "ATIL", reserved u32.
    out += struct.pack("<Q", index_offset)
    out += struct.pack("<Q", 1)                 # tile_count
    out += ATIL_MAGIC
    out += struct.pack("<I", 0)                 # reserved
    return bytes(out)


def build_sidecar() -> dict:
    """Matching sidecar for tiny_los.bit (contract §5a)."""
    return {
        "job_id": "tiny_los",
        "type": "LOS_BITMASK",
        "output_format": "1BIT_LOS",
        "tile_format": "TILED",
        "tile_size": OUTPUT_TILE_SIZE,
        "dimensions": {"width": OUTPUT_TILE_SIZE, "height": OUTPUT_TILE_SIZE},
        "geotransform": [UL_LON, SCALE, 0.0, UL_LAT, 0.0, -SCALE],
        "projection": "EPSG:4326",
    }


# ── License-key structural goldens (contract §10) ────────────────────────────
#
# These are the *decoded* blobs (i.e. post-Base58), because §10's offsets index
# the decoded blob. The 64-byte Ed25519 signature region is ZERO-FILLED: a valid
# signature needs the vendor private key, which is by design not in a public
# repo. So these goldens are structure-only — they exercise the length
# discriminator and payload offsets, never signature verification.
#
# Worked-example values (kept identical in tools/validate_fixtures.py and
# fixtures/keys/README.md). Epoch is 2026-01-01.
KEY_EPOCH = "2026-01-01"
KEY_EXP_DAYS = 730          # -> big-endian 0x02DA -> expires 2028-01-01
KEY_MAINT_DAYS = 365        # -> big-endian 0x016D -> maintenance through 2027-01-01
KEY_SEED = b"SEEDSEEDSEEDSEED"          # 16 bytes; obviously synthetic, not real key material
# A synthetic Linux-style /etc/machine-id, hashed exactly as-is (no trailing newline).
KEY_MACHINE_ID = b"0123456789abcdef0123456789abcdef"
KEY_FINGERPRINT = hashlib.sha256(KEY_MACHINE_ID).digest()   # 32 bytes -> node-locked example
KEY_ZERO_SIG = b"\x00" * 64             # placeholder signature (unsigned golden)
KEY_ZERO_FP = b"\x00" * 32              # all-zero fingerprint => not node-locked


def build_key_blob(version, fingerprint):
    """Build a decoded license-key blob (§10) with a zero-filled signature.

    ``version == 1`` -> 84-byte blob (no fingerprint).
    ``version == 2`` -> 116-byte blob; ``fingerprint`` is the 32-byte field
    (pass ``KEY_ZERO_FP`` for an unlocked v2 key)."""
    payload = struct.pack(">HH", KEY_EXP_DAYS, KEY_MAINT_DAYS) + KEY_SEED  # 20 bytes
    if version == 2:
        assert fingerprint is not None and len(fingerprint) == 32
        payload += fingerprint                                            # -> 52 bytes
    blob = payload + KEY_ZERO_SIG
    assert len(blob) == (84 if version == 1 else 116), len(blob)
    return blob


def main() -> None:
    os.makedirs(FORMATS_DIR, exist_ok=True)
    os.makedirs(KEYS_DIR, exist_ok=True)

    abt_path = os.path.join(FORMATS_DIR, "tiny_16x16.abt")
    bit_path = os.path.join(FORMATS_DIR, "tiny_los.bit")
    sidecar_path = os.path.join(FORMATS_DIR, "tiny_los.json")

    with open(abt_path, "wb") as f:
        f.write(build_abt())
    with open(bit_path, "wb") as f:
        f.write(build_bit())
    with open(sidecar_path, "w", encoding="utf-8") as f:
        json.dump(build_sidecar(), f, indent=2)
        f.write("\n")

    # License-key structural goldens (§10).
    key_v1_path = os.path.join(KEYS_DIR, "v1_unsigned.blob")
    key_v2_lock_path = os.path.join(KEYS_DIR, "v2_nodelock_unsigned.blob")
    key_v2_open_path = os.path.join(KEYS_DIR, "v2_unlocked_unsigned.blob")
    with open(key_v1_path, "wb") as f:
        f.write(build_key_blob(1, None))
    with open(key_v2_lock_path, "wb") as f:
        f.write(build_key_blob(2, KEY_FINGERPRINT))
    with open(key_v2_open_path, "wb") as f:
        f.write(build_key_blob(2, KEY_ZERO_FP))

    for p in (abt_path, bit_path, sidecar_path,
              key_v1_path, key_v2_lock_path, key_v2_open_path):
        print(f"wrote {os.path.relpath(p, REPO_ROOT)} ({os.path.getsize(p)} bytes)")


if __name__ == "__main__":
    main()
