// Subsequence matcher in the spirit of fuzzysort: every needle char must appear in order;
// consecutive runs, word starts and early matches score higher.
pub fn score(needle: &str, haystack: &str) -> Option<i64> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.chars().collect();
    let lower: Vec<char> = haystack.to_lowercase().chars().collect();
    if lower.len() != hay.len() {
        return score_simple(needle, haystack);
    }
    let mut total = 0i64;
    let mut at = 0usize;
    let mut previous: Option<usize> = None;
    for wanted in needle.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let found = (at..lower.len()).find(|&i| lower[i] == wanted)?;
        total += 10;
        if previous.is_some_and(|p| p + 1 == found) {
            total += 15;
        }
        let boundary = found == 0 || !hay[found - 1].is_alphanumeric() || (hay[found].is_uppercase() && hay[found - 1].is_lowercase());
        if boundary {
            total += 8;
        }
        total -= (found - at) as i64;
        previous = Some(found);
        at = found + 1;
    }
    Some(total - (hay.len() as i64) / 8)
}

fn score_simple(needle: &str, haystack: &str) -> Option<i64> {
    haystack.to_lowercase().contains(&needle.to_lowercase()).then_some(1)
}

// Indexes of matching items, best first; ties keep the original order.
pub fn filter<T>(needle: &str, items: &[T], text: impl Fn(&T) -> String) -> Vec<usize> {
    if needle.trim().is_empty() {
        return (0..items.len()).collect();
    }
    let mut scored: Vec<(i64, usize)> = items.iter().enumerate().filter_map(|(i, item)| Some((score(needle, &text(item))?, i))).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_subsequences_and_ranks_tight_matches_first() {
        assert!(score("xyz", "src/main.rs").is_none());
        assert!(score("smr", "src/main.rs").is_some());
        let items = ["docs/readme.md", "src/main.rs", "src/domain/mod.rs"];
        let order = filter("main", &items, |s| s.to_string());
        assert_eq!(order[0], 1);
        assert_eq!(filter("", &items, |s| s.to_string()), vec![0, 1, 2]);
        assert_eq!(filter("GPT", &["Claude Sonnet", "GPT-5"], |s| s.to_string()), vec![1]);
    }
}
