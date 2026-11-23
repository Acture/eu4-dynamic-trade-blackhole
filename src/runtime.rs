//! The live engine backend over the typed trade views, plus the
//! recalculation entry the detour drives.
//!
//! [`monthly`] resolves the hook's context with
//! [`foch_runtime::dataplane::Eu4Game::from_tick`]. Node, route, tax,
//! participant, and treasury access then go through [`TradeNodes`],
//! [`NodeView`], [`Province`], [`Eu4Game::pay_country`], and
//! [`RouteMutation`]. Closing one vanilla edge rewrites that node's live
//! route vector from the captured original: open entries stay first,
//! closed entries park at the tail, and the vector end shrinks to the
//! open prefix.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::Config;
use crate::engine::{Engine, EngineBackend};
use crate::policy::NodeSnapshot;
use foch_runtime::dataplane::{self, Eu4Game, NodeView, RouteMutation, TradeNodes};

type LiveEngine = Engine<LiveBackend, fn(i32, &str)>;

static ENGINE: OnceLock<Mutex<LiveEngine>> = OnceLock::new();
static EXE_BASE: AtomicUsize = AtomicUsize::new(0);

/// The host-reported image base of this process's `eu4.exe`.
/// Install the engine the economy-update detour drives. `exe_base` is the
/// host-reported image base, used to cross-check the context pointer;
/// `plugin_dir` is the plugin's own read-only package directory, where the
/// offline node-members table ships.
pub fn install(config: Config, exe_base: usize, plugin_dir: Option<std::path::PathBuf>) {
	EXE_BASE.store(exe_base, Ordering::Release);
	let members = plugin_dir
		.and_then(|dir| std::fs::read(dir.join("node_members.txt")).ok())
		.map(|bytes| parse_members(&bytes))
		.unwrap_or_default();
	if members.is_empty() {
		crate::logging::write(
			crate::logging::WARN,
			"node_members.txt missing or empty: node tax stays zero",
		);
	}
	let basis = config.sink_basis;
	let _ = ENGINE.set(Mutex::new(Engine::new(
		LiveBackend::with_members(members, basis),
		config,
		crate::logging::write,
	)));
}

/// Parse the offline `node_members.txt`: one line per node,
/// `<internal key> <province id>...`.
fn parse_members(bytes: &[u8]) -> Vec<(String, Vec<u32>)> {
	let text = String::from_utf8_lossy(bytes);
	let mut out = Vec::new();
	for line in text.lines() {
		let mut fields = line.split_whitespace();
		let Some(key) = fields.next() else { continue };
		let ids: Vec<u32> = fields.filter_map(|f| f.parse().ok()).collect();
		if !ids.is_empty() {
			out.push((key.to_string(), ids));
		}
	}
	out
}

/// Where a node's member-province list came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemberSource {
	/// `NodeView::member_provinces`.
	/// Statically identified for EU4 1.37.5; pending live verification.
	LiveVector,
	/// Offline `node_members.txt` from `tools/gen_members.py`.
	StaticTable,
}

/// Prefer the live member vector. `None` or an empty vector falls back to
/// the offline table; an empty table entry is not a usable member list.
fn resolve_members(
	live: Option<&[u32]>,
	static_ids: Option<&[u32]>,
) -> Option<(MemberSource, Vec<u32>)> {
	if let Some(ids) = live.filter(|ids| !ids.is_empty()) {
		return Some((MemberSource::LiveVector, ids.to_vec()));
	}
	static_ids
		.filter(|ids| !ids.is_empty())
		.map(|ids| (MemberSource::StaticTable, ids.to_vec()))
}

fn member_source_summary(
	covered: usize,
	count: usize,
	live_nodes: usize,
	static_nodes: usize,
) -> String {
	format!(
		"tax aggregated for {covered}/{count} nodes; member source: {live_nodes} live vector (CTradeNodeDefinition members, statically identified, pending live verification), {static_nodes} static node_members.txt",
	)
}

