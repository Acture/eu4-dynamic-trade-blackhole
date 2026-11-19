//! The live engine backend over the reversed trade structures, plus the
//! recalculation entry the detour drives.
//!
//! The hook hands the database pointer to [`monthly`]; the backend walks
//! `CTradeNodeDataBase` exactly as the RE spike documented it (see
//! [`crate::offsets::layout`]). M1 mutation granularity is whole-node:
//! closing swaps the route vector's end back to its begin (an empty vector
//! is the engine's own representation of an end node) and reopening
//! restores the cached original end. Per-edge flips with entry reordering
//! arrive with the policy milestone.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::Config;
use crate::engine::{Engine, EngineBackend};
use crate::policy::NodeSnapshot;
use foch_runtime::dataplane::{self, Memory};

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
	/// `NodeView::member_provinces`: `CTradeNodeDefinition + def::MEMBERS`
	/// (`0x50`). Statically identified for EU4 1.37.5; pending live verification.
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
		"tax aggregated for {covered}/{count} nodes; member source: {live_nodes} live vector (CTradeNodeDefinition+0x{:x}, statically identified, pending live verification), {static_nodes} static node_members.txt",
		dataplane::layout::def::MEMBERS,
	)
}

/// One economy-update step on the game thread. `context` is the hook's
/// first argument and `db` the embedded trade database; both are validated
/// against the context global before the engine touches anything.
pub fn monthly(context: usize, db: usize) {
	let Some(engine) = ENGINE.get() else { return };
	let base = EXE_BASE.load(Ordering::Acquire);
	if base > 0x1_0000 {
		let stored = Memory
			.read_qword(base + dataplane::layout::CONTEXT_GLOBAL_RVA)
			.unwrap_or(0) as usize;
		if stored != context {
			crate::logging::write(
				crate::logging::WARN,
				&format!(
					"context 0x{context:x} is not the global 0x{stored:x}; skipping this update"
				),
			);
			return;
		}
	}
	if let Ok(mut engine) = engine.lock() {
		engine.set_source(context, db);
		engine.tick();
	}
}

/// The per-node mutation state: the vanilla route entries and steering
/// weights captured before the first mutation, plus the live closed flags.
/// Applying a state rewrites the live arrays from the originals with the
/// open entries first and parks the closed ones at the tail, then shrinks
/// the vector's end — so any open subset is a contiguous, legal vector
/// and restoring every flag rebuilds the exact vanilla layout.
struct NodeMutation {
	def: usize,
	routes_begin: usize,
	original_routes: Vec<u8>,
	weights_begin: Option<usize>,
	original_weights: Vec<i32>,
	closed: Vec<bool>,
}

/// The live slot order for vanilla edges with the given closed flags:
/// open vanilla slots first in vanilla order, closed slots parked at the
/// tail. Pure, so the ordering is unit-tested without engine memory.
fn live_order(closed: &[bool]) -> Vec<usize> {
	dataplane::live_order(closed)
}

/// Reads and mutates the engine's trade structures. Every access happens
/// on the game thread inside the recalculation hook, so the database and
/// its nodes are stable for the whole call; reads still guard against
/// implausible addresses instead of trusting the layout blindly.
pub struct LiveBackend {
	db: usize,
	/// The shared data plane every guarded access goes through.
	memory: Memory,
	/// The global game context, kept for the context cross-check.
	context: usize,
	/// Per-node mutation state, captured before the first mutation.
	mutations: HashMap<u32, NodeMutation>,
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
	/// members' CProvince base tax: the per-province mean or the sum.
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
			db: 0,
			memory: Memory,
			context: 0,
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

