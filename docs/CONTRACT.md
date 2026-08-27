# Aether engine contract

**Status: Contract version 2.0.** This document is the canonical, versioned
description of the interface between the proprietary **`aether_core`** engine
(which lives in the private `AETHER` repository and is *not* shipped here) and
its consumers. It targets the current `aether_core` **0.x** engine line and the
crates in this workspace (`aether_converter`, `aether_export`,
`aether_aggregate`).

> **Changelog — contract v2.0 (breaking, `aether_converter` only).** This
> version bundles every pending `aether_converter` change — the four
> corrections that had been recorded here unversioned since v1.1, plus the
> generalized ingest surface — into **one** major bump. The maintainer
> approved bundling these as a single major version rather than shipping them
> piecemeal. `aether_core`, `aether_export`, `aether_aggregate`, the license
> format, and the `download` job format are unchanged from v1.1.
>
> Carried over from the previously-unversioned corrections (details inline
> where they apply):
> 1. **`.abt` bytes** change for DEM inputs that used Float32-NaN or
>    Int32-minimum no-data — those pixels were written as 0 m (sea level) and
>    are now the `-9999` void sentinel (§6).
> 2. **`download` exit code** — a run that lost more than half its terrain
>    tiles now fails instead of exiting 0 (§1.2).
> 3. **`ingest` exit code** — a mixed-zoom or unreadable `buildings_pbf_dir`
>    now fails instead of warning and continuing (§9a).
> 4. **`.abt` payload bytes** change for nearly every input: terrain is now
>    **area-averaged** over each output pixel's footprint instead of taking one
>    source sample per pixel (§6). This is the largest of the four — it moves
>    pixel values for any source finer than the target grid, which is the normal
>    case — and it deliberately voids any "pixel-identical" expectation a
>    consumer held.
>
> New in v2.0 proper:
> 5. **`sources[]` ingest (§9a).** An ingest job's terrain inputs are now a
>    prioritised `sources` array of `{path, crs?, nodata?}`; first valid
>    sample wins per pixel. A source's CRS comes from its explicit `crs`
>    field, else from the GeoTIFF's GeoKeyDirectory; an absent or
>    user-defined key is a **hard error** naming the file. `GDAL_NODATA` is
>    honored (explicit `nodata` wins). The legacy `base_tif`/`swiss_tifs`
>    fields remain accepted as **deprecated aliases** (normalized internally
>    to `sources`) and are slated for removal in the next major version;
>    supplying both surfaces in one job is a hard error.
> 6. **Filename-derived georeferencing removed (§9a).** A source GeoTIFF
>    without a geotransform used to fall back to coordinates parsed from its
>    file name (or to garbage); it is now a **hard error**.
> 7. **EPSG:2056 sampling via a real projection (§9a).** The former
>    approximate CH1903+ polynomial is replaced by proj4rs; coordinates move
>    by **≤ 1 m**, so `.abt` pixels over such sources may shift by one source
>    sample. Any projected CRS with an EPSG definition (or a supplied proj
>    string) now works, not just EPSG:2056.
> 8. **`void_fill_m` (§9a, additive).** Optional; a pixel no source covers is
>    written as `round(void_fill_m * 2)` half-metres instead of the `-9999`
>    sentinel.
> 9. **`plan` subcommand (§1.2, additive).** Prints the `.abt` tile grid for
>    a bbox + resolution set as a single JSON document (schema
>    `aether-plan/1`) without converting anything.
> 10. **`.abt` files as ingest sources (§9a, additive).** A `sources[].path`
>    may be an `.abt` tile (detected by the `AETH` magic, never by
>    extension); it is self-describing and sampled like a geographic GeoTIFF.
>    `crs`/`nodata` on such a source, a truncated/corrupt header, or a BC6H
>    (version 2) tile are hard errors.
> 11. **`buildings_file` heights and exit code (§9a).** A `buildings_file`
>    used to take the roof from the geometry Z **only**; a 2D FlatGeobuf —
>    the ordinary shape of an OSM extract — therefore drew nothing and the
>    run exited 0 with a tile identical to the building-less one. Geometry Z
>    is still the top rung, unchanged and byte-identical where it exists, but
>    a geometry without one now falls back to the same attribute ladder the
>    vector-tile path uses (**above-ground** metres; see §9a "Building height
>    ladder"). Consequences: a 2D source that previously changed nothing now
>    raises the surface; a `buildings_file` that cannot be **read** now fails
>    the run instead of warning and exiting 0; and every burn prints one
>    `[Buildings]` line, where the ingest path previously printed nothing at
>    all on success. Same release also fixes two reads that dropped or
>    mangled geometry regardless of dimension — see the note in §9a.
> 12. **Multi-band / colour GeoTIFF sources refused (§9a).** A source whose
>    TIFF colortype is anything but single-band greyscale (RGB(A), palette,
>    GrayA, CMYK, YCbCr — a rendered basemap export, a hillshade, a photo)
>    is now a **hard error naming the file**: reading a picture's colour
>    values as metres produced confident garbage terrain, and the extra
>    samples per pixel additionally sheared the sampling grid (the samplers
>    index `y·w+x`). Genuine single-band DEMs of any sample type — UInt8
>    included — remain ingestable. A decode returning anything but exactly
>    `w·h` samples is likewise refused.
> 13. **TIN / PolyhedralSurface buildings burn (§9a, additive).** A
>    `buildings_file` feature with `TIN`, `PolyhedralSurface` or `Triangle`
>    geometry — what GDAL's FlatGeobuf driver writes for swissBUILDINGS3D
>    3.0 — was silently skipped by the Polygon/MultiPolygon type filter and
>    the burn drew nothing; those types now burn like MultiPolygon, each
>    part at its own max Z.
> 14. **`ingest` sampling registration (§6, §9a).** The area-average window
>    is now anchored on each output cell's **centre** (the footprint §6
>    always promised) instead of its NW corner, and a `GTRasterTypeGeoKey`
>    of `RasterPixelIsPoint` shifts the source origin to the pixel corner on
>    load instead of being ignored. `.abt` payload bytes move for every
>    `ingest` input (up to one source sample); see the Correction in §6.
> 15. **`download` voids and XYZ tile size (§1.2, §6, §9b).** Two changes to
>    the download path, both changing `.abt` bytes:
>    * A tile that never arrived is now written as the `-9999` **void**
>      sentinel, not as **0 m**. The assembly grid used to be zero-filled and
>      a failed tile simply never copied in, so a 404'd tile became flat
>      sea-level terrain that no consumer could tell from surveyed ground.
>      **Bytes change for any run that lost a tile** — and for any output
>      pixel whose footprint fell outside the fetched tile grid, which was
>      likewise 0 m. A run that lost nothing is byte-for-byte unchanged.
>    * The XYZ tile edge is now read from the PNG instead of assumed to be
>      256 px. MapTiler's terrain-rgb serves **512 px** (`@2x`) tiles; those
>      were indexed with a 256 px row stride, which folded each tile in half
>      (west half onto the even output rows, east half onto the odd) for
>      terrain that looked plausible and was ~555 m out at p95. **Bytes change
>      for every 512 px source**; a 256 px source (AWS Terrarium) is
>      byte-for-byte unchanged. A **non-square** tile, one that is not 8 bits
>      per channel, and a source that **mixes** tile sizes within one run are
>      now hard errors naming the offending dimensions (§9b).

> **Version note (verified against source):** the task that commissioned this
> contract referred to "engine 0.4.x", but the engine's own
> `rust/aether_core/Cargo.toml` declares `version = "0.1.0"`, and
> `aether_core --version` therefore prints `aether_core 0.1.0`. The source is
> authoritative, so this contract describes the **0.x** line and pins nothing to
> a `0.4` tag. When the engine adopts semantic versioning in earnest, bump the
> *Contract version* above in lockstep.

The engine and its consumers communicate **only** through the surfaces
enumerated here: command-line invocations, JSON files, environment variables,
stdout/stderr text, and binary file formats. There is no shared library, no RPC,
and no in-process coupling. Everything a public contributor can change in this
repository that could break a private consumer is, by definition, one of these
surfaces.

## Who consumes what

Three consumers depend on this contract. Two of them are **private and not
visible to public contributors**, so their needs are called out explicitly
throughout this document:

| Consumer | Repo visibility | Role |
|----------|-----------------|------|
| **Waveshed QGIS plugin** | public-facing, GPL-2.0-or-later | Runs the CLIs; parses `RUST_LOG` log lines and disk-space strings; consumes the release manifest. |
| **MPT_SIGMA KADAS plugin** | **private** | Runs the CLIs; parses the `[P:]/[S:]/[E:]/[D:]` stdout markers; reads `.tif`, `.bit`+sidecar, P2P `.csv`, and `.vix`. |
| **waveshed.io web / WASM pipeline** | mixed | Drives the `*_wasm` crates in-browser; uses `compute_backend: "GPU_ONLY"`. |

Because MPT_SIGMA and the QGIS plugin cannot be inspected from this repository,
**their load-bearing expectations are frozen here.** Several behaviors below are
relied on by MPT_SIGMA yet appear in no other document; they are marked
**`[MPT-critical]`**.

---

## Compatibility policy (read this before changing anything)

1. **All changes must be additive.** New JSON fields, new stdout lines, new
   optional CLI flags, new sidecar keys, and new file-format trailer fields are
   allowed. Renaming or removing any existing field, flag, marker, column, magic
   value, or byte offset is a **breaking change**.

2. **Consumers must tolerate unknown JSON fields.** Every JSON schema in
   `schemas/` sets `"additionalProperties": true`. A producer that adds a field
   must not break a consumer, and a consumer must ignore fields it does not
   recognize. Reciprocally, do not rely on a field's *absence*.

3. **Any breaking change requires a major engine version bump** *and* a
   synchronized update to (a) this document, (b) the JSON schemas in `schemas/`,
   and (c) the fixtures in `fixtures/`. A breaking change that lands in only one
   of those three places is an incomplete change.

4. **The private consumers count even when you cannot see them.** A change that
   compiles, passes this repo's tests, and looks locally reasonable can still
   break MPT_SIGMA or the QGIS plugin. When in doubt, treat every item in this
   document as a hard interface and open a discussion rather than "cleaning it
   up". The frozen behaviors marked `[MPT-critical]` are the ones most likely to
   look removable but are not.

5. **Where an existing doc, comment, or CLI help string disagrees with the
   actual emitted bytes/strings, the emitted behavior wins.** Several such
   disagreements are recorded inline below (search for "Correction").

---

## 1. Binaries & CLIs

Four binaries participate in the contract. `aether_core` is proprietary and
ships from the private repo; the other three are built from this workspace
(`target/release/{aether_converter,aether_export,aether_aggregate}`).

### 1.1 `aether_core`

```
aether_core --config <job.json>   # run a job (the normal invocation)
aether_core --fingerprint         # print this machine's node-lock fingerprint, then exit 0
```

* `--config <path>` (short `-c`) is the invocation form for **running a job**,
  and is required to run one (`clap` `Parser`, field `config: PathBuf`,
  `#[arg(short, long)]`). There are no positional arguments and no
  sub-commands.
