# Test data

| File | Provenance |
|---|---|
| `ofm_z14_8651_5782.pbf` | OpenFreeMap planet vector tile `14/8651/5782` (Zernez, Graubünden, CH), fetched 2026-08-18 from `https://tiles.openfreemap.org/planet`. Map data © OpenStreetMap contributors, **ODbL 1.0**. Used by `canopy.rs` to pin the forest mask against real data instead of only synthetic squares. |
| `tin_roofs_z.fgb` | Two single-triangle **TIN Z** features (synthetic, Bern coordinates, roof Z 550 m / 560 m), written by GDAL 3.6's FlatGeobuf driver (`ogr.wkbTINZ` layer) — the exact encoding the Waveshed plugin's GDB→FGB conversion produces for swissBUILDINGS3D 3.0. Used by `test_buildings_fgb.rs` to pin that TIN geometry burns. |
| `lzw_edge_tiles.tif` | Synthetic 244×128 Float32 GeoTIFF (LV95, 2 m), `round(400 + x·y/200 + 3·sin((x+y)/5), 2)`, written by GDAL 3.6 with `COMPRESS=LZW TILED=YES BLOCKXSIZE=BLOCKYSIZE=32`. 244 is not a multiple of 32, so the right-edge tiles carry padding; with this data tiff 0.11.3 + weezl 0.1.12 fail `read_chunk` on tile 23 ("no lzw end code found"). Used by `test_lzw_edge_tiles.rs`. |
| `lzw_edge_tiles_deflate.tif` | The same pixels re-encoded with `COMPRESS=DEFLATE` (`gdal_translate`, same tiling) — the reference the LZW decode is compared against. |