	/// Sum the members' base tax for every node, once the province array
	/// is available. Member provinces come from the live
	/// `CTradeNodeDefinition + def::MEMBERS` vector (`0x50`,
	/// `std::vector<i32>`, statically identified for EU4 1.37.5 and still
	/// pending a live Foch session) via [`dataplane::NodeView::member_provinces`].
	/// `None` or an empty vector falls back to the offline `node_members.txt`
	/// table, whose keys are read from incoming route entries.
	fn aggregate_taxes(&mut self, count: usize) {
		// Re-aggregate every 6 months alongside the evidence window:
		// province development changes tax and manpower, and the sink
		// signal must track it.
		if !self.node_tax.is_empty() && !self.observes.is_multiple_of(6) {
			return;
		}
		self.node_tax.clear();
		let Some(cprov_base) = self
			.read_qword(self.context + dataplane::layout::province::CPROVINCES)
			.map(|p| p as usize)
			.filter(|p| *p > 0x1_0000)
		else {
			return;
		};
		// Route entries name their target: entry+0x10 holds the target's
		// internal key, entry+0x30 the target definition pointer. The
		// offline table is the only consumer; skip the walk when it is absent.
		let mut key_by_def: HashMap<usize, String> = HashMap::new();
		if !self.member_ids.is_empty() {
			for index in 0..count {
				let Some(node) = self.node_at(index) else {
					continue;
				};
				let Some(def) = self.definition_of(node) else {
					continue;
				};
				let Some((begin, end)) = self.routes(def) else {
					continue;
				};
				for slot in (begin..end).step_by(dataplane::layout::route::SIZE) {
					let Some(target) = self
						.read_qword(slot + dataplane::layout::route::TARGET)
						.map(|p| p as usize)
					else {
						continue;
					};
					if key_by_def.contains_key(&target) {
						continue;
					}
					if let Some(key) = self.read_sso_key(slot + 0x10) {
						key_by_def.insert(target, key);
					}
				}
			}
		}
		// `def::MEMBERS` (`0x50`) is a static identification, pending live
		// verification. An empty or unreadable vector is not a member list:
		// fall back to `node_members.txt` for that node.
		let trade = self.eu4().trade_nodes();
		let mut live_nodes = 0usize;
		let mut static_nodes = 0usize;
		for index in 0..count {
			let live = trade.as_ref().and_then(|nodes| {
				let view = nodes.node(index)?;
				view.member_provinces()
			});
			let static_ids = self.static_member_ids(index, &key_by_def);
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
				let base = cprov_base + *id as usize * dataplane::layout::province::CPROVINCE_SIZE;
				if let Some(tax) = self.read_i32(base + dataplane::layout::province::TAX) {
					sum += tax as f64;
					// Manpower is potential consumption: a large population
					// represents purchasing power that tax alone understates.
					// It joins the sink signal at half weight so tax stays
					// the dominant expression of current wealth.
					if let Some(man) = self.read_i32(base + dataplane::layout::province::MANPOWER) {
						manpower_sum += man as f64;
					}
					counted += 1.0;
				}
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

	/// The offline table row for one node. The key is the definition
	/// pointer an incoming route recorded; missing when that route or
	/// the table row is absent.
	fn static_member_ids(
		&self,
		index: usize,
		key_by_def: &HashMap<usize, String>,
	) -> Option<Vec<u32>> {
		let node = self.node_at(index)?;
		let def = self.definition_of(node)?;
		let key = key_by_def.get(&def)?;
		self.member_ids.get(key).cloned()
	}

	/// Read an MSVC SSO string holding an internal key (`a-z0-9_`).
	fn read_sso_key(&self, addr: usize) -> Option<String> {
		dataplane::read_sso(&self.memory, addr, dataplane::is_key_byte)
	}

	fn read_qword(&self, addr: usize) -> Option<u64> {
		self.memory.read_qword(addr)
	}

	fn read_i32(&self, addr: usize) -> Option<i32> {
		self.memory.read_i32(addr)
	}

	/// One byte, for the SSO-string and route-byte walks: reading four
	/// bytes where one is walked risks touching the page after the
	/// object near a segment boundary.
	fn read_u8(&self, addr: usize) -> Option<u8> {
		self.memory.read_u8(addr)
	}

	/// # Safety
	///
	/// Same contract as [`foch_runtime::dataplane::Memory::write_qword`]:
	/// an engine-owned target no Rust object aliases, on the game thread.
	unsafe fn write_qword(&self, addr: usize, value: u64) -> Option<()> {
		unsafe { self.memory.write_qword(addr, value) }
	}

	/// # Safety
	///
	/// Same contract as [`Memory::write_qword`]: an engine-owned target,
	/// no Rust alias, game thread.
	unsafe fn write_i32(&self, addr: usize, value: i32) -> Option<()> {
		unsafe { self.memory.write_i32(addr, value) }
	}

	/// One-shot diagnostic: a few countries' tag and treasury, so the
	/// e2e probe (a known `add_treasury` effect) can pin the field's unit.
	fn probe_countries(&self) {
		let base = EXE_BASE.load(Ordering::Acquire);
		if base <= 0x1_0000 {
			return;
		}
		let manager_raw = self.read_qword(base + dataplane::layout::COUNTRY_MANAGER_GLOBAL_RVA);
		crate::logging::write(
			crate::logging::DEBUG,
			&format!("country probe: manager global reads {manager_raw:?}"),
		);
		let Some(manager) = manager_raw.map(|p| p as usize).filter(|p| *p > 0x1_0000) else {
			return;
		};
		for (label, offset) in [("count_1d14", 0x1d14usize), ("count_1d10", 0x1d10)] {
			crate::logging::write(
				crate::logging::DEBUG,
				&format!(
					"country probe: {label} reads {:?}",
					self.read_i32(manager + offset)
				),
			);
		}
		let Some(count) = self.country_count() else {
			return;
		};
		let Some(pointers) = self
			.read_qword(manager + dataplane::layout::country_manager::INDEX_ARRAY)
			.map(|p| p as usize)
			.filter(|p| *p > 0x1_0000)
		else {
			return;
		};
		let mut lines = Vec::new();
		for index in 0..count.min(6) {
			let Some(country) = self
				.read_qword(pointers + index * 8)
				.map(|p| p as usize)
				.filter(|p| *p > 0x1_0000)
			else {
				continue;
			};
			let tag = self.read_tag(country);
			let treasury = self.read_i32(country + dataplane::layout::country::TREASURY);
			lines.push(format!("country {index} tag {tag} treasury {treasury:?}"));
		}
		crate::logging::write(
			crate::logging::DEBUG,
			&format!("country probe ({} countries): {}", count, lines.join("; ")),
		);
	}

	/// The tag embedded in a country's identity qword at +0x20: the low
	/// bytes as ASCII, up to the first non-tag character.
	fn read_tag(&self, country: usize) -> String {
		dataplane::CountryView::tag_at(&self.memory, country)
	}

	/// The number of countries, from the country vector in the game
	/// context: the same count [`dataplane::Eu4Game::countries`] accepts
	/// (`1..=i16::MAX`).
	fn country_count(&self) -> Option<usize> {
		if self.context == 0 {
			return None;
		}
		let raw = self.read_i32(self.context + dataplane::layout::COUNTRIES_COUNT)?;
		let count = usize::try_from(raw).ok()?;
		(1..=i16::MAX as usize).contains(&count).then_some(count)
	}

	fn eu4(&self) -> dataplane::Eu4Game {
		dataplane::Eu4Game {
			memory: self.memory,
			exe_base: EXE_BASE.load(Ordering::Acquire),
			context: self.context,
		}
	}

	/// # Safety
	///
	/// Same contract as [`Memory::write_qword`]: an engine-owned target,
	/// no Rust alias, game thread.
	unsafe fn write_bytes(&self, addr: usize, bytes: &[u8]) -> Option<()> {
		unsafe { self.memory.write_bytes(addr, bytes) }
	}

	/// Capture a node's vanilla routes and weights before the first
	/// mutation so every later state is a rewrite of the originals.
	fn capture(&mut self, node: u32) -> Result<(), &'static str> {
		if self.mutations.contains_key(&node) {
			return Ok(());
		}
		let address = self
			.node_at(node as usize)
			.ok_or("implausible node pointer")?;
		let def = self
			.definition_of(address)
			.ok_or("implausible definition")?;
		let (begin, end) = self.routes(def).ok_or("implausible route vector")?;
		let count = (end - begin) / dataplane::layout::route::SIZE;
		let mut original_routes = Vec::with_capacity(count * dataplane::layout::route::SIZE);
		for offset in 0..count * dataplane::layout::route::SIZE {
			let Some(byte) = self.read_i32(begin + offset) else {
				return Err("route bytes unreadable");
			};
			original_routes.push(byte as u8);
		}
		// The steering weights pair with the routes entry for entry; when
		// they do not, route reordering alone still holds and the weights
		// keep their live order (the pairing diagnostic covers the case).
		let weights_begin = self
			.read_qword(address + dataplane::layout::node::WEIGHTS)
			.map(|p| p as usize);
		let weights_end = self
			.read_qword(address + dataplane::layout::node::WEIGHTS + 8)
			.map(|p| p as usize);
		let paired = matches!((weights_begin, weights_end), (Some(a), Some(e)) if e >= a && (e - a) / 4 == count);
		let (weights_begin, original_weights) = if paired {
			let Some(begin) = weights_begin else {
				unreachable!("paired implies begin")
			};
			let mut values = Vec::with_capacity(count);
			for slot in 0..count {
				match self.read_i32(begin + slot * 4) {
					Some(value) => values.push(value),
					None => return Err("weight bytes unreadable"),
				}
			}
			(Some(begin), values)
		} else {
			(None, Vec::new())
		};
		self.mutations.insert(
			node,
			NodeMutation {
				def,
				routes_begin: begin,
				original_routes,
				weights_begin,
				original_weights,
				closed: vec![false; count],
			},
		);
		Ok(())
	}

	/// Rewrite a node's live route and weight arrays from the captured
	/// originals: open entries first, closed entries parked at the tail,
	/// and the vector end shrunk to the open prefix.
	fn apply(&self, node: u32) -> Result<(), &'static str> {
		let Some(state) = self.mutations.get(&node) else {
			return Ok(());
		};
		let stride = dataplane::layout::route::SIZE;
		let order = live_order(&state.closed);
		// SAFETY: every target below was captured from the engine's own
		// route and steering vectors before the first mutation (never
		// caller-supplied), the writes stay inside the captured original
		// capacity, and the whole detour runs on the game thread inside
		// the monthly hook where the route tables are stable.
		unsafe {
			for (live_slot, vanilla_slot) in order.iter().enumerate() {
				let source = vanilla_slot * stride;
				let target = state.routes_begin + live_slot * stride;
				let bytes = &state.original_routes[source..source + stride];
				self.write_bytes(target, bytes)
					.ok_or("route write refused")?;
				if let (Some(weights_begin), value) = (
					state.weights_begin,
					state.original_weights.get(*vanilla_slot).copied(),
				) {
					let Some(value) = value else { continue };
					self.memory
						.write_i32(weights_begin + live_slot * 4, value)
						.ok_or("weight write refused")?;
				}
			}
		}
		// The paired steering-weight vector's end is left untouched: the
		// engine walks the weights by the route count (the pairing
		// diagnostic has never tripped in a live session), and the
		// parked tail entries keep the vector's own length stable for
		// the observe-side permutation recovery.
		let open_count = state.closed.iter().filter(|c| !**c).count();
		let new_end = state.routes_begin + open_count * stride;
		// SAFETY: the route vector's own end field on the same captured
		// engine definition, same thread contract as the loop above.
		unsafe {
			self.write_qword(
				state.def + dataplane::layout::def::ROUTES_END,
				new_end as u64,
			)
		}
		.ok_or("end write refused")?;
		Ok(())
	}