* `--fingerprint` (long flag; **output-only**) prints this machine's node-lock
  fingerprint — the **lowercase hex** (64 chars) of `SHA-256(machine_id_string)`
  (§10) — to **stdout** and exits `0`. It **needs no license** and
  short-circuits *before* any license enforcement, so it works in both dev and
  `proprietary` builds. The binary derives the value from the host OS only and
  **never accepts a fingerprint as input** — no flag, env var, or job field
  supplies one. Consumers call this to read the fingerprint they must send the
  vendor to be issued a node-locked (v2) key.
* **Exit code `0` = success.** Any failure prints `[E:<message>]` to stdout and
  exits `1` (`main.rs`: config-read failure, JSON-parse failure, and the
  top-level `Err` handler all `std::process::exit(1)`).
* **stdout carries two protocols simultaneously.** Both are contract:

  **(a) Marker protocol — parsed by MPT_SIGMA `[MPT-critical]`.** Plain
  `println!` lines, always emitted (no env var required):

  | Marker | Meaning | Example emit site |
  |--------|---------|-------------------|
  | `[P:<int>]` | Progress percent (0–100) | `println!("[P:{}]", wedge_percent)` |
  | `[S:<msg>]` | Human-readable status | `println!("[S:Executing Wedge {}/{}]", …)` |
  | `[E:<msg>]` | Error (precedes exit 1) | `println!("[E:{:#}]", e)` |
  | `[D:<msg>]` | Debug detail | emitted by sub-engines |

  MPT_SIGMA extracts these with the regexes `\[P:(.*?)\]`, `\[S:(.*?)\]`,
  `\[E:(.*?)\]`, `\[D:(.*?)\]` (`workers/splat_worker.py`). Do not change the
  bracket/colon shape of these markers.

  **(b) Log-line protocol — parsed by the QGIS plugin.** Diagnostic lines
  requiring `RUST_LOG=info`. `aether_core` calls `env_logger::init()`, so
  `log::info!` output (from `solver.rs` etc.) is written to **stderr** and only
  appears when `RUST_LOG` selects the `info` level.

  **The wedge-progress string `Wedge <n>/<total>`** is load-bearing for the
  QGIS plugin, which scans for it.

  > **Correction (verified in `engines/coverage.rs`):** the commissioning task
  > described `Wedge <n>/<total>` as a `RUST_LOG=info` log line. In the current
  > source it is emitted as part of the **`[S:]` marker on stdout** —
  > `println!("[S:Executing Wedge {}/{}]", wedge_idx + 1, wedges_needed)` (and
  > `[S:Skipping Wedge {}/{}]`) — and is therefore present **regardless of
  > `RUST_LOG`**. No `log::info!` call emits the substring "Wedge". A consumer
  > that greps merged stdout+stderr for `Wedge (\d+)/(\d+)` will still match,
  > because the substring lives inside the `[S:…]` line. Treat *both* the marker
  > shape and the `Wedge <n>/<total>` substring as frozen; do not gate wedge
  > progress on `RUST_LOG`, and do not remove it from the `[S:]` line.

* **stderr may be merged into stdout by callers.** Both MPT_SIGMA and the QGIS
  plugin run the process with stderr redirected onto stdout, so a marker and a
  log line can interleave on the same stream. Never assume a marker is alone on
  its stream.

### 1.2 `aether_converter`

```
aether_converter ingest   --job-file <json>      # -j
aether_converter download  --job-file <json>     # -j
aether_converter plan --south S --north N --west W --east E --resolutions 30,90
```

* Sub-commands: `ingest`, `download`, `plan` (and a deprecated `convert` that
  just prints a deprecation line). `ingest`/`download` take `--job-file <path>`
  (short `-j`).
* stdout/stderr here use a **different, `[Rust]`/`[Download]`/`[Stats]`-prefixed
  style** — *not* the `[P:]/[S:]/[E:]/[D:]` markers of §1.1.
* **Ingest progress** (stdout): `[Rust] Progress: <n>/<total>` (emitted every 10
  tiles and once at completion) — `println!("[Rust] Progress: {}/{}", curr, total)`.
* **Ingest failure modes (v2.0, fail-loudly).** A job file that parses as
  neither a single job nor a job array now **fails with the parse error**
  (it used to exit 0 having done nothing). A listed terrain source that cannot
  be loaded fails the tile (and thus the run) instead of warning and writing a
  terrain-less tile. CRS/georeferencing problems are hard errors naming the
  file and the fix (§9a).
* **An ingest that covered nothing exits non-zero.** A run in which **no
  source supplied a sample for a single pixel of a single tile** fails
  (`no source covered any pixel of …` for one job, `… came from a source` for
  a batch) instead of exiting 0 with a tile that is nothing but void. Coverage
  is counted **before** `void_fill_m` is applied, so filling the holes with
  0 m does not disguise it, and it is judged **over the whole run**, never per
  tile: the edge tiles of any area legitimately fall outside the sources, and
  those are still written. The usual cause is a source that does not overlap
  the requested area at all, or the wrong CRS on one.

  > **Consumer note.** A caller that today treats exit 0 as "terrain exists"
  > keeps working. A caller that deliberately converts an area with no data —
  > to pre-create empty tiles — must now pass `void_fill_m` **and** at least
  > one overlapping source, or handle the non-zero exit.
* **`plan` (new in v2.0, additive).** Enumerates, without downloading or
  converting anything, exactly the `.abt` tiles an area/resolution request
  produces: one JSON document on stdout with `schema: "aether-plan/1"`,
  `tile_count`, `total_bytes`, and per-tile `resolution_m`, `filename`
  (`tile_N{lat:.2}E{lon:.2}_{res}m.abt`), `ul_lat`, `ul_lon`, `size_px`,
  `exact_res_m`, `est_bytes`. Schema: `schemas/plan_output.schema.json`.
  The geometry reproduces the Waveshed plugin's Python tile enumeration
  **exactly** (extent ladder, u16-stride guard, outward snap on the finest
  sub-tile grid, 6-decimal stepping); the plugin cross-checks its own
  enumeration against this output and aborts on mismatch. Validation is
  strict: non-finite coordinates, `south >= north`, `west >= east`, a bbox
  reaching outside `[-180, 180] x [-90, 90]` (`longitude out of range` /
  `latitude out of range` — the grid wraps nowhere, so an antimeridian
  crossing is refused rather than planned), an empty or non-positive
  resolution list, and requests over 2,000,000 tiles (`bbox too large`) all
  fail with a non-zero exit.
* **Download progress** (stderr):
  `[Download] <pct>% (<done>/<total>) — <MB/s>, <errors> errors, <in-flight> in-flight`.

  > **Parsed interface (frozen).** The `download` subcommand's stdout/stderr
  > progress lines — the `[Download] …` progress format above and the
  > `[Stats] …` completion block below — are **read programmatically by the
  > Waveshed QGIS plugin** to drive its progress bar, not just shown to
  > humans. Treat their shape exactly like the `[Rust] Progress: X/N` ingest
  > line: any change to the prefix, ordering, or number formats is a
  > breaking change under this contract.
* **Download stats line** (stderr, at completion):

  ```rust
  eprintln!("[Stats] Tiles: {}/{} OK ({:.1}% success)", ok, total, …);
  ```

  i.e. `[Stats] Tiles: <ok>/<total> OK (<pct>% success)`.
* **Disk-space failure — `[MPT-critical]` / QGIS-critical.** When the estimated
  `.abt` output exceeds free space, `download` **fails** (`anyhow::bail!`, non-zero
  exit). The exact and only emitted string (`crates/aether_converter/src/download.rs`,
  in the native `run_download` disk-space guard) is:

  ```rust
  anyhow::bail!(
      "Insufficient disk space: need ~{} MB for {} .abt output files, \
       only {} MB available in {:?}. Free disk space or reduce area/resolution.",
      total_abt_mb, job.tiles.len(), avail_mb, job.output_dir
  );
  ```

  A near-full (>80 %) warning is also emitted (does not fail):

  ```rust
  eprintln!("[Download] WARNING: disk space is tight — need {} MB, {} MB available",
      total_abt_mb, avail_mb);
  ```

  > **Correction:** the commissioning task said the QGIS plugin matches
  > `"insufficient disk"` **or** `"not enough space"`. The converter emits only
  > the string above; **no `"not enough space"` string exists anywhere in
  > `crates/aether_converter`.** A case-insensitive substring match on
  > `insufficient disk` matches; a match on `not enough space` does **not**. If
  > you rely on this failure text, match `insufficient disk` (case-insensitive).
  > If you must keep `not enough space` support in a consumer, do so *in
  > addition to*, not instead of, `insufficient disk`. Do not change the
  > `Insufficient disk space` prefix without a major bump.

* **Tile-fetch failure — new failure mode.** When **more than half** of the
  requested XYZ terrain tiles could not be fetched, `download` **fails**
  (`anyhow::bail!`, non-zero exit) after emitting the usual `[Stats]` block.
  The emitted string (`crates/aether_converter/src/download.rs`, guarded by
  `fetch_failure_is_fatal`) is:

  ```rust
  anyhow::bail!(
      "Tile download failed: {} of {} terrain tiles ({}%) could not be fetched; \
       the output would be mostly void, not terrain. Errors: {}. Check that \
       the tile source serves zoom {} over this area and that the network is \
       reachable.",
      failed, attempted, pct, stats.error_breakdown(), job.zoom
  );
  ```

  `Errors:` names the **failure class**. It renders the same per-class tally as
  the `[Stats] ERRORS` line — one builder, `DownloadStats::error_breakdown()` —
  so the two can never disagree: `timeout`, `connect`,
  `HTTP_429_rate_limited`, `HTTP_4xx`, `HTTP_5xx`, `decode`, `other`, in that
  order, only the non-zero ones, comma-separated. A real message reads (one
  line, wrapped here):

  ```
  Tile download failed: 2 of 2 terrain tiles (100%) could not be fetched; the
  output would be mostly void, not terrain. Errors: decode=2. Check that the
  tile source serves zoom 12 over this area and that the network is reachable.
  ```

  Without it, "could not be fetched" reads as a network fault for every cause.
  It is not one: a source that serves **WebP** to this PNG decoder (MapTiler's
  Terrain-RGB v2 does) fails every tile with `decode=N`, and the repair is a
  different tile source, not a different network.

  > **Behaviour change for consumers.** The `Errors: {}.` sentence is new; it
  > is inserted before the `Check that the tile source…` sentence. The clause
  > `{} of {} terrain tiles ({}%) could not be fetched` is frozen alongside the
  > prefix; the words between it and `Errors:` are not — they described the
  > output as `mostly flat 0 m` until missing tiles became void, and now read
  > `mostly void`. A consumer matching the `Tile download failed:` prefix (as
  > documented below), or that clause, is unaffected; one matching the whole
  > former string exactly must drop the tail from its pattern.

  > **Behavior change for consumers.** Previously *every* download exited 0,
  > including one where all tiles 404'd — the classic cause being a requested
  > zoom the tile source does not serve. Consumers that treated exit 0 as
  > "tiles are usable" were wrong then and are right now; consumers that
  > treated any non-zero exit as fatal need no change. Sparse 404s at the edge
  > of a provider's coverage remain non-fatal — the threshold is deliberately a
  > *majority*, and a minority loss is still only reported through
  > `[Stats] ERRORS`. Match `Tile download failed:` if you need to distinguish
  > this from the disk-space failure.

  > **Behaviour change for consumers — a lost tile is now a hole, not sea
  > level.** A tile that never arrives is never copied into the assembly grid.
  > That grid used to be zero-filled, so the pixels it covered were written as
  > **0 m** — a valid sea-level elevation, indistinguishable downstream from
  > surveyed ground, which is how a partly-404'd run produced confident flat
  > ocean. Those pixels are now the **`-9999` void sentinel** of §6, the same
  > value `ingest` writes for a pixel no source covered. The same applies to
  > any output pixel whose footprint falls outside the fetched tile grid at
  > all. **`.abt` bytes therefore change for every run that lost a tile**; a
  > run in which every tile arrived is byte-for-byte unchanged. A consumer
  > that reads `-9999` as an elevation sees -4999.5 m; one that already
  > handles the `ingest` sentinel needs no change, and one that filled or
  > flagged 0 m regions as suspect can now distinguish "no data" from "sea".