/// One economy-update step on the game thread. `context` is the hook's
/// first argument; it is accepted only when it matches the pinned global.
pub fn monthly(context: usize) {
	let Some(engine) = ENGINE.get() else { return };
	let base = EXE_BASE.load(Ordering::Acquire);
	if Eu4Game::from_tick(base, context).is_none() {
		crate::logging::write(
			crate::logging::WARN,
			&format!("context 0x{context:x} is not the global; skipping this update"),
		);
		return;
	}
	if let Ok(mut engine) = engine.lock() {
		engine.set_source(context, context);
		engine.tick();
	}
}

/// Vanilla route capture plus the edge count frozen before the first shrink.
struct CapturedRoute {
	mutation: RouteMutation,
	edges: usize,
}

/// Reads and mutates the engine's trade structures. Every access happens
/// on the game thread inside the recalculation hook, so the database and
/// its nodes are stable for the whole call.
pub struct LiveBackend {
	/// The global game context, kept for [`Eu4Game::from_tick`].
	context: usize,
	/// Image base captured for this backend. Zero falls back to [`EXE_BASE`].
	exe_base: usize,
	/// Per-node mutation state, captured before the first mutation.
	mutations: HashMap<u32, CapturedRoute>,
	/// Vanilla outgoing targets per node index, frozen at the first
	/// observe and reported from cache afterwards.
	vanilla_outgoing: HashMap<u32, Vec<u32>>,
	/// Definition pointer and name per node index, refreshed on observe.
	names: Vec<(usize, String)>,
	/// Whether the weights-pairing diagnostic was already logged.
	pairing_logged: bool,
	/// Whether the node-field diagnostic was already logged.
	fields_logged: bool,
	/// Observes served, for the periodic state summary.
	observes: u32,
	/// Whether the route-entry dump was already logged.
	entry_logged: bool,
	/// Whether the one-shot country probe already ran.
	countries_probed: bool,
	/// Whether the modded-trade-map warning already fired.
	members_warned: bool,
	/// Offline node-members table: internal key -> member province ids,
	/// shipped in the plugin package (generated from 00_tradenodes.txt).
	/// Used when the live member vector is missing or empty.
	member_ids: HashMap<String, Vec<u32>>,
	/// Per-node-index sink signal, built at the first observe from the
	/// members' province base tax: the per-province mean or the sum.
	node_tax: HashMap<u32, f64>,
	/// Which basis `node_tax` holds.
	sink_basis: crate::config::SinkBasis,
}

impl LiveBackend {
	pub fn new() -> Self {
		Self::with_members(Vec::new(), crate::config::SinkBasis::Mean)
	}

	fn with_members(
		members: Vec<(String, Vec<u32>)>,
		sink_basis: crate::config::SinkBasis,
	) -> Self {
		Self {
			context: 0,
			exe_base: 0,
			mutations: HashMap::new(),
			vanilla_outgoing: HashMap::new(),
			names: Vec::new(),
			pairing_logged: false,
			fields_logged: false,
			observes: 0,
			entry_logged: false,
			countries_probed: false,
			members_warned: false,
			member_ids: members.into_iter().collect(),
			node_tax: HashMap::new(),
			sink_basis,
		}
	}

	fn resolved_base(&self) -> usize {
		if self.exe_base > 0x1_0000 {
			self.exe_base
		} else {
			EXE_BASE.load(Ordering::Acquire)
		}
	}

	fn game(&self) -> Option<Eu4Game> {
		Eu4Game::from_tick(self.resolved_base(), self.context)
	}

