"""Automated checks for tools/gen_members.py.

The load matches EU4: playset order, replace_path from the .mod file the
playset names, whole-file replacement, and filename order.
"""

import json
import subprocess
import sys
import textwrap
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gen_members

TOOL = Path(__file__).with_name("gen_members.py")


def write_nodes(root: Path, name: str, body: str) -> None:
    directory = root / "common" / "tradenodes"
    directory.mkdir(parents=True, exist_ok=True)
    (directory / name).write_text(textwrap.dedent(body).lstrip("\n"), encoding="utf-8")


def run(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-I", str(TOOL), *args],
        check=False,
        capture_output=True,
        text=True,
    )


def read_table(path: Path) -> dict[str, list[str]]:
    return {
        line.split(maxsplit=1)[0]: line.split(maxsplit=1)[1].split()
        for line in path.read_text(encoding="utf-8").splitlines()
    }


def ids(table: dict[str, list[str]], key: str) -> list[str]:
    return table[key]


def test_mod_override_and_custom_nodes() -> None:
    with tempfile_dir() as tmp:
        game, mod, out = tmp / "game", tmp / "mod", tmp / "members.txt"
        write_nodes(
            game,
            "00_base.txt",
            """
            base_node = { members = { 1 2 3 } }
            shared = { members = { 10 11 } }
            """,
        )
        write_nodes(
            mod,
            "99_custom.txt",
            """
            custom_node = { members = { 700 701 } }
            shared = { members = { 20 } }
            """,
        )
        result = run("--game", str(game), str(mod), "--out", str(out))
        assert result.returncode == 0, result.stderr
        lines = read_table(out)
        assert "base_node" in lines and "custom_node" in lines, lines
        assert ids(lines, "shared") == ["20"], "mod must override vanilla wholesale"
        assert ids(lines, "base_node") == ["1", "2", "3"]
        assert len(lines) == 3, "custom node count must not be assumed"


def test_same_filename_replaces_the_whole_file() -> None:
    with tempfile_dir() as tmp:
        game, mod = tmp / "game", tmp / "mod"
        write_nodes(
            game,
            "00_tradenodes.txt",
            """
            kept = { members = { 1 } }
            dropped = { members = { 2 } }
            """,
        )
        write_nodes(
            mod,
            "00_tradenodes.txt",
            """
            kept = { members = { 9 } }
            """,
        )
        table = gen_members.build_table(
            [
                gen_members.layer_from_directory(game),
                gen_members.layer_from_directory(mod),
            ]
        )
        assert table == {"kept": [9]}


def test_later_filename_wins_across_layers() -> None:
    with tempfile_dir() as tmp:
        game, mod = tmp / "game", tmp / "mod"
        write_nodes(game, "zz_nodes.txt", "shared = { members = { 1 } }\n")
        write_nodes(mod, "aa_nodes.txt", "shared = { members = { 2 } }\n")
        table = gen_members.build_table(
            [
                gen_members.layer_from_directory(game),
                gen_members.layer_from_directory(mod),
            ]
        )
        assert table["shared"] == [1]


def test_directory_replace_path_drops_vanilla_and_later_mod_overlays() -> None:
    with tempfile_dir() as tmp:
        game, replacer, later = tmp / "game", tmp / "replacer", tmp / "later"
        write_nodes(
            game,
            "00_base.txt",
            """
            vanilla = { members = { 1 } }
            shared = { members = { 2 } }
            """,
        )
        (replacer / "descriptor.mod").parent.mkdir(parents=True)
        (replacer / "descriptor.mod").write_text(
            'name="Replacer"\nreplace_path="common/tradenodes"\n',
            encoding="utf-8",
        )
        write_nodes(replacer, "01_new.txt", "custom = { members = { 7 } }\nshared = { members = { 3 } }\n")
        write_nodes(later, "02_more.txt", "extra = { members = { 8 } }\nshared = { members = { 4 } }\n")
        table = gen_members.build_table(
            [
                gen_members.layer_from_directory(game),
                gen_members.layer_from_directory(replacer),
                gen_members.layer_from_directory(later),
            ]
        )
        assert "vanilla" not in table
        assert table["custom"] == [7]
        assert table["shared"] == [4]
        assert table["extra"] == [8]