* **A failed `download` deletes its own `.abt` outputs.** Every output file is
  created with a valid 44-byte `AETH` header *before* the first tile is fetched
  and is padded to full length once assembly ends, so a run that fails leaves a
  complete, well-formed `.abt` behind — void everywhere the tiles never arrived
  (0 m, before the change above). Consumers pool tiles by filename, so that file
  is a cache hit for every later run: one refusal poisons the pool. A run that
  ends in **any** error therefore removes the `.abt` files it created (a drop
  guard, so this covers the fatal tile-fetch branch, an assembly error, a
  buildings post-pass error, and cancellation alike) and logs one line to
  stderr before the error:

  ```
  [Download] Removed {n} incomplete .abt file(s) — a failed download leaves nothing reusable behind; the next run must fetch again.
  ```

  A **successful** run keeps its files, unchanged. The output directory itself
  is never removed, only the `.abt` files this run created — and note that a
  failed re-run also removes tiles of the same name that existed beforehand,
  because they were already truncated and rewritten as headers at step 4 and
  were gone regardless.

  > **Behaviour change for consumers.** A consumer that harvested whatever the
  > `download` job left on disk after a non-zero exit now finds nothing to
  > harvest. That output was mostly empty terrain — flat 0 m before the void
  > sentinel — and never usable data; a consumer that re-runs on failure needs
  > no change, and one that reports a missing file as "download failed" is now
  > correct where it was previously fooled by a full-size sea-level tile.
  > Nothing changes for a run that exits 0.

### 1.3 `aether_export`

```
aether_export -i <input .bit|.tiles> -j <sidecar.json> -o <out.tif>
```

| Short | Long | Meaning | Default |
|-------|------|---------|---------|
| `-i` | `--bit-path` | Input `.bit` (flat) or `.tiles` (tiled) | required |
| `-j` | `--json-path` | JSON sidecar (see §5a) | required |
| `-o` | `--output` | Output GeoTIFF | required |
| `-l` | `--level` | Deflate level 1–9 (`0` = none) | `1` |
| `-t` | `--tile-size` | Output tile px (multiple of 16) | `512` |
| | `--no-overviews` | Skip overview pyramid | `false` |