	/// Sum the members' base tax for every node, once the province array
	/// is available. Member provinces come from
	/// [`NodeView::member_provinces`]. `None` or an empty vector falls
	/// back to the offline `node_members.txt` table. That table is keyed
	/// by the internal route key from [`NodeView::route_target_key`].
	fn aggregate_taxes(&mut self, count: usize) {
		// Re-aggregate every 6 months alongside the evidence window:
		// province development changes tax and manpower, and the sink
		// signal must track it.
		if !self.node_tax.is_empty() && !self.observes.is_multiple_of(6) {
			return;
		}
		self.node_tax.clear();
		let Some(game) = self.game() else {
			return;
		};
		// `province(0)` only checks that the province array pointer is
		// plausible. During Late History that pointer is not yet safe;
		// retrying on a later observe is the load-phase gate.
		if game.province(0).is_none() {
			return;
		}
		let Some(trade) = game.trade_nodes() else {
			return;
		};
		let mut key_by_def: HashMap<usize, String> = HashMap::new();
		if !self.member_ids.is_empty() {
			for index in 0..count {
				let Some(view) = trade.node(index) else {
					continue;
				};
				Self::collect_route_keys(&trade, &view, &mut key_by_def);
			}
		}
		let mut live_nodes = 0usize;
		let mut static_nodes = 0usize;
		for index in 0..count {
			let Some(view) = trade.node(index) else {
				continue;
			};
			let live = view.member_provinces();
			let static_ids = self.static_member_ids(view.definition, &key_by_def);
			let Some((source, ids)) = resolve_members(live.as_deref(), static_ids.as_deref())
			else {
				continue;
			};
			match source {
				MemberSource::LiveVector => live_nodes += 1,
				MemberSource::StaticTable => static_nodes += 1,
			}
			let mut sum = 0.0;
			let mut manpower_sum = 0.0;
			let mut counted = 0.0;
			for id in &ids {
				let Some(province) = game.province(*id) else {
					continue;
				};
				let Some(tax) = province.tax() else {
					continue;
				};
				sum += tax as f64;
				// Manpower is potential consumption: a large population
				// represents purchasing power that tax alone understates.
				// It joins the sink signal at half weight so tax stays
				// the dominant expression of current wealth.
				if let Some(man) = province.manpower() {
					manpower_sum += man as f64;
				}
				counted += 1.0;
			}
			// The mean expresses wealth intensity; the raw sum expresses
			// mass. A big reworked region full of poor provinces wins the
			// sum but not the mean. Manpower joins at half weight as
			// potential consumption.
			let combined = sum + manpower_sum * 0.5;
			let signal = match self.sink_basis {
				crate::config::SinkBasis::Mean if counted > 0.0 => combined / counted,
				_ => combined,
			};
			self.node_tax.insert(index as u32, signal);
		}
		crate::logging::write(
			crate::logging::INFO,
			&member_source_summary(self.node_tax.len(), count, live_nodes, static_nodes),
		);
		// A modded trade map replaces the nodes the offline members
		// table was built from: tax coverage collapses while the rest of
		// the policy (flow, fees, gazette) keeps working. Name the fix
		// once per session instead of failing silently.
		let covered = self.node_tax.len();
		if covered * 4 < count * 3 && !self.members_warned {
			self.members_warned = true;
			crate::logging::write(
				crate::logging::WARN,
				&format!(
					"tax covers only {covered}/{count} nodes: the members table does not cover this trade map; regenerate it with tools/gen_members.py --game <EU4> --playset <dlc_load.json> --out node_members.txt"
				),
			);
		}
	}

	/// Read each live route's internal key and remember it under the
	/// target definition. Keys come from [`NodeView::route_target_key`]
	/// (the same string as [`TradeNodes::route_target_key`] and one entry
	/// of [`NodeView::outgoing_target_keys`]). The stride is the public
	/// route span divided by [`NodeView::live_route_count`], so the walk
	/// does not carry a size constant. One unreadable key is skipped;
	/// [`NodeView::outgoing_target_keys`] would drop the whole vector.
	/// An empty vector contributes nothing.
	fn collect_route_keys(nodes: &TradeNodes, view: &NodeView, into: &mut HashMap<usize, String>) {
		let Some(routes) = view.live_route_count(nodes) else {
			return;
		};
		if routes == 0 {
			return;
		}
		let Some((begin, end)) = nodes.routes(view.definition) else {
			return;
		};
		let stride = (end - begin) / routes;
		if stride == 0 {
			return;
		}
		for slot in 0..routes {
			let Some(offset) = slot.checked_mul(stride) else {
				continue;
			};
			let Some(addr) = begin.checked_add(offset) else {
				continue;
			};
			let Some(target) = view.route_target(addr) else {
				continue;
			};
			if into.contains_key(&target) {
				continue;
			}
			if let Some(key) = view.route_target_key(addr) {
				into.insert(target, key);
			}
		}
	}

