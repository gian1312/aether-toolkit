# Test data

Fixtures outside this folder:

| Files | Provenance |
|---|---|
| `../fixtures/terrarium_seam_10e/<z>_<x>_<y>.png` (11 tiles) | **Terrarium** terrain tiles exactly as the service returned them, fetched 2026-11-05 from `https://s3.amazonaws.com/elevation-tiles-prod/terrarium/` (AWS Open Data "Terrain Tiles", the former Mapzen service): z15 x 17292 … 17295 × y 11552 … 11553, z14 8646/5776 and 8647/5776, z13 4323/2888 — the 10°E seam near 46.8° N. Used by `test_seam_filter.rs` to pin the seam filter (CONTRACT changelog 49) against real bytes. Attribution, as the upstream [tilezen/joerd attribution](https://github.com/tilezen/joerd/blob/master/docs/attribution.md) requires for this area: Mapzen; Europe terrain data produced using Copernicus data and information funded by the European Union – EU-DEM layers; Austria terrain data © offene Daten Österreichs – Digitales Geländemodell (DGM); United States 3DEP and global GMTED2010 and SRTM terrain data courtesy of the U.S. Geological Survey. |

| File | Provenance |
|---|---|
| `ofm_z14_8651_5782.pbf` | OpenFreeMap planet vector tile `14/8651/5782` (Zernez, Graubünden, CH), fetched 2026-08-18 from `https://tiles.openfreemap.org/planet`. Map data © OpenStreetMap contributors, **ODbL 1.0**. Used by `canopy.rs` to pin the forest mask against real data instead of only synthetic squares. |
| `tin_roofs_z.fgb` | Two single-triangle **TIN Z** features (synthetic, Bern coordinates, roof Z 550 m / 560 m), written by GDAL 3.6's FlatGeobuf driver (`ogr.wkbTINZ` layer) — the exact encoding the Waveshed plugin's GDB→FGB conversion produces for swissBUILDINGS3D 3.0. Used by `test_buildings_fgb.rs` to pin that TIN geometry burns. |
| `lzw_edge_tiles.tif` | Synthetic 244×128 Float32 GeoTIFF (LV95, 2 m), `round(400 + x·y/200 + 3·sin((x+y)/5), 2)`, written by GDAL 3.6 with `COMPRESS=LZW TILED=YES BLOCKXSIZE=BLOCKYSIZE=32`. 244 is not a multiple of 32, so the right-edge tiles carry padding; with this data tiff 0.11.3 + weezl 0.1.12 fail `read_chunk` on tile 23 ("no lzw end code found"). Used by `test_lzw_edge_tiles.rs`. |
| `lzw_edge_tiles_deflate.tif` | The same pixels re-encoded with `COMPRESS=DEFLATE` (`gdal_translate`, same tiling) — the reference the LZW decode is compared against. |
