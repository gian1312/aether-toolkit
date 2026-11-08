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
> 16. **`download` repairs a wholly-missing tile from coarser parents
>    (§1.2).** The parent backfill that already repaired in-tile voids never
>    fired for a tile whose fetch failed outright — the `Ok(...)` guard in
>    front of `fill_from_parents` skipped it — so a 404'd tile stayed void
>    (item 15) even when its z-1 ancestor was on the server. It is now
>    repaired from the first ancestor that decodes (down to the same minimum
>    zoom the in-tile repair uses); a tile with no usable ancestor at any
>    zoom stays fully void. A **repaired tile counts as OK**, not as an
>    error (narrowed by item 24: only a repaired **404** does; any other
>    repaired failure is "degraded"): `[Stats] Tiles: N/M OK` and the >50%-loss fatal guard both see
>    it as fetched (the request failure still appears in the error-class
>    log lines), and a new line — `[Stats] Repaired from coarser parent
>    tiles: N` — reports the repairs. **Bytes change for any run that lost
>    a tile an ancestor covers**: those pixels were `-9999` (or, before
>    item 15, fake 0 m) and are now upsampled parent terrain.
> 17. **`download` HTTP 404 is no-data, never failure (§1.2; 2026-08-31).**
>    A 404 is the service answering "no tile here" — a bounded source (a
>    regional DEM service, a local fixture) answers it for the whole world
>    outside its coverage. 404s are now tallied in their own error class
>    (`HTTP_404_no_data`), reported on a new `[Stats] NO-DATA (HTTP 404):`
>    line, and **excluded from the >50% fatal guard**, which now counts only
>    genuine failures (timeout, connect, 403/429/4xx-other, 5xx, decode,
>    other). A run whose only losses are 404s — **all** of them included —
>    exits 0 and keeps its output, void where no tile exists; consumers that
>    fill voids (Waveshed Site Analysis, `void_fill_m: 0.0`) read those areas
>    as 0 m sea level and are expected to tell the user so. Behavior change:
>    a majority-404 run previously exited non-zero and deleted its output.
>    Consumers matching `Tile download failed:` are unaffected in the cases
>    where it still fires; consumers that RELIED on majority-404 aborting
>    must now read the `[Stats] NO-DATA` line instead. `.abt` bytes are
>    unchanged for any run that previously succeeded.
> 18. **`ingest` batch with zero covered pixels is no-data, not an error
>    (§9a; 2026-08-31).** The same ruling as item 17, one path over: a batch
>    whose tiles took not one pixel from any source previously failed
>    (`anyhow::bail!`, "the batch holds no terrain"). It now exits 0, keeps
>    its tiles — void, or `void_fill_m` where the job sets it — and prints
>    one loud line (frozen prefix):
>    `[Warn] NO-DATA batch: <n> tile(s) were written and not one pixel of any
>    of them came from a source — …`. A run over ground outside a bounded
>    DEM is legitimate (Waveshed Site Analysis reads it as 0 m sea level and
>    warns from the source bounds); a wrong-CRS mistake produces the same
>    shape, and this line — plus the caller's own bounds check — is what
>    keeps it visible. Consumers that relied on the bail must match the
>    `[Warn] NO-DATA batch:` prefix. Per-tile partial coverage was always
>    exit 0 and is unchanged.
> 19. **P2P honors `compute_backend: "CPU"`, and a failed link is 9999, not 0
>    (§3.3/§7; 2026-09-01).** P2P/BATCH_P2P previously ignored
>    `compute_backend` and always ran the GPU; `"CPU"` now selects a reference
>    CPU implementation (same links, same frozen CSV columns, same single-link
>    artifacts). Independently, a link whose loss cannot be computed (non-finite
>    ITM result, pattern-blocked bearing) reports `Path_Loss_dB` = `9999.00` —
>    both backends — where some failure paths previously wrote `0.00`, which
>    reads as a perfect link. LOS-mode `-1`/`0` visibility is unchanged.
> 20. **`download` job `fetch_bounds` (§9b, additive; 2026-09-05).** Optional
>    region of interest: only source tiles intersecting it are fetched and
>    decoded, everything else keeps the `-9999` void pre-fill; output grid,
>    geometry and `.abt` layout unchanged. In-memory downloader only — the
>    file-based `download` CLI rejects jobs carrying it (its sparse writer
>    would read skipped rows as 0 m sea level). Same change: web tile URLs
>    are sharded by tile coordinate (`(x+y) % hosts`) instead of fetch order,
>    so a tile's URL is stable across runs (browser-cache reuse); URL choice
>    is not a contract surface, noted for completeness.
> 21. **`download_terrain` optional `fetch_fn` (WASM surface only, additive;
>    2026-09-14).** The `aether_converter_wasm` export takes an optional third
>    argument: a JS `(url) => Promise<{ok, status, arrayBuffer()}>` used for
>    every source-tile request in place of the global `fetch`. Omitted or
>    `undefined`, behaviour is exactly as before. Nothing else moves — the
>    `download` job JSON (§9b), the planned tile URLs, the retry/backoff and
>    progress callbacks, and the `.abt` bytes are all unchanged, and no native
>    or CLI surface is touched. Consumer: the waveshed.io web pipeline, which
>    serves source tiles from its own browser cache.
> 22. **`ingest` honours non-square source pixels (§9a; changes `.abt`
>    bytes; 2026-10-13).** A GeoTIFF source's X and Y pixel steps are read
>    separately (`ModelPixelScale[0]` / `[1]`, or `ModelTransformation[0]` /
>    `[5]`). The reader used the X step for both axes, so a file whose pixels
>    are not square — Copernicus GLO-30 north of 50°N (0.000417° × 0.000278°),
>    any geographic grid with equal metres per pixel — was stretched
>    north–south by the step ratio and "covered" ground beyond its real edge.
>    Measured on `Copernicus_DSM_COG_10_N50_00_E014_00_DEM.tif` against GDAL:
>    90 m median / 432 m max error before, ≤ 0.5 m (the storage step) after.
>    Square-pixel sources are byte-identical. This regressed when contract
>    v2.0 moved reprojection from the consumer's GDAL warp (which handles any
>    pixel shape) into `ingest`; consumers that cache file-built tiles must
>    invalidate them (the Waveshed plugin's `_FILE_SOURCE_SCHEMA = "g2"`).
> 23. **`ingest` streams its sources; GDAL scale/offset, rotated-grid and
>    codec refusals, full error causes (§1.2, §9a; changes `.abt` bytes
>    **only** for sources carrying a GDAL scale/offset; 2026-10-13).**
>    (a) *Memory and speed, bytes unchanged.* A tile used to decode every
>    source it touched WHOLE and hold them all at once (~580 swissALTI3D
>    0.5 m files under one 10 m tile ≈ 4.6 GB per thread — out of memory),
>    with a count-based cache and `total RAM / 1.5 GB` threads. It now
>    streams the output in row bands and decodes, per band, only the part
>    of each source the band's cell footprints reach (the band's own
>    transformed row geometry + 2 source px, clamped to the file), through
>    the TIFF's own strips/tiles and one byte-budgeted LRU chunk cache
>    shared by the batch. Per tile ≤ 128 MiB of band windows, per process
>    ≤ 192 MiB of cached chunks (a chunk over half the cache is held by its
>    tile only — a huge single-strip file still costs one strip per tile;
>    re-tile it, `gdal_translate -co TILED=YES`, for the bound). The batch
>    pool is sized from a per-tile estimate and **available** memory; the
>    `[Rust] Rayon Pool: <n> threads (System RAM: <t> GB` line gains
>    `, <a> GB available, ~<m> MB per tile)` (diagnostic, not frozen). The
>    sampling arithmetic is unchanged: verified byte-identical to the
>    previous converter on geographic + projected + priority-overlap +
>    nodata fixtures and on a 1600-file LV95 batch; a read outside a planned
>    window refuses the tile (`internal error converting …`) rather than
>    guess. Every listed source's **header** is still read for every tile
>    that lists it, so a missing, non-georeferenced, CRS-less, rotated or
>    undecodable-codec source still fails that tile; only damage inside
>    pixel data the tile never reads no longer does.
>    (b) *GDAL scale/offset (changes bytes).* The `GDAL_METADATA` tag
>    (42112) band-1 `SCALE`/`OFFSET` items are applied — metres =
>    `raw × scale + offset` — before the half-metre conversion; nodata is
>    matched on the **raw** value, as GDAL does. An Int16 decimetre DEM
>    (5000, scale 0.1) came out as 5000 m, now 500 m. Identity (1, 0) or
>    absent: unchanged bytes. An unparseable value is a hard error naming
>    the file (`GDAL_METADATA SCALE "…" is not a finite number`).
>    Consumers that cache file-built tiles of scaled sources must
>    invalidate them.
>    (c) *Codecs.* ZSTD (50000) now decodes in native builds and WebP
>    (50001) in all builds; LERC (34887), JPEG-XL (50002), LZMA (34925),
>    JPEG 2000, old-style JPEG, CCITT RLE/Fax3 — and ZSTD in the WASM build
>    — are refused **from the header, before any pixel**, naming file and
>    codec: `source "<path>": unsupported compression <NAME> (<code>) — this
>    build cannot decode it. …` (previously the decode aborted the tile with
>    the bare `compression method Unknown(34887) is unsupported`). Match
>    `unsupported compression` to route that one file through GDAL.
>    (d) *Rotated / sheared grids refused.* A `ModelTransformation` with
>    non-zero term [1] or [4] is a hard error naming the file:
>    `source "<path>": rotated/sheared geotransform is not supported — warp
>    it to a north-up grid first …` (it was sampled as north-up, silently
>    misplaced).
>    (e) *Error causes.* The batch line `[Error] Failed to process tile:
>    <message>` prints the whole cause chain, `: `-joined (outermost
>    first), e.g. `loading source "<p>": Opening "<p>": No such file or
>    directory (os error 2)` — it printed only the outermost context. The
>    prefix is unchanged.
> 24. **`download`: a failed tile filled from a coarser zoom is DEGRADED,
>    not OK (§1.2, §9b; changes the `[Stats] Tiles:` OK count; 2026-10-13).**
>    Item 16 counted every tile the parent backfill repaired as OK. That
>    still holds for a tile that answered **HTTP 404** (no tile at this zoom
>    — the coarser one is the data that exists). A tile whose fetch failed
>    for **any other reason** (timeout, connect, 429, 5xx, other 4xx,
>    decode, other) and was then filled from an ancestor is now
>    **degraded**: it is tallied in its error class (`[Stats] ERRORS`), it
>    is **not** counted in `<ok>` of `[Stats] Tiles: <ok>/<total> OK`, and
>    each one is named on stderr (frozen, parsed by the Waveshed plugin):
>    `[Degraded] z=<z> x=<x> y=<y> reason=<timeout|connect|http_<code>|decode|other> filled_from=z<k>`
>    (`k` = the coarsest zoom that supplied a pixel), the first 20000 of
>    them, then one `[Degraded] … <n> more not listed`; and, only when
>    n > 0, `[Stats] DEGRADED: <n> of <total> source tile(s) filled from
>    coarser zooms after a failed fetch`. **Consumer-visible:** a run with
>    degraded tiles no longer reports 100 % success — it used to print
>    `760/760 OK (100.0% success)` over 2 m near-field terrain that was
>    partly z14-or-coarser, and the plugin cached it as complete. Degraded
>    tiles are **excluded from the >50 % fatal guard** (their pixels exist,
>    coarse; a coarse patch beats a hole for this run), so exit codes and
>    `.abt` bytes are unchanged; the `Tile download failed:` message gains a
>    parenthetical naming them when they occurred. `[Stats] Repaired from
>    coarser parent tiles: N` still counts every repair, degraded included.
>    Same change: (a) the parent backfill requests all candidate ancestor
>    zooms **concurrently under one 30 s deadline** (pixels still taken
>    finest zoom first), so a tile's worst case is its retries plus one
>    deadline — 233 s, was 443 s at z14 (levels fetched one by one at 30 s
>    each); an ancestor that decoded but supplied no terrain no longer
>    counts as a repair (the tile was void either way; bytes unchanged).
>    (b) `[Stats] Retries:` states each path's real maximum: `(up to 5 per
>    failed tile)` for the CLI `download` (it printed `3`), `(up to 3 …)` for
>    the native in-memory downloader; the browser path, which counts
>    nothing, prints `[Stats] Retries: not counted (the browser retries up
>    to 3 per failed tile)`. (c) The disk-space guard now works on Windows:
>    the lookup canonicalized the path, whose `\\?\C:\…` verbatim prefix
>    never matched the disk's `C:\` mount point, so the guard silently never
>    fired there. It now matches the absolute path as written (nearest
>    existing ancestor, longest mount point) and, on Unix, also requires the
>    same device, else reports "unknown" and skips the check rather than
>    quoting another volume's free space. The in-memory/WASM downloaders
>    never repair a *failed* tile from parents, so they have no degraded
>    tiles (the browser path repairs **404s** since item 25 — those count OK,
>    as on the native paths).
> 25. **`download`: WebP source tiles decode; the browser path fills a 404
>    from the coarser zoom (§1.2, §9b; additive; 2026-10-16).** Motivated by
>    Mapterhorn (`https://tiles.mapterhorn.com/{z}/{x}/{y}.webp`): 512 px
>    lossless-WebP Terrarium tiles, z12 worldwide and z13–z17 only where a
>    country publishes finer open data. (a) `decode_png` became
>    `decode_tile`: the body is sniffed (`RIFF…WEBP` → WebP, else PNG) and
>    both arms feed one sample loop; the tile-geometry rules and messages are
>    unchanged. A WebP source used to fail every tile with `decode=N`. The
>    job JSON is unchanged — there is no format field. (b) In the browser
>    path (`download_terrain`, WASM), a source tile the service answered
>    **HTTP 404** is now filled from the nearest coarser zoom that has one
>    (z-1, z-2, … down to z6, one level at a time, each ancestor fetched
>    once through the page's `fetch_fn` so siblings share it), exactly the
>    pixel mapping of the native parent backfill (`upsample_from_ancestor`).
>    A filled 404 counts **OK** and in `Repaired from coarser parent tiles`,
>    as items 16/24 define for the native paths; a 404 no ancestor could
>    supply stays `HTTP_404_no_data`. Failures other than 404 are still not
>    repaired there (no degraded tiles in the browser path, as item 24
>    says). One informational stderr line is new:
>    `[DownloadWeb] <n> tile(s) answered 404 at z<z>; <k> filled from coarser zooms`.
>    (c) The browser fetch pool no longer retries a **4xx other than 429**
>    (the native paths never did): a 404 used to cost three retries and
>    ~3 s before it was accepted as the answer. `.abt` bytes change only for
>    jobs that had 404 tiles in the browser path (void → coarser terrain).
> 26. **`ingest` decodes tiled LZW sources with padded edge tiles (§9a;
>    2026-10-20).** A tiled LZW GeoTIFF whose width is not a multiple of the
>    tile width — every swissALTI3D 2 m file (500 px, 128² tiles) — failed
>    each output tile with `loading source "…": no lzw end code found`. Cause:
>    the `tiff` 0.11 row-by-row read of a padded tile skips the padding with a
>    small read, which weezl 0.1 can answer with a false `NoProgress`
>    (image-rs/image-tiff#395; fixed upstream only by the unreleased weezl 0.2
>    upgrade). `ingest` now reads every strip/tile whole and crops the padding
>    itself. Files that decoded before give byte-identical `.abt` output; the
>    job JSON and messages are unchanged.
> 27. **CLI `download` transport (§1.2, §9b; 2026-10-20).** Five changes,
>    none to the job JSON or the `.abt` bytes:
>    (a) **Retries are decided by error type.** The old rule substring-matched
>    the error message for decode words, and reqwest quotes the URL in every
>    transport error — so for any `….png` source (AWS Terrarium) every
>    connection reset and pre-header timeout was treated as permanent, never
>    retried, and became a `[Degraded]` tile (which the Waveshed plugin then
>    re-downloads as a whole output tile). They are now retried. Tally side
>    effect: a truncated body (reqwest body error that is not a timeout) is
>    classed `connect`, no longer `decode`.
>    (b) **Timeouts:** 10 s connect + 15 s read-idle instead of a 30 s total.
>    Measured on a 7 Mbit/s link at 256 connections: the old client ran 809 s,
>    timed out 4925 attempts, discarded 248 MB and failed the run; now 148 s,
>    no error. `Retry-After` is honoured (per tile and as a gate on new
>    requests).
>    (c) **Strips are assembled in completion order** (a straggler no longer
>    idles the strips behind it) and the prefetch depth is re-sized from the
>    real source tile edge once the first tile decodes (it assumed 512 px
>    throughout); additive line `[Download] source tiles are <e> px: prefetch
>    depth <d>`. Output is byte-identical.
>    (d) **`[Download] SLOW tile` is replaced by `[Download] STALL tile`** and
>    a `[Stats] STALL:` summary (§1.2). SLOW was undocumented and had no
>    parsing consumer.
>    (e) **Adaptive connection window** (AIMD, ceiling and start =
>    `max_connections`, default still 256; §9b) and the additive `[Stats]
>    Connection window:` line. The native in-memory downloader gets (a), the
>    STALL lines and `Retry-After`, not the window; the browser path is
>    unchanged.
> 28. **CLI `download`: shared ancestor fetches, a per-tile deadline, and
>    repair jobs (§1.2, §1.5, §9b; additive; `aether_converter` 0.2.6;
>    2026-10-20).** (a) **Each ancestor tile is fetched at most once per
>    run.** The parent fill (404 repair and degraded fill alike) used to let
>    every failed tile fetch all of its ancestors itself — an all-404 z15 run
>    of 13,700 tiles made 123,300 ancestor requests. A per-run cache now
>    starts each `(z, x, y)` once; concurrent and later requesters share the
>    result, failed ancestors are remembered as failed, decoded ones are kept
>    up to 256 MB (oldest dropped first, refetched if needed again). Each
>    ancestor request is capped at 30 s; a tile still waits at most 30 s for
>    its ancestors. Pixel mapping unchanged: `.abt` bytes are identical for
>    the same tile answers. (b) **Per-tile deadline:** 60 s per source tile
>    across all attempts, back-offs and `Retry-After` waits (optional job
>    field `tile_deadline_s`). At expiry the tile is a `timeout` failure
>    (also a congestion event for the adaptive window) and goes to the
>    parent fill. A silent server used to hold a tile 143 s, a trickle with
>    gaps under 15 s without limit; now 90 s at most including the fill.
>    Tally side effect: a tile whose retries could not fit in the deadline is
>    classed `timeout`, not by its last attempt's error. (c) **Repair jobs:**
>    optional `repair_source_tiles: [[z, x, y], …]` fetches only those tiles
>    and patches them into the job's existing `.abt` files, leaving every
>    other byte unchanged (§9b *Repair jobs*). The `aether_converter` version
>    is 0.2.6 so consumers can gate on it (§1.5) — older builds ignore the
>    field and would re-download the whole job over the files.
> 29. **CLI `download` progress cadence (§1.2; 2026-10-20).** The
>    `[Download] <pct>% (<done>/<total>) — …` line is printed on every whole
>    percent instead of every 10 %, and still once at `done == total`. Its
>    format is unchanged; a parser that accepts any `<pct>` (the Waveshed
>    plugin's does) sees more updates and nothing else. Job JSON, `.abt`
>    bytes and every other line are unchanged.
> 30. **CLI `download`: fetch only what the outputs sample, RAM-budgeted
>    strips, a fixed connection pool, a 20 s tile deadline (§1.2, §9b;
>    `aether_converter` 0.2.7; 2026-10-21).** No job-JSON change; `.abt`
>    bytes are identical for every tile a run writes (tested against the
>    whole-rectangle fetch and across 32-, 3- and 1-row strips). Motivation:
>    a 2 169-output-tile z15 pass (~265 k source tiles, 250 Mbit/s, 16 CPUs,
>    ~4 GB free) collapsed to a crawl. (a) **Fetch set.** Only source tiles
>    that touch at least one requested output tile are fetched; the bounding
>    rectangle still fixes the assembly grid. The 4th number of
>    `[Download] z=<z> tiles=<nx>x<ny>=<n>` — the plugin's pass total — is
>    now the tiles fetched (no longer always `nx × ny`); progress
>    `<total>`, `[Stats] Tiles: <ok>/<total>` and the majority rule count
>    the same tiles; new informational line `[Download] fetching <n> of the
>    <nx·ny> source tiles …` when fewer. A consumer that skips output tiles
>    it already has now also skips their download. (b) **Memory.** A strip
>    used to be 32 tile-rows × the full width, decoded to f32 on arrival,
>    with up to ~3 strips held (≈ 16 GB on that pass) and RAM read once.
>    Now, before every strip launch: budget = min(available / 4, 2 GiB),
>    ≥ 256 MiB; strip rows = clamp(budget / 2 / (nx × edge² × 4 B), 1, 32);
>    launches are gated on the bytes of all launched, unassembled strips.
>    `[Download] RAM: … prefetch depth: …` becomes `[Download] RAM budget:
>    <mb> MB, strip rows: <r> (…)` (informational). (c) **Connections.**
>    The AIMD window (27e) is replaced by `max_connections` concurrent
>    requests (as before 27e), cut 25 %/s only by ≥ 3 HTTP 429/503 answers
>    in a second (floor max(max/4, 16)), +10 % per clean second; timeouts,
>    resets and slow bodies never move it; a cut gates new requests only.
>    A tile holds its slot through retries and back-offs and releases it
>    before the parent fill. `[Stats] Connection window:` keeps its format;
>    the progress line gains a trailing `, window <w>` (prefix and earlier
>    fields unchanged). (d) **`tile_deadline_s` default 20 s** (was 60 s):
>    a tile still failing then is parent-filled (`[Degraded] …
>    reason=timeout`) for the consumer's repair job to refetch; worst case
>    per tile 50 s.
> 31. **`ingest` diagnostic timing lines (§1.2; additive, NOT parsed;
>    `aether_converter` 0.2.7; 2026-10-21).** Every tile prints on stdout
>    `[Rust] tile <name>: <n> sources, header <s>s (open <ms> ms, read <ms>
>    ms avg), decode <s>s (<files> files, <MB> MB read), sample <s>s, bands
>    <n>`, and an `ingest` process prints at exit, after the batch result
>    (also on failure), `[Rust] run: wall <s>s, cpu user <s>s sys <s>s,
>    page faults <n>, threads <n>` (`threads` = the batch's worker-pool
>    size, omitted for a single job). They locate where a slow batch spends its
>    time (header opens, decode I/O, sampling, memory pressure); their
>    wording may change and no consumer may parse them. Exit codes, `.abt`
>    bytes and every parsed line are unchanged.
> 32. **`probe` subcommand (§1.2a, additive; `aether_converter` 0.2.7;
>    2026-10-21).** `aether_converter probe --list <file>` (or `--stdin`;
>    `--jobs N`, default `min(64, 4 × CPUs)`) prints one JSON line per listed
>    file, in list order (schema `aether-probe/1`,
>    `schemas/probe_output.schema.json`): `size`, `mtime`/`mtime_ns` (Python
>    `os.stat`-exact), `width`, `height`, `bands`, `dtype`, `compression`
>    `{code, name}`, `predictor`, `tiled`/`tile_w`/`tile_h`, `epsg` (or the
>    reserved `proj`), `geotransform` (GDAL order), `rotated`,
>    `pixel_is_point`, `nodata`, `scale`/`offset`, `overviews`,
>    `ifd_offset` — raw header facts from a few positioned reads, no pixels,
>    no reprojection; each equals GDAL's value (`tools/probe_vs_gdal.py`).
>    Files it cannot vouch for (not TIFF, multi-band, sub-byte, palette,
>    user-defined CRS, GCPs, …) are `ok: false` with a `reason`, for the
>    caller to send to GDAL. Final stderr line
>    `[Probe] <n> files, <ok> ok, <s>s, open <ms> ms avg, read <ms> ms avg`.
>    Nothing else changes.
> 33. **`ingest` batch prints a `[Tile]` line per tile (§1.2; additive,
>    parsed; `aether_converter` 0.2.7; 2026-10-21).** After each written
>    tile, stdout gets `[Tile] <output file name> covered=<covered_px>/<total_px>`
>    (coverage counted before `void_fill_m`), flushed with the
>    `[Rust] Progress:` line. The Waveshed plugin parses it; with an older
>    converter (no `[Tile]` lines) a consumer must treat per-tile coverage as
>    unknown. Single-job ingest, exit codes and `.abt` bytes are unchanged.
> 34. **`ingest` replaces tiles atomically (§1.2, §6; `aether_converter`
>    0.2.7; 2026-10-21).** A tile is written to `<output_path>.tmp-<pid>`
>    beside the output, flushed and `sync_data`'d, then renamed over
>    `output_path`; on any error the temp file is removed and the tile
>    fails. Before, the output was truncated and rewritten in place, and the
>    final buffered flush ran in a destructor that discarded its error — a
>    full disk could leave a **truncated tile with exit 0**, and rewriting an
>    existing tile changed the bytes under every hardlink to it. Now a
>    reader sees the old complete tile or the new complete one, and a
>    replaced tile is a new inode. `.abt` bytes are identical (verified on
>    24 swissALTI3D 2 m tiles of 5556² px against the previous converter).
>    A killed converter can leave `*.abt.tmp-<pid>` files; consumers that
>    scan output directories should ignore them. Since 0.2.8 the final
>    rename waits out a transient refusal (Windows "access denied" /
>    sharing violation while a scanner or indexer still holds the file: up
>    to ten attempts 100 ms apart, then the tile fails as before), and a
>    volume that cannot sync (`sync_data` answers ENOTSUP / EINVAL — some
>    network and exFAT mounts) no longer fails the tile; the rename still
>    replaces the output atomically.
> 35. **`probe` reports where the pixel data ends (§1.2a; additive;
>    `aether_converter` 0.2.7; 2026-10-21).** Every `ok: true` line gains
>    `last_chunk_end` (int | null): offset + byte count of the image's LAST
>    strip/tile — the bottom-right block GDAL's truncation test reads — from
>    one positioned read of that entry of TileOffsets/TileByteCounts (else
>    StripOffsets/StripByteCounts) of the first IFD; `0` when that block is
>    sparse (byte count 0); `null` when those arrays cannot be read. A value
>    greater than `size` means the file is cut off behind an intact header
>    (before, such a COMPRESSED file was indistinguishable from a whole one
>    and only `ingest` failed on it). Equal to GDAL's `BLOCK_OFFSET_x_y` +
>    `BLOCK_SIZE_x_y` of that block (`tools/probe_vs_gdal.py`). Lines of a
>    converter built before this item lack the field: a consumer must then
>    not take the pixel data as complete. Nothing else changes.
> 36. **CLI `download`: one task per source tile, strips sized from the real
>    tile edge, longer keep-alive, two concurrent assemblies (§1.2, §9b;
>    `aether_converter` 0.2.8; 2026-10-21).** No job-JSON change; `.abt` bytes
>    are identical (`cmp` of all six outputs of a z15 AWS Terrarium pass —
>    4 690 source tiles, 389 MB, six 10 m tiles — between 0.2.7 and this
>    build; tests against 1-row strips). Motivation: at 256 connections a run
>    plateaued at 190–270 tiles/s (Windows: 230 tiles/s, 14–15 MB/s) with ONE
>    tokio worker thread at ~100 % CPU — a strip's tiles were futures of one
>    task, so their TLS reads, body copies and decodes ran on one thread at a
>    time — while a warm S3 request takes 237 ms and 1 024 connections drew no
>    429/503. (a) **One task per source tile.** A tile's whole life —
>    connection slot, attempts, back-offs, deadline, decode, parent fill,
>    tallies, progress line — is its own tokio task, spread over all worker
>    threads; per strip at most `max_connections` of them are alive, as
>    before. The connection window and slot rules (§1.2), the `[Download]
>    <pct>%` cadence and format, the `[Degraded]`/`[Stats]` lines and the
>    failure rules are unchanged, and progress lines still reach stderr in
>    count order. Measured on 4 CPUs, five runs each, against S3 (the network
>    there is capped at ~21 MB/s, so the rate is the link's: 186–253 tiles/s,
>    before 198–253; 230 both at equal background load): busiest thread per
>    0.5 s sample 50–60 % max, 30–48 % median (before 81–105 % / 49–54 %);
>    against a loopback server, where the CPUs are the limit,
>    968–1 444 tiles/s (before 762, three threads at 106–110 %). Decoding
>    stays on the tile's thread (1.6 ms mean per 256 px tile):
>    `spawn_blocking` ran 19–24 % fewer tiles/s there. (b) **Strips sized from
>    the real tile edge.** The first tile of the first strip is fetched alone
>    before any strip is sized, through the normal path (counted in progress
>    and `[Stats]` like any tile; its result goes to its strip, it is never
>    fetched twice), and the strip rows come from its edge. Strips used to be
>    sized for 512 px tiles until a tile had decoded: a wide 256 px pass
>    opened with `strip rows: 1` ("367 strips"), and at ≥ 512 connections its
>    first two strips reserved 2 × 1 GB of the 2 GiB budget and the pipeline
>    idled 2–4 s. Now `[Download] RAM budget: … strip rows: <r> (…;
>    re-evaluated before every strip)` and `[Download] <s> strips, …` are
>    printed once, after that tile, with the real edge (the pass above: `strip
>    rows: 32`, 3 strips; before 14 and 5). The first strip starts one request
>    later: the RAM-budget line came 0.69–0.99 s after the `z=` header
>    (before: 0.05–0.09 s), cold connections at a 146 ms RTT. When that tile
>    fails, or has not arrived after 5 s, the first strip is sized for 512 px
>    as before and takes the tile over when it lands; later strips are
>    re-sized as before (`[Download] RAM budget: … (<a> GB available)`). In a
>    pass of ≤ 100 tiles that tile's progress line may now precede the
>    RAM-budget lines; the `[Download] z=…` header still precedes every
>    request. (c) **Keep-alive 30 s** (was 5 s): an idle gap of a few seconds
>    no longer closes the warm sockets (~700 reopened per gap at ≥ 512
>    connections). (d) **Two strips assemble at once** on max(CPUs / 2, 2)
>    threads (was one at a time on max(CPUs / 4, 2): ~450 tiles/s on 16 CPUs).
>    The RAM budget still holds them — a strip's tiles count against it until
>    its assembly ends — plus one tile-row of grid (`nx × edge² × 4 B`) per
>    assembly; the plan line reads `… assembly <t>/<n> threads (max 2
>    concurrent)`. (e) **`[Stats] Tile latency:` gains a trailing ` first-byte
>    avg=<ms>ms`**: the mean, over every request attempt, of the wait from
>    sending the request to the response head (connection setup, TLS, server
>    time; an attempt that failed first counts until it failed), beside `avg`,
>    the whole tile (body, retries, parent fill): a slow server or setup shows
>    in the first, a slow link in the gap (measured: 464–615 ms of
>    852–1 162 ms). CLI `download` and repair jobs only; the in-memory and
>    browser paths print the line as before. A parser of the text after `Tile
>    latency:` must allow the suffix. (f) **Per-strip lines are diagnostics:**
>    `[Strip i/n] …` (assembly timing, `… FAILED`) and `[StripMem i/n] …` go
>    to `AETHER_DIAG_FILE` when it is set (item 37), else to stderr as before.
>    (g) **Open-file limit (unix).** A run holds up to a socket per connection
>    in use, as many again idle in its pool, and resolver sockets — measured
>    peaks of 263 to 548 descriptors at 256 connections, over macOS's default
>    soft limit of 256, where EMFILE fails a connect or the `open()` of an
>    output tile (which fails the run). When the soft limit is below `2 ×
>    max_connections + 96`, the converter raises it towards the hard limit
>    (macOS: at most `kern.maxfilesperproc`), best effort, never fatal; if
>    even that cannot hold them, it runs `(limit − 96) / 2` connections (at
>    least 1) and says so before the `[Download] z=` header, whose
>    `connections=` and the `[Stats] Connection window: start` then carry that
>    number: `[Download] open-file limit <l>: <c> connections instead of
>    max_connections <m> (<d> descriptors would not fit)` (informational; one
>    line). Windows has no such limit and is unchanged. Unchanged: the job
>    JSON (the schema's `max_connections` description gains (g)), `.abt`
>    bytes, exit codes, every parsed line (`[Download] z=…`, `[Download]
>    <pct>%`, `[Degraded]`, `[Stats] Tiles:`, `NO-DATA`, `DEGRADED:`,
>    `Connection window:`), retries, deadlines, the parent fill, and repair
>    jobs (which get (a), (c), (e) and (g)).
> 37. **`AETHER_DIAG_FILE`: diagnostic lines to a file; batch `ingest` keeps
>    file I/O off its compute threads (§1.2; additive; `aether_converter`
>    0.2.8; 2026-10-21).** No job-JSON change; `.abt` bytes, exit codes and
>    every parsed line are unchanged (`cmp` of all 25 outputs against 0.2.7: a
>    15-tile 2 m batch over a synthetic swissALTI3D-like set — 600 LZW Float32
>    LV95 files, partial files, `-9999` borders, partial tiles, `void_fill_m`,
>    a WGS84 base behind — a 10 m batch, a 90 m single job over 601 sources,
>    the repository's LZW/Deflate test rasters with a `nodata` override, and
>    `.abt` sources). Motivation: a 920-tile 2 m batch on 16 threads (1 028 s)
>    spent 10.4 s of each tile's 17.9 s of thread time blocked in opens,
>    reads, writes and syncs on the compute threads, and the Waveshed log
>    panel mirrored its 920 diagnostic lines. (a) **`AETHER_DIAG_FILE`.**
>    When this environment variable names a path, `ingest`'s per-tile
>    `[Rust] tile …` lines and `download`'s `[Strip …]` / `[StripMem …]`
>    lines (item 36) are appended to that file — created if missing, never
>    truncated, each line flushed — instead of stdout / stderr; unset or
>    empty, they stay on the console. A file that cannot be opened costs one
>    stderr line, `[Warn] AETHER_DIAG_FILE = "<path>" cannot be opened
>    (<cause>); diagnostics go to the console`, and never fails a run.
>    `[Rust] run:` and every parsed line (`[Tile]`, `[Rust] Progress:`,
>    `[Download] …`, `[Degraded]`, `[Stats] …`, `[Warn] …`) stay on their
>    streams. (b) **`[Rust] tile` gains `, geom <s>s, write <s>s, total
>    <s>s`** (row geometry and band planning; the wall time of writing,
>    syncing and renaming the tile file; the tile's wall time on its compute
>    thread, which in a batch ends at the hand-off to a writer thread — the
>    write overlaps the next tile). The line now appears once the tile is
>    written (or has failed after sampling, with `write 0.0s`), before its
>    `[Tile]` line; `decode` is the window decode plus any wait for a read
>    still in flight, `MB read` includes reads issued ahead. (c) **`[Rust]
>    run:` gains `, blocked ~<s>s`** = wall × threads − (user + sys), floored
>    at 0 — thread time not on a CPU (a single job counts rayon's global
>    pool). (d) **I/O off the compute threads.** A batch hands each sampled
>    tile to one of max(1, min(4, threads / 2)) writer threads that create,
>    write, flush, `sync_data`, close and rename it (item 34 unchanged: a
>    failed write removes its temp file and fails that tile, `[Error] Failed
>    to process tile:` as before); a compute thread waits only when every
>    writer is busy, so a batch holds at most one finished tile per writer
>    beyond the one each compute thread works on. Source opens, header reads
>    and chunk reads run on I/O threads (2 × CPUs, 8..32); a band's reads are
>    issued while the band before it samples (≤ 32 MB per tile ahead, the
>    rest when its window decodes). Source files stay open between reads —
>    at most a quarter of the soft `RLIMIT_NOFILE`, 16..256, on Unix; 512 on
>    Windows — so a file is opened once per run while it stays cached, not
>    once per band; they are opened with the platform defaults (Windows:
>    share read/write/delete), so a source can still be replaced or deleted
>    during a run. `[Tile]` and `[Rust] Progress:` are printed once a tile's
>    file is committed, as before; the progress count is now taken under the
>    stdout lock, so no tile's progress line can overtake another tile's
>    `[Tile]` line. (e) **Less allocation, less CPU.** Written tiles' output
>    buffers, window buffers and read buffers are reused (no zero fill where
>    every sample is overwritten); the Windows binary allocates through
>    mimalloc (Linux and macOS keep the system allocator). Row geometry
>    reuses a transform wherever the next row repeats it with bit-identical
>    inputs, and the sampler skips, per 64-px block, sources whose rectangle
>    the block's cell centres provably miss, with a fast path for blocks
>    inside one file — the same floating-point operations in the same order.
>    Measured on 4 CPUs (medians of three interleaved runs). Sources and
>    outputs on a 9p mount, the 2 m batch (755 source listings): wall 12.7 →
>    9.6 s, CPU 26.4 → 21.1 s, page faults 294 k → 195 k; opens 1 606 → 851;
>    the compute threads' openat / pread64 / write / fdatasync / rename
>    1 596 / 1 913 / 932 / 15 / 15 → none (strace); per tile, header 0.13–0.21
>    → 0.03–0.05 s, decode 0.79–0.81 → 0.25–0.31 s, geom 0.31–0.35 s, write
>    1.0–1.1 s on the writer threads, whose throughput on that mount (~100
>    MB/s) now bounds the run. In RAM, with 0.2.7's tiff decoder: CPU 27.2 →
>    20.1 s, wall 8.2 → 5.3 s, utilization 83 → 95 % (the row-geometry and
>    sampler steps each took 18–19 % off the CPU in step-by-step runs). Peak
>    RSS of that batch 0.47–0.49 → 0.71–0.75 GB (writer-held tiles, kept
>    buffers, reads ahead).
> 38. **CLI `download`: raw source-tile cache, `cache_dir` job field (§1.2, §9b; additive; `aether_converter` 0.2.8; 2026-10-21).**
>    New optional job field `cache_dir` (string | null; absent, `null` or
>    `""` → no cache, the run is exactly as before). Set, it names a
>    directory that keeps what tile services answered — each HTTP 200 body as
>    it arrived (PNG/WebP bytes, before decode) and each HTTP 404 — shared by
>    every job, service and zoom that names it and by converter processes
>    running at once: the CLI `download` looks every source tile of the fetch
>    set up before any request, decodes a hit from the cached bytes without a
>    connection slot or a request, and writes every answer it fetches (the
>    parent fill's coarser tiles too), so a later job fetches only what the
>    cache lacks. Never cached: a failed, timed-out or undecodable tile, nor
>    what the parent fill makes of a tile (a `[Degraded]` tile is fetched
>    again by the next job); a 404 is remembered until the directory is
>    deleted. Repair jobs (changelog 28) refetch their listed tiles and
>    replace their cached answers. **Layout and format** (§9b *Raw source-tile
>    cache*): `<cache_dir>/<service_key>/z<zoom>/x<x>_y<y div 256>.pack` and
>    `<service_key>/service.json`, `service_key` = the first 16 hex digits of
>    SHA-256(`url_template`, query string included, + "\n" + lower-cased
>    `encoding`) — templates that differ only in their query (`?layer=a` /
>    `?layer=b`, or two API keys) never share a directory — while
>    `service.json` records the template without its query string, so a key
>    in the query never reaches the disk in clear text (one placed in the path
>    does); a pack holds one
>    tile column's 256 rows: magic `AETP`, u16 version 1, u32 zoom, u32 x, u32
>    yblock, 256 index entries of 20 bytes (u64 offset, u32 length, u8 status
>    0 unknown / 1 present / 2 HTTP 404, 3 reserved, u32 CRC-32), then the
>    bodies, appended; little-endian. A writer locks the pack (`flock` on
>    unix incl. macOS, an exclusive-write open on Windows), appends, then
>    rewrites the entry; readers take no lock and validate every entry
>    (header, bounds, CRC), so a torn write, a damaged entry or a body that no
>    longer decodes is a miss — refetched and rewritten (appended; the old
>    bytes stay behind, nothing compacts) — never a wrong tile. A cache that
>    cannot be created or used costs one `[Warn] cache_dir <dir> cannot be
>    used (<reason>); downloading without cache` and the run goes on; a pack
>    another writer holds for 10 s is not written by this run (one `[Warn]
>    cache_dir …` line per run). **Consumers see** informational lines, not
>    parsed: `[Download] cache: <hits> of <total> source tiles from cache`
>    once, after the pass header and before the first request (`<hits>` =
>    entries found; one whose bytes then fail their CRC or decode is fetched
>    after all), and, last in the `[Stats]` block, `[Stats] Cache: <hits>
>    hits, <fetched> fetched, <written> written to <cache_dir>` (`<hits> +
>    <fetched>` = `<total>`), followed, when the parent fill asked for coarser
>    tiles, by `[Stats] Cache, coarser parents: <h> hits, <f> fetched, <w>
>    written`. **Unchanged:** `.abt` bytes (tested: cached against fetched
>    against cache-less runs, with 404s, parent fills, corrupt entries, a
>    repair, two concurrent processes and 1-row strips; measured on S3 with
>    the z15 pass of item 36 — 4 690 source tiles, six 10 m tiles, 256
>    connections, 4 CPUs: run 1 fetched 388.6 MB in 20.3 s and wrote 4 690
>    entries, 407 881 368 bytes in 70 packs; run 2, same job into a fresh
>    output directory, made no request — no bytes over the network — took
>    6.3 s and is `cmp`-identical on all six tiles), every parsed line — a hit
>    counts in `[Download] <pct>%`, `[Stats] Tiles:`, `NO-DATA`, `Repaired`,
>    `DEGRADED:` exactly like the fetched tile it stands for — exit codes,
>    retries, deadlines, and the in-memory and browser downloaders (they
>    ignore the field). `[Stats] Downloaded:` and the progress line's MB/s
>    count network bytes only, and a run that made no request at all prints
>    `[Stats] Tile latency:` without its `first-byte avg=` suffix. With a
>    cache, a strip hands its tiles out column by column (a pack is a column;
>    order never changes a byte). Older converters ignore `cache_dir`: gate
>    on `aether_converter` 0.2.8 (§1.5).
> 39. **CLI `download`: the RAM budget is fixed per run, tile memory is
>    released per tile-row, outputs are opened once (§1.2, §9b;
>    `aether_converter` 0.2.9; 2026-10-23).** No job-JSON change; `.abt`
>    bytes are identical (the byte-identity tests of items 30, 36 and 38 —
>    32-, 3- and 1-row strips, the whole-rectangle fetch, cached runs, a
>    lost or slow first tile — pass unchanged; the new row-block writer is
>    tested byte for byte against the per-row writer it replaces, and
>    outputs held open against outputs reopened for each write). Motivation:
>    a z15 pass on Windows downloaded in pulses — "available" RAM sank with
>    the run's own unflushed output writes, the budget re-read before every
>    strip sank with it, strips shrank to one tile-row of 611 tiles, and with
>    two strips assembling only one could download, so the 256 connections
>    drained to zero between strips. (a) **Budget fixed per run.** The
>    budget (min(available / 4, 2 GiB), ≥ 256 MiB, as before) is read once,
>    after the run's first tile (item 36b), and never again; the line reads
>    `[Download] RAM budget: <mb> MB, strip rows: <r> (<a> GB available at
>    start; fixed for the run)` (was `(<a> GB available; re-evaluated before
>    every strip)`). The rows can still change once, when the tile edge is
>    learnt only after the first strip was sized for 512 px: `[Download] RAM
>    budget: <mb> MB, strip rows: <r> (re-sized for <e> px source tiles)`
>    (was `(<a> GB available)`, on every change of the rows). Both keep the
>    `[Download] RAM budget: ` prefix; they are informational. (b)
>    **Per-row release.** An assembly owns each tile-row's decoded tiles
>    apart and frees them as soon as it has copied that row into its grid,
>    before the row is sampled and written. The row's share of the strip's
>    bytes — the bytes reckoned at launch (with the edge known then),
>    apportioned by tile count, the last row taking the rounding remainder —
>    leaves the launch gate's count at that moment; whatever is left when an
>    assembly ends (a failure part-way) leaves then, so exactly what a launch
>    added comes back (tested: the count is 0 at the end of every run). The
>    next strip therefore launches as soon as enough rows have drained, not
>    once a whole strip is assembled. The launch rules are otherwise
>    unchanged (≤ 5 strips ahead, 2 assembling at once, one tile-row of grid
>    per assembly on top of the budget — 152.75 MiB for 611 tiles of
>    256 px). (c) **Outputs opened once.** The output files are created with
>    their headers before the first request, as before, and now stay open
>    for the run: each (output, tile-row) block — every row's samples and
>    the stride's zero padding, the same bytes as before — is one positional
>    write (`pwrite` on unix, `WriteFile` at an offset on Windows) through
>    that handle, so two assemblies writing rows of one file share no cursor
>    and take no lock. Before, every block opened, seeked, wrote and closed
>    its file. The tail fill uses the same handles, which close before the
>    buildings post-pass. On unix the open-file raise (item 36g) asks for one
>    descriptor per output on top of `2 × max_connections + 96`; where the
>    outputs do not fit beside the connections, the connections keep their
>    descriptors (never fewer connections than 0.2.8 runs) and each write
>    opens its file as before, said once: `[Download] open-file limit <l>:
>    <n> output tiles are opened for each write instead of held open (<d>
>    descriptors would not fit)` (informational). (d) **New diagnostic,**
>    once per run after the last assembly: `[Download] launch gate closed
>    for <s> s (assembly behind the network)` — the time the run waited with
>    no strip downloading because the next one did not fit the budget. It
>    goes to `AETHER_DIAG_FILE` when that is set (item 37), else to stderr;
>    no consumer may parse it. Unchanged: every parsed line, exit codes,
>    retries, deadlines, the parent fill, the raw source-tile cache, repair
>    jobs (they patch one file at a time and have no strips) and the
>    in-memory and browser downloaders. Nothing to gate on: a consumer can
>    tell this build by `--version` (0.2.9). Throughput on the Windows pass
>    that motivated it is not yet measured.
> 40. **CLI `download`: the raw source-tile cache derives a tile from the
>    finer tiles it holds (§9b; additive; `aether_converter` 0.2.9;
>    2026-10-23).** No job-JSON change. A fetch-set tile without a valid
>    cache entry is no longer fetched when the cache holds the ground it
>    covers at a finer zoom: it is **derived** from its four children one
>    zoom finer, `(z+1, 2x+dx, 2y+dy)`, each of which must be a cached body,
>    a cached 404, or itself derivable the same way — **four zooms deep at
>    most** (tiles at `z+4` must be cached as they are; a z11 tile from z15
>    reads 4 + 16 + 64 + 256 child entries). So after a 2 m/5 m run cached
>    z15, a 10 m job (z14) over the same ground requests only the z14 tiles
>    whose four z15 children are not all cached (at the edges of the ground
>    the z15 run covered), and 30 m (z12) and 90 m (z11) jobs likewise derive
>    where the fine cache reaches and fetch only where it does not. A
>    partly covered tile is fetched whole — a missing child is never
>    replaced by a void. Output pixel `(i, j)` is the mean of the 2 × 2
>    samples `(2i + {0,1}, 2j + {0,1})` of the matching child quadrant,
>    voids — samples at or below −11000 m, and a 404 child's whole quadrant —
>    left out, void where all four are; four 404 children make a 404. The
>    tile is encoded back in the service's own encoding as an 8-bit RGB PNG
>    (Terrarium: `R·65536 + G·256 + B = round((v + 32768)·256)`, ties up,
>    void = `0,0,0`; Mapbox: the triple whose **decoded** value is nearest
>    `v`, so every value the decoder produces round-trips exactly — all 2²⁴
>    triples checked for both encodings) and stored with the new index
>    entry **status 3 = derived body** (validated and read like status 1),
>    and so is every derived intermediate: a cascade costs four decodes per
>    level once the level below is stored. A derived 404 is not stored
>    (re-deriving it reads index entries only). Mapbox has no void value
>    (its lowest, −10000 m, is terrain), so a Mapbox tile that would hold a
>    void — a 404 child beside children with data — is not derived but
>    fetched. Nothing is ever requested for a child. Whether a tile can be
>    derived is judged before the first request from index entries alone (no
>    body read); a derivation that then fails (a child body that fails its
>    CRC or decode, children of different sizes) fetches the tile as before,
>    and a tile whose own entry fails its CRC or decode is derived when it
>    can be. A derived tile is a hit in every respect: no connection slot,
>    no request, the same `[Download] <pct>%`, `[Stats] Tiles:`,
>    `NO-DATA (HTTP 404):`, `Repaired …` and `DEGRADED:` counting, and a
>    derived 404 goes to the parent fill exactly like a cached one. The
>    parent fill itself never derives (a derived ancestor of a hole holds
>    that hole) but reads status-3 entries like any other; repair jobs never
>    derive their listed tiles. **Not bit-identical to the service:** a
>    derived tile is our 2 × 2 average of the service's finer tiles, not the
>    service's own coarser tile (a service builds its pyramid its own way —
>    other resampling, other sources per zoom), so `.abt` bytes of a job
>    that derives can differ from those of a run that fetched the coarse
>    tiles; they are byte-identical to a run served coarse tiles that hold
>    those very means (tested, one and two zooms deep, with a partly covered
>    area and a derived 404). Pack version stays 1: a converter before 0.2.9
>    reads status 3 as unknown — a miss — and refetches the tile, rewriting
>    the entry as status 1; harmless. **Line changes** (informational, not
>    parsed): `[Download] cache: <n> of <total> source tiles from cache
>    (<derived> derived)` — `<n>` counts the derivable tiles too — and
>    `[Stats] Cache: <hits> hits (<derived> derived), <fetched> fetched,
>    <written> written to <cache_dir>` — `<hits>` counts the derived tiles
>    too, so `<hits> + <fetched>` is still the pass total, and `<written>`
>    counts derived tiles and intermediates; both always carry the
>    parenthesis, `(0 derived)` included. Unchanged: every parsed line, exit
>    codes, `.abt` bytes of every tile that is not derived, the parent
>    fill, repair jobs, the in-memory and browser downloaders. Measured:
>    deriving one 256 px tile from four cached children takes 8–9 ms on one
>    core (release build, synthetic 157 KB children: CRC checks 1.4 ms, 4
>    decodes 3.5–4.0 ms, the means 1.2 ms, the PNG encode 0.9 ms; the
>    derived PNG is ~8 % larger than the default-compressed children), one
>    four zooms deep (256 decodes, 85 encodes) ~0.38 s; a derived tile is
>    worked out on the tile's worker thread like a hit's decode. Nothing to
>    gate on: a consumer can tell this build by `--version` (0.2.9).
>    Since 0.2.12 (changelog 48f) a derived tile is never written over a
>    real entry — a body or a 404 another process stored after this one
>    read its index.
> 41. **Vector-tile buildings: the duplicate filter keys on the whole
>    footprint (§9a, §9b; `aether_converter` 0.2.9; 2026-10-23).** No
>    job-JSON change. Merging vector tiles drops the buildings that arrive
>    twice through neighbouring tiles' buffer zones — in `buildings_pbf_dir`
>    (`ingest`, the `download` post-pass and repairs) and in the WASM
>    building, surface and GeoJSON functions. The filter kept one building
>    per **first vertex** (rounded to 1e-6°), so it also deleted every
>    distinct building whose ring merely starts at a corner it shares with
>    an earlier one: measured on live OpenFreeMap tiles, −7.3 % of the
>    building footprint in Bern's old town and −4.1 % at Zürich HB; on the
>    z14 tile 8651/5782 (Zernez) in this repository, 5 of 470 buildings within
>    the one tile. A building is now dropped only when **every vertex of every
>    ring** (exterior, then its holes — item 42 — in order, each rounded to
>    1e-6°) **and its height** equal an earlier one's. A building two tiles
>    clip differently is no duplicate under that rule and is drawn once per
>    copy, which the `max` write rule makes harmless (every roof is resolved
>    against the bare terrain before any is written; nothing stacks).
>    **`.abt` bytes change** wherever the old filter had dropped a building:
>    more buildings are drawn, more pixels raised; elsewhere byte-identical.
>    The informational counts move with it (`[Info] buildings_pbf_dir: … (<n>
>    after dedup)`, `[Buildings] … (<n> dup dropped)`, the WASM console's
>    `after dedup`); none is parsed. FlatGeobuf (`buildings_file`) never had
>    the filter. Nothing to gate on: a consumer can tell this build by
>    `--version` (0.2.9).
> 42. **Buildings: an interior ring is a courtyard, not a building (§9a, §9b;
>    `aether_converter` 0.2.9; 2026-10-23).** No job-JSON change. Every ring
>    of a polygon used to be drawn as a building of its own, so every
>    courtyard went up to its building's roof — inside the exterior's fill
>    and again as a "building". A building is now its **exterior ring minus
>    its interior rings**: a pixel belongs to it when its centre is inside the
>    exterior and inside none of the holes (point-in-polygon, half-open edges
>    as before). The holes are left out of the roof's ground reference too
>    (the P10 of the terrain under the footprint); `max` between buildings is
>    unchanged. **Vector tiles** (`buildings_pbf_dir`, the WASM functions):
>    rings are paired as MVT spec 2.1 §4.3.4.4 lays them out — each exterior
>    ring begins a polygon, the interior rings after it are its holes — with
>    the winding taken exactly from the integer tile coordinates (positive
>    surveyor's-formula area, clockwise with y down, is exterior; zero area
>    is degenerate and skipped) and, as the reference decoder (vector-tile-js
>    / MapLibre `classifyRings`) does, the first ring's winding standing for
>    "exterior", so a tile written with the opposite convention pairs up
>    alike. (The area computed before omitted the closing edge and was taken
>    in absolute lon/lat, so its sign was meaningless; it only ever served to
>    skip "degenerate" rings.) **FlatGeobuf** (`buildings_file`) had the same
>    defect and is fixed the same way: the first ring of each polygon — of
>    each part of a MultiPolygon, PolyhedralSurface or TIN — is its exterior,
>    the rest its holes, by position as OGC Simple Features defines it
>    (winding is not consulted). **`.abt` bytes change** under every building
>    with a courtyard (the courtyard stays at the terrain; the roof can move
>    by the changed ground reference); buildings without holes are
>    byte-identical, FlatGeobuf's legacy byte-equivalence test included. An
>    invalid polygon whose interior ring lies outside its exterior no longer
>    draws that ring. Interior rings no longer count as buildings in the
>    informational counts (`[Buildings] … <n> footprint(s)`, `after dedup`).
>    The WASM `decode_buildings_geojson` writes a courtyard as an interior
>    ring of its building's `Polygon` (`coordinates: [exterior, hole, …]`,
>    RFC 7946) instead of as a `Feature` of its own. Nothing to gate on: a
>    consumer can tell this build by `--version` (0.2.9).
> 43. **`plan`: no row or column outside the area from float artefacts
>    (§1.2; `aether_converter` 0.2.9; 2026-10-23).** No CLI or schema change
>    (`aether-plan/1`, same fields); the tile set changes for about one bbox
>    in four. The grid is now, with `finest` the smallest sub-tile extent of
>    the requested resolutions and `sub` each one's own:
>
>    ```text
>    idx_south = floor(south / finest + 1e-9)    idx_north = ceil(north / finest - 1e-9)
>    idx_west  = floor(west  / finest + 1e-9)    idx_east  = ceil(east  / finest - 1e-9)
>    edge      = round(idx * finest, 9)          # Rust: (idx * finest * 1e9).round() / 1e9
>    lat start = floor(edge_south / sub + 1e-9) * sub   (lon start: edge_west alike)
>    step      = round(x + sub, 6) while x < edge_north (lon: x < edge_east)   # unchanged
>    ```
>
>    Before (≤ 0.2.8): `edge = floor/ceil(x / finest) * finest` and `start =
>    floor(edge / sub) * sub`. Each change removes one artefact, and all
>    three are needed: (1) an edge **on** the grid whose quotient lands a
>    hair past its line (S 47.3 / 0.1 = 472.99999999999994) was snapped a
>    whole step out; (2) the bare product can land a hair past the line the
>    6-decimal steps arrive at (E 11.6466 → 117 · 0.1 = 11.700000000000001,
>    the steps reach 11.7, and `lon < east` held once more); (3) with the
>    edge rounded, a resolution's start `floor(edge / sub)` is taken a step
>    out wherever that quotient lands a hair short (47.3 / 0.1 again) — the
>    rounding without the start term still adds a row or column on the
>    south/west side of about one bbox in eight (761 of 6,000 in the sweep
>    below). `round(idx * finest, 9)` is Python's correctly rounded
>    `round()`; the Rust form gives the same double on every grid line
>    within ±180° for every extent (checked exhaustively: such a product is
>    within ~1e-14 of a multiple of 0.1, 0.25, 0.5, 1 or 2, never near a
>    tie). Effect: the owner's bbox S45.7161 N48.1913 W4.9482 E11.6466 at
>    2, 5, 10, 30, 90 m plans **2,438** tiles (1,700 + 308 + 308 + 90 + 32,
>    68 columns of 2 m) instead of 2,463 — the 25-tile E11.70 column lay
>    wholly east of the area (all VOID, 1.56 GB); S47.3 N47.6 W8.3 E8.6 at
>    2 m plans 9 tiles instead of 12. Tested against exact decimal
>    arithmetic over 6,000 bboxes — half with 4-decimal edges anywhere,
>    half with edges on the 0.1° grid, five resolution sets: no difference
>    (the same sweep over the ≤ 0.2.8 formulas: 1,373 differ; the 1e-9 snap
>    alone: 995; snap and rounding without the start term: 761). **Gate
>    (consumers that cross-check `plan`):** for this build and later,
>    enumerate with exactly the formulas above — the snap, the rounding
>    **and** the start term; for older builds, the old ones. Any other mix
>    makes the tile lists differ and a cross-check abort (§1.5).
> 44. **`ingest`: a batch decodes its `buildings_pbf_dir` in parallel and
>    reports how far it is (§1.2, §9a; additive; `aether_converter` 0.2.9;
>    2026-10-23).** No job-JSON change; `.abt` bytes and every count are
>    unchanged (on NTFS by construction, elsewhere see (a)). Motivation: a
>    batch decodes each `buildings_pbf_dir` once, before its first tile — one
>    read and one MVT decode per file, one file after the other, printing
>    nothing. 20,808 tiles took 6 min 6 s while a download wrote to the same
>    HDD (17 ms a tile; 40 s with the disk idle), and the plugin's progress
>    bar stood still while its ETA ran away. (a) **Parallel, same result.**
>    The tile files are listed, sorted by name (byte-wise), and read and
>    decoded on the batch's worker pool (the `[Rust] Rayon Pool:` threads);
>    the results are taken in name order, exactly as a walk over the sorted
>    list one file at a time takes them: the building list in the same order,
>    hence the duplicate filter's choices, `tiles_read` and the
>    decoded/after-dedup counts; the `[Warn] buildings <name>: <error>` skip;
>    the first unreadable tile as the run's error. Before, the walk followed
>    `read_dir` order. NTFS lists a directory in index order — upcased names
>    as UTF-16 units — which for every name the decoder accepts is byte order
>    (two such names part at a digit, `+`, `_` or `.`, never at a letter;
>    pinned by a test), so on Windows the walk, and with it every output byte
>    and count, is that of the sequential decoder it replaces (the container's
>    Windows-backed mount lists in name order too, checked). A file system
>    with an order of its own (ext4 hashes, APFS) was walked in that order and
>    is now walked in name order: the same buildings, in an order independent
>    of the disk; `.abt` bytes could move only where the duplicate filter now
>    keeps the other of two copies of one building, whose coordinates differ
>    by float rounding, and a pixel centre lies that close to an edge. Checked
>    with the 0.2.9 sequential build against this one on 2,100 building tiles
>    (synthetic and real OSM, 1 undecodable) and 4 output tiles, on the
>    Windows-backed mount and on the container's overlay file system: all
>    `.abt` byte-identical, every `[Info] buildings_pbf_dir` and `[Warn]` line
>    identical. (b) **Progress lines** on stdout (§9a): `[Rust] Decoding
>    building tiles: <n>/<m>` every 1,000 tiles and once at the end, then
>    `[Rust] Building tiles decoded: <m> tile(s), <b> building(s) in <s.s> s`;
>    neither contains `Progress:`. The summary line is now the first line
>    after the decode, so a consumer that stops a run at the line after
>    `[Warn] buildings …` (the Waveshed plugin's `BurnMonitor`) stops it there
>    — before any tile is sampled. (c) **Order of the edges.** The `[Warn]
>    buildings` lines come together, in name order, after the last progress
>    line (they came as the walk met them); a mixed-zoom directory is refused
>    before any tile is read (same message; before, after the tiles ahead of
>    the odd zoom). The single-job form and `download`'s buildings post-pass
>    and repairs use the same loader on rayon's global pool, without the new
>    lines. Measured in the container (4 CPUs, warm cache, release build;
>    `buildings.rs` `bench_pbf_decode_2000_tiles_sequential_vs_parallel`, an
>    ignored test — `AETHER_PBF_BENCH_DIR` picks the disk), 2,000 gzipped
>    copies of the OpenFreeMap tile 14/8651/5782 (940,000 buildings):
>    sequential 6.0 s, of which ~1.7 s is the duplicate filter, which stays on
>    one thread; 4 threads 2.5 s (2.4×). On the container's Windows-backed 9p
>    mount, where every file costs a round trip: 19.5 s → 5.9 s at 4 threads;
>    2,000 small tiles 12.6 s → 1.65 s. The owner's HDD under a concurrent
>    download is not measured. Nothing to gate on: a consumer can tell this
>    build by `--version` (0.2.9).
> 45. **CLI `download`: building tiles fetched during the download —
>    `buildings_pbf_ready` / `buildings_pbf_wait_s` (§9b; additive;
>    `aether_converter` 0.2.10; 2026-10-23).** Motivation: a download job
>    burns its `buildings_pbf_dir` after the terrain is assembled, reading
>    the directory as it finds it then, so a caller had to finish fetching
>    the building tiles before it could launch the job — the Waveshed
>    plugin's Site Analysis fetched OpenFreeMap tiles for 27 s, then
>    downloaded terrain for 19 s, one after the other. New optional job
>    fields: `buildings_pbf_ready` (string: one plain file name inside
>    `buildings_pbf_dir`, e.g. `"_ready"`, whose existence declares the
>    directory complete) and `buildings_pbf_wait_s` (seconds, 0 … 86400;
>    absent or `null` → **1800**). With the marker, nothing in the job reads
>    the directory before the buildings post-pass (it need not exist at
>    launch); the post-pass runs where it always did and first waits for
>    the marker — one look at once, then one every 250 ms — announcing a
>    wait on stderr with `[Buildings] waiting for the building tiles
>    (<marker path>)` and its end with `[Buildings] building tiles ready
>    after <s.s> s` (nothing when the marker is there at the first look),
>    then reads and burns the directory exactly as before: the parallel
>    decode of item 44, the same `[Buildings] <n> pbf tile(s) → …` line, the
>    same `.abt` bytes as a job whose directory was complete at launch. A
>    marker still missing after `buildings_pbf_wait_s` fails the run —
>    `Error: Building tiles not ready: …`, exit 1, no burn line, its `.abt`
>    files removed — never a silent run without buildings. Bad values are
>    refused before any request. `[Stats] Downloaded: … in <s>s` leaves the
>    wait out, `[Download] TOTAL:` keeps it. Details, messages and the
>    caller's side: §9b *Building tiles fetched during the download*.
>    **Unchanged:** a job without `buildings_pbf_ready`, byte for byte and
>    line for line (`buildings_pbf_wait_s` alone is not read); repair jobs
>    and the in-memory/WASM downloaders, which ignore both fields. Tested in
>    `tests/test_download_buildings_ready.rs` (loopback terrain, synthetic
>    MVT tiles): the marker there at launch → no wait line, the burn line and
>    bytes of the job without the field; the directory created, its tiles and
>    then the marker written 2 s after launch → one wait line, one ready line
>    (its seconds at least the time from the wait line to the marker, at
>    most 1.5 s more), then the same burn line and bytes; no marker within
>    `buildings_pbf_wait_s: 1` → exit 1 with the
>    message, no ready or burn line, the outputs removed; without the field
>    → today's line and bytes (`buildings_pbf_wait_s` alone included); bad
>    fields → refused with no request made and nothing written; a repair
>    with the marker gone → no wait. Against the 0.2.9 build, on the
>    OpenFreeMap tile 14/8651/5782 (470 buildings, nine 5 m output tiles, 12
>    z15 source tiles): without the field, with `buildings_pbf_wait_s`
>    alone, with the marker there at launch and with the marker written 2 s
>    in, all nine `.abt` `cmp`-identical to 0.2.9's, and stderr identical
>    modulo numbers but for the two wait lines. **Gate** on
>    `aether_converter` 0.2.10 (§1.5): an older build ignores both fields
>    and burns whatever the directory holds when its terrain is done — part
>    of the tiles while a fetch still runs — or fails with
>    `buildings_pbf_dir … is not a directory` when it does not exist yet.
> 46. **`[Tile] <name> voids=<n>`: each tile's void samples from `download`
>    and batch `ingest`; `download`'s progress record also in
>    `AETHER_DIAG_FILE` (§1.2, §2; additive, parsed; `aether_converter`
>    0.2.11; 2026-10-25).** No job-JSON change; `.abt` bytes are unchanged
>    (`cmp` against 0.2.10 of a 6-tile batch `ingest` over 2,100 building
>    tiles — synthetic and real OSM, one undecodable — with a void hole,
>    partial coverage and `void_fill_m`, and of two 6-tile z13 downloads,
>    with and without that building directory, two source tiles 404'd and
>    one deep-water tile: all 18 `.abt` identical; stdout/stderr identical
>    modulo numbers but for the new lines). Motivation: after a download
>    the Waveshed plugin read every output tile in full to find its void
>    samples (7.3 GB for 119 tiles of 2 m, 18 s, on the owner's machine),
>    and its log panel is to show only every 10th progress line. **What
>    counts as void:** a sample at or below the validity floor of **-5000
>    half-metres (-2500 m)** — the `-9999` void sentinel and anything else
>    a consumer must not read as ground: bathymetry (Terrarium carries it
>    down to -11000 m), Mapbox's lowest value (-10000 m), a deep
>    `void_fill_m`. It is the floor `ingest`'s samplers already apply (a
>    source sample is ground only above it), and the rule a consumer that
>    fills voids uses — counting the sentinel alone reported 0 for a tile
>    of water at -3000 m, which then reached the engine unfilled. (a)
>    **CLI `download`, full runs:** once every output is finished — strip
>    assembly, tail fill and the buildings post-pass done — and before the
>    `[Stats]` block, stderr gets one line per output tile, in job order,
>    written and flushed together: `[Tile] <output file name> voids=<n>`,
>    `<n>` = the samples of the file as it now is at or below the floor.
>    Counted as the rows are written (a row no tile-row covered — the tail
>    fill — is all void), then lowered by what the burn lifts: `max` takes
>    any roof over a void sample, so a footprint whose ground reference is
>    terrain puts its roof on the void pixels it reaches — no longer void
>    —, while one whose ground reference is itself void puts the sentinel
>    plus its height there (e.g. `-9987` for 6 m), still below the floor
>    and still counted. No file is read back. A run that fails prints none
>    (it removes its outputs); a repair job prints none.
>    (b) **Batch `ingest`:** right after each tile's `[Tile] <name>
>    covered=<a>/<b>` line — unchanged, it still ends after `<a>/<b>`, as an
>    older plugin's pattern requires — a line of its own, `[Tile] <name>
>    voids=<n>`, same name, written and flushed with it before the `[Rust]
>    Progress:` line. `<n>` is counted after `void_fill_m` and the building
>    burns: a covered pixel is never void (the samplers average only samples
>    above the floor), so it is `<b> − <a>` for a tile without a fill and
>    without buildings (a source sample at or below the floor is no
>    coverage), 0 with a `void_fill_m` above -2500 m and `<b> − <a>` with one
>    at or below it; a burn lowers it as in (a). The single-job form prints
>    neither line, as before. (c) **`AETHER_DIAG_FILE` keeps the progress
>    record.** When it is set, every `[Download] <pct>% (…)` line of a
>    download pass (full run or repair), the `[Download] STALL …` and
>    `(suppressing …)` lines, the `[Download] first <class>: …` lines
>    (`timeout`, `connect error`, `decode error`, `unknown error`), the
>    `[Degraded] …` lines and every line of the `[Stats]` block are ALSO
>    appended to that file: byte for byte the stderr lines, no prefix, in
>    stderr's order. stderr is unchanged — still every whole percent. Not
>    copied: the `[Download] z=` header and the other `[Download]` lines,
>    `[Tile]` (the voids lines are, since 0.2.12 — changelog 48, also a
>    batch ingest's), `[Buildings]`, `[Warn]`. Tested in `tests/test_tile_voids.rs`
>    (every count against an independent count of the written file: a
>    download with a 404'd source column — tiles with no, some and all
>    samples void; the same with buildings — a house on terrain reaching
>    into the void lifts it, one wholly inside it stays void; water at
>    -3000 m (-6000) counted and ground at -2499.5 m (-4999) not, in a
>    download and in ingest, where a -6000 source sample is no coverage and
>    a fill of -3000 m counts while one of -2499.5 m does not; a repair, no
>    line; ingest batches with partial coverage and an internal void hole,
>    with `void_fill_m`, fully covered, not covered, and with buildings
>    over the void), `buildings.rs` (the rasterizer's count of lifted
>    samples) and `tests/test_diag_file.rs` (a download with a STALL, a
>    timeout and a decode failure filled from z−1: the file holds exactly
>    stderr's progress, STALL, first-failure, `[Degraded]` and `[Stats]`
>    lines, besides the diagnostics it always had). **Gate** on
>    `aether_converter` 0.2.11 (§1.5): an older build prints no voids line —
>    the consumer must count the voids itself or treat them as unknown —
>    and keeps the progress record on stderr only.
> 47. **`buildings_pbf_dir`: the duplicate filter runs in parallel (§9a,
>    §9b; `aether_converter` 0.2.11; 2026-10-25).** No job-JSON or line
>    change; the surviving buildings, their order, every count and every
>    `.abt` byte are unchanged. The decode of a building directory went
>    parallel in changelog 44, but the whole-ring duplicate filter after it
>    (changelog 41) stayed on one thread, building a key — one allocation
>    per ring — for every building and inserting it into one hash set: 16 s
>    for 5.1 M buildings (21,148 tiles), 8 s for 3.1 M, 7 s for 2.2 M on the
>    owner's 16-thread machine, all inside the `[Rust] Building tiles
>    decoded: … in <s.s> s` a batch prints. It now runs on the decode's
>    pool (a batch's `[Rust] Rayon Pool:` threads, else rayon's global
>    pool): each building's key is hashed in place, without allocating; the
>    `(hash, index)` pairs are sorted; equal keys hash equal, so every group
>    of copies lies in one run of equal hashes, in list order, and in each
>    run the first building of each key survives — decided by comparing the
>    keys themselves, so a hash collision costs time, never a building. The
>    result is the sequential filter's, by construction and by test
>    (`buildings.rs`): survivor indices equal on the real OpenFreeMap tile
>    (twice at its own address and once beside it), 1,500 synthetic tiles
>    with buffer-zone copies, a seam, and a set of every near miss the key
>    tells apart (height one ulp off, the other datum, a courtyard more or
>    with a vertex moved, a vertex one 1e-6° quantum off or 2e-7° off, a
>    rotated ring, keyless buildings, NaN and −0.0 vertices) — on 1, 3 and
>    8 threads and the global pool, and with every hash forced to collide.
>    Measured in the container (16 CPUs visible, a cgroup quota of 4;
>    release build; `bench_duplicate_filter_5m_sequential_vs_parallel`, an
>    ignored test): 5,115,800 buildings — the real OpenFreeMap tile at
>    10,000 addresses with its buffer-zone copies, 415,800 of them dropped
>    — sequential 10.3 s, parallel 0.47 s on 4 threads (0.47 s on 8, 0.56 s
>    on 16, oversubscribed). End to end, a batch over 11,000 copies of that
>    tile on the Windows-backed mount (5,169,530 buildings): `[Rust]
>    Building tiles decoded: … in` 26.7 s → 16.7–17.7 s, the rest being the
>    decode's file reads over 9p; peak RSS 2.26 → 1.39 GB. `.abt` bytes and
>    `[Info] buildings_pbf_dir` lines are identical to 0.2.10's for that
>    batch and for the checks of item 46. Nothing to gate on: a consumer
>    can tell this build by `--version` (0.2.11).
> 48. **Batch `ingest`: building tiles fetched while the batch samples its
>    terrain — `buildings_pbf_ready` / `buildings_pbf_wait_s` /
>    `buildings_hold_mb`; the `[Tile] … voids=` lines also in
>    `AETHER_DIAG_FILE`; the raw tile cache keeps a real answer over a
>    derived tile (§1.2, §1.5, §9a, §9b; additive; `aether_converter`
>    0.2.12; 2026-10-25).** Motivation: a batch decodes its
>    `buildings_pbf_dir` once, before its first tile, so a caller had to
>    finish fetching the building tiles before it could launch the ingest —
>    on the owner's machine the Waveshed plugin's Site Analysis waited 6 s of
>    a 27 s run for them, its Map Converter's Phase 2 39 s before a 123 s
>    converter run. (a) **The hand-over** — the `download` job's (changelog
>    45), with the same meaning. New optional fields of a batch job:
>    `buildings_pbf_ready` (one plain file name inside `buildings_pbf_dir`
>    whose existence declares the directory complete; the same name rules
>    and messages), `buildings_pbf_wait_s` (0 … 86400 s, absent or `null` →
>    **1800**, counted from the batch's first look, at launch) and
>    `buildings_hold_mb` (b). The marker there at launch: the directory is
>    decoded at once and the batch runs exactly as without the field — bytes
>    and lines. Missing at launch: nothing reads the directory (it need not
>    exist), every tile's terrain starts at once, a watcher looks for the
>    marker every 250 ms and decodes the directory as soon as it is there —
>    the loader and lines of changelog 44/47, now when the marker appears, on
>    a pool of the batch's size beside the batch's own. A tile whose terrain
>    (and `buildings_file` burn) is done before the set is decoded is
>    **held** in memory — its samples, not written — while the held tiles
>    fit the hold budget; a tile that does not fit **waits on its worker
>    thread**, which starts no other tile meanwhile. Once the set is decoded
>    the held tiles are burnt and written in the order they finished (as
>    many drainer threads as the batch has workers, and every worker that
>    comes free, take them from the front), every later tile inline, as
>    before: the same `.abt` bytes as a batch whose directory was complete
>    at launch. The `[Tile] … covered=` / `voids=`, `[Info]
>    buildings_pbf_dir:` and `[Rust] Progress:` lines keep their meaning and
>    come when a tile is written — so none of a held tile before its
>    buildings are ready. New stdout lines (informational, prefixes stable,
>    as the download's): `[Buildings] waiting for the building tiles
>    (<marker path>)` once, when the first tile has to wait; once the set is
>    decoded, `[Buildings] building tiles ready after <s.s> s` (seconds since
>    the waiting line) and `[Buildings] <n> tile(s) held (<m.m> MB) until the
>    building tiles were ready` (+ `; <k> more waited on their worker threads
>    (hold budget <b.b> MB full)` when some did) — none of the three when no
>    tile had to wait. A marker still missing `buildings_pbf_wait_s` after
>    the first look fails the batch, exit 1: `Error: Building tiles not
>    ready: the marker file <marker path> (buildings_pbf_ready) did not
>    appear within <w> s (buildings_pbf_wait_s); the ingest fails rather
>    than write tiles without their buildings`; a directory that does not
>    decode fails it with the loader's error, as at launch (e.g.
>    `buildings_pbf_dir mixes zoom levels …`). Either way the held tiles are
>    dropped, a tile not yet started is skipped, one being sampled stops at
>    its next band, and since no tile of the directory is written before its
>    set is ready, **the batch leaves no output tile of it behind** (no
>    `[Tile]`, `[Error]` or progress line for them). (b) **Hold budget**:
>    `buildings_hold_mb` (MB = MiB, 0 … 1048576; the smallest value the
>    watched jobs give, taken as given — `0` holds nothing); absent: half of
>    the available memory the batch measures for its pool once the pool's
>    reservation is taken off (threads × the per-tile estimate of the `[Rust]
>    Rayon Pool:` line, plus the 192 MB chunk cache), at most 4096 MB, never
>    below one tile (`size_px² × 2` bytes of the largest watched tile). (c)
>    **Checked before any tile**, nothing read: the fields by the download
>    job's rules, `buildings_hold_mb`, and that the jobs naming one
>    directory give the same `buildings_pbf_ready` and `buildings_pbf_wait_s`
>    (it is decoded once for the run); a job's error is prefixed `ingest job
>    for "<output_path>": `. `buildings_pbf_wait_s` and `buildings_hold_mb`
>    without `buildings_pbf_ready` are not read — not even checked — so a
>    batch without the field is unchanged byte for byte and line for line.
>    (d) **The single-job form** (a JSON object) refuses
>    `buildings_pbf_ready` before anything is read, exit 1:
>    `buildings_pbf_ready "<v>" is read by a batch ingest only (a JSON array
>    of jobs): …` — it decodes its `buildings_pbf_dir` before its tile, as it
>    finds it, so ignoring the marker would burn a directory still being
>    fetched; a one-job array takes the field. (e) **`AETHER_DIAG_FILE` keeps
>    the `[Tile] <name> voids=<n>` lines** too, which changelog 46 had left
>    out: a download's (stderr) and a batch ingest's (stdout), byte for byte,
>    in the stream's order; both streams are unchanged, the ingest's covered
>    lines are not copied. (f) **The raw tile cache never writes a derived
>    tile over a real answer** (§9b *Raw source-tile cache*). Two processes
>    may share one cache — the Waveshed plugin runs band b's coarser derive
>    pass beside band b+1's z15 fetch — and the pack writer wrote its entries
>    without looking at the slots, last writer winning: the fetch's parent
>    fill could store the service's own z14 (z13, …) tile, and the derive
>    pass, which had judged that tile from an index read at its start, then
>    replace it with its 2 × 2 mean. Now the writer reads, under the pack
>    lock, what each target slot holds and skips a derived item (status 3)
>    when the slot holds a body that is all there and matches its CRC
>    (status 1) or a 404 (status 2), or when the same batch carries a real
>    answer for that slot before it; a real answer still replaces anything,
>    a derived tile a derived one or an entry that does not read. `[Stats]
>    Cache: … <written> written` no longer counts a skipped derived tile.
>    The reverse order never lost anything. A converter before 0.2.12
>    sharing the cache still writes blindly: both processes must be this
>    build or later for the guard to hold. Tested:
>    `tests/test_ingest_buildings_ready.rs` (an `.abt` terrain under 12
>    tiles, synthetic MVT): the fields alone and the marker at launch → the bytes
>    and stdout of the batch without them; the directory created, its tiles
>    and then the marker written 2 s after launch → no tile written before,
>    one waiting line naming the marker, the decode lines after it, one
>    ready line (its seconds at least the time from the waiting line to the
>    marker, at most 1.5 s more), one held line, every tile line after them,
>    the reference's bytes; a hold of one tile → `1 tile(s) held`, workers
>    waiting, the same bytes; no marker within `buildings_pbf_wait_s: 1` →
>    exit 1 with the message, no tile, no tile line; a mixed-zoom directory
>    after its marker → its error, no tile; bad fields and disagreeing jobs
>    → refused before the batch starts; a single job → refused, its other
>    fields ignored. `buildings_ready.rs` (the budget rule, the checks, the
>    held line); `tests/test_diag_file.rs` (the voids lines in the file, for
>    a batch ingest and a download); `tile_cache.rs` (a derived write over a
>    real body, over a 404, over a derived entry, over a body that fails its
>    CRC, before and after a real answer in one batch; the writer's count).
>    Against the 0.2.11 build, on the batch of changelog 46/47's checks (6
>    tiles over 2,100 building tiles — synthetic and real OSM, one
>    undecodable — a void hole, partial coverage, `void_fill_m`): without
>    the fields, with `buildings_pbf_wait_s` / `buildings_hold_mb` alone and
>    with the marker there at launch, all `.abt` `cmp`-identical and stdout /
>    stderr identical modulo numbers and completion order; with the
>    directory appearing during the run (6 and 24 tiles), a hold of one
>    512 px tile and a hold of 0, all `.abt` identical to 0.2.11's. Measured
>    in the container (4 threads, release build, building directory
>    renamed into place 3 s after launch, three runs each), against the
>    batch launched once the directory is complete: a terrain-bound batch
>    (32 tiles of 700 px at 10 m over one LV95 2 m LZW source) 6.4–6.5 s →
>    5.4–5.6 s from the start of the fetch, the terrain now running during
>    it; a write-bound one (120 tiles of 1000 px at 2 m, 229 MB written to
>    the container's 9p mount — writes that cannot start before the set is
>    ready in either form) 7.9–8.4 s → 7.9–8.2 s, no gain; all `.abt`
>    identical between the two forms. The owner's runs are not measured.
>    **Gate** on `aether_converter` 0.2.12 (§1.5): an older build ignores
>    the three fields and reads the directory at launch — `buildings_pbf_dir
>    … is not a directory` when it does not exist yet, else whatever it
>    holds then — and keeps the voids lines off the diag file.
> 49. **CLI `download`: the seam filter — samples of a tile service's mosaic
>    seam become voids, which the parent fill fills; `seam_filter` job field;
>    the backfill reported — `[Stats] Backfill:`, and `[Backfill]` lines per
>    tile in `AETHER_DIAG_FILE` (§1.2, §1.5, §2, §6, §9b; additive;
>    `aether_converter` 0.2.13; 2026-11-05).** Motivation: AWS Terrarium's
>    zoom-15 mosaic has a seam at the 1° source boundary just west of 10°E.
>    Over lat 46.7 – 46.9 the tile column x = 17294 holds −8,956 … −9,717 m in
>    pixel column 52 (lon 9.9998) and −1,184 … +149 m in column 53 between
>    terrain of 1,500 – 3,000 m — valid samples blended into a −32768 m nodata
>    area by the service's resampling (≈ 0.33 and ≈ 0.08 of −32768 m, measured)
>    — and the zoom-14 tile above them one column of −3,524 … −4,709 m; the
>    tiles beside them are blank (z15 x = 17292, 17293, 17295, 17296 in places,
>    z14 8646 and 8648 nearly whole). The blank samples (at or below −11000 m)
>    were filled from coarser zooms without a word; the seam samples, being
>    above that, were written into the `.abt` as a 9 km trench beside a 1 km
>    one (the Waveshed plugin's −2500 m floor caught the first and filled it
>    with 0 m; nothing caught the second). The same seams were found elsewhere:
>    z15 rows at 28°N over Everest (−6,555 / +3,680 m between 8,260 m), z15
>    columns at 70°W over Aconcagua (−8,142 / +1,544 m between 6,070 m),
>    single-sample lines at z14/z15 near Trollveggen (one of them slanting
>    across its tile), at 95°E over Yarlung Tsangpo, and around the Dead Sea's
>    −410 m surface. (a) **The rule** (`download/seam.rs`). A valid sample is a
>    *seam notch* when both its row neighbours are more than **D = 2,200 m**
>    higher, or both its column neighbours are — per axis: a seam is a line,
>    and the samples along it are its neighbours on the other axis. A neighbour
>    is the nearest valid sample within **W = 2** samples in that direction,
>    inside the tile: a void is skipped, never a neighbour; the tile edge or a
>    run of voids ends the search, and an axis without a neighbour on both
>    sides is not judged (no sample is judged across a blank area or the tile
>    edge). Each pass finds its notches against the tile as it found it and
>    voids them together; the rule runs again on the result, at most W passes,
>    stopping at the first that finds none — a two-sample seam whose inner
>    sample is the lower (column 52 below column 53) goes in two: column 53's
>    left neighbour is column 52 in the first pass, column 51 in the second.
>    Only tiles of zoom ≥ **11** are filtered. (b) **Where.** One decoder
>    (`TileDecoder`) decodes every tile a CLI `download` run uses — fetched,
>    read from the raw tile cache, the children a cached tile is derived from
>    (changelog 40), and the coarser tiles the parent fill reads — and runs the
>    rule on each right after the decode, so the voided samples reach the
>    parent fill as voids (it fills samples at or below −11000 m; a seam void
>    is −65536 m in memory) and a seam in a parent (the z14 column above the
>    10°E seam) is skipped there, the fill going on to the next coarser zoom.
>    Repair jobs decode the same way. The cache keeps the service's bytes
>    exactly as they arrived. A tile the cache derives is built from filtered
>    children and then filtered at its own zoom (exactly what decoding its
>    stored body gives); a sample that is void because its children's seam
>    samples were voided is stored as Terrarium RGB `0, 0, 1` (−32767.996 m, a
>    void to every converter, an older one included) instead of the service's
>    void `0, 0, 0`, and read back as a seam void by a filtering run, so a
>    later run reporting on a stored derived tile counts its seam samples as
>    the run that derived it did. Not filtered: Mapbox jobs (no void value, not
>    surveyed), zooms ≤ 10 (below), and the in-memory and browser downloaders
>    (`run_download_mem`, the WASM `download_terrain` and `assemble_terrain`),
>    which have no in-tile backfill to fill a void. (c) **`seam_filter`**
>    (§9b): absent, `null` or `true` → on; `false` → off, every tile decoded
>    exactly as by 0.2.12 — for A/B runs, which need a `cache_dir` of their own
>    (a tile a filtering run derived and stored holds its filtered samples).
>    The Waveshed plugin does not set it. (d) **Lines** (informational, not
>    parsed). Last in the `[Stats]` block but for the cache lines, which stay
>    last: `[Stats] Backfill: <n> px in <m> tile(s) filled from coarser zooms
>    (<k> px seam samples)` — `<n>` the void samples of the tiles that arrived
>    (fetched, cached or derived) that a coarser zoom filled, `<m>` those
>    tiles, `<k>` how many of the `<n>` the seam filter had voided — plus `;
>    <l> px in <t> tile(s) left void (<s> px seam samples)` when some had no
>    coarser data (they stay void, as before, and are counted by the `[Tile] …
>    voids=` lines). Printed on stderr and kept in `AETHER_DIAG_FILE` like the
>    whole block; omitted when no arrived tile had a void sample. A tile that
>    never arrived and was repaired whole is counted by `[Stats] Repaired from
>    coarser parent tiles:` as before, not here. With `AETHER_DIAG_FILE` set,
>    each such tile also gets one line in the file only — never on the console,
>    which a GUI shows its users: `[Backfill] z=<z> x=<x> y=<y>: <v> void px
>    (<s> seam), <f> filled (z<a> <n>, z<b> <n>, …), <l> left void` (the zooms
>    that filled, finest first), at most 20,000 per run, then `[Backfill] … <n>
>    more tile(s) not listed`. (e) **The survey behind D, W and the zoom
>    floor** (the converter's own `seam.rs`, compiled into a survey tool; AWS
>    Terrarium tiles fetched 2026-11-05). Deepest both-sided single-sample
>    notch, metres, by zoom — 6 × 6 tiles at z15/z14, 4 × 4 at z13 – z11, 2 × 2
>    at z10 – z6, around Matterhorn, Eiger/Lauterbrunnen, Aareschlucht,
>    Dolomites, Verdon gorge, Trollveggen, Geirangerfjord, Yosemite, Grand
>    Canyon, Zion, Everest, Aconcagua, Monterey Canyon, the Dead Sea, Yarlung
>    Tsangpo and Kali Gandaki; 8 × 8 at z12/z11 around 17 high-relief regions
>    (Himalaya, Karakoram, Pamir, Andes, Alaska, Alps, Norway, New Zealand,
>    Caucasus, US); and **every** z11 tile of 68 – 98°E × 27 – 38°N (13,072
>    tiles). Outside the seams and the corrupted samples named below: z15 ≤ 17
>    (Yosemite 9.6, Zion 8.3), z14 ≤ 51 (Grand Canyon 50.1), z13 ≤ 141
>    (Everest), z12 ≤ 439 (Southern Alps 438.9, Denali 433.7), z11 ≤ 1,030 (a
>    gorge at 84.15°E 28.5°N that the service's z12 and z13 tiles show too: 935
>    m in z12 2 × 2 means, 645 m in z13 4 × 4 means) — D is 2.14× the deepest.
>    Everything deeper at z11 – z15 is a seam (above) or a single corrupted
>    sample: holes at the shores of flat-filled lakes (Tibet, Karakul), pits
>    beside +6 … +21 km spikes (Piz Buin z11, Cordillera Blanca), isolated pits
>    (Denali z11: 658 m between 2,160 – 2,202 m), isolated pits in Monterey
>    Bay, where Terrarium has no bathymetry above z10 (0.0 m at z14/z15, flat 0
>    m with a few pits at z11 – z13). The seam's shallowest sample at 10°E is
>    2,793 m deep (column 53, pass 1; 2,800 m in pass 2), the z14 column 6,317
>    m. Every sample the rule voids in the survey tiles — 8,692 outside the
>    10°E ones — lies on such a line (one sample per row or column of its tile)
>    or is a corrupted sample: at z11 – z13 each is absent from the 2 × 2 means
>    of the service's own next finer tiles or holds a negative elevation in
>    high mountains (Tibet, Silvretta); of the 110 z11 notches deeper than
>    1,000 m, 103 are absent from those means, 6 of the other 7 are such
>    corruption and one is the 1,030 m gorge. At zooms 7 – 9 the service's own
>    tiles hold single-sample pits of 3.7 – 6.0 km (Kali Gandaki z8: 540 m
>    between 6,573 m samples), so no depth there tells a seam from the data:
>    the floor is z11. What the rule leaves: columns 54 – 56 of the 10°E seam
>    lie −0.54 / −0.36 / −0.18 m below a fit through the clean columns on both
>    sides (5th percentile −5.7 / −4.2 / −2.7 m; the same fit at two seam-free
>    places of those tiles: 0.00 / −0.15 m, 5th percentile ≈ −2.5 m); the
>    isolated −88 m pit at 5°E 45.9°N (375 m deep) and notches under D in
>    general; spikes. (f) **Measured** (release build, the container, AWS S3,
>    2026-11-05). The owner's job — lat 46.7 – 46.9, lon 9.95 – 10.05 at 2 m:
>    four 5,556 px tiles from 532 z15 source tiles and 76 parents. 0.2.12
>    writes `voids=5007` and `voids=5019` into the two E9.90 tiles (the −9 km
>    column) and 15,011 / 15,825 samples below 0 m; 0.2.13 writes `voids=0` and
>    none below 0 m, prints `[Stats] Backfill: 7092426 px in 137 tile(s) filled
>    from coarser zooms (8943 px seam samples)` and 137 `[Backfill]` lines in
>    the diagnostic file (8,943 = the 6,895 + 2,048 samples the survey's two
>    passes void there, in 27 tiles; the other 110 tiles hold 7,083,483 of the
>    service's own void samples, which 0.2.12 filled without a word). It
>    changes 16,119 and 15,825 samples in the E9.90 tiles — columns 5544 – 5548
>    — from −9,717 … +149.5 m to 1,619.5 … 3,051.5 m; the E10.00 tiles are
>    `cmp`-identical. 31,919 of the 31,944 changed samples equal the z13 sample
>    under them to the half-metre (the z14 column above the seam is voided
>    too), the other 25 the z13 sample one row over (the assembly places rows
>    linearly in latitude within a tile-row); they lie a median 9 m, at most 55
>    m, from the mean of the output samples three columns west and east of the
>    changed band (columns 5541 and 5551, which differ from each other by a
>    median 7 m, at most 33 m). The same job with `seam_filter: false` is
>    `cmp`-identical to 0.2.12 on all four tiles (`[Stats] Backfill: 7083483 px
>    in 110 tile(s) … (0 px seam samples)`), and fetched and cached runs of
>    0.2.13 are identical. Clean ground: a 2 m tile over the Matterhorn (140
>    source tiles, no void sample — no `Backfill` line) and four over 5°E
>    45.9°N (513 source tiles, 108 of them blank: `Backfill: 7077888 px in 108
>    tile(s) … (0 px seam samples)`) are `cmp`-identical to 0.2.12, fetched and
>    from the cache. The filter costs 136 – 244 µs per 256 px tile (one core,
>    release; 222 µs over the 13,072 z11 tiles), beside a decode of ~1.6 ms
>    (changelog 36). Tested: `download/seam.rs` (a one-sample notch on either
>    axis; the measured two-sample 10°E seam, which needs the second pass and W
>    = 2; a 6,000 m cliff, a spike, a 2-sample-wide 5,000 m trench and gorges
>    1,030 and 2,200 m deep left alone, 2,201 m voided; a void skipped, a void
>    run and the tile edge ending the search, the service's voids untouched; a
>    bathymetry slope from −10,900 m with a canyon and a trench; the zoom
>    floor; the shortcut equal to the rule on 200 random tiles at four depths
>    and reaches), `download.rs` (`seam_filter` absent/`null`/`true`/`false`,
>    Mapbox; with the filter off the decode is `decode_tile`'s bit for bit; the
>    derived-body seam void read back; the `[Backfill]` and `[Stats] Backfill:`
>    lines), `tile_cache.rs` (a tile derived from children with the seam: its
>    samples, its stored body, read back; without the filter the seam averaged
>    to −4,900 m; a trench that is a notch only at the parent's zoom: derived
>    and read back alike), `tests/test_seam_filter.rs` (the real 10°E tiles as
>    fixtures, the real CLI: with the filter, fetched, from the cache, derived
>    from the cached z15 tiles and read back as a stored derived tile, every
>    `.abt` byte equals a run without it over the same tiles with their seam
>    samples blanked; the `[Stats] Backfill:` counts; without it the seam
>    reaches the output; the `[Backfill]` lines in the diagnostic file only).
>    Every mechanism was broken once on purpose and a test failed for each.
>    **Gate** on `aether_converter` 0.2.13 (§1.5): an older build ignores
>    `seam_filter`, writes the seam into its tiles and prints neither line.
> 50. **The process's peak resident memory at the end of a run: `[Rust]
>    run: … peak rss <mb> MB` and `[Stats] Peak RSS:` (§1.2, §1.5, §2;
>    additive, informational — NOT parsed; `aether_converter` 0.2.13;
>    2026-11-07).** No job-JSON change; `.abt` bytes, exit codes and every
>    parsed line are unchanged. Motivation: the converter printed what it
>    budgets and estimates — `[Download] RAM budget: <mb> MB …`, `[Rust]
>    Rayon Pool: <n> threads (System RAM: …, ~<mb> MB per tile)` — but never
>    what the process used, so a log could not say whether a run stayed
>    inside its budget, or whether memory made it slow or made it fail.
>    (a) **`ingest`:** the `[Rust] run:` line (item 31, 37) ends with `,
>    peak rss <mb> MB`, after `blocked ~<s>s`; every earlier field keeps its
>    place and shape. (b) **CLI `download`:** every pass — a full run, a
>    repair job, and a run that fails the majority rule, before its `Tile
>    download failed:` — prints once in its `[Stats]` block `[Stats] Peak
>    RSS: <mb> MB (RAM budget <m> MB)`, after every other line of the block
>    but the `[Stats] Cache…` lines, which stay last; `<m>` = the run's
>    budget as its `[Download] RAM budget: <m> MB` line printed it. A repair
>    job has no RAM budget (it holds its listed tiles at once): its line
>    ends at `MB`. Kept in `AETHER_DIAG_FILE` like the whole block (item
>    46). The in-memory and browser downloaders (`run_download_mem`, the
>    WASM `download_terrain`) print neither: they run inside another
>    program's process. `<mb>` = the peak of the process's resident memory
>    so far, in MB (2^20 bytes, the `MB` of every converter line) with one
>    decimal; where the OS does not give it the field (ingest, with its
>    comma) or the line (download) is omitted — never 0. (c) **What is
>    read:** Linux: `VmHWM` of `/proc/self/status`; macOS: `getrusage`
>    `ru_maxrss` (bytes there, kilobytes on the BSDs); Windows:
>    `GetProcessMemoryInfo` `PeakWorkingSetSize` — one reader for both
>    commands (`proc_usage.rs`, which also reads the `[Rust] run:` line's
>    CPU time and page faults, moved there from `ingest.rs`; on macOS and
>    Windows the peak comes from those very calls). Not `ru_maxrss` on
>    Linux: the kernel carries the parent's peak into a child's `ru_maxrss`
>    at `exec` — after `fork`, `vfork` and `posix_spawn` alike — so a
>    converter started by QGIS would report QGIS's peak. Measured (the
>    container, debug build, the 2-tile batch `ingest` of
>    `tests/test_diag_file.rs`; two runs each): started by a Python process
>    holding 1,500 MB, the converter's `ru_maxrss` (the parent's
>    `getrusage(RUSAGE_CHILDREN)` after it exited) is 1,510 MB while its
>    line says `peak rss` 13.6 / 13.9 MB; started by a 10 MB one, 13.75 MB
>    and `peak rss` 13.8 / 13.6 MB. Examples (debug build, from a test
>    process holding 256 MB):
>    `[Rust] run: wall 0.1s, cpu user 0.0s sys 0.1s, page faults 979,
>    threads 4, blocked ~0.3s, peak rss 13.2 MB`; a download of two 256 px
>    tiles from 12 z12 source tiles `[Stats] Peak RSS: 24.6 MB (RAM budget
>    2048 MB)`, a repair of one of them `[Stats] Peak RSS: 17.8 MB`, a run
>    whose tiles all fail to decode `[Stats] Peak RSS: 18.1 MB (RAM budget
>    2048 MB)`. Tested: `proc_usage.rs` (this platform's peak above the 64
>    MB the test process wrote and below the RAM, and still that after the
>    free; `VmHWM` read in KiB, a missing, zero or non-kB value unknown; MB
>    = 2^20 bytes), `ingest.rs` (the `[Rust] run:` line: the field last,
>    omitted with its comma when unknown, this process's value plausible),
>    `download.rs` (the line with and without a budget, none when unknown,
>    this process's value plausible), `tests/test_peak_rss.rs` (the real
>    CLI, started by a test process holding 256 MB: a batch and a single
>    `ingest` — one `[Rust] run:` line, still last on stdout, its old fields
>    in order and shape, `peak rss` last with one decimal, above 1 MB, below
>    the RAM and below the parent's 256 MB; a full download with `cache_dir`
>    and `AETHER_DIAG_FILE`, a repair of one of its source tiles, and a
>    download that fails — one line per pass, the full and the failed run's
>    with the budget of their `[Download] RAM budget:` line, the repair's
>    without, after every other `[Stats]` line and followed by exactly the
>    cache lines, and — full run and repair — once, byte for byte, in the
>    diagnostic file); `tests/test_download_cache.rs` still finds `[Stats]
>    Cache:` last in the block. Every mechanism was broken once on purpose
>    and a test failed for each (`ru_maxrss` on Linux included). Not run
>    here: macOS and Windows — `proc_usage.rs` type-checks for
>    `aarch64-apple-darwin` (and `wasm32-unknown-unknown`); the Windows
>    branch is read only: `PeakWorkingSetSize` is a `usize` field of
>    windows-sys 0.61's `PROCESS_MEMORY_COUNTERS`, the struct the `[Rust]
>    run:` line already filled. **Gate:** none — no consumer may parse
>    either; `aether_converter` 0.2.13 (§1.5) prints both, an older build
>    neither.
> 51. **`buildings_pbf_list`: the building tiles a job needs, named in a
>    list file and read straight from the caller's store — `download` and
>    batch `ingest` (§1.2, §1.5, §9a, §9b; additive; `aether_converter`
>    0.2.14; 2026-11-07).** Motivation: the converter decodes a whole
>    `buildings_pbf_dir`, so the Waveshed plugin built, before each converter
>    batch, a directory of hard links holding exactly the building tiles the
>    batch needs, out of its store `<cache>/pool/<md5>/{z}_{x}_{y}.pbf`
>    (~49,000 files and growing): 21,000 links took 24 s on the owner's NTFS
>    HDD (2026-11-05). The owner approved a list instead (2026-11-07). (a)
>    **The field.** `buildings_pbf_list`: the **absolute** path of a UTF-8
>    text file, one building tile file name per line (`{z}_{x}_{y}.pbf` or
>    `.mvt`, no directory part; `\n` or `\r\n`; blank lines, `#` lines, blanks
>    around a name and a byte-order mark ignored; a name listed twice decoded
>    once). The names resolve inside `buildings_pbf_dir`, which is **never
>    listed**: exactly the listed files are read, in name order, through the
>    directory's loader (changelog 44/47) — the same building list,
>    duplicate filter, counts and `[Warn] buildings <name>: …` skips, hence
>    the `.abt` bytes and lines of the same job whose `buildings_pbf_dir`
>    holds exactly the listed tiles. A name with a path separator, or a line
>    that is no tile name, fails the run naming the list and the line; an
>    **empty list** (no byte, or blank/comment lines only) is no error and
>    decodes nothing, exactly as an empty directory (`[Info]
>    buildings_pbf_dir: 0 tile(s), no buildings in extent` per ingest tile,
>    `[Buildings] 0 pbf tile(s), no buildings in extent` on a download). On
>    `download` jobs — full runs **and repair jobs** (`repair_source_tiles`),
>    every pass — and `ingest` batch jobs; the single-job ingest form refuses
>    it, as `buildings_pbf_ready`; the in-memory/WASM downloaders ignore it.
>    A repair honours the list exactly as a first pass: the listed tiles
>    burnt onto its repaired pixels, the same checks, the same wait for a
>    list that is not there, the same failures (it still ignores the
>    marker, `buildings_pbf_ready`; §9b *Repair jobs*). (b)
>    **Checked before anything runs** (exit 1, no request, no tile, nothing
>    written): the list needs `buildings_pbf_dir`, excludes
>    `buildings_pbf_ready` and must be absolute; `buildings_pbf_wait_s` and
>    (ingest) `buildings_hold_mb` are read and checked as with a marker;
>    `buildings_pbf_dir` must **exist at launch** — the one look at the disk,
>    never its listing (the plugin creates its store before launching); a
>    batch's jobs naming one list must give the same directory and wait (one
>    decode per distinct list; jobs may name one list or several). Messages:
>    §9a. (c) **The list is the ready signal**: the marker's hand-over of
>    changelog 45 (download) and 48 (batch), step for step — the same
>    watcher, 250 ms polls, `buildings_pbf_wait_s`, hold budget, clock and
>    lines, the list's path exactly as the job gives it in place of the
>    marker's: `[Buildings] waiting for the building tiles (<list path>)`,
>    `[Buildings] building tiles ready after <s.s> s`, the held line, and on
>    timeout `Error: Building tiles not ready: the list file <list path>
>    (buildings_pbf_list) did not appear within <w> s (buildings_pbf_wait_s);
>    …`, nothing written. Write the list last, under another name, then
>    rename it into place: it is read once, right after it is seen. (d)
>    **Lines.** A batch's decode starts with `[Rust] Decoding <n> listed
>    building tiles from "<dir>" (once for the run)...` (`<n>` the distinct
>    listed names, a plain integer; `<dir>` as the job gives it, between
>    quotes — not the `Debug` form of the directory's start line, which
>    doubles a Windows path's backslashes); `[Rust] Decoding building tiles:
>    <n>/<m>`, `[Rust] Building tiles decoded: …`, `[Info]
>    buildings_pbf_dir: …` and `[Buildings] <n> pbf tile(s) → …` are
>    unchanged (`<n>` there: the listed tiles that decoded). A listed tile not
>    in the directory, or unreadable, fails the run — `Error: Building tile
>    <name> listed in <list> is not in <dir>`, exit 1, nothing written (a
>    download's outputs removed); two zooms listed: `buildings_pbf_list
>    <list> mixes zoom levels (…)`. (e) **Tested**: `buildings.rs` (the
>    list's CRLF/LF lines, blanks, comments, BOM, duplicates and name order;
>    directory parts and non-tile names refused with their line; a missing
>    or non-UTF-8 list; the listed tiles of a 400-tile store with an
>    undecodable tile, across the 9999|10000 seam where name order picks the
>    duplicate filter's survivor, decoded bit for bit as a directory of
>    exactly those tiles on 1, 3 and 8 threads, with the same progress
>    calls; a missing listed tile — one line — and an unreadable one — its
>    OS error as cause; two zooms; an empty list reading nothing),
>    `buildings_ready.rs` (the checks and their messages, one decode per
>    list, a list and its directory whole as two sources, the hold budget of
>    a watched list, the watcher's timeout naming the list), `main.rs` (the
>    start line: `<dir>` as given, `<n>` plain) and
>    `tests/test_buildings_pbf_list.rs` (the real CLI: batch `ingest` and
>    `download` with a list over a store that also holds a needed tile the
>    list leaves out, an undecodable tile and a z15 tile → the bytes and
>    lines of a directory of exactly the listed tiles, no `[Warn]`, other
>    bytes than with the left-out tile; the list written after launch → the
>    wait, ready (and held) lines naming the list as the job spells it — the
>    store spelt `<dir>/cache//./pool\0a1b2c` and named so in the start
>    line — and the same bytes; a list that never comes, a listed tile
>    missing (present at launch and arriving late), a directory part → the
>    errors, no tile written, a download's outputs removed; empty lists of
>    no byte and of comments only → an empty directory's bytes and lines;
>    two lists in one batch → two decodes; bad fields and a missing store →
>    refused before any tile or request; the single-job form refused, a
>    one-job array taken; a repair pass with the list over that store → the
>    `[Buildings] repair:` line and bytes of a repair over a directory of
>    exactly the listed tiles, other bytes than with the left-out tile, no
>    `[Warn]`; a repair whose list comes late → the wait lines, the same
>    bytes; one whose list never comes or names a missing tile → the error,
>    the existing files unchanged; a repair's bad fields → refused before
>    any request). On unix the store also holds a FIFO named like a tile:
>    a run that opened any unlisted file would block until the test kills
>    it. Every mechanism was broken once on purpose and a test failed for
>    each: decoding the store's listing instead of the names, keeping
>    duplicate names, list order instead of name order, the start line's
>    `Debug` path, a normalised path in the wait line, an empty list
>    refused, the store's check skipped, a repair ignoring the list, a
>    repair not waiting for it, opening every file of the store (12 tests
>    blocked on the FIFO). (f) **Measured** (release build, 4
>    threads, the container's Windows-backed 9p mount): a store of 49,005
>    files — 21,000 copies of the OpenFreeMap tile 14/8651/5782 (470
>    buildings) at their own addresses, listed, and 28,000 undecodable files
>    and five z15 tiles, unlisted; a batch of six 1000 px tiles at 5 m.
>    The old way's preparation, 21,000 hard links: 227.1 s on this mount
>    (24 s on the owner's HDD); the list, 378,032 bytes: 0.041 s. `[Rust]
>    Building tiles decoded: 21000 tile(s), 9870000 building(s) in`: from
>    the link directory 52.5 s (0.2.13), 52.3 – 55.2 s (0.2.14, five runs);
>    from the store through the list 54.4 – 57.6 s (seven runs; one more at
>    65.6 s with twice the system time); the list over the link directory
>    51.6 / 52.1 s — the list costs nothing, the 2 – 4 s are the larger
>    directory on this mount (a `readdir` of the store first changed
>    nothing: 57.6 / 57.1 s). Peak RSS 2,586 – 2,590 MB in every run. All
>    six `.abt` of every run `cmp`-identical to 0.2.13's from the link
>    directory, `[Info] buildings_pbf_dir` lines identical, stderr empty in
>    every list run (none of the 28,000 undecodable files read); `strace` of
>    a list run: no `getdents64` at all, the store never opened as a
>    directory, 21,000 `openat` inside it — the listed files, each once. A
>    CLI `download` (AWS Terrarium z14 from a raw-tile cache, two 1000 px
>    tiles at 5 m, the 20 building tiles under them listed from the same
>    store): `[Buildings] 20 pbf tile(s) → 9400 building(s) (0 dup dropped)
>    → 2/2 abt tile(s), 51287 px in 0.2s`, as 0.2.13's from a directory of
>    links to those 20, `.abt` `cmp`-identical; the list renamed into place
>    4 s after launch: the wait line, `[Buildings] building tiles ready
>    after 3.8 s`, the same bytes; an empty list: the empty-directory line,
>    the terrain-only bytes. A repair of the z14 source tile under them
>    (over a terrain-only first pass; a store of the 20 listed tiles, 2,000
>    undecodable and five z15 files): `[Buildings] repair: 9400 building(s),
>    1406 repaired px raised`, `[Download] repair: 68172 px rewritten in 1
>    output tile(s)` — the lines and `cmp`-identical `.abt` of 0.2.13's and
>    0.2.14's repair over a directory of links to the 20 —; `strace`: no
>    `getdents64`, 20 `openat` in the store, the listed files; the list
>    renamed into place 4 s in: the wait, `[Buildings] building tiles ready
>    after 3.3 s`, the same bytes; a list that never comes: the error, the
>    files unchanged; the same repair with the store as its directory and no
>    list fails `buildings_pbf_dir mixes zoom levels (14 and 15)`. The
>    owner's machine is not measured. **Gate** on `aether_converter` 0.2.14
>    (§1.5): an older build ignores the field — it decodes the whole
>    `buildings_pbf_dir` (the store: every file) without waiting for the
>    list, a batch at launch, a download after its terrain and a repair
>    before its burn; slow, burning the store's every building, and failing
>    on a store of two zooms or with an unreadable file.
> 52. **Honest memory budgets: a CLI `download` pass's `RAM budget` is the
>    whole process's and a pass short of memory runs fewer connections; a
>    batch `ingest` counts the building set it decodes (§1.2, §1.5, §2,
>    §9a, §9b; additive, informational — nothing parsed; `aether_converter`
>    0.2.15; 2026-11-07).** No job-JSON change; `.abt` bytes, exit codes and
>    every parsed line are unchanged. Motivation: measured by the owner on
>    2026-11-07 (Windows, 31 GB RAM, 16 threads, HDD) with the lines of item
>    50, z15 download passes peaked at 1.9 – 2.4× their budget —
>    `[Stats] Peak RSS: 2907.9 MB (RAM budget 1540 MB)`, `3567.4 MB (RAM
>    budget 2048 MB)` — which priced the strips' decoded tiles alone, so two
>    download lanes and an ingest left 3 GB available and the next ingest
>    started on 12 threads instead of 16; batches peaked at up to 6,250.2 MB,
>    their pool sized `~250 MB per tile` × threads without the decoded
>    building set (5,134,165 buildings in one). Approved by the owner
>    (2026-11-07): "honest memory budgets; shrink the connection window when
>    memory is short".
>    (a) **Where a download pass's memory goes** (measured in the container:
>    Linux, 4 CPUs, release builds; peak RSS from `/proc` every 20 ms, the
>    live heap by an `LD_PRELOAD` counter of every allocation by size).
>    0.2.14, a z15 pass 456 tiles wide and 34 deep (15,504 tiles, five
>    strips, 20 outputs of 5,556 px at 5 m, 256 connections) served by a
>    local server from 7,752 real AWS Terrarium bodies: `[Stats] Peak RSS:
>    2292.3 MB (RAM budget 2048 MB)`; at the live heap's peak (2,209.7 MB)
>    7,595 decoded strip tiles (1,898.8 MB, 93 % of the budget), the two
>    assemblies' grids (2 × 114.0 MB — a tile-row, 456 × 256 KiB), their
>    output rows (35.4 MB), hyper's read buffers (254 × 128 KiB, 31.8 MB),
>    15 MB else. Connections: 32 / 256 / 1,024 of them left 12.9 / 46.1 /
>    153.8 MB beside tiles, grids and rows — 142 KB each (over AWS itself,
>    256 slow bodies left ~33 MB). Every seventh tile a 404: 63 MB more of
>    256 KiB tiles (the parent fill's), peak 2,365.8 MB. 2 m outputs (25 of
>    5,556 px over 3,192 tiles): a tile-row's output rows 112 MB beside a
>    57 MB grid (180.4 MB of rows at the live peak of 0.2.15, two
>    assemblies). A raw tile cache that writes: 74 MB of queued bodies (57 –
>    98 KB classes); deriving z14 from cached z15 (4,104 tiles): 35 MB of
>    32 – 256 KB blocks (children, PNG buffers, queued bodies). The
>    allocator's share, peak RSS over peak live heap: glibc 1.0 – 10.4 % on
>    fetch-only passes with 5 m outputs, 14.0 – 18.8 % with 2 m ones (their
>    thousands of 11 KB rows), 3.4 – 17.0 % with a cache (21.6 % on a
>    681 MB pass); mimalloc — the Windows binary's allocator (item 37), the
>    same 3.3.2 built on Linux, transparent huge pages off as on Windows —
>    17.2 – 35.4 % fetch-only, 31.3 – 53.4 % with a cache (14 passes). Input
>    slower than the assembly never fills the strips: a 228-wide pass over
>    AWS itself (12.9 MB/s) peaked at 1,373.0 MB of its 2,048, the 456-wide
>    one writing a raw tile cache to the slow mount at 1,609.0 MB.
>    (b) **The budget is the process's.** The share is unchanged — min(
>    available / 4, 2 GiB), at least 256 MiB, read once after the first
>    tile — and pays, in live bytes (`download/memory.rs`): the assemblies
>    (2, or 1 for a one-strip job) × (a tile-row of grid + the fullest
>    tile-row's output rows + the column tables), the connections × (64 KiB
>    + edge² × 2 B) — no more than tiles the network serves —, 96 MB for a
>    raw tile cache that writes, the workers' decoding (4 tiles each, 16
>    when deriving), then the allocator's share (12 %, 20 % with a cache;
>    Windows 40 %, 55 %) and 24 MB base. The strips get the rest, never more
>    than the job's tiles. The line keeps its text, its `<mb>` is that total
>    and its breakdown follows in parentheses (§1.2); `[Stats] Peak RSS: …
>    (RAM budget <m> MB)` names it. (c) **Fewer connections when memory is
>    short.** Where the rest is less than two tile-rows of strips, the pass
>    runs the most connections that leave them, at least 16, says so once —
>    `[Download] connections <a> → <b>: …` — and its window runs between
>    max(`<b>` / 4, 16) and `<b>`; where even 16 do not fit, the strips get
>    their two rows all the same and the line says what that takes, over
>    the share: never a failure. (d) **The parent fill shares the strips'
>    budget**: a strip waits while the cached coarser tiles take its room
>    (never when no strip holds any), and the cache keeps at most a quarter
>    of it (was a fixed 256 MiB on top). The buildings post-pass, which
>    runs after the terrain, is priced apart where its tiles are there when
>    the budget is drawn — the set estimated as for ingest (below), held
>    × 1.4 at the decode's peak, plus the largest output tile — and the
>    budget is the larger phase; where its marker or list is still to come
>    the line says it is not counted. (e) **Ingest.** Each building
>    source there at launch is estimated from its tile files' bytes on disk
>    (metadata only; a list's store never listed) × 3 —
>    `buildings::DECODED_PER_DISK_BYTE`, measured as the decoded set's bytes
>    over the files' on OpenFreeMap z14 tiles gzipped as the plugin stores
>    them: 2.58 central Zürich (256 tiles, 190,014 buildings), 2.92 Bern
>    (576, 222,933), 0.41 Lower Engadin (256, 4,059); uncompressed 1.70 /
>    2.05 / 0.31; the fixture tile 1.41 — and taken off the memory the pool
>    is sized from (and the default hold budget's); the `[Rust] Rayon Pool:`
>    line appends it. `[Rust] Building tiles decoded: …` ends with the set's
>    own size, `<mb> MB` (`PbfBuildingSet::heap_bytes`: within 0 – 5.3 % of
>    what glibc's `mallinfo2` reports the decode left allocated, six real
>    sets). Each decode replaces its source's estimate with that size, and
>    where the sets then leave room for fewer tiles than run at once, fewer
>    are sampled from there on — `[Rust] Tiles at once: …` — through one
>    lane per worker that takes the jobs in order and runs each on the pool
>    (a worker never waits for another tile; a batch without buildings runs
>    as before). Both, because the Waveshed plugin launches a batch before
>    its list exists (changelog 51): only the decode can count such a set,
>    and only a cap can act on it then; the estimate sizes the pool where
>    the source is there at launch. Not capped: the decode itself, which
>    holds ~1.4× the set at its peak (below), and the tiles held (changelog
>    48) when a late set arrives.
>    (f) **Measured after** (same container and jobs, 0.2.15; peak RSS
>    against the printed budget). Strips that fill (input faster than the
>    assembly): the 456-wide pass `[Download] RAM budget: 2047 MB, strip
>    rows: 6 (18.3 GB available at start; fixed for the run) (strips 1453 MB
>    + assembly 2 × 150 MB + 256 connections 48 MB + decoding 4 MB, +12 %
>    allocator, +24 MB base)`, peak 1,743.9 MB (85 %); with its 404s
>    1,793.4 MB (88 %); the 2 m pass 1,204.5 of 1,354 MB (89 %); 2,640 MB
>    available: `[Download] connections 256 → 184: 660 MB available for this
>    pass; the assembly (2 × 150 MB), decoding (4 MB) and two tile-rows of
>    strips (228 MB) leave room for 184 connections (0.19 MB each, +12 %
>    allocator)`, peak 612.4 of 659 MB (93 %), 184 requests at once; 2,400
>    MB: `… leave no room for connections: the pass runs 16 and takes 624
>    MB, over its share`, peak 562.0 of 624 MB (90 %). Strips that cannot
>    fill: over AWS 1,262.2 of 2,047 MB (62 %), from the raw tile cache
>    1,235.8 (60 %), deriving z14 1,185.7 of 1,757 (67 %), writing a cache
>    to the slow mount 1,059.6 of 2,047 (52 %). mimalloc with the Windows
>    shares (the code built on Linux): 1,603.3 / 1,288.4 / 1,006.7 /
>    1,312.9 MB against 2,047 / 1,687 / 2,047 / 2,047 (the 456-wide pass,
>    the 2 m pass, cache writes, deriving) — under it in every pass. A
>    one-tile terrain pass whose post-pass decodes the 13,824 tiles below:
>    0.2.14 `RAM budget: 2048 MB`, peak 1,445.0 MB; 0.2.15 `RAM budget: 1499
>    MB, … (strips 15 MB + assembly 1 × 2 MB + 63 connections 11 MB +
>    decoding 4 MB, +12 % allocator, +24 MB base; buildings post-pass 1499
>    MB)`, peak 1,444.5 MB (96 %).
>    Ingest, 13,824 real OpenFreeMap tiles (327,270,016 bytes, 5,350,392
>    buildings) listed in a batch of 16 tiles: `building set ~936 MB`,
>    `[Rust] Building tiles decoded: 13824 tile(s), 5350392 building(s) in
>    41.0 s, 912.4 MB`, the live heap 929 – 1,013 MB while the tiles ran
>    (1,258 MB at the decode's peak, its arrays and duplicate filter on top),
>    peak RSS 1,460.2 MB (0.2.14: 1,460.8, the same 4 threads); 2,000 MB
>    available: `[Rust] Rayon Pool: 2 threads (…, building set ~936 MB)`
>    (4 without it); the list written 20 s after launch: `building set: 1
>    source(s) decoded later`, then `[Rust] Tiles at once: 2 of 4 threads
>    from here on (building sets 912.4 MB; ~200 MB per tile, 1 GB available
>    at start)`. The estimate's cost, a list there at launch: 13,824
>    metadata reads on the container's 9p mount, 4.1 s (8 threads), the
>    decode after them 3.1 s faster — `[Rust] run: wall` 25.7 s against
>    24.1 s. The owner's machine is not measured.
>    (g) **Tested**: `download/memory.rs` (the overhead's terms, the total's
>    allocator share and base, a roomy share giving the strips the rest, a
>    small job priced at what it holds, the shrink to the most connections
>    that leave the strips' floor, the floor of 16 and over the share, a
>    fixed strip budget, the output rows by brute force, the post-pass as
>    the larger phase or not counted, the lines),
>    `download.rs` (a gate limited for memory admits its new ceiling and
>    adapts below it; the launch rule counts the parent fill's tiles but
>    never stalls; the ancestor cache's lowered budget and bytes),
>    `buildings.rs` (heap blocks; the set's heap bytes as allocated; the
>    disk estimate counts exactly the tile files a directory or list
>    decodes — a FIFO named like a listed tile never opened; the factor's
>    measurement, ignored), `main.rs` (the decoded line, the threads, the
>    pool line's suffix, the cap lowered and never raised, the lanes: every
>    job once, at most the cap at once, a panic), `proc_usage.rs` (the test
>    override) and `tests/test_memory_budget.rs` (the real CLI: a one-strip
>    download of 2,000 tiles, 400 wide, over 256 loopback connections peaks within
>    15 % of its budget and above its strips by more — 659.9 of 754 MB here,
>    debug build, 500 MB of strips; a post-pass of 200 tiles there at launch
>    priced as the larger phase, one whose list comes later named not
>    counted; 1,000 MB
>    available → `connections 256 → <b>`, the window at `<b>`, no more
>    requests at once, the bytes of a run with the machine's memory; a
>    batch's building set estimated, counted and its size printed — 1
>    thread instead of 2; a list written late lowering the tiles at once,
>    right after the decode's line). Every mechanism was broken once on
>    purpose and a test failed for each (thirteen: the strips-only budget,
>    no shrink, the shrink without the gate's limit, the estimate not
>    counted, the cap never lowered, the decode line without its size, the
>    array's length for its capacity, every file counted, lanes ignoring the
>    cap, the parent fill not counted, two assemblies for one strip, the
>    ancestor limit raising the budget; and the post-pass left out of the
>    budget). **Gate:** none — informational
>    lines, nothing to parse; `aether_converter` 0.2.15 (§1.5).

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
aether_converter probe (--list <file> | --stdin) [--jobs N]    # §1.2a, ≥ 0.2.7
```

* Sub-commands: `ingest`, `download`, `plan`, `probe` (§1.2a) (and a deprecated
  `convert` that just prints a deprecation line). `ingest`/`download` take
  `--job-file <path>` (short `-j`).
* stdout/stderr here use a **different, `[Rust]`/`[Download]`/`[Stats]`-prefixed
  style** — *not* the `[P:]/[S:]/[E:]/[D:]` markers of §1.1.
* **Ingest progress** (stdout): `[Rust] Progress: <n>/<total>` (emitted every 10
  tiles and once at completion) — `println!("[Rust] Progress: {}/{}", curr, total)`.
* **Building-tile decode progress** (stdout, batch ingest with a
  `buildings_pbf_dir`; `aether_converter` ≥ 0.2.9, changelog 44), before the
  first tile: `[Rust] Decoding building tiles: <n>/<m>` every 1,000 building
  tiles and once at the end, then `[Rust] Building tiles decoded: <m>
  tile(s), <b> building(s) in <s.s> s, <mb> MB` (`, <mb> MB` ≥ 0.2.15,
  changelog 52) — see §9a *Building-tile decode*.
  Neither contains `Progress:`. A directory whose `buildings_pbf_ready`
  marker is missing at launch (≥ 0.2.12, changelog 48) prints the same lines
  when the marker appears, while tiles are being sampled — still before any
  `[Rust] Progress:` line of a tile that burns it. A `buildings_pbf_list`
  (≥ 0.2.14, changelog 51) prints them too, once per list, after its own
  start line `[Rust] Decoding <n> listed building tiles from "<dir>" (once
  for the run)...` (`<n>` = the distinct listed names, a plain integer;
  `<dir>` = `buildings_pbf_dir` exactly as the job gives it) in place of
  the directory's `[Rust] Decoding building tiles in "<dir>" (once for the
  run)...`; `<m>` counts the listed tiles.
* **Building-tile hand-over lines** (stdout, batch ingest with
  `buildings_pbf_ready` whose marker was missing at launch;
  `aether_converter` ≥ 0.2.12, changelog 48 — or with a `buildings_pbf_list`
  whose list was, ≥ 0.2.14, changelog 51, the list's path, exactly as the
  job gives it, in place of `<marker path>`; informational, prefixes stable
  — match the prefix, as for the download's `[Buildings]` wait lines):

  ```
  [Buildings] waiting for the building tiles (<marker path>)
  [Buildings] building tiles ready after <s.s> s
  [Buildings] <n> tile(s) held (<m.m> MB) until the building tiles were ready[; <k> more waited on their worker threads (hold budget <b.b> MB full)]
  ```

  The first once, when the first tile's terrain is done and the set is not
  decoded yet; the other two once the set is decoded (`<s.s>` = seconds since
  the first, one decimal; `<n>` / `<m.m>` = tiles held in memory and their
  MB; `<k>` = tiles whose worker waited, the hold being full); none of them
  when no tile had to wait. None matches `[Buildings] <source>:` (the
  `buildings_file` line) or `[Info] buildings_pbf_dir:`. See §9a *Building
  tiles fetched during the batch*.
* **Ingest diagnostic lines** (stdout, or the file `AETHER_DIAG_FILE` names
  — below; `aether_converter` ≥ 0.2.7, changelog 31, 37; **diagnostic, not
  parsed** — for people reading a log, wording may change, consumers must
  not match them). Every tile, single job or batch, prints once its file is
  written (or the tile failed after its terrain was sampled):

  ```
  [Rust] tile <name>: <n> sources, header <s>s (open <ms> ms, read <ms> ms avg), decode <s>s (<files> files, <MB> MB read), sample <s>s, bands <n>, geom <s>s, write <s>s, total <s>s
  ```

  `header` is the wall time of resolving the tile's source headers (open and
  read-and-parse averaged over the headers this tile actually read — 0 when
  all came from the batch's header cache), `decode` the window loads (the
  decode plus any wait for a read still in flight; files read from disk and
  bytes read, reads issued ahead included; windows served from the chunk
  cache cost no file), `sample` the resampling, `bands` the row bands,
  `geom` the row geometry and band planning, `write` the wall time of
  writing, syncing and renaming the tile file (`0.0` when the tile failed
  before), and `total` the tile's wall time on its compute thread — in a
  batch the write runs on a writer thread, overlapping the next tile, and is
  not part of it; a tile held for its building tiles (≥ 0.2.12, changelog
  48) counts its terrain and its burn, not the wait between. In a batch the
  line precedes the tile's `[Tile]` line. A converter before 0.2.8 ends the
  line at `bands <n>` and prints it when
  sampling ends. An `ingest` process prints at exit — after the batch
  result, also when the run fails — on stdout:

  ```
  [Rust] run: wall <s>s, cpu user <s>s sys <s>s, page faults <n>, threads <n>, blocked ~<s>s, peak rss <mb> MB
  ```

  (`getrusage` on Unix: minor + major faults; `GetProcessTimes` /
  `GetProcessMemoryInfo` `PageFaultCount` on Windows). `threads` is the
  batch's worker-pool size (the `[Rust] Rayon Pool:` count); the field and
  its comma are omitted when no batch ran (a single-job file). `blocked`
  (≥ 0.2.8) is wall × threads − (user + sys), floored at 0: thread time
  spent waiting rather than computing (a single job counts rayon's global
  pool). `peak rss` (≥ 0.2.13, changelog 50) is the process's peak
  resident memory, MB (2^20 bytes) with one decimal — read as for the
  download's *Peak RSS line* below — last, so every earlier field keeps
  its place; the field and its comma are omitted where the OS does not
  give it, never `0`. Neither line contains `Progress:` or starts with
  `[Tile]`.

  **`AETHER_DIAG_FILE` (≥ 0.2.8, changelog 37).** When this environment
  variable names a path, the `[Rust] tile …` lines (and `download`'s
  `[Strip …]` / `[StripMem …]` lines, changelog 36, and — 0.2.9, changelog
  39 — its `[Download] launch gate closed for …` line) are appended to that
  file — created if missing, never truncated, each line flushed as written
  — and not printed on the console. Unset or empty: the console, as before.
  A file that cannot be opened costs one stderr line, `[Warn]
  AETHER_DIAG_FILE = "<path>" cannot be opened (<cause>); diagnostics go to
  the console`, and the lines stay on the console; it never fails a run.
  `[Rust] run:` and every parsed line stay on their streams. Older builds
  ignore the variable. Since 0.2.11 (changelog 46) the file also keeps a
  download's progress record: every `[Download] <pct>% (…)` line (full run
  or repair), the `[Download] STALL …` / `[Download] (suppressing …)` lines,
  the `[Download] first <class>: …` lines, the `[Degraded] …` lines and
  every line of the `[Stats]` block are appended to it **as well** — they
  stay on stderr exactly as before (every whole percent), and the file gets
  the same bytes, no prefix, in stderr's order. A consumer may show only
  part of them (the Waveshed plugin shows every 10th percent) and point at
  the file for the rest. Since 0.2.12 (changelog 48) the `[Tile] <name>
  voids=<n>` lines are appended **as well** — a download's from stderr, a
  batch ingest's from stdout (not its `covered=` line) — byte for byte, in
  the stream's order; both streams are unchanged. Since 0.2.13 (changelog
  49) it also gets a download's per-tile `[Backfill] …` lines, which go to
  the file **only** (see *Backfill line* below). The download's `[Stats]
  Peak RSS:` line (≥ 0.2.13, changelog 50) is a line of the `[Stats]` block
  and is kept like the rest of it; `ingest`'s `[Rust] run:` line, its `peak
  rss` included, stays on stdout only.
* **Batch pool lines** (stdout, batch `ingest`; informational, **not
  parsed** — consumers must not match them):

  ```
  [Rust] Rayon Pool: <n> threads (System RAM: <t> GB, <a> GB available, ~<m> MB per tile[, building set ~<b> MB[ + <k> source(s) decoded later]])
  [Rust] Rayon Pool: <n> threads (System RAM: <t> GB, <a> GB available, ~<m> MB per tile, building set: <k> source(s) decoded later)
  [Rust] Tiles at once: <c> of <n> threads from here on (building sets <s.s> MB; ~<m> MB per tile, <a> GB available at start)
  ```

  The pool line comes once, before any building-tile decode: `<n>` =
  clamp((80 % of the available RAM − the 192 MB chunk cache − the building
  sets) / `<m>`, 1, CPUs), `<m>` the per-tile estimate (changelog 23, MB),
  `<t>` and `<a>` whole GB. Since 0.2.15 (changelog 52) the building sets
  count: each distinct building source there at launch — a
  `buildings_pbf_dir` without a marker or with its marker in place, a
  `buildings_pbf_list` that exists — is decoded before the first tile and
  is estimated, before the pool is built, as its tile files' bytes on disk
  × 3 (metadata only; a list's directory is never listed): `<b>`, whole
  MB; a source whose marker or list is not there yet (`<k>`) counts once it
  is decoded. A batch without a building source prints the line as before,
  ending at `per tile)`. The default `buildings_hold_mb` budget takes the
  sets decoded at launch off the available RAM too (§9a). After each
  decode the set's own size (`<mb>` of the decode's summary line, §9a)
  replaces its estimate; where the sets then leave room for fewer tiles
  than run at once, the second line says so — once per lowering, right
  after that summary line — and from then on at most `<c>` tiles are
  sampled at once (lowered only, never raised; `<s.s>` = the sets counted,
  MB with one decimal). With a building source a batch takes its tiles in
  job order, each of `<n>` workers taking the next when its own is done
  (without one, as before). Neither line contains `Progress:`. A test may
  impose the available RAM (`AETHER_TEST_AVAILABLE_MB`, §2).
* **Per-tile coverage line** (stdout, batch ingest only; `aether_converter`
  ≥ 0.2.7, changelog 33). After each tile that was written:

  ```
  [Tile] <output file name> covered=<covered_px>/<total_px>
  ```

  `<output file name>` is the last component of the job's `output_path`
  (lossy UTF-8; it may contain spaces — split on the **last** ` covered=`),
  `<covered_px>` the output pixels a source supplied **before** `void_fill_m`
  is applied, `<total_px>` = `size_px²`. `covered=0/…` is an all-void tile
  (or an all-`void_fill_m` one); `covered=<t>/<t>` a full one. Lines come in
  completion order (tiles run in parallel), each written and flushed together
  with the `[Rust] Progress:` line it precedes, so every tile counted by a
  progress line has had its `[Tile]` line. A tile that failed prints its
  `[Error] Failed to process tile:` line instead and no `[Tile]` line. The
  single-job form (a JSON object) prints none.

  > **Parsed interface (frozen).** The Waveshed QGIS plugin reads `[Tile]`
  > lines to learn per-tile coverage without opening the tiles. Treat the
  > prefix, the `covered=` key and the `<n>/<m>` integers like the
  > `[Rust] Progress:` line: changing them is a breaking change. A converter
  > older than 0.2.7 prints no `[Tile]` lines — a consumer must then treat
  > every tile's coverage as **unknown** (not as void, not as full), and
  > likewise for any tile whose line it did not see.

  **Per-tile void line** (stdout, batch ingest only; `aether_converter` ≥
  0.2.11, changelog 46; parsed). Right after each covered line, as a line
  of its own (the covered line still ends after `<covered_px>/<total_px>`):

  ```
  [Tile] <output file name> voids=<void_px>
  ```

  — the same name (split on the **last** ` voids=`), and `<void_px>` the
  samples of the written tile **at or below -5000 half-metres (-2500 m)**
  — the `-9999` sentinel and anything else that is no ground (the floor
  the samplers apply: a source sample is ground only above it) — counted
  **after** `void_fill_m` and the building burns: for a tile without
  buildings it is `<total_px> − <covered_px>` without a fill, 0 with a fill
  above -2500 m and `<total_px> − <covered_px>` with one at or below it; a
  burn can lower it (a roof resolved on terrain lifts the void samples it
  covers). Both lines are written and flushed together, before the
  `[Rust] Progress:` line; a failed tile prints neither, the single-job
  form neither. Same freeze as the covered line: the prefix, the `voids=`
  key and the integer. A converter older than 0.2.11 prints no voids line
  — a consumer must then find a tile's voids itself (or treat them as
  unknown), and likewise for any tile whose voids line it did not see.
  Since 0.2.12 the line also goes to `AETHER_DIAG_FILE` when that is set
  (changelog 48), the covered line does not.
* **Tile files are replaced atomically (`aether_converter` ≥ 0.2.7,
  changelog 34).** `ingest` writes each `.abt` to `<output_path>.tmp-<pid>`
  in the same directory (`<pid>` = the converter's process id), then
  `flush`, `sync_data`, close, and renames it over `output_path`. Any failure
  on the way — a full disk, an I/O error, a failed final flush — removes the
  temp file and fails the tile (`[Error] Failed to process tile: …`); it
  never leaves a truncated file under the final name. An existing tile is
  **replaced by a new inode, never truncated and rewritten in place**, so
  other names hardlinked to the old tile (e.g. views of a tile pool) keep
  the old bytes. Consumers that scan output directories should ignore (and
  may delete, once no converter runs) `*.abt.tmp-*` files left by a killed
  converter. The `.abt` bytes are unchanged (§6). Note for Windows callers:
  a tile another process holds open without delete sharing makes the rename
  fail, which fails the tile and keeps the old file — previously the
  truncate-in-place failed the same way.
* **Ingest failure modes (v2.0, fail-loudly).** A job file that parses as
  neither a single job nor a job array now **fails with the parse error**
  (it used to exit 0 having done nothing). A listed terrain source that cannot
  be loaded fails the tile (and thus the run) instead of warning and writing a
  terrain-less tile. CRS/georeferencing problems are hard errors naming the
  file and the fix (§9a). In a batch each failed tile prints
  `[Error] Failed to process tile: <message>` on stdout with the **full
  cause chain**, `: `-joined outermost first (changelog 23), and the run
  ends with `<n> of <total> tiles failed to convert`. A batch whose
  building-tile hand-over fails (`buildings_pbf_ready`, changelog 48: the
  marker did not come, or the directory did not decode; `buildings_pbf_list`,
  changelog 51: the list did not come, does not parse, or names a tile the
  directory does not hold) ends with that error instead and prints no line
  for the tiles it dropped, none of which was written (§9a).
* **An ingest that covered nothing: batch warns and exits 0; a single job
  still exits non-zero (changelog item 18, 2026-08-31).** A **batch** whose
  tiles took not one pixel from any source keeps its tiles — void, or
  `void_fill_m` where the job sets it — prints the frozen
  `[Warn] NO-DATA batch:` line and exits 0: ground outside a bounded source
  degrades to sea, loudly, never an error. A **single job** (a JSON object,
  not an array) that covers nothing still fails with
  `no source covered any pixel of …` — one tile with one source set is a
  deliberate request for exactly that ground, so covering none of it is
  treated as caller error (usually no overlap, or the wrong CRS). Coverage is
  counted **before** `void_fill_m` is applied, so filling the holes with 0 m
  does not disguise it, and for a batch it is judged **over the whole run**,
  never per tile: the edge tiles of any area legitimately fall outside the
  sources, and those are still written.

  > **Consumer note.** A caller that treats exit 0 as "terrain exists" must,
  > for batches, also match the `[Warn] NO-DATA batch:` prefix (item 18) —
  > exit 0 now includes the all-void case. Single-job callers keep the old
  > behavior: pre-creating empty tiles deliberately needs `void_fill_m`
  > **and** at least one overlapping source, or handling the non-zero exit.
* **`plan` (new in v2.0, additive).** Enumerates, without downloading or
  converting anything, exactly the `.abt` tiles an area/resolution request
  produces: one JSON document on stdout with `schema: "aether-plan/1"`,
  `tile_count`, `total_bytes`, and per-tile `resolution_m`, `filename`
  (`tile_N{lat:.2}E{lon:.2}_{res}m.abt`), `ul_lat`, `ul_lon`, `size_px`,
  `exact_res_m`, `est_bytes`. Schema: `schemas/plan_output.schema.json`.
  The geometry reproduces the Waveshed plugin's Python tile enumeration
  **exactly** (extent ladder, u16-stride guard, outward snap on the finest
  sub-tile grid, 6-decimal stepping); the plugin cross-checks its own
  enumeration against this output and aborts on mismatch. Since 0.2.9 the
  snap tolerates 1e-9 grid steps, its edges are rounded to 9 decimals and
  each resolution's start tolerates 1e-9 steps too (changelog 43 gives the
  formulas) — a cross-checking consumer gates on that version. Validation is
  strict: non-finite coordinates, `south >= north`, `west >= east`, a bbox
  reaching outside `[-180, 180] x [-90, 90]` (`longitude out of range` /
  `latitude out of range` — the grid wraps nowhere, so an antimeridian
  crossing is refused rather than planned), an empty or non-positive
  resolution list, and requests over 2,000,000 tiles (`bbox too large`) all
  fail with a non-zero exit.
* **Download progress** (stderr):
  `[Download] <pct>% (<done>/<total>) — <MB/s>, <errors> errors, <in-flight> in-flight, window <w>`,
  printed whenever `<pct>` (= `done * 100 / total`, rounded down) reaches a
  new whole percent, and once more at `done == total` (changelog item 29;
  it used to be every 10 %). Parse any `<pct>`, never a fixed step.
  `, window <w>` — the connection window at that moment — is a **trailing
  field** added by changelog 30 (CLI `download`; the in-memory downloader's
  line still ends at `in-flight`): parse the prefix
  `[Download] <pct>% (<done>/<total>)` and treat the later fields as
  optional, as the Waveshed plugin does.

  `<total>` is the number of source tiles the run **fetches**, announced
  once before any request as the 4th number of the pass header
  `[Download] z=<z> tiles=<nx>x<ny>=<total> (~<mb>MB) connections=<c>`.
  Since changelog 30 that is the tiles that touch at least one requested
  output tile (`tiles[]`, §9b) — no longer always `<nx> × <ny>`, the
  bounding rectangle, which still fixes the assembly grid. `[Stats] Tiles:`
  `<total>` and the majority rule below count the same tiles. When fewer
  than `<nx> × <ny>` are fetched, an informational line follows the header:
  `[Download] fetching <total> of the <nx·ny> source tiles in the bounding
  rectangle; the other <m> touch no requested output tile` (one line). A
  skipped tile is one no output pixel samples, so `.abt` bytes are
  identical to fetching the whole rectangle.

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

  `<ok>` counts tiles that arrived, plus tiles that answered HTTP 404 and were
  repaired from a coarser zoom. It does **not** count a **degraded** tile — one
  whose fetch failed for any other reason and was then filled from a coarser
  zoom (changelog item 24); those are tallied in `[Stats] ERRORS` by class,
  named one per line, and summarised (stderr, frozen, parsed by the Waveshed
  plugin):

  ```
  [Degraded] z=<z> x=<x> y=<y> reason=<timeout|connect|http_<code>|decode|other> filled_from=z<k>
  [Degraded] … <n> more not listed
  [Stats] DEGRADED: <n> of <total> source tile(s) filled from coarser zooms after a failed fetch
  ```

  One `[Degraded]` line per degraded source tile, printed as it completes
  (interleaved with `[Download]` progress), at most 20000; beyond that one
  `… <n> more not listed` line in the summary. `k` is the coarsest zoom that
  supplied at least one of the tile's pixels. The `[Stats] DEGRADED:` line
  appears only when `n > 0`, after `[Stats] Repaired from coarser parent
  tiles:` (which counts every repair, degraded or not). A consumer that caches
  output as complete must not do so for a run that printed it — or must cache
  it as degraded. Degraded tiles do not count toward the fatal majority below.

  `[Stats] Retries: <n> (up to <m> per failed tile)` — `m` is 5 for the CLI
  `download`, 3 for the native in-memory downloader. What is retried is
  decided by the error's **type**, never its text: every transport failure
  (connect, reset, connect/read-idle timeout, truncated body), HTTP 429 and
  5xx retry; any other status and any decode/geometry error is final. The
  backoff is 1, 2, 4, 8, 8 s, or the server's `Retry-After` (delta-seconds,
  capped at 120 s) when that is longer. Native clients use a 10 s connect
  timeout and a 15 s **read-idle** timeout — no total per-request timeout, so a
  body that keeps arriving is never cut off. Each tile's parent backfill is
  bounded by one 30 s deadline (all ancestor zooms requested concurrently), so
  a silent server costs a tile at most `6 × 15 s + 23 s + 30 s` = 143 s.
  Since changelog item 28 the CLI `download` also caps each source tile at
  one deadline **across all attempts, back-offs and `Retry-After` waits** —
  **20 s** since changelog 30 (60 s before; job field `tile_deadline_s`,
  §9b): a tile still unfinished then — silent, trickling with gaps under
  15 s, or still answering 429/503 — stops as a `timeout` failure and goes
  to the parent fill (a `[Degraded] … reason=timeout` tile when an ancestor
  fills it, which a consumer's repair job refetches), so its worst case is
  20 + 30 = 50 s. A tile keeps its connection slot through its retries and
  back-offs and hands it back before the parent fill. Each ancestor tile is
  fetched at most once per run and shared by every descendant that needs it.

  Additive lines (CLI `download`, changelog item 27; not frozen in wording —
  match the prefix):

  ```
  [Download] STALL tile z=<z>/x=<x>/y=<y>: first byte after <s>s, longest body gap <s>s
  [Download] (suppressing further stall warnings; [Stats] STALL counts them all)
  [Stats] STALL: <n> of <total> tile(s) waited > 5 s for the first byte or between body chunks
  [Stats] Connection window: start <max>, min <m>, final <f> (adaptive <lo>..=<max>, <k> back-off(s))
  ```

  A STALL tile is one with an attempt that waited more than 5 s for its
  response head or between two body chunks; at most 5 per-tile lines, then
  the suppression note. The `[Stats] STALL:` line follows `[Stats] Tile
  latency:` and appears only when `n > 0`. The window line follows it and is
  always printed by the CLI `download` (see `max_connections`, §9b). They
  replace the undocumented `[Download] SLOW tile …` line (wall time > 5 s),
  which flagged nearly every tile on any link where the connections outnumber
  the bandwidth and is no longer printed.

  The connection window since changelog 30 (the line's format is unchanged):
  `<max>` = `max_connections` (default 256) concurrent requests; the window
  is cut by 25 % in a second only when **≥ 3 responses in that second were
  HTTP 429 or 503**, never below `<lo>` = max(`<max>`/4, 16) (at most
  `<max>`), and grows by 10 % per second without such a burst back to
  `<max>`. Timeouts, connection resets and slow bodies never move it. A cut
  only gates new requests — nothing in flight is withdrawn or "owed" — and
  `<k>` counts the cuts. (The AIMD window of changelog 27e halved on any
  timeout/reset/429/503 burst or a slow median body and collapsed a
  250 Mbit/s link to a trickle.)

  Additive lines (CLI `download`, changelog 30, 39, 52; informational, match
  the prefix):

  ```
  [Download] RAM budget: <mb> MB, strip rows: <r> (<a> GB available at start; fixed for the run) (strips <s> MB + assembly <k> × <g> MB + <c> connections <n> MB[ + cache writes <w> MB] + decoding <d> MB, +<p> % allocator, +<b> MB base[; buildings post-pass <x> MB|; buildings post-pass not counted, its tiles are not in yet])
  [Download] RAM budget: <mb> MB, strip rows: <r> (re-sized for <e> px source tiles)
  [Download] connections <want> → <c>: <share> MB available for this pass; the assembly (<k> × <g> MB), decoding (<d> MB)[, cache writes (<w> MB)] and two tile-rows of strips (<f> MB) leave room for <c> connections (<x.xx> MB each, +<p> % allocator)
  [Download] connections <want> → <c>: <share> MB available for this pass; the assembly (…), decoding (…)[, cache writes (…)] and two tile-rows of strips (…) leave no room for connections: the pass runs <c> and takes <mb> MB, over its share
  ```

  **Since 0.2.15 (changelog 52) `<mb>` is the whole process's budget** — what
  the pass peaks at when its strips fill, not the strips' share alone, which
  is all builds before 0.2.15 priced (their passes peaked at up to 2.4× the
  printed figure). The share is unchanged — min(available RAM / 4, 2 GiB),
  at least 256 MiB, taken once after the first tile — and pays first, in
  live bytes: `<k>` assemblies at once (2, or 1 when the job is one strip),
  each `<g>` = a tile-row of grid (`<nx>` × edge² × 4 B) plus the output rows
  of the fullest tile-row (`size_px` × 2 B + 48 B each) and the outputs'
  column tables; `<c>` connections × (64 KiB + edge² × 2 B) — only as many
  as tiles the network serves (a raw-tile-cache hit or derived tile takes
  none); `<w>` = 96 MB for a raw tile cache that writes (its 64 MiB queue
  and a pack's buffer); `<d>` = the runtime's workers × edge² × 4 B × 4 (× 16
  when the cache derives tiles); then `<p>` % of all that for the allocator
  (12 without a raw tile cache, 20 with one; the Windows binary, whose
  allocator is mimalloc, 40 and 55) and `<b>` = 24 MB base. The strips get
  the rest (`<s>`, never more than the job's tiles; the parent fill's
  decoded coarser tiles are counted in it — a strip waits while they take
  its room — and keep at most a quarter of it); strip rows =
  clamp(`<s>` / 2 / (`<nx>` × edge² × 4 B), 1, 32) as before. A pass whose
  strips cannot fill — slower input than assembly: a slow link, a raw tile
  cache on a slow disk — peaks below `<mb>`. A job with a building source
  adds its buildings post-pass, which runs after the terrain: where its
  tiles are there when the budget is drawn (a `buildings_pbf_dir` without
  `buildings_pbf_ready`, or whose marker or `buildings_pbf_list` is in
  place), `<x>` = its set estimated from the tiles' bytes on disk × 3
  (metadata only, as for a batch, §1.2 *Batch pool lines*), held × 1.4 at
  the decode's peak, plus the largest output tile, which it reads whole,
  priced like the rest (`<p>` %, `<b>`); `<mb>` is then the larger of the
  two phases. Where its marker or list is still to come, it is not
  counted, and the line says so. Where the rest is less than two
  tile-rows of strips (`<f>`, or the whole job when smaller), the pass runs
  fewer connections, `<c>` = the most that leave `<f>`, at least 16 (fewer
  only when the job asks for fewer), says so once in the third line —
  before the budget line, which then prices `<c>` — and its connection
  window runs between `max(<c> / 4, 16)` and `<c>` (the `[Stats] Connection
  window:` line's start and ceiling are `<c>`); where even 16 leave no room,
  the fourth line, and the strips get `<f>` all the same: a pass never fails
  for memory, and `<mb>` then exceeds the share. Byte-identical either way.
  A job's fixed strip budget for tests (`DownloadTuning::ram_budget`)
  replaces `<s>` and never shrinks the connections. `<share>` is in MB;
  `<x.xx>` the MB a connection takes, two decimals. A test may impose the
  available RAM (`AETHER_TEST_AVAILABLE_MB`, §2).

  The first replaces `[Download] RAM: <a> GB available, ~<m> MB/strip,
  prefetch depth: <d>`; the second appears at most once, when the source
  tile edge `<e>` is learnt only after the first strip was sized for 512 px
  (its first tile failed or was out for more than 5 s) and that changes the
  strip rows. Builds before 0.2.9 (changelog 39) re-read the budget before
  every strip and printed `(<a> GB available; re-evaluated before every
  strip)` and, on every change of the rows, `(<a> GB available)`. Decoded
  source tiles (`edge² × 4` bytes each) are held from arrival until the
  assembly has copied their tile-row into its grid, so the run budgets them:
  the strips' budget — before 0.2.15 all of min(available RAM / 4, 2 GiB),
  at least 256 MiB; since, `<s>` above — taken once —
  after the run's first tile, before any strip is sized — and fixed for the
  run; strip rows = clamp(budget / 2 / (`<nx>` × edge² × 4 B), 1, 32), so
  two strips fit (edge = that of the run's first tile, fetched alone before
  any strip is sized — changelog 36; 512 only while that tile has failed or
  is out for more than 5 s); a strip starts only when its bytes plus those
  launched strips still hold (since 0.2.15 plus the parent fill's decoded
  tiles) fit the budget (or no strip holds any), and
  only once the strips in flight have fewer unstarted tiles than the
  connection window. A strip's bytes leave the count tile-row by tile-row as
  its assembly drops each row's tiles (shares by tile count, changelog 39).
  Each output row samples exactly one tile-row, so strip size never changes
  a byte. `[Download] <s> strips, <r> tile-rows/strip, …` reports the
  initial plan; `[Strip i/n]` lines count against the current estimate
  (they are diagnostics and go to `AETHER_DIAG_FILE` when it is set —
  changelog 36, 37). So does, once per run after the last assembly
  (changelog 39), `[Download] launch gate closed for <s> s (assembly behind
  the network)`: the time the run waited with no strip downloading because
  the next one did not fit the budget.
* **Backfill line** (stderr, CLI `download` full runs and repair jobs;
  `aether_converter` ≥ 0.2.13, changelog 49; **informational, not parsed**):

  ```
  [Stats] Backfill: <n> px in <m> tile(s) filled from coarser zooms (<k> px seam samples)[; <l> px in <t> tile(s) left void (<s> px seam samples)]
  ```

  In the `[Stats]` block after every line older builds print there but the
  `[Stats] Cache…` lines, which stay last; omitted when no source tile that
  arrived had a void sample. `<n>` = the void samples — the service's own (at
  or below −11000 m) and those the seam filter made (§9b *Seam filter*) — of
  the source tiles that arrived (fetched, from the raw tile cache or derived)
  that a coarser zoom filled, `<m>` = those tiles, `<k>` = how many of the
  `<n>` were seam samples. The clause after `;`, only when `<l>` > 0, counts
  the void samples no coarser zoom had: they stay void, as before, and the
  `[Tile] … voids=` lines count them in the output. A source tile that never
  arrived and was filled whole is counted by `[Stats] Repaired from coarser
  parent tiles:` as before, not here. Like the whole block, it is kept in
  `AETHER_DIAG_FILE` too. With `AETHER_DIAG_FILE` set, each such tile also
  gets one diagnostic line **in the file only** — never on the console, which
  a GUI shows its users:

  ```
  [Backfill] z=<z> x=<x> y=<y>: <v> void px (<s> seam), <f> filled (z<a> <n>, z<b> <n>, …), <l> left void
  ```

  (the zooms that filled, finest first; at most 20,000 lines per run, then
  `[Backfill] … <n> more tile(s) not listed`; not parsed, wording may
  change). A converter before 0.2.13 prints neither and fills in-tile voids
  without a word.
* **Peak RSS line** (stderr, CLI `download` full runs and repair jobs;
  `aether_converter` ≥ 0.2.13, changelog 50; **informational, not
  parsed**):

  ```
  [Stats] Peak RSS: <mb> MB (RAM budget <m> MB)
  [Stats] Peak RSS: <mb> MB
  ```

  Once per pass, in the `[Stats]` block — on the fatal path too, before
  `Tile download failed:`; a run that fails before its block (a write
  error, a building-tile marker that never came) prints neither, as
  before — after every line older builds print there (and after `[Stats]
  Backfill:`) but the `[Stats] Cache…` lines, which stay last. `<mb>` = the converter process's peak resident memory so far,
  MB (2^20 bytes) with one decimal: on Linux `VmHWM` of
  `/proc/self/status` — not `getrusage`'s `ru_maxrss`, which on Linux
  includes the peak of the process that started the converter (changelog
  50) —, on macOS `getrusage` `ru_maxrss`, on Windows
  `GetProcessMemoryInfo` `PeakWorkingSetSize`. `<m>` = the run's RAM
  budget exactly as its `[Download] RAM budget: <m> MB` line printed it; a
  repair job, which has no budget, prints the second form. Since 0.2.15
  (changelog 52) that budget is the whole process's, so the two figures
  compare like for like. No line where
  the OS does not give the peak — never `0`. Like the whole block, it is
  kept in `AETHER_DIAG_FILE` too. The in-memory and browser downloaders
  print none (they run inside another program's process). The same peak
  ends `ingest`'s `[Rust] run:` line (above). A converter before 0.2.13
  prints neither.
* **Per-tile void line** (stderr, CLI `download` full runs; `aether_converter`
  ≥ 0.2.11, changelog 46; parsed). Once every output is finished — the strip
  assembly, the tail fill and the buildings post-pass (§9b) done — and
  before the `[Stats]` block, one line per output tile, in the order of the
  job's `tiles`, written and flushed together:

  ```
  [Tile] <output file name> voids=<void_px>
  ```

  `<output file name>` is the last component of `output_dir`/`filename` —
  for the plain `filename` consumers pass, `filename` itself (lossy UTF-8;
  split on the **last** ` voids=`); `<void_px>` the samples of the written
  file **at or below -5000 half-metres (-2500 m)**: the `-9999` void
  sentinel (§6) and anything else a consumer must not read as ground — the
  source's bathymetry included (Terrarium carries it down to -11000 m;
  Mapbox's lowest value is -10000 m). `0` for a tile every pixel of which
  is ground above -2500 m, `size_px²` for one no tile reached. It describes
  the file as the run leaves it: the buildings post-pass can lower it, as a
  roof resolved on terrain lifts the void samples it covers (a roof over
  void ground stays below the floor). Counted while the rows are written,
  never by reading a file back. A run that fails prints none — it
  removes its outputs (below); a repair job (§9b) prints none, nor do the
  in-memory and browser downloaders. Same freeze as the ingest `[Tile]`
  lines (prefix, `voids=` key, the integer). A converter older than 0.2.11
  prints none: count the voids from the file, or treat them as unknown.
  Since 0.2.12 the lines also go to `AETHER_DIAG_FILE` when that is set
  (changelog 48).
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

* **Tile-fetch failure.** When **more than half** of the requested XYZ
  terrain tiles **genuinely failed to fetch** — timeout, connect, 403, 429,
  other 4xx, 5xx, decode, other; **HTTP 404 is excluded** (changelog item
  17: a 404 is the service answering "no tile here", tallied as
  `HTTP_404_no_data` and reported on its own `[Stats] NO-DATA (HTTP 404):`
  line, never fatal in any quantity) — `download` **fails**
  (`anyhow::bail!`, non-zero exit) after emitting the usual `[Stats]` block.
  The emitted string (`crates/aether_converter/src/download.rs`, guarded by
  `fetch_failure_is_fatal` over `DownloadStats::fatal_errors()`) is:

  ```rust
  anyhow::bail!(
      "Tile download failed: {} of {} terrain tiles ({}%) could not be fetched; \
       the output would be mostly void, not terrain. Errors: {}.{} Check that \
       the tile source serves zoom {} over this area and that the network is \
       reachable.",
      failed, attempted, pct, stats.error_breakdown(),
      /* when 404s also occurred: */
      " (A further {n} tile(s) answered HTTP 404 — no data there — which alone \
       would not fail the run.)",
      job.zoom
  );
  ```

  `Errors:` names the **failure class**. It renders the same per-class tally as
  the `[Stats] ERRORS` line — one builder, `DownloadStats::error_breakdown()` —
  so the two can never disagree: `timeout`, `connect`,
  `HTTP_429_rate_limited`, `HTTP_404_no_data`, `HTTP_4xx`, `HTTP_5xx`,
  `decode`, `other`, in that order, only the non-zero ones, comma-separated.
  A real message reads (one line, wrapped here):

  ```
  Tile download failed: 2 of 2 terrain tiles (100%) could not be fetched; the
  output would be mostly void, not terrain. Errors: decode=2. Check that the
  tile source serves zoom 12 over this area and that the network is reachable.
  ```

  Without it, "could not be fetched" reads as a network fault for every cause.
  It is not one: a source that serves bytes the decoder cannot read (JPEG, a
  16-bit PNG, an HTML error page) fails every tile with `decode=N`, and the
  repair is a different tile source, not a different network. (PNG and WebP
  both decode since item 25; a WebP source was such a failure before it.)

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
  > treated any non-zero exit as fatal need no change. Sparse *transport*
  > failures remain non-fatal — the threshold is deliberately a *majority*,
  > and a minority loss is still only reported through `[Stats] ERRORS`.
  > Match `Tile download failed:` if you need to distinguish this from the
  > disk-space failure.

  > **Behavior change for consumers (changelog item 17, 2026-08-31).** 404s
  > no longer count toward the fatal majority at all: a run whose only losses
  > are 404s exits 0 — however many, all of them included — and keeps its
  > output, void where no tile exists. Alongside `[Stats] ERRORS` the summary
  > then carries a machine-readable line (frozen prefix, parsed by the
  > Waveshed plugin):
  >
  > ```
  > [Stats] NO-DATA (HTTP 404): <n> of <total> tile(s) — the service has no
  > data there; written as void
  > ```
  >
  > (one line; wrapped here). Match `NO-DATA (HTTP 404): <n>` to learn how
  > much of the request had no data. A consumer that fills voids
  > (`void_fill_m`) reads those areas as its fill value — Waveshed Site
  > Analysis fills 0 m sea level and warns the user; a consumer that keeps
  > voids (the Map Converter) sees the `-9999` sentinel. Consumers that
  > relied on a majority-404 run aborting must switch to this line.

  > **Behaviour change for consumers — a lost tile is repaired or a hole,
  > never sea level.** A tile that never arrives is first offered to the
  > same parent backfill that repairs in-tile voids (changelog item 16): the
  > first coarser ancestor that decodes supplies all of its pixels,
  > upsampled. A tile that answered 404 then counts as OK; one that failed
  > for any other reason is **degraded** (changelog item 24 — named on a
  > `[Degraded]` line, not OK, not fatal); both are counted by `[Stats]
  > Repaired from coarser parent tiles: N`. Only a tile with **no usable ancestor at any
  > zoom** stays a hole — and a hole is now the **`-9999` void sentinel** of
  > §6, the same value `ingest` writes for a pixel no source covered, never
  > **0 m** as before (a valid sea-level elevation, indistinguishable
  > downstream from surveyed ground, which is how a partly-404'd run
  > produced confident flat ocean). The same sentinel applies to any output
  > pixel whose footprint falls outside the fetched tile grid at all.
  > **`.abt` bytes therefore change for every run that lost a tile**; a
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

### 1.2a `aether_converter probe` (raw GeoTIFF header facts, `aether-probe/1`)

Additive, `aether_converter` ≥ 0.2.7 (changelog 32, `last_chunk_end` 35; gate on `--version`,
§1.5 — older builds reject the sub-command with clap's exit `2`).

```
aether_converter probe --list <file> [--jobs N]     # one path per line, UTF-8
aether_converter probe --stdin [--jobs N]           # the same list on stdin
```

Reads the **header** of every listed file — the first IFD, and the IFD chain
for the overview count — with a few positioned 16 KB reads through the
ingest block reader; pixel data is never read and of the strip/tile offset
tables only the last block's entry (`last_chunk_end`, changelog 35); nothing
is decoded, reprojected or footprinted (the caller does its own CRS work). Files are probed in parallel by `--jobs` workers (default
`min(64, 4 × CPUs)`; each opens, reads and closes its file).

* **Input.** One path per line; `\n` or `\r\n`; blank lines skipped; a
  leading UTF-8 BOM dropped; nothing else trimmed. A list that is not UTF-8
  or cannot be read fails the run (non-zero exit, nothing on stdout).
* **stdout:** exactly one JSON object per listed path, one per line (JSON
  Lines), **in list order**: a line is written once every earlier line is
  out, and stdout is flushed whenever no further result is ready, so a
  reader sees lines progressively. Schema `schemas/probe_output.schema.json`;
  fixture `fixtures/outputs/probe_output.jsonl`.
* **stderr:** one final line,
  `[Probe] <n> files, <ok> ok, <s.ss>s, open <ms> ms avg, read <ms> ms avg`
  (`open` = the open call, `read` = stat + header reads, both averaged over
  all `n` files, two decimals). Diagnostic, not frozen.
* **Exit code** 0 whenever the list was read and stdout written, whatever
  the files were: an unreadable or non-TIFF file is an `ok: false` line,
  never an error.

Every line carries `schema: "aether-probe/1"`, `path` (the list line as
given), `ok`, and `size` / `mtime` / `mtime_ns` (`null` when the file cannot
be stat'ed). `mtime` is exactly Python's `int(os.stat(p).st_mtime)` and
`mtime_ns` exactly `os.stat(p).st_mtime_ns`, so either fingerprint the
Waveshed plugin keeps (`size:mtime`, `size:mtime_ns`) can be compared without
a second stat. An `ok: false` line adds only `reason` and carries **no**
header fields. An `ok: true` line carries all of:

| Field | Type | Meaning |
|-------|------|---------|
| `width`, `height` | int | ImageWidth / ImageLength. |
| `bands` | int | SamplesPerPixel — always `1` on an `ok: true` line. |
| `dtype` | string | `u8 i8 u16 i16 u32 i32 u64 i64 f32 f64` (SampleFormat × BitsPerSample; `i8` = GDAL < 3.7's Byte + `PIXELTYPE=SIGNEDBYTE`). |
| `compression` | `{code, name}` | Compression tag (`1` when absent) and GDAL's name for it (`NONE LZW DEFLATE PACKBITS JPEG ZSTD WEBP LERC LZMA JXL …`, `UNKNOWN` otherwise; `LERC` whatever GDAL's `LERC_DEFLATE`/`LERC_ZSTD`). Not a promise that `ingest` decodes it (§9a). |
| `predictor` | int | Predictor tag; `1` when absent. |
| `tiled`, `tile_w`, `tile_h` | bool, int, int | TileWidth/TileLength; for a stripped file `tiled: false`, `tile_w` = width, `tile_h` = RowsPerStrip clamped to the height. |
| `epsg` | int \| null | Horizontal EPSG code from the GeoKeys — ProjectedCSTypeGeoKey, else GeographicTypeGeoKey (the rule `ingest` uses, §9a; a vertical code is ignored). `null` = the file carries no CRS (no GeoKeyDirectory, or one naming no coordinate system). |
| `proj` | string \| null | Reserved for a CRS the GeoKeys define without an EPSG code; always `null` in `aether-probe/1` (such files are `ok: false`). Accept a string here. |
| `geotransform` | 6 numbers | GDAL order `[x0, dx, rx, y0, ry, dy]`: ModelPixelScale (both steps non-zero) + the first ModelTiepoint, as GDAL prefers, else ModelTransformation; a negative ScaleY is read north-up (GDAL's default); a `RasterPixelIsPoint` origin is shifted to the pixel corner exactly as GDAL does (`x0 -= (dx+rx)/2`, `y0 -= (ry+dy)/2`). |
| `rotated` | bool | `rx != 0` or `ry != 0` (`ingest` refuses such sources). |
| `pixel_is_point` | bool | GTRasterTypeGeoKey = RasterPixelIsPoint (GDAL `AREA_OR_POINT=Point`). |
| `nodata` | number \| null \| `"nan"` \| `"inf"` \| `"-inf"` | GDAL_NODATA as GDAL parses it; the strings stand for the values JSON numbers cannot carry. |
| `scale`, `offset` | number \| null | Band-1 SCALE / OFFSET items of GDAL_METADATA (what `ingest` applies); `null` when absent (= 1 / 0). |
| `overviews` | int | Later IFDs flagged reduced-resolution (NewSubfileType bit 0) and not mask (bit 2), with the image's sample count — GDAL's `GetOverviewCount()` without external `.ovr`. |
| `ifd_offset` | int | Offset of the first IFD (GDAL band metadata `IFD_OFFSET`, domain `TIFF`). |
| `last_chunk_end` | int \| null | Additive (changelog 35). Where the pixel data ends: offset + byte count of the image's **last** strip/tile (index block count − 1 of TileOffsets/TileByteCounts, else StripOffsets/StripByteCounts — the bottom-right block, the one GDAL's truncation test reads; GDAL band metadata `BLOCK_OFFSET_x_y` + `BLOCK_SIZE_x_y`, domain `TIFF`). `0` when that block is sparse (byte count 0, which GDAL reads as empty). **Greater than `size` = cut off behind an intact header**: GDAL opens the file but cannot read that block. `null` = the arrays cannot be read (absent, shorter than the block count, not unsigned integers, past the end of the file) — GDAL cannot read the block either. Absent from converters built before changelog 35: then do not take the pixel data as complete. |

**`ok: true` means GDAL-equal.** Every field above equals what GDAL reports
for the same file — `tools/probe_vs_gdal.py` checks it field by field over a
GDAL-written matrix (10 dtypes × NONE/LZW/DEFLATE/PACKBITS/ZSTD × predictors
× strips/tiles × byte orders, COG and `gdaladdo` overviews, internal masks,
BigTIFF, PixelIsPoint, rotation, scale/offset, NaN nodata, compound and
geographic CRSs, LERC, JPEG, LZMA, an IFD rewritten at the end of the file).
Two documented differences: `tile_h` of a **single uncompressed strip** is
the file's RowsPerStrip, where GDAL shows libtiff's ~8 KB virtual strips;
and `scale`/`offset` are `null` where GDAL reports an explicit `1`/`0` for
a file with a vertical CRS. Sidecars (`.aux.xml`, `.ovr`, `.prj`) are not
read: a value GDAL takes from one is the caller's concern.

**`ok: false` → ask GDAL.** `reason` is human-readable and starts with one
of: `cannot open`, `cannot stat`, `not a regular file`, `not a TIFF file`
(an `.abt` says so), `corrupt TIFF`, `corrupt BigTIFF header`, `truncated
TIFF`, `read error`, `multi-band` (SamplesPerPixel > 1, alpha included),
`palette`, `sub-byte samples`, `complex samples`, `unsupported sample type`
(e.g. 12-bit or Float16), `photometric interpretation` (anything but
BlackIsZero), `no geotransform`, `GCP georeferencing`, `non-finite
geotransform`, `user-defined CRS` (ProjectedCSType/GeographicType 32767 or
0, or CRS parameter keys without a code), `GDAL_NODATA` / `GDAL_METADATA`
(an unparseable value). The caller falls back to GDAL for exactly those
files; the list of prefixes may grow (additively).

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
| `aether_core` | **yes** — `#[command(author, version, about)]` | Prints `aether_core <CARGO_PKG_VERSION>` and exits `0`. `-V` also works. |
| `aether_converter` | **yes** (since 0.2.5) | Prints `aether_converter <CARGO_PKG_VERSION>`, exit `0`. Older builds: unknown argument, exit `2`. |
| `aether_export` | **yes** (since 1.0.1) | Prints `aether_export <CARGO_PKG_VERSION>`, exit `0`. Older builds: unknown argument, exit `2`. |
| `aether_aggregate` | **yes** (since 1.0.1) | Prints `aether_aggregate <CARGO_PKG_VERSION>`, exit `0`. Older builds: unknown argument, exit `2`. |