	/// The offline table row for one definition. The key is the string an
	/// incoming route recorded; missing when that route or the table row
	/// is absent.
	fn static_member_ids(
		&self,
		definition: usize,
		key_by_def: &HashMap<usize, String>,
	) -> Option<Vec<u32>> {
		let key = key_by_def.get(&definition)?;
		self.member_ids.get(key).cloned()
	}

	/// One-shot diagnostic: a few countries' tag and treasury, so the
	/// e2e probe (a known `add_treasury` effect) can pin the field's unit.
	fn probe_countries(&self) {
		let Some(count) = self.country_count() else {
			crate::logging::write(crate::logging::DEBUG, "country probe: registry unavailable");
			return;
		};
		let Some(countries) = self.game().and_then(|game| game.countries()) else {
			return;
		};
		let mut lines = Vec::new();
		for index in 0..count.min(6) {
			let Some(country) = countries.country(index) else {
				continue;
			};
			lines.push(format!(
				"country {index} tag {} treasury {:?}",
				country.tag(),
				country.treasury()
			));
		}
		crate::logging::write(
			crate::logging::DEBUG,
			&format!("country probe ({count} countries): {}", lines.join("; ")),
		);
	}

	/// The number of countries [`Eu4Game::countries`] accepts (`1..=i16::MAX`).
	fn country_count(&self) -> Option<usize> {
		let count = self.game()?.countries()?.count();
		(1..=i16::MAX as usize).contains(&count).then_some(count)
	}

	/// Trade-node count through [`Eu4Game::trade_nodes`].
	///
	/// The shared gate is `1..=MAX_TRADE_NODES` (4096). Counts below 20
	/// (small regional maps) and above 512 (total conversions) stay valid;
	/// only zero, negative, and clearly corrupt counts are rejected.
	fn node_count(&self) -> Option<usize> {
		let count = self.game()?.trade_nodes()?.count();
		(1..=dataplane::game::MAX_TRADE_NODES)
			.contains(&count)
			.then_some(count)
	}

	/// Capture a node's vanilla routes and weights before the first
	/// mutation so every later state is a rewrite of the originals.
	fn capture(&mut self, node: u32) -> Result<(), &'static str> {
		if self.mutations.contains_key(&node) {
			return Ok(());
		}
		let game = self.game().ok_or("no exe base")?;
		let nodes = game.trade_nodes().ok_or("implausible node count")?;
		let view = nodes
			.node(node as usize)
			.ok_or("implausible node pointer")?;
		let edges = view
			.live_route_count(&nodes)
			.ok_or("implausible route vector")?;
		let mutation = view
			.capture_routes(&nodes)
			.ok_or("route bytes unreadable")?;
		self.mutations
			.insert(node, CapturedRoute { mutation, edges });
		Ok(())
	}

	/// Steering weights in vanilla edge order. After a mutation the live
	/// slots hold the open-first permutation.
	fn vanilla_steering(&self, node: u32, outgoing: usize, weights: &[f64]) -> Vec<f64> {
		let order = match self.mutations.get(&node) {
			Some(state) => {
				let closed: Vec<bool> = (0..state.edges)
					.map(|edge| state.mutation.is_closed(edge))
					.collect();
				dataplane::live_order(&closed)
			}
			None => (0..outgoing).collect(),
		};
		let mut steering = vec![0.0_f64; outgoing];
		for (slot, &vanilla_slot) in order.iter().enumerate() {
			if slot >= weights.len() || vanilla_slot >= steering.len() {
				continue;
			}
			steering[vanilla_slot] = weights[slot];
		}
		steering
	}
}

impl Default for LiveBackend {
	fn default() -> Self {
		Self::new()
	}
}

