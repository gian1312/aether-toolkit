#!/usr/bin/env python3
"""Check `aether_converter probe` against GDAL, field by field.

Writes a matrix of GeoTIFFs with GDAL (every dtype x codec x predictor x
layout x byte order the converter claims, plus COG/internal overviews, masks,
BigTIFF, PixelIsPoint, rotation, scale/offset, NaN nodata, compound/geographic
CRSs, an IFD rewritten at the end of the file — and the files probe must
refuse: multi-band, palette, sub-byte, complex, user-defined CRS, GCPs, no
geotransform, not a TIFF), runs `probe` over all of them and compares every
field of every `ok: true` line with what GDAL reports for the same file
(CONTRACT §1.2 "probe"). Refused files must come back `ok: false` with the
expected reason.

Needs GDAL's Python bindings and numpy (unlike validate_fixtures.py, which is
stdlib-only). Exit 0 = every field equal.

    python3 tools/probe_vs_gdal.py [--bin PATH] [--dir SCRATCH] [--keep]

--bin defaults to the first built aether_converter under target*/release;
--dir defaults to target/probe_vs_gdal (gitignored), removed afterwards
unless --keep.
"""

import argparse
import itertools
import json
import math
import os
import shutil
import subprocess
import sys

import numpy as np
from osgeo import gdal, osr

gdal.UseExceptions()
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
W, H = 300, 203

DTYPES = {
    "Byte": (gdal.GDT_Byte, np.uint8, [], 255),
    "Int8": (gdal.GDT_Byte, np.int8, ["PIXELTYPE=SIGNEDBYTE"], -128),
    "UInt16": (gdal.GDT_UInt16, np.uint16, [], 65535),
    "Int16": (gdal.GDT_Int16, np.int16, [], -9999),
    "UInt32": (gdal.GDT_UInt32, np.uint32, [], 4294967295),
    "Int32": (gdal.GDT_Int32, np.int32, [], -9999),
    "UInt64": (gdal.GDT_UInt64, np.uint64, [], None),
    "Int64": (gdal.GDT_Int64, np.int64, [], None),
    "Float32": (gdal.GDT_Float32, np.float32, [], -9999),
    "Float64": (gdal.GDT_Float64, np.float64, [], -9999),
}
GDAL_DTYPE = {"Byte": "u8", "UInt16": "u16", "Int16": "i16", "UInt32": "u32", "Int32": "i32",
              "UInt64": "u64", "Int64": "i64", "Float32": "f32", "Float64": "f64", "Int8": "i8"}
CODE_OF = {"NONE": {1}, "LZW": {5}, "DEFLATE": {8, 32946}, "PACKBITS": {32773}, "ZSTD": {50000},
           "LERC": {34887}, "JPEG": {7}, "LZMA": {34925}, "WEBP": {50001}}


def srs_wkt(code):
    s = osr.SpatialReference()
    s.SetFromUserInput(code)
    return s.ExportToWkt()


def surface(dt):
    yy, xx = np.mgrid[0:H, 0:W]
    v = 520 + 40 * np.sin(xx / 17.0) * np.cos(yy / 23.0) + 0.37 * xx - 0.21 * yy
    if dt in ("Byte", "Int8"):
        v = (v - 450) % 120
    if dt == "Int16":
        v = (v - 600) * 7
    if dt in ("Int32", "Int64"):
        v = (v - 600) * 70001
    if dt.startswith("Float"):
        v = v + np.random.default_rng(1).normal(0, 0.3, v.shape)
    return v.astype(DTYPES[dt][1])


def create(path, dt, opts, gt=(2600000.0, 2.0, 0.0, 1200000.0, 0.0, -2.0), crs="EPSG:2056",
           nodata="default", w=W, h=H, bands=1, before=None, after=None):
    gtype, _, extra, nod = DTYPES[dt]
    ds = gdal.GetDriverByName("GTiff").Create(path, w, h, bands, gtype, list(opts) + extra)
    if before:
        before(ds)
    if gt is not None:
        ds.SetGeoTransform(gt)
    if crs is not None:
        ds.SetProjection(srs_wkt(crs) if not crs.startswith("WKT:") else crs[4:])
    for i in range(1, bands + 1):
        b = ds.GetRasterBand(i)
        nd = nod if nodata == "default" else nodata
        if nd is not None:
            b.SetNoDataValue(nd)
        a = surface(dt) if (w, h) == (W, H) else np.resize(surface(dt), (h, w))
        b.WriteArray(a)
    if after:
        after(ds)
    ds = None


