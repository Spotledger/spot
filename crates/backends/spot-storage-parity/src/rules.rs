//! The rules, read from `docs/storage-semantics.md`.
//!
//! The document is the one place the rules live. Each rule is one table row:
//! `| SEM-TXT-01 | decided | the rule in one sentence | the source |`. A row whose status is
//! `open` carries the question for the lead in its sentence and is never given a guessed answer.

use std::collections::BTreeMap;
use std::path::Path;

/// Whether the sources decide a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuleStatus {
    /// An ADR or the contract states it.
    Decided,
    /// The sources do not say; the lead decides.
    Open,
}

/// One rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Stable id, `SEM-<GROUP>-<NN>`.
    pub id: String,
    /// Decided or open.
    pub status: RuleStatus,
    /// The rule (or, for an open rule, the question).
    pub text: String,
    /// Where it comes from.
    pub source: String,
}

/// Parses the rule rows of the document.
pub fn parse_rules(markdown: &str) -> Result<BTreeMap<String, Rule>, String> {
    let mut rules = BTreeMap::new();
    for line in markdown.lines() {
        let line = line.trim();
        if !line.starts_with("| SEM-") {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        let [id, status, text, source] = cells[..] else {
            return Err(format!("a rule row needs 4 cells: {line}"));
        };
        let valid_id = id.split('-').count() == 3
            && id.starts_with("SEM-")
            && id
                .rsplit('-')
                .next()
                .is_some_and(|n| n.len() == 2 && n.chars().all(|c| c.is_ascii_digit()));
        if !valid_id {
            return Err(format!("`{id}` is not a rule id (SEM-<GROUP>-<NN>)"));
        }
        let status = match status {
            "decided" => RuleStatus::Decided,
            "open" => RuleStatus::Open,
            other => {
                return Err(format!(
                    "{id}: status must be decided or open, not `{other}`"
                ));
            }
        };
        let rule = Rule {
            id: id.to_owned(),
            status,
            text: text.to_owned(),
            source: source.to_owned(),
        };
        if rules.insert(id.to_owned(), rule).is_some() {
            return Err(format!("rule {id} is listed twice"));
        }
    }
    if rules.is_empty() {
        return Err("no rules found (rows start with `| SEM-`)".into());
    }
    Ok(rules)
}

/// Reads the rules from the document at `path`.
pub fn load_rules(path: &Path) -> Result<BTreeMap<String, Rule>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_rules(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_parse() -> Result<(), String> {
        let md = "| id | status |\n| SEM-TXT-01 | decided | exact | ADR-053 |\n| SEM-TXT-02 | open | Question? | none |\n";
        let r = parse_rules(md)?;
        assert_eq!(r.len(), 2);
        assert_eq!(r["SEM-TXT-02"].status, RuleStatus::Open);
        assert!(parse_rules("| SEM-TXT-1 | decided | x | y |").is_err());
        assert!(parse_rules("| SEM-TXT-01 | maybe | x | y |").is_err());
        assert!(parse_rules("nothing").is_err());
        Ok(())
    }
}
