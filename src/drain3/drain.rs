use crate::drain3::types::*;

impl DrainState {
    pub fn new(config: DrainConfig) -> Self {
        Self {
            config,
            root: Node::default(),
            clusters: Vec::new(),
            next_cluster_id: 1,
        }
    }

    /// Train: process a log message, return the matched cluster_id.
    pub fn add_log_message(&mut self, content: &str) -> usize {
        let tokens = self.tokenize(content);
        if tokens.is_empty() {
            return 0;
        }

        // Tree search for candidate clusters
        let match_result = self.tree_search_match(&tokens);

        if let Some((cluster_idx, _sim)) = match_result {
            // Update existing cluster template
            let param_str = self.config.param_str.clone();
            let new_template =
                create_template(&self.clusters[cluster_idx].template_tokens, &tokens, &param_str);
            self.clusters[cluster_idx].template_tokens = new_template;
            self.clusters[cluster_idx].size += 1;
            // Track example (deduped, capped)
            let cluster = &mut self.clusters[cluster_idx];
            if cluster.examples.len() < MAX_EXAMPLES && !cluster.examples.contains(&content.to_string()) {
                cluster.examples.push(content.to_string());
            }
            cluster.cluster_id
        } else {
            // Create new cluster
            let cluster_id = self.next_cluster_id;
            self.next_cluster_id += 1;

            let cluster = LogCluster {
                cluster_id,
                template_tokens: tokens.clone(),
                size: 1,
                examples: vec![content.to_string()],
            };

            self.clusters.push(cluster);
            let cluster_idx = self.clusters.len() - 1;
            self.add_to_prefix_tree(cluster_idx, &tokens);

            // LRU eviction
            self.evict_if_needed();

            cluster_id
        }
    }

    /// Match a log message against the model without modifying state.
    pub fn match_log_message(&self, content: &str) -> Option<&LogCluster> {
        let tokens = self.tokenize(content);
        if tokens.is_empty() {
            return None;
        }
        let (cluster_idx, _sim) = self.tree_search_match(&tokens)?;
        Some(&self.clusters[cluster_idx])
    }

    pub fn get_clusters(&self) -> &[LogCluster] {
        &self.clusters
    }

    /// Rebuild from clusters (e.g., after loading from table).
    /// Reconstructs the prefix tree from existing clusters.
    pub fn from_clusters(config: DrainConfig, clusters: Vec<LogCluster>) -> Self {
        let next_cluster_id = clusters.iter().map(|c| c.cluster_id).max().unwrap_or(0) + 1;
        let mut state = Self {
            config,
            root: Node::default(),
            clusters,
            next_cluster_id,
        };
        // Rebuild prefix tree
        for idx in 0..state.clusters.len() {
            let tokens = state.clusters[idx].template_tokens.clone();
            state.add_to_prefix_tree(idx, &tokens);
        }
        state
    }