def build(d):
    """Write the matrix into *d*: returns [(path, expectation)], where an
    expectation is {"tiled": bool} for files probe must read, or
    {"refuse": "<reason prefix>"} for files it must refuse."""
    files = []

    def add(name, exp, fn):
        p = os.path.join(d, name)
        try:
            fn(p)
        except RuntimeError as e:  # a combination this GDAL cannot write
            print(f"  skip {name}: {e}")
            return
        files.append((p, exp))

    for dt, comp, pred, tiled, endian in itertools.product(
            DTYPES, ["NONE", "LZW", "DEFLATE", "PACKBITS", "ZSTD"], [1, 2, 3], [True, False], ["LITTLE", "BIG"]):
        if comp in ("NONE", "PACKBITS") and pred != 1:
            continue
        if pred == 3 and not dt.startswith("Float"):
            continue
        if comp in ("PACKBITS", "ZSTD") and (endian == "BIG" or not tiled):
            continue
        opts = [f"COMPRESS={comp}", f"PREDICTOR={pred}", f"ENDIANNESS={endian}"]
        opts += ["TILED=YES", "BLOCKXSIZE=128", "BLOCKYSIZE=64"] if tiled else ["BLOCKYSIZE=7"]
        name = f"{dt}_{comp}_p{pred}_{'tiled' if tiled else 'strips'}_{endian}.tif"
        add(name, {"tiled": tiled}, lambda p, dt=dt, opts=opts: create(p, dt, opts))

    def cog(p):
        src = p + ".src.tif"
        create(src, "Float32", [], w=1100, h=900)
        gdal.Translate(p, src, format="COG", creationOptions=["BLOCKSIZE=256", "COMPRESS=DEFLATE", "PREDICTOR=YES"])
        os.remove(src)

    def addo(p, mask=False):
        create(p, "Int16", ["TILED=YES", "COMPRESS=LZW"], w=1000, h=800)
        ds = gdal.Open(p, gdal.GA_Update)
        if mask:
            gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", "YES")
            ds.CreateMaskBand(gdal.GMF_PER_DATASET)
            ds.GetRasterBand(1).GetMaskBand().Fill(255)
        ds.BuildOverviews("NEAREST", [2, 4, 8])
        ds = None
        gdal.SetConfigOption("GDAL_TIFF_INTERNAL_MASK", None)

    def ifd_at_end(p):
        create(p, "Float32", ["COMPRESS=DEFLATE"])
        ds = gdal.Open(p, gdal.GA_Update)
        ds.SetMetadataItem("NOTE", "x" * 20000)  # grows the IFD: rewritten at the end
        ds = None

    def point(p, gt=(2600000.0, 2.0, 0.0, 1200000.0, 0.0, -2.0)):
        create(p, "Float32", ["TILED=YES"], gt=gt, before=lambda ds: ds.SetMetadataItem("AREA_OR_POINT", "Point"))

    def scale_offset(p, scale=0.1, offset=-100.0):
        def so(ds):
            b = ds.GetRasterBand(1)
            if scale is not None:
                b.SetScale(scale)
            if offset is not None:
                b.SetOffset(offset)
        create(p, "Int16", ["COMPRESS=LZW"], after=so)

    rot = (2600000.0, 2.0, 0.5, 1200000.0, 0.25, -2.0)
    add("cog_overviews.tif", {"tiled": True}, cog)
    add("gdaladdo_internal.tif", {"tiled": True}, addo)
    add("gdaladdo_internal_mask.tif", {"tiled": True}, lambda p: addo(p, mask=True))
    add("ifd_at_end.tif", {"tiled": False}, ifd_at_end)
    add("bigtiff_tiled.tif", {"tiled": True}, lambda p: create(p, "Float32", ["BIGTIFF=YES", "TILED=YES", "COMPRESS=DEFLATE", "PREDICTOR=3"]))
    add("bigtiff_strips_be.tif", {"tiled": False}, lambda p: create(p, "Int16", ["BIGTIFF=YES", "ENDIANNESS=BIG", "COMPRESS=LZW", "PREDICTOR=2"]))
    add("pixel_is_point.tif", {"tiled": True}, point)
    add("pixel_is_point_rotated.tif", {"tiled": True}, lambda p: point(p, rot))
    add("rotated.tif", {"tiled": False}, lambda p: create(p, "Float32", [], gt=rot))
    add("scale_offset.tif", {"tiled": False}, scale_offset)
    add("scale_only.tif", {"tiled": False}, lambda p: scale_offset(p, 0.5, None))
    add("geographic_4326_nonsquare.tif", {"tiled": True},
        lambda p: create(p, "Float32", ["TILED=YES"], gt=(7.0, 0.000417, 0.0, 47.0, 0.0, -0.000278), crs="EPSG:4326"))
    add("no_crs.tif", {"tiled": False}, lambda p: create(p, "Int16", [], crs=None))
    add("nan_nodata.tif", {"tiled": True}, lambda p: create(p, "Float32", ["TILED=YES"], nodata=float("nan")))
    add("no_nodata.tif", {"tiled": False}, lambda p: create(p, "Float32", [], nodata=None))
    add("compound_2056_5728.tif", {"tiled": False}, lambda p: create(p, "Float32", ["COMPRESS=LZW"], crs="EPSG:2056+5728"))
    add("utm_32632.tif", {"tiled": False}, lambda p: create(p, "Float32", [], gt=(500000.0, 30.0, 0.0, 5300000.0, 0.0, -30.0), crs="EPSG:32632"))
    add("webmercator_3857.tif", {"tiled": True}, lambda p: create(p, "Float32", ["TILED=YES"], gt=(800000.0, 20.0, 0.0, 5900000.0, 0.0, -20.0), crs="EPSG:3857"))
    add("lambert93_2154.tif", {"tiled": False}, lambda p: create(p, "Float32", [], gt=(650000.0, 5.0, 0.0, 6860000.0, 0.0, -5.0), crs="EPSG:2154"))
    add("single_strip.tif", {"tiled": False}, lambda p: create(p, "Float32", [f"BLOCKYSIZE={H}"]))
    add("single_strip_big.tif", {"tiled": False}, lambda p: create(p, "Float32", ["BLOCKYSIZE=2100"], w=3000, h=2100))
    add("lerc.tif", {"tiled": True}, lambda p: create(p, "Float32", ["COMPRESS=LERC", "TILED=YES"]))
    add("lerc_deflate.tif", {"tiled": True}, lambda p: create(p, "Float32", ["COMPRESS=LERC_DEFLATE", "TILED=YES"]))
    add("jpeg_byte.tif", {"tiled": True}, lambda p: create(p, "Byte", ["COMPRESS=JPEG", "TILED=YES"]))
    add("lzma.tif", {"tiled": False}, lambda p: create(p, "Int16", ["COMPRESS=LZMA"]))

    # Files probe must refuse (the caller falls back to GDAL for them).
    add("multi_band_rgb.tif", {"refuse": "multi-band"}, lambda p: create(p, "Byte", ["PHOTOMETRIC=RGB"], bands=3))
    add("gray_alpha.tif", {"refuse": "multi-band"}, lambda p: create(p, "Byte", ["ALPHA=YES"], bands=2))

    def palette(p):
        def ct(ds):
            t = gdal.ColorTable()
            for i in range(256):
                t.SetColorEntry(i, (i, 255 - i, 0, 255))
            ds.GetRasterBand(1).SetRasterColorTable(t)
        create(p, "Byte", [], before=ct)
    add("palette.tif", {"refuse": "palette"}, palette)
    add("one_bit.tif", {"refuse": "sub-byte"}, lambda p: create(p, "Byte", ["NBITS=1"], nodata=None))
    add("four_bit.tif", {"refuse": "sub-byte"}, lambda p: create(p, "Byte", ["NBITS=4"], nodata=None))
    add("twelve_bit.tif", {"refuse": "unsupported sample type"}, lambda p: create(p, "UInt16", ["NBITS=12"], nodata=None))
    add("float16.tif", {"refuse": "unsupported sample type"}, lambda p: create(p, "Float32", ["NBITS=16"]))

    def cint16(p):
        ds = gdal.GetDriverByName("GTiff").Create(p, 16, 16, 1, gdal.GDT_CInt16)
        ds.SetGeoTransform((0, 1, 0, 0, 0, -1)); ds.SetProjection(srs_wkt("EPSG:2056")); ds = None
    add("complex.tif", {"refuse": "complex"}, cint16)
    add("miniswhite.tif", {"refuse": "photometric"}, lambda p: create(p, "Byte", ["PHOTOMETRIC=MINISWHITE"]))
    tm = ("+proj=tmerc +lat_0=0 +lon_0=7.5 +k=0.99987 +x_0=1000000 +y_0=-4000000 "
          "+ellps=GRS80 +units=m +no_defs")
    s = osr.SpatialReference(); s.ImportFromProj4(tm)
    add("user_defined_tm.tif", {"refuse": "user-defined CRS"}, lambda p: create(p, "Float32", [], crs="WKT:" + s.ExportToWkt()))
    g = osr.SpatialReference(); g.ImportFromProj4("+proj=longlat +a=6378000 +rf=300 +no_defs")
    add("user_defined_geog.tif", {"refuse": "user-defined CRS"},
        lambda p: create(p, "Float32", [], gt=(7.0, 0.001, 0.0, 47.0, 0.0, -0.001), crs="WKT:" + g.ExportToWkt()))

    def gcps(p):
        ds = gdal.GetDriverByName("GTiff").Create(p, 64, 64, 1, gdal.GDT_Int16)
        gl = [gdal.GCP(2600000 + x * 2, 1200000 - y * 2, 0, x, y) for x, y in [(0, 0), (63, 0), (0, 63), (63, 63)]]
        ds.SetGCPs(gl, srs_wkt("EPSG:2056")); ds = None
    add("gcps.tif", {"refuse": "GCP georeferencing"}, gcps)
    add("no_georef.tif", {"refuse": "no geotransform"}, lambda p: create(p, "Int16", [], gt=None, crs=None))

    def png(p):
        src = gdal.GetDriverByName("MEM").Create("", 16, 16, 1, gdal.GDT_Byte)
        gdal.GetDriverByName("PNG").CreateCopy(p, src)
    add("not_a_tiff.png", {"refuse": "not a TIFF"}, png)
    add("text.tif", {"refuse": "not a TIFF"}, lambda p: open(p, "w").write("hello\n"))
    add("empty.tif", {"refuse": "not a TIFF"}, lambda p: open(p, "wb").close())
    add("tile.abt", {"refuse": "not a TIFF"},
        lambda p: shutil.copy(os.path.join(REPO, "fixtures", "formats", "tiny_16x16.abt"), p))
    add("folder.tif", {"refuse": "not a regular file"}, lambda p: os.mkdir(p))
    files.append((os.path.join(d, "missing.tif"), {"refuse": "cannot open"}))
    return files


