"""Automated checks for tools/gen_members.py: mod override and custom nodes."""

import subprocess, sys, tempfile
from pathlib import Path

TOOL = Path(__file__).with_name("gen_members.py")

def write(root: Path, name: str, body: str) -> None:
    d = root / "common" / "tradenodes"
    d.mkdir(parents=True, exist_ok=True)
    (d / name).write_text(body, encoding="utf-8")

def main() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        game, mod, out = Path(tmp) / "game", Path(tmp) / "mod", Path(tmp) / "members.txt"
        write(game, "00_base.txt", """
base_node = { members = { 1 2 3 } }
shared = { members = { 10 11 } }
""")
        write(mod, "99_custom.txt", """
custom_node = { members = { 700 701 } }
shared = { members = { 20 } }
""")
        subprocess.run([sys.executable, "-I", str(TOOL), "--game", str(game), str(mod),
                        "--out", str(out)], check=True, capture_output=True)
        lines = dict(l.split(maxsplit=1) for l in out.read_text().splitlines())
        assert "base_node" in lines and "custom_node" in lines, lines
        assert lines["shared"].split() == ["20"], "mod must override vanilla wholesale"
        assert lines["base_node"].split() == ["1", "2", "3"]
        assert len(lines) == 3, "custom node count must not be assumed"
    print("test_gen_members: all checks passed")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