	/// Trade-node count through [`dataplane::Eu4Game::trade_nodes`].
	///
	/// The shared gate is `1..=MAX_TRADE_NODES` (4096). Counts below 20
	/// (small regional maps) and above 512 (total conversions) stay valid;
	/// only zero, negative, and clearly corrupt counts are rejected.
	fn node_count(&self) -> Option<usize> {
		let embedded = self.context.checked_add(dataplane::layout::DB_IN_CONTEXT)?;
		if self.db != embedded {
			return None;
		}
		let count = self.eu4().trade_nodes()?.count();
		(count <= dataplane::game::MAX_TRADE_NODES).then_some(count)
	}

	fn nodes_base(&self) -> Option<usize> {
		let base = self.read_qword(self.db + dataplane::layout::db::NODES)? as usize;
		(base > 0x1_0000 && base & 7 == 0).then_some(base)
	}

	/// The node pointer array dereference: element `index` is a heap
	/// `CTradeNode*`, loaded through one indirection.
	fn node_at(&self, index: usize) -> Option<usize> {
		let slot = self.nodes_base()? + index * 8;
		let node = self.read_qword(slot)? as usize;
		(node > 0x1_0000 && node & 7 == 0).then_some(node)
	}

	fn definition_of(&self, node: usize) -> Option<usize> {
		let def = self.read_qword(node + dataplane::layout::node::DEFINITION)? as usize;
		(def > 0x1_0000 && def & 7 == 0).then_some(def)
	}

