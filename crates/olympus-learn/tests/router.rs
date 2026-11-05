mod common;

use std::collections::HashMap;

use common::fixture;
use olympus_learn::{RunRecord, Store};

const CLASS: &str = "debug";
const REPO: &str = "repo";

fn models(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

fn observe(store: &Store, id: &str, model: &str, success: bool, output_tokens: u64) {
    store
        .record_run(&RunRecord {
            run_id: id.to_owned(),
            task_class: CLASS.to_owned(),
            repo: REPO.to_owned(),
            model: model.to_owned(),
            provider: "claude".to_owned(),
            output_tokens,
            outcome: Some(success),
            ..RunRecord::default()
        })
        .expect("record");
}

fn tally(store: &Store, candidates: &[String], rounds: usize) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for _ in 0..rounds {
        let pick = store.route(CLASS, REPO, candidates).expect("route");
        *counts.entry(pick).or_insert(0) += 1;
    }
    counts
}

#[test]
fn empty_candidates_are_an_error() {
    let f = fixture();
    assert!(f.store.route(CLASS, REPO, &[]).is_err());
}

#[test]
fn unseen_model_is_explored_even_against_a_strong_incumbent() {
    let f = fixture();
    for n in 0..200 {
        observe(&f.store, &format!("strong-{n}"), "strong", true, 0);
    }
    let candidates = models(&["strong", "new"]);
    let counts = tally(&f.store, &candidates, 20);
    assert_eq!(counts.get("new"), Some(&20));

    observe(&f.store, "new-0", "new", true, 0);
    let after = tally(&f.store, &candidates, 200);
    assert!(after.get("strong").copied().unwrap_or(0) > 100, "{after:?}");
}

#[test]
fn every_unseen_model_gets_picked_eventually() {
    let f = fixture();
    let candidates = models(&["a", "b", "c"]);
    let counts = tally(&f.store, &candidates, 200);
    assert_eq!(counts.len(), 3, "{counts:?}");
}

#[test]
fn routing_converges_on_the_better_model() {
    let f = fixture();
    for n in 0..60 {
        observe(&f.store, &format!("good-{n}"), "good", n % 10 != 0, 0);
        observe(&f.store, &format!("bad-{n}"), "bad", n % 10 == 0, 0);
    }
    let counts = tally(&f.store, &models(&["good", "bad"]), 500);
    let good = counts.get("good").copied().unwrap_or(0);
    assert!(good >= 480, "picked good {good}/500");
}

#[test]
fn token_cost_breaks_ties_between_equally_reliable_models() {
    let f = fixture();
    for n in 0..80 {
        observe(&f.store, &format!("cheap-{n}"), "cheap", true, 1_000);
        // 5 x 1M output tokens: 5M weighted, a penalty of 0.25 per success.
        observe(&f.store, &format!("costly-{n}"), "costly", true, 1_000_000);
    }
    let counts = tally(&f.store, &models(&["cheap", "costly"]), 400);
    let cheap = counts.get("cheap").copied().unwrap_or(0);
    assert!(cheap >= 380, "picked cheap {cheap}/400");
}

#[test]
fn routing_stats_are_isolated_per_class_and_repo() {
    let f = fixture();
    for n in 0..60 {
        observe(&f.store, &format!("known-{n}"), "known", true, 0);
    }
    let candidates = models(&["known", "other"]);
    let mut other_repo = HashMap::new();
    for _ in 0..200 {
        let pick = f
            .store
            .route(CLASS, "another-repo", &candidates)
            .expect("route");
        *other_repo.entry(pick).or_insert(0) += 1;
    }
    assert_eq!(other_repo.len(), 2, "both unseen there: {other_repo:?}");
}

#[test]
fn route_ignores_runs_without_outcome_and_repeat_records() {
    let f = fixture();
    let pending = RunRecord {
        run_id: "p".to_owned(),
        task_class: CLASS.to_owned(),
        repo: REPO.to_owned(),
        model: "a".to_owned(),
        ..RunRecord::default()
    };
    f.store.record_run(&pending).expect("pending");
    let counts = tally(&f.store, &models(&["a", "b"]), 100);
    assert_eq!(counts.len(), 2, "pending run is not data: {counts:?}");

    let raw = f.raw();
    observe(&f.store, "x", "a", true, 0);
    observe(&f.store, "x", "a", true, 0);
    let alpha: f64 = raw
        .query_row("SELECT alpha FROM route_stats WHERE model = 'a'", [], |r| {
            r.get(0)
        })
        .expect("stats");
    assert!((alpha - 2.0).abs() < 1e-9, "alpha {alpha}");
}

#[test]
fn stats_separate_recorded_run_tokens_by_model() {
    let f = fixture();
    observe(&f.store, "s1", "a", true, 10);
    observe(&f.store, "s2", "a", true, 20);
    let stats = f.store.stats().expect("stats");
    assert_eq!(stats.runs, 2);
    assert_eq!(stats.run_per_model.len(), 1);
    assert_eq!(stats.run_per_model[0].output_tokens, 30);
    assert!((stats.run_per_model[0].weighted_tokens - 150.0).abs() < 1e-9);
    assert!(stats.per_model.is_empty());
}
