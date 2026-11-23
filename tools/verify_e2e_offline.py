#!/usr/bin/env python3
"""Offline player chain for the Dynamic Trade Blackhole companion.

Checks the Steam BBCode and workshop companion, assembles a package under
``target/e2e-pkg/trade-blackhole``, then runs ``foch check``, ``plugin
import``, ``enable``, ``plan``, and ``launch --prepare-only``. Never starts
``eu4.exe``. Store, selections, the runtime layer, and the user directory
stay in a temporary directory (``FOCH_DATA_DIR`` / ``FOCH_CONFIG_DIR``).

Build first, from the Foch workspace, with ``CARGO_BUILD_JOBS=2``:

    cargo build -p foch-cli -p foch-runtime-host -p foch-plugin-trade-blackhole --release --locked
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

PACKAGE = Path(__file__).resolve().parent.parent
REPO = next(
	(
		candidate
		for candidate in (PACKAGE, PACKAGE.parent, PACKAGE.parent.parent)
		if (candidate / "target").is_dir()
	),
	PACKAGE.parent.parent,
)
WORKSHOP = PACKAGE / "workshop"
PLUGIN_ID = "dev.foch.eu4-dynamic-trade-blackhole"
FOCH = REPO / "target" / "release" / "foch.exe"
HOST = REPO / "target" / "release" / "foch_runtime_host.dll"
PKG = REPO / "target" / "e2e-pkg" / "trade-blackhole"


def fail(message: str) -> None:
	raise SystemExit(message)


def require(condition: bool, message: str) -> None:
	if not condition:
		fail(message)


def run(args: list[str], env: dict[str, str], cwd: Path) -> subprocess.CompletedProcess[str]:
	completed = subprocess.run(
		args,
		cwd=cwd,
		env=env,
		capture_output=True,
		text=True,
		encoding="utf-8",
		errors="replace",
	)
	if completed.returncode != 0:
		fail(
			f"command failed ({completed.returncode}): {' '.join(args)}\n"
			f"--- stdout ---\n{completed.stdout}\n--- stderr ---\n{completed.stderr}"
		)
	return completed


def check_bbcode() -> None:
	for name in ("STEAM_WORKSHOP_EN.bbcode", "STEAM_WORKSHOP_ZH.bbcode"):
		text = (WORKSHOP / name).read_text(encoding="utf-8")
		require("`" not in text, f"{name} still contains a Markdown backtick")
		require(
			"foch plugin install Acture/eu4-dynamic-trade-blackhole --mirror auto" in text,
			f"{name} is missing foch plugin install --mirror auto",
		)
		require(
			"foch plugin install Acture/eu4-dynamic-trade-blackhole --mirror cn" in text,
			f"{name} is missing foch plugin install --mirror cn",
		)
		require("gh release" not in text, f"{name} still tells players to use gh release")
		require(
			f"foch plugin enable {PLUGIN_ID}" in text,
			f"{name} enable command does not use {PLUGIN_ID}",
		)


def check_companion_files() -> None:
	descriptor = (WORKSHOP / "descriptor.mod").read_text(encoding="utf-8")
	for needle in (
		'name="Dynamic Trade Blackhole (Foch Companion)"',
		'supported_version="1.37.*"',
		'picture="thumbnail.png"',
		'"Balance"',
		'"Gameplay"',
		'"Trade"',
		'"Utilities"',
	):
		require(needle in descriptor, f"descriptor.mod is missing {needle}")
	require((WORKSHOP / "thumbnail.png").is_file(), "thumbnail.png is missing")
	require(not (WORKSHOP / "foch-companion.toml").exists(), "foch-companion.toml must be removed")
	manifest = tomllib.loads((WORKSHOP / "foch.toml").read_text(encoding="utf-8"))
	require("project" not in manifest, "workshop/foch.toml must not carry a [project] table")
	plugins = manifest.get("plugins")
	require(
		isinstance(plugins, list) and len(plugins) == 1,
		"workshop/foch.toml must contain one [[plugins]] entry",
	)
	plugin = plugins[0]
	require(isinstance(plugin, dict), "workshop/foch.toml [[plugins]] entry")
	require(plugin.get("id") == PLUGIN_ID, "foch.toml plugin id")
	require(plugin.get("repo") == "Acture/eu4-dynamic-trade-blackhole", "foch.toml repo")
	require(plugin.get("game") == "eu4", "foch.toml game")
	require(plugin.get("game_versions") == "=1.37.5", "foch.toml game_versions")
	require(plugin.get("required") is True, "foch.toml required")
	decision = (WORKSHOP / "decisions" / "foch_dtb_companion.txt").read_text(encoding="utf-8")
	require("foch_dtb_status" in decision and "foch_dtb_info.1" in decision, "decision is incomplete")
	event = (WORKSHOP / "events" / "foch_dtb_companion.txt").read_text(encoding="utf-8")
	require("namespace = foch_dtb_info" in event, "event namespace")
	require("is_triggered_only = yes" in event, "event must be triggered only")
	headers = {
		"foch_dtb_companion_l_english.yml": "l_english:",
		"foch_dtb_companion_l_simp_chinese.yml": "l_simp_chinese:",
	}
	for name, header in headers.items():
		raw = (WORKSHOP / "localisation" / name).read_bytes()
		require(raw.startswith(b"\xef\xbb\xbf"), f"{name} is missing the UTF-8 BOM")
		text = raw.decode("utf-8-sig")
		require(text.startswith(header), f"{name} header is not {header}")
		for key in (
			"foch_dtb_status_title",
			"foch_dtb_status_desc",
			"foch_dtb_info.1.t",
			"foch_dtb_info.1.d",
			"foch_dtb_info.1.a",
		):
			require(key in text, f"{name} is missing {key}")


def isolated_env(root: Path) -> dict[str, str]:
	env = os.environ.copy()
	env["FOCH_DATA_DIR"] = str(root / "data")
	env["FOCH_CONFIG_DIR"] = str(root / "config")
	env["CARGO_BUILD_JOBS"] = env.get("CARGO_BUILD_JOBS", "2")
	# Keep Steam discovery from writing the player's real library into this config.
	(root / "config").mkdir(parents=True, exist_ok=True)
	(root / "config" / "config.toml").write_text(
		f'steam_root_path = "{(root / "missing-steam").as_posix()}"\n',
		encoding="utf-8",
		newline="\n",
	)
	return env


def parse_json_stdout(completed: subprocess.CompletedProcess[str]) -> dict:
	text = completed.stdout.strip()
	require(text.startswith("{") or text.startswith("["), f"expected JSON, got:\n{text[:500]}")
	return json.loads(text)


def main() -> int:
	require(FOCH.is_file(), f"missing {FOCH}; build foch-cli --release first")
	require(HOST.is_file(), f"missing {HOST}; build foch-runtime-host --release first")
	check_bbcode()
	check_companion_files()
	with tempfile.TemporaryDirectory(prefix="foch-dtb-e2e-") as tmp:
		root = Path(tmp)
		env = isolated_env(root)
		check = run(
			[
				str(FOCH),
				"check",
				str(WORKSHOP),
				"--no-game-base",
				"--strict",
				"--no-color",
				"--format",
				"json",
			],
			env,
			REPO,
		)
		report = parse_json_stdout(check)
		require(report.get("fatal_errors") == [], f"foch check fatal errors: {report.get('fatal_errors')}")
		require(report.get("findings") == [], f"foch check findings: {report.get('findings')}")
		require(
			report.get("strict_findings") == [],
			f"foch check strict findings: {report.get('strict_findings')}",
		)
		require(
			report.get("advisory_findings") == [],
			f"foch check advisory findings: {report.get('advisory_findings')}",
		)
		meta = report.get("analysis_meta") or {}
		require(meta.get("parse_errors") == 0, f"foch check parse_errors: {meta.get('parse_errors')}")
		stats = (meta.get("parse_stats") or {}).get("clausewitz_mainline") or {}
		loc = (meta.get("parse_stats") or {}).get("localisation") or {}
		require(stats.get("parse_issue_count") == 0, f"script parse issues: {stats}")
		require(loc.get("parse_issue_count") == 0, f"localisation parse issues: {loc}")
		print(
			"foch check: 0 fatal, 0 findings, "
			f"documents={meta.get('text_documents')} parsed_files={meta.get('parsed_files')}"
		)

		if PKG.exists():
			shutil.rmtree(PKG)
		assemble = run(
			[sys.executable, "-I", str(PACKAGE / "tools" / "assemble_pkg.py"), "--out", str(PKG)],
			env,
			REPO,
		)
		print(assemble.stdout.strip())
		manifest = (PKG / "foch-plugin.toml").read_text(encoding="utf-8")
		require(f'id = "{PLUGIN_ID}"' in manifest, "assembled manifest is missing the plugin id")
		require((PKG / "trade_blackhole.dll").is_file(), "assembled package is missing the DLL")

		imported = run([str(FOCH), "plugin", "import", str(PKG)], env, REPO)
		require(PLUGIN_ID in imported.stdout, imported.stdout)
		print(imported.stdout.strip())
		enabled = run(
			[str(FOCH), "plugin", "enable", PLUGIN_ID, "--playset", "e2e-test"],
			env,
			REPO,
		)
		print(enabled.stdout.strip())

		game = root / "game"
		game.mkdir()
		(game / "launcher-settings.json").write_text(
			'{ "rawVersion": "1.37.5" }\n',
			encoding="utf-8",
			newline="\n",
		)
		# A stand-in executable. --prepare-only copies and hashes it; it is never spawned.
		(game / "eu4.exe").write_bytes(b"MZ-not-launched\n")
		plan = run(
			[
				str(FOCH),
				"plugin",
				"plan",
				"--playset",
				"e2e-test",
				"--game-path",
				str(game),
				"--format",
				"json",
			],
			env,
			REPO,
		)
		planned = parse_json_stdout(plan)
		require(planned.get("launchable") is True, f"plan is not launchable: {planned}")
		require(planned.get("game_version") == "1.37.5", f"unexpected game version: {planned}")
		require(
			planned.get("order") and planned["order"][0].get("id") == PLUGIN_ID,
			f"unexpected load order: {planned.get('order')}",
		)
		print(f"foch plugin plan: launchable on {planned.get('game_version')}")

		runtime_base = root / "rt"
		user_dir = root / "user"
		launched = run(
			[
				str(FOCH),
				"plugin",
				"launch",
				"--playset",
				"e2e-test",
				"--prepare-only",
				"--runtime-base",
				str(runtime_base),
				"--user-dir",
				str(user_dir),
				"--game-path",
				str(game),
				"--format",
				"json",
			],
			env,
			REPO,
		)
		record = parse_json_stdout(launched)
		require(record.get("prepared_only") is True, f"prepared_only: {record.get('prepared_only')}")
		require(record.get("pid") is None, f"pid: {record.get('pid')}")
		runtime = Path(record["runtime"])
		require((runtime / "VERSION.dll").is_file(), "VERSION.dll was not staged")
		plugin_dlls = list(runtime.joinpath("plugins").rglob("trade_blackhole.dll"))
		require(len(plugin_dlls) == 1, f"staged plugin DLL: {plugin_dlls}")
		run_json = json.loads((runtime / "foch-host" / "run.json").read_text(encoding="utf-8"))
		plan_json = json.loads((runtime / "foch-host" / "plan.json").read_text(encoding="utf-8"))
		require(run_json.get("prepared_only") is True, "run.json prepared_only")
		require(run_json.get("pid") is None, "run.json pid")
		require(
			any(plugin.get("id") == PLUGIN_ID for plugin in plan_json.get("plugins", [])),
			"plan.json is missing the plugin",
		)
		print(f"foch plugin launch --prepare-only: {runtime}")
		print("prepared_only=true pid=null VERSION.dll staged plugin DLL staged")
	print("verify_e2e_offline: all checks passed")
	return 0


if __name__ == "__main__":
	raise SystemExit(main())