	/// MSVC SSO `std::string`: inline buffer unless capacity exceeds 0xF.
	fn read_name(&self, def: usize) -> Option<String> {
		let capacity = self.read_qword(def + dataplane::layout::def::NAME + 0x18)?;
		let length = self.read_qword(def + dataplane::layout::def::NAME + 0x10)? as usize;
		if length == 0 || length > 64 {
			return None;
		}
		let data = if capacity > 0xF {
			self.read_qword(def + dataplane::layout::def::NAME)? as usize
		} else {
			def + dataplane::layout::def::NAME
		};
		let mut bytes = Vec::with_capacity(length);
		for i in 0..length {
			let byte = self.read_u8(data + i)?;
			if !(byte == b' ' || byte.is_ascii_graphic() || byte >= 0x80) {
				return None;
			}
			bytes.push(byte);
		}
		Some(String::from_utf8_lossy(&bytes).into_owned())
	}

	/// The route vector. An empty vector is legal and common: vanilla end
	/// nodes have no outgoing routes at all (`begin == end`, both null in
	/// a default-constructed MSVC vector).
	fn routes(&self, def: usize) -> Option<(usize, usize)> {
		let begin = self.read_qword(def + dataplane::layout::def::ROUTES)? as usize;
		let end = self.read_qword(def + dataplane::layout::def::ROUTES_END)? as usize;
		if begin == end {
			return Some((begin, end));
		}
		if begin == 0
			|| end < begin
			|| !(end - begin).is_multiple_of(dataplane::layout::route::SIZE)
		{
			return None;
		}
		Some((begin, end))
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

	fn set_source(&mut self, context: usize, db: usize) {
		self.context = context;
		self.db = db;
	}

	fn observe(&mut self) -> Result<Vec<NodeSnapshot>, &'static str> {
		if self.db == 0 {
			return Err("no database yet");
		}
		let count = self.node_count().ok_or("implausible node count")?;
		self.nodes_base().ok_or("implausible node array")?;
		for index in 0..count {
			let node = self.node_at(index).ok_or("implausible node pointer")?;
			self.definition_of(node)
				.ok_or("implausible definition pointer")?;
		}
		// Definition pointer to engine index: a route entry's target is
		// the destination node's *definition* pointer (the live dump
		// showed entry+0x30 inside the definition heap range).
		let mut index_by_def: HashMap<usize, u32> = HashMap::with_capacity(count);
		for index in 0..count {
			let node = self.node_at(index).expect("validated above");
			let def = self.definition_of(node).expect("validated above");
			index_by_def.insert(def, index as u32);
		}
		self.names.clear();
		// The vanilla outgoing edges, captured at the first observe (which
		// always precedes any mutation) and reported from the cache after
		// that: mutations reorder and shrink the live vector, so the live
		// order cannot serve as a stable vanilla-slot index for the policy
		// mirror. The live vector is still counted below as a diagnostic.
		if self.vanilla_outgoing.is_empty() {
			for index in 0..count {
				let node = self.node_at(index).expect("validated above");
				let def = self.definition_of(node).expect("validated above");
				let Some((begin, end)) = self.routes(def) else {
					return Err("implausible route vector");
				};
				let outgoing: Vec<u32> = (begin..end)
					.step_by(dataplane::layout::route::SIZE)
					.filter_map(|slot| {
						let target =
							self.read_qword(slot + dataplane::layout::route::TARGET)? as usize;
						index_by_def.get(&target).copied()
					})
					.collect();
				self.vanilla_outgoing.insert(index as u32, outgoing);
			}
		}
		let mut nodes = Vec::with_capacity(count);
		for index in 0..count {
			let node = self.node_at(index).expect("validated above");
			let def = self.definition_of(node).expect("validated above");
			self.names
				.push((def, self.read_name(def).unwrap_or_default()));
			let outgoing = self
				.vanilla_outgoing
				.get(&(index as u32))
				.cloned()
				.unwrap_or_default();
			let Some((begin, end)) = self.routes(def) else {
				return Err("implausible route vector");
			};
			let route_count = (end - begin) / dataplane::layout::route::SIZE;
			// After mutations the live vector holds exactly the open
			// entries; a drift from the plugin's own closed flags would
			// mean an external writer touched the topology.
			if let Some(state) = self.mutations.get(&(index as u32)) {
				let expected = state.closed.iter().filter(|c| !**c).count();
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
			let weights = self
				.read_qword(node + dataplane::layout::node::WEIGHTS)
				.ok_or("implausible weights vector")? as usize;
			let weights_end = self
				.read_qword(node + dataplane::layout::node::WEIGHTS + 8)
				.ok_or("implausible weights vector")? as usize;
			if !self.pairing_logged
				&& (weights_end < weights || (weights_end - weights) / 4 != route_count)
			{
				self.pairing_logged = true;
				crate::logging::write(
					crate::logging::DEBUG,
					&format!(
						"weights pairing differs on node {index}: {} weight slots vs {route_count} routes",
						weights_end.saturating_sub(weights) / 4
					),
				);
			}
			// The index fields are spike hypotheses, so the first observe
			// logs them as diagnostics instead of trusting them; the node
			// names are the real proof the definitions were reversed right.
			if !self.fields_logged {
				self.fields_logged = true;
				for probe in 0..count.min(3) {
					let node = self.node_at(probe).expect("validated above");
					let def = self.definition_of(node).expect("validated above");
					let routes_begin = self.read_qword(def + dataplane::layout::def::ROUTES);
					let routes_end = self.read_qword(def + dataplane::layout::def::ROUTES_END);
					let routes_cap = self.read_qword(def + dataplane::layout::def::ROUTES_END + 8);
					let name_bytes = (0..16)
						.filter_map(|i| self.read_i32(def + dataplane::layout::def::NAME + i))
						.map(|v| v as u8)
						.take_while(|b| *b != 0)
						.collect::<Vec<u8>>();
					let name = String::from_utf8_lossy(&name_bytes);
					crate::logging::write(
						crate::logging::DEBUG,
						&format!(
							"node {probe}: ptr 0x{node:x}, def 0x{def:x}, routes {routes_begin:?}..{routes_end:?} cap {routes_cap:?}, inline name {name:?}"
						),
					);
				}
			}
			// Engine-cached aggregates, all in thousandths: goods produced
			// as the production-side wealth input, total trade power, and
			// the current value. A per-node tax aggregate does not exist
			// in the trade node object; tax stays zero until the province
			// walk lands (see the README's known limits).
			// Steering: the engine's per-edge effort vector, read from
			// the live node (merchants, investments, light ships).
			// Steering, restored to vanilla edge order: after a mutation
			// the live slots hold the open-first permutation, so slot i
			// carries vanilla edge live_order(closed)[i]. Invert the
			// permutation before pairing the weights with the vanilla
			// outgoing list.
			let mut steering = vec![0.0_f64; outgoing.len()];
			if let Some(begin) = self
				.read_qword(node + dataplane::layout::node::WEIGHTS)
				.map(|p| p as usize)
			{
				let end = self
					.read_qword(node + dataplane::layout::node::WEIGHTS + 8)
					.map(|p| p as usize)
					.unwrap_or(begin);
				let live_count = end.saturating_sub(begin) / 4;
				let order = self
					.mutations
					.get(&(index as u32))
					.map(|state| live_order(&state.closed))
					.unwrap_or_else(|| (0..outgoing.len()).collect::<Vec<_>>());
				for (slot, &vanilla_slot) in order.iter().enumerate() {
					if slot >= live_count || vanilla_slot >= steering.len() {
						continue;
					}
					if let Some(value) = self.read_i32(begin + slot * 4) {
						steering[vanilla_slot] = value as f64;
					}
				}
			}
			// The engine's own retention rate: retained / (retained +
			// transferred), from the live node after the monthly update.
			// This is what the game's mechanics (trade power shares,
			// merchant modes) actually compute — no artificial formula.
			let retained_value = self
				.read_i32(node + dataplane::layout::node::VALUE)
				.unwrap_or(0) as f64;
			let transferred = self
				.read_i32(node + dataplane::layout::node::TRANSFERRED)
				.unwrap_or(0) as f64;
			let retention = if retained_value + transferred > 0.0 {
				retained_value / (retained_value + transferred)
			} else {
				0.0
			};
			nodes.push(NodeSnapshot {
				id: index as u32,
				tax: self.node_tax.get(&(index as u32)).copied().unwrap_or(0.0),
				retention,
				production: self
					.read_i32(node + dataplane::layout::node::LOCAL_VALUE)
					.unwrap_or(0) as f64,
				trade_power: self
					.read_i32(node + dataplane::layout::node::POWER)
					.unwrap_or(0) as f64,
				current_value: retained_value,
				outgoing,
				steering,
			});
		}
		// M2 semantics survey, once, after every name is collected:
		// per-node engine-cached candidates across a dozen nodes, to
		// tell trade power from tax-like aggregates by correlation.
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
				let Some(probe_node) = self.node_at(probe) else {
					continue;
				};
				let value = self.read_i32(probe_node + dataplane::layout::node::VALUE);
				let c8 = self.read_i32(probe_node + 0xc8);
				let d0 = self.read_i32(probe_node + 0xd0);
				let d8 = self.read_i32(probe_node + 0xd8);
				let e4 = self.read_i32(probe_node + 0xe4);
				lines.push(format!(
					"{}: value {value:?} c8 {c8:?} d0 {d0:?} d8 {d8:?} e4 {e4:?}",
					self.names[probe].1
				));
			}
			crate::logging::write(
				crate::logging::DEBUG,
				&format!("node field survey:\n  {}", lines.join("\n  ")),
			);
		}
		// Province-memory survey, deferred past the load phase (the first
		// observe fires during Late History, when the province pointer
		// table is not yet safe to walk). Sample entries of the province
		// table shared by all nodes (node+0x18, 0xc0 stride): each entry
		// carries its province id (entry+0x10 high half) and, by the
		// spike's reading, its node index at entry+0xa8. The first
		// 0x400 bytes of each province object are dumped as hex words to
		// correlate against the history files' base_tax offline.
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
			let edges: usize = nodes.iter().map(|n| n.outgoing.len()).sum();
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
					nodes.len(),
					edges,
					sample.join(", ")
				),
			);
		}
		Ok(nodes)
	}

	fn node_index(&self, name: &str) -> Option<u32> {
		self.names
			.iter()
			.position(|(_, node)| node.eq_ignore_ascii_case(name))
			.map(|i| i as u32)
	}

	/// The countries holding trade power in one node: every country
	/// index whose entry in the node's per-country table carries power.
	fn node_participants(&mut self, node: u32) -> Vec<(u32, f64)> {
		let Some(node_ptr) = self.node_at(node as usize) else {
			return Vec::new();
		};
		let Some(table) = self
			.read_qword(node_ptr + dataplane::layout::node::ENTRY_TABLE)
			.map(|p| p as usize)
			.filter(|p| *p > 0x1_0000)
		else {
			return Vec::new();
		};
		let count = self.country_count().unwrap_or(0);
		if count == 0 {
			return Vec::new();
		}
		let mut participants = Vec::new();
		for index in 0..count {
			let entry = table + index * dataplane::layout::node::ENTRY_SIZE;
			match self.read_i32(entry + dataplane::layout::entry::POWER) {
				Some(power) if power > 0 => {
					participants.push((index as u32, power as f64));
				}
				_ => {}
			}
		}
		participants
	}

	/// Add to one country's treasury, mirroring `CCountry::ChangeTreasury`:
	/// the field add with the ±1e9 clamp and the player-UI dirty bit.
	fn pay_country(&mut self, country: u32, amount: f64) -> Result<(), &'static str> {
		let base = EXE_BASE.load(Ordering::Acquire);
		if base <= 0x1_0000 {
			return Err("no exe base");
		}
		let manager = self
			.read_qword(base + dataplane::layout::COUNTRY_MANAGER_GLOBAL_RVA)
			.map(|p| p as usize)
			.filter(|p| *p > 0x1_0000)
			.ok_or("implausible country manager")?;
		let pointers = self
			.read_qword(manager + dataplane::layout::country_manager::INDEX_ARRAY)
			.map(|p| p as usize)
			.filter(|p| *p > 0x1_0000)
			.ok_or("implausible country array")?;
		let country_ptr = self
			.read_qword(pointers + country as usize * 8)
			.map(|p| p as usize)
			.filter(|p| *p > 0x1_0000)
			.ok_or("implausible country pointer")?;
		let treasury = self
			.read_i32(country_ptr + dataplane::layout::country::TREASURY)
			.ok_or("treasury unreadable")? as i64;
		// The amounts arrive as engine thousandths; the treasury field's
		// own unit is the same thousandths-of-a-ducat scale the trade
		// values use, so the add is direct.
		let updated = (treasury + amount.round() as i64).clamp(
			-dataplane::layout::country::TREASURY_CLAMP,
			dataplane::layout::country::TREASURY_CLAMP,
		);
		// SAFETY: the target is the engine's treasury field reached
		// through the validated country registry — no Rust object
		// aliases it — and this runs on the game thread.
		unsafe {
			self.write_i32(
				country_ptr + dataplane::layout::country::TREASURY,
				updated as i32,
			)
		}
		.ok_or("treasury write refused")?;
		// Refresh the player's treasury label, as ChangeTreasury does.
		if let Some(controller) = self
			.read_qword(self.context + dataplane::layout::ui::CONTROLLER)
			.map(|p| p as usize)
			.filter(|p| *p > 0x1_0000)
			&& let Some(flags) = self.read_i32(controller + dataplane::layout::ui::FLAGS)
		{
			// SAFETY: the UI controller's flag word, same provenance and
			// thread contract as the treasury write above.
			let _ = unsafe {
				self.write_i32(
					controller + dataplane::layout::ui::FLAGS,
					flags | dataplane::layout::ui::TREASURY_DIRTY,
				)
			};
		}
		Ok(())
	}

	fn set_edge_closed(
		&mut self,
		node: u32,
		edge: usize,
		closed: bool,
	) -> Result<(), &'static str> {
		self.capture(node)?;
		let Some(state) = self.mutations.get_mut(&node) else {
			return Ok(());
		};
		if edge >= state.closed.len() {
			return Err("edge slot beyond the vanilla set");
		}
		state.closed[edge] = closed;
		self.apply(node)
	}
}

