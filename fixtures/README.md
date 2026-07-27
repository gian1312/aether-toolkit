# Fixtures

Example inputs and binary goldens for the interfaces described in
[`../docs/CONTRACT.md`](../docs/CONTRACT.md). They exist so contributors can see
concrete, valid instances of each contract surface and so a change that breaks
the contract also visibly breaks a fixture.

> **These are spec-derived synthetic goldens, not engine-generated output.**
> The binary files in `formats/` were written by
> [`../tools/make_fixtures.py`](../tools/make_fixtures.py) directly from the
> byte layouts documented in the contract — no `aether_core` run was involved.
> They are correct *by construction against the spec*, which is exactly what you
> want for catching accidental contract drift, but they do **not** prove the
> engine actually emits these bytes.
>
> **TODO:** add engine-generated goldens produced by a real `aether_core` /
> `aether_aggregate` run in private CI (a tiny coverage job → `.bit`+sidecar+`.tif`,
> a P2P job → `.csv`, an aggregate run → `.vix`) and check them in alongside
> these synthetic ones. Until then, treat `formats/` as a spec mirror.

## Layout

```
fixtures/
├── jobs/                    # JSON (+ CSV/text) inputs to the CLIs
│   ├── coverage_los.json    # SINGLE coverage, 1BIT_LOS
│   ├── coverage_itm.json    # SINGLE coverage, ITM, full propagation block
│   ├── batch_p2p.json       # BATCH_P2P job (pairs analysis.batch_file …)
│   ├── batch_links.csv      # … with this batch_links.csv
│   ├── path.json            # PATH job (MPT_SIGMA-style; analysis.path_file …)
│   ├── waypoints.txt        # … with this waypoint file
│   ├── ingest_job.json      # aether_converter ingest --job-file
│   └── download_job.json    # aether_converter download --job-file
├── formats/                 # binary goldens (generated, committed)
│   ├── tiny_16x16.abt       # .abt terrain tile, v1 R16SINT, 16x16 px (§6)
│   ├── tiny_los.bit         # ATIL container, 1 tile, 1BIT_LOS, 512x512 (§7)
│   └── tiny_los.json        # sidecar matching tiny_los.bit (§5a)
└── keys/                     # license-key structural goldens (generated) — see keys/README.md
    ├── README.md            # byte layout + worked example + why no real key
    ├── v1_unsigned.blob     # decoded v1 key blob, 84 B, zero signature (§10)
    ├── v2_nodelock_unsigned.blob  # decoded v2 key, 116 B, non-zero fingerprint (§10)
    └── v2_unlocked_unsigned.blob  # decoded v2 key, 116 B, all-zero fingerprint (§10)
```

> **The `keys/` blobs are NOT valid license keys.** They are the *decoded*
> (post-Base58) blobs with a **zero-filled signature**, so the validator can
> check the v1/v2 byte layout without the vendor private key (which a public
> repo must not contain). See [`keys/README.md`](keys/README.md).

## Tools

* **Regenerate the binary goldens** (deterministic — must reproduce identical
  bytes):

  ```sh
  python3 tools/make_fixtures.py
  ```

* **Validate everything** against the contract (structural checks of the JSON
  jobs; byte-by-byte checks of the binaries). Exits `0` on success:

  ```sh
  python3 tools/validate_fixtures.py
  ```

Both scripts are stdlib-only (no third-party dependencies) and are not yet wired
into CI.