def test_later_replace_path_drops_the_earlier_replacement() -> None:
    with tempfile_dir() as tmp:
        game, first, second = tmp / "game", tmp / "first", tmp / "second"
        write_nodes(game, "00.txt", "vanilla = { members = { 1 } }\n")
        for root, node in ((first, "alpha"), (second, "beta")):
            (root / "descriptor.mod").parent.mkdir(parents=True, exist_ok=True)
            (root / "descriptor.mod").write_text(
                'replace_path="common\\\\tradenodes"\n',
                encoding="utf-8",
            )
            write_nodes(root, "00.txt", f"{node} = {{ members = {{ 5 }} }}\n")
        table = gen_members.build_table(
            [
                gen_members.layer_from_directory(game),
                gen_members.layer_from_directory(first),
                gen_members.layer_from_directory(second),
            ]
        )
        assert table == {"beta": [5]}


def test_replace_path_prefix_is_whole_components_and_case_sensitive() -> None:
    with tempfile_dir() as tmp:
        game = tmp / "game"
        write_nodes(game, "00.txt", "vanilla = { members = { 1 } }\n")
        cases = {
            "common": False,
            "common/tradenodes": False,
            "common/tradenodes/00.txt": False,
            "common/tradenodes_extra": True,
            "Common/tradenodes": True,
            "events": True,
        }
        for replace_path, vanilla_survives in cases.items():
            mod = tmp / "mod"
            if mod.exists():
                for child in mod.rglob("*"):
                    if child.is_file():
                        child.unlink()
            (mod / "descriptor.mod").parent.mkdir(parents=True, exist_ok=True)
            (mod / "descriptor.mod").write_text(
                f'replace_path="{replace_path}"\n',
                encoding="utf-8",
            )
            write_nodes(mod, "01.txt", "custom = { members = { 9 } }\n")
            table = gen_members.build_table(
                [
                    gen_members.layer_from_directory(game),
                    gen_members.layer_from_directory(mod),
                ]
            )
            assert ("vanilla" in table) is vanilla_survives, replace_path
            assert table["custom"] == [9]


def test_file_specific_replace_path_drops_only_that_file() -> None:
    with tempfile_dir() as tmp:
        game, mod = tmp / "game", tmp / "mod"
        write_nodes(game, "00.txt", "alpha = { members = { 1 } }\n")
        write_nodes(game, "01.txt", "beta = { members = { 2 } }\n")
        (mod / "descriptor.mod").parent.mkdir(parents=True, exist_ok=True)
        (mod / "descriptor.mod").write_text(
            'replace_path="common/tradenodes/00.txt"\n',
            encoding="utf-8",
        )
        table = gen_members.build_table(
            [
                gen_members.layer_from_directory(game),
                gen_members.layer_from_directory(mod),
            ]
        )
        assert "alpha" not in table
        assert table == {"beta": [2]}


def test_playset_reads_replace_path_from_the_mod_file_not_descriptor() -> None:
    with tempfile_dir() as tmp:
        game = tmp / "game"
        write_nodes(game, "00.txt", "vanilla = { members = { 1 } }\n")
        content = tmp / "mods" / "overhaul"
        (content / "descriptor.mod").parent.mkdir(parents=True)
        # The mod's own descriptor does not replace the trade map. The
        # launcher .mod the game loads does.
        (content / "descriptor.mod").write_text('name="Overhaul"\n', encoding="utf-8")
        write_nodes(content, "01.txt", "custom = { members = { 4 } }\n")
        mod_dir = tmp / "mod"
        mod_dir.mkdir()
        (mod_dir / "ugc_1.mod").write_text(
            '\n'.join(
                [
                    'name="Overhaul"',
                    'path="mods/overhaul"',
                    'replace_path="common/tradenodes"',
                    '# replace_path="events"',
                    'replace_path="events"',
                    "",
                ]
            ),
            encoding="utf-8",
        )
        (tmp / "dlc_load.json").write_text(
            json.dumps({"enabled_mods": ["mod/ugc_1.mod"], "disabled_dlcs": []}),
            encoding="utf-8",
        )
        out = tmp / "members.txt"
        result = run("--game", str(game), "--playset", str(tmp / "dlc_load.json"), "--out", str(out))
        assert result.returncode == 0, result.stderr
        assert read_table(out) == {"custom": ["4"]}

        # The same content directory, passed directly, follows descriptor.mod
        # and therefore keeps vanilla.
        direct = tmp / "direct.txt"
        result = run("--game", str(game), str(content), "--out", str(direct))
        assert result.returncode == 0, result.stderr
        table = read_table(direct)
        assert table["vanilla"] == ["1"]
        assert table["custom"] == ["4"]