impl EngineBackend for LiveBackend {
	fn describe(&self) -> &'static str {
		"live EU4 1.37.5 trade database"
	}

	fn set_source(&mut self, context: usize, _db: usize) {
		self.context = context;
		self.exe_base = EXE_BASE.load(Ordering::Acquire);
	}

	fn observe(&mut self) -> Result<Vec<NodeSnapshot>, &'static str> {
		if self.context == 0 {
			return Err("no database yet");
		}
		let count = self.node_count().ok_or("implausible node count")?;
		let game = self.game().ok_or("no database yet")?;
		let nodes = game.trade_nodes().ok_or("implausible node count")?;
		for index in 0..count {
			nodes.node(index).ok_or("implausible node pointer")?;
		}
		self.names.clear();
		// The vanilla outgoing edges, captured at the first observe (which
		// always precedes any mutation) and reported from the cache after
		// that: mutations reorder and shrink the live vector, so the live
		// order cannot serve as a stable vanilla-slot index for the policy
		// mirror. The live vector is still counted below as a diagnostic.
		if self.vanilla_outgoing.is_empty() {
			for index in 0..count {
				let view = nodes.node(index).expect("validated above");
				let outgoing = view
					.outgoing_targets(&nodes)
					.ok_or("implausible route vector")?;
				self.vanilla_outgoing.insert(
					index as u32,
					outgoing.into_iter().map(|target| target as u32).collect(),
				);
			}
		}
		let mut snapshots = Vec::with_capacity(count);
		for index in 0..count {
			let view = nodes.node(index).expect("validated above");
			self.names
				.push((view.definition, view.name().unwrap_or_default()));
			let outgoing = self
				.vanilla_outgoing
				.get(&(index as u32))
				.cloned()
				.unwrap_or_default();
			let route_count = view
				.live_route_count(&nodes)
				.ok_or("implausible route vector")?;
			// After mutations the live vector holds exactly the open
			// entries; a drift from the plugin's own closed flags would
			// mean an external writer touched the topology.
			if let Some(state) = self.mutations.get(&(index as u32)) {
				let expected = (0..state.edges)
					.filter(|edge| !state.mutation.is_closed(*edge))
					.count();
				if route_count != expected {
					crate::logging::write(
						crate::logging::WARN,
						&format!(
							"node {index}: live routes {route_count} but {expected} expected open"
						),
					);
				}
			}
			// The steering weights are expected to pair with the routes
			// entry for entry, but that pairing is a spike hypothesis, not
			// a pinned fact: log the first mismatch as a diagnostic and
			// keep observing, so a live session tells us the real shape.
			let weights = view.steering();
			if !self.pairing_logged && weights.len() != route_count {
				self.pairing_logged = true;
				crate::logging::write(
					crate::logging::DEBUG,
					&format!(
						"weights pairing differs on node {index}: {} weight slots vs {route_count} routes",
						weights.len()
					),
				);
			}
			// Names are the proof the definitions were read. The first
			// observe logs a few of them with the live route count.
			if !self.fields_logged {
				self.fields_logged = true;
				for probe in 0..count.min(3) {
					let probed = nodes.node(probe).expect("validated above");
					let name = probed.name().unwrap_or_default();
					crate::logging::write(
						crate::logging::DEBUG,
						&format!(
							"node {probe}: ptr 0x{:x}, def 0x{:x}, live routes {:?}, name {name:?}",
							probed.raw_address(),
							probed.definition,
							probed.live_route_count(&nodes),
						),
					);
				}
			}
			let retained_value = view.value().unwrap_or(0) as f64;
			let transferred = view.transferred().unwrap_or(0) as f64;
			let retention = if retained_value + transferred > 0.0 {
				retained_value / (retained_value + transferred)
			} else {
				0.0
			};
			let steering = self.vanilla_steering(index as u32, outgoing.len(), &weights);
			snapshots.push(NodeSnapshot {
				id: index as u32,
				tax: self.node_tax.get(&(index as u32)).copied().unwrap_or(0.0),
				retention,
				production: view.local_value().unwrap_or(0) as f64,
				trade_power: view.power().unwrap_or(0) as f64,
				current_value: retained_value,
				outgoing,
				steering,
			});
		}
		// M2 semantics survey, once, after every name is collected.
		if !self.entry_logged {
			self.entry_logged = true;
			let edges: Vec<String> = self
				.vanilla_outgoing
				.iter()
				.filter(|(_, targets)| !targets.is_empty())
				.map(|(node, targets)| {
					let list: Vec<String> = targets.iter().map(|t| t.to_string()).collect();
					format!("{node}->{}", list.join(","))
				})
				.collect();
			crate::logging::write(
				crate::logging::DEBUG,
				&format!("vanilla edges: {}", edges.join(" ")),
			);
			let mut lines = Vec::new();
			for probe in 0..count {
				let Some(probed) = nodes.node(probe) else {
					continue;
				};
				lines.push(format!(
					"{}: value {:?} local {:?} transferred {:?} power {:?}",
					self.names[probe].1,
					probed.value(),
					probed.local_value(),
					probed.transferred(),
					probed.power(),
				));
			}
			crate::logging::write(
				crate::logging::DEBUG,
				&format!("node field survey:\n  {}", lines.join("\n  ")),
			);
		}
		// Taxes need the province array, which is only safe after the
		// load phase; retry until it lands.
		self.aggregate_taxes(count);
		// One-shot country probe: the first few countries' tag and
		// treasury, to calibrate the treasury field's unit against a
		// known `add_treasury` amount in the e2e probe case.
		if !self.countries_probed {
			self.countries_probed = true;
			self.probe_countries();
		}
		// A periodic summary shows whether a late observe sees a database
		// the first one could not (the update can fire mid-load).
		self.observes += 1;
		if self.observes == 1 || self.observes.is_multiple_of(100) {
			let edges: usize = snapshots.iter().map(|n| n.outgoing.len()).sum();
			let sample: Vec<&str> = self
				.names
				.iter()
				.map(|(_, name)| name.as_str())
				.filter(|name| !name.is_empty())
				.take(3)
				.collect();
			crate::logging::write(
				crate::logging::DEBUG,
				&format!(
					"observe #{}: {} nodes, {} edges, names [{}]",
					self.observes,
					snapshots.len(),
					edges,
					sample.join(", ")
				),
			);
		}
		Ok(snapshots)
	}

	fn node_index(&self, name: &str) -> Option<u32> {
		self.names
			.iter()
			.position(|(_, node)| node.eq_ignore_ascii_case(name))
			.map(|i| i as u32)
	}

	/// The countries holding trade power in one node: every registry
	/// country whose entry power reads above zero.
	fn node_participants(&mut self, node: u32) -> Vec<(u32, f64)> {
		let Some(game) = self.game() else {
			return Vec::new();
		};
		let Some(nodes) = game.trade_nodes() else {
			return Vec::new();
		};
		let Some(countries) = game.countries() else {
			return Vec::new();
		};
		let Some(view) = nodes.node(node as usize) else {
			return Vec::new();
		};
		view.participants(&countries)
			.into_iter()
			.filter(|(_, power)| *power > 0)
			.map(|(index, power)| (index as u32, power as f64))
			.collect()
	}

	/// Add to one country's treasury, mirroring `CCountry::ChangeTreasury`
	/// through [`Eu4Game::pay_country`] (the ±1e9 clamp and the player-UI
	/// dirty bit).
	fn pay_country(&mut self, country: u32, amount: f64) -> Result<(), &'static str> {
		let game = self.game().ok_or("no exe base")?;
		let thousandths = amount.round() as i64;
		// SAFETY: the monthly detour runs on the game thread. `pay_country`
		// resolves the country through this view's registry and writes that
		// country's treasury, plus the UI dirty bit when a controller is
		// present. `from_tick` already matched `context` to the global.
		unsafe { game.pay_country(country as usize, thousandths) }.ok_or("treasury write refused")
	}

	fn set_edge_closed(
		&mut self,
		node: u32,
		edge: usize,
		closed: bool,
	) -> Result<(), &'static str> {
		self.capture(node)?;
		let edges = self
			.mutations
			.get(&node)
			.map(|state| state.edges)
			.ok_or("implausible route vector")?;
		if edge >= edges {
			return Err("edge slot beyond the vanilla set");
		}
		let memory = self.game().ok_or("no exe base")?.memory;
		let state = self
			.mutations
			.get_mut(&node)
			.ok_or("implausible route vector")?;
		// SAFETY: the monthly detour runs on the game thread. `capture`
		// recorded the engine's own route and steering arrays, and
		// `set_closed` writes only inside that captured capacity. No Rust
		// reference aliases those arrays.
		unsafe { state.mutation.set_closed(&memory, edge, closed) }.ok_or("route write refused")
	}
}

