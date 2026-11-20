"""Automated checks for tools/presets.py: run with `uv run python -I tools/test_presets.py`."""

import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

TOOL = Path(__file__).with_name("presets.py")
ID = "dev.foch.eu4-dynamic-trade-blackhole"

def run(*args: str, env_home: Path) -> subprocess.CompletedProcess:
    import os
    env = dict(os.environ)
    return subprocess.run(
        [sys.executable, "-I", str(TOOL), *args],
        capture_output=True, text=True, env=env, cwd=env_home,
        check=True,
    )

def main() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        home = Path(tmp)
        # presets.py prefers ./.foch/selections.toml when present, so the
        # test redirects it via the working directory instead of HOME.
        cfg = home / ".foch"
        cfg.mkdir(parents=True)
        (cfg / "selections.toml").write_text(
            'format = 1\n\n[playsets.alpha.plugins."other.plugin"]\nversion = "1.0"\nenabled = true\n',
            encoding="utf-8",
        )
        run("--playset", "alpha", "--preset", "standard", env_home=home)
        doc = tomllib.load(open(cfg / "selections.toml", "rb"))
        mine = doc["playsets"]["alpha"]["plugins"][ID]["config"]
        assert mine["mode"] == "policy" and mine["evidence_months"] == 6, mine
        other = doc["playsets"]["alpha"]["plugins"]["other.plugin"]
        assert other["enabled"] is True, "other plugin clobbered"
        # switch preset idempotently
        run("--playset", "alpha", "--preset", "observer", env_home=home)
        doc = tomllib.load(open(cfg / "selections.toml", "rb"))
        mine = doc["playsets"]["alpha"]["plugins"][ID]["config"]
        assert mine["mode"] == "observe", mine
        # unknown playset refused without --force
        bad = subprocess.run(
            [sys.executable, "-I", str(TOOL), "--playset", "ghost", "--preset", "standard"],
            capture_output=True, text=True, cwd=home,
        )
        assert bad.returncode != 0 and "--force" in bad.stderr, bad.stderr
        run("--playset", "new", "--preset", "sandbox", "--force", env_home=home)
        doc = tomllib.load(open(cfg / "selections.toml", "rb"))
        assert doc["playsets"]["new"]["plugins"][ID]["config"]["enabled"] is False
    print("test_presets: all checks passed")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
