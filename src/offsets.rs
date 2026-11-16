//! Pinned game build and the hook's pattern signature.
//!
//! The structure layout lives in `foch_runtime::dataplane::layout`;
//! what stays here is the digest gate and the economy-update signature
//! this plugin hooks. The RE spike (see docs/internals.md) reversed EU4
//! 1.37.5 Steam x64 with Ghidra 12.1.3.

/// The Steam Windows x64 `eu4.exe` build this plugin was reversed against:
/// EU4 1.37.5, sha256 `c9ec55a4...`.
pub const EXPECTED_EXE_SHA256: &str =
	"c9ec55a4783fc0f250bbf536dfd25c8ec190ed0ed621384ca501908c60ae9f8e";

/// The engine layout the spike verified for that build. Every value is a
/// `find_pattern` signatures for one pinned build. A signature matches
/// exactly one address in the executable sections or resolution fails.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sigs {
	/// The economy update whose tail recalculates every trade node. Its
	/// first argument is the global game context, which both identifies
	/// the hook (cross-checked against [`layout::CONTEXT_GLOBAL_RVA`]) and
	/// embeds the trade database at [`layout::DB_IN_CONTEXT`].
	pub tick: &'static str,
}

impl Sigs {
	/// The verified signatures for [`EXPECTED_EXE_SHA256`], or `None` for
	/// any other build.
	pub fn for_build(exe_sha256: &str) -> Option<Self> {
		(exe_sha256 == EXPECTED_EXE_SHA256).then(Self::pinned)
	}

	/// The signature table for the reversed build.
	pub const fn pinned() -> Self {
		Self {
			tick: "88 54 24 10 55 53 56 57 41 54 41 55 41 56 41 57 48 8D AC 24 28 F1 FF FF 48 81 EC D8 0F 00 00 4C 8B F9 BA FF FF FF FF",
		}
	}

	pub fn is_pending(&self) -> bool {
		self.tick.is_empty()
	}
}