    /// Extract parameter values from a log message given a template.
    /// Supports both unnamed `<*>` and named `<user>` wildcards.
    pub fn extract_params(template: &str, log_message: &str) -> Vec<(String, String)> {
        let tmpl_tokens: Vec<&str> = template.split_whitespace().collect();
        let msg_tokens: Vec<&str> = log_message.split_whitespace().collect();

        if tmpl_tokens.len() != msg_tokens.len() {
            return Vec::new();
        }

        let mut idx = 0usize;
        tmpl_tokens
            .iter()
            .zip(msg_tokens.iter())
            .filter_map(|(t, m)| {
                if is_wildcard(t) {
                    let name = extract_wildcard_name(t).unwrap_or_else(|| {
                        idx += 1;
                        format!("_{idx}")
                    });
                    Some((name, m.to_string()))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Tokenize a log message into tokens.
    ///
    /// If `delimiters` is set, split ONLY on those characters.
    /// Otherwise, split on whitespace and replace `extra_delimiters` with spaces first.
    fn tokenize(&self, content: &str) -> Vec<String> {
        let mut s = content.to_string();

        if !self.config.delimiters.is_empty() {
            // Custom delimiters override: split only on these
            for delim in &self.config.delimiters {
                s = s.replace(delim.as_str(), "\x00");
            }
            s.split('\x00')
                .filter(|t| !t.is_empty())
                .map(|t| self.maybe_parametrize(t))
                .collect()
        } else {
            // Default: whitespace + extra_delimiters
            for delim in &self.config.extra_delimiters {
                s = s.replace(delim.as_str(), " ");
            }
            s.split_whitespace()
                .map(|t| self.maybe_parametrize(t))
                .collect()
        }
    }

    fn maybe_parametrize(&self, token: &str) -> String {
        if self.config.parametrize_numeric_tokens && is_numeric(token) {
            self.config.param_str.clone()
        } else {
            token.to_string()
        }
    }

    /// Search prefix tree and fast-match among candidate clusters.
    /// Returns (cluster_index_in_self.clusters, similarity).
    fn tree_search_match(&self, tokens: &[String]) -> Option<(usize, f64)> {
        let token_count = tokens.len().to_string();

        // Level 1: by token count
        let count_node = self.root.key_to_child.get(&token_count)?;

        // Levels 2..depth: by prefix tokens
        let max_depth = self.config.depth.max(3) - 2; // tree traversal depth
        let mut current = count_node;

        for (i, token) in tokens.iter().enumerate() {
            if i >= max_depth {
                break;
            }

            if let Some(child) = current.key_to_child.get(token.as_str()) {
                current = child;
            } else if let Some(child) = current.key_to_child.get(&self.config.param_str) {
                current = child;
            } else {
                return None;
            }
        }

        // Fast match among candidate clusters
        self.fast_match(&current.cluster_ids, tokens)
    }

    /// Find best matching cluster from candidates.
    fn fast_match(&self, cluster_ids: &[usize], tokens: &[String]) -> Option<(usize, f64)> {
        let mut best_idx = None;
        let mut best_sim = 0.0f64;

        for &cluster_idx in cluster_ids {
            if cluster_idx >= self.clusters.len() {
                continue; // stale reference after eviction
            }
            let cluster = &self.clusters[cluster_idx];
            let sim = get_seq_distance(
                &cluster.template_tokens,
                tokens,
                &self.config.param_str,
            );
            if sim > best_sim {
                best_sim = sim;
                best_idx = Some(cluster_idx);
            }
        }

        if best_sim >= self.config.sim_th {
            best_idx.map(|idx| (idx, best_sim))
        } else {
            None
        }
    }

    /// Add a cluster to the prefix tree.
    fn add_to_prefix_tree(&mut self, cluster_idx: usize, tokens: &[String]) {
        let token_count = tokens.len().to_string();
        let max_depth = self.config.depth.max(3) - 2;
        let max_children = self.config.max_children;
        let param_str = self.config.param_str.clone();

        // Level 1: by token count
        let count_node = self
            .root
            .key_to_child
            .entry(token_count)
            .or_default();

        // Levels 2..depth: by prefix tokens
        let mut current = count_node;

        for (i, token) in tokens.iter().enumerate() {
            if i >= max_depth {
                break;
            }

            let key = if current.key_to_child.len() >= max_children
                && !current.key_to_child.contains_key(token.as_str())
            {
                // Too many children, use wildcard
                param_str.clone()
            } else {
                token.clone()
            };

            current = current.key_to_child.entry(key).or_default();
        }

        current.cluster_ids.push(cluster_idx);
    }

    /// Evict oldest cluster if over max_clusters limit.
    fn evict_if_needed(&mut self) {
        while self.clusters.len() > self.config.max_clusters {
            // Remove first (oldest) cluster
            self.clusters.remove(0);

            // Adjust all cluster indices in the tree (shift down by 1)
            adjust_tree_indices(&mut self.root);
        }
    }
}

/// Adjust cluster indices in tree after removing index 0.
fn adjust_tree_indices(node: &mut Node) {
    node.cluster_ids.retain_mut(|idx| {
        if *idx == 0 {
            false // remove stale
        } else {
            *idx -= 1;
            true
        }
    });

    for child in node.key_to_child.values_mut() {
        adjust_tree_indices(child);
    }
}

/// Calculate similarity between a template and a token sequence.
fn get_seq_distance(seq1: &[String], seq2: &[String], param_str: &str) -> f64 {
    if seq1.len() != seq2.len() {
        return 0.0;
    }
    if seq1.is_empty() {
        return 0.0;
    }

    let match_count = seq1
        .iter()
        .zip(seq2.iter())
        .filter(|(t1, t2)| *t1 == *t2 || t1.as_str() == param_str)
        .count();

    match_count as f64 / seq1.len() as f64
}

/// Merge two sequences into a template, replacing differences with param_str.
fn create_template(seq1: &[String], seq2: &[String], param_str: &str) -> Vec<String> {
    seq1.iter()
        .zip(seq2.iter())
        .map(|(t1, t2)| {
            if t1 == t2 {
                t1.clone()
            } else {
                param_str.to_string()
            }
        })
        .collect()
}

/// Check if a token is a wildcard like `<*>`, `<user>`, `{}`, etc.
fn is_wildcard(token: &str) -> bool {
    (token.starts_with('<') && token.ends_with('>'))
        || token == "{}"
        || (token.starts_with('{') && token.ends_with('}'))
}

/// Extract the name from a wildcard token. Returns None for unnamed wildcards.
/// `<*>` → None, `<user>` → Some("user"), `{ip}` → Some("ip"), `{}` → None
fn extract_wildcard_name(token: &str) -> Option<String> {
    let inner = if (token.starts_with('<') && token.ends_with('>'))
        || (token.starts_with('{') && token.ends_with('}'))
    {
        &token[1..token.len() - 1]
    } else {
        return None;
    };

    if inner.is_empty() || inner == "*" {
        None
    } else {
        Some(inner.to_string())
    }
}

/// Check if a string is numeric (integer or float).
fn is_numeric(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Fast path: try to detect common numeric patterns
    let bytes = s.as_bytes();
    let start = if bytes[0] == b'-' || bytes[0] == b'+' {
        if bytes.len() == 1 {
            return false;
        }
        1
    } else {
        0
    };

    let mut has_dot = false;
    for &b in &bytes[start..] {
        if b == b'.' {
            if has_dot {
                return false;
            }
            has_dot = true;
        } else if !b.is_ascii_digit() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_numeric() {
        assert!(is_numeric("123"));
        assert!(is_numeric("3.14"));
        assert!(is_numeric("-42"));
        assert!(is_numeric("+1.0"));
        assert!(!is_numeric("abc"));
        assert!(!is_numeric("12.34.56"));
        assert!(!is_numeric(""));
        assert!(!is_numeric("-"));
        assert!(!is_numeric("192.168.1.1"));
    }

    #[test]
    fn test_tokenize_basic() {
        let state = DrainState::new(DrainConfig::default());
        let tokens = state.tokenize("User alice logged in");
        assert_eq!(tokens, vec!["User", "alice", "logged", "in"]);
    }

    #[test]
    fn test_tokenize_numeric() {
        let state = DrainState::new(DrainConfig::default());
        let tokens = state.tokenize("Request took 123 ms");
        assert_eq!(tokens, vec!["Request", "took", "<*>", "ms"]);
    }

    #[test]
    fn test_tokenize_extra_delimiters() {
        let config = DrainConfig {
            extra_delimiters: vec!["=".to_string(), "/".to_string()],
            ..Default::default()
        };
        let state = DrainState::new(config);
        let tokens = state.tokenize("key=value path/to/file");
        assert_eq!(
            tokens,
            vec!["key", "value", "path", "to", "file"]
        );
    }

    fn s(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_seq_distance() {
        let param = "<*>";
        assert_eq!(
            get_seq_distance(
                &s(&["User", "<*>", "logged", "in"]),
                &s(&["User", "bob", "logged", "in"]),
                param
            ),
            1.0
        );

        assert_eq!(
            get_seq_distance(&s(&["Error", "A"]), &s(&["User", "B"]), param),
            0.0
        );
    }

    #[test]
    fn test_create_template() {
        let tmpl = create_template(
            &s(&["User", "alice", "logged", "in"]),
            &s(&["User", "bob", "logged", "in"]),
            "<*>",
        );
        assert_eq!(tmpl, s(&["User", "<*>", "logged", "in"]));
    }

    #[test]
    fn test_add_and_match() {
        // depth=3 so tree path is: token_count → first_token → leaf
        // This means "alice"/"bob" end up at the same leaf for fast_match
        let config = DrainConfig {
            depth: 3,
            ..Default::default()
        };
        let mut state = DrainState::new(config);

        state.add_log_message("User alice logged in from 192.168.1.1");
        state.add_log_message("User bob logged in from 10.0.0.1");
        state.add_log_message("User charlie logged in from 172.16.0.1");

        let matched = state.match_log_message("User dave logged in from 1.2.3.4");
        assert!(matched.is_some());
        let tmpl = matched.unwrap().get_template();
        assert!(tmpl.contains("<*>"));
        assert!(tmpl.contains("User"));
        assert!(tmpl.contains("logged"));
        assert!(tmpl.contains("in"));
    }

    #[test]
    fn test_multiple_clusters() {
        let config = DrainConfig {
            depth: 3,
            sim_th: 0.3, // "Error: X Y" vs "Error: A B" = 1/3 ≈ 0.33
            ..Default::default()
        };
        let mut state = DrainState::new(config);

        // Pattern 1: user login (4 tokens, sim=3/4=0.75 > 0.3)
        state.add_log_message("User alice logged in");
        state.add_log_message("User bob logged in");

        // Pattern 2: error (3 tokens, sim=1/3=0.33 > 0.3)
        state.add_log_message("Error: connection timeout");
        state.add_log_message("Error: disk full");

        let clusters = state.get_clusters();
        assert_eq!(clusters.len(), 2);

        let templates: Vec<String> = clusters.iter().map(|c| c.get_template()).collect();
        assert!(templates.iter().any(|t| t.contains("User") && t.contains("<*>")));
        assert!(templates.iter().any(|t| t.contains("Error:") && t.contains("<*>")));
    }

    #[test]
    fn test_cluster_sizes() {
        let config = DrainConfig {
            depth: 3,
            ..Default::default()
        };
        let mut state = DrainState::new(config);

        state.add_log_message("User alice logged in");
        state.add_log_message("User bob logged in");
        state.add_log_message("User charlie logged in");

        assert_eq!(state.clusters.len(), 1);
        assert_eq!(state.clusters[0].size, 3);
    }

    #[test]
    fn test_extract_params_unnamed() {
        let params = DrainState::extract_params(
            "User <*> logged in from <*>",
            "User admin logged in from 192.168.1.1",
        );
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], ("_1".to_string(), "admin".to_string()));
        assert_eq!(params[1], ("_2".to_string(), "192.168.1.1".to_string()));
    }

    #[test]
    fn test_extract_params_named() {
        let params = DrainState::extract_params(
            "User <user> logged in from <ip>",
            "User admin logged in from 192.168.1.1",
        );
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], ("user".to_string(), "admin".to_string()));
        assert_eq!(params[1], ("ip".to_string(), "192.168.1.1".to_string()));
    }

    #[test]
    fn test_extract_params_no_params() {
        let params = DrainState::extract_params("hello world", "hello world");
        assert!(params.is_empty());
    }

    #[test]
    fn test_lru_eviction() {
        let config = DrainConfig {
            max_clusters: 3,
            sim_th: 0.99, // very strict to force new clusters
            ..Default::default()
        };
        let mut state = DrainState::new(config);

        state.add_log_message("aaa bbb ccc");
        state.add_log_message("ddd eee fff");
        state.add_log_message("ggg hhh iii");
        assert_eq!(state.clusters.len(), 3);

        // This should evict the oldest
        state.add_log_message("jjj kkk lll");
        assert!(state.clusters.len() <= 3);
    }

    #[test]
    fn test_serialization_roundtrip() {
        let mut state = DrainState::new(DrainConfig::default());
        state.add_log_message("User alice logged in from 192.168.1.1");
        state.add_log_message("User bob logged in from 10.0.0.1");
        state.add_log_message("Error: connection timeout");

        let json = serde_json::to_string(&state).unwrap();
        let restored: DrainState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.clusters.len(), state.clusters.len());
        assert_eq!(restored.next_cluster_id, state.next_cluster_id);
        for (orig, rest) in state.clusters.iter().zip(restored.clusters.iter()) {
            assert_eq!(orig.cluster_id, rest.cluster_id);
            assert_eq!(orig.template_tokens, rest.template_tokens);
            assert_eq!(orig.size, rest.size);
        }
    }

    #[test]
    fn test_custom_param_str() {
        let config = DrainConfig {
            depth: 3,
            param_str: "{}".to_string(),
            ..Default::default()
        };
        let mut state = DrainState::new(config);

        state.add_log_message("User alice logged in");
        state.add_log_message("User bob logged in");

        let tmpl = state.clusters[0].get_template();
        assert!(tmpl.contains("{}"));
        assert!(!tmpl.contains("<*>"));
    }

    #[test]
    fn test_custom_delimiters() {
        // Split only on "|" — whitespace is preserved in tokens
        let config = DrainConfig {
            delimiters: vec!["|".to_string()],
            parametrize_numeric_tokens: false,
            ..Default::default()
        };
        let state = DrainState::new(config);
        let tokens = state.tokenize("2024-01-01|INFO|User logged in");
        assert_eq!(tokens, vec!["2024-01-01", "INFO", "User logged in"]);
    }

    #[test]
    fn test_empty_input() {
        let mut state = DrainState::new(DrainConfig::default());
        let id = state.add_log_message("");
        assert_eq!(id, 0);
        assert!(state.clusters.is_empty());

        let matched = state.match_log_message("");
        assert!(matched.is_none());
    }
}