#[cfg(test)]
mod tests {
	#[cfg(windows)]
	use super::LiveBackend;
	use super::{MemberSource, member_source_summary, parse_members, resolve_members};
	#[cfg(windows)]
	use crate::config::SinkBasis;
	#[cfg(windows)]
	use foch_runtime::dataplane;

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
	fn member_source_log_names_the_static_offset_and_which_table_won() {
		let text = member_source_summary(2, 3, 1, 1);
		assert!(text.contains("2/3"));
		assert!(text.contains("1 live vector"));
		assert!(text.contains("1 static node_members.txt"));
		assert!(text.contains("0x50"));
		assert!(text.contains("pending live verification"));
	}

	/// A context-sized buffer whose embedded trade database reports `count`.
	/// The node-array pointer aims at the buffer itself: `trade_nodes` only
	/// requires it to be a plausible aligned address.
	#[cfg(windows)]
	struct CountFixture {
		context: Vec<u64>,
	}

	#[cfg(windows)]
	impl CountFixture {
		fn nodes(count: i32) -> Self {
			Self::write_count(
				dataplane::layout::DB_IN_CONTEXT + dataplane::layout::db::NODE_COUNT,
				count,
				true,
			)
		}

		fn countries(count: i32) -> Self {
			Self::write_count(dataplane::layout::COUNTRIES_COUNT, count, false)
		}