#[cfg(test)]
mod tests {
	use super::{MemberSource, member_source_summary, parse_members, resolve_members};

	#[test]
	fn members_parse_into_keys_and_province_ids() {
		let parsed = parse_members(
			b"malacca 593 594 junk 595
siam 592
",
		);
		assert_eq!(
			parsed,
			vec![
				("malacca".to_string(), vec![593, 594, 595]),
				("siam".to_string(), vec![592]),
			]
		);
	}

	#[test]
	fn members_skip_lines_without_any_province_id() {
		// Empty lines, bare keys, and non-numeric fields all drop out.
		assert!(parse_members(b"").is_empty());
		assert!(
			parse_members(
				b"
   
only-a-key"
			)
			.is_empty()
		);
		assert_eq!(
			parse_members(b"cape 12 oh-no 13"),
			vec![("cape".to_string(), vec![12, 13])]
		);
	}

	#[test]
	fn live_member_vector_beats_the_static_table() {
		let static_ids = [9, 10];
		let (source, ids) = resolve_members(Some(&[1, 2]), Some(&static_ids)).unwrap();
		assert_eq!(source, MemberSource::LiveVector);
		assert_eq!(ids, vec![1, 2]);
		let (source, ids) = resolve_members(Some(&[1]), None).unwrap();
		assert_eq!(source, MemberSource::LiveVector);
		assert_eq!(ids, vec![1]);
	}