Every binary therefore answers `--version` with `<name> <semver>` on stdout
and exit `0` (pinned by each crate's `tests/test_version.rs`). A consumer
that sees exit `2` / "unexpected argument" is talking to a build older than
the versions above and must treat its capabilities as the oldest known set.

Known capability gates consumers rely on:

| Capability | Minimum version |
|------------|-----------------|
| `aether_core` coverage at resolutions outside `{2, 5, 10, 30}` m on the GPU backend (the hardcoded list was removed; the CPU/WASM paths never had it) | `aether_core 0.4.3` |
| `aether_core --fingerprint` | `aether_core 0.4.2` |
| `download` job fields `repair_source_tiles` and `tile_deadline_s` (§9b; older builds ignore both silently — a repair job would run as a full download over the existing files) | `aether_converter 0.2.6` |
| `probe` subcommand (§1.2a; older builds exit `2`, unknown sub-command) | `aether_converter 0.2.7` |
| `[Tile] <name> covered=<c>/<t>` lines in batch `ingest` (§1.2; absent before → coverage unknown) | `aether_converter 0.2.7` |
| Atomic tile replacement in `ingest` (temp file + rename; §1.2, §6) | `aether_converter 0.2.7` |
| `AETHER_DIAG_FILE` — `ingest`'s `[Rust] tile` and `download`'s `[Strip …]` diagnostic lines to a file (§1.2; older builds ignore the variable and keep printing them on the console) | `aether_converter 0.2.8` |
| `download` job field `cache_dir` (§9b; older builds ignore it — nothing is cached and every job fetches everything) | `aether_converter 0.2.8` |
| `plan` grid of changelog 43 — 1e-9-step snap, edges rounded to 9 decimals, tolerant start per resolution (§1.2; a consumer that cross-checks `plan` must enumerate this way for this build and later and the old way for older ones, or its check aborts) | `aether_converter 0.2.9` |
| `[Rust] Decoding building tiles: <n>/<m>` and `[Rust] Building tiles decoded: …` lines while a batch `ingest` decodes its `buildings_pbf_dir` (§9a, changelog 44; older builds print nothing during the decode and decode one file after the other) | `aether_converter 0.2.9` |
| `download` job fields `buildings_pbf_ready` and `buildings_pbf_wait_s` — fetch the building tiles while the download runs (§9b, changelog 45; older builds ignore both and burn whatever `buildings_pbf_dir` holds when their terrain is done, or fail when it does not exist yet) | `aether_converter 0.2.10` |
| `[Tile] <name> voids=<n>` lines — CLI `download` full runs (stderr, one per output before the `[Stats]` block) and batch `ingest` (stdout, after each `covered=` line): the samples at or below -5000 half-metres (-2500 m: the `-9999` sentinel, bathymetry) each written tile holds (§1.2, changelog 46; older builds print none — count the voids from the files, or treat them as unknown) | `aether_converter 0.2.11` |
| `AETHER_DIAG_FILE` also keeps `download`'s `[Download] <pct>%`, `STALL`, `first …`, `[Degraded]` and `[Stats]` lines, which stay on stderr (§1.2, changelog 46; older builds keep them on stderr only) | `aether_converter 0.2.11` |
| Batch `ingest` job fields `buildings_pbf_ready`, `buildings_pbf_wait_s` and `buildings_hold_mb` — fetch the building tiles while the batch samples its terrain (§9a, changelog 48; older builds ignore all three and read `buildings_pbf_dir` at launch: they fail with `buildings_pbf_dir … is not a directory` when it does not exist yet, else burn whatever it holds then; a single job refuses `buildings_pbf_ready` from this version on) | `aether_converter 0.2.12` |
| `AETHER_DIAG_FILE` also keeps the `[Tile] <name> voids=<n>` lines of `download` (stderr) and batch `ingest` (stdout), which stay on their streams (§1.2, changelog 48; older builds keep them on their streams only) | `aether_converter 0.2.12` |
| `download` seam filter and its job field `seam_filter` — a Terrarium mosaic seam's samples become voids the parent fill fills (§9b, changelog 49; older builds ignore the field and write the seam into the `.abt` as terrain, e.g. −9,300 m at 10°E) — with the informational `[Stats] Backfill:` line and the per-tile `[Backfill]` lines in `AETHER_DIAG_FILE` (§1.2; older builds print neither) | `aether_converter 0.2.13` |
| The converter's peak resident memory: `[Stats] Peak RSS: <mb> MB (RAM budget <m> MB)` once per CLI `download` pass (a repair's without the budget), in the `[Stats]` block before the cache lines and kept in `AETHER_DIAG_FILE` with it, and `, peak rss <mb> MB` at the end of `ingest`'s `[Rust] run:` line (§1.2, changelog 50; informational, not parsed — nothing to gate; older builds print neither) | `aether_converter 0.2.13` |
| `download` and batch `ingest` job field `buildings_pbf_list` — only the building tiles a list file names are read from `buildings_pbf_dir` (the caller's store, never listed), and the list file is their ready signal (§9a, §9b, changelog 51; older builds ignore the field: they decode the whole `buildings_pbf_dir` — the store, every file — without waiting for the list: a batch at launch, a download after its terrain, a repair before its burn; slow, and a store holding two zooms or an unreadable file fails the run) | `aether_converter 0.2.14` |
| Honest memory budgets: the CLI `download`'s `[Download] RAM budget: <mb> MB` is the whole process's (its strips, assembly, connections, cache writes, decoding, the allocator's share and the base — or its buildings post-pass where that is larger and its tiles are there; a breakdown in parentheses at the end), a pass whose share cannot hold its connections runs fewer and says so (`[Download] connections <a> → <b>: …`), and a batch `ingest` counts its building sets — the `[Rust] Rayon Pool:` line's `building set ~<b> MB`, `, <mb> MB` at the end of `[Rust] Building tiles decoded:`, `[Rust] Tiles at once: …` (§1.2, §9a, §9b, changelog 52; informational, nothing parsed — nothing to gate; older builds price the strips alone and print none of the additions) | `aether_converter 0.2.15` |

---

## 2. Environment variables

| Variable | Read by | Purpose |
|----------|---------|---------|
| `AETHER_LICENSE` | `aether_core` (proprietary build) | Base58 license/API key — **v1 or v2** (§10). Proprietary builds **refuse to run without a valid, unexpired key**. If unset, the engine falls back to reading a `license.key` file (see below). |
| `RUST_LOG` | `aether_core`, toolkit CLIs | `env_logger` level filter. **Must be `info` (or lower)** for the log-line progress diagnostics of §1.1(b) to appear. Markers in §1.1(a) do not depend on it. |
| `AETHER_DIAG_FILE` | `aether_converter` (`ingest`, `download`) | Path of a file the per-tile `[Rust] tile …` and per-strip `[Strip …]`/`[StripMem …]` diagnostic lines (and, ≥ 0.2.9, `download`'s once-per-run `[Download] launch gate closed for …`, changelog 39) are appended to instead of the console (§1.2, changelog 37; `aether_converter` ≥ 0.2.8, older builds ignore it). Unset or empty = console as before. Parsed lines and `[Rust] run:` never move; since 0.2.11 (changelog 46) `download`'s `[Download] <pct>%`, `STALL`, `first …`, `[Degraded]` and `[Stats]` lines are copied into the file as well, staying on stderr unchanged. Since 0.2.13 (changelog 49) a download also writes one `[Backfill] …` line per source tile whose void samples the parent fill filled to the file **only** (never to the console); the block's `[Stats] Peak RSS:` line (≥ 0.2.13, changelog 50) is copied like every `[Stats]` line. |
| `AETHER_TEST_AVAILABLE_MB` | `aether_converter` (`download`, batch `ingest`) | **Tests only** — no consumer sets it, and it may change or go without notice (changelog 52, ≥ 0.2.15): a whole number of MB (2^20 bytes) used in place of the OS's available-memory figure by the CLI `download`'s RAM budget and the batch `ingest`'s pool sizing, so a test can run a memory-short pass on any machine. Unset, empty or not a whole number: the OS's figure, as before. |
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
| `range_profile` | array? | `null` | Variable radial step for coverage (`SINGLE`) jobs — see below. Added engine 0.4.9 (additive). |

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

**`range_profile`** (`range_grid.rs`) — optional list of up to 8 bands
`{"until_m": <metres>, "step_m": <metres>}` in ascending `until_m`. Without it the
radial step is uniform (`resolution_m`) and the output is byte-identical to engines
that predate the field. With it each radial stays one continuous ray from the
transmitter (far bands keep the horizon the fine near field produced) but is sampled
at each band's own step; `resolution_m` keeps meaning the **output pixel size**.
Normalisation: the last band is clipped/extended to `max_range_km`; each band holds a
whole number of steps (`n = ceil(len/step)`, actual step `len/n` ≤ requested), the
GPU's 64-step padding goes into the last band; the radial count is
`ceil(2π·max_k(end_k/step_k))` (arc spacing ≤ each band's requested step at its outer
edge). Output pixels read the **nearest** range sample (the uniform engine floors).
Native GPU and CPU engines honour it, PATH too (per waypoint, the last band follows
the waypoint's range); P2P/BATCH_P2P and the WASM build ignore it. A malformed profile (empty, > 8 bands, non-ascending `until_m`,
`step_m` < 0.1) fails the job with an error.

**Mixed-resolution terrain** (what profile jobs rely on): `aether_core` keeps **every**
`.abt` tile group in `terrain_dir` with a distinct (UL corner, extent) and lets finer
tiles overwrite coarser ones where they overlap; within one group (same corner and
extent) it picks the tile closest to `resolution_m`. Tile extents must nest (each
extent an integer multiple of the smallest). The CPU engine, which otherwise skips
tiles more than 50 % off `resolution_m`, accepts tiles near any band's step when a
profile is set and samples the finest first.

**`compute_backend`** (`main.rs` for native; `aether_core_wasm` for the web build):

| Value | Native `aether_core` | WASM build |
|-------|----------------------|------------|
| `"AUTO"` (default / any unrecognized) | Try GPU, fall back to CPU. | Try GPU, fall back to the in-process CPU engine. |
| `"GPU"` | Force GPU. | (treated as AUTO — tries GPU, may fall back) |
| `"CPU"` | Force CPU. | Skip GPU; run the in-process CPU engine. |
| `"GPU_ONLY"` | (treated as AUTO) | **WASM-only:** try GPU; on failure **return an error** (`AETHER_GPU_UNAVAILABLE: …`) instead of the slow single-threaded CPU path, so the JS caller can orchestrate a multi-Worker CPU fallback. Matching is case-insensitive. |

> **P2P/BATCH_P2P (2026-09-01):** `"CPU"` now selects a CPU implementation for
> P2P tasks too (reference `cpu_itm`, same links, same frozen CSV, same
> single-link artifacts). Before this, P2P ignored `compute_backend` and always
> ran the GPU — a `"CPU"` job on a GPU-less machine simply failed. `"AUTO"` and
> `"GPU"` keep the GPU path.

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
* **No-signal sentinel (2026-09-01).** A link whose loss cannot be computed —
  the ITM math returned a non-finite value, or the TX antenna pattern blocks
  the bearing — reports `Path_Loss_dB` = `9999.00` (and the correspondingly
  impossible `Signal_dBm`), never `0.00`: a zero here reads as a perfect link.
  A link longer than `max_range_km` reports the same `9999.00` on the **CPU**
  backend, with one `[P2P] WARNING: link … exceeds max_range_km` line on stderr
  (the GPU shader still writes `0.00` for that case).
  LOS-mode links keep their `-1`/`0` visibility convention.

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
> Since `aether_converter` 0.2.13 (changelog 49) so is a sample of a mosaic
> seam — both row or both column neighbours more than 2,200 m higher, in a
> Terrarium tile of zoom 11 or finer (§9b *Seam filter*; `seam_filter: false`
> turns it off).

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

**How `ingest` writes a tile (≥ 0.2.7, changelog 34).** Bytes go to
`<output_path>.tmp-<pid>` beside the output, are flushed and `sync_data`'d,
and the file is renamed over `output_path` only then. A reader therefore
sees either the previous complete tile or the new complete one — never a
partial file under the final name — and a replaced tile is a **new inode**
(hardlinks to the old tile keep the old bytes). The byte layout above is
unchanged. (`download` writes its `.abt` files as before.)

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
| `output_path` | string (path) | **required** | Destination `.abt` file; its parent directory is created if missing, so the caller need not pre-create it. |
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
| `buildings_pbf_ready` | string? | optional | Additive, changelog 48, `aether_converter` ≥ 0.2.12 (older builds ignore it: gate on §1.5). **Batch only.** One plain file name inside `buildings_pbf_dir` (e.g. `"_ready"`) whose existence declares the directory complete, so the building tiles can be fetched **while** the batch samples its terrain — the download job's hand-over (§9b, changelog 45), same name rules and messages. There at launch: the directory is decoded at once, exactly as without the field. Missing at launch: see *Building tiles fetched during the batch* below. Requires `buildings_pbf_dir`; empty, `.`, `..`, a name containing `/` or `\`, or a `{z}_{x}_{y}.pbf`/`.mvt` tile name fails the batch before any tile; so do jobs naming one directory with different `buildings_pbf_ready` / `buildings_pbf_wait_s`. The single-job form (a JSON object) refuses it. Absent or `null`: unchanged. |
| `buildings_pbf_wait_s` | number? | optional | Additive, changelog 48, ≥ 0.2.12. Seconds a batch waits for `buildings_pbf_ready` — or, ≥ 0.2.14, for the `buildings_pbf_list` file (changelog 51) — counted from its first look, at launch; absent or `null` → **1800**. Must be 0 … 86400, else the batch fails before any tile: `ingest job for "<output_path>": buildings_pbf_wait_s must be a number of seconds from 0 to 86400, got <v>`. Read only together with `buildings_pbf_ready` or `buildings_pbf_list` (alone it is not even checked). |
| `buildings_hold_mb` | number? | optional | Additive, changelog 48, ≥ 0.2.12. Megabytes (MiB) of finished tiles a batch may hold in memory while it waits for `buildings_pbf_ready` — a held tile costs `size_px² × 2` bytes; a tile that does not fit waits on its worker thread. The batch's budget is the smallest value its watched jobs give, taken as given (`0` holds nothing); absent on all of them: half of the available memory the batch measures for its pool once the pool's reservation is taken off (threads × the per-tile estimate of the `[Rust] Rayon Pool:` line, plus the 192 MB chunk cache, and — ≥ 0.2.15, changelog 52 — the building sets decoded at launch), at most 4096 MB, never below one tile (the largest watched tile). Must be 0 … 1048576, else the batch fails before any tile: `ingest job for "<output_path>": buildings_hold_mb must be a number of megabytes from 0 to 1048576, got <v>`. Read only together with `buildings_pbf_ready` or (≥ 0.2.14, changelog 51) `buildings_pbf_list`, whose wait it budgets the same way. |
| `buildings_pbf_list` | string? | optional | Additive, changelog 51, `aether_converter` ≥ 0.2.14 (older builds ignore it and decode the whole `buildings_pbf_dir` at launch, without waiting: gate on §1.5). **Batch only.** The **absolute** path of a UTF-8 text file naming, one per line, the building tile files of `buildings_pbf_dir` the job burns (`{z}_{x}_{y}.pbf`, no directory part): only those files are read — the directory is never listed, so it may be a store of far more tiles (the Waveshed plugin's `<cache>/pool/<md5>/`). The file is also the **ready signal**: missing when the batch looks, it is waited for exactly as `buildings_pbf_ready`'s marker (same watcher, `buildings_pbf_wait_s`, `buildings_hold_mb`, lines and timeout). Requires `buildings_pbf_dir`, which must **exist when the batch starts**; excludes `buildings_pbf_ready`; jobs naming one list must give the same `buildings_pbf_dir` and `buildings_pbf_wait_s`. Every rule, line and message: *Building tiles named by a list* below. The single-job form (a JSON object) refuses it. Absent or `null`: unchanged. |

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
MultiPolygon: parts recurse, rings split on `ends` — the first ring of each
part its exterior, the rest its holes (since 0.2.9, changelog 42) — and each
part takes its own max Z as the absolute roof. They were previously skipped silently, so a
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
  without one is a **hard error** naming the file. The X and Y pixel steps are
  taken separately, so non-square pixels are sampled at their true size. A
  **rotated or sheared** grid (`ModelTransformation` term [1] or [4]
  non-zero) is a **hard error** naming the file (*"rotated/sheared
  geotransform is not supported — warp it to a north-up grid first"*,
  changelog 23); it used to be sampled as north-up and misplaced without an
  error. Georeferencing is **never**
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
  is decoded. A decode that yields anything but exactly `w·h` samples (per
  strip/tile: its own data size) is refused the same way.
  The TIFF **compression** must be one this build decodes — none, LZW,
  Deflate, PackBits, JPEG, WebP, and ZSTD in native builds; anything else
  (LERC, JPEG-XL, LZMA, JPEG 2000, … and ZSTD in the WASM build) is a **hard
  error raised from the header**, naming file and codec (changelog 23):

  ```
  source "<path>": unsupported compression <NAME> (<code>) — this build cannot decode it. Re-encode the file (e.g. `gdal_translate -co COMPRESS=DEFLATE`) or convert it through GDAL first.
  ```

  A band-1 **GDAL scale/offset** (`GDAL_METADATA` tag 42112, `<Item
  name="SCALE"|"OFFSET" sample="0" role="scale"|"offset">`) is applied —
  metres = `raw × scale + offset` — before the half-metre conversion; an
  unparseable value is a hard error naming the file.
  Sources are **streamed**: each output row band decodes only the strips/
  tiles its footprints reach, so a source's size does not bound the run's
  memory (changelog 23).
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
  Int32-widening rules of §6 apply unchanged on top. Nodata is compared
  with the **raw** sample, before any GDAL scale/offset.
* Supplying `sources` **and** either legacy field in one job is a **hard
  error** (the priority order would be ambiguous). A job with none of the
  three writes an all-void (or all-`void_fill_m`) tile as before at the
  library level, but the CLI refuses a whole run that covered nothing (§1.2,
  "An ingest that covered nothing exits non-zero").
* A listed source that cannot be opened, or whose header is unusable,
  **fails every tile that lists it** — never a warning; a pixel decode
  error fails the tile that needed those pixels.

> **Mixed zooms are refused on both paths.** `ingest` previously accepted a
> mixed-zoom `buildings_pbf_dir` and drew every zoom's copy of the same
> building, so each above-ground height was applied more than once and the
> building grew (see the precondition below). It now refuses the directory with
> the same message `download` has always used. A single-zoom directory — the
> only kind that ever produced correct output — is unaffected.

**Building-tile decode (`buildings_pbf_dir`, batch ingest; additive;
`aether_converter` ≥ 0.2.9, changelog 44).** A batch decodes each distinct
`buildings_pbf_dir` once, before its first tile, after the unchanged line
`[Rust] Decoding building tiles in "<dir>" (once for the run)...` — a
directory whose `buildings_pbf_ready` marker is missing at launch when the
marker appears, with the same lines (≥ 0.2.12; *Building tiles fetched
during the batch* below). A `buildings_pbf_list` (≥ 0.2.14, changelog 51)
is decoded the same way, once per distinct list, with its own start line
`[Rust] Decoding <n> listed building tiles from "<dir>" (once for the
run)...` and only the listed files taken (*Building tiles named by a
list* below). The
directory's tile files — every name `{z}_{x}_{y}.pbf` or `{z}_{x}_{y}.mvt`;
anything else is ignored — are taken in **name order** (byte-wise) and read
and decoded in parallel on the batch's worker pool (the `[Rust] Rayon Pool:`
threads). The result is exactly what a walk over that order, one file after
the other, gives: the same building list in the same order (so the duplicate
filter keeps the same copies), the same counts, the same warnings. On NTFS,
name order is also the order the sequential decoder before it walked
(changelog 44). The duplicate filter after the decode runs on the same pool
too (≥ 0.2.11, changelog 47) and keeps exactly the buildings, in exactly the
order, the one-thread filter kept. On stdout, the stream of `[Rust] Progress:`:

```
[Rust] Decoding building tiles: <n>/<m>
[Rust] Building tiles decoded: <m> tile(s), <b> building(s) in <s.s> s, <mb> MB
```

* `<m>` is the directory's tile files, decoded or not; `<n>` those finished
  (decoded, skipped or failed). A progress line comes at every multiple of
  1,000 and once at `<n>` = `<m>` — one line, when `<m>` is a multiple of
  1,000 — in increasing `<n>`, each flushed as printed; none for a directory
  without tile files.
* The summary line follows when the decode and the duplicate filter are
  done: `<b>` = buildings decoded **before** the duplicate filter (the
  `<n> building(s)` of the per-tile `[Info] buildings_pbf_dir:` line),
  `<s.s>` = wall seconds of the whole load, one decimal, and — since
  0.2.15 (changelog 52) — `<mb>` = the memory the decoded set holds for the
  rest of the run, MB (2^20 bytes) with one decimal: its building array (as
  allocated, before the duplicate filter shrank it) and every ring's
  vertices, each heap block as glibc hands it out (request + 8, rounded up
  to 16, at least 32 bytes; within 0 – 5.3 % of what glibc reports
  allocated on six real sets, changelog 52). An older plugin's pattern, which ends at
  `in <s.s> s`, still matches. It is the line
  after the decode's last line (a progress line, or the warnings below) and
  comes before any tile is sampled (a directory decoded when its marker
  appears: before any tile that burns it is written).
* A tile that does not decode is still skipped with `[Warn] buildings
  <name>: <error>` on stderr (and still counts in `<m>`). These lines now come
  together, in name order, after the last progress line and before the
  summary line; they used to come as the walk met them. A tile file that
  cannot be read, and a mixed-zoom directory — now refused before any tile
  is read, same message — fail the run as before, without a summary line.
* Neither line contains `Progress:`, so an ingest-progress parser
  (`Progress:\s*(\d+)/(\d+)`) does not take them for output tiles. The
  progress lines keep the start line's prefix `[Rust] Decoding building
  tiles`; the summary line does not. The format is stable: a consumer may
  show `<n>/<m>` as the decode's progress.
* The single-job form (a JSON object) decodes the same way on rayon's global
  pool, without these lines; so do `download`'s buildings post-pass and its
  repair jobs (§9b), whose lines are unchanged.

**Building tiles fetched during the batch (`buildings_pbf_ready`,
changelog 48, `aether_converter` ≥ 0.2.12).** A caller that fetches the
building tiles itself (the Waveshed plugin: OpenFreeMap z14) can launch a
batch at once, with `buildings_pbf_dir` naming the directory its fetch writes
into and `buildings_pbf_ready` naming a marker file it creates there when the
fetch is done — the hand-over of the download job (§9b). The batch then:

1. checks every job's fields before anything is read — exit 1, no tile
   started, no output directory created — failing with the download job's
   messages (§9b, step 1), prefixed `ingest job for "<output_path>": `, or
   with `buildings_hold_mb must be a number of megabytes from 0 to 1048576,
   got <v>` (same prefix), or, for jobs that name one directory with
   different hand-overs, `buildings_pbf_dir "<dir>": the jobs for "<a>" and
   "<b>" give it different buildings_pbf_ready / buildings_pbf_wait_s (<x>
   against <y>); the directory is decoded once for the run, so every job
   naming it must give the same`;
2. looks for `<buildings_pbf_dir>/<buildings_pbf_ready>` as a regular file
   once, at launch (its contents are ignored; a directory of that name is no
   marker). There: the directory is decoded at once, before the first tile,
   exactly as without the field — the same lines and bytes; nothing below
   applies;
3. missing: nothing reads the directory (it need not exist yet), every
   tile's terrain starts at once, and a watcher looks for the marker every
   250 ms. Once it is there the directory is read once and decoded with the
   lines of *Building-tile decode* above, on a pool of the batch's size
   beside the batch's own (whose threads may be waiting for that set);
4. a tile whose terrain — sampling, `void_fill_m`, `buildings_file` burn — is
   done before the set is decoded is **held**: its samples stay in memory,
   unwritten, while all held tiles fit `buildings_hold_mb` (the table above:
   default half of what the pool leaves, at most 4096 MB, at least one
   tile — a 2 m tile of 0.1° is about 59 MB); a tile that does not fit waits
   on its worker thread, which starts no other tile until the set is
   decoded. When the first tile has to wait, stdout gets, once,

   ```
   [Buildings] waiting for the building tiles (<marker path>)
   ```

   (`<marker path>` = `buildings_pbf_dir` joined with the marker name as the
   jobs give them);
5. once the set is decoded, stdout gets, when a tile did wait,

   ```
   [Buildings] building tiles ready after <s.s> s
   [Buildings] <n> tile(s) held (<m.m> MB) until the building tiles were ready
   ```

   (`<s.s>` = seconds since the waiting line, one decimal; the second line
   gains `; <k> more waited on their worker threads (hold budget <b.b> MB
   full)` when `<k>` tiles waited on their workers), and the held tiles are
   burnt and written in the order they finished — on as many threads as the
   batch has workers, beside every worker that comes free — then every later
   tile inline, as without the field. Each tile's `[Tile] … covered=` /
   `voids=`, `[Info] buildings_pbf_dir:` and `[Rust] Progress:` lines come
   as it is written, so none before its buildings are ready; the `.abt`
   bytes are those of a batch whose directory was complete at launch;
6. when the marker is still missing `buildings_pbf_wait_s` (default 1800)
   seconds after the first look, fails — the jobs asked for buildings, and a
   batch without them is never silent: exit 1 with, on stderr,

   ```
   Error: Building tiles not ready: the marker file <marker path> (buildings_pbf_ready) did not appear within <w> s (buildings_pbf_wait_s); the ingest fails rather than write tiles without their buildings
   ```

   (one line; match the `Building tiles not ready:` prefix). A directory that
   does not decode once its marker is there fails the batch the same way
   with the decode's error, as at launch (e.g. `buildings_pbf_dir mixes zoom
   levels (14 and 15); expected exactly one`). Either way the held tiles are
   dropped, the waiting workers released, every tile not yet started is
   skipped and every one being sampled stops at its next band; since no tile
   of the directory is written before its set is ready, **the batch leaves
   no tile of that directory behind** — no `.abt`, no `*.tmp-*` file, no
   `[Tile]`, `[Error] Failed to process tile:` or `[Rust] Progress:` line
   for them. Tiles of other jobs of a mixed batch (no `buildings_pbf_dir`,
   or one decoded at launch) written before keep their files and lines.

A held tile's `[Rust] tile …` diagnostic line counts its terrain and its
burn in `total`, not the wait between (§1.2).

**The caller's side.** As for the download (§9b): write the marker **last**,
once every tile file is complete and closed — the directory is read once,
right after the marker is seen; delete a marker left from an earlier run (or
use a fresh directory or marker name) before launching — a marker that
already exists counts; a caller whose fetch failed stops the batch (kills
it) or lets the wait run out rather than write the marker over an
incomplete directory. Killed while it waits, the batch has written no tile
of the directory (its other jobs' tiles may be there).

**Not affected:** a batch without `buildings_pbf_ready`, byte for byte and
line for line — `buildings_pbf_wait_s` and `buildings_hold_mb` alone are not
read; a batch whose marker is there at launch; the single-job form, which
refuses the field before anything is read (exit 1):

```
buildings_pbf_ready "<v>" is read by a batch ingest only (a JSON array of jobs): a single job decodes its buildings_pbf_dir before its tile, as it finds it — complete the directory before launching the job, or pass the job as a one-job array
```

(it reads its directory before its tile, as it finds it, so ignoring the
marker would burn a directory that is still being fetched; a one-job array
takes the field).

**Building tiles named by a list (`buildings_pbf_list`, changelog 51,
`aether_converter` ≥ 0.2.14).** A caller that keeps every building tile it
has fetched in one store directory (the Waveshed plugin:
`<cache>/pool/<md5>/{z}_{x}_{y}.pbf`, ~49,000 files and growing) names the
tiles a batch needs in a list file instead of building a directory holding
exactly those tiles: `buildings_pbf_dir` is the store, `buildings_pbf_list`
the list. The batch reads exactly the listed files from the store — it
never lists the store's directory — and writes what it writes from a
directory holding exactly the listed tiles: the same `.abt` bytes and the
same lines, but the decode's start line.

* **The list file**: UTF-8 text, one building tile file name per line —
  `{z}_{x}_{y}.pbf` (or `.mvt`), the name only — lines ending in `\n` or
  `\r\n`. Blank lines, lines whose first non-blank character is `#`,
  blanks around a name and a leading byte-order mark are ignored; a name
  listed twice is decoded once. A name with a directory part fails the
  batch: `buildings_pbf_list <list>: line <k>: "<name>" has a directory
  part; list the tile file names inside buildings_pbf_dir
  ({z}_{x}_{y}.pbf), one per line`; any other line that is no tile name:
  `buildings_pbf_list <list>: line <k>: "<line>" is not a building tile name
  ({z}_{x}_{y}.pbf)`; a file that is not UTF-8: `buildings_pbf_list <list>
  is not UTF-8 text`. An **empty list** — no byte at all, or blank and
  comment lines only — is no error: it decodes nothing, exactly as an empty
  directory does (every tile's `[Info] buildings_pbf_dir: 0 tile(s), no
  buildings in extent`, `[Rust] Building tiles decoded: 0 tile(s), 0
  building(s) in <s.s> s`, no progress line).
* **Checked before any tile**, with the marker's fields (exit 1, no tile
  started, no output directory created; each message prefixed `ingest job
  for "<output_path>": `):

  ```
  buildings_pbf_list "<v>" needs buildings_pbf_dir: the listed tile names are files inside that directory
  buildings_pbf_list and buildings_pbf_ready exclude each other: the list file is itself the ready signal (it is read once it exists); give buildings_pbf_list alone
  buildings_pbf_list must be the absolute path of a file, got "<v>"
  buildings_pbf_dir "<dir>" is not a directory; with buildings_pbf_list it must exist when the job starts — the listed tiles are read from it
  ```

  `buildings_pbf_wait_s` and `buildings_hold_mb` are read and checked as
  with a marker. The last check is the one look at the disk: **the store
  must exist at launch** (the Waveshed plugin creates it before launching);
  its listing is never read. Jobs naming one list must give the same
  `buildings_pbf_dir` and `buildings_pbf_wait_s` — the list is decoded once
  for the run: `buildings_pbf_list "<list>": the jobs for "<a>" and "<b>"
  give it different buildings_pbf_dir ("<x>" against "<y>"); the list is
  decoded once for the run, so every job naming it must give the same` (or
  `… different buildings_pbf_wait_s (<x> s against <y> s); …`). Jobs may
  name one list or different lists (one decode per distinct list); a job
  naming the same directory without a list is a source of its own, the
  whole directory, decoded as before.
* **The list is the ready signal** — the marker's hand-over above, step for
  step, with the list's path, exactly as the job gives it (no canonical
  form, no quotes), in place of `<marker path>`: there when the batch looks
  at launch → read and decoded at once, before the first tile; missing →
  every tile's terrain starts, the list is looked for every 250 ms, finished
  tiles are held within `buildings_hold_mb`, and stdout gets
  `[Buildings] waiting for the building tiles (<list path>)`, `[Buildings]
  building tiles ready after <s.s> s` and the held line. A list still
  missing `buildings_pbf_wait_s` after the first look fails the batch, no
  tile of the list written:

  ```
  Error: Building tiles not ready: the list file <list path> (buildings_pbf_list) did not appear within <w> s (buildings_pbf_wait_s); the ingest fails rather than write tiles without their buildings
  ```

  The list is read **once**, right after it is seen: write it under another
  name and rename it into place when it is complete — a list read while it
  is being written is read short.
* **The decode**: the distinct listed names, in name (byte) order, read and
  decoded by the directory's loader — the same parallel decode on the same
  pool, the same building order and duplicate filter, the same counts and
  `[Warn] buildings <name>: <error>` skips — after the start line

  ```
  [Rust] Decoding <n> listed building tiles from "<dir>" (once for the run)...
  ```

  (`<n>` = the distinct listed names, a plain integer; `<dir>` =
  `buildings_pbf_dir` exactly as the job gives it, between quotes — where
  the directory's start line prints Rust's `Debug` form, which doubles a
  Windows path's backslashes). `[Rust] Decoding building tiles: <n>/<m>`,
  `[Rust] Building tiles decoded: <m> tile(s), …` (`<m>` = the distinct
  listed names) and every tile's `[Info] buildings_pbf_dir: <n> tile(s), …`
  are unchanged. A listed tile missing from `buildings_pbf_dir`, or one that
  cannot be read, fails the batch — the caller guarantees the files, so a
  missing one is a bug or a store cleared under the run:

  ```
  Error: Building tile <name> listed in <list> is not in <dir>
  ```

  (the first such name in name order; `<list>` and `<dir>` as the job gives
  them; for a file that is there but cannot be read, the OS error follows
  as `Caused by:`), exit 1, no tile of the list written. Listed names of two
  zooms fail before any tile is read: `buildings_pbf_list <list> mixes zoom
  levels (<a> and <b>); expected exactly one`.

**Not affected:** a batch without `buildings_pbf_list`, byte for byte and
line for line. The single-job form refuses the field before anything is
read (exit 1) — it would decode the whole directory, the caller's store:

```
buildings_pbf_list "<v>" is read by a batch ingest only (a JSON array of jobs): a single job decodes its whole buildings_pbf_dir before its tile, as it finds it, and would ignore the list — pass the job as a one-job array
```

**Building write rule (both sources).** Every height off the ladder above is
normalised to an absolute roof elevation — an above-ground rung by adding the
terrain under its own footprint, read before anything is written — and
composited with `max` against the surface, never added to it. Consequences
callers can rely on:

* a roof is **flat**, even where the terrain under the footprint slopes;
* overlapping footprints do **not** accumulate;
* re-applying an *absolute*-height source is a no-op;
* a footprint is its exterior ring **minus its interior rings**: a courtyard
  stays at the terrain (since 0.2.9, changelog 42);
* a vector-tile building that arrives through two tiles' buffer zones is drawn
  once — a duplicate is identical in every vertex of every ring (1e-6°) and
  in height; anything less is drawn per copy, harmlessly (since 0.2.9,
  changelog 41).

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
| `max_connections` | usize? | optional | Concurrent HTTP connections. Default **256** when absent. The CLI `download` runs a fixed pool of this many requests (changelog 30); its window is cut by 25 % in a second only when ≥ 3 responses in that second were HTTP 429 or 503 (floor `max(max_connections / 4, 16)`, at most `max_connections`), and grows by 10 % per second without such a burst back to `max_connections`. Timeouts, connection resets and slow bodies never move it, and a cut only gates new requests. A clean link therefore runs at exactly `max_connections`. Reported by `[Stats] Connection window:` and the progress line's `window` field (§1.2). On unix the CLI `download` first makes room for `2 × max_connections + 96` open descriptors by raising its soft open-file limit (best effort); only where even the raised limit is lower does it run `(limit − 96) / 2` connections instead, printing `[Download] open-file limit …` (changelog 36). A full run (not a repair) also keeps its output files open for the run, so the raise asks for one descriptor per output tile on top; where they do not fit beside the connections' descriptors, the connections keep theirs and each write opens its file instead, printing `[Download] open-file limit <l>: <n> output tiles are opened for each write instead of held open (<d> descriptors would not fit)` (changelog 39). Since 0.2.15 (changelog 52) a full run whose memory share (min(available RAM / 4, 2 GiB), at least 256 MiB) cannot hold this many connections beside its assembly, decoding, cache writes and two tile-rows of strips runs fewer — the most that fit, at least 16 — and says so once: `[Download] connections <a> → <b>: …` (§1.2); its window then runs between `max(<b> / 4, 16)` and `<b>`. Bytes never depend on it. |
| `buildings_pbf_dir` | string? | optional | Directory of `{z}_{x}_{y}.pbf` vector tiles, same encoding and same **building height ladder** (§9a) as §9a's `buildings_pbf_dir`. When present, buildings are fused onto the finished `.abt` tiles as a post-pass once the download completes (with `buildings_pbf_ready`, once its marker exists; with `buildings_pbf_list`, ≥ 0.2.14, only the listed tiles, once the list exists). All tiles in the directory must share **one** zoom; a mixed-zoom directory fails with `buildings_pbf_dir mixes zoom levels`. Absent means terrain only. |
| `buildings_pbf_ready` | string? | optional | Additive, changelog 45, `aether_converter` ≥ 0.2.10 (older builds ignore it: gate on §1.5). One plain file name inside `buildings_pbf_dir` (e.g. `"_ready"`) whose existence declares the directory complete, so the building tiles can be fetched **while** the job downloads the terrain: nothing reads the directory before the buildings post-pass (it need not exist at launch), and the post-pass first waits for the file — see *Building tiles fetched during the download* below. Requires `buildings_pbf_dir`; empty, `.`, `..`, a name containing `/` or `\`, or a `{z}_{x}_{y}.pbf`/`.mvt` tile name fails the job before any request. Absent or `null`: the post-pass reads the directory as it finds it, as before. CLI `download` full runs only; repair jobs and the in-memory/WASM downloaders ignore it. A batch `ingest` takes the same field, with the same meaning, since 0.2.12 (§9a, changelog 48). |
| `buildings_pbf_wait_s` | number? | optional | Additive, changelog 45, ≥ 0.2.10. Seconds the post-pass waits for `buildings_pbf_ready` — or, ≥ 0.2.14, for the `buildings_pbf_list` file (changelog 51) — counted from its first look; absent or `null` → **1800**. Must be 0 … 86400, else the job fails before any request: `buildings_pbf_wait_s must be a number of seconds from 0 to 86400, got <v>`. Read only together with `buildings_pbf_ready` or `buildings_pbf_list` (alone it changes nothing). |
| `buildings_pbf_list` | string? | optional | Additive, changelog 51, `aether_converter` ≥ 0.2.14 (older builds ignore it and burn the whole `buildings_pbf_dir` as they find it after the terrain, without waiting: gate on §1.5). The **absolute** path of a UTF-8 text file naming, one per line, the building tile files of `buildings_pbf_dir` the post-pass burns (`{z}_{x}_{y}.pbf`, no directory part): only those files are read — the directory is never listed, so it may be a store of far more tiles. The file is also the **ready signal**: the post-pass waits for it exactly as for `buildings_pbf_ready`'s marker (same lines, `buildings_pbf_wait_s`, timeout), then reads it and decodes the listed tiles. Requires `buildings_pbf_dir`, which must **exist when the job starts**; excludes `buildings_pbf_ready`. CLI `download` full runs **and repair jobs** — a repair honours the list exactly as a first pass (selection, wait, errors; it ignores only the marker); the in-memory/WASM downloaders ignore it. Rules, lines and messages: *Building tiles named by a list* below and §9a. |
| `fetch_bounds` | object? | optional | `{south, north, west, east}` in degrees (additive, 2026-09-05). Only source tiles intersecting it are fetched/decoded; every pixel outside keeps the `-9999` void pre-fill. Output grid, geometry and `.abt` layout unchanged — this trims the fetch set, nothing else. **In-memory downloader only**: the file-based `download` CLI rejects jobs carrying it (`fetch_bounds is not supported by the file-based download path …`), because its sparse writer would read skipped rows as 0 m sea level. Callers pass the area they actually need, inflated ≥ 2 % (covers the engine's own 1 % bounds margin). |
| `tiles` | array of `SubTileSpec` | **required** | One entry per output `.abt`. |
| `tile_deadline_s` | number? | optional | Additive, changelog 28, `aether_converter` ≥ 0.2.6. Wall-clock cap per source tile, in seconds, across all its attempts, back-offs and `Retry-After` waits; absent → **20** (changelog 30; was 60). At expiry the tile stops as a `timeout` failure and goes to the parent fill (`[Degraded] … reason=timeout` when an ancestor fills it). Must be a positive number ≤ 86400, else the job fails `tile_deadline_s must be a positive number of seconds, got <v>` before any request. CLI `download` only; the in-memory downloaders ignore it. |
| `repair_source_tiles` | array of `[z, x, y]`? | optional | Additive, changelog 28, `aether_converter` ≥ 0.2.6 (gate on `--version`, §1.5: older builds ignore the field and download the whole job). Fetch **only** these source tiles and patch them into the job's **existing** `.abt` files — see *Repair jobs* below. |
| `cache_dir` | string? | optional | Additive, changelog 38, `aether_converter` ≥ 0.2.8 (older builds ignore it). Directory of the raw source-tile cache, shared by every job, service and zoom that names it and by concurrent converter processes; absent, `null` or `""` → no cache. Every source tile is looked up there before any request and every HTTP 200 body that decoded and every 404 the run fetches is written there; since 0.2.9 (changelog 40) a tile without an entry is derived from the cached tiles of up to four finer zooms when they cover it, without a request — see *Raw source-tile cache* below. A directory that cannot be used is one `[Warn]` line, never an error. CLI `download` only (repair jobs refetch their listed tiles and rewrite their entries); the in-memory downloaders ignore it. |
| `seam_filter` | bool? | optional | Additive, changelog 49, `aether_converter` ≥ 0.2.13 (older builds ignore it and filter nothing: gate on §1.5). Absent, `null` or `true`: **on** — every Terrarium tile of zoom 11 or finer the CLI `download` decodes (fetched, read from the raw tile cache, derived from cached finer tiles, or a coarser parent of the backfill) has its mosaic-seam samples turned into voids before the parent fill, which fills them from the coarser zooms like the service's own voids — see *Seam filter* below. `false`: off — every tile decoded exactly as 0.2.12 decodes it, for A/B runs (give them a `cache_dir` of their own: a tile a filtering run derived and stored holds the filtered samples). Mapbox jobs are never filtered; nor are the in-memory and browser downloaders, which ignore the field. The Waveshed plugin does not set it. |

`SubTileSpec`:

| Field | Type | Notes |
|-------|------|-------|
| `filename` | string | Output `.abt` name within `output_dir`. |
| `ul_lat` | f64 | Upper-left latitude. |
| `ul_lon` | f64 | Upper-left longitude (east-positive). |
| `size_px` | u32 | Tile side (px). |
| `resolution_m` | f64 | Metres per pixel; the writer sets `scale_x = scale_y = resolution_m / 111111`. |

**Tile format is read, never assumed.** A source tile is **PNG or WebP**
(lossless `VP8L` as Mapterhorn serves, or lossy), told apart by the bytes
(`RIFF…WEBP` magic), never by the URL or a content type. The job carries no
format field; `encoding` describes the pixel→metre mapping only, and the same
job runs unchanged against `…/terrarium/{z}/{x}/{y}.png` (AWS, 256 px) and
`https://tiles.mapterhorn.com/{z}/{x}/{y}.webp` (512 px). Both decode to 8-bit
RGB(A) and go through one sample loop. (Changelog item 25.)

**Tile size is read, never assumed.** The job says nothing about how large the
source's XYZ tiles are, and services disagree: AWS Terrarium serves **256 px**
tiles, Mapterhorn and MapTiler's `terrain-rgb` serve **512 px** ones (MapTiler's
`tiles.json` declares `"scale": "2.000000"`, i.e. `@2x`). The edge is therefore
taken from the first tile that decodes and held for the rest of the run; the
assembly grid, its row stride and the per-tile-row sample count all follow it.
The CLI `download` fetches its first tile before it sizes any strip, so the
strips' share of the RAM budget is reckoned with the real edge (since 0.2.15
the budget's assembly, connection and decoding terms too, changelog 52);
512 px is assumed only while that tile has failed or is out for more than
5 s (changelog 36).

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

Anything else a tile source does wrong — a 404, a timeout, a JPEG body — stays a
per-tile failure, tallied in `[Stats] ERRORS` and judged by the majority rule.

What becomes of each source tile, and how the `[Stats]` block counts it
(§1.2; changelog items 16, 17, 24):

| Fetch outcome (after retries) | Coarser ancestor supplied pixels? | Output pixels | `<ok>` of `[Stats] Tiles:` | Error tally | Fatal majority | Extra line |
|---|---|---|---|---|---|---|
| arrived | — (in-tile voids may be filled; since 0.2.13 a Terrarium tile's seam samples are voided first, *Seam filter* below) | the tile | counted | — | — | `[Stats] Backfill: …` (≥ 0.2.13) when it had void samples |
| HTTP 404 | yes | upsampled ancestor | counted | — | — | `[Stats] Repaired …` |
| HTTP 404 | no | `-9999` void | not counted | `HTTP_404_no_data` | never | `[Stats] NO-DATA (HTTP 404): …` |
| any other failure | yes | upsampled ancestor | **not counted** | its class | **no** | `[Degraded] …`, `[Stats] DEGRADED: …`, `[Stats] Repaired …` |
| any other failure | no | `-9999` void | not counted | its class | yes | — |

"Any other failure" is timeout, connect, HTTP 429, other 4xx (403, 410, …),
5xx, decode, other. The `reason=` token of `[Degraded]` uses the same
classifier as the tally: `timeout`, `connect`, `http_<code>` (any status),
`decode`, `other`. This table describes the file-based CLI `download`; the
in-memory/WASM downloaders never fill a failed tile from parents, so for them
every failure is the last row.

**Seam filter (`seam_filter`, changelog 49, `aether_converter` ≥ 0.2.13).**
A tile service's mosaic can blend its nodata into the valid samples beside a
source boundary: AWS Terrarium's zoom-15 tiles at 10°E (lat 46.7 – 46.9) hold
a column of −8,956 … −9,717 m and one of −1,184 … +149 m between terrain of
1,500 – 3,000 m, its zoom-14 tile above them one column of −3,524 … −4,709 m
(more such seams in changelog 49). Such samples are above the void threshold
(−11000 m), so before 0.2.13 they reached the `.abt` as terrain. With the
filter on (`seam_filter` absent, `null` or `true`; Terrarium only), every
tile of zoom ≥ 11 the CLI `download` decodes — fetched, read from the raw
tile cache, the children a cached tile is derived from, the coarser tiles
the parent fill reads — has its **seam notches** turned into voids right
after the decode, so the parent fill fills them from the coarser zooms
exactly like the service's own voids (a notch in a parent is skipped there
and the next coarser zoom fills). A valid sample is a seam notch when **both
its row neighbours, or both its column neighbours, are more than 2,200 m
higher**; a neighbour is the nearest valid sample within 2 samples in that
direction inside the tile — voids are skipped and are never neighbours, the
tile edge or a run of voids ends the search, and an axis without a neighbour
on both sides is not judged (no sample is judged across a blank area). Each
pass voids all the notches it finds against the tile as it found it; the
rule runs again on the result, at most 2 passes, stopping at the first that
finds none — so a two-sample seam whose inner sample is the lower goes in
two. Real terrain's deepest both-sided single-sample notch in the survey of
changelog 49 is 1,030 m (z11; ≤ 439 m at z12, ≤ 51 m at z14, ≤ 17 m at
z15); the shallowest 10°E seam sample is 2,793 m deep. Zooms ≤ 10 are not
filtered: the service's own tiles there hold single-sample pits of 3.7 –
6.0 km. The cache keeps the service's bytes as they arrived; a tile it
derives is built from filtered children and filtered at its own zoom — what
decoding its stored body gives — and a sample void because its children's
seam samples were voided is stored as Terrarium RGB `0, 0, 1` (−32767.996 m,
a void to every converter) instead of `0, 0, 0`, read back as a seam sample
by a filtering run. `seam_filter: false` decodes every tile exactly as 0.2.12
did. The backfill is reported either way (§1.2): `[Stats] Backfill: <n> px in
<m> tile(s) filled from coarser zooms (<k> px seam samples)` in the `[Stats]`
block, and with `AETHER_DIAG_FILE` set one `[Backfill] z=<z> x=<x> y=<y>: …`
line per tile in that file only. Mapbox jobs and the in-memory and browser
downloaders are never filtered (they have no in-tile backfill to fill a
void).

**Repair jobs (`repair_source_tiles`, changelog 28).** A job carrying
`repair_source_tiles: [[z, x, y], …]` is the full job (same `url_template`,
`encoding`, `output_dir`, `zoom`, `tiles`) plus the list of source tiles to
fetch again — typically the tiles a previous run named on `[Degraded]` lines.
The CLI `download` then:

1. validates before any request, failing (non-zero exit, nothing written)
   with: `repair_source_tiles is empty — nothing to repair`;
   `repair_source_tiles: tile [z, x, y] is not at the job's zoom <zoom>`;
   `repair_source_tiles: tile [z, x, y] does not intersect any output tile of this job`;
   `repair_source_tiles: output tile <path> does not exist — a repair patches the files of a finished download; run the job without repair_source_tiles first`
   (every output file a listed tile touches must exist); or
   `repair_source_tiles: <path> is not this job's output tile (…)` when its
   44-byte header or length differs from what the job's spec writes;
2. fetches only the listed tiles (duplicates once) with the normal retries,
   per-tile deadline, parent fill and classification, and prints the normal
   protocol restricted to them — `[Download] <pct>% (<done>/<n>) …`,
   `[Degraded] …`, the `[Stats]` block with `<total>` = the number of listed
   tiles; no `[Tile] … voids=` line (changelog 46: a repair patches pixels
   of files it does not rewrite, so a consumer re-counts them or treats
   their voids as unknown) — preceded by
   `[Download] repair: z=<z> <n> source tile(s) into <m> existing output tile(s) connections=<c>`
   and followed by
   `[Download] repair: <p> px rewritten in <k> output tile(s)` (and, when some
   listed tile still failed with nothing to fill it,
   `[Download] repair: <f> listed tile(s) still failed with no coarser fill; their pixels were left unchanged`);
   informational, match the prefix;
3. rewrites an output pixel **only** when every source sample under its
   footprint belongs to a listed tile that produced data (arrived, or filled
   from an ancestor). Such a pixel gets exactly the bytes a full run with the
   same tile answers writes (same grid, same area averaging). Every other
   byte — header, row padding, all other pixels — is left unchanged,
   including the one-pixel border column where a footprint straddles a
   listed and an unlisted tile (list the neighbour too to rewrite it) and
   the pixels of a listed tile that failed with no ancestor (a void would be
   no better than what is there);
4. applies the majority rule of §1.2 over the listed tiles: a fatal repair
   fails with `Tile download failed: <f> of <n> listed repair tiles (<p>%) could not be fetched; nothing was written — the existing .abt files are unchanged. Errors: <breakdown>.`
   and, unlike a full run, never removes a file;
5. with `buildings_pbf_dir`, rasterizes buildings onto the repaired terrain
   and takes only the repaired pixels from it (`[Buildings] repair: …`); a
   building straddling the repair edge is resolved over a mix of repaired
   and already-built pixels. The directory is read as the repair finds it —
   a repair ignores `buildings_pbf_ready` — but with `buildings_pbf_list`
   (≥ 0.2.14, changelog 51) only the listed tiles are read, after the same
   wait for the list as a full run's post-pass, before any file is written
   (*Building tiles named by a list* below). A repair that rewrote no pixel
   burns nothing and reads neither.

`fetch_bounds` stays rejected on this path, as for every file-based run.

**Raw source-tile cache (`cache_dir`, changelog 38, `aether_converter` ≥ 0.2.8).**
A job naming `cache_dir` shares one on-disk store of what tile services
answered with every other job, service and zoom that names the same
directory (a relative path resolves against the working directory, like
`output_dir`; absent, `null` or `""` → no cache and the run is exactly as
without the field). What it holds is the **raw answer**: each HTTP 200 body
exactly as it arrived (PNG/WebP bytes, before any decode) and each HTTP 404
("no tile here"). The CLI `download` looks every source tile of its fetch
set up before any request (one index read per pack), decodes a hit from the
cached bytes when its turn comes — without a connection slot and without a
request — and writes every answer it fetches into the cache, so a later job
fetches only what the cache lacks. A tile the cache has no entry for, but
holds at a finer zoom, is derived instead of fetched (*Deriving* below,
≥ 0.2.9). The parent fill (§1.2) looks its coarser tiles up and writes them
the same way. **Never cached:** a tile that failed
(any class of the `[Stats] ERRORS` tally but 404), timed out, did not decode,
or was filled from a coarser zoom (a `[Degraded]` tile is fetched again by
the next job; what the fill made of a tile is never stored, only the raw
answers it was made from). A 404 is remembered until the directory is
deleted. Repair jobs refetch their listed tiles on purpose (no lookup) and
replace their cached answers with the fresh ones; their parent fill uses the
cache like a full run. The in-memory and browser downloaders ignore the
field.

Layout (`service_key` = the first 16 hex digits, lower case, of SHA-256 over
`url_template` exactly as the job gives it — **query string included** — +
`"\n"` + the lower-cased `encoding`; Python:
`hashlib.sha256(f"{url_template}\n{encoding.lower()}".encode()).hexdigest()[:16]`
— the AWS Terrarium template of `fixtures/jobs/download_job.json` with
`terrarium` is `54d18ad3dc1a2f05`):

```
<cache_dir>/<service_key>/service.json                 {"encoding": …, "url_template": <the template without its query string>}  (informational, written once)
<cache_dir>/<service_key>/z<zoom>/x<x>_y<yblock>.pack   yblock = y div 256
```

Templates that differ only in their query string never share a directory,
so datasets a service tells apart by a query parameter
(`…/{z}/{x}/{y}?layer=a` / `?layer=b`) can never read each other's tiles.
Two API keys for one service (`…/{y}.webp?key=A` / `?key=B`) therefore get
two directories — disk, not correctness — whose `service.json` files show
the same template: it is recorded without its query string (everything from
the first `?` on removed; the path, placeholders included, stays), so a key
in the query never reaches the disk in clear text; the directory name is a
truncated hash. A key placed in the **path** is part of the recorded
template and does land in `service.json`.

A pack holds the 256 tile rows `y = yblock·256 + i` of one tile column. All
integers little-endian:

| Offset | Bytes | Field |
|--------|-------|-------|
| 0 | 4 | magic `AETP` |
| 4 | 2 | u16 version = 1 |
| 6 | 4 | u32 zoom |
| 10 | 4 | u32 x |
| 14 | 4 | u32 yblock |
| 18 | 256 × 20 | index, entry `i` = tile `y = yblock·256 + i`: u64 offset of the body, u32 length, u8 status (0 unknown, 1 present, 2 no tile = HTTP 404, 3 derived body — ≥ 0.2.9, changelog 40), 3 reserved bytes (0), u32 CRC-32 (IEEE, zlib's) of the body |
| 5138 | … | data region: bodies, appended |

A full pack of AWS z15 tiles is ~256 × 73 KB ≈ 19 MB. **Writing:** the
writer locks the pack (unix incl. macOS: `flock(LOCK_EX)`; Windows: the
pack opened for writing with share mode `FILE_SHARE_READ`, which refuses
every other writer and admits readers), appends the bodies in one write,
then writes their index entries in place, and closes the file (which
releases the lock; the OS releases it when a process dies, so no lock
outlives its writer). Under the lock it first reads what each target slot
holds: a derived tile (status 3, *Deriving* below) is not written into a
slot that holds a body matching its CRC (status 1) or a 404 (status 2) —
another process may have stored the service's own tile since this run
judged the tile derivable from its index — nor after a real answer for the
same slot in the same batch; a real answer replaces anything (≥ 0.2.12,
changelog 48f; an older converter writes blindly, last writer winning). A
pack another writer holds for 10 s is not written by this run (one `[Warn]
cache_dir …` line per run); the run goes on. Several converter processes may
use one cache at once. **Reading** takes no lock and
validates every entry: the header must name this very zoom/column/yblock
and version 1, a present or derived body must lie past the index and inside
the file and match its CRC, a 404 entry must have length and CRC 0 (any
other status is a miss). Any entry that
fails — a torn or interrupted write, a damaged byte, a body that no longer
decodes — is a miss: the tile is fetched and the entry rewritten (appended
again and repointed; the old bytes become garbage). Nothing compacts a
pack; deleting the directory (or any part of it) is always safe while no
converter runs. A pack with another version is neither read nor written. A
run keeps at most 16 packs open for reading (a most-recently-used set) plus
one transient open per worker thread and the writer's handle, never one
descriptor per pack; the CLI's open-file reserve (changelog 36) counts them.

**Deriving (changelog 40, `aether_converter` ≥ 0.2.9).** A fetch-set tile
`(z, x, y)` without a valid entry is built from its four children
`(z+1, 2x+dx, 2y+dy)` when each of them is a cached body (status 1 or 3), a
cached 404, or itself derivable the same way — **at most four zooms deep**:
the tiles at `z+4` must be cached as they are (a z11 tile from z15 reads
4 + 16 + 64 + 256 child entries); a tile with a child the cache lacks is
fetched whole — a missing child never becomes a void. Output pixel `(i, j)`
is the mean of the child quadrant's 2 × 2 samples `(2i + {0,1}, 2j + {0,1})`
that are not void (a sample at or below −11000 m is void, and so is a 404
child's whole quadrant); void where all four are; four 404 children make a
404, which is not stored. All children must have one even tile edge, which the result
keeps. The result is encoded in the service's own encoding as an 8-bit RGB
PNG — Terrarium `R·65536 + G·256 + B = round((v + 32768)·256)` (ties up;
void = `0,0,0`), Mapbox the triple whose decoded value is nearest `v` (every
value either decoder produces round-trips exactly) — and stored with
**status 3**, as is every tile derived on the way (a cascade costs four
decodes per level once the level below is stored). Mapbox has no void value
(its lowest value, −10000 m, is terrain), so a Mapbox tile that would hold a
void — a 404 child beside children with data — is not derived but fetched.
Nothing is ever requested for a child. Whether a tile can be derived is
judged before the first request, from index entries alone; a derivation
that then fails (a child body that fails its CRC or decode, children of
different sizes) fetches the tile as before, and a tile whose own entry
fails is derived when it can be. A derived tile counts as a hit in every
respect (no connection slot, the same progress and `[Stats]` counting; a
derived 404 goes to the parent fill like a cached one). The parent fill
never derives (a derived ancestor of a hole holds that hole) but reads
status-3 entries like any other; repair jobs never derive their listed
tiles. **A derived tile is not the service's tile:** it is our 2 × 2 average
of the service's finer tiles, not the coarser tile the service would have
sent (services build their pyramids their own way), so the `.abt` bytes of
a job that derives can differ from a run that fetched; they equal those of
a run served coarser tiles that hold those very means. The pack version
stays 1: a converter before 0.2.9 reads status 3 as unknown, a miss, and
refetches the tile (rewriting the entry as status 1). Since 0.2.13, with the
seam filter on (changelog 49, *Seam filter* above), the children are decoded
with it and the result is filtered at its own zoom; a sample void only
because its children's seam samples were voided is written `0, 0, 1`
(−32767.996 m) instead of `0, 0, 0` — a void to every converter, and read
back as a seam sample by a filtering run. A Terrarium tile derived with the
filter on therefore differs from one derived without it wherever its
children held a seam.

What a run prints (stderr, informational, **not parsed** — consumers may log
them): after the pass header and before the first request,

```
[Download] cache: <n> of <total> source tiles from cache (<derived> derived)
```

once, `<n>` = the fetch-set tiles whose entry the lookup found plus the
`<derived>` ones it found derivable (an entry whose bytes then fail their
CRC or decode, or a derivation that then fails, is fetched after all); as
the last lines of the `[Stats]` block,

```
[Stats] Cache: <hits> hits (<derived> derived), <fetched> fetched, <written> written to <cache_dir>
[Stats] Cache, coarser parents: <hits> hits, <fetched> fetched, <written> written
```

(the first always, `<hits>` counting the derived tiles too, so `<hits> +
<fetched>` = the pass `<total>`, and `<written>` counting derived tiles and
their intermediates — those written: since 0.2.12 not one a real entry kept
out (*Writing* above); the second only when the parent fill asked for coarser
tiles). Before 0.2.9 neither `[Download] cache:` nor `[Stats] Cache:` had
the `(… derived)` part; both always carry it now, `(0 derived)` included. A
directory that cannot be
created or used:
`[Warn] cache_dir <dir> cannot be used (<reason>); downloading without cache`
— once, never fatal. A repair job prints `[Download] cache: 0 of <n> source
tiles from cache (a repair refetches every listed tile)`. A hit — a derived
tile included — counts in every parsed line exactly like the fetched tile
it stands for —
`[Download] <pct>% (<done>/<total>)`, `[Stats] Tiles:`, `NO-DATA (HTTP 404):`,
`Repaired …`, `DEGRADED:` — and the `.abt` bytes are identical whether a tile
came from the cache or the network (a derived tile is the cache's own
average, see *Deriving* above); `[Stats] Downloaded:` and the progress
line's MB/s count network bytes only.

**Building tiles fetched during the download (`buildings_pbf_ready`,
changelog 45, `aether_converter` ≥ 0.2.10).** A caller that fetches the
building tiles itself (the Waveshed plugin: OpenFreeMap z14) can launch the
download job at once, with `buildings_pbf_dir` naming the directory its fetch
writes into and `buildings_pbf_ready` naming a marker file it creates there
when the fetch is done. The CLI `download` then:

1. checks the fields before any request — exit 1, nothing written,
   `output_dir` not created — failing with one of
   `buildings_pbf_ready "<v>" needs buildings_pbf_dir: the marker is a file inside that directory`;
   `buildings_pbf_ready must be a file name inside buildings_pbf_dir, got "<v>"`
   (empty, `.`, `..`, or containing `/` or `\`);
   `buildings_pbf_ready "<v>" is named like a building tile ({z}_{x}_{y}.pbf) and would be decoded as one; choose another name`;
   `buildings_pbf_wait_s must be a number of seconds from 0 to 86400, got <v>`;
2. downloads and assembles the terrain without looking at
   `buildings_pbf_dir` at all — the directory need not exist yet;
3. at the buildings post-pass — where it always ran: after every output
   is assembled and padded and the majority rule of §1.2 has passed (a
   fatal terrain failure fails without waiting) — looks for
   `<buildings_pbf_dir>/<buildings_pbf_ready>` as a regular file (its
   contents are ignored; a directory of that name is no marker), at once
   and then every 250 ms. Found at the first look: nothing is printed.
   Otherwise it prints, on stderr, once each:

   ```
   [Buildings] waiting for the building tiles (<marker path>)
   [Buildings] building tiles ready after <s.s> s
   ```

   the first before the first poll, the second when a look finds the
   marker; `<marker path>` is `buildings_pbf_dir` joined with the marker
   name as the job gives them, `<s.s>` the seconds since the first look,
   one decimal;
4. then reads the directory **once** and burns it exactly as a job without
   the field does: the parallel decode (changelog 44), the same
   `[Buildings] <n> pbf tile(s) → …` (or `… no buildings in extent`) line,
   the same `.abt` bytes as a job whose directory was complete at launch;
5. when the marker is still missing `buildings_pbf_wait_s` seconds (default
   1800) after the first look, fails — the job asked for buildings, and a
   run without them is never silent: exit 1 with, on stderr,

   ```
   Error: Building tiles not ready: the marker file <marker path> (buildings_pbf_ready) did not appear within <w> s (buildings_pbf_wait_s); the download fails rather than leave terrain without its buildings
   ```

   (one line; match the `Building tiles not ready:` prefix), preceded, as
   for every failed download (§1.2), by `[Download] Removed <n> incomplete
   .abt file(s) …` — the terrain is not left behind to be pooled as
   terrain with buildings — and with no `[Buildings] … pbf tile(s)` line and
   no `[Stats]` block.

`[Stats] Downloaded: <mb> MB in <s>s` (and its MB/s) leaves the wait out;
`[Download] TOTAL:` is the wall time, the wait included. Neither new line
matches `[Buildings] <n> pbf tile(s)`; a consumer that takes **any**
`[Buildings]` line for a burn must match that line (and exit 0) instead.

**The caller's side.** Write the marker **last**, once every tile file is
complete and closed: the directory is read once, right after the marker is
seen, so a tile written later is not burnt and one still being written may
be read short (decoded in part, or skipped with `[Warn] buildings <name>:
<error>`). Delete a marker left from an earlier run (or use a fresh
directory or marker name) before launching — a marker that already exists
counts. A caller whose fetch failed stops the job (or lets the wait run
out) rather than write the marker over an incomplete directory. A job
killed from outside cannot clean up after itself (only an error removes
its outputs): killed during the wait, it leaves complete terrain **without**
buildings under the output names — remove them, as after any killed
download.

**Not affected:** a job without `buildings_pbf_ready` (byte for byte and
line for line; `buildings_pbf_wait_s` alone is not read); repair jobs
(`repair_source_tiles`), which ignore both fields — they patch a finished
run, whose post-pass already saw the marker, and read the directory as they
find it (a repair with `buildings_pbf_list` reads `buildings_pbf_wait_s` for
the list, changelog 51); and the in-memory/WASM downloaders, which never
burn `buildings_pbf_dir` and ignore both fields.

**Building tiles named by a list (`buildings_pbf_list`, changelog 51,
`aether_converter` ≥ 0.2.14).** The batch ingest's list (§9a), on a CLI
`download` — a full run, and a repair (below): `buildings_pbf_dir` is the caller's store,
`buildings_pbf_list` names the tiles this job burns, and the post-pass reads
exactly those files — never the store's listing — and writes what a job
whose `buildings_pbf_dir` holds exactly the listed tiles writes: the same
`.abt` bytes, the same `[Buildings] <n> pbf tile(s) → …` line (`<n>` = the
listed tiles that decoded — all of them, normally; never the store's
count), the same `[Tile] … voids=` lines. An empty list (no byte, or blank
and comment lines only) burns nothing, as an empty directory does:
`[Buildings] 0 pbf tile(s), no buildings in extent`. The list file's format,
the field checks — before any request, `output_dir` not created — and their
messages are §9a's, unprefixed; among them, the store must exist when the
job starts (`buildings_pbf_dir "<dir>" is not a directory; with
buildings_pbf_list it must exist when the job starts — the listed tiles are
read from it`), and nothing in it is read before the post-pass. The list is
the ready signal of the steps above, step for step: the post-pass looks for
it where it would look for the marker; when it has to wait, stderr gets
`[Buildings] waiting for the building tiles (<list path>)` and `[Buildings]
building tiles ready after <s.s> s` (the list's path exactly as the job
gives it); a list that does not come fails the run:

```
Error: Building tiles not ready: the list file <list path> (buildings_pbf_list) did not appear within <w> s (buildings_pbf_wait_s); the download fails rather than leave terrain without its buildings
```

A listed tile that is not in `buildings_pbf_dir` (or cannot be read) fails
it too: `Error: Building tile <name> listed in <list> is not in <dir>` (§9a).
Either failure removes a full run's `.abt` files (`[Download] Removed <n>
incomplete .abt file(s) …`) and prints no burn line and no `[Stats]` block
(a repair keeps its files unchanged, below).
Write the list last and atomically (complete under another name, then
renamed into place): it is read once, right after it is seen.

**Repair jobs honour `buildings_pbf_list`** exactly as a first pass does —
the caller puts the list on every pass, and it is the tile selection; only
the marker (`buildings_pbf_ready`) is pointless on a repair and still
ignored. A repair with the list: its fields are checked before any request
by the rules above (needs `buildings_pbf_dir`, which must exist; excludes
`buildings_pbf_ready`; absolute; `buildings_pbf_wait_s` 0 … 86400), with
the same messages; once its pixels are rewritten in memory, it waits for
the list when it is not there — the same `[Buildings] waiting for the
building tiles (<list path>)` / `[Buildings] building tiles ready after
<s.s> s` lines and timeout — then reads it and decodes exactly the listed
tiles, never the store's listing, and burns them onto the repaired pixels:
the `[Buildings] repair: <b> building(s), <p> repaired px raised` line and
the bytes of a repair whose `buildings_pbf_dir` holds exactly the listed
tiles. A list that does not come, or names a tile the directory does not
hold, fails the repair with the first pass's error before any file is
written: the existing `.abt` files are unchanged (a repair never removes
one). `[Stats] Downloaded: … in <s>s` leaves the wait out, as in a full
run. A repair that rewrote no pixel burns nothing and does not look for
the list. Without the list, a repair is unchanged: it reads
`buildings_pbf_dir` whole, as it finds it.

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

Consumed by the **Waveshed QGIS plugin** (`waveshed/core/binary_manager.py`) to
discover, verify and install engine binary updates, and by the waveshed.io
downloads page.

* **Producer:** the AETHER release workflow (`.github/workflows/release.yml`)
  via `python/release/` (`package_release.py` per platform → `make_manifest.py`,
  validated by `release_common.py`). It uploads the zips to the R2 bucket
  `waveshed-releases` (public at `https://releases.waveshed.io/`) as
  `vX.Y.Z/<filename>`, and the manifest as both `manifests/vX.Y.Z/latest.json`
  and `manifests/latest.json`.
* **Served at** `https://waveshed.io/releases/latest.json`, a relay of
  `manifests/latest.json` (AETHER_Web `routes/releases/latest.json/+server.ts`).
  The relay validates a parsed copy but **returns the upstream bytes verbatim**:
  the detached signature below covers the exact bytes, so any re-serialisation
  (key order, whitespace, escapes) would invalidate it. A relay that cannot
  validate answers 503 `{schema_version: 1, version: null, assets: []}`.

```jsonc
{
  "schema_version": 1,
  "version": "0.4.7",                       // engine semver (== aether_core Cargo version)
  "released_at": "2026-10-01",              // release DATE, YYYY-MM-DD (UTC)
  "changelog_url": "https://waveshed.io/downloads#changelog", // optional
  "eula_url": "https://waveshed.io/legal/aether-engine-eula.md", // optional
  "min_plugin_version": "0.1.0",            // optional; plugin warns if older
  "assets": [
    {
      "platform": "windows",                // windows | linux | macos
      "arch": "x64",                        // x64 | arm64
      "filename": "aether-0.4.7-windows-x64.zip",
      "url": "https://releases.waveshed.io/v0.4.7/aether-0.4.7-windows-x64.zip",
      "size_bytes": 22035144,
      "sha256": "<64 lowercase hex chars>"
    }
  ],
  "revocation": {                           // optional, see §10 / License-v3.md
    "key_id": 0,
    "seq": 1,
    "blob": "<base64 revocation.bin>"
  }
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `schema_version` | int | yes | `1`. Bump on breaking manifest changes. |
| `version` | string | yes | Semver `X.Y.Z` of the release. |
| `released_at` | string | yes | Release **date** `YYYY-MM-DD` (not a timestamp). The licence Worker derives a free key's `epoch_from` from it. |
| `changelog_url` | string | no | |
| `eula_url` | string | no | Overrides the plugin's default EULA URL. |
| `min_plugin_version` | string | no | Plugin warns when it is older (advisory). |
| `assets` | array | yes | Exactly windows/x64, linux/x64, macos/arm64 in that order (producer rule). |
| `assets[].platform` | enum | yes | `windows`\|`linux`\|`macos`. |
| `assets[].arch` | enum | yes | `x64`\|`arm64`. |
| `assets[].filename` | string | yes | `aether-X.Y.Z-<platform>-<arch>.zip`. |
| `assets[].url` | string | yes | `https://releases.waveshed.io/vX.Y.Z/<filename>`; relay and plugin refuse other hosts. |
| `assets[].size_bytes` | int | yes | > 0. |
| `assets[].sha256` | string | yes | Lowercase 64-hex digest. |
| `revocation` | object | no | `{key_id, seq, blob}`: signed cumulative revocation list; the plugin writes `blob` (base64) as `revocation.bin` next to the engine. |

* **`sha256` is fail-closed.** The plugin must compute the SHA-256 of the
  downloaded asset and **refuse to install** on any mismatch, missing digest, or
  absent asset for the running platform/arch. A download that cannot be verified
  is treated as a failure, never as "install anyway".

### 11.1 Detached signature (`latest.json.sig`)

Every release from the first signed engine release on also publishes
`manifests/vX.Y.Z/latest.json.sig` and `manifests/latest.json.sig`:

```json
{"alg": "ed25519", "context": "waveshed-manifest-v1", "public_key": "<64 hex>", "signature": "<base64, 64 bytes>"}
```

* Signed message = `b"waveshed-manifest-v1\0"` ‖ **the exact bytes of
  `latest.json`** as uploaded (and as relayed, see above).
* The key is the dedicated **manifest signing key** — not a licence key; the
  engine never trusts it. Consumers embed its public half and refuse a
  signature whose `public_key` they do not trust.
* Consumers fetch the **versioned** sig (`manifests/v{version}/latest.json.sig`,
  `version` taken from the manifest and checked to be `X.Y.Z` before it goes
  into a URL), so a manifest/sig pair cannot straddle a release upload.
* A bad signature is always fatal. A *missing* signature is tolerated only for
  manifests whose `version` is at or below the last unsigned release (0.4.7);
  anything newer without a valid signature is refused.

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
| `probe` (`aether-probe/1`), its GDAL cross-check | `crates/aether_converter/src/probe.rs`, `tools/probe_vs_gdal.py`, `schemas/probe_output.schema.json` |
| `[Tile]` lines (ingest coverage and voids; `download` voids), atomic tile replacement | `crates/aether_converter/src/{main.rs,ingest.rs}` (`TileFile`, `TileStats::void_px`), `crates/aether_converter/src/download.rs` (`print_tile_void_lines`) |
| `download` seam filter (`seam_filter`), `[Stats] Backfill:` / `[Backfill]` lines, derived-tile seam voids | `crates/aether_converter/src/download/seam.rs` (the rule and its constants), `crates/aether_converter/src/download.rs` (`TileDecoder`, `DownloadStats::note_backfill`), `crates/aether_converter/src/download/tile_cache.rs` (`Codec::seam_void`, `TileCache::open_for_run`); fixtures `crates/aether_converter/tests/fixtures/terrarium_seam_10e/` |
| `[Rust] run: … peak rss`, `[Stats] Peak RSS:` (the process's peak resident memory) | `crates/aether_converter/src/proc_usage.rs` (the OS reads, shared with the `[Rust] run:` CPU time and page faults), `crates/aether_converter/src/ingest.rs` (`run_line`), `crates/aether_converter/src/download.rs` (`peak_rss_line`, `DownloadStats::log_cli_summary`) |
| `buildings_pbf_list` — the list file and its checks, the decode of the listed tiles only, the list as the ready signal, the decode's start line | `crates/aether_converter/src/buildings.rs` (`read_pbf_list`, `load_pbf_building_list_with_progress`, `PbfSource`), `crates/aether_converter/src/download.rs` (`buildings_ready_marker`, `listed_store_exists`, `ReadySignal`, `wait_for_buildings_marker`), `crates/aether_converter/src/ingest/buildings_ready.rs` (`batch_markers`, `BuildingsHandOver`), `crates/aether_converter/src/main.rs` (`pbf_list_decode_line`, `decode_building_source`) |
| ATIL reader, GeoTIFF export | `crates/aether_export/src/main.rs` |
| `.vix` writer, ATIL reader, COG output | `crates/aether_aggregate/src/{aggregate.rs,reader.rs,writer.rs}` |
| `.vix` reader (consumer) | `MPT_SIGMA/…/core/vix_reader.py` |
| P2P CSV positional parse, PATH usage | `MPT_SIGMA/…/workers/optimization_worker.py`, `core/analysis_manager.py` |