def test_descriptor_replace_path_is_not_read_when_the_mod_file_omits_it() -> None:
    with tempfile_dir() as tmp:
        game = tmp / "game"
        write_nodes(game, "00.txt", "vanilla = { members = { 1 } }\n")
        content = tmp / "mods" / "local"
        (content / "descriptor.mod").parent.mkdir(parents=True)
        (content / "descriptor.mod").write_text(
            'name="Local"\nreplace_path="common/tradenodes"\n',
            encoding="utf-8",
        )
        write_nodes(content, "01.txt", "custom = { members = { 4 } }\n")
        (tmp / "mod").mkdir()
        (tmp / "mod" / "local.mod").write_text(
            'name="Local"\npath="mods/local"\n',
            encoding="utf-8",
        )
        (tmp / "dlc_load.json").write_text(
            json.dumps({"enabled_mods": ["mod/local.mod"]}),
            encoding="utf-8",
        )
        out = tmp / "members.txt"
        result = run("--game", str(game), "--playset", str(tmp / "dlc_load.json"), "--out", str(out))
        assert result.returncode == 0, result.stderr
        table = read_table(out)
        assert table["vanilla"] == ["1"]
        assert table["custom"] == ["4"]


def test_archive_replace_path_and_invalid_values() -> None:
    with tempfile_dir() as tmp:
        game = tmp / "game"
        write_nodes(game, "00.txt", "vanilla = { members = { 1 } }\n")
        archive = tmp / "pack.zip"
        with zipfile.ZipFile(archive, "w") as bundle:
            bundle.writestr("common/tradenodes/01.txt", "zipped = { members = { 6 } }\n")
        (tmp / "mod").mkdir()
        (tmp / "mod" / "zip.mod").write_text(
            f'name="Zip"\narchive="{archive.as_posix()}"\nreplace_path="common/tradenodes"\n',
            encoding="utf-8",
        )
        (tmp / "dlc_load.json").write_text(
            json.dumps({"enabled_mods": ["mod/zip.mod"]}),
            encoding="utf-8",
        )
        out = tmp / "members.txt"
        result = run("--game", str(game), "--playset", str(tmp / "dlc_load.json"), "--out", str(out))
        assert result.returncode == 0, result.stderr
        assert read_table(out) == {"zipped": ["6"]}

        (tmp / "mod" / "bad.mod").write_text(
            'name="Bad"\npath="mods/missing"\nreplace_path="common/../events"\n',
            encoding="utf-8",
        )
        (tmp / "bad_load.json").write_text(
            json.dumps({"enabled_mods": ["mod/bad.mod"]}),
            encoding="utf-8",
        )
        refused = run("--game", str(game), "--playset", str(tmp / "bad_load.json"), "--out", str(tmp / "no.txt"))
        assert refused.returncode != 0
        assert ".." in refused.stderr

        (tmp / "escape.json").write_text(
            json.dumps({"enabled_mods": ["../outside.mod"]}),
            encoding="utf-8",
        )
        escaped = run("--game", str(game), "--playset", str(tmp / "escape.json"), "--out", str(tmp / "no.txt"))
        assert escaped.returncode != 0
        assert "normalized" in escaped.stderr


def tempfile_dir():
    import tempfile

    class _Dir:
        def __enter__(self):
            self._raw = tempfile.TemporaryDirectory()
            return Path(self._raw.__enter__())

        def __exit__(self, exc_type, exc, tb):
            return self._raw.__exit__(exc_type, exc, tb)

    return _Dir()


def main() -> int:
    test_mod_override_and_custom_nodes()
    test_same_filename_replaces_the_whole_file()
    test_later_filename_wins_across_layers()
    test_directory_replace_path_drops_vanilla_and_later_mod_overlays()
    test_later_replace_path_drops_the_earlier_replacement()
    test_replace_path_prefix_is_whole_components_and_case_sensitive()
    test_file_specific_replace_path_drops_only_that_file()
    test_playset_reads_replace_path_from_the_mod_file_not_descriptor()
    test_descriptor_replace_path_is_not_read_when_the_mod_file_omits_it()
    test_archive_replace_path_and_invalid_values()
    print("test_gen_members: all checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
