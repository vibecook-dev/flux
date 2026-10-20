//! Offline evaluation of the export prefilter + matcher against labelled titles.
//!
//! Used by the `meta_bench` example and the integration tests. The labelled
//! set is `tests/fixtures/real_movies_truth.json`: the `REAL_MOVIES` list of
//! `tools/corpus/gen-library.py` resolved to TMDB ids through Wikidata (CC0,
//! property P4947), see `tools/wikidata_truth.py`.
//!
//! "Parser-free" means the query is the ground-truth title and year, so the
//! numbers measure the offline index and scorer, not the filename parser.

use serde::{Deserialize, Serialize};

use crate::export_index::ExportIndex;
use crate::matcher::{Candidate, Decision, MatchQuery, Matcher, MediaKind};
use crate::normalize::normalize_title;

/// One labelled title.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TruthRow {
    pub title: String,
    pub year: u16,
    /// `None` when the title is not a TMDB *movie* (excluded from accuracy).
    pub tmdb_id: Option<u32>,
}

/// Why a title was not matched top-1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// TMDB's `original_title` is in another language/spelling than the query
    /// (*Parasite* → 기생충, *Run Lola Run* → Lola rennt). The export has no
    /// translations, so only `/search` (or Wikidata aliases) can fix it.
    TranslatedTitle,
    /// Same normalised title, another film won on popularity: remakes and
    /// homonyms (*King Kong* 1933/1976/2005). The export has no year.
    SameTitleOtherFilm,
    /// The original title differs only slightly (punctuation, subtitle) and a
    /// different title scored higher.
    NearTitle,
    /// The right id was not among the candidates at all.
    NotInCandidates,
}

/// One evaluated title.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalCase {
    pub title: String,
    pub year: u16,
    pub truth: u32,
    pub truth_original_title: Option<String>,
    pub predicted: Option<u32>,
    pub predicted_title: Option<String>,
    pub rank_of_truth: Option<usize>,
    pub correct: bool,
    /// The matcher's verdict and confidence for its top candidate.
    pub decision: Decision,
    pub confidence: f32,
    pub failure: Option<FailureClass>,
}

/// Aggregate results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalReport {
    pub evaluated: usize,
    pub excluded: usize,
    pub top1: usize,
    pub top1_accuracy: f64,
    /// The truth id is somewhere in the top-`candidate_limit` list.
    pub recall_at_k: f64,
    pub candidate_limit: usize,
    pub failures_by_class: Vec<(FailureClass, usize)>,
    /// Auto-accepted cases, and how many of those were wrong (precision guard).
    pub auto_accepted: usize,
    pub wrong_auto_accepted: usize,
    pub cases: Vec<EvalCase>,
}

/// Parse the truth fixture.
pub fn load_truth(json: &str) -> Result<Vec<TruthRow>, serde_json::Error> {
    serde_json::from_str(json)
}

/// Evaluate `rows` against a movie export index with the default matcher.
pub fn evaluate_movies(
    index: &ExportIndex,
    matcher: &Matcher,
    rows: &[TruthRow],
    candidate_limit: usize,
) -> EvalReport {
    let mut cases = Vec::new();
    let mut excluded = 0;
    for row in rows {
        let Some(truth) = row.tmdb_id else {
            excluded += 1;
            continue;
        };
        let export = index.candidates(&row.title, candidate_limit);
        let candidates: Vec<Candidate> = export.iter().map(Candidate::from_export).collect();
        let query = MatchQuery {
            kind: Some(MediaKind::Movie),
            titles: vec![row.title.clone()],
            year: Some(row.year),
            ..MatchQuery::default()
        };
        let result = matcher.match_candidates(&query, &candidates);
        let predicted = result.ids.tmdb.and_then(|i| u32::try_from(i).ok());
        let rank_of_truth = export.iter().position(|c| c.id == truth).map(|p| p + 1);
        let truth_original_title = index.title_of(truth).map(str::to_owned);
        let correct = predicted == Some(truth);
        let failure = (!correct).then(|| {
            let q = normalize_title(&row.title);
            match (&truth_original_title, rank_of_truth) {
                (Some(t), _) if *t == q => FailureClass::SameTitleOtherFilm,
                (_, None) => match &truth_original_title {
                    Some(t) if crate::normalize::title_similarity(&q, t) < 0.70 => {
                        FailureClass::TranslatedTitle
                    }
                    _ => FailureClass::NotInCandidates,
                },
                _ => FailureClass::NearTitle,
            }
        });
        cases.push(EvalCase {
            title: row.title.clone(),
            year: row.year,
            truth,
            truth_original_title,
            predicted,
            predicted_title: result.best().map(|b| b.title.clone()),
            rank_of_truth,
            correct,
            decision: result.decision,
            confidence: result.confidence,
            failure,
        });
    }
    let evaluated = cases.len();
    let top1 = cases.iter().filter(|c| c.correct).count();
    let in_k = cases.iter().filter(|c| c.rank_of_truth.is_some()).count();
    let mut by_class: Vec<(FailureClass, usize)> = Vec::new();
    for c in cases.iter().filter_map(|c| c.failure) {
        match by_class.iter_mut().find(|(k, _)| *k == c) {
            Some((_, n)) => *n += 1,
            None => by_class.push((c, 1)),
        }
    }
    by_class.sort();
    EvalReport {
        evaluated,
        excluded,
        top1,
        top1_accuracy: if evaluated == 0 {
            0.0
        } else {
            top1 as f64 / evaluated as f64
        },
        recall_at_k: if evaluated == 0 {
            0.0
        } else {
            in_k as f64 / evaluated as f64
        },
        candidate_limit,
        failures_by_class: by_class,
        auto_accepted: cases.iter().filter(|c| c.decision == Decision::Accept).count(),
        wrong_auto_accepted: cases
            .iter()
            .filter(|c| c.decision == Decision::Accept && !c.correct)
            .count(),
        cases,
    }
}
