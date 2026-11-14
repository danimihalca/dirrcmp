//! Fuzzy subsequence matching for the file picker (std only).
//!
//! `fuzzy("tui", "src/tui.rs")` finds the query characters in order, scoring
//! consecutive runs, word starts (after `/ _ - .` or a camelCase hump) and matches
//! inside the file name higher, so the usual "type a few letters" workflow ranks the
//! expected file first. Lower-case queries ignore case, a capital makes it exact.

#[derive(Debug, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    /// Char indices of the text that matched, in order.
    pub positions: Vec<usize>,
}

const NEG: i32 = i32::MIN / 2;
const CONSEC: i32 = 15;
const GAP_CAP: usize = 10;
const WINDOW: usize = 12;

fn bonus(t: &[char], j: usize) -> i32 {
    if j == 0 {
        return 8;
    }
    let (p, c) = (t[j - 1], t[j]);
    if p == '/' || p == '\\' {
        10
    } else if matches!(p, '_' | '-' | '.' | ' ') {
        6
    } else if p.is_lowercase() && c.is_uppercase() {
        4
    } else {
        0
    }
}

pub fn fuzzy(query: &str, text: &str) -> Option<Match> {
    let q: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return Some(Match { score: 0, positions: Vec::new() });
    }
    let t: Vec<char> = text.chars().collect();
    let (n, m) = (q.len(), t.len());
    if n > m {
        return None;
    }
    let exact = q.iter().any(|c| c.is_uppercase());
    let eq = |a: char, b: char| {
        if exact {
            a == b
        } else {
            a == b || a.to_lowercase().eq(b.to_lowercase())
        }
    };
    // cheap reject: is it a subsequence at all?
    let mut qi = 0;
    for &c in &t {
        if qi < n && eq(q[qi], c) {
            qi += 1;
        }
    }
    if qi < n {
        return None;
    }

    let mut dp = vec![vec![NEG; m]; n];
    let mut from = vec![vec![usize::MAX; m]; n];
    // prefix maxima of the previous row: (value, index) over p <= j
    let mut pm: Vec<(i32, usize)> = vec![(NEG, 0); m];
    for i in 0..n {
        for j in i..m {
            if !eq(q[i], t[j]) {
                continue;
            }
            let base = 10 + bonus(&t, j);
            if i == 0 {
                dp[i][j] = base - (j.min(6) as i32);
                continue;
            }
            let (mut best, mut bp) = (NEG, usize::MAX);
            // recent predecessors: exact gap penalty
            let lo = j.saturating_sub(1 + WINDOW).max(i - 1);
            for p in lo..j {
                if dp[i - 1][p] == NEG {
                    continue;
                }
                let gap = j - p - 1;
                let v = dp[i - 1][p] + if gap == 0 { CONSEC } else { -(gap.min(GAP_CAP) as i32) };
                if v > best {
                    best = v;
                    bp = p;
                }
            }
            // older predecessors all pay the capped penalty: take the best of them
            if lo > i - 1 {
                let (v, p) = pm[lo - 1];
                if v != NEG && v - (GAP_CAP as i32) > best {
                    best = v - GAP_CAP as i32;
                    bp = p;
                }
            }
            if best != NEG {
                dp[i][j] = best + base;
                from[i][j] = bp;
            }
        }
        // prefix maxima for the next row
        let mut cur = (NEG, 0usize);
        for j in 0..m {
            if dp[i][j] > cur.0 {
                cur = (dp[i][j], j);
            }
            pm[j] = cur;
        }
    }
    let (mut score, mut j) = (NEG, usize::MAX);
    for e in 0..m {
        if dp[n - 1][e] == NEG {
            continue;
        }
        let v = dp[n - 1][e] + if e == m - 1 { 2 } else { 0 };
        if v > score {
            score = v;
            j = e;
        }
    }
    if j == usize::MAX {
        return None;
    }
    let mut positions = vec![0; n];
    let mut i = n - 1;
    loop {
        positions[i] = j;
        if i == 0 {
            break;
        }
        j = from[i][j];
        i -= 1;
    }
    // a match inside the file name beats one spread over the directories
    let name_start = t.iter().rposition(|c| *c == '/' || *c == '\\').map_or(0, |p| p + 1);
    if positions[0] >= name_start {
        score += 15;
    }
    score -= (m / 16) as i32;
    Some(Match { score, positions })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rank(q: &str, items: &[&str]) -> Vec<String> {
        let mut v: Vec<(i32, &str)> =
            items.iter().filter_map(|t| fuzzy(q, t).map(|m| (m.score, *t))).collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        v.into_iter().map(|(_, t)| t.to_string()).collect()
    }

    #[test]
    fn matches_subsequences_only() {
        assert!(fuzzy("abc", "a-b-c").is_some());
        assert!(fuzzy("abc", "acb").is_none());
        assert!(fuzzy("abcd", "abc").is_none());
        assert!(fuzzy("x", "").is_none());
        let m = fuzzy("", "anything").unwrap();
        assert_eq!((m.score, m.positions.len()), (0, 0));
        assert!(fuzzy("  ", "a").is_some(), "spaces in the query are ignored");
        assert!(fuzzy("a b", "xaxb").is_some());
    }

    #[test]
    fn reports_the_matched_positions() {
        let m = fuzzy("tui", "src/tui.rs").unwrap();
        assert_eq!(m.positions, [4, 5, 6]);
        let m = fuzzy("sr", "src/main.rs").unwrap();
        assert_eq!(m.positions, [0, 1]);
        // unicode: positions are char indices
        let m = fuzzy("ü", "dïr/ü.txt").unwrap();
        assert_eq!(m.positions, [4]);
    }

    #[test]
    fn case_is_smart() {
        assert!(fuzzy("readme", "README.md").is_some());
        assert!(fuzzy("README", "README.md").is_some());
        assert!(fuzzy("README", "readme.md").is_none(), "a capital makes it exact");
        assert!(fuzzy("Re", "docs/readme.md").is_none());
    }

    #[test]
    fn ranks_the_expected_file_first() {
        let files = [
            "src/domain.rs",
            "src/main.rs",
            "docs/m_a_i_n_notes.txt",
            "tests/maintenance/index.rs",
            "src/mail/inbox.rs",
        ];
        assert_eq!(rank("main", &files)[0], "src/main.rs");
        let files = ["src/tui.rs", "tests/utility/index.rs", "src/ui/t.rs", "build/output.rs"];
        assert_eq!(rank("tui", &files)[0], "src/tui.rs");
        // word starts beat scattered letters
        let files = ["src/side_view.rs", "src/something_else_view.rs", "src/sidebar.rs"];
        assert_eq!(rank("sv", &files)[0], "src/side_view.rs");
        // camelCase humps count as word starts
        let files = ["src/FileViewer.ts", "src/fileviewer_old.ts", "src/xfxvx.ts"];
        assert_eq!(rank("FV", &files)[0], "src/FileViewer.ts");
        // exact file name beats a longer path with the same letters
        let files = ["a/b/c/config.json", "config.json", "configure/json.rs"];
        assert_eq!(rank("config.json", &files)[0], "config.json");
        // consecutive beats gapped
        let files = ["x/abcdef.txt", "x/a_b_c_d_e_f.txt"];
        assert_eq!(rank("abc", &files)[0], "x/abcdef.txt");
    }

    #[test]
    fn handles_long_paths_and_many_items_quickly() {
        let long = format!("{}/target/file_name.rs", "dir/".repeat(60));
        assert!(fuzzy("tfn", &long).is_some());
        let start = std::time::Instant::now();
        let mut hits = 0;
        for i in 0..20_000 {
            let p = format!("crates/module_{i}/src/some_long_directory/name_{i}.rs");
            if fuzzy("mdsrcnm", &p).is_some() {
                hits += 1;
            }
        }
        assert!(hits > 0);
        assert!(start.elapsed().as_secs() < 5, "{:?}", start.elapsed());
    }
}
