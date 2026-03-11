use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Configuration for the Drain3 algorithm.
/// All fields have defaults, so partial JSON like `{"depth": 3}` works.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DrainConfig {
    /// Prefix tree depth (minimum 3).
    pub depth: usize,
    /// Similarity threshold [0.0, 1.0].
    pub sim_th: f64,
    /// Maximum children per tree node.
    pub max_children: usize,
    /// Maximum number of clusters (LRU eviction).
    pub max_clusters: usize,
    /// Wildcard string for parameterized tokens.
    pub param_str: String,
    /// Automatically replace numeric tokens with wildcard.
    pub parametrize_numeric_tokens: bool,
    /// Extra delimiter characters to split on in addition to whitespace (e.g., ["_", "=", "/"]).
    pub extra_delimiters: Vec<String>,
    /// Override the default tokenizer: if set, split ONLY on these delimiters
    /// instead of whitespace. Useful for non-standard log formats.
    /// When empty (default), split on whitespace + extra_delimiters.
    pub delimiters: Vec<String>,
}

impl Default for DrainConfig {
    fn default() -> Self {
        Self {
            depth: 4,
            sim_th: 0.4,
            max_children: 100,
            max_clusters: 1024,
            param_str: "<*>".to_string(),
            parametrize_numeric_tokens: true,
            extra_delimiters: Vec::new(),
            delimiters: Vec::new(),
        }
    }
}

/// A cluster of similar log messages sharing a template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogCluster {
    pub cluster_id: usize,
    pub template_tokens: Vec<String>,
    pub size: usize,
    /// Sample log messages for this cluster (deduped, capped).
    pub examples: Vec<String>,
}

/// Max examples to track per cluster.
pub const MAX_EXAMPLES: usize = 5;

impl LogCluster {
    pub fn get_template(&self) -> String {
        self.template_tokens.join(" ")
    }
}

/// Prefix tree node.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Node {
    pub key_to_child: HashMap<String, Node>,
    pub cluster_ids: Vec<usize>,
}

/// Complete Drain3 state, serializable for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrainState {
    pub config: DrainConfig,
    pub root: Node,
    pub clusters: Vec<LogCluster>,
    pub next_cluster_id: usize,
}
