# EU4 Dynamic Trade Blackhole

A native DLL plugin (built on the [Foch](https://github.com/Acture/foch)
runtime host) that makes EU4's trade topology **dynamic**: instead of trade
value flowing forever toward the three hardcoded European end nodes, outgoing
routes close and reopen during play according to where wealth actually
converges — so black holes emerge where purchasing power lives, and players
can create or break them by developing provinces.

It never edits `common/tradenodes`, so it does not conflict with script-level
trade mods: the engine only ever sees its own loaded routes, reversibly
pruned in memory.

## What changes in gameplay

### 1. Dynamic route closures and reopens

**Trade converges on wealth, not on Western Europe.** Every month the plugin
runs a bidirectional propagation model over the full initial trade graph:
producers inject local trade value, hubs relay it, and high-retention nodes
absorb it.

- **Closing dead or backward routes:** Edges whose net forward flow stays
  below the close threshold (`flow_epsilon_pct`, default `5%` of the source
  node's reference pool) for `evidence_months` consecutive months (default
  `6`) close, gated by the per-edge **wealth gradient** (the source node's
  sink signal must be at least as high as the target's, so value stops
  draining from a richer node into a poorer one). Nodes whose entire outflow
  closes become emergent black holes — from a 1444 vanilla start, Malacca,
  Coromandel, and Gujarat earn that on their own.
- **Province development moves the levers:**
  - Develop **tax** (and **manpower**, which joins the sink signal at half
    weight as potential consumption) so a node outbids its downstream
    neighbor: Japan's trade stays in Nippon instead of leaking to Hangzhou.
  - Develop **production** to grow a node's local trade-value injection and
    shift its retention and outflow balance.
  - Revive a closed route's forward flow past the hysteresis reopen band
    (`reopen_pct`, default `15%` of the source node's pool) for
    `evidence_months` consecutive months, and the route **reopens** —
    closures are reversible decisions, not permanent deletions.

### 2. Three distinct role incomes

Vanilla EU4 only pays collectors. This plugin splits each node's monthly
outflow by origin into two non-overlapping fee pools so transit hubs and
upstream producers profit from their roles without double-counting a single
ducat:

| Role | Lever | Income channel |
| --- | --- | --- |
| **Sink** (collector) | Tax & manpower | **Amplified vanilla collection**: closing outgoing routes keeps arriving value inside the node |
| **Transit** (middleman) | Trade power & steering | **Transit fee**: a cut (`transit_fee_pct`, default `20%`) of the volume the node forwards on behalf of upstream partners (`outflow × inflow / (inflow + production)`) |
| **Source** (producer) | Production | **Source bounty**: a cut (`source_fee_pct`, default `20%`) of the node's own local production exported downstream (`outflow × production / (inflow + production)`) |

Both fee pools are distributed each month among the countries holding trade
power in that node, proportional to their power share (shares below `0.001`
ducats are skipped), and credited directly to each country's treasury. Pure
relays earn the transit fee in full; upstream producers earn the source
bounty even when their goods flow away; pure sinks forward nothing and earn
neither fee because their reward is the retained collection pool.

### 3. World Trade Hub Gazette, 12-month moving average, and corridor intelligence

Every policy month the plugin ranks all trade nodes by their entrepôt score —
`(current_value - production).max(0) × trade_power.max(1)` — measuring which
hubs both attract external trade value and command the power to move it
(stripping local production so pure producers do not masquerade as transit
entrepôts):

- **World Trade Hub Gazette:** Logs the monthly tier standings — the **World
  Entrepôt** (rank 1), **Great Entrepôts** (top 3), and **Regional
  Entrepôts** (top 8) — and announces whenever the World Entrepôt crown
  changes hands (`world entrepot <node> dethrones <prev>`).
- **12-month moving average:** Tracks a rolling 12-month window of the
  leading node's entrepôt score and reports the smoothed average alongside
  each month's gazette line and structured receipt, preventing single-month
  spikes from distorting the long-term benchmark.
- **Transit intelligence & reopen diagnostics:** Logs the top 10 high-volume
  transit corridors (flagging `← under-steered` whenever total steering
  effort is below 10% of transit volume) and emits per-edge `DEBUG` progress
  lines showing how close each closed route's net flow is to the
  `reopen_pct` recovery band.

## Requirements

- **EU4 1.37.5, Steam, Windows x64** — verified by executable SHA-256 at
  startup; any other binary is refused cleanly and leaves the plugin inert.
- The [Foch](https://github.com/Acture/foch) CLI (`foch-cli`) and its native
  runtime host (`foch-runtime-host`). This repository is mounted as a
  submodule of Foch at `plugins/eu4-dynamic-trade-blackhole`.

## Build and install

From the Foch workspace root:

```powershell
git submodule update --init
cargo build -p foch-plugin-trade-blackhole -p foch-runtime-host -p foch-cli --release --locked
uv run python -I plugins/eu4-dynamic-trade-blackhole/tools/assemble_pkg.py
foch plugin import ./target/plugin-pkg
foch plugin enable dev.foch.eu4-dynamic-trade-blackhole --playset default
foch plugin launch --playset default
```

`tools/assemble_pkg.py` locates the workspace `target/` directory, copies
`target/release/foch_plugin_trade_blackhole.dll` (renamed to
`trade_blackhole.dll`) and `plugins/eu4-dynamic-trade-blackhole/tools/node_members.txt`
into `target/plugin-pkg/` (or `--out <dir>`), and writes `foch-plugin.toml`
with plugin id `dev.foch.eu4-dynamic-trade-blackhole` and the computed
`[[files]]` SHA-256 digests. `foch plugin install` still takes the GitHub
repository `Acture/eu4-dynamic-trade-blackhole`; enable and presets use the
plugin id. After re-importing a rebuilt package, remove any older
`0.0.1+<hash>` store directory if `foch plugin enable` reports multiple
installed digests for the same version.

The Steam Workshop companion lives in `workshop/`: a descriptor, one optional
human-only decision, and one informational event. It does not edit trade
nodes or any other vanilla file. Subscribe to it for the playset entry, then
launch through Foch so the native plugin loads. `tools/verify_e2e_offline.py`
checks that companion, assembles `target/e2e-pkg/trade-blackhole`, and runs
`foch check`, `plugin import`, `enable`, `plan`, and `launch --prepare-only`
without starting `eu4.exe`.

## Configuration and presets

### Player presets (`tools/presets.py`)

Five built-in presets write the plugin's section in `selections.toml`
(`.foch/selections.toml` if present in the working directory, otherwise
`~/.config/foch/plugins/selections.toml`) without manual TOML editing:

```powershell
uv run plugins/eu4-dynamic-trade-blackhole/tools/presets.py --playset default --preset standard
# Pass --force to create the playset entry if it does not exist yet:
uv run plugins/eu4-dynamic-trade-blackhole/tools/presets.py --playset custom --preset gentle --force
```

| Preset | Mode | Summary |
| --- | --- | --- |
| `observer` | `observe` | Watch only: `enabled = true`, full monthly observer logs, game routes and treasuries untouched |
| `gentle` | `policy` | Slow convergence: `evidence_months = 12`, `flow_epsilon_pct = 5`, `reopen_pct = 20`, `max_flips_total = 8`, `10%` transit & source fees |
| `standard` | `policy` | Balanced default: `evidence_months = 6`, `flow_epsilon_pct = 5`, `reopen_pct = 15`, `max_flips_total = 16`, `20%` transit & source fees |
| `assertive` | `policy` | Fast convergence: `evidence_months = 4`, `flow_epsilon_pct = 3`, `reopen_pct = 12`, `max_flips_total = 24`, `25%` transit & source fees, `observer_log = false` |
| `sandbox` | `observe` | Master switch off (`enabled = false`, `observer_log = true`) — inert first-run sanity check |

`tools/presets.py` is idempotent: re-running updates only
`dev.foch.eu4-dynamic-trade-blackhole` under the target playset and preserves
all other playsets and plugin configurations.

### Configuration reference

Individual keys can be overridden under
`[playsets.<name>.plugins."dev.foch.eu4-dynamic-trade-blackhole".config]`:

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | `bool` | `true` | Master switch; `false` leaves the plugin an inert observer after startup |
| `mode` | `string` | `"observe"` | `"observe"` logs without mutating; `"force_toggle"` alternates closing/reopening `force_node` each month; `"policy"` runs the propagation planner |
| `force_node` | `string` | `"Great Lakes"` | Engine display name of the node toggled in `force_toggle` mode (case-insensitive, supports spaces and non-ASCII such as `"Lübeck"`) |
| `observer_log` | `bool` | `true` | Emit per-node monthly `DEBUG` snapshots (`tax`, `prod`, `power`, `value`, `retention`, `edges`) |
| `sink_basis` | `string` | `"mean"` | How member provinces' `tax + 0.5 × manpower` aggregates into a node's sink signal: `"mean"` (per-province wealth intensity) or `"sum"` (raw regional mass) |
| `relay_ref` | `int` | `3000` | Trade power (thousandths, `> 0`) at which a partner doubles its relay pull (`1 + steering/1000 + trade_power/relay_ref`); higher values flatten hub preference |
| `flow_epsilon_pct` | `int` | `5` | Close threshold (`0..=50`), as a percentage of the source node's reference pool (`max(retained, production, 1)`) |
| `reopen_pct` | `int` | `15` | Reopen hysteresis threshold (`flow_epsilon_pct..=100`), as a percentage of the source node's reference pool |
| `evidence_months` | `int` | `6` | Consecutive months of sustained below-epsilon or above-reopen flow required before an edge flips |
| `max_flips_total` | `int` | `16` | Maximum route flips (`> 0`) the entire graph may apply in a single month |
| `transit_fee` | `bool` | `true` | Enable monthly distribution of transit-fee and source-bounty pools |
| `transit_fee_pct` | `int` | `20` | Transit fee pool (`0..=100`), as a percentage of volume forwarded for upstream nodes |
| `source_fee_pct` | `int` | `20` | Source bounty pool (`0..=100`), as a percentage of local production exported out of the node |

Any unknown enum value or out-of-range threshold automatically downgrades the
plugin to safe observe-only mode (`enabled = false`, `mode = "observe"`).
Decisions and diagnostics stream to the host event log and can be inspected
with `foch plugin status --run-dir <dir>`.

## Overhaul and map-mod compatibility

- **Unbounded from vanilla entity counts:** The shared `foch-runtime` data
  plane does not restrict trade nodes to a narrow vanilla band (`20..=512`)
  or cap countries at `1024`. It accepts **`1..=4096` trade nodes**
  (`MAX_TRADE_NODES`), **`1..=32767` countries** (`i16::MAX`), and up to
  `65,536` member provinces per node — supporting both compact regional
  scenarios and large total-conversion mods (Anbennar, MEIOU and Taxes,
  Voltaire's Nightmare, etc.) without recompilation.
- **Two-tier member-province resolution (live vector with automatic
  fallback):**
  1. **Primary — shared data-plane live vector:** Every 6 months when
     aggregating node tax and manpower, the runtime first attempts to read
     each node's member-province list directly from live engine memory via
     `NodeView::member_provinces` (`CTradeNodeDefinition + 0x50`,
     `std::vector<i32>`). *Note: this offset is statically identified for
     EU4 1.37.5 and is pending live in-game verification.*
  2. **Fallback — packaged `node_members.txt`:** Whenever the live vector
     read returns `None` or is empty for a node, the runtime automatically
     falls back to the offline `node_members.txt` table and logs a per-source
     summary (`member source: <N> live vector ..., <M> static node_members.txt`).
     If fewer than 75% of nodes resolve a member list, the plugin logs a
     one-time warning naming `tools/gen_members.py` while keeping flow
     propagation, role fees, and the gazette running.
- **Regenerating `node_members.txt` (`tools/gen_members.py`):**
  The shipped `tools/node_members.txt` is generated from vanilla
  `00_tradenodes.txt`. To regenerate the fallback table for a modded playset,
  run `tools/gen_members.py`, which replicates EU4's virtual directory loader
  for `common/tradenodes/*.txt` — layering vanilla first, applying
  `dlc_load.json` `enabled_mods` in exact load order, honoring
  `replace_path` directives from the `.mod` descriptors (or `descriptor.mod`
  when a mod directory is passed directly), reading both directory mods and
  `.zip` Workshop archives, and merging surviving files in case-sensitive
  filename order:

  ```powershell
  # From a launcher dlc_load.json playset:
  uv run plugins/eu4-dynamic-trade-blackhole/tools/gen_members.py `
    --game "<EU4 install>" --playset "<path/to/dlc_load.json>" `
    --out plugins/eu4-dynamic-trade-blackhole/tools/node_members.txt

  # Or by passing .mod descriptor files / mod directories directly in load order:
  uv run plugins/eu4-dynamic-trade-blackhole/tools/gen_members.py `
    --game "<EU4 install>" path/to/overhaul.mod path/to/submod_dir `
    --out plugins/eu4-dynamic-trade-blackhole/tools/node_members.txt
  ```

  Re-run `tools/assemble_pkg.py` and `foch plugin import ./target/plugin-pkg`
  afterwards so the packaged plugin carries the updated fallback table.

## Offline test verification and `FOCH_RECEIPT/1` receipts

The end-to-end test mod under `plugins/eu4-dynamic-trade-blackhole/tests/e2e-mod`
defines annotated `#test(...)` scenarios covering baseline convergence, tax
and production farming, scheduled route reopening, treasury income A/B
comparisons, and receipt delivery.

You can discover and validate all test cases offline without launching
`eu4.exe`:

```powershell
# List all discovered e2e test cases:
foch test plugins/eu4-dynamic-trade-blackhole/tests/e2e-mod --collect-only

# Validate test annotations, expect/expect_receipt blocks, and event scripts offline:
foch test plugins/eu4-dynamic-trade-blackhole/tests/e2e-mod --no-run
```

During policy runs, the plugin emits structured **`FOCH_RECEIPT/1 <json>`**
lines (`case = "policy_month"`, `request = "<tick>"`, all payload values
serialized as strings) that `expect_receipt` blocks in `foch test` can assert
against:

| Receipt `type` | Trigger | Payload fields |
| --- | --- | --- |
| `edge_flip` | A route closes or reopens | `case`, `request`, `tick`, `action` (`"close"` \| `"reopen"`), `node`, `target`, `net_flow`, `pool` |
| `role_fees` | A node distributes a non-empty fee pool | `case`, `request`, `tick`, `kind` (`"transit fee"` \| `"source bounty"`), `node`, `volume`, `pool`, `recipients` |
| `gazette` | Monthly World Entrepôt update | `case`, `request`, `tick`, `world_entrepot`, `previous_entrepot` (`""` on first crown), `world_score` (12-month moving average) |

## How it works

Monthly, on the game thread, inside the economy-update hook:

1. **Inject** — each node's local trade value (`goods × price`, read from the
   engine) enters its pool.
2. **Propagate** — pools relax over 8 rounds across the *full bidirectional
   initial graph* (closed edges included, so reopens stay possible and the
   model never cascades on its own closures). Relay weight is
   `1 + steering/1000 + partner_power/relay_ref`; retention is the engine's
   own `value / (value + transferred)` rate.
3. **Judge** — every edge is judged by its accumulated net forward flow
   against `flow_epsilon_pct` and `reopen_pct`, gated on closure by the
   per-edge wealth gradient (`source.tax >= target.tax`), `evidence_months`
   of consecutive evidence, and the `max_flips_total` monthly flip budget.
4. **Mutate** — closures and reopens rewrite the engine's live route vectors
   reversibly from captured originals (open routes first, closed routes
   parked at the tail, vector `end` shrunk to the open prefix).
5. **Report & Pay** — emits the transit intelligence lines, the World Trade
   Hub Gazette (with the 12-month moving average), closed-edge reopen
   diagnostics, and `FOCH_RECEIPT/1` receipts, then distributes transit-fee
   and source-bounty pools to each node's trade-power holders via the same
   clamped treasury write and UI-refresh bit as `CCountry::ChangeTreasury`.

Engine layout details, reverse-engineering records, and historical acceptance
logs are documented in [`docs/internals.md`](docs/internals.md).

## Verification status and known limits

- **Verified in live 800-day EU4 1.37.5 sessions:**
  - Economy-update hook installation and monthly tick cadence.
  - Reversible route-vector closures and reopens across baseline and
    development-farming scenarios (a tax-farmed Nippon closes its single
    outgoing route at month 8; a scheduled 1446 farm drives a closed
    `hangzhou -> xi'an` route's net flow from `−1.9%` to `+24.1%` and reopens
    it at month 22).
  - Role-income payouts and top-bar treasury updates (the same Malacca farm
    run with and without the policy leaves Malacca a full treasury bracket
    higher `[400, 600)` vs. `[200, 400)`, holding `48.5` vs. `34.9` trade
    value in the closed black hole over 26 months).
- **Verified by unit tests and offline checks (`--collect-only` / `--no-run`), pending live in-game verification:**
  - Dynamic member-province reading from `CTradeNodeDefinition + 0x50`
    (`NodeView::member_provinces`) is statically identified from reverse
    engineering and covered by unit tests, but has not yet been probed in a
    live EU4 session. The runtime automatically falls back to
    `node_members.txt` if the live vector is unavailable or empty.
  - The newest `FOCH_RECEIPT/1` structured receipt stream (`edge_flip`,
    `role_fees`, `gazette`) combined with the World Trade Hub Gazette and
    12-month moving-average leader score is verified in unit tests and
    offline `foch test --no-run` validation, and awaits a full live
    `foch test --with-plugins` session run.
- **Other limits:**
  - Pinned strictly to **Steam EU4 1.37.5 (Windows x64)**; other builds are
    refused at load.
  - Ironman and achievements are unavailable while any native DLL plugin is
    loaded.
  - When relying on the static `node_members.txt` fallback, mods that alter
    trade-node province memberships must regenerate `node_members.txt` with
    `tools/gen_members.py`.
  - Transit fees and source bounties are direct treasury writes visible in
    the top-bar ducat counter; they do not appear as a separate line item in
    the in-game budget ledger UI.
