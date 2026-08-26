# Test data

| File | Provenance |
|---|---|
| `ofm_z14_8651_5782.pbf` | OpenFreeMap planet vector tile `14/8651/5782` (Zernez, Graubünden, CH), fetched 2026-08-18 from `https://tiles.openfreemap.org/planet`. Map data © OpenStreetMap contributors, **ODbL 1.0**. Used by `canopy.rs` to pin the forest mask against real data instead of only synthetic squares. |
| `tin_roofs_z.fgb` | Two single-triangle **TIN Z** features (synthetic, Bern coordinates, roof Z 550 m / 560 m), written by GDAL 3.6's FlatGeobuf driver (`ogr.wkbTINZ` layer) — the exact encoding the Waveshed plugin's GDB→FGB conversion produces for swissBUILDINGS3D 3.0. Used by `test_buildings_fgb.rs` to pin that TIN geometry burns. |
