//! Search over settings: tokenised, case-insensitive, accent-folded, with filters and a
//! documented ranking.

use spot_mgmt_ops::Risk;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::builder::SettingsRegistry;
use crate::diff::is_different;
use crate::model::{Origin, Restart, Scope, ScopeRef};
use crate::resolution::{Chain, resolve};
use crate::spec::SettingSpec;
use crate::store::ValueStore;

/// Which field a hit matched, in ranking order (earliest is best).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MatchedField {
    /// The whole key.
    ExactKey,
    /// A prefix of the key.
    KeyPrefix,
    /// A word of the title.
    TitleWord,
    /// A word of the summary or description.
    SummaryWord,
    /// A tag.
    Tag,
}

impl MatchedField {
    /// The ranking score: higher is better, integer only.
    #[must_use]
    fn score(self) -> u32 {
        match self {
            MatchedField::ExactKey => 1000,
            MatchedField::KeyPrefix => 500,
            MatchedField::TitleWord => 100,
            MatchedField::SummaryWord => 50,
            MatchedField::Tag => 10,
        }
    }
}

/// One search result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// The setting key.
    pub key: String,
    /// The setting title.
    pub title: String,
    /// The setting summary.
    pub summary: String,
    /// The ranking score (higher is better).
    pub score: u32,
    /// The best field that matched.
    pub matched_field: MatchedField,
    /// The matched fragment, for display.
    pub snippet: String,
}

/// Filters applied before a setting is scored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    /// Only settings in this area (the key's first segment).
    pub area: Option<String>,
    /// Only settings at this risk level.
    pub risk: Option<Risk>,
    /// Only settings that may be set at this scope.
    pub scope: Option<Scope>,
    /// Only settings needing this kind of restart.
    pub restart: Option<Restart>,
    /// Only settings whose effective value differs from their default.
    pub only_changed: bool,
    /// Only settings whose effective value is set exactly at this scope.
    pub only_modified_at_scope: Option<ScopeRef>,
}

/// Searches settings, returning hits ranked best first with ties broken by key.
///
/// # Ranking
///
/// The best field any query token matched decides the rank: an exact key beats a key prefix,
/// which beats a title word, which beats a summary/description word, which beats a tag.
/// Ties break on the key, so the order is stable.
#[must_use]
pub fn search(
    query: &str,
    registry: &SettingsRegistry,
    chain: &Chain,
    store: &dyn ValueStore,
    filters: &Filter,
) -> Vec<Hit> {
    let mut comparisons = 0u64;
    search_counted(query, registry, chain, store, filters, &mut comparisons)
}

/// Like [`search`], counting every token/field comparison into `comparisons`.
///
/// The counter exists so a test can assert a budget without depending on wall-clock time.
#[must_use]
pub fn search_counted(
    query: &str,
    registry: &SettingsRegistry,
    chain: &Chain,
    store: &dyn ValueStore,
    filters: &Filter,
    comparisons: &mut u64,
) -> Vec<Hit> {
    let tokens = tokenize(query);
    if tokens.is_empty() {
        return Vec::new();
    }
    let whole = fold(query.trim());

    let mut hits = Vec::new();
    for spec in registry.specs() {
        if !passes_filters(spec, chain, store, registry, filters) {
            continue;
        }
        if let Some(hit) = score_spec(spec, &whole, &tokens, comparisons) {
            hits.push(hit);
        }
    }

    hits.sort_by(|left, right| {
        left.matched_field
            .cmp(&right.matched_field)
            .then_with(|| right.score.cmp(&left.score))
            .then_with(|| left.key.cmp(&right.key))
    });
    hits
}