	#[test]
	fn empty_or_missing_live_vector_falls_back_to_node_members() {
		let static_ids = [12, 13];
		let missing_or_empty: [Option<&[u32]>; 2] = [None, Some(&[])];
		for live in missing_or_empty {
			let (source, ids) = resolve_members(live, Some(&static_ids)).unwrap();
			assert_eq!(source, MemberSource::StaticTable);
			assert_eq!(ids, vec![12, 13]);
		}
		assert_eq!(resolve_members(None, None), None);
		assert_eq!(resolve_members(Some(&[]), Some(&[])), None);
		assert_eq!(resolve_members(Some(&[]), None), None);
	}

	#[test]
	fn member_source_log_names_which_table_won() {
		let text = member_source_summary(2, 3, 1, 1);
		assert!(text.contains("2/3"));
		assert!(text.contains("1 live vector"));
		assert!(text.contains("1 static node_members.txt"));
		assert!(text.contains("pending live verification"));
		assert!(!text.contains("0x"));
	}

	#[cfg(windows)]
	use super::LiveBackend;
	#[cfg(windows)]
	use crate::config::SinkBasis;
	#[cfg(windows)]
	use foch_runtime::dataplane::testing::FakeGame;

	/// Bind a sealed scene. The returned game's buffers stay alive only as
	/// long as `scene` does; callers keep `scene` until the reads finish.
	#[cfg(windows)]
	fn bind(scene: &mut FakeGame, members: Vec<(String, Vec<u32>)>) -> LiveBackend {
		let game = scene.game().expect("synthetic scene");
		let mut backend = LiveBackend::with_members(members, SinkBasis::Mean);
		backend.context = game.context;
		backend.exe_base = game.exe_base;
		backend
	}

	#[cfg(windows)]
	fn scene_with_nodes(count: usize) -> FakeGame {
		let mut scene = FakeGame::new();
		for _ in 0..count {
			scene.add_trade_node(&[], &[]).expect("node");
		}
		scene
	}

	#[test]
	#[cfg(windows)]
	fn node_count_accepts_regional_and_overhaul_maps() {
		// The retired gate was `20..=512`. Shared `Eu4Game::trade_nodes`
		// accepts `1..=MAX_TRADE_NODES`; country counts use `1..=i16::MAX`.
		let ceiling = foch_runtime::dataplane::game::MAX_TRADE_NODES;
		for count in [1usize, 5, 19, 20, 512, 513, 1024, ceiling] {
			let mut scene = scene_with_nodes(count);
			let backend = bind(&mut scene, Vec::new());
			assert_eq!(backend.node_count(), Some(count), "count {count}");
			std::hint::black_box(&scene);
		}

		let mut empty = FakeGame::new();
		let backend = bind(&mut empty, Vec::new());
		assert_eq!(backend.node_count(), None);
		assert_eq!(backend.country_count(), None);
		std::hint::black_box(&empty);

		let mut wide = FakeGame::new();
		wide.add_country("FRA", 1499, 0, None, &[])
			.expect("country");
		let backend = bind(&mut wide, Vec::new());
		assert_eq!(backend.country_count(), Some(1500));
		std::hint::black_box(&wide);

		let mut at_cap = FakeGame::new();
		at_cap
			.add_country("FRA", (i16::MAX as u16) - 1, 0, None, &[])
			.expect("country");
		let backend = bind(&mut at_cap, Vec::new());
		assert_eq!(backend.country_count(), Some(i16::MAX as usize));
		std::hint::black_box(&at_cap);
	}

