use serde::{Deserialize, Serialize};

const SEARCH_HISTORY_LIMIT: usize = 30;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SearchHistory(Vec<String>);

impl SearchHistory {
    pub(crate) fn entries(&self) -> &[String] {
        &self.0
    }

    pub(crate) fn record(&mut self, query: &str) -> bool {
        let query = query.trim();
        if query.is_empty() || self.0.first().is_some_and(|first| first == query) {
            return false;
        }
        self.0.retain(|entry| entry != query);
        self.0.insert(0, query.to_owned());
        self.0.truncate(SEARCH_HISTORY_LIMIT);
        true
    }

    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    pub(crate) fn normalize(&mut self) {
        let mut entries = Vec::with_capacity(SEARCH_HISTORY_LIMIT);
        for entry in &self.0 {
            let entry = entry.trim();
            if !entry.is_empty() && !entries.iter().any(|existing| existing == entry) {
                entries.push(entry.to_owned());
                if entries.len() == SEARCH_HISTORY_LIMIT {
                    break;
                }
            }
        }
        self.0 = entries;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_keeps_thirty_recent_unique_queries_and_can_be_cleared() {
        let mut history = SearchHistory::default();
        assert!(!history.record(" \n "));
        for i in 0..35 {
            assert!(history.record(&format!("query-{i}")));
        }
        assert_eq!(history.entries().len(), 30);
        assert_eq!(history.entries().first().unwrap(), "query-34");
        assert_eq!(history.entries().last().unwrap(), "query-5");
        assert!(history.record("  query-10  "));
        assert_eq!(history.entries()[..2], ["query-10", "query-34"]);
        assert_eq!(history.entries().len(), 30);
        assert!(!history.record("query-10"));
        history.clear();
        assert!(history.entries().is_empty());
    }
}
