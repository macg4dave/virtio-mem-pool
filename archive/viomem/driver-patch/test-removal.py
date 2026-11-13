#!/usr/bin/env python3
"""Compile the actual upstream removal function with a model memory manager."""

import argparse
from pathlib import Path
import subprocess
import tempfile


def function(source, signature):
    start = source.index(signature)
    opening = source.index("{", start)
    depth = 1
    end = opening + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("source", type=Path, help="upstream viomem/sys/viomem.c")
parser.add_argument("--cc", default="cc")
parser.add_argument("--sanitize", action="store_true", help="enable address/undefined sanitizers")
args = parser.parse_args()
source = args.source.read_text(encoding="utf-8-sig")
functions = []
if "ULONG RtlFindLongestRunSet(" in source:
    functions.append(function(source, "ULONG RtlFindLongestRunSet("))
    functions.append(function(source, "inline BOOLEAN FindAllocatedMemoryRangeInBitmap(").replace("inline ", ""))
functions.append(function(source, "BOOLEAN VirtioMemRemovePhysicalMemory("))
harness = Path(__file__).with_name("removal-harness.c").read_text()
with tempfile.TemporaryDirectory(prefix="viomem-removal-") as temp:
    translation_unit = Path(temp) / "removal.c"
    executable = Path(temp) / "removal-test"
    translation_unit.write_text(harness.replace("/* REMOVAL_FUNCTIONS */", "\n\n".join(functions)))
    sanitizers = ["-fsanitize=undefined,address"] if args.sanitize else []
    subprocess.run([args.cc, "-std=c11", "-Wall", "-Wextra", "-Werror", *sanitizers,
                    "-g", str(translation_unit),
                    "-o", str(executable)], check=True)
    subprocess.run([str(executable)], check=True)
