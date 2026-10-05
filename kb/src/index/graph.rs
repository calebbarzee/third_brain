//! pure graph algorithms used by the build: pagerank over the canon `kb:` edge set, and kahn's
//! algorithm to find nodes left over from a topological sort (a cycle touches every leftover node).
use std::collections::{HashMap, HashSet, VecDeque};

/// pagerank over a directed graph given as (node id -> outgoing target ids). isolated nodes (present
/// as keys with no edges, or only appearing as targets) get the uniform base score like any other
/// node. dangling nodes (no outgoing edges) distribute their rank uniformly, as usual.
pub fn pagerank(nodes: &[String], edges: &HashMap<String, Vec<String>>, damping: f64, iterations: u32) -> HashMap<String, f64> {
	let n = nodes.len();
	if n == 0 {
		return HashMap::new();
	}
	let base = 1.0 / n as f64;
	let mut rank: HashMap<String, f64> = nodes.iter().map(|id| (id.clone(), base)).collect();
	let empty: Vec<String> = Vec::new();
	for _ in 0..iterations {
		let dangling_mass: f64 = nodes.iter().filter(|id| edges.get(*id).map(|e| e.is_empty()).unwrap_or(true)).map(|id| rank[id]).sum();
		let mut next: HashMap<String, f64> = nodes.iter().map(|id| (id.clone(), (1.0 - damping) / n as f64 + damping * dangling_mass / n as f64)).collect();
		for id in nodes {
			let out = edges.get(id).unwrap_or(&empty);
			if out.is_empty() {
				continue;
			}
			let share = damping * rank[id] / out.len() as f64;
			for target in out {
				if let Some(slot) = next.get_mut(target) {
					*slot += share;
				}
			}
		}
		rank = next;
	}
	rank
}

/// nodes left over after kahn's algorithm removes every node reachable from a topological order;
/// a non-empty result means the graph has at least one cycle, and every remaining node is part of
/// one (though not every remaining node is necessarily on the same cycle).
pub fn kahn_leftover(nodes: &[String], edges: &[(String, String)]) -> Vec<String> {
	let mut out_edges: HashMap<&str, Vec<&str>> = HashMap::new();
	let mut in_degree: HashMap<&str, u32> = nodes.iter().map(|n| (n.as_str(), 0)).collect();
	for (from, to) in edges {
		if !in_degree.contains_key(from.as_str()) || !in_degree.contains_key(to.as_str()) {
			continue;
		}
		out_edges.entry(from.as_str()).or_default().push(to.as_str());
		*in_degree.get_mut(to.as_str()).unwrap() += 1;
	}
	let mut queue: VecDeque<&str> = in_degree.iter().filter(|(_, d)| **d == 0).map(|(n, _)| *n).collect();
	let mut removed: HashSet<&str> = HashSet::new();
	while let Some(n) = queue.pop_front() {
		removed.insert(n);
		for &next in out_edges.get(n).unwrap_or(&Vec::new()) {
			if let Some(d) = in_degree.get_mut(next) {
				*d -= 1;
				if *d == 0 {
					queue.push_back(next);
				}
			}
		}
	}
	nodes.iter().filter(|n| !removed.contains(n.as_str())).cloned().collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn pagerank_ranks_the_hub_highest() {
		let nodes: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
		let mut edges = HashMap::new();
		edges.insert("a".to_string(), vec!["c".to_string()]);
		edges.insert("b".to_string(), vec!["c".to_string()]);
		edges.insert("c".to_string(), vec![]);
		let rank = pagerank(&nodes, &edges, 0.85, 50);
		assert!(rank["c"] > rank["a"]);
		assert!(rank["c"] > rank["b"]);
		let sum: f64 = rank.values().sum();
		assert!((sum - 1.0).abs() < 1e-6, "ranks should sum to ~1, got {sum}");
	}

	#[test]
	fn kahn_finds_no_leftover_on_a_dag() {
		let nodes: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
		let edges = vec![("a".to_string(), "b".to_string()), ("b".to_string(), "c".to_string())];
		assert!(kahn_leftover(&nodes, &edges).is_empty());
	}

	#[test]
	fn kahn_finds_a_cycle() {
		let nodes: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
		let edges = vec![("a".to_string(), "b".to_string()), ("b".to_string(), "c".to_string()), ("c".to_string(), "a".to_string())];
		let mut leftover = kahn_leftover(&nodes, &edges);
		leftover.sort();
		assert_eq!(leftover, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
	}
}
