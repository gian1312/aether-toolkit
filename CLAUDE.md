# aether-toolkit — working notes for Claude

Open-source Rust toolkit for the data formats around the proprietary **Aether**
GPU signal-propagation engine. See [README.md](README.md) for the public
overview and licensing; this file covers how to work in the repo.

---

## ⚠️ Work in THIS checkout, not in the AETHER submodule

This repository exists **twice** on disk, and they are separate directories that
drift independently:

| Path | Host path | Role |
|---|---|---|
| `aether-tools/` (mounted at `/workspace/aether-tools`) | `…\RustroverProjects\aether-toolkit` | **← the working checkout. Edit here.** |
| `AETHER/rust/toolkit/` | `…\RustroverProjects\AETHER\rust\toolkit` | Submodule checkout consumed by AETHER. **Read-only in practice.** |

Both sit at the same commit and `AETHER` pins that same commit, so `git log`
looks identical in both — the divergence is entirely in uncommitted work. At the
time of writing the working checkout had **51 modified files** and the submodule
copy only **6** (all six byte-identical to the working copy). The submodule copy
is therefore **stale, not different** — it is missing 45 files' worth of
in-progress work.

**Rules:**

- Make every edit in the working checkout (`aether-tools/`).
- Never edit `AETHER/rust/toolkit/` directly. Changes there are invisible to the
  working checkout, will not be committed by anything watching this repo, and
  are silently clobbered the next time the submodule is updated.
- If you need AETHER to pick up toolkit changes: commit and push here first,
  then in the AETHER repo run `git submodule update --remote rust/toolkit` and
  commit the new pin. Never the reverse.
- If a file looks unexpectedly out of date, check which of the two copies you
  are in before concluding anything: `git rev-parse --show-toplevel`.

The two copies being byte-identical on shared files is a coincidence of manual
syncing, not a guarantee. Do not rely on it.

---

## Layout

```
crates/          five workspace members (see table below)
docs/CONTRACT.md the canonical, versioned engine contract  ← read before changing anything observable
schemas/         JSON Schemas for every job/manifest format
fixtures/        worked examples + byte-exact binary goldens
tools/           make_fixtures.py, validate_fixtures.py
```

| Crate | Kind | Purpose |
|---|---|---|
| `aether_converter` | CLI + lib | GeoTIFF/terrain + vector buildings → `.abt` tiles (engine input) |
| `aether_export` | CLI | engine `.bit`/`.tiles` coverage → GeoTIFF. Pure Rust, **no GDAL** |
| `aether_aggregate` | CLI | many rasters → max/count Cloud-Optimized GeoTIFF. **No GDAL** |
| `aether_converter_wasm` | `cdylib` | WASM wrapper around `aether_converter` |
| `export_geotiff_wasm` | `cdylib` | WASM wrapper: raw GPU coverage → GeoTIFF in memory |

The engine core (`aether_core`) is **proprietary and not in this repo**. Coupling
is only ever through CLI invocations, JSON files, env vars, stdout/stderr text,
and binary formats — never a shared library or in-process call. Do not add a
dependency that assumes otherwise.

## Build & test

```sh
cargo build --release          # binaries → target/release/{aether_converter,aether_export,aether_aggregate}
cargo test --workspace         # 8 test files under crates/*/tests/ + in-module tests
python3 tools/validate_fixtures.py   # golden fixtures must stay valid
```

WASM crates build via `wasm-pack`, not `cargo build`:

```sh
wasm-pack build --release --target web crates/aether_converter_wasm
```

There is no CI in this repo (no `.github/`), so these run locally or not at all.
Run `cargo test --workspace` **and** `validate_fixtures.py` before calling any
change done — the fixture validator catches contract drift that `cargo test`
does not.

## Ingest jobs (contract v2.0)

Terrain inputs are a prioritised `sources` array (first valid sample wins per
pixel); the converter resolves each source's CRS itself and reprojects while
sampling — callers never warp:

```json
{
  "output_path": "./out/tile_N47.50E8.00_10m.abt",
  "format": "r16sint",
  "ul_lat": 47.5, "ul_lon": 8.0,
  "resolution_m": 10.0, "size_px": 2048,
  "sources": [
    {"path": "overlay.tif", "crs": "EPSG:2056", "nodata": -9999.0},
    {"path": "base.tif"}
  ],
  "void_fill_m": 0.0
}
```

- `crs` is `"EPSG:nnnn"` or a `+proj=…` string; absent → read from the file's
  GeoKeys, and an absent/user-defined key is a hard error naming the file.
  `nodata` overrides the file's `GDAL_NODATA` tag. Files without a
  geotransform are hard errors — georeferencing is never parsed from names.
- A source `path` may also be an `.abt` tile (detected by the `AETH` magic,
  not extension): self-describing (header geometry, raw i16 half-metres,
  R16SINT only), sampled like a geographic GeoTIFF; `crs`/`nodata` on it is
  a hard error.
- `void_fill_m` (optional) fills uncovered pixels with that elevation instead
  of the `-9999` void sentinel.
- `base_tif`/`swiss_tifs` are deprecated aliases (still accepted, normalized
  internally to `sources`; combining them with `sources` is an error).
- `aether_converter plan --south … --north … --west … --east … --resolutions 30,90`
  prints the `.abt` tile grid as JSON (`aether-plan/1`); its geometry mirrors
  the Waveshed plugin's Python enumeration exactly and consumers cross-check
  against it. See `docs/CONTRACT.md` §1.2/§9a and `schemas/`.

## The contract is the hard constraint

`docs/CONTRACT.md` is consumed by two **private** repos that public contributors
cannot see (the MPT_SIGMA KADAS plugin and the Waveshed QGIS plugin). Its policy
is strict and is the single most important rule in this repo:

- **All changes must be additive.** Consumers must tolerate unknown JSON fields.
- Any breaking change needs a major engine version bump **plus** a synchronized
  update to `docs/CONTRACT.md`, the schemas in `schemas/`, and the goldens in
  `fixtures/`.
- Changing a byte layout, CLI flag, env var, JSON field, or stdout format is a
  contract change even if `cargo test` still passes. Read the contract first.

If a requested change would break the contract, say so before implementing it —
the blast radius includes repos not visible from here.

## License

Dual `MIT OR Apache-2.0`; contributions accepted under the same terms. The MIT
arm is deliberate: it keeps the GPL-2.0-or-later QGIS plugin compatible. Don't
introduce dependencies that are incompatible with either arm.
