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
  tile(s), <b> building(s) in <s.s> s` — see §9a *Building-tile decode*.
  Neither contains `Progress:`.
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
  not part of it. In a batch the line precedes the tile's `[Tile]` line. A
  converter before 0.2.8 ends the line at `bands <n>` and prints it when
  sampling ends. An `ingest` process prints at exit — after the batch
  result, also when the run fails — on stdout:

  ```
  [Rust] run: wall <s>s, cpu user <s>s sys <s>s, page faults <n>, threads <n>, blocked ~<s>s
  ```

  (`getrusage` on Unix: minor + major faults; `GetProcessTimes` /
  `GetProcessMemoryInfo` `PageFaultCount` on Windows). `threads` is the
  batch's worker-pool size (the `[Rust] Rayon Pool:` count); the field and
  its comma are omitted when no batch ran (a single-job file). `blocked`
  (≥ 0.2.8) is wall × threads − (user + sys), floored at 0: thread time
  spent waiting rather than computing (a single job counts rayon's global
  pool). Neither line contains `Progress:` or starts with `[Tile]`.

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
  ignore the variable.
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
  ends with `<n> of <total> tiles failed to convert`.
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

  Additive lines (CLI `download`, changelog 30, 39; informational, match the
  prefix):

  ```
  [Download] RAM budget: <mb> MB, strip rows: <r> (<a> GB available at start; fixed for the run)
  [Download] RAM budget: <mb> MB, strip rows: <r> (re-sized for <e> px source tiles)
  ```

  The first replaces `[Download] RAM: <a> GB available, ~<m> MB/strip,
  prefetch depth: <d>`; the second appears at most once, when the source
  tile edge `<e>` is learnt only after the first strip was sized for 512 px
  (its first tile failed or was out for more than 5 s) and that changes the
  strip rows. Builds before 0.2.9 (changelog 39) re-read the budget before
  every strip and printed `(<a> GB available; re-evaluated before every
  strip)` and, on every change of the rows, `(<a> GB available)`. Decoded
  source tiles (`edge² × 4` bytes each) are held from arrival until the
  assembly has copied their tile-row into its grid, so the run budgets them:
  budget = min(available RAM / 4, 2 GiB), at least 256 MiB, taken once —
  after the run's first tile, before any strip is sized — and fixed for the
  run; strip rows = clamp(budget / 2 / (`<nx>` × edge² × 4 B), 1, 32), so
  two strips fit (edge = that of the run's first tile, fetched alone before
  any strip is sized — changelog 36; 512 only while that tile has failed or
  is out for more than 5 s); a strip starts only when its bytes plus those
  launched strips still hold fit the budget (or none is outstanding), and
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

---

## 2. Environment variables

| Variable | Read by | Purpose |
|----------|---------|---------|
| `AETHER_LICENSE` | `aether_core` (proprietary build) | Base58 license/API key — **v1 or v2** (§10). Proprietary builds **refuse to run without a valid, unexpired key**. If unset, the engine falls back to reading a `license.key` file (see below). |
| `RUST_LOG` | `aether_core`, toolkit CLIs | `env_logger` level filter. **Must be `info` (or lower)** for the log-line progress diagnostics of §1.1(b) to appear. Markers in §1.1(a) do not depend on it. |
| `AETHER_DIAG_FILE` | `aether_converter` (`ingest`, `download`) | Path of a file the per-tile `[Rust] tile …` and per-strip `[Strip …]`/`[StripMem …]` diagnostic lines (and, ≥ 0.2.9, `download`'s once-per-run `[Download] launch gate closed for …`, changelog 39) are appended to instead of the console (§1.2, changelog 37; `aether_converter` ≥ 0.2.8, older builds ignore it). Unset or empty = console as before. Parsed lines and `[Rust] run:` never move. |
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
`[Rust] Decoding building tiles in "<dir>" (once for the run)...`. The
directory's tile files — every name `{z}_{x}_{y}.pbf` or `{z}_{x}_{y}.mvt`;
anything else is ignored — are taken in **name order** (byte-wise) and read
and decoded in parallel on the batch's worker pool (the `[Rust] Rayon Pool:`
threads). The result is exactly what a walk over that order, one file after
the other, gives: the same building list in the same order (so the duplicate
filter keeps the same copies), the same counts, the same warnings. On NTFS,
name order is also the order the sequential decoder before it walked
(changelog 44). On stdout, the stream of `[Rust] Progress:`:

```
[Rust] Decoding building tiles: <n>/<m>
[Rust] Building tiles decoded: <m> tile(s), <b> building(s) in <s.s> s
```

* `<m>` is the directory's tile files, decoded or not; `<n>` those finished
  (decoded, skipped or failed). A progress line comes at every multiple of
  1,000 and once at `<n>` = `<m>` — one line, when `<m>` is a multiple of
  1,000 — in increasing `<n>`, each flushed as printed; none for a directory
  without tile files.
* The summary line follows when the decode and the duplicate filter are
  done: `<b>` = buildings decoded **before** the duplicate filter (the
  `<n> building(s)` of the per-tile `[Info] buildings_pbf_dir:` line),
  `<s.s>` = wall seconds of the whole load, one decimal. It is the line
  after the decode's last line (a progress line, or the warnings below) and
  comes before any tile is sampled.
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
| `max_connections` | usize? | optional | Concurrent HTTP connections. Default **256** when absent. The CLI `download` runs a fixed pool of this many requests (changelog 30); its window is cut by 25 % in a second only when ≥ 3 responses in that second were HTTP 429 or 503 (floor `max(max_connections / 4, 16)`, at most `max_connections`), and grows by 10 % per second without such a burst back to `max_connections`. Timeouts, connection resets and slow bodies never move it, and a cut only gates new requests. A clean link therefore runs at exactly `max_connections`. Reported by `[Stats] Connection window:` and the progress line's `window` field (§1.2). On unix the CLI `download` first makes room for `2 × max_connections + 96` open descriptors by raising its soft open-file limit (best effort); only where even the raised limit is lower does it run `(limit − 96) / 2` connections instead, printing `[Download] open-file limit …` (changelog 36). A full run (not a repair) also keeps its output files open for the run, so the raise asks for one descriptor per output tile on top; where they do not fit beside the connections' descriptors, the connections keep theirs and each write opens its file instead, printing `[Download] open-file limit <l>: <n> output tiles are opened for each write instead of held open (<d> descriptors would not fit)` (changelog 39). |
| `buildings_pbf_dir` | string? | optional | Directory of `{z}_{x}_{y}.pbf` vector tiles, same encoding and same **building height ladder** (§9a) as §9a's `buildings_pbf_dir`. When present, buildings are fused onto the finished `.abt` tiles as a post-pass once the download completes. All tiles in the directory must share **one** zoom; a mixed-zoom directory fails with `buildings_pbf_dir mixes zoom levels`. Absent means terrain only. |
| `fetch_bounds` | object? | optional | `{south, north, west, east}` in degrees (additive, 2026-09-05). Only source tiles intersecting it are fetched/decoded; every pixel outside keeps the `-9999` void pre-fill. Output grid, geometry and `.abt` layout unchanged — this trims the fetch set, nothing else. **In-memory downloader only**: the file-based `download` CLI rejects jobs carrying it (`fetch_bounds is not supported by the file-based download path …`), because its sparse writer would read skipped rows as 0 m sea level. Callers pass the area they actually need, inflated ≥ 2 % (covers the engine's own 1 % bounds margin). |
| `tiles` | array of `SubTileSpec` | **required** | One entry per output `.abt`. |
| `tile_deadline_s` | number? | optional | Additive, changelog 28, `aether_converter` ≥ 0.2.6. Wall-clock cap per source tile, in seconds, across all its attempts, back-offs and `Retry-After` waits; absent → **20** (changelog 30; was 60). At expiry the tile stops as a `timeout` failure and goes to the parent fill (`[Degraded] … reason=timeout` when an ancestor fills it). Must be a positive number ≤ 86400, else the job fails `tile_deadline_s must be a positive number of seconds, got <v>` before any request. CLI `download` only; the in-memory downloaders ignore it. |
| `repair_source_tiles` | array of `[z, x, y]`? | optional | Additive, changelog 28, `aether_converter` ≥ 0.2.6 (gate on `--version`, §1.5: older builds ignore the field and download the whole job). Fetch **only** these source tiles and patch them into the job's **existing** `.abt` files — see *Repair jobs* below. |
| `cache_dir` | string? | optional | Additive, changelog 38, `aether_converter` ≥ 0.2.8 (older builds ignore it). Directory of the raw source-tile cache, shared by every job, service and zoom that names it and by concurrent converter processes; absent, `null` or `""` → no cache. Every source tile is looked up there before any request and every HTTP 200 body that decoded and every 404 the run fetches is written there; since 0.2.9 (changelog 40) a tile without an entry is derived from the cached tiles of up to four finer zooms when they cover it, without a request — see *Raw source-tile cache* below. A directory that cannot be used is one `[Warn]` line, never an error. CLI `download` only (repair jobs refetch their listed tiles and rewrite their entries); the in-memory downloaders ignore it. |

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
strips' share of the RAM budget is reckoned with the real edge; 512 px is
assumed only while that tile has failed or is out for more than 5 s
(changelog 36).

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
| arrived | — (in-tile voids may be filled) | the tile | counted | — | — | — |
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
   tiles — preceded by
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
   and already-built pixels.

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
outlives its writer). A pack another writer holds for 10 s is not written by
this run (one `[Warn] cache_dir …` line per run); the run goes on. Several
converter processes may use one cache at once. **Reading** takes no lock and
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
refetches the tile (rewriting the entry as status 1).

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
their intermediates; the second only when the parent fill asked for coarser
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
| `[Tile]` line, atomic tile replacement | `crates/aether_converter/src/{main.rs,ingest.rs}` (`TileFile`) |
| ATIL reader, GeoTIFF export | `crates/aether_export/src/main.rs` |
| `.vix` writer, ATIL reader, COG output | `crates/aether_aggregate/src/{aggregate.rs,reader.rs,writer.rs}` |
| `.vix` reader (consumer) | `MPT_SIGMA/…/core/vix_reader.py` |
| P2P CSV positional parse, PATH usage | `MPT_SIGMA/…/workers/optimization_worker.py`, `core/analysis_manager.py` |
