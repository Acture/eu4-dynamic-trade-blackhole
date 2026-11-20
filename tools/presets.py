#!/usr/bin/env python3
"""Player-side playset presets for the trade-blackhole plugin.

Writes a playset's plugin selection (mode + policy knobs) into Foch's
selections.toml, so players never hand-edit config. Each preset is a
named balance of the three levers:

  observer   only watch: full monthly logs, the engine untouched
  gentle     slow convergence: a year of evidence, few flips, small fees
  standard   the tuned default the experiments validated
  assertive  fast convergence: six months of evidence, higher fees
  sandbox    everything off except the sandbox flag (safest first run)

Usage:
  uv run tools/presets.py --playset default --preset standard
  uv run tools/presets.py --playset mine --preset gentle --force

Idempotent: re-running replaces the playset's trade-blackhole config
section and leaves every other plugin's choice untouched.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

try:
	import tomllib
except ModuleNotFoundError:  # py<3.11
	sys.exit("presets.py needs Python 3.11+ (tomllib)")

ID = "dev.foch.eu4-dynamic-trade-blackhole"

PRESETS: dict[str, dict[str, object]] = {
	"observer": {
		"enabled": True,
		"mode": "observe",
		"observer_log": True,
	},
	"gentle": {
		"enabled": True,
		"mode": "policy",
		"observer_log": True,
		"sink_basis": "mean",
		"relay_ref": 3000,
		"flow_epsilon_pct": 5,
		"reopen_pct": 20,
		"evidence_months": 12,
		"max_flips_total": 8,
		"transit_fee": True,
		"transit_fee_pct": 10,
		"source_fee_pct": 10,
	},
	"standard": {
		"enabled": True,
		"mode": "policy",
		"observer_log": True,
		"sink_basis": "mean",
		"relay_ref": 3000,
		"flow_epsilon_pct": 5,
		"reopen_pct": 15,
		"evidence_months": 6,
		"max_flips_total": 16,
		"transit_fee": True,
		"transit_fee_pct": 20,
		"source_fee_pct": 20,
	},
	"assertive": {
		"enabled": True,
		"mode": "policy",
		"observer_log": False,
		"sink_basis": "mean",
		"relay_ref": 3000,
		"flow_epsilon_pct": 3,
		"reopen_pct": 12,
		"evidence_months": 4,
		"max_flips_total": 24,
		"transit_fee": True,
		"transit_fee_pct": 25,
		"source_fee_pct": 25,
	},
	"sandbox": {
		"enabled": False,
		"mode": "observe",
		"observer_log": True,
	},
}


def selections_path() -> Path:
	home = Path.home() / ".config" / "foch" / "plugins" / "selections.toml"
	if override := Path(".foch/selections.toml"):
		if override.is_file():
			return override
	return home


def format_selections(doc: dict) -> str:
	lines = [f"format = {doc.get('format', 1)}", ""]
	playsets = doc.get("playsets", {})
	for name in sorted(playsets):
		plugins = playsets[name].get("plugins", {})
		for pid in sorted(plugins):
			choice = plugins[pid]
			lines.append(f"[playsets.{name}.plugins.\"{pid}\"]")
			for key in ("version", "enabled"):
				if key in choice:
					value = choice[key]
					if isinstance(value, str):
						value = f'"{value}"'
					elif isinstance(value, bool):
						value = "true" if value else "false"
					lines.append(f"{key} = {value}")
			config = choice.get("config", {})
			if config:
				lines.append("")
				lines.append(f"[playsets.{name}.plugins.\"{pid}\".config]")
				for ck in sorted(config):
					value = config[ck]
					if isinstance(value, str):
						lines.append(f'{ck} = "{value}"')
					elif isinstance(value, bool):
						lines.append(f"{ck} = {'true' if value else 'false'}")
					else:
						lines.append(f"{ck} = {value}")
			lines.append("")
	return chr(10).join(lines)


def main() -> int:
	parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
	parser.add_argument("--playset", required=True, help="playset name to write")
	parser.add_argument("--preset", required=True, choices=sorted(PRESETS))
	parser.add_argument("--force", action="store_true",
		help="create the playset entry even if it does not exist yet")
	args = parser.parse_args()

	path = selections_path()
	doc = {"format": 1, "playsets": {}}
	if path.is_file():
		doc = tomllib.load(open(path, "rb"))

	playsets = doc.setdefault("playsets", {})
	entry = playsets.get(args.playset)
	if entry is None and not args.force:
		sys.exit(f"playset {args.playset!r} not found in {path}; pass --force to create it")
	if entry is None:
		entry = {"plugins": {}}
		playsets[args.playset] = entry
	entry.setdefault("plugins", {})
	choice = entry["plugins"].get(ID)
	if choice is None:
		# A fresh choice: the runner fills the real version at plan time;
		# the manifest default (0.0.1) keeps the file loadable meanwhile.
		choice = {"version": "0.0.1", "enabled": True, "config": {}}
		entry["plugins"][ID] = choice
	choice["config"] = dict(PRESETS[args.preset])

	text = format_selections(doc)
	path.parent.mkdir(parents=True, exist_ok=True)
	path.write_text(text, encoding="utf-8", newline="\n")
	print(f"[{args.playset}] {ID} -> preset {args.preset} ({len(PRESETS[args.preset])} keys)")
	print(f"wrote {path}")
	return 0


if __name__ == "__main__":
	raise SystemExit(main())