		fn write_count(offset: usize, count: i32, point_nodes_at_self: bool) -> Self {
			let bytes = offset + 4;
			let mut context = vec![0_u64; bytes.div_ceil(8)];
			let base = context.as_mut_ptr() as usize;
			unsafe {
				if point_nodes_at_self {
					let nodes =
						base + dataplane::layout::DB_IN_CONTEXT + dataplane::layout::db::NODES;
					std::ptr::write_unaligned(nodes as *mut u64, base as u64);
				}
				std::ptr::write_unaligned((base + offset) as *mut i32, count);
			}
			Self { context }
		}

		fn context(&self) -> usize {
			self.context.as_ptr() as usize
		}
	}

	#[cfg(windows)]
	fn backend_on(fixture: &CountFixture) -> LiveBackend {
		let mut backend = LiveBackend::new();
		backend.context = fixture.context();
		backend.db = fixture.context() + dataplane::layout::DB_IN_CONTEXT;
		backend
	}

	#[test]
	#[cfg(windows)]
	fn node_count_accepts_regional_and_overhaul_maps() {
		// The retired gate was `20..=512`. Shared `Eu4Game::trade_nodes`
		// accepts `1..=MAX_TRADE_NODES`; country counts use `1..=i16::MAX`.
		let ceiling = dataplane::game::MAX_TRADE_NODES;
		for count in [1, 5, 19, 20, 512, 513, 1024, ceiling as i32] {
			let fixture = CountFixture::nodes(count);
			assert_eq!(
				backend_on(&fixture).node_count(),
				Some(count as usize),
				"count {count}"
			);
		}
		for count in [0, -1, ceiling as i32 + 1] {
			let fixture = CountFixture::nodes(count);
			assert_eq!(backend_on(&fixture).node_count(), None, "count {count}");
		}

		let wide = CountFixture::countries(1500);
		assert_eq!(backend_on(&wide).country_count(), Some(1500));
		let at_cap = CountFixture::countries(i16::MAX as i32);
		assert_eq!(backend_on(&at_cap).country_count(), Some(i16::MAX as usize));
		let over = CountFixture::countries(i16::MAX as i32 + 1);
		assert_eq!(backend_on(&over).country_count(), None);
		assert_eq!(
			backend_on(&CountFixture::countries(0)).country_count(),
			None
		);
	}

