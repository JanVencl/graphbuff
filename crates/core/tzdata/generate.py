"""Builds tzdata.bin, the time zone table graphbuff-core embeds, from the IANA time zone
database (public domain) as the `tzdata` Python package ships it (compiled TZif files).

    python -m pip install --upgrade tzdata
    python crates/core/tzdata/generate.py

Run it again when a new tzdata release changes a zone. The format (all little-endian):

    "GBTZ" u8:format=1  u8:len version
    u16:zones   per zone, by name:  u8:len name  u16:data
    u16:datas   per data (zones that are links share one):
        u8:offsets  i32 offset (seconds east of UTC) per offset
        u8:initial  (offset index before the first transition)
        u32:transitions  i64 UTC second per transition, then u8 offset index per transition
        u8:len POSIX TZ rule for after the last transition (may be empty)
"""

import os
import struct
import sys

import tzdata

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "tzdata.bin")


def read_tzif(path):
    """(utc offsets per type, transition times, type per transition, footer) of a TZif file."""
    with open(path, "rb") as f:
        data = f.read()
    if data[:4] != b"TZif":
        raise ValueError(f"{path}: not a TZif file")
    version = data[4]

    def counts(at):
        return struct.unpack(">6l", data[at + 20:at + 44])

    isut, isstd, leap, timecnt, typecnt, charcnt = counts(0)
    if version == 0:
        raise ValueError(f"{path}: TZif version 1 has no 64-bit data")
    # Skip the 32-bit block; the 64-bit one follows with its own header.
    at = 44 + timecnt * 5 + typecnt * 6 + charcnt + leap * 8 + isstd + isut
    isut, isstd, leap, timecnt, typecnt, charcnt = counts(at)
    at += 44
    times = list(struct.unpack(f">{timecnt}q", data[at:at + timecnt * 8]))
    at += timecnt * 8
    idx = list(data[at:at + timecnt])
    at += timecnt
    types = []
    for i in range(typecnt):
        off, _dst, _abbr = struct.unpack(">lBB", data[at + i * 6:at + i * 6 + 6])
        types.append(off)
    at += typecnt * 6 + charcnt + leap * 12 + isstd + isut
    footer = data[at:].strip(b"\n").decode("ascii")
    return types, times, idx, footer


def zone_names(root):
    with open(os.path.join(root, "..", "zones"), encoding="utf-8") as f:
        return sorted(line.strip() for line in f if line.strip())


def encode(types, times, idx, footer):
    # Offsets used, in first-use order; the initial one is type 0 (RFC 8536).
    offsets = []

    def index(o):
        if o not in offsets:
            offsets.append(o)
        return offsets.index(o)

    initial = index(types[0])
    # Drop transitions that keep the offset (only the abbreviation or DST flag changes).
    kept_times, kept_idx, last = [], [], types[0]
    for t, i in zip(times, idx):
        if types[i] != last:
            kept_times.append(t)
            kept_idx.append(index(types[i]))
            last = types[i]
    out = bytearray()
    out += struct.pack("<B", len(offsets)) + struct.pack(f"<{len(offsets)}l", *offsets)
    out += struct.pack("<BI", initial, len(kept_times))
    out += struct.pack(f"<{len(kept_times)}q", *kept_times) + bytes(kept_idx)
    out += struct.pack("<B", len(footer)) + footer.encode("ascii")
    return bytes(out)


def main():
    root = os.path.join(os.path.dirname(tzdata.__file__), "zoneinfo")
    names = zone_names(root)
    datas, data_index, zones = [], {}, []
    for name in names:
        blob = encode(*read_tzif(os.path.join(root, *name.split("/"))))
        if blob not in data_index:
            data_index[blob] = len(datas)
            datas.append(blob)
        zones.append((name, data_index[blob]))
    out = bytearray(b"GBTZ" + bytes([1]))
    version = tzdata.IANA_VERSION.encode("ascii")
    out += bytes([len(version)]) + version
    out += struct.pack("<H", len(zones))
    for name, d in zones:
        n = name.encode("ascii")
        out += bytes([len(n)]) + n + struct.pack("<H", d)
    out += struct.pack("<H", len(datas))
    for blob in datas:
        out += blob
    with open(OUT, "wb") as f:
        f.write(out)
    print(f"tzdata {tzdata.IANA_VERSION}: {len(zones)} zones, {len(datas)} distinct, {len(out)} bytes -> {OUT}",
          file=sys.stderr)


if __name__ == "__main__":
    main()
