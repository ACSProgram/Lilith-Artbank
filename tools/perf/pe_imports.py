"""List PE import and delay-import DLLs without external dependencies.

Usage:
    python tools/perf/pe_imports.py src-tauri/target/debug/lilith-artbank.exe
"""

from __future__ import annotations

import struct
import sys


def main(path: str) -> None:
    with open(path, "rb") as handle:
        data = handle.read()
    pe_offset = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe_offset : pe_offset + 4] != b"PE\0\0":
        raise SystemExit("not a PE file")

    coff = pe_offset + 4
    section_count = struct.unpack_from("<H", data, coff + 2)[0]
    optional_size = struct.unpack_from("<H", data, coff + 16)[0]
    optional = coff + 20
    magic = struct.unpack_from("<H", data, optional)[0]
    directories = optional + (112 if magic == 0x20B else 96)
    section_table = optional + optional_size

    ranges = []
    for index in range(section_count):
        offset = section_table + index * 40
        virtual_size, virtual_address, raw_size, raw_pointer = struct.unpack_from(
            "<IIII", data, offset + 8
        )
        ranges.append((virtual_address, max(virtual_size, raw_size), raw_pointer))

    def to_offset(rva: int) -> int:
        for virtual_address, size, raw_pointer in ranges:
            if virtual_address <= rva < virtual_address + size:
                return raw_pointer + rva - virtual_address
        raise ValueError(f"unmapped RVA {rva:#x}")

    def c_string(rva: int) -> str:
        offset = to_offset(rva)
        end = data.index(b"\0", offset)
        return data[offset:end].decode(errors="replace")

    for directory_index, label in ((1, "imports"), (13, "delay-imports")):
        rva, size = struct.unpack_from("<II", data, directories + directory_index * 8)
        if not rva or not size:
            continue
        print(f"== {label} ==")
        offset = to_offset(rva)
        index = 0
        while True:
            entry = struct.unpack_from("<IIIII", data, offset + index * 20)
            if all(value == 0 for value in entry):
                break
            if entry[3]:
                print(c_string(entry[3]))
            index += 1


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    main(sys.argv[1])