	/// One trade node, a province array, and an optional live member vector.
	/// Province 1's tax differs from province 2's so a test can see which
	/// list `aggregate_taxes` actually summed.
	#[cfg(windows)]
	struct TaxMap {
		context: Vec<u64>,
		nodes: Vec<u64>,
		node: Vec<u64>,
		definition: Vec<u64>,
		members: Vec<i32>,
		route: Vec<u64>,
		provinces: Vec<u8>,
	}

	#[cfg(windows)]
	enum LiveMembers {
		Ids(&'static [i32]),
		Empty,
		/// Header fails `StdVector` validation, so `member_provinces` is `None`.
		Corrupt,
	}

	#[cfg(windows)]
	impl TaxMap {
		fn build(live: LiveMembers) -> Self {
			let mut context =
				vec![
					0_u64;
					(dataplane::layout::DB_IN_CONTEXT + dataplane::layout::db::NODE_COUNT + 4)
						.div_ceil(8)
				];
			let mut nodes = vec![0_u64; 1];
			let mut node = vec![0_u64; (dataplane::layout::node::DEFINITION + 8).div_ceil(8)];
			let mut definition = vec![0_u64; (dataplane::layout::def::ROUTES_END + 8).div_ceil(8)];
			let members = match live {
				LiveMembers::Ids(ids) => ids.to_vec(),
				LiveMembers::Empty | LiveMembers::Corrupt => vec![1_i32],
			};
			let mut route = vec![0_u64; dataplane::layout::route::SIZE.div_ceil(8)];
			let stride = dataplane::layout::province::CPROVINCE_SIZE;
			let mut provinces = vec![0_u8; stride * 3];

			let context_addr = context.as_mut_ptr() as usize;
			let nodes_addr = nodes.as_mut_ptr() as usize;
			let node_addr = node.as_mut_ptr() as usize;
			let def_addr = definition.as_mut_ptr() as usize;
			let members_addr = members.as_ptr() as usize;
			let route_addr = route.as_mut_ptr() as usize;
			let prov_addr = provinces.as_mut_ptr() as usize;

			unsafe {
				std::ptr::write_unaligned(
					(context_addr + dataplane::layout::province::CPROVINCES) as *mut u64,
					prov_addr as u64,
				);
				std::ptr::write_unaligned(
					(context_addr + dataplane::layout::DB_IN_CONTEXT + dataplane::layout::db::NODES)
						as *mut u64,
					nodes_addr as u64,
				);
				std::ptr::write_unaligned(
					(context_addr
						+ dataplane::layout::DB_IN_CONTEXT
						+ dataplane::layout::db::NODE_COUNT) as *mut i32,
					1,
				);
				std::ptr::write_unaligned(nodes_addr as *mut u64, node_addr as u64);
				std::ptr::write_unaligned(
					(node_addr + dataplane::layout::node::DEFINITION) as *mut u64,
					def_addr as u64,
				);

				let (begin, end, cap) = match live {
					LiveMembers::Ids(ids) => {
						let end = members_addr + ids.len() * 4;
						(members_addr, end, end)
					}
					LiveMembers::Empty => (0, 0, 0),
					LiveMembers::Corrupt => {
						(members_addr, members_addr.wrapping_sub(4), members_addr)
					}
				};
				let header = def_addr + dataplane::layout::def::MEMBERS;
				std::ptr::write_unaligned(header as *mut u64, begin as u64);
				std::ptr::write_unaligned((header + 8) as *mut u64, end as u64);
				std::ptr::write_unaligned((header + 16) as *mut u64, cap as u64);

				std::ptr::write_unaligned(
					(def_addr + dataplane::layout::def::ROUTES) as *mut u64,
					route_addr as u64,
				);
				std::ptr::write_unaligned(
					(def_addr + dataplane::layout::def::ROUTES_END) as *mut u64,
					(route_addr + dataplane::layout::route::SIZE) as u64,
				);
				let key = b"malacca";
				std::ptr::copy_nonoverlapping(
					key.as_ptr(),
					(route_addr + 0x10) as *mut u8,
					key.len(),
				);
				std::ptr::write_unaligned((route_addr + 0x20) as *mut u64, key.len() as u64);
				std::ptr::write_unaligned((route_addr + 0x28) as *mut u64, 0xF);
				std::ptr::write_unaligned(
					(route_addr + dataplane::layout::route::TARGET) as *mut u64,
					def_addr as u64,
				);

				// Province 1 is the live list; province 2 is the static table.
				std::ptr::write_unaligned(
					(prov_addr + stride + dataplane::layout::province::TAX) as *mut i32,
					3_000,
				);
				std::ptr::write_unaligned(
					(prov_addr + stride * 2 + dataplane::layout::province::TAX) as *mut i32,
					9_000,
				);
			}

			Self {
				context,
				nodes,
				node,
				definition,
				members,
				route,
				provinces,
			}
		}

