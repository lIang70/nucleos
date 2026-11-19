//! A small in-memory BM25 index, for ranking a catalog of a few hundred short documents.
//!
//! This is the regime where a hand-written inverted index beats a search engine: the corpus is
//! tool definitions and skill files, it changes when a generation changes, and it has to be scored
//! in microseconds because it runs on the path to every prompt.
//!
//! Four rules are worth stating, because each one exists to stop a specific misfire:
//!
//! - **A name hit outscores a text hit** (weight 3 against 1). "webfetch" in a name is a stronger
//!   signal than the word "fetch" in a paragraph of description.
//! - **A query term matches by prefix, at a discount.** `webf` still finds `webfetch`, but half as
//!   strongly as the whole term would.
//! - **Per field, a query term contributes its best match, never the sum of them.** Otherwise a
//!   document that merely prefixes four expansions of one term outranks one that contains it.
//! - **CJK is indexed as single characters plus adjacent bigrams.** Han, Kana and Hangul have no
//!   whitespace boundaries; bigrams let a fragment match without a segmentation dictionary.
//!
//! The index keeps term statistics only — never the document text — and is immutable once built,
//! so it can be shared across tasks without a lock.

use std::collections::BTreeMap;

/// Okapi's term-frequency saturation.
const K1: f64 = 1.2;
/// Okapi's length-normalisation strength.
const B: f64 = 0.75;
/// What a name hit is worth, relative to a hit in the description.
const NAME_WEIGHT: f64 = 3.0;
/// What a text hit is worth.
const TEXT_WEIGHT: f64 = 1.0;
/// What a prefix-only match is worth, relative to the whole term.
const PREFIX_WEIGHT: f64 = 0.5;

/// One document to index.
#[derive(Debug, Clone)]
pub struct Doc {
    /// The identity returned in results.
    pub id: String,
    /// The boosted field: an identifier such as a tool name.
    pub name: String,
    /// The body field: a description.
    pub text: String,
}

/// One document whose fields are already tokenized.
///
/// For a caller that persists term counts: re-tokenizing text it already tokenized is work with a
/// way to get it wrong, since a query is always tokenized here and terms from another splitter
/// would silently never match.
#[derive(Debug, Clone)]
pub struct TermDoc {
    /// The identity returned in results.
    pub id: String,
    /// Term frequencies from the name field.
    pub name: BTreeMap<String, u32>,
    /// Term frequencies from the body field.
    pub text: BTreeMap<String, u32>,
}

/// One ranked match.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The document's identity.
    pub id: String,
    /// Its score, for ordering. Comparable across one index, meaningless across two.
    pub score: f64,
}

/// A built index.
#[derive(Debug)]
pub struct Index {
    /// Document ids, in the order they were given: ties keep that order, so a caller that wants a
    /// specific tie-break sorts its documents first.
    ids: Vec<String>,
    name: Field,
    text: Field,
}

#[derive(Debug, Default)]
struct Field {
    postings: BTreeMap<String, Vec<Posting>>,
    lengths: Vec<u32>,
    average: f64,
}

#[derive(Debug)]
struct Posting {
    doc: usize,
    frequency: u32,
}

impl Index {
    /// Build an index over documents.
    #[must_use]
    pub fn new(docs: Vec<Doc>) -> Self {
        Self::from_terms(
            docs.into_iter()
                .map(|doc| TermDoc {
                    name: frequencies(&tokenize(&doc.name)),
                    text: frequencies(&tokenize(&doc.text)),
                    id: doc.id,
                })
                .collect(),
        )
    }

    /// Build an index over documents that were tokenized elsewhere.
    #[must_use]
    pub fn from_terms(docs: Vec<TermDoc>) -> Self {
        let mut index = Self {
            ids: docs.iter().map(|doc| doc.id.clone()).collect(),
            name: Field::default(),
            text: Field::default(),
        };
        for (position, doc) in docs.into_iter().enumerate() {
            index.name.add(position, &doc.name);
            index.text.add(position, &doc.text);
        }
        index.name.finish();
        index.text.finish();
        index
    }

    /// Rank documents against a query, at most `limit` of them.
    ///
    /// `limit <= 0` means no limit. A query with no indexable terms, or a corpus with no match,
    /// returns nothing.
    #[must_use]
    pub fn search(&self, query: &str, limit: usize) -> Vec<Hit> {
        let terms = unique(&tokenize(query));
        if terms.is_empty() || self.ids.is_empty() {
            return Vec::new();
        }

        let mut scores = vec![0.0f64; self.ids.len()];
        let mut best = vec![0.0f64; self.ids.len()];

        for term in &terms {
            for (field, weight) in [(&self.name, NAME_WEIGHT), (&self.text, TEXT_WEIGHT)] {
                if field.average == 0.0 {
                    continue;
                }
                field.score(term, weight, &mut best);
                for (doc, score) in best.iter_mut().enumerate() {
                    scores[doc] += *score;
                    *score = 0.0;
                }
            }
        }

        let mut hits: Vec<Hit> = scores
            .into_iter()
            .enumerate()
            .filter(|(_, score)| *score > 0.0)
            .map(|(position, score)| Hit {
                id: self.ids[position].clone(),
                score,
            })
            .collect();
        // Descending score; ties keep the input order, which `sort_by` on a stable sort preserves.
        hits.sort_by(|left, right| right.score.total_cmp(&left.score));
        if limit > 0 {
            hits.truncate(limit);
        }
        hits
    }
}