* Output is **always a (Big)GeoTIFF**. There is no CSV output path.

  > **Correction:** the task described `-o <out.tif|out.csv>`. `aether_export`
  > writes only GeoTIFF (`crates/aether_export/src/main.rs`, "Convert AETHER
  > .bit/.tiles → Cloud-Optimized GeoTIFF"). The P2P `.csv` is produced by
  > `aether_core` (§5c), not by `aether_export`. Also note the *long* flag names
  > are `--bit-path`/`--json-path`/`--output` (kebab-case), not `--input`/`--json`;
  > only the short forms are `-i/-j/-o` (verified empirically via `--help`).

### 1.4 `aether_aggregate`

```
aether_aggregate --file-list <txt> --output <basename> --level <n> \
                 --tile-size 2048 [--visibility <path.vix>]
```

| Short | Long | Meaning | Default |
|-------|------|---------|---------|
| `-f` | `--file-list` | Text file, one input path per line (`.bit` or `.tif`) | required |
| `-o` | `--output` | Output **basename** | required |
| `-l` | `--level` | Deflate level (`0`=none) | `1` |
| `-t` | `--tile-size` | Tile px (multiple of 16) | `2048` |
| | `--visibility` | Path for the `.vix` visibility index (§8) | none |

* Produces `<basename>_max.tif` (8-bit COG) and `<basename>_count.tif`
  (16-bit COG). When `--visibility` is given, it *also* writes a `.vix` file.
* Emits the `[P:]`/`[S:]`/`[E:]` **marker protocol** on stdout (like
  `aether_core`): `[P:0]`, `[S:Starting Aggregation…]`, … `[P:100]`,
  `[S:Aggregation complete]`, and `[E:<msg>]` + exit 1 on error. `[Aggregate]`
  progress/summary lines go to stderr.

  > **Correction:** the `--visibility` help text calls the output a "binary
  > visibility bitmask file (.aeth)". That is stale: the bytes actually written
  > are the **`.vix`** dense-tiled-CSR format (trailing `VIX!` magic — §8), *not*
  > an `.aeth` bitmask. Use `.vix`; MPT_SIGMA's `VixReader` consumes it. **The
  > `.vix` writer emits per-tile CSR arrays of size `tile_size²`, and
  > MPT_SIGMA's reader hardcodes `TILE_SIZE = 2048`; therefore `--tile-size`
  > MUST stay `2048` when `--visibility` is used**, or the produced `.vix` is
  > unreadable by the consumer.

### 1.5 Version handshake (`--version`)

Verified by reading each `clap` setup and (for the toolkit binaries)
empirically:

| Binary | `clap` `version` attribute? | `--version` behavior |
|--------|-----------------------------|----------------------|
| `aether_core` | **yes** — `#[command(author, version, about)]` | Prints `aether_core <CARGO_PKG_VERSION>` (currently `0.1.0`) and exits `0`. `-V` also works. |
| `aether_converter` | no | `--version` is an *unknown argument* → clap error, exit `2`. |
| `aether_export` | no | `--version`/`-V` → `error: unexpected argument '--version' found`, exit `2` (verified). |
| `aether_aggregate` | no | same as above, exit `2` (verified). |

Current versions: `aether_converter 0.2.4`, `aether_export 1.0.0`,
`aether_aggregate 1.0.0` (from each crate's `Cargo.toml`; the toolkit binaries
do **not** expose these on the CLI).

> **RECOMMENDED future behavior:** every binary should accept `--version` and
> print a real semantic version (`<name> <semver>`) to stdout with exit `0`.
> This lets consumers gate on engine capability without parsing help text.
> Adding `#[command(version)]` to the three toolkit CLIs is a safe additive
> change; do it before consumers begin depending on version output.

---

## 2. Environment variables

| Variable | Read by | Purpose |
|----------|---------|---------|
| `AETHER_LICENSE` | `aether_core` (proprietary build) | Base58 license/API key — **v1 or v2** (§10). Proprietary builds **refuse to run without a valid, unexpired key**. If unset, the engine falls back to reading a `license.key` file (see below). |
| `RUST_LOG` | `aether_core`, toolkit CLIs | `env_logger` level filter. **Must be `info` (or lower)** for the log-line progress diagnostics of §1.1(b) to appear. Markers in §1.1(a) do not depend on it. |
| `AETHER_BIN_DIR` | consumers only | Convention for locating the engine binaries. **Not read by any engine/toolkit code** — it is purely a consumer-side discovery hint. Do not add engine logic that depends on it. |
| `PYTHONUNBUFFERED` | consumers only | Set by Python consumers so a child process's stdout markers/log lines arrive line-by-line rather than block-buffered. Not read by Rust code. |

**License discovery (proprietary build, `crypto.rs`):**

```rust
let license_b58 = std::env::var("AETHER_LICENSE")
    .or_else(|_| fs::read_to_string("license.key"))   // relative to CWD
    .map_err(|_| anyhow::anyhow!("Missing license. Set AETHER_LICENSE env var or create license.key file."))?;
```

* First `AETHER_LICENSE`; else a `license.key` file. Note the file is read via
  the **relative path `license.key`, i.e. from the process's current working
  directory**. The keygen tool instructs operators to place it "in the same
  directory as the `aether_core` executable", which only coincides with CWD when
  the process is launched from the binary's directory — the usual case for both
  plugins. MPT_SIGMA populates `AETHER_LICENSE` from a `license.key` file it
  locates itself (`workers/splat_worker.py`), sidestepping the CWD nuance.
* License enforcement is compiled in only under the `proprietary` Cargo feature.
  Non-proprietary/dev builds do not require a license.
* The supplied key may be a **v1 (84-byte)** or **v2 (116-byte, node-locked)**
  blob (§10); both travel through `AETHER_LICENSE`/`license.key` identically —
  the engine discriminates by decoded length. There is **no environment variable
  (or job field) for the node-lock fingerprint**: the engine computes it from
  the host OS and only ever *prints* it via `aether_core --fingerprint` (§1.1).
  It is never taken as input.

---

## 3. `job.json` schema (input to `aether_core`)

Authority: `rust/aether_core/src/config.rs` (`JobConfig` and friends). Every
field, its Rust type, its default (via `#[serde(default …)]`), and which
consumers send it are below. **Unlisted fields are ignored** (serde drops
unknown keys; consumers must tolerate them per policy §2).

Top-level object:

| Key | Type | Required? | Notes |
|-----|------|-----------|-------|
| `tx` | object | **required** | Transmitter — §3.1 |
| `rx` | object | optional (`RxConfig::default`) | Receiver — §3.2 |
| `analysis` | object | **required** | §3.3 |
| `output` | object | **required** | §3.4 |
| `processing` | object | optional (default) | §3.5 |
| `propagation` | object | optional (default) | §3.6 |

### 3.1 `tx` (`TxConfig`)

| Field | Type | Default | Sent by | Notes |
|-------|------|---------|---------|-------|
| `lat` | f64 | `0.0` | site/coverage | Ignored for P2P/BATCH_P2P (positions come from the batch file) and for PATH (overridden per waypoint). |
| `lon` | f64 | `0.0` | site/coverage | WGS84, **east-positive**. |
| `height_m` | f32 | `0.0` | all | Antenna height; interpreted per `mode`. |
| `mode` | string | `"AGL"` | all | `"AGL"` or `"AMSL"`. |
| `freq_mhz` | f32 | **required (no default)** | all | Omitting it is a JSON-parse error → `[E:…]`, exit 1. |
| `erp_watts` | f32? | `null` | site, path, p2p | Effective radiated power. Converts to dBm as `10·log10(W)+30+2.14`. |
| `az_pattern_file` | string? | `null` | site (when a pattern exists) | Path to azimuth antenna pattern. |
| `el_pattern_file` | string? | `null` | site | Path to elevation pattern. |
| `az_rotation` | f32? | `null` | path (per-waypoint heading) | Azimuth rotation of the pattern, degrees. |
| `az_pattern_data` | `[[f32;2]]`? | `null` | web/WASM | Inline `[angle_deg, gain]` pairs (linear gain 0–1); used instead of `az_pattern_file`. |
| `el_pattern_data` | `[[f32;2]]`? | `null` | web/WASM | Inline elevation pattern. |

### 3.2 `rx` (`RxConfig`)

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `height_m` | f32 | `1.5` | |
| `mode` | string | `"AGL"` | `"AGL"`/`"AMSL"`. |
| `az_pattern_file` | string? | `null` | |
| `el_pattern_file` | string? | `null` | |
| `az_pattern_data` | `[[f32;2]]`? | `null` | |
| `el_pattern_data` | `[[f32;2]]`? | `null` | |

### 3.3 `analysis` (`AnalysisConfig`)

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `task_type` | string | `"SINGLE"` | Execution route (see below). |
| `propagation_model` | string | **required** | Output/physics mode (see below). |
| `max_range_km` | u32 | **required** | Max analysis range. |
| `resolution_m` | f32 | **required** | Output pixel resolution in metres. |
| `azimuth_start_deg` | f64 | `0.0` | Azimuth sweep window start. |
| `azimuth_end_deg` | f64 | `360.0` | Azimuth sweep window end. |
| `path_file` | string? | `null` | **Required when `task_type == "PATH"`.** §3.7 — **`[MPT-critical]`**. |
| `batch_file` | string? | `null` | **Required when `task_type == "BATCH_P2P"`/`"P2P"`.** Points at a `batch_links.csv` (§4). |
| `min_db` | f32 | `-140.0` | Signal floor; pixels weaker than this are treated as no-coverage. |
| `compute_backend` | string | `"AUTO"` | Backend selection (see below). |

**`task_type`** routing (`main.rs`):

| Value | Route | Notes |
|-------|-------|-------|
| `"SINGLE"` | coverage engine | Single-site area coverage. Any value other than the ones below also routes here. |
| `"PATH"` | path orchestrator | Reads `path_file`; runs one coverage per waypoint into `output.directory/wp_<i>/`. **`[MPT-critical]`** — see §3.7. |
| `"BATCH_P2P"` | P2P engine | Reads `batch_file`. |
| `"P2P"` | P2P engine | Same engine as `BATCH_P2P`. |

**`propagation_model`** → output format (`solver.rs::OutputMode::from_analysis_mode` / `as_str`):

| `propagation_model` | Internal mode | Sidecar `output_format` | Physics |
|---------------------|---------------|-------------------------|---------|
| `"LOS"` | `OneBitLos` | `"1BIT_LOS"` | 1-bit line-of-sight visibility. **Any unrecognized value also maps here.** |
| `"SIMPLE_LOSS"` | `EightBitProp` | `"8BIT_PROP"` | 8-bit FSPL + knife-edge. |
| `"ITM"` | `ItmLoss` | `"8BIT_PROP"` | 8-bit Longley-Rice (ITM). |
| `"MIN_ALT"` | `MinAltLos` | `"16BIT_ALT"` | 16-bit minimum-LOS-altitude map. **Coverage-only**: PATH and P2P jobs reject it with `[E:MIN_ALT is only supported for COVERAGE analysis]`. |

**`resolution_m`** — the engine accepts *any* positive `f32`; it performs **no
enum validation**. Consumers conventionally use one of **`{2, 5, 10, 30}`**
metres (the resolutions at which terrain `.abt` tiles are produced). This set is
a *consumer/data convention, not an engine constraint*; do not assume the engine
rejects other values.

**`compute_backend`** (`main.rs` for native; `aether_core_wasm` for the web build):

| Value | Native `aether_core` | WASM build |
|-------|----------------------|------------|
| `"AUTO"` (default / any unrecognized) | Try GPU, fall back to CPU. | Try GPU, fall back to the in-process CPU engine. |
| `"GPU"` | Force GPU. | (treated as AUTO — tries GPU, may fall back) |
| `"CPU"` | Force CPU. | Skip GPU; run the in-process CPU engine. |
| `"GPU_ONLY"` | (treated as AUTO) | **WASM-only:** try GPU; on failure **return an error** (`AETHER_GPU_UNAVAILABLE: …`) instead of the slow single-threaded CPU path, so the JS caller can orchestrate a multi-Worker CPU fallback. Matching is case-insensitive. |

### 3.4 `output` (`OutputConfig`)

| Field | Type | Required? | Notes |
|-------|------|-----------|-------|
| `directory` | string | **required** | Output directory (created if missing). |
| `filename` | string | **required** | Base name (no extension). Outputs are `<filename>.bit`, `<filename>.json`, `<filename>.tif`, or `<filename>.csv` depending on task. MPT_SIGMA sets this to `"coverage_report"` for coverage/path and `"p2p_results"` for P2P. |

### 3.5 `processing` (`ProcessingConfig`, all optional)

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `max_ram_usage_gb` | u64? | `null` | RAM budget for the memory-bounded solver. |
| `max_vram_usage_gb` | u64? | `null` | VRAM budget (P2P defaults to `8` GB when absent). |
| `terrain_dir` | string? | `null` | Directory of `.abt` terrain tiles. Defaults to `"data/cache_abt"` when absent. |

### 3.6 `propagation` (`PropagationConfig`, all optional)

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `eps_dielect` | f64 | `15.0` | Ground dielectric constant. |
| `sgm_conductivity` | f64 | `0.005` | Ground conductivity (S/m). |
| `eno_ns_surfref` | f64 | `301.0` | Surface refractivity (N-units). |
| `radio_climate` | u32 | `5` | ITM radio-climate code (1–7). |
| `pol` | u32 | `0` | Polarization: `0`=horizontal, `1`=vertical. |
| `conf` | f64 | `0.50` | ITM confidence fraction. |
| `rel` | f64 | `0.50` | ITM reliability fraction. |
| `earth_radius_mode` | string | `"FOUR_THIRDS"` | `"FOUR_THIRDS"` (built-in refractivity k-factor) or `"ADVANCED"` (Sandia/Doerry adaptive). |
| `bwd_scan_interval` | u32? | `null` | Backward-scan tuning. |
| `bwd_scan_min_range_m` | f32? | `null` | Backward-scan tuning. |
| `ltm_capacity` | u32? | `null` | Long-term-memory peak capacity. |

### 3.7 PATH task and the waypoint file (`[MPT-critical]`)

`task_type == "PATH"` is used **only by MPT_SIGMA** and is undocumented outside
this contract. `analysis.path_file` points at a **comma-separated waypoint
file** (`rust/aether_core/src/engines/path.rs`). Lines beginning with `#` and
blank lines are skipped. Columns (only the first three are required):

```
lat_wgs84, lon_wgs84, altitude_m, alt_mode, flight_heading, sweep_min, sweep_max, max_range_km
```

| Col | Field | Required | Default when absent |
|-----|-------|----------|---------------------|
| 0 | `lat_wgs84` (f64) | yes | — |
| 1 | `lon_wgs84` (f64, east-positive) | yes | — |
| 2 | `altitude_m` (f32) | yes | — |
| 3 | `alt_mode` | no | job `tx.mode`; only `AGL`/`AMSL` (case-insensitive) accepted |
| 4 | `flight_heading` (f32°) | no | `0.0` — becomes `tx.az_rotation` |
| 5 | `sweep_min` (f64°) | no | `analysis.azimuth_start_deg` |
| 6 | `sweep_max` (f64°) | no | `analysis.azimuth_end_deg` |
| 7 | `max_range_km` (f32) | no | job `analysis.max_range_km` (used only if `> 0`) |

Per waypoint `i`, the orchestrator writes a full coverage result into
`output.directory/wp_<i>/` (so files are `wp_<i>/<output.filename>.bit`, etc.).
**Per-waypoint sub-jobs run with the internal TIFF export suppressed** (see §5b),
so a bare PATH run produces `.bit`+sidecar per waypoint but no per-waypoint
`.tif`. Progress markers: `[P:0]` … `[P:100]`, plus
`[S:Processing Waypoint <i> of <n>]`.

---

## 4. `batch_links.csv` (BATCH_P2P / P2P input)

Authority: `rust/aether_core/src/engines/p2p.rs`. Comma-separated, `#`-comment
and blank lines skipped, **at least 5 columns required per row**:

```
# Type, ID_String, Latitude, Longitude, Altitude_meters, Mode
S, Site_0, 47.3700, 8.5400, 30.0, AGL
T, WP_0,   47.4200, 8.6100,  2.0, AGL
T, WP_1,   47.3100, 8.4800,  2.0, AMSL
```

| Col | Field | Notes |
|-----|-------|-------|
| 0 | `Type` | `S` = source/TX; anything else (conventionally `T`) = target/RX. Exact test: `ptype == "S"`. |
| 1 | `ID_String` | Free-form ID. **Echoed verbatim** into the result CSV's `Source_ID`/`Target_ID` columns. MPT_SIGMA uses `Site_<i>` for sources and `WP_<i>` for targets. |
| 2 | `Latitude` | f64, WGS84. |
| 3 | `Longitude` | f64, WGS84, **east-positive**. |
| 4 | `Altitude_meters` | f32. |
| 5 | `Mode` | Optional. `AGL`/`AMSL` (case-insensitive). When absent/unrecognized, falls back to the job's `tx.mode` (for sources) or `rx.mode` (for targets). |

The engine forms the full cross-product of every source against every target.

---

## 5. Outputs

### 5a. `.bit` / `.tiles` + `.json` sidecar

`aether_core` writes coverage results as a **tiled ATIL container** (§7) named
`<output.filename>.bit`, alongside a JSON **sidecar** `<output.filename>.json`.
Sidecar authority: `engines/coverage.rs` (the `serde_json::json!` block after the
tile file is finalized). Fields:

| Key | Type | Value / notes |
|-----|------|---------------|
| `job_id` | string | `= output.filename`. |
| `type` | string | Constant `"LOS_BITMASK"` — **emitted for every mode**, including 8-bit and 16-bit outputs. Informational; do not infer the pixel format from it — use `output_format`. |
| `output_format` | string | `"1BIT_LOS"` \| `"8BIT_PROP"` \| `"16BIT_ALT"` (§3.3). |
| `tile_format` | string | Constant `"TILED"` (selects the ATIL reader path in `aether_export`). |
| `tile_size` | int | `OUTPUT_TILE_SIZE` = `512`. |
| `dimensions` | object | `{ "width": <int>, "height": <int> }` in pixels. |
| `geotransform` | `[f64;6]` | GDAL order `[ul_lon, scale_x, 0, ul_lat, 0, -scale_y]` (north-up ⇒ negative `[5]`). |
| `projection` | string | Constant `"EPSG:4326"`. |
| `alt_step_m` | f64 | **`16BIT_ALT` only:** `0.5` (metres per stored step). |
| `alt_ref` | string | **`16BIT_ALT` only:** `"AGL_CLAMPED"`. |
| `alt_sentinel` | int | **`16BIT_ALT` only:** `65535` (no-data). |

`aether_export` additionally honors an optional `row_stride_bytes` (int) key —
used **only for legacy flat `.bit` input**; TILED sidecars do not set it.

### 5b. Direct GeoTIFF (`<filename>.tif`) — `[MPT-critical]`

After writing the `.bit`+sidecar, a single-site coverage run **produces a
GeoTIFF `<output.filename>.tif`** in the same directory. MPT_SIGMA reads this
`.tif` directly (e.g. `coverage_report.tif`) **without itself invoking
`aether_export`**.

> **Correction / precise mechanism (verified in `engines/coverage.rs`):**
> `aether_core` does *not* contain its own TIFF writer for this path. When the
> internal flag `export_tif` is true it **spawns the sibling `aether_export`
> binary** as a subprocess:
> ```rust
> // export_exe = current_exe().parent()/"aether_export"  (fallback: "aether_export" on PATH)
> Command::new(&export_exe)
>     .arg("-i").arg(&final_bit_path)
>     .arg("-j").arg(&sidecar_path)
>     .arg("-o").arg(&tif_path)   // <out_dir>/<filename>.tif
>     .status();
> ```
> Consequences for the contract:
> * **`aether_export` must be deployed beside `aether_core`.** MPT_SIGMA gets the
>   `.tif` "for free" because `aether_core` runs the exporter on its behalf.
> * `export_tif` is **true for single-site coverage** (the default `main.rs`
>   route) and **false for PATH sub-jobs** (`path.rs` calls the coverage engine
>   with `export_tif=false`) — so PATH waypoints yield `.bit`+sidecar but no
>   per-waypoint `.tif` unless a consumer runs `aether_export` itself.
> * The `.tif` is named `<output.filename>.tif`; the literal `coverage_report.tif`
>   arises because MPT_SIGMA sets `output.filename = "coverage_report"`.
> * If `aether_export` is missing or fails, `aether_core` logs to stderr
>   (`[Core] GeoTIFF export failed …` / `Could not launch aether_export …`) but
>   **still exits 0** — the `.tif` simply will not exist. Consumers that require
>   the `.tif` must check for its presence (fail-closed), not just the exit code.

### 5c. P2P outputs (`<filename>.csv` + profiles)

For `BATCH_P2P`/`P2P`, `aether_core` writes `<output.filename>.csv`
(MPT_SIGMA sets `filename = "p2p_results"`). **Both the column names and their
order are frozen** — the header and every data row (`engines/p2p.rs`):

```
Source_ID,Target_ID,Signal_dBm,Path_Loss_dB
```
```rust
writeln!(out_file, "Source_ID,Target_ID,Signal_dBm,Path_Loss_dB")?;      // header
writeln!(out_file, "{},{},{:.2},{:.2}", src_id, tgt_id, r.signal, r.loss)?; // row
```

| Col | Name | Type | Notes |
|-----|------|------|-------|
| 0 | `Source_ID` | string | The batch file's source `ID_String` (e.g. `Site_<i>`). |
| 1 | `Target_ID` | string | The batch file's target `ID_String` (e.g. `WP_<i>`). |
| 2 | `Signal_dBm` | f (2 dp) | Received signal power, dBm. |
| 3 | `Path_Loss_dB` | f (2 dp) | Path loss, dB. |

* **`[MPT-critical]`** — MPT_SIGMA parses this file **positionally**:
  `s_id_str, t_id_str, signal_str = parts[0], parts[1], parts[2]`
  (`workers/optimization_worker.py`). The QGIS side reads the **named** columns
  `Source_ID, Target_ID, Signal_dBm, Path_Loss_dB`. **Both the order (for
  MPT_SIGMA) and the names (for QGIS) are therefore load-bearing.** Do not
  reorder, rename, or insert columns.

**Single-link extras.** When the job resolves to **exactly one** source×target
link, `aether_core` also writes SPLAT-compatible profile files into
`output.directory`:

| File | Content |
|------|---------|
| `p2p_report.txt` | Human-readable link budget (ground elevations, antenna heights, distance, frequency, LOS yes/no or path-loss + signal). |
| `terrain_profile.gp` | `distance_km elevation_m` per sample (space-separated, `{:.4} {:.2}`). |
| `height_profile.gp` | `distance_km (elevation + earth-bulge)` per sample. |
| `path_profile.gp` | `distance_km value` — cumulative path loss (dB), or `0`/`1` visibility for LOS mode. |

These profile files are **not** written for multi-link batch runs (only the CSV
is).

---

## 6. `.abt` terrain tile format (input to `aether_core`)

Fixed **44-byte header**, little-endian, followed by the row-major payload.
Verified against **two independent implementations** — the reader
`rust/aether_core/src/io/mod.rs` (`AbtHeader` / `peek_abt` / `load_abt_from_bytes`)
and the writer `crates/aether_converter/src/download.rs` (`AbtWriter::create`).
**They agree byte-for-byte.**

| Offset | Size | Type | Field | Notes |
|-------:|-----:|------|-------|-------|
| 0 | 4 | bytes | magic | ASCII `AETH`. |
| 4 | 2 | u16 LE | `version` | `1` = R16SINT, `2` = BC6H. |
| 6 | 2 | u16 LE | `width` (a.k.a. `size`) | Tile side in pixels (square tile). |
| 8 | 8 | f64 LE | `ul_lat` | Upper-left latitude (WGS84). |
| 16 | 8 | f64 LE | `ul_lon` | Upper-left longitude (east-positive). |
| 24 | 8 | f64 LE | `scale_y` | Degrees per pixel (latitude). |
| 32 | 8 | f64 LE | `scale_x` | Degrees per pixel (longitude). |
| 40 | 2 | i16 LE | `base_elev` | BC6H dynamic elevation offset; **`0` for R16SINT**. |
| 42 | 2 | u16 LE | `row_stride` | **Bytes per row** of payload. |
| 44 | … | payload | | See below. |

**Payload / sampling.** For `version == 1` (R16SINT) the payload is
row-major `i16` elevations at **0.5 m granularity** — a stored value `raw`
means `raw * 0.5` metres (BC6H tiles add `base_elev`). A pixel `(row, col)` is
at byte offset:

```
44 + row * row_stride + col * 2
```

(`io/mod.rs::sample_abt_at`, `engines/p2p.rs::sample_elevation_cached`.)

**No-data pixels.** The ingest path writes **`-9999`** (i.e. -4999.5 m) for a
pixel it found no terrain for, and treats any stored value **at or below
`-5000`** (-2500 m) as no-data when it samples a source. Note both numbers are
in the stored half-metre unit, so the real floor is **-2500 m**, not -5000 m;
the deepest land depression on Earth is ~-430 m, so no genuine terrain is lost,
but a bathymetric source below -2500 m would be discarded.

> **Correction (previously undocumented).** This sentinel has always been
> emitted — `crates/aether_converter/src/ingest.rs` (`VOID_ELEV`) — but no
> earlier revision of this document mentioned it. It is recorded here as
> existing behavior, not as a new field. A consumer that treats `-9999` as an
> elevation reads a void as 4999.5 m below sea level.
>
> A void is **not** 0. Until the fix recorded here, a Float32 source whose no-data
> was NaN (GDAL's default) and an Int32 source whose no-data was `-2147483648`
> both decoded to **0 half-metres — sea level — and were written as valid
> ground**, because `f32::NAN as i16` is defined to be 0 and the Int32 path
> narrowed to `i16` (keeping only the low 16 bits, which are zero) before
> scaling. Those pixels now decode to `-9999`. **`.abt` bytes therefore change
> for any input that used Float32-NaN or Int32-minimum no-data**; every other
> input is byte-for-byte unchanged. `download`-produced tiles never had *that*
> bug — but the download path had its own 0-for-void defect, fixed separately:
> see changelog item 15 and §1.2, where a pixel no XYZ tile covered was written
> as 0 m and is now this same `-9999`.
>
> The `download` path has a different no-data convention on its *input* side: a
> Terrarium tile pixel decoding below **-11000 m** is a void and is backfilled
> from a coarser parent tile. That is an input rule and never reaches `.abt`.

**Resampling — how a source elevation becomes a pixel.** Both converter paths
**area-average**. An output pixel is the mean of every source sample whose
*centre* falls inside the ground footprint that pixel stands for, accumulated in
a wider type (`i32`/`i64` half-metres for `ingest`, `f32` metres for `download`)
and rounded once at the end. Two rules qualify it:

* **No-data is excluded from the mean, never averaged into it.** A footprint
  that mixes ground and no-data averages only the ground; a footprint that holds
  nothing but no-data stays no-data — `ingest` falls through to its next source
  and ultimately writes `-9999`, and `download` writes `-9999` too, whether the
  footprint held a source void (a blank Terrarium pixel) or no tile at all.
* **A source at or coarser than the target is not interpolated.** The footprint
  then holds no source-sample centre at all, and the pixel takes the single
  source sample it sits inside — the same sample the previous point sampler took.
  Nothing is invented between source samples, at any ratio.

> **Correction (changes `.abt` payload bytes for most inputs).** Until this
> change both paths took **one** source sample per output pixel and discarded
> the rest of the footprint — `ingest.rs` with a truncating index, `download.rs`
> with a rounded column/row lookup. At swissALTI3D's 0.5 m onto a 30 m grid that
> keeps 1 sample in 3600. The discarded relief does not disappear: it aliases
> into pixel-to-pixel jitter, which the engine's LOS test renders as
> checkerboard speckle. **`.abt` payload bytes therefore change for every input
> whose source is finer than the target**, which is most of them; the 44-byte
> header, the stride rule and the no-data sentinel are untouched.
>
> Two narrower consequences are worth stating separately:
> * `download` also had its source lookup half a sample too far east and south
>   (it rounded a grid *coordinate* to an index, but the assembly grid's sample
>   `i` covers `[i, i+1)`). That is corrected here, so `download` tiles move by
>   up to half a source sample as well as being averaged.
> * `ingest` at a ratio of exactly 1 (a base DEM warped onto the analysis grid)
>   used to pick between two adjacent source pixels depending on which side of
>   the integer the floating-point column landed on — a per-column coin flip on
>   arithmetic noise. It now resolves that consistently, which changes those
>   pixels by one source sample.
>
> Any consumer that pinned expected `.abt` bytes — including the "pixel-identical
> output" claim made for the earlier E1 building/ingest work — must re-baseline.
> The fixtures in `fixtures/formats/` are **unaffected**: `tiny_16x16.abt` is
> synthesized by `tools/make_fixtures.py`, not produced by the converter.

> **Correction (changes `.abt` payload bytes again — `ingest` registration).**
> The area-average above was specified over "the ground footprint that pixel
> stands for", but `ingest` anchored the ±half-pixel window on each output
> cell's **NW corner** instead of its centre, sampling everything half an
> output pixel to the north-west of the promised footprint. Compounding it,
> `GTRasterTypeGeoKey` (1025) was ignored, so a **RasterPixelIsPoint** source
> (Copernicus GLO, SRTM) was additionally misregistered half a *source* pixel
> to the east and south — together nearly a full source pixel at ratio 1,
> enough to void a pixel whose true footprint sits one pixel from a no-data
> boundary. Both are corrected: the window is now centred on the cell centre
> (§6 above now holds as written) and PixelIsPoint origins are shifted to the
> pixel corner on load (§9a). `.abt` payload bytes move for every `ingest`
> input; header, stride and sentinel are untouched. `download` is unaffected
> (its assembly-grid lookup was already centre-corrected above). Consumers
> that cache tiles keyed on converter behaviour must invalidate (the Waveshed
> plugin bumps its pool cache schema for this).

**Row stride details.**
* `row_stride` is a stored `u16`, so it caps the maximum tile width at
  **≈ 32 000 px** (`row_stride ≤ 65535` bytes ⇒ ≤ ~32 640 px at 2 bytes/px).
* The converter's `AbtWriter` writes **256-byte-aligned rows**:
  `stride = (width*2 + 255) & !255`. So a `width` whose `width*2` is not a
  multiple of 256 has trailing zero padding on each row. **Readers must always
  use the stored `row_stride`**, never assume `width*2`. (A fixture in
  `fixtures/formats/` follows this 256-byte alignment.)

---

## 7. ATIL tiled container (`.bit` / `.tiles`)

The coverage output container. Layout: **tile data blocks**, then a **binary
index**, then a **fixed 24-byte footer**. Verified against the writer
(`rust/aether_core/src/io/tiled_buffer.rs::write_index_and_footer`) and **two
independent readers** — `crates/aether_export/src/main.rs` (`TiledInput::open`)
and `crates/aether_aggregate/src/reader.rs`. All three agree.

```
┌───────────────────────────────┐
│ tile block 0                  │  raw (mode-dependent) tile bytes
│ tile block 1                  │
│ …                             │
├───────────────────────────────┤  ← index_offset
│ index entry 0 … N-1           │  20 bytes each
├───────────────────────────────┤
│ footer                        │  24 bytes
└───────────────────────────────┘
```

**Index entry — 20 bytes, little-endian, in this order:**

| Offset | Size | Type | Field |
|-------:|-----:|------|-------|
| 0 | 4 | u32 LE | `tx` (tile column) |
| 4 | 4 | u32 LE | `ty` (tile row) |
| 8 | 8 | u64 LE | `offset` (byte offset of the tile block) |
| 16 | 4 | u32 LE | `size` (tile block length in bytes) |

**Footer — 24 bytes, little-endian:**

| Offset | Size | Type | Field |
|-------:|-----:|------|-------|
| 0 | 8 | u64 LE | `index_offset` |
| 8 | 8 | u64 LE | `tile_count` |
| 16 | 4 | bytes | magic `ATIL` |
| 20 | 4 | u32 LE | `reserved` (`0`) |

Readers seek to `end − 24`, validate the `ATIL` magic, then read `tile_count`
20-byte entries starting at `index_offset`.

**Constants & invariants.**
* `OUTPUT_TILE_SIZE = 512` (`tiled_buffer.rs`). Each ATIL tile is 512×512 px.
* **Sparse tiles are omitted.** A tile that is entirely the fill byte
  (`0x00` for 1-/8-bit, `0xFF` for the `16BIT_ALT` sentinel) is not written and
  has no index entry; readers treat a missing tile as all-nodata.
* Per-tile bytes by mode (`solver.rs::OutputMode::tile_bytes` /
  in-tile row stride): `1BIT_LOS` = `512²/8` = 32 768 B, tile row = `512/8` = 64 B
  (u32-aligned); `8BIT_PROP` = `512²` = 262 144 B; `16BIT_ALT` = `512²·2` = 524 288 B.

**Row-stride invariants (flat buffers) — known failure mode.** For the *flat*
(non-tiled) in-memory representations the row stride is **u32-aligned**
(`solver.rs::OutputMode::global_row_bytes`):

| Mode | Flat row stride |
|------|-----------------|
| `1BIT_LOS` | `ceil(width/32) * 4` bytes |
| `8BIT_PROP` | `width` bytes |
| `16BIT_ALT` | `ceil(width/2) * 4` bytes |

**A byte-aligned buffer (`ceil(width/8)` for 1-bit, `width*2` for 16-bit) fed to
a u32-aligned reader shears every row** — the coverage circle renders as a
skewed square/block. Any new producer or consumer of these flat buffers must use
the u32 stride above, or realign at the boundary. (This is the documented CPU vs
GPU hazard from the engine's own guidelines.)

---

## 8. `.vix` visibility index (aggregate output → MPT_SIGMA)

Written by `aether_aggregate --visibility <path>` (a background writer thread in
`crates/aether_aggregate/src/aggregate.rs`) and consumed by MPT_SIGMA
(`MPT_SIGMA_KADAS_Plugin/core/vix_reader.py`, classes `VixReader`/`VixWriter`).
A **Dense Tiled CSR** index giving O(1) "which waypoints are visible at pixel
(x,y)". Verified against both the Rust writer and the Python reader/writer.

**File layout (in write order):**

```
┌──────────────────────────────────────┐
│ body: concatenated zlib(DEFLATE)      │  one compressed chunk per non-empty tile
│       tile chunks                     │
├──────────────────────────────────────┤  ← toc_offset
│ TOC: UTF-8 JSON  {tiles, meta}        │
├──────────────────────────────────────┤
│ footer: u64 LE toc_offset  (8 bytes)  │
│ magic:  "VIX!"             (4 bytes)  │  ← last 4 bytes of file
└──────────────────────────────────────┘
```

* Trailer is **12 bytes**: `toc_offset` (u64 LE) then `VIX!`. Readers seek
  `end − 4` for the magic and `end − 12` for `toc_offset`.
* **TOC JSON:**
  * `tiles`: map `"<tx>,<ty>" → { "offset": <u64>, "size": <u64> }` (byte range
    of that tile's compressed chunk in the body).
  * `meta`: at least `{ "master_width", "master_height", "tile_size" }`. The
    Rust writer additionally includes `geotransform` and
    `projection: "EPSG:4326"`; the Python writer includes only the three core
    keys. Readers must tolerate either (the extra keys are additive).
* **Each decompressed tile chunk** is:
  * `offsets`: `u32[TILE_SIZE² + 1]` — CSR row pointers.
  * `wp_ids`: `u16[…]` — CSR column data (waypoint IDs).
  * Split point between the two arrays = `(TILE_SIZE² + 1) * 4` bytes.
* **`TILE_SIZE = 2048`** and is **hardcoded in the MPT_SIGMA reader**. The Rust
  writer's per-tile array size is `tile_size²`, driven by `aether_aggregate
  --tile-size`; therefore **`--tile-size` MUST be `2048`** for a `.vix` that
  MPT_SIGMA can read (see §1.4).
* **Lookup:** for master-grid pixel `(x, y)`: `tx = x // 2048`, `ty = y // 2048`,
  `local = (y % 2048) * 2048 + (x % 2048)`; visible IDs =
  `wp_ids[offsets[local] : offsets[local+1]]`.

Waypoint IDs are parsed by `aether_aggregate` from `wp_<n>` substrings in each
input path (so PATH outputs under `…/wp_<n>/…` are keyed by `<n>`).

---

## 9. Ingest & download job JSON (`aether_converter`)

### 9a. Ingest job (`aether_converter ingest --job-file`)

Authority: `crates/aether_converter/src/ingest.rs` (`IngestJob`). The file may be
a **single object** or a **JSON array** of such objects (batch).

| Field | Type | Required? | Notes |
|-------|------|-----------|-------|
| `output_path` | string (path) | **required** | Destination `.abt` file. |
| `format` | string? | optional | `"r16sint"` (default behavior) or `"bc6h"`. |
| `ul_lat` | f64 | **required** | Upper-left latitude. |
| `ul_lon` | f64 | **required** | Upper-left longitude (east-positive). |
| `resolution_m` | f64 | **required** | Metres per pixel. |
| `size_px` | u32 | **required** | Output tile side (px). |
| `sources` | array of source | optional (v2.0) | Prioritised terrain inputs; see **Sources** below. Array order is priority: the **first** source with a valid sample under a pixel wins. Mutually exclusive with `base_tif`/`swiss_tifs`. |
| `void_fill_m` | f64? | optional (v2.0) | A pixel **no** source covers is written as `round(void_fill_m * 2)` half-metres (saturating i16) instead of the `-9999` sentinel. Buildings rasterize after, unchanged. Absent keeps the sentinel. |
| `base_tif` | string? | **DEPRECATED** | Alias: normalized to a trailing `sources` entry with `crs "EPSG:4326"`. Slated for removal in the next major version. |
| `swiss_tifs` | array of string | **DEPRECATED**, no longer required | Alias: normalized to leading `sources` entries with `crs "EPSG:2056"` (in order — which preserves the old per-pixel priority: this stack first, `base_tif` as fallback). Slated for removal in the next major version. |
| `buildings_file` | string? | optional | FlatGeobuf building footprints to burn in — one `.fgb`, or a directory of them (a directory holding no `.fgb` is an error, not an empty burn). Height follows the **building height ladder** below: geometry Z as an **absolute roof elevation (AMSL)** where the geometry has one, else the attribute rungs as **above-ground** metres. A source that cannot be opened, parsed or scanned fails the run rather than silently producing building-less tiles. |
| `buildings_pbf_dir` | string? | optional | Directory of Mapbox-Vector-Tile building tiles named `{z}_{x}_{y}.pbf` (gzip or plain), e.g. an OpenFreeMap planet fetch. Vector tiles carry no geometry Z, so height starts at the ladder's attribute rungs and is always **above-ground**, resolved against the terrain under each footprint. All tiles in the directory must share **one** zoom; a mixed-zoom directory fails with `buildings_pbf_dir mixes zoom levels` (same rule and same message as §9b — see the note below). A directory that cannot be read fails the run rather than silently producing building-less tiles. |

Both building fields are optional and additive; a job that omits them behaves
exactly as before. They may be combined, in which case FlatGeobuf is applied
first.

**Building height ladder (both sources).** One ladder, walked in this order,
stopping at the first rung that yields a **finite, strictly positive** value.
A value that is zero, negative or unparseable does not stop the walk — it
falls through to the next rung.

| # | Rung | Datum | Units |
|---|------|-------|-------|
| 1 | geometry Z (FlatGeobuf 3D; the maximum Z of the ring) | **absolute**, metres above mean sea level | m AMSL |
| 2 | attribute `height`, `render_height`, `building:height` (first present wins, in that order) | **above ground** | m |
| 3 | attribute `levels`, `building:levels`, `building_levels` (first present wins, in that order) × **3.0 m** per storey | **above ground** | storeys → m |
| 4 | default **6.0 m** | **above ground** | m |

* **Rungs 2–4 are never absolute.** An attribute height is metres from the
  ground to the top of the building, the OSM convention. Reading one as an
  elevation would put a 12 m building 528 m below Bern's terrain, where the
  `max` write rule silently discards it — the failure mode this ladder
  replaced.
* **Attribute values are text.** The leading decimal number is taken and the
  rest ignored, so `"12"`, `"12.5"` and `"12 m"` all read as the same height.
  Numeric columns and MVT numeric tag values are read directly.
* Rung 1 applies only where the geometry actually carries Z. A 2D geometry in
  an otherwise 3D file falls to rung 2, per feature.
* An absolute roof that lands below zero in half-metre units is dropped, as
  it always has been; an above-ground height is screened by the rasterizer
  against real terrain instead.

> **Behaviour change for consumers.** A 2D `buildings_file` used to draw
> **nothing at all** and exit 0 — the output was byte-identical to the
> building-less tile. It now draws, at rung 2, 3 or 4. A consumer that cached
> such tiles under a buildings-keyed identity was caching terrain; those
> entries are stale and must be rebuilt. Output for a source whose geometry
> carries Z is unchanged, byte for byte.
>
> Two further reads were wrong for **any** dimension and are fixed in the same
> release, so a 3D source may also start drawing where it did not before:
> a FlatGeobuf writes its geometry type once in the **header** and repeats it
> per feature only in a mixed-type file, so reading it off the feature alone
> saw `Unknown` and skipped every feature of an ordinary single-type file;
> and a polygon's ring `ends` count **points**, not coordinate slots, so a
> multi-ring footprint was read at half length and discarded as degenerate.

**`[Buildings]` on ingest (additive).** A `buildings_file` burn prints one
line per tile to stdout, opening with the same `[Buildings]` marker `download`
has always used (§1.2). Both the drew-something and the drew-nothing case are
reported; the ingest path previously printed nothing whatsoever on a
successful burn, so a consumer could not tell a burn from a no-op. The counts
after the marker are diagnostic and not frozen — match the marker, not the
wording.

```
[Buildings] <source>: <n> polygon feature(s) → <n> footprint(s), <n> with a height from the data (<n> at the 6 m default), <n> drawn, <n> px raised
[Buildings] <source>: <n> polygon feature(s) in this tile's extent, no footprint to draw
```

**A `buildings_file` that cannot be read fails the run.** Opening, parsing or
scanning the source, and a directory holding no `.fgb`, are hard errors naming
the file, and the tile is **not** written — matching what `buildings_pbf_dir`
has done since v2.0. Finding no buildings *over a given tile* is not that: an
edge tile legitimately has none, so it is reported and its terrain written.

**Accepted geometry types (additive).** Features whose geometry is `Polygon`,
`MultiPolygon`, **`TIN`**, **`PolyhedralSurface`** or **`Triangle`** are
burned; anything else is skipped per feature (points/lines are not
footprints). TIN and PolyhedralSurface — the shape GDAL's FlatGeobuf driver
writes for swissBUILDINGS3D 3.0 solids/roofs — walk exactly like a
MultiPolygon: parts recurse, rings split on `ends`, and each part takes its
own max Z as the absolute roof. They were previously skipped silently, so a
whole TIN dataset burned nothing with only the "no footprint to draw" line as
a trace.
The message keeps the phrase **`failed to apply buildings`** that the earlier
warning used, so a consumer already grepping for it still matches:

```
failed to apply buildings from buildings_file "<path>" to "<output>": <cause>; refusing to write a building-less tile
```

> **Behaviour change for consumers.** This used to be one `[Warn] Failed to
> apply buildings: …` line and exit 0, which handed back building-*less*
> terrain under an identity claiming buildings — a permanent cache hit. A
> consumer that treated exit 0 as "buildings applied" was wrong then and is
> right now; one that already parsed the warning needs no change beyond
> tolerating the non-zero exit.

**Sources (v2.0).** Each `sources` entry is `{path, crs?, nodata?}`:

* `path` — a GeoTIFF or an **`.abt` tile**; the two are told apart by the
  `AETH` magic, **never by extension**. An `.abt` source is self-describing:
  its 44-byte header (§6) fixes the geometry — geographic WGS84 degrees,
  square tile, per-pixel step from `scale_x` (older builds wrote the tile's
  whole *span* there; anything ≥ 0.005° is treated as a span and divided by
  the size, matching the plugin's reader) — and its payload is **already**
  i16 half-metres, loaded raw with the row-stride padding stripped and
  `-9999` voids passing through as no-data. Sampling and area-averaging are
  identical to a geographic GeoTIFF source. Hard errors, each naming the
  file: a `crs` or `nodata` field on an `.abt` source (*"abt sources are
  self-describing — remove …"*), a truncated/corrupt header, and a BC6H
  (version 2) tile — only R16SINT (version 1) is ingestable.
  A GeoTIFF `path` **must** carry a geotransform (a
  `ModelTransformation` tag, or `ModelTiepoint` + `ModelPixelScale`); a file
  without one is a **hard error** naming the file. Georeferencing is **never**
  derived from file names (the old filename fallback is gone).
  `GTRasterTypeGeoKey` (1025) is honoured: when it is `2`
  (**RasterPixelIsPoint** — how Copernicus GLO and SRTM ship), the origin
  names the *centre* of pixel (0,0) and is shifted by half a source pixel to
  the NW corner before sampling, exactly as GDAL does on read. Absent key or
  `1` (RasterPixelIsArea) reads the origin as the corner unchanged.
  A GeoTIFF `path` must also be **single-band** (greyscale colortype, one
  sample per pixel, any sample type): an RGB(A)/palette/GrayA file is a
  picture — a rendered basemap, hillshade or photo — and is a **hard error**
  naming the file (*"… not an elevation raster"*), raised before any sample
  is decoded. A decode that yields anything but exactly `w·h` samples is
  refused the same way.
* `crs` — `"EPSG:nnnn"` or a raw proj string starting with `+`. Optional:
  when absent, the file's GeoKeyDirectory is read — `ProjectedCSTypeGeoKey`
  (3072) first, else `GeographicTypeGeoKey` (2048). An absent key, or the
  user-defined value 32767, is a **hard error** telling the user to add
  `"crs"` to that source. An EPSG code missing from the built-in registry,
  or a proj string that does not parse, is likewise a hard error naming the
  code/file. A **geographic** CRS is sampled directly as lon/lat degrees
  (the historical `base_tif` fast path, byte-identical); a **projected** CRS
  is sampled through a WGS84→source transform (proj4rs) — EPSG:2056 output
  thereby moved ≤ 1 m vs. the retired polynomial, i.e. up to one source
  sample.
* `nodata` — no-data value in source units. Optional: when absent, the
  file's `GDAL_NODATA` ascii tag applies if present. Matching samples become
  the `-9999` void sentinel **at load time**; the Float32-NaN and
  Int32-widening rules of §6 apply unchanged on top.
* Supplying `sources` **and** either legacy field in one job is a **hard
  error** (the priority order would be ambiguous). A job with none of the
  three writes an all-void (or all-`void_fill_m`) tile as before at the
  library level, but the CLI refuses a whole run that covered nothing (§1.2,
  "An ingest that covered nothing exits non-zero").
* A listed source that cannot be opened or decoded **fails the tile** —
  never a warning.

> **Mixed zooms are refused on both paths.** `ingest` previously accepted a
> mixed-zoom `buildings_pbf_dir` and drew every zoom's copy of the same
> building, so each above-ground height was applied more than once and the
> building grew (see the precondition below). It now refuses the directory with
> the same message `download` has always used. A single-zoom directory — the
> only kind that ever produced correct output — is unaffected.

**Building write rule (both sources).** Every height off the ladder above is
normalised to an absolute roof elevation — an above-ground rung by adding the
terrain under its own footprint, read before anything is written — and
composited with `max` against the surface, never added to it. Consequences
callers can rely on:

* a roof is **flat**, even where the terrain under the footprint slopes;
* overlapping footprints do **not** accumulate;
* re-applying an *absolute*-height source is a no-op.

**Precondition.** Buildings must be burned into building-free terrain. An
above-ground height is measured against the surface it is written onto, so
re-applying one to a tile that already contains buildings measures from the
previous roof and the building grows. Callers that cache `.abt` tiles must key
"with buildings" separately from "without".

### 9b. Download job (`aether_converter download --job-file`)

Authority: `crates/aether_converter/src/download.rs` (`DownloadJob` / `SubTileSpec`).

| Field | Type | Required? | Notes |
|-------|------|-----------|-------|
| `url_template` | string | **required** | XYZ URL with `{z}`/`{x}`/`{y}` placeholders. |
| `encoding` | string | **required** | `"terrarium"` or `"mapbox"` (case-insensitive). Any other value fails: `Unknown encoding '<x>'`. |
| `output_dir` | string (path) | **required** | Directory for the produced `.abt` tiles. |
| `zoom` | u32 | **required** | XYZ zoom level. |
| `max_connections` | usize? | optional | Concurrent HTTP connections. Default **256** when absent. |
| `buildings_pbf_dir` | string? | optional | Directory of `{z}_{x}_{y}.pbf` vector tiles, same encoding and same **building height ladder** (§9a) as §9a's `buildings_pbf_dir`. When present, buildings are fused onto the finished `.abt` tiles as a post-pass once the download completes. All tiles in the directory must share **one** zoom; a mixed-zoom directory fails with `buildings_pbf_dir mixes zoom levels`. Absent means terrain only. |
| `tiles` | array of `SubTileSpec` | **required** | One entry per output `.abt`. |

`SubTileSpec`:

| Field | Type | Notes |
|-------|------|-------|
| `filename` | string | Output `.abt` name within `output_dir`. |
| `ul_lat` | f64 | Upper-left latitude. |
| `ul_lon` | f64 | Upper-left longitude (east-positive). |
| `size_px` | u32 | Tile side (px). |
| `resolution_m` | f64 | Metres per pixel; the writer sets `scale_x = scale_y = resolution_m / 111111`. |

**Tile size is read, never assumed.** The job says nothing about how large the
source's XYZ tiles are, and services disagree: AWS Terrarium serves **256 px**
tiles, MapTiler's `terrain-rgb` serves **512 px** ones (its `tiles.json`
declares `"scale": "2.000000"`, i.e. `@2x`). The edge is therefore taken from
the first PNG that decodes and held for the rest of the run; the assembly grid,
its row stride and the per-tile-row sample count all follow it.

This does **not** change the ground resolution: `zoom` is the caller's choice
and is untouched. A 512 px z12 tile covers exactly the ground a 256 px z12 tile
covers, with four times the samples, and the area-averaging of §6 absorbs them.

Three tile-geometry faults are **hard errors**, each naming what arrived. They
abort the whole run rather than counting as one lost tile, because tile geometry
is a property of the service and every other tile is wrong the same way:

| Fault | Message (prefix `XYZ tile geometry:`) |
|-------|----------------------------------------|
| Non-square tile | `tile is {w}x{h} px; XYZ tiles must be square` |
| Not 8 bits per channel | `tile is {n}-bit; XYZ terrain tiles must be 8 bits per channel` |
| Mixed sizes in one run | `tile x={x} y={y} is {a}x{a} px but this source already served {b}x{b} px tiles; one XYZ service cannot mix tile sizes — the assembly grid has one stride.` |

These are distinct from the `Tile download failed:` refusal above: that one
counts lost tiles against a majority threshold, whereas any single tile with
unusable geometry stops the run. Both remove the `.abt` files the run created.

Anything else a tile source does wrong — a 404, a timeout, a WebP body — stays a
per-tile failure, tallied in `[Stats] ERRORS` and judged by the majority rule.

---

## 10. License key format

Authority: `python/utils/keygen.py` (issuer) and
`rust/aether_core/src/crypto.rs` (verifier). Byte layout only — no key material.
Worked byte-offset examples and structural goldens live in
[`../fixtures/keys/`](../fixtures/keys/README.md).

There are **two key versions**. Both share the same envelope: a binary blob,
**Base58-encoded** using the **Bitcoin alphabet** (`base58` in Python / `bs58`
in Rust, both default to that alphabet), carrying a **payload** followed by a
**64-byte Ed25519 signature** over exactly that payload. All multi-byte integers
are **big-endian**, and all day counts are measured from the epoch
**`2026-01-01`**.

* **v1** — the original **general / unlocked** key. Decoded blob is **84 bytes**.
* **v2** — the new **node-locked** key (binds a license to one machine). Decoded
  blob is **116 bytes**.

**Version discriminator — by decoded length.** After Base58-decoding, the blob
length selects the version: **`84` ⇒ v1, `116` ⇒ v2**. Any other decoded length
is **malformed** and must be rejected. There is no version byte; the **length is
the discriminator**.

### 10.1 v1 — general / unlocked (84 bytes, unchanged)

* **84-byte blob** = `payload[0:20]` + `sig[20:84]`:
  * **payload — 20 bytes** `[0:20]`.
  * **Ed25519 signature — 64 bytes** `[20:84]`, over `payload[0:20]`.
* **Payload layout (20 bytes):**

  | Offset | Size | Type | Field | Notes |
  |-------:|-----:|------|-------|-------|
  | 0 | 2 | u16 **big-endian** | `exp_days` | Expiry, days since the epoch `2026-01-01`. |
  | 2 | 2 | u16 **big-endian** | `maint_days` | Maintenance-window end, days since `2026-01-01`. |
  | 4 | 16 | bytes | `master_seed` | Shader-decryption master seed. |

  (Issuer: `struct.pack(">HH", exp_days, maint_days) + master_seed_bytes`.
  Verifier: `u16::from_be_bytes(payload[0..2])`, `payload[2..4]`, `payload[4..20]`.)

**v1 keys remain valid indefinitely.** Adding v2 does not deprecate, shorten, or
otherwise change v1: an already-issued v1 key keeps verifying exactly as before.

### 10.2 v2 — node-locked (116 bytes, new)

A v2 key extends the v1 payload with a 32-byte **machine fingerprint**, letting
the vendor bind a license to a specific machine.

* **116-byte blob** = `payload[0:52]` + `sig[52:116]`:
  * **payload — 52 bytes** `[0:52]`.
  * **Ed25519 signature — 64 bytes** `[52:116]`, over `payload[0:52]`.
* **Payload layout (52 bytes):**

  | Offset | Size | Type | Field | Notes |
  |-------:|-----:|------|-------|-------|
  | 0 | 2 | u16 **big-endian** | `exp_days` | Expiry, days since `2026-01-01` (as v1). |
  | 2 | 2 | u16 **big-endian** | `maint_days` | Maintenance-window end, days since `2026-01-01` (as v1). |
  | 4 | 16 | bytes | `master_seed` | Shader-decryption master seed (as v1). |
  | 20 | 32 | bytes | `fingerprint` | Machine fingerprint = `SHA-256(machine_id_string)`. **All-zero ⇒ not node-locked** (runs on any machine). |

  The first 20 payload bytes are byte-for-byte the v1 payload; v2 only **appends**
  the 32-byte `fingerprint`, and the signature now covers all 52 payload bytes.

**Fingerprint semantics.**

* `fingerprint` = **`SHA-256(machine_id_string)`**, 32 bytes. The **user-facing
  fingerprint** is the **lowercase hex** of those 32 bytes (**64 hex chars**) —
  exactly the string `aether_core --fingerprint` prints (§1.1).
* **An all-zero fingerprint (32 × `0x00`) means the key is *not* node-locked**
  and runs anywhere. A vendor issues an unlocked v2 key by zero-filling this
  field.
* `machine_id_string` is read per-OS:

  | OS | `machine_id_string` source |
  |----|----------------------------|
  | Windows | registry `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` |
  | Linux | `/etc/machine-id` (fallback `/var/lib/dbus/machine-id`) |
  | macOS | `IOPlatformUUID` (IOKit) |

  The engine derives the fingerprint from the OS itself; it **never accepts a
  fingerprint as input** (see `--fingerprint`, §1.1).

### 10.3 Verification & enforcement (proprietary build)

Enforcement is compiled in only under the `proprietary` Cargo feature; dev builds
do not require a license (§2). Checks run in this order:

1. **Signature** — verified against the vendor public key over the version's
   payload (`payload[0:20]` for v1, `payload[0:52]` for v2). A bad signature is
   fatal.
2. **Expiry** — `today_days > exp_days` ⇒ `[FATAL] License expired`.
3. **Maintenance** — `build_days > maint_days` ⇒ *"released after your
   maintenance period ended"*.
4. **Node-lock (v2 only)** — if the v2 `fingerprint` is **not** all-zero, require
   `SHA-256(local machine_id) == fingerprint`; otherwise the engine fails with a
   fatal *"locked to a different machine"* error. An all-zero fingerprint skips
   this step (runs anywhere), and v1 keys have no fingerprint so never reach it.

`today_days`/`build_days` are counted from `2026-01-01`. Provide the key via
`AETHER_LICENSE` or a `license.key` file (§2). The `--fingerprint` query (§1.1)
short-circuits **before** all of the above, so it needs no key.

### 10.4 Compatibility & the one consumer requirement

Adding v2 is an **additive** change under the compatibility policy (see the top
of this document): v1 keys stay valid indefinitely, and the v2 length (`116`) is
simply an *additional* accepted decoded length. Keys are a **binary blob, not
JSON**, so no `schemas/` file applies — this section plus the `fixtures/keys/`
goldens are the whole surface.

**Load-bearing consumer requirement:** any consumer that length-checks the
decoded blob **MUST accept both `84` and `116`.** The **Waveshed QGIS plugin**
(`api_key.py`) previously asserted `len == 84` and was **updated** to accept
both; the **pipeline GUI** `inspect_key` parses both lengths. A consumer that
still hard-checks `== 84` will **reject every v2 (node-locked) key**. Consumers
that treat the key as an **opaque** string and never length-check it are
unaffected — **MPT_SIGMA** passes `AETHER_LICENSE` through verbatim
(`workers/splat_worker.py`) and needs no change.

---

## 11. Release / distribution manifest (`latest.json`)

Consumed by the **Waveshed QGIS plugin** to discover, verify, and install engine
binary updates. It is served from waveshed.io.

> **Verification caveat:** unlike every other section, this schema could **not**
> be verified against readable source — the sole consumer is the QGIS plugin,
> which is out of scope for this repository, and no `latest.json`,
> `min_plugin_version`, `schema_version`, or `sha256`-manifest producer exists
> anywhere in the readable repos (`AETHER`, `MPT_SIGMA`, this workspace). The
> schema below is the agreed interface; when the QGIS plugin or the web release
> tooling becomes inspectable, re-verify and update `schemas/release_manifest.schema.json`.

```jsonc
{
  "schema_version": 1,
  "version": "1.4.2",                       // engine/bundle semver
  "released_at": "2026-07-01T12:00:00Z",    // ISO-8601 UTC
  "changelog_url": "https://waveshed.io/…", // optional
  "eula_url": "https://waveshed.io/eula",   // optional
  "min_plugin_version": "2.7.0",            // reject if plugin older
  "assets": [
    {
      "platform": "windows",                // windows | linux | macos
      "arch": "x64",                        // x64 | arm64
      "filename": "aether-1.4.2-win-x64.zip",
      "url": "https://waveshed.io/dl/…",
      "size_bytes": 123456789,
      "sha256": "<64 hex chars>"
    }
  ]
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `schema_version` | int | yes | `1`. Bump on breaking manifest changes. |
| `version` | string | yes | Semver of the release. |
| `released_at` | string | yes | ISO-8601 UTC timestamp. |
| `changelog_url` | string | no | |
| `eula_url` | string | no | |
| `min_plugin_version` | string | no | Plugin refuses assets requiring a newer plugin. |
| `assets` | array | yes | ≥ 1 asset. |
| `assets[].platform` | enum | yes | `windows`\|`linux`\|`macos`. |
| `assets[].arch` | enum | yes | `x64`\|`arm64`. |
| `assets[].filename` | string | yes | |
| `assets[].url` | string | yes | Download URL. |
| `assets[].size_bytes` | int | yes | |
| `assets[].sha256` | string | yes | Lowercase 64-hex digest. |

* **`sha256` is fail-closed.** The plugin must compute the SHA-256 of the
  downloaded asset and **refuse to install** on any mismatch, missing digest, or
  absent asset for the running platform/arch. A download that cannot be verified
  is treated as a failure, never as "install anyway".

---

## Appendix — files that back this contract

| Concern | Source of truth |
|---------|-----------------|
| `job.json` schema | `AETHER/rust/aether_core/src/config.rs` |
| task routing, `[E:]` on failure, exit codes | `AETHER/rust/aether_core/src/main.rs` |
| output modes, row strides | `AETHER/rust/aether_core/src/solver.rs` |
| sidecar, direct `.tif`, wedge markers | `AETHER/rust/aether_core/src/engines/coverage.rs` |
| PATH waypoint file | `AETHER/rust/aether_core/src/engines/path.rs` |
| P2P CSV + profiles | `AETHER/rust/aether_core/src/engines/p2p.rs` |
| `.abt` reader | `AETHER/rust/aether_core/src/io/mod.rs` |
| ATIL writer | `AETHER/rust/aether_core/src/io/tiled_buffer.rs` |
| license verify (v1 + v2), node-lock enforcement, `--fingerprint` | `AETHER/rust/aether_core/src/{crypto.rs,main.rs}` |
| license issue (v1 + v2 keygen) | `AETHER/python/utils/keygen.py` |
| license-blob length check (must accept 84 **and** 116) | QGIS `…/api_key.py`; pipeline GUI `inspect_key` |
| WASM backend selection | `AETHER/rust/aether_core_wasm/src/lib.rs` |
| `.abt` writer, disk-space string, download job | `crates/aether_converter/src/{download.rs,ingest.rs}` |
| ATIL reader, GeoTIFF export | `crates/aether_export/src/main.rs` |
| `.vix` writer, ATIL reader, COG output | `crates/aether_aggregate/src/{aggregate.rs,reader.rs,writer.rs}` |
| `.vix` reader (consumer) | `MPT_SIGMA/…/core/vix_reader.py` |
| P2P CSV positional parse, PATH usage | `MPT_SIGMA/…/workers/optimization_worker.py`, `core/analysis_manager.py` |