		fn aggregate(&self, static_ids: Vec<u32>) -> LiveBackend {
			// The buffers are only reached through copied addresses. Keep
			// those addresses observable so the copies stay live.
			std::hint::black_box((
				self.context.as_ptr(),
				self.nodes.as_ptr(),
				self.node.as_ptr(),
				self.definition.as_ptr(),
				self.members.as_ptr(),
				self.route.as_ptr(),
				self.provinces.as_ptr(),
			));
			let mut backend = LiveBackend::with_members(
				if static_ids.is_empty() {
					Vec::new()
				} else {
					vec![("malacca".to_string(), static_ids)]
				},
				SinkBasis::Mean,
			);
			backend.context = self.context.as_ptr() as usize;
			backend.db = backend.context + dataplane::layout::DB_IN_CONTEXT;
			// `self` owns every buffer the reads walk; it stays alive for
			// this call, which is the whole window `ReadProcessMemory` needs.
			backend.aggregate_taxes(1);
			backend
		}
	}

	#[test]
	#[cfg(windows)]
	fn aggregate_taxes_prefers_the_live_member_vector() {
		let map = TaxMap::build(LiveMembers::Ids(&[1]));
		let backend = map.aggregate(vec![2]);
		assert_eq!(backend.node_tax.get(&0).copied(), Some(3_000.0));
	}

	#[test]
	#[cfg(windows)]
	fn aggregate_taxes_falls_back_when_the_live_vector_is_empty_or_invalid() {
		for live in [LiveMembers::Empty, LiveMembers::Corrupt] {
			let map = TaxMap::build(live);
			let backend = map.aggregate(vec![2]);
			assert_eq!(backend.node_tax.get(&0).copied(), Some(9_000.0));
		}
		let map = TaxMap::build(LiveMembers::Empty);
		let backend = map.aggregate(Vec::new());
		assert!(backend.node_tax.is_empty());
	}
}
