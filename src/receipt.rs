//! Structured receipts for the judge: `FOCH_RECEIPT/1` lines carrying
//! the monthly decisions as compact JSON with string values.
//!
//! The host stamps the envelope (plugin id, run id, timestamps); the
//! payload below repeats none of those keys. One receipt per decision,
//! field names stable so `expect_receipt` blocks in Foch e2e cases can
//! assert exact values.

use serde_json::json;

/// The receipt prefix the judge recognizes.
pub const PREFIX: &str = "FOCH_RECEIPT/1 ";

/// The self-declared case name every receipt carries: these decisions
/// are the plugin's own monthly policy, not answers to a scripted
/// command, so the case is a constant and the request number is the
/// tick that decided.
pub const CASE: &str = "policy_month";

/// One receipt line, ready for the host log.
pub fn line(payload: serde_json::Value) -> String {
	format!("{PREFIX}{payload}")
}

/// An edge flip decision: `close` or `reopen`.
pub fn flip(action: &str, node: u32, target: u32, tick: u32, net_flow: i64, pool: i64) -> String {
	line(json!({
		"type": "edge_flip",
		"case": CASE,
		"request": tick.to_string(),
		"action": action,
		"node": node.to_string(),
		"target": target.to_string(),
		"tick": tick.to_string(),
		"net_flow": net_flow.to_string(),
		"pool": pool.to_string(),
	}))
}

/// A fee distribution: one receipt per node per month, carrying the
/// totals rather than one line per country (those stay in the free-form
/// log).
pub fn fees(kind: &str, node: u32, tick: u32, volume: i64, pool: i64, recipients: usize) -> String {
	line(json!({
		"type": "role_fees",
		"case": CASE,
		"request": tick.to_string(),
		"kind": kind,
		"node": node.to_string(),
		"tick": tick.to_string(),
		"volume": volume.to_string(),
		"pool": pool.to_string(),
		"recipients": recipients.to_string(),
	}))
}

/// The monthly gazette: the entrepot tiers and any crown change.
pub fn gazette(tick: u32, world: u32, previous: Option<u32>, world_score: i64) -> String {
	line(json!({
		"type": "gazette",
		"case": CASE,
		"request": tick.to_string(),
		"tick": tick.to_string(),
		"world_entrepot": world.to_string(),
		"previous_entrepot": previous.map(|p| p.to_string()).unwrap_or_default(),
		"world_score": world_score.to_string(),
	}))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn receipts_carry_string_values_and_a_type() {
		for payload in [
			flip("close", 36, 49, 6, 2, 16971),
			fees("transit fee", 27, 8, 218, 44, 2),
			gazette(14, 33, Some(33), 1836),
		] {
			let body = payload.strip_prefix(PREFIX).unwrap();
			let value: serde_json::Value = serde_json::from_str(body).unwrap();
			let map = value.as_object().unwrap();
			assert!(map.values().all(serde_json::Value::is_string));
			assert!(map.contains_key("type"));
			assert_eq!(map["case"], CASE);
			for key in ["plugin_id", "run_id", "timestamp_ms", "pid"] {
				assert!(!map.contains_key(key), "envelope key {key} leaked");
			}
		}
	}

	#[test]
	fn gazette_marks_a_first_crown_with_an_empty_previous() {
		let receipt = gazette(1, 7, None, 900);
		let body = receipt.strip_prefix(PREFIX).unwrap();
		let value: serde_json::Value = serde_json::from_str(body).unwrap();
		assert_eq!(value["previous_entrepot"], "");
	}
}