impl Field {
    fn add(&mut self, doc: usize, frequencies: &BTreeMap<String, u32>) {
        let mut length = 0u32;
        for (term, frequency) in frequencies {
            if *frequency == 0 {
                continue;
            }
            self.postings
                .entry(term.clone())
                .or_default()
                .push(Posting {
                    doc,
                    frequency: *frequency,
                });
            length = length.saturating_add(*frequency);
        }
        self.lengths.push(length);
    }

    fn finish(&mut self) {
        let total: u64 = self.lengths.iter().map(|length| u64::from(*length)).sum();
        self.average = if self.lengths.is_empty() {
            0.0
        } else {
            total as f64 / self.lengths.len() as f64
        };
    }

    /// Fold one query term's contribution into `best`: the maximum over exact and prefix matches,
    /// never their sum.
    fn score(&self, term: &str, weight: f64, best: &mut [f64]) {
        for (index_term, postings) in &self.postings {
            let matched = if index_term == term {
                weight
            } else if index_term.starts_with(term) {
                weight * PREFIX_WEIGHT
            } else {
                continue;
            };

            let idf = inverse_document_frequency(self.lengths.len(), postings.len());
            for posting in postings {
                let frequency = f64::from(posting.frequency);
                let length = f64::from(self.lengths[posting.doc]);
                let denominator = frequency + K1 * (1.0 - B + B * length / self.average);
                let score = matched * idf * frequency * (K1 + 1.0) / denominator;
                if score > best[posting.doc] {
                    best[posting.doc] = score;
                }
            }
        }
    }
}

/// Okapi's inverse document frequency, with the usual smoothing.
///
/// Without the `+1` a term that appears in every document scores zero, and a query made only of
/// such terms ranks nothing at all — which reads as "search is broken" rather than "those words
/// are everywhere".
fn inverse_document_frequency(total: usize, matching: usize) -> f64 {
    let total = total as f64;
    let matching = matching as f64;
    (1.0 + (total - matching + 0.5) / (matching + 0.5)).ln()
}

fn frequencies(tokens: &[String]) -> BTreeMap<String, u32> {
    let mut counts = BTreeMap::new();
    for token in tokens {
        *counts.entry(token.clone()).or_insert(0) += 1;
    }
    counts
}

fn unique(tokens: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for token in tokens {
        if !seen.iter().any(|existing| existing == token) {
            seen.push(token.clone());
        }
    }
    // Sorted, so scoring is deterministic and needs no per-call allocation for ordering.
    seen.sort();
    seen
}

/// Split text into index terms.
///
/// Lowercased word runs; everything else separates, which is what makes `read_file` two terms for
/// free. Scripts without whitespace boundaries (Han, Hiragana, Katakana, Hangul) are emitted as
/// single characters *and* adjacent bigrams: a single character alone matches far too much, and a
/// bigram is what lets a two-character query hit a longer document without a dictionary.
#[must_use]
pub fn tokenize(value: &str) -> Vec<String> {
    let lowered = value.to_lowercase();
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut previous_cjk: Option<char> = None;

    for character in lowered.chars() {
        if is_cjk(character) {
            flush(&mut word, &mut tokens);
            if let Some(previous) = previous_cjk {
                tokens.push(format!("{previous}{character}"));
            }
            tokens.push(character.to_string());
            previous_cjk = Some(character);
        } else if character.is_alphanumeric() {
            word.push(character);
            previous_cjk = None;
        } else {
            flush(&mut word, &mut tokens);
            previous_cjk = None;
        }
    }
    flush(&mut word, &mut tokens);
    tokens
}

fn flush(word: &mut String, tokens: &mut Vec<String>) {
    if !word.is_empty() {
        tokens.push(std::mem::take(word));
    }
}

fn is_cjk(character: char) -> bool {
    matches!(character as u32,
        0x3040..=0x30FF      // Hiragana, Katakana
        | 0x3400..=0x4DBF    // Han extension A
        | 0x4E00..=0x9FFF    // Han
        | 0xAC00..=0xD7AF    // Hangul syllables
        | 0xF900..=0xFAFF    // Han compatibility
        | 0x20000..=0x2FA1F  // Han extensions B–F
    )
}

