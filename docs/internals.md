# Internals

Engineering record for contributors: the reversed layout, the mutation
primitive, and the historical acceptance runs. Everything here is also
documented inline in `src/offsets.rs` and the module headers; this file
collects it in one place.

## Reverse-engineering record (EU4 1.37.5 Steam x64)

Reversed with Ghidra 12.1.3 (headless PyGhidra) against `eu4.exe`
sha256 `c9ec55a4783fc0f250bbf536dfd25c8ec190ed0ed621384ca501908c60ae9f8e`.
The binary has no RTTI; the anchors were found through the `tradenodes`
string cluster, which carries the developers' own source paths
(`source/tradenode.cpp`, `tradenodedatabase.cpp`,
`tradenodedefinition.cpp`), then confirmed instruction by instruction
and corrected against live memory dumps from automated test sessions.

Hook target: the economy update at image+0x75d690 that ends with
`CTradeNodeDataBase::UpdateAll(ctx + 0x2198)` (image+0xb4ba90). It was
chosen over `UpdateAll` itself because the host's hook gate only steals
whole instructions that end exactly at byte 14, and this candidate's
prologue (`MOV [rsp+0x10],DL` + seven pushes) tiles exactly 14 bytes.
Its first argument is the global game context; the plugin cross-checks
it against the context global before every use. The hook fires on the
game's monthly economy cadence (a 70-day automated session produced
exactly four ticks), so each tick is one policy month.

| Structure | Offset | Meaning |
| --- | --- | --- |
| global context ptr | image+0x233fe78 | the hook's first argument |
| country registry | image+0x233d8d0 → +0x118 | country-index → `CCountry*` array |
| country vector | ctx+0x1d08 / +0x1d14 | `CCountry*` begin / int count |
| `CTradeNodeDataBase` | ctx+0x2198 | embedded, not allocated |
| db | +0x30 / +0x3c | node pointer array (stride 8) / count; 81 nodes |
| node | +0x88 | `vector<int32>` steering weights, paired with routes |
| node | +0xa8 | `CTradeNodeDefinition*` |
| node | +0xb0 / +0xb4 / +0xbc | value / local trade value / transferred (int32, thousandths); retention = b0/(b0+bc) |
| node | +0xc8 | total trade power |
| node | +0x18 | per-country entry table, stride 0xc0 (power at +0x28, merchant-mode flags at +0xac/ad/ae) |
| def | +0x30 | MSVC SSO display name |
| def | +0x98 / +0xa0 | `vector<CTradeRoute>` begin/end, stride 0x78 |
| route | +0x10 | MSVC SSO internal key of the target |
| route | +0x30 | target node's `CTradeNodeDefinition*` |
| province array | ctx+0x1ca8 | contiguous `CProvince`, stride 0x2e10, indexed by province id |
| `CProvince` | +0x3e0 / +0x3e4 | base manpower / base tax (int32, thousandths) |
| `CCountry` | +0x20 / +0xbb0 | packed tag+index / treasury (int32, thousandths, ±1e9 clamp) |
| UI controller | ctx+0x1e00 → +0x388 | flag word; bit 0x40 refreshes the player's treasury label |

The treasury write mirrors the game's own `ChangeTreasury` console
handler (`country.cpp:0x5882`), including the clamp and the UI dirty
bit. The unit was calibrated live: SWE's treasury field reads 48195 =
48.2 ducats, i.e. thousandths, matching the trade-value scale.

## Mutation primitive

Per edge. The plugin captures a node's vanilla route entries and
steering weights before its first mutation, and every state change
rewrites the live arrays from the originals — open entries first in
vanilla order, closed entries parked at the tail — then shrinks the
route vector's `end` to the open prefix. Any open subset is a
contiguous, legal vector (exactly what vanilla end nodes look like),
all writes stay inside the original capacity, and restoring every flag
rebuilds the byte-exact vanilla layout. The paired steering-weight
vector's `end` is left untouched: the engine walks the weights by the
route count, and the parked tail keeps the observe-side permutation
recovery simple. The vanilla outgoing list the policy plans against is
frozen at the first observe, so live reordering never disturbs vanilla
slot indices.

Invariants the backend re-checks every observe: node count within
`1..=MAX_TRADE_NODES` (4096, the shared dataplane gate), every
node/definition pointer plausible, route vector strides exact, SSO names
readable, and the exe digest verified before any pattern runs.

## Design history (what was tried and replaced)

