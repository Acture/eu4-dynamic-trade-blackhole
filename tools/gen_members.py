#!/usr/bin/env python3
"""Build node_members.txt from any mod combination's tradenodes files.

Feeds the plugin's tax aggregation on modded trade maps (Anbennar,
MEIOU, ...) where the shipped vanilla table mismatches. Later entries
win (mod overrides vanilla), matching the game's own file-load order;
this mirrors replace_path for the common case of whole-directory
replacement.

Usage:
  uv run tools/gen_members.py --game <EU4 install> [mod dirs...] \
      --out tools/node_members.txt
"""

from __future__ import annotations

import argparse
import re
from pathlib import Path

MEMBER = re.compile(r"members\s*=\s*\{([^}]*)}", re.S)
HEADER = re.compile(r"^([A-Za-z0-9_]+)\s*=\s*\{", re.M)


def parse(text: str) -> dict[str, list[int]]:
	table: dict[str, list[int]] = {}
	for header in HEADER.finditer(text):
		key = header.group(1)
		if key in ("outgoing", "path", "control"):
			continue
		# The members block of THIS node: search forward from the header.
		block = MEMBER.search(text, header.end())
		if block is None:
			continue
		ids = [int(m) for m in re.findall(r"\d+", block.group(1))]
		if ids:
			table[key] = ids
	return table


def main() -> int:
	parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
	parser.add_argument("--game", required=True, type=Path, help="EU4 install root")
	parser.add_argument("mods", nargs="*", type=Path, help="mod roots in load order")
	parser.add_argument("--out", required=True, type=Path)
	args = parser.parse_args()

	# Load order: vanilla first, then each mod on top; later files with
	# the same node key replace earlier ones entirely.
	table: dict[str, list[int]] = {}
	for root in [args.game, *args.mods]:
		dir_ = root / "common" / "tradenodes"
		if not dir_.is_dir():
			continue
		for file in sorted(dir_.glob("*.txt")):
			table.update(parse(file.read_text(encoding="utf-8", errors="replace")))

	if not table:
		sys = __import__("sys")
		sys.exit("no tradenodes members found under the given roots")
	lines = [f"{key} {' '.join(str(i) for i in ids)}" for key, ids in sorted(table.items())]
	args.out.parent.mkdir(parents=True, exist_ok=True)
	args.out.write_text("\n".join(lines) + "\n", encoding="utf-8", newline="\n")
	print(f"{len(table)} nodes -> {args.out}")
	return 0


if __name__ == "__main__":
	raise SystemExit(main())
