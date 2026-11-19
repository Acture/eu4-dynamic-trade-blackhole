# EU4 Dynamic Trade Blackhole

A DLL plugin (built on the [Foch](https://github.com/Acture/foch) native
host) that makes EU4's trade topology **dynamic**: instead of value
flowing forever toward the three hardcoded end nodes, routes close and
reopen during play according to where wealth actually is — so black
holes emerge where purchasing power lives, and players can create them
by developing the right provinces.

It never touches `common/tradenodes`, so it does not conflict with any
trade mod: the engine only ever sees its own vanilla routes, pruned.

## What changes in gameplay

**Trade converges on wealth, not on Western Europe.** Every month the
plugin runs a propagation model over the full vanilla trade graph:
producers inject their local trade value, hubs relay it, and rich nodes
absorb it. Edges that consistently carry nothing close; the wealth
gradient (value should not flow from a richer node to a poorer one)
gates every closure. Nodes whose whole outflow dies become emergent
black holes — at the 1444 start, Malacca, Coromandel and Gujarat earn
that on their own.

**Farming creates black holes.** Development moves the levers:

- Develop **tax**, and a node outbids its downstream: Japan's trade
  stays in Japan.
- Develop **production**, and a node's injection grows until its
  outflow dies at the valve.
- Let a closed route's flow recover (develop its source), and the route
  **reopens** — closures are decisions, not deletions.

**Every trade role earns an income.** Vanilla only pays collectors;
this plugin adds two channels so a middleman monopoly profits without
owning either end:

| Role | Income |
| --- | --- |
| tax — sink | vanilla collection, amplified: a black hole keeps everything that arrives |
| trade power — transit | a **transit fee**: a cut of the volume the node forwards for others |
| production — source | a **source bounty**: a cut of the local production that leaves the node |

The two fees split a node's outflow by origin, so no ducat is paid
twice, and both are distributed among the countries holding trade power
in the node — by share. Pure relays earn the transit fee in full;
producers earn the bounty even though their value flows away; sinks
earn neither (their channel is the amplified collection).

Verified end to end on the real game (800-day sessions): a tax-farmed
Nippon closes its only edge at month 8; a scheduled late farm flips a
closed edge's net flow from −1.9% to +24% and reopens it at month 22;
the same farm run with and without the plugin leaves the farming nation
a full treasury bracket richer, with the gap attributed to the black
hole holding 48.5 vs 34.9 trade value in its node.

## Requirements

- EU4 **1.37.5, Steam, Windows x64** — the only pinned build; anything
  else is refused at load.
- The [Foch](https://github.com/Acture/foch) CLI and its native plugin
  host. This repository is a submodule of Foch
  (`plugins/eu4-dynamic-trade-blackhole`); build from the Foch root.

## Build and install

From the Foch checkout root:

```powershell
git submodule update --init
cargo build -p foch-plugin-trade-blackhole -p foch-eu4-host -p foch-cli --release --locked
uv run python -I plugins/eu4-dynamic-trade-blackhole/tools/assemble_pkg.py
foch plugin import ./target/plugin-pkg
foch plugin enable Acture/eu4-dynamic-trade-blackhole --playset default
foch plugin launch --playset default
```

`tools/assemble_pkg.py` packages the release DLL with the offline
node-membership table and the `foch-plugin.toml` manifest (digests
filled in). After importing a rebuilt package, remove the previous
`0.1.0+<hash>` store directory so `plugin enable` stays unambiguous.

## Configuration

Five presets cover the common play styles — no hand-editing of
`selections.toml`:

```powershell
uv run plugins/eu4-dynamic-trade-blackhole/tools/presets.py --playset default --preset standard
```

| Preset | Feel |
| --- | --- |
| `observer` | watch only: full monthly logs, the engine untouched |
| `gentle` | slow convergence: a year of evidence, few flips, small fees |
| `standard` | the tuned default the experiments validated |
| `assertive` | fast convergence: four months of evidence, higher fees |
| `sandbox` | everything off — the safest first run |

The tool is idempotent (re-running replaces only this plugin's config
section), preserves every other plugin's choices, refuses unknown
playsets unless `--force`, and the result loads through
`foch plugin plan` unchanged. Individual keys stay overridable by hand
afterwards; the [reference table](#configuration) below documents them.


Per playset, in `~/.config/foch/plugins/selections.toml`:

| Key | Default | Meaning |
| --- | --- | --- |
| `mode` | `observe` | `observe` logs without mutating; `force_toggle` proves the mutation primitive on one node; `policy` runs the real planner |
| `transit_fee` | `true` | pay the two role-income fees |
| `transit_fee_pct` | `20` | the transit fee pool, as a percent of forwarded volume |
| `source_fee_pct` | `20` | the source bounty pool, as a percent of exported local production |
| `evidence_months` | `6` | months of sustained flow evidence before any edge flips |
| `flow_epsilon_pct` / `reopen_pct` | `5` / `15` | the close/reopen hysteresis bands, as a percent of the node's pool |
| `max_flips_total` | `16` | edge flips the whole graph may apply per month |

The plugin logs every decision (flips, fees, per-node monthly series)
to the host event stream; `foch plugin status --run-dir <dir>` reads it.

## How it works

Monthly, on the game thread, inside the economy-update hook:

1. **Inject** — each node's local trade value (goods × price, read from
   the engine) enters its pool.
2. **Propagate** — pools relay across the *full bidirectional vanilla
   graph* (closed edges included, so reopens stay possible and the model
   never reacts to its own closures). Relay weight: steering effort plus
   the partner's trade power. Retention is the engine's own rate — no
   invented parameters.
3. **Judge** — every vanilla edge is judged by its tracked net flow
   against the hysteresis bands, gated by the per-edge wealth gradient;
   six months of sustained evidence and a monthly flip budget prevent
   flapping.
4. **Mutate** — closures and reopens rewrite the engine's route vectors
   reversibly from captured vanilla originals; the engine always sees a
   legal subgraph of its own DAG.
5. **Pay** — fee pools are distributed to each node's trade-power
   holders by share, written to the countries' treasuries through the
   same path the game's own `ChangeTreasury` uses.

Engine internals, the reverse-engineering record, and the historical
acceptance runs live in [`docs/internals.md`](docs/internals.md). The
automated end-to-end harness is `tests/e2e-mod/` (run via
`foch test --with-plugins`, each case its own real 800-day session).

## Known limits

- Pinned to Steam 1.37.5 x64; other builds are refused.
- Ironman/achievements are unavailable while any DLL plugin is loaded.
- Node membership ships as an offline table generated from vanilla
  `00_tradenodes.txt`; mods that redefine trade-node membership need it
  regenerated.
- The fee incomes are treasury writes visible in the top bar; they do
  not yet appear as a line in the budget UI.