def num_eq(a, b):
    if isinstance(a, float) and isinstance(b, float) and math.isnan(a) and math.isnan(b):
        return True
    return a == b


def gdal_view(path):
    """The probe fields as GDAL reports them for *path*."""
    ds = gdal.Open(path)
    b = ds.GetRasterBand(1)
    ims = ds.GetMetadata("IMAGE_STRUCTURE") or {}
    srs = ds.GetSpatialRef()
    epsg = None
    if srs is not None:
        code = srs.GetAuthorityCode("PROJCS") if srs.IsProjected() else srs.GetAuthorityCode("GEOGCS")
        epsg = int(code) if code else None
    nod = b.GetNoDataValue()
    if nod is not None and math.isnan(nod):
        nod = "nan"
    gt = list(ds.GetGeoTransform())
    dtype = GDAL_DTYPE[gdal.GetDataTypeName(b.DataType)]
    if dtype == "u8" and b.GetMetadataItem("PIXELTYPE", "IMAGE_STRUCTURE") == "SIGNEDBYTE":
        dtype = "i8"
    comp = ims.get("COMPRESSION", "NONE")
    bw, bh = b.GetBlockSize()
    # The bottom-right block's end (0 when GDAL calls it sparse: no offset).
    bx, by = -(-ds.RasterXSize // bw) - 1, -(-ds.RasterYSize // bh) - 1
    boff = b.GetMetadataItem(f"BLOCK_OFFSET_{bx}_{by}", "TIFF")
    bsize = b.GetMetadataItem(f"BLOCK_SIZE_{bx}_{by}", "TIFF")
    last_end = int(boff) + int(bsize) if boff and bsize and int(bsize) else 0
    return {
        "width": ds.RasterXSize, "height": ds.RasterYSize, "bands": ds.RasterCount, "dtype": dtype,
        "compression": "LERC" if comp.startswith("LERC") else comp,
        "predictor": int(ims.get("PREDICTOR", 1)),
        "block": (bw, bh),
        "epsg": epsg, "geotransform": gt, "rotated": gt[2] != 0.0 or gt[4] != 0.0,
        "pixel_is_point": ds.GetMetadataItem("AREA_OR_POINT") == "Point",
        "nodata": nod, "scale": b.GetScale(), "offset": b.GetOffset(),
        "overviews": b.GetOverviewCount(),
        "ifd_offset": int(b.GetMetadataItem("IFD_OFFSET", "TIFF")),
        "last_chunk_end": last_end,
    }


def compare(path, exp, line):
    errs = []
    st = os.stat(path) if os.path.exists(path) else None

    def eq(field, want):
        got = line.get(field, "<absent>")
        if not num_eq(got, want):
            errs.append(f"{field}: probe {got!r} != expected {want!r}")

    if line.get("schema") != "aether-probe/1":
        errs.append(f"schema {line.get('schema')!r}")
    if st is not None:
        eq("size", st.st_size)
        eq("mtime", int(st.st_mtime))
        eq("mtime_ns", st.st_mtime_ns)
    if "refuse" in exp:
        if line.get("ok") is not False:
            errs.append(f"expected ok:false ({exp['refuse']}), got {line}")
        elif not line.get("reason", "").startswith(exp["refuse"]):
            errs.append(f"reason {line.get('reason')!r} does not start with {exp['refuse']!r}")
        return errs
    if line.get("ok") is not True:
        return [f"expected ok:true, got {line}"]
    g = gdal_view(path)
    for f in ("width", "height", "bands", "dtype", "predictor", "epsg", "rotated",
              "pixel_is_point", "nodata", "overviews", "ifd_offset", "last_chunk_end"):
        eq(f, g[f])
    eq("proj", None)
    # null = no GDAL_METADATA item = the identity. GDAL reports an explicit
    # 1.0 / 0.0 for a file with a vertical CRS even without the items.
    for f, identity in (("scale", 1.0), ("offset", 0.0)):
        if not (line[f] == g[f] or (line[f] is None and g[f] in (None, identity))):
            errs.append(f"{f}: probe {line[f]!r} != GDAL {g[f]!r}")
    if line["compression"]["name"] != g["compression"]:
        errs.append(f"compression: probe {line['compression']} != GDAL {g['compression']}")
    if line["compression"]["code"] not in CODE_OF.get(g["compression"], set()):
        errs.append(f"compression code {line['compression']['code']} is not GDAL's {g['compression']}")
    if line["geotransform"] != g["geotransform"]:
        errs.append(f"geotransform: probe {line['geotransform']} != GDAL {g['geotransform']}")
    eq("tiled", exp["tiled"])
    block = (line["tile_w"], line["tile_h"])
    if not exp["tiled"] and line["tile_h"] == line["height"] and line["compression"]["code"] == 1:
        # One uncompressed strip: libtiff (STRIPCHOP) hands GDAL ~8 KB virtual
        # strips; probe reports the file's own RowsPerStrip.
        row_bytes = line["width"] * int(line["dtype"][1:]) // 8
        block = (line["width"], max(1, 8192 // row_bytes) if row_bytes <= 8192 else 1)
    if block != g["block"]:
        errs.append(f"block: probe {(line['tile_w'], line['tile_h'])} != GDAL {g['block']}")
    return errs


def find_bin():
    for rel in ("target/release", "target-linux/release", "target/debug", "target-linux/debug"):
        p = os.path.join(REPO, rel, "aether_converter")
        for cand in (p, p + ".exe"):
            if os.path.exists(cand):
                return cand
    sys.exit("no aether_converter binary found; build it (cargo build --release) or pass --bin")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--bin", default=os.environ.get("AETHER_CONVERTER"))
    ap.add_argument("--dir", default=os.path.join(REPO, "target", "probe_vs_gdal"))
    ap.add_argument("--keep", action="store_true")
    a = ap.parse_args()
    exe = a.bin or find_bin()
    shutil.rmtree(a.dir, ignore_errors=True)
    os.makedirs(a.dir)
    try:
        files = build(a.dir)
        aux = [f for f in os.listdir(a.dir) if f.endswith(".aux.xml") or f.endswith(".ovr")]
        if aux:
            sys.exit(f"GDAL wrote sidecars that would mask header facts: {aux}")
        lst = os.path.join(a.dir, "list.txt")
        with open(lst, "w", encoding="utf-8") as f:
            f.write("".join(p + "\n" for p, _ in files))
        r = subprocess.run([exe, "probe", "--list", lst], capture_output=True, text=True)
        if r.returncode != 0:
            sys.exit(f"probe failed ({r.returncode}): {r.stderr}")
        lines = [json.loads(x) for x in r.stdout.splitlines()]
        if [x["path"] for x in lines] != [p for p, _ in files]:
            sys.exit("probe lines are not one per listed file, in list order")
        bad = 0
        for (p, exp), line in zip(files, lines):
            errs = compare(p, exp, line)
            if errs:
                bad += 1
                print(f"MISMATCH {os.path.basename(p)}:")
                for e in errs:
                    print(f"    {e}")
        n_ok = sum(1 for x in lines if x["ok"])
        print(r.stderr.strip().splitlines()[-1])
        print(f"{len(files)} files ({n_ok} ok, {len(files) - n_ok} refused): "
              f"{'all fields equal GDAL' if not bad else f'{bad} file(s) differ'}")
        sys.exit(1 if bad else 0)
    finally:
        if not a.keep:
            shutil.rmtree(a.dir, ignore_errors=True)


if __name__ == "__main__":
    main()
