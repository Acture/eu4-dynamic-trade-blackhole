#!/usr/bin/env python3
"""Build node_members.txt the way EU4 loads common/tradenodes.

The game builds one virtual directory. Vanilla is the first layer. Each
later layer is a mod, in playset order (``dlc_load.json`` ``enabled_mods``;
declared dependencies do not reorder it). A ``replace_path`` whose
components are a prefix of ``common/tradenodes/<file>`` drops every earlier
copy of that file, including vanilla and an earlier mod that itself
replaced the directory. A later mod without ``replace_path`` still overlays.
The same relative filename replaces the whole file; a different filename is
kept. The surviving files are then read in case-sensitive filename order,
and a later file's node key replaces the earlier key entirely.

``replace_path`` is read from the ``.mod`` file the playset names, which is
the descriptor the game loads. A mod directory's own ``descriptor.mod`` is
used only when that directory is passed directly. An invalid ``replace_path``
(a drive or UNC prefix, ``..``, or an empty path) is an error: ignoring it
would keep files the game may not load.

Usage:
  uv run tools/gen_members.py --game <EU4 install> --playset <dlc_load.json> \
      --out tools/node_members.txt
  uv run tools/gen_members.py --game <EU4 install> mod.mod mod_dir \
      --out tools/node_members.txt
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import zipfile
from pathlib import Path

MEMBER = re.compile(r"members\s*=\s*\{([^}]*)}", re.S)
HEADER = re.compile(r"^([A-Za-z0-9_]+)\s*=\s*\{", re.M)
TRADE_DIR = ("common", "tradenodes")


class LoadError(Exception):
	"""The playset cannot be read as the game would load it."""


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


def decode_descriptor(data: bytes) -> str:
	payload = data[3:] if data.startswith(b"\xef\xbb\xbf") else data
	try:
		return payload.decode("utf-8")
	except UnicodeDecodeError:
		return payload.decode("cp1252")


def descriptor_assignments(text: str) -> list[tuple[str, str]]:
	"""Scalar assignments, with ``#`` comments outside quotes ignored.

	Quoted values keep backslashes. ``\\"`` is an escaped quote, matching
	the descriptor reader: a value cannot end in a backslash because that
	backslash escapes the closing quote.
	"""
	found: list[tuple[str, str]] = []
	index = 0
	length = len(text)
	while index < length:
		char = text[index]
		if char in " \t\r\n":
			index += 1
			continue
		if char == "#":
			while index < length and text[index] not in "\r\n":
				index += 1
			continue
		if not (char.isalpha() or char == "_"):
			while index < length and text[index] not in "\r\n":
				index += 1
			continue
		start = index
		index += 1
		while index < length and (text[index].isalnum() or text[index] == "_"):
			index += 1
		key = text[start:index]
		while index < length and text[index] in " \t":
			index += 1
		if index >= length or text[index] != "=":
			continue
		index += 1
		while index < length and text[index] in " \t":
			index += 1
		if index < length and text[index] == '"':
			index += 1
			chars: list[str] = []
			while index < length:
				if text[index] == "\\" and index + 1 < length and text[index + 1] == '"':
					chars.append('"')
					index += 2
					continue
				if text[index] == '"':
					index += 1
					break
				chars.append(text[index])
				index += 1
			found.append((key, "".join(chars)))
			continue
		start = index
		while index < length and text[index] not in " \t\r\n#":
			index += 1
		found.append((key, text[start:index]))
	return found


def parse_replace_path(value: str) -> tuple[str, ...]:
	"""The directory a ``replace_path`` names, in Windows path syntax.

	``/`` and ``\\`` both separate. A leading, trailing or repeated
	separator and ``.`` add nothing. Case and spaces stay as written.
	"""
	if value.startswith("\\\\") or value.startswith("//"):
		raise LoadError(
			f"replace_path {value!r} is not a directory under the game root: "
			"it has a drive, UNC or device prefix"
		)
	parts: list[str] = []
	for part in re.split(r"[/\\]", value):
		if part == "" or part == ".":
			continue
		if part == "..":
			raise LoadError(
				f"replace_path {value!r} is not a directory under the game root: "
				"it has a `..` component"
			)
		if len(part) >= 2 and part[1] == ":" and part[0].isalpha():
			raise LoadError(
				f"replace_path {value!r} is not a directory under the game root: "
				"it has a drive, UNC or device prefix"
			)
		if any(char in part for char in (":", "\0")):
			raise LoadError(
				f"replace_path {value!r} is not a directory under the game root: "
				f"component {part!r} contains a reserved character"
			)
		parts.append(part)
	if not parts:
		raise LoadError(
			f"replace_path {value!r} is not a directory under the game root: "
			"it names no directory"
		)
	return tuple(parts)


def replace_covers_file(prefix: tuple[str, ...], filename: str) -> bool:
	file_parts = (*TRADE_DIR, filename)
	return len(prefix) <= len(file_parts) and file_parts[: len(prefix)] == prefix


class Layer:
	def __init__(self, label: str, files: dict[str, str], replace_paths: list[tuple[str, ...]]):
		self.label = label
		self.files = files
		self.replace_paths = replace_paths

	def covers(self, filename: str) -> bool:
		return any(replace_covers_file(prefix, filename) for prefix in self.replace_paths)


def virtual_tradenode_files(layers: list[Layer]) -> list[tuple[str, str]]:
	"""Files the game loads, in the order it feeds them to the database."""
	files: dict[str, str] = {}
	for layer in layers:
		for name in [name for name in files if layer.covers(name)]:
			del files[name]
		files.update(layer.files)
	return [(name, files[name]) for name in sorted(files)]


def build_table(layers: list[Layer]) -> dict[str, list[int]]:
	table: dict[str, list[int]] = {}
	for _name, text in virtual_tradenode_files(layers):
		table.update(parse(text))
	return table


def tradenode_files_in_directory(root: Path) -> dict[str, str]:
	directory = root / "common" / "tradenodes"
	if not directory.is_dir():
		return {}
	files: dict[str, str] = {}
	for file in directory.glob("*.txt"):
		if file.parent != directory or not file.is_file():
			continue
		files[file.name] = file.read_text(encoding="utf-8", errors="replace")
	return files


def tradenode_files_in_zip(archive: Path) -> dict[str, str]:
	files: dict[str, str] = {}
	with zipfile.ZipFile(archive) as bundle:
		for info in bundle.infolist():
			if info.is_dir():
				continue
			parts = [part for part in info.filename.replace("\\", "/").split("/") if part]
			if (
				len(parts) == 3
				and parts[0] == "common"
				and parts[1] == "tradenodes"
				and parts[2].endswith(".txt")
			):
				files[parts[2]] = bundle.read(info).decode("utf-8", errors="replace")
	return files


def resolve_host_path(user_dir: Path, value: str) -> Path:
	"""A descriptor ``path`` or ``archive``.

	Absolute values are used as written. A relative value is tried under
	the Paradox user directory (the directory that holds ``dlc_load.json``)
	and then under its ``mod`` directory.
	"""
	candidate = Path(value)
	if candidate.is_absolute():
		return candidate
	primary = user_dir / candidate
	if primary.exists():
		return primary
	nested = user_dir / "mod" / candidate
	if nested.exists():
		return nested
	return primary


def layer_from_assignments(label: str, fields: list[tuple[str, str]], user_dir: Path) -> Layer:
	path: str | None = None
	archive: str | None = None
	replace_paths: list[tuple[str, ...]] = []
	for key, value in fields:
		if key == "replace_path":
			replace_paths.append(parse_replace_path(value))
		elif key == "path":
			if path is not None:
				raise LoadError(f"{label}: duplicate path")
			path = value
		elif key == "archive":
			if archive is not None:
				raise LoadError(f"{label}: duplicate archive")
			archive = value
	if path is not None and archive is not None:
		raise LoadError(f"{label}: both path and archive are set; refusing to guess which the game loads")
	if path is None and archive is None:
		raise LoadError(f"{label}: no path or archive")
	if path is not None:
		root = resolve_host_path(user_dir, path)
		if not root.is_dir():
			raise LoadError(f"{label}: path {path!r} is not a directory ({root})")
		files = tradenode_files_in_directory(root)
	else:
		assert archive is not None
		bundle = resolve_host_path(user_dir, archive)
		if not bundle.is_file():
			raise LoadError(f"{label}: archive {archive!r} is not a file ({bundle})")
		files = tradenode_files_in_zip(bundle)
	return Layer(label, files, replace_paths)


def layer_from_mod_file(mod_file: Path, user_dir: Path) -> Layer:
	if not mod_file.is_file():
		raise LoadError(f"mod descriptor does not exist: {mod_file}")
	fields = descriptor_assignments(decode_descriptor(mod_file.read_bytes()))
	return layer_from_assignments(str(mod_file), fields, user_dir)


def layer_from_directory(root: Path) -> Layer:
	"""A mod root. ``replace_path`` comes from its ``descriptor.mod``."""
	if not root.is_dir():
		raise LoadError(f"mod root is not a directory: {root}")
	descriptor = root / "descriptor.mod"
	replace_paths: list[tuple[str, ...]] = []
	if descriptor.is_file():
		for key, value in descriptor_assignments(decode_descriptor(descriptor.read_bytes())):
			if key == "replace_path":
				replace_paths.append(parse_replace_path(value))
	return Layer(str(root), tradenode_files_in_directory(root), replace_paths)


def parse_enabled_mod_entry(entry: str) -> tuple[str, ...]:
	if "\\" in entry or entry.startswith("/") or not entry:
		raise LoadError(
			f"enabled mod descriptor path {entry!r} must be normalized and relative"
		)
	parts = tuple(entry.split("/"))
	if any(part in ("", ".", "..") for part in parts):
		raise LoadError(
			f"enabled mod descriptor path {entry!r} must be normalized and relative"
		)
	return parts


def layers_from_playset(playset: Path) -> list[Layer]:
	"""``dlc_load.json``: the ordered list the game actually loads."""
	try:
		payload = json.loads(playset.read_text(encoding="utf-8-sig"))
	except (OSError, json.JSONDecodeError) as exc:
		raise LoadError(f"{playset}: {exc}") from exc
	if not isinstance(payload, dict) or "enabled_mods" not in payload:
		raise LoadError(
			f"{playset}: expected the launcher dlc_load.json (enabled_mods), "
			"which is the playset the game loads"
		)
	enabled = payload["enabled_mods"]
	if not isinstance(enabled, list):
		raise LoadError(f"{playset}: enabled_mods must be a list")
	user_dir = playset.parent
	layers: list[Layer] = []
	for entry in enabled:
		if not isinstance(entry, str):
			raise LoadError(f"{playset}: enabled_mods entries must be strings")
		mod_file = user_dir.joinpath(*parse_enabled_mod_entry(entry))
		layers.append(layer_from_mod_file(mod_file, user_dir))
	return layers


def user_dir_for_mod_file(mod_file: Path) -> Path:
	parent = mod_file.parent
	if parent.name.lower() == "mod":
		return parent.parent
	return parent


def render(table: dict[str, list[int]]) -> str:
	lines = [f"{key} {' '.join(str(i) for i in ids)}" for key, ids in sorted(table.items())]
	return "\n".join(lines) + "\n"


def main() -> int:
	parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
	parser.add_argument("--game", required=True, type=Path, help="EU4 install root")
	parser.add_argument(
		"--playset",
		action="append",
		default=[],
		type=Path,
		help="dlc_load.json; enabled_mods load in order, before positional mods",
	)
	parser.add_argument(
		"mods",
		nargs="*",
		type=Path,
		help="mod directories or .mod files in load order, after --playset",
	)
	parser.add_argument("--out", required=True, type=Path)
	args = parser.parse_args()

	try:
		layers = [Layer("vanilla", tradenode_files_in_directory(args.game), [])]
		for playset in args.playset:
			layers.extend(layers_from_playset(playset))
		for mod in args.mods:
			if mod.is_file() and mod.suffix.lower() == ".mod":
				layers.append(layer_from_mod_file(mod, user_dir_for_mod_file(mod)))
			elif mod.is_dir():
				layers.append(layer_from_directory(mod))
			else:
				raise LoadError(f"not a mod directory or .mod file: {mod}")
		table = build_table(layers)
	except LoadError as exc:
		sys.exit(str(exc))

	if not table:
		sys.exit("no tradenodes members found under the given roots")
	args.out.parent.mkdir(parents=True, exist_ok=True)
	args.out.write_text(render(table), encoding="utf-8", newline="\n")
	print(f"{len(table)} nodes -> {args.out}")
	return 0


if __name__ == "__main__":
	raise SystemExit(main())