/// Terms for a catalog query: tokenized, then stripped of single-character ASCII tokens.
///
/// A bare `a` or `1` matches nearly every definition, and with prefix matching it expands to every
/// term starting with that letter — one filler word in a query would drag unrelated definitions
/// into the result. Single CJK characters stay: there they *are* whole words, and the bigrams need
/// them to match a fragment.
#[must_use]
pub fn catalog_terms(value: &str) -> Vec<String> {
    tokenize(value)
        .into_iter()
        .filter(|token| !(token.len() == 1 && token.is_ascii()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: &str, name: &str, text: &str) -> Doc {
        Doc {
            id: id.to_owned(),
            name: name.to_owned(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn underscores_split_because_they_are_not_word_characters() {
        assert_eq!(tokenize("read_file"), ["read", "file"]);
        assert_eq!(tokenize("Read File"), ["read", "file"]);
        // camelCase does not split: there is no boundary character, and guessing at one breaks
        // identifiers like "HTTPServer" into pieces nobody searches for.
        assert_eq!(tokenize("readFile"), ["readfile"]);
    }

    #[test]
    fn cjk_is_single_characters_plus_adjacent_bigrams() {
        assert_eq!(tokenize("读取"), ["读", "读取", "取"]);
        // A separator breaks the bigram chain: the two characters are no longer adjacent.
        assert_eq!(tokenize("读 取"), ["读", "取"]);
    }

    #[test]
    fn a_name_hit_outscores_a_text_hit() {
        let index = Index::new(vec![
            doc("named", "webfetch", "Fetch a URL."),
            doc("described", "request", "Fetch a URL over the network."),
        ]);
        let hits = index.search("webfetch", 2);
        assert_eq!(hits[0].id, "named");

        // The same term in the body ranks the other document, at the smaller weight.
        let hits = index.search("network", 2);
        assert_eq!(hits[0].id, "described");
    }

    #[test]
    fn a_prefix_matches_at_a_discount() {
        let index = Index::new(vec![doc("fetch", "webfetch", "")]);
        let prefix = index.search("webf", 1);
        let exact = index.search("webfetch", 1);

        assert_eq!(prefix[0].id, "fetch", "a fragment still finds the term");
        assert!(
            prefix[0].score < exact[0].score,
            "but never as strongly as the whole term"
        );
    }

    #[test]
    fn a_query_term_takes_its_best_match_per_field_not_the_sum() {
        // One document contains the term; the other prefixes it four ways. Summing prefix matches
        // would let the second win, which is the misfire this rule exists to stop.
        let index = Index::new(vec![
            doc("contains", "fetch", ""),
            doc("prefixes", "fetching fetchable fetcher fetches", ""),
        ]);
        let hits = index.search("fetch", 2);
        assert_eq!(hits[0].id, "contains");
    }

    #[test]
    fn a_cjk_fragment_matches_without_a_dictionary() {
        let index = Index::new(vec![doc("zh", "读文件", "读取工作区中的文件")]);
        assert_eq!(index.search("文件", 1)[0].id, "zh");
        assert_eq!(index.search("读取", 1)[0].id, "zh");
    }

    #[test]
    fn ties_keep_the_order_the_documents_arrived_in() {
        let index = Index::new(vec![doc("b", "same", ""), doc("a", "same", "")]);
        let hits = index.search("same", 2);
        assert_eq!(
            hits[0].id, "b",
            "the caller sorts if it wants another order"
        );
    }

    #[test]
    fn an_empty_query_or_an_empty_corpus_returns_nothing() {
        let index = Index::new(vec![doc("a", "read_file", "reads")]);
        assert!(index.search("", 10).is_empty());
        assert!(index.search("!!!", 10).is_empty());
        assert!(Index::new(Vec::new()).search("read", 10).is_empty());
    }

    #[test]
    fn a_limit_of_zero_means_no_limit() {
        let docs = (0..5)
            .map(|position| doc(&format!("t{position}"), "read_file", "reads a file"))
            .collect();
        assert_eq!(Index::new(docs).search("read", 0).len(), 5);
        assert_eq!(
            Index::new(
                (0..5)
                    .map(|position| doc(&format!("t{position}"), "read_file", "reads a file"))
                    .collect()
            )
            .search("read", 2)
            .len(),
            2
        );
    }

    #[test]
    fn a_term_in_every_document_still_ranks() {
        // Without the `+1` in the smoothing this scores zero and reads as "search is broken".
        let index = Index::new(vec![doc("a", "read", ""), doc("b", "read", "")]);
        assert!(!index.search("read", 1).is_empty());
    }

    #[test]
    fn a_catalog_query_drops_single_character_ascii() {
        // `a` matches nearly every definition and prefix-expands to every term starting with it.
        assert_eq!(catalog_terms("a read_file"), ["read", "file"]);
        // A single CJK character is a whole word and stays.
        assert!(catalog_terms("读").contains(&"读".to_owned()));
    }
}