The policy went through several designs, each retired for a specific
failure — recorded here so they do not come back:

1. **Blended wealth score** (tax+power+production in one number):
   Girin's huge trade power made it "richer" than Beijing and cut the
   edge to the region's actual wealth center. Rejected: traits are
   roles, never weights.
2. **Neighborhood tax-peak**: the richest node of a neighborhood closes
   everything. Too strict — Beijing could not close its edge to poor
   Yumen because rich Hangzhou sat on the incoming side. Replaced by
   the per-edge gradient (source tax ≥ target tax).
3. **Pruned-graph propagation**: propagating only over open edges made
   closed edges' flow zero by construction, so reopens became
   impossible and closures cascaded. Replaced by always propagating on
   the full vanilla graph.
4. **absorb_ref / colonial suppression**: artificial formulas. Replaced
   by the engine's own retention rate (node+0xb0/(+0xb0+0xbc)).
5. **Fee = inflow − outflow**: paid sinks the most (they retain
   everything) and producers nothing. Replaced by splitting the outflow
   by origin: transit share (inflow-proportional) and source share
   (production-proportional).

## Historical acceptance runs

- **2026-11-06, force_toggle smoke** (`foch test --with-plugins`,
  1444.11.11 SWE, 70 days): Great Lakes' two routes closed and fully
  restored four times in one session, clean exit, empty error log.
- **2026-11-07, 7-case farm matrix** (800 days each, own session):
  baseline vs tax-only / production-only / upstream farming on the
  Malacca region. Tax farming flips the wealth gradient (Nippon
  3884→15884 vs Hangzhou 6315) and closes its only edge at month 8;
  Siam tax+prod closes siam→canton with retention 0.76→0.88; production
  farming raises the injection (node+0xb4 follows `add_base_production`
  within the month); upstream farming raises hub transit volume and
  changed a decision two hops away.
- **2026-11-11, scheduled reopen** (natural MTTH event applies the farm
  at 1446 after the baseline close): hangzhou→xi'an net flow −1.9% →
  +24.1% the month the farm lands, reopen at month 22; the same farm
  also closed yumen→samarkand. The reopen needed three levers together
  — source production (forward scales), target tax (relay share past
  the 15% band), and developing the target's other partners (a
  retention-0 partner relays >50% back and cancels the forward).
- **2026-11-11, income A/B** (same malacca farm, policy vs observe
  playsets): treasury ladder [400,600) with the plugin vs [200,400)
  vanilla at identical monthly income; malacca node value 48491
  (closed, retention 1.00) vs 34943 (open, engine leaks 7%); 426
  transit-fee and 1260 source-bounty payment lines over 26 months,
  distributed by power share (Ming takes 100% of Hangzhou's source
  bounty, 93% of Beijing's transit fee).

## Headless test environment

The automated sessions launch the real game; after a reboot with nobody
logged into the desktop, the game refuses to start from a service
session. The rescue chain (session to console via `tscon`, display kept
awake via `SetThreadExecutionState`, the "Steam is not running." modal
auto-closed by window enumeration) is scripted in the RE workspace, not
in this repository. Test effects must be real effects: `owner = X` and
`treasury = 0` are trigger syntax and crash the game at effect
application — use `change_owner` / `add_treasury`.

## Extracting shared services toward the host

Pieces of this plugin are generally useful and should not stay
plugin-private. Candidates, in extraction order:

1. **Safe memory accessors** (`read_qword`/`read_i32`/`read_u8`/
   `write_*` with plausibility guards) — pure utility, plugin-agnostic.
2. **The exe pin + pattern-resolution flow** (digest check, signature
   table per build) — every pinned-build plugin reimplements this.
3. **The route-vector mutation primitive** (capture vanilla, open-first
   rewrite, reversible end shrink) — any topology-touching plugin wants
   exactly this.
4. **Country services** (index→`CCountry*`, treasury payment with the
   UI dirty bit) — income-touching plugins want this.

Items 1–2 belong in the host's API table (append-only, versioned by
`abi_major`); items 3–4 are EU4 game knowledge rather than host
services and fit better as a shared `foch-eu4-sdk` crate that plugins
depend on alongside `foch-plugin-abi`. Deliberately not done yet:
extending the C ABI table is a breaking-change decision for the host
project and deserves its own review, and publishing `foch-plugin-abi`
to crates.io (it is 211 lines, zero dependencies, currently
`publish = false`) would make standalone plugin builds possible — one
`cargo publish` away once the foch side opts in.
