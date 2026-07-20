# aether-toolkit

Open-source Rust toolkit for the data formats around the **Aether** GPU signal
propagation engine — a proprietary CUDA/compute engine developed by
[Waveshed](https://waveshed.io).

The engine itself (`aether_core`) is **not** part of this repository (see
[Engine contract](#engine-contract) below). What lives here is the permissively
licensed tooling that produces the engine's inputs and consumes its outputs, so
that the `.abt` and `.bit` formats can be read, written, and integrated by
anyone without a proprietary license.

## Crates

| Crate | Kind | What it does |
|-------|------|--------------|
| [`aether_converter`](crates/aether_converter) | CLI + lib | Converts GeoTIFF / terrain raster + vector building data into `.abt` tiles (the engine's packed input format). |
| [`aether_export`](crates/aether_export) | CLI (`aether_export`) | Exports the engine's `.bit` / `.tiles` coverage output back to GeoTIFF. Pure Rust, **no GDAL**. |
| [`aether_aggregate`](crates/aether_aggregate) | CLI (`aether_aggregate`) | Aggregates many rasters into max/count Cloud-Optimized GeoTIFF outputs. Pure Rust, **no GDAL**. |
| [`aether_converter_wasm`](crates/aether_converter_wasm) | `cdylib` | WebAssembly wrapper around `aether_converter` for in-browser tile conversion. |
| [`export_geotiff_wasm`](crates/export_geotiff_wasm) | `cdylib` | WebAssembly wrapper that turns raw GPU coverage output into GeoTIFF in memory, in the browser. |

## Building

Native binaries:

```sh
cargo build --release
```

The three CLI binaries land in `target/release/` as `aether_converter`,
`aether_export`, and `aether_aggregate`.

The two `*_wasm` crates are built with [`wasm-pack`](https://rustwasm.github.io/wasm-pack/)
against the `wasm32-unknown-unknown` target, e.g.:

```sh
wasm-pack build --release --target web crates/aether_converter_wasm
```

## Licensing

This toolkit is **dual-licensed under `MIT OR Apache-2.0`** — you may use it
under the terms of either license, at your option. See [LICENSE-MIT](LICENSE-MIT)
and [LICENSE-APACHE](LICENSE-APACHE).

**Why this license (recommended):**

- **It is the norm in the Rust ecosystem.** The overwhelming majority of Rust
  crates ship under `MIT OR Apache-2.0`; matching that convention keeps the
  toolkit friction-free to depend on and contribute to.
- **The MIT arm is GPLv2-compatible.** The companion Waveshed QGIS plugin is
  licensed **GPL-2.0-or-later**. Because the MIT option is compatible with
  GPLv2, that plugin (and any other GPL consumer) can incorporate this toolkit
  without a licensing conflict.
- **Permissive terms maximize adoption of the formats.** The whole point of
  open-sourcing this tooling is to spread the `.abt` and `.bit` formats.
  Permissive licensing removes the biggest barrier to third parties reading and
  writing those formats in their own (including commercial) software.

**Contributions** are accepted under the same dual license: unless you state
otherwise, any contribution you intentionally submit for inclusion shall be
`MIT OR Apache-2.0`, with no additional terms or conditions.

## Engine contract

The Aether engine core (`aether_core`) is **proprietary** and is intentionally
**not** included in this repository. The toolkit here is decoupled from it and
communicates only through the stable on-disk formats (`.abt` inputs, `.bit` /
`.tiles` outputs).

The format specification and the input/output contract these crates target are
documented in [docs/CONTRACT.md](docs/CONTRACT.md) *(to be added)*.