	/// Write the internal key of node 0's first outgoing route. The scene
	/// must already contain that route, and must stay alive across the write.
	///
	/// `ROUTE_TARGET_KEY` is `route.target_key` on the compiled-in 1.37.5
	/// profile. Production reads that field through
	/// [`NodeView::route_target_key`]; the profile keeps the offset private,
	/// so only this plant names it. The following two stores are the MSVC
	/// inline-string length and capacity, not a trade-route field.
	#[cfg(windows)]
	fn plant_route_key(scene: &mut FakeGame, key: &str) {
		const ROUTE_TARGET_KEY: usize = 0x10;
		const SSO_LENGTH: usize = 0x10;
		const SSO_CAPACITY: usize = 0x18;
		let game = scene.game().expect("scene");
		let nodes = game.trade_nodes().expect("nodes");
		let view = nodes.node(0).expect("node");
		let (begin, end) = nodes.routes(view.definition).expect("routes");
		let count = view.live_route_count(&nodes).expect("route count");
		assert!(count >= 1, "the key lives on an outgoing route");
		let slot = begin;
		let stride = (end - begin) / count;
		assert!(stride > ROUTE_TARGET_KEY);
		let key_at = slot + ROUTE_TARGET_KEY;
		let memory = game.memory;
		// SAFETY: `scene` owns the route allocation for this call, and the
		// test is the only writer. The bytes are the inline SSO
		// `NodeView::route_target_key` reads back.
		unsafe {
			memory
				.write_bytes(key_at, key.as_bytes())
				.expect("key bytes");
			memory
				.write_qword(key_at + SSO_LENGTH, key.len() as u64)
				.expect("key length");
			memory
				.write_qword(key_at + SSO_CAPACITY, 0xF)
				.expect("key capacity");
		}
		assert_eq!(view.route_target_key(slot).as_deref(), Some(key));
	}

	#[cfg(windows)]
	fn tax_scene(members: &[u32], with_route: bool) -> FakeGame {
		let mut scene = FakeGame::new();
		scene.add_province(1, 3_000, 0).expect("province");
		scene.add_province(2, 9_000, 0).expect("province");
		let routes: &[usize] = if with_route { &[0] } else { &[] };
		scene.add_trade_node(routes, members).expect("node");
		scene
	}

	#[test]
	#[cfg(windows)]
	fn aggregate_taxes_prefers_the_live_member_vector() {
		let mut scene = tax_scene(&[1], false);
		let mut backend = bind(&mut scene, vec![("malacca".to_string(), vec![2])]);
		backend.aggregate_taxes(1);
		assert_eq!(backend.node_tax.get(&0).copied(), Some(3_000.0));
		std::hint::black_box(&scene);
	}

	#[test]
	#[cfg(windows)]
	fn aggregate_taxes_falls_back_when_the_live_vector_is_empty() {
		let mut scene = tax_scene(&[], true);
		plant_route_key(&mut scene, "malacca");
		let mut backend = bind(&mut scene, vec![("malacca".to_string(), vec![2])]);
		backend.aggregate_taxes(1);
		assert_eq!(backend.node_tax.get(&0).copied(), Some(9_000.0));

		let mut bare = tax_scene(&[], false);
		let mut backend = bind(&mut bare, Vec::new());
		backend.aggregate_taxes(1);
		assert!(backend.node_tax.is_empty());
		std::hint::black_box((&scene, &bare));
	}
}