/// Whether a spec passes every filter (visibility included).
fn passes_filters(
    spec: &SettingSpec,
    chain: &Chain,
    store: &dyn ValueStore,
    registry: &SettingsRegistry,
    filters: &Filter,
) -> bool {
    if let Some(area) = &filters.area
        && spec.area() != area
    {
        return false;
    }
    if let Some(risk) = filters.risk
        && spec.risk != risk
    {
        return false;
    }
    if let Some(scope) = filters.scope
        && !spec.scopes.contains(&scope)
    {
        return false;
    }
    if let Some(restart) = &filters.restart
        && std::mem::discriminant(&spec.restart) != std::mem::discriminant(restart)
    {
        return false;
    }
    if let Some(dep) = &spec.depends_on {
        let visible = resolve(&dep.key, chain, store, registry).is_ok_and(|resolved| {
            resolved.value == crate::model::EffectiveValue::Value(dep.equals.clone())
        });
        if !visible {
            return false;
        }
    }
    if filters.only_changed || filters.only_modified_at_scope.is_some() {
        let Ok(resolved) = resolve(&spec.key, chain, store, registry) else {
            return false;
        };
        if filters.only_changed && !is_different(spec, &resolved.value, &resolved.origin) {
            return false;
        }
        if let Some(scope) = &filters.only_modified_at_scope
            && !matches!(&resolved.origin, Origin::Set(at) if at == scope)
        {
            return false;
        }
    }
    true
}

/// Scores one spec against the tokens; `None` when nothing matched.
fn score_spec(
    spec: &SettingSpec,
    whole_query: &str,
    tokens: &[String],
    comparisons: &mut u64,
) -> Option<Hit> {
    let key = fold(&spec.key);
    let title = fold(&spec.title);
    let summary = fold(&spec.summary);
    let description = spec.description.as_deref().map(fold);

    let mut best: Option<(MatchedField, u32, String)> = None;

    // A query that names a whole dotted key (or a prefix of one) ranks above any word match.
    if !whole_query.is_empty() {
        *comparisons += 1;
        if key == whole_query {
            consider(&mut best, MatchedField::ExactKey, spec.key.clone());
        } else {
            *comparisons += 1;
            if key.starts_with(whole_query) {
                consider(&mut best, MatchedField::KeyPrefix, spec.key.clone());
            }
        }
    }

    for token in tokens {
        *comparisons += 1;
        if key == *token {
            consider(&mut best, MatchedField::ExactKey, spec.key.clone());
        }
        *comparisons += 1;
        if key.starts_with(token.as_str()) {
            consider(&mut best, MatchedField::KeyPrefix, spec.key.clone());
        }
        *comparisons += 1;
        if has_word(&title, token) {
            consider(&mut best, MatchedField::TitleWord, spec.title.clone());
        }
        *comparisons += 1;
        let summary_hit = has_word(&summary, token)
            || description
                .as_deref()
                .is_some_and(|text| has_word(text, token));
        if summary_hit {
            let snippet = if has_word(&summary, token) {
                spec.summary.clone()
            } else {
                spec.description.clone().unwrap_or_default()
            };
            consider(&mut best, MatchedField::SummaryWord, snippet);
        }
        for tag in &spec.tags {
            *comparisons += 1;
            if fold(tag) == *token {
                consider(&mut best, MatchedField::Tag, tag.clone());
            }
        }
    }

    best.map(|(matched_field, score, snippet)| Hit {
        key: spec.key.clone(),
        title: spec.title.clone(),
        summary: spec.summary.clone(),
        score,
        matched_field,
        snippet,
    })
}

/// Keeps the best (highest-priority, then highest-scoring) match seen so far.
fn consider(best: &mut Option<(MatchedField, u32, String)>, field: MatchedField, snippet: String) {
    let score = field.score();
    let replace = match best {
        Some((current_field, current_score, _)) => {
            field < *current_field || (field == *current_field && score > *current_score)
        }
        None => true,
    };
    if replace {
        *best = Some((field, score, snippet));
    }
}

/// Normalises text: decomposed, combining marks removed, lower-cased.
fn fold(text: &str) -> String {
    text.nfd()
        .filter(|c| !is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase()
}

/// Splits a query into folded, non-empty tokens.
fn tokenize(query: &str) -> Vec<String> {
    fold(query)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether `token` is a whole word of `text`.
fn has_word(text: &str, token: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric())
        .any(|word| word == token)
}
