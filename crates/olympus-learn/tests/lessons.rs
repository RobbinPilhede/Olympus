mod common;

use std::fs;

use common::{active, candidate, fixture, run_id_with_holdout};
use olympus_learn::{fnv1a64, LessonKind, LessonState, NewLesson, RunRecord, Store};

const PROMPT: &str = "fix the tokenizer";

fn run(id: &str, outcome: bool) -> RunRecord {
    RunRecord {
        run_id: id.to_owned(),
        task_class: "debug".to_owned(),
        repo: "repo".to_owned(),
        model: "m".to_owned(),
        provider: "claude".to_owned(),
        outcome: Some(outcome),
        ..RunRecord::default()
    }
}

fn state_of(store: &Store, id: i64) -> LessonState {
    store
        .lessons(None, 1000)
        .expect("lessons")
        .into_iter()
        .find(|l| l.id == id)
        .expect("lesson exists")
        .state
}

fn visible(store: &Store, prompt: &str) -> olympus_learn::Injection {
    let (id, _) = run_id_with_holdout(store, false, "visible", 0);
    store
        .select_injection(&id, prompt, None, 400)
        .expect("select")
}

#[test]
fn store_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Store>();
}

#[test]
fn candidates_are_never_injected_until_approved() {
    let f = fixture();
    let id = f
        .store
        .add_lesson(candidate("tokenizer rejects tabs"))
        .expect("add");
    assert!(visible(&f.store, PROMPT).lessons.is_empty());
    assert!(f
        .store
        .recall("tokenizer", None, 5)
        .expect("recall")
        .is_empty());

    f.store.approve_lesson(id).expect("approve");

    let got = visible(&f.store, PROMPT);
    assert_eq!(got.lessons.len(), 1);
    assert_eq!(got.text, "- tokenizer rejects tabs");
}

#[test]
fn rejected_lessons_stop_being_injected() {
    let f = fixture();
    let id = f
        .store
        .add_lesson(active("tokenizer rejects tabs", LessonKind::Fact))
        .expect("add");
    f.store.reject_lesson(id).expect("reject");
    assert!(visible(&f.store, PROMPT).lessons.is_empty());
    assert!(f.store.approve_lesson(99_999).is_err());
}

#[test]
fn adding_the_same_lesson_twice_returns_the_same_id() {
    let f = fixture();
    let first = f
        .store
        .add_lesson(active("Same text", LessonKind::Fact))
        .expect("add");
    let second = f
        .store
        .add_lesson(active(" same TEXT ", LessonKind::Fact))
        .expect("add");
    assert_eq!(first, second);
}

#[test]
fn holdout_is_deterministic_and_near_twenty_percent() {
    let f = fixture();
    let total = 2000;
    let mut held = 0;
    for n in 0..total {
        let id = format!("run-{n}");
        let first = f
            .store
            .select_injection(&id, "", None, 400)
            .expect("select");
        let second = f
            .store
            .select_injection(&id, "", None, 400)
            .expect("select");
        assert_eq!(first.holdout, second.holdout);
        held += usize::from(first.holdout);
    }
    assert!((340..=460).contains(&held), "held out {held} of {total}");

    let other_store = fixture();
    for n in 0..50 {
        let id = format!("run-{n}");
        let a = f.store.select_injection(&id, "", None, 400).expect("a");
        let b = other_store
            .store
            .select_injection(&id, "", None, 400)
            .expect("b");
        assert_eq!(a.holdout, b.holdout, "hash must not depend on store state");
    }
}

#[test]
fn holdout_runs_get_no_memory_and_do_not_touch_lesson_evidence() {
    let f = fixture();
    let id = f
        .store
        .add_lesson(active("tokenizer rejects tabs", LessonKind::Fact))
        .expect("add");
    let (run_id, _) = run_id_with_holdout(&f.store, true, "held", 0);

    let got = f
        .store
        .select_injection(&run_id, PROMPT, None, 400)
        .expect("select");
    assert!(got.holdout);
    assert!(got.lessons.is_empty());
    assert!(got.text.is_empty());

    f.store.record_run(&run(&run_id, false)).expect("record");
    let lesson = &f.store.lessons(None, 10).expect("lessons")[0];
    assert_eq!(lesson.id, id);
    assert!((lesson.alpha - 1.0).abs() < 1e-9 && (lesson.beta - 1.0).abs() < 1e-9);

    let report = f.store.holdout_report().expect("report");
    assert_eq!(report.holdout_runs, 1);
    assert_eq!(report.with_memory_runs, 0);
}

#[test]
fn holdout_report_compares_arms_and_excludes_ineligible_runs() {
    let f = fixture();
    f.store
        .add_lesson(active("tokenizer rejects tabs", LessonKind::Fact))
        .expect("add");
    let (with_a, n) = run_id_with_holdout(&f.store, false, "arm", 0);
    let (with_b, n) = run_id_with_holdout(&f.store, false, "arm", n);
    let (held, n) = run_id_with_holdout(&f.store, true, "arm", n);
    let (no_match, _) = run_id_with_holdout(&f.store, false, "arm", n);
    for id in [&with_a, &with_b, &held] {
        f.store
            .select_injection(id, PROMPT, None, 400)
            .expect("select");
    }
    f.store
        .select_injection(&no_match, "unrelated", None, 400)
        .expect("select");

    let tokens = |id: &str, success: bool, output: u64| RunRecord {
        output_tokens: output,
        ..run(id, success)
    };
    f.store
        .record_run(&tokens(&with_a, true, 100))
        .expect("record");
    f.store
        .record_run(&tokens(&with_b, false, 300))
        .expect("record");
    f.store
        .record_run(&tokens(&held, true, 50))
        .expect("record");
    f.store
        .record_run(&tokens(&no_match, true, 999_999))
        .expect("record");

    let report = f.store.holdout_report().expect("report");
    assert_eq!(report.with_memory_runs, 2);
    assert!((report.with_memory_success_rate - 0.5).abs() < 1e-9);
    assert!((report.with_memory_mean_weighted_tokens - 1000.0).abs() < 1e-9);
    assert_eq!(report.holdout_runs, 1);
    assert!((report.holdout_success_rate - 1.0).abs() < 1e-9);
    assert!((report.holdout_mean_weighted_tokens - 250.0).abs() < 1e-9);
}

#[test]
fn injection_respects_token_budget_and_lesson_count() {
    let f = fixture();
    for n in 0..12 {
        let text = format!("tokenizer note {n} {}", "padding words here ".repeat(30));
        f.store
            .add_lesson(active(&text, LessonKind::Fact))
            .expect("add");
    }

    let wide = visible(&f.store, "tokenizer");
    assert!(wide.lessons.len() <= 5, "got {}", wide.lessons.len());
    assert!(wide.text.chars().count().div_ceil(4) <= 400);
    for line in wide.text.lines() {
        assert!(
            line.chars().count().div_ceil(4) <= 62,
            "bullet too long: {line}"
        );
    }

    let narrow = visible_with_budget(&f.store, "tokenizer", 70);
    assert_eq!(narrow.lessons.len(), 1);
    assert!(narrow.text.chars().count().div_ceil(4) <= 70);

    let none = visible_with_budget(&f.store, "tokenizer", 0);
    assert!(none.lessons.is_empty() && none.text.is_empty());

    let over = visible_with_budget(&f.store, "tokenizer", 100_000);
    assert!(over.text.chars().count().div_ceil(4) <= 400);
}

fn visible_with_budget(store: &Store, prompt: &str, budget: usize) -> olympus_learn::Injection {
    let (id, _) = run_id_with_holdout(store, false, &format!("budget{budget}"), 0);
    store
        .select_injection(&id, prompt, None, budget)
        .expect("select")
}

#[test]
fn one_slot_is_reserved_for_a_constraint() {
    let f = fixture();
    for n in 0..8 {
        let text = format!("tokenizer tokenizer tokenizer strong fact {n}");
        f.store
            .add_lesson(active(&text, LessonKind::Fact))
            .expect("add");
    }
    let weak = "never touch the generated tables that sit beside the tokenizer module in this repo";
    let constraint = f
        .store
        .add_lesson(active(weak, LessonKind::Constraint))
        .expect("add");

    let got = visible(&f.store, "tokenizer");
    assert_eq!(got.lessons.len(), 5);
    assert!(got.lessons.iter().any(|l| l.id == constraint));
    let others = got
        .lessons
        .iter()
        .filter(|l| l.kind != LessonKind::Constraint);
    assert_eq!(others.count(), 4);

    f.store.reject_lesson(constraint).expect("reject");
    let without = visible(&f.store, "tokenizer");
    assert_eq!(without.lessons.len(), 4, "the slot stays reserved");
}

#[test]
fn changed_or_vanished_provenance_drops_and_marks_stale() {
    let f = fixture();
    let repo = f.dir.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("mkdir");
    fs::write(repo.join("src/a.rs"), "one").expect("write");
    fs::write(repo.join("src/b.rs"), "two").expect("write");
    let with_file = |text: &str, file: &str, content: &[u8]| NewLesson {
        provenance_file: Some(file.to_owned()),
        provenance_hash: Some(fnv1a64(content)),
        ..active(text, LessonKind::Fact)
    };
    let a = f
        .store
        .add_lesson(with_file("tokenizer lives in a", "src/a.rs", b"one"))
        .expect("add");
    let b = f
        .store
        .add_lesson(with_file("tokenizer lives in b", "src/b.rs", b"two"))
        .expect("add");

    let inject = |n: usize| {
        let (id, _) = run_id_with_holdout(&f.store, false, &format!("stale{n}"), 0);
        f.store
            .select_injection(&id, "tokenizer", Some(&repo), 400)
            .expect("select")
    };
    assert_eq!(inject(0).lessons.len(), 2);

    fs::write(repo.join("src/a.rs"), "changed").expect("rewrite");
    let got = inject(1);
    assert_eq!(
        got.lessons.iter().map(|l| l.id).collect::<Vec<_>>(),
        vec![b]
    );
    assert_eq!(state_of(&f.store, a), LessonState::Stale);

    fs::remove_file(repo.join("src/b.rs")).expect("remove");
    assert!(inject(2).lessons.is_empty());
    assert_eq!(state_of(&f.store, b), LessonState::Stale);
}

#[test]
fn provenance_without_a_repo_is_dropped_but_not_marked_stale() {
    let f = fixture();
    let id = f
        .store
        .add_lesson(NewLesson {
            provenance_file: Some("a.rs".to_owned()),
            provenance_hash: Some(1),
            ..active("tokenizer lives in a", LessonKind::Fact)
        })
        .expect("add");
    assert!(visible(&f.store, "tokenizer").lessons.is_empty());
    assert_eq!(state_of(&f.store, id), LessonState::Active);
}

#[test]
fn provenance_paths_cannot_escape_the_repo() {
    let f = fixture();
    let repo = f.dir.path().join("repo");
    fs::create_dir_all(&repo).expect("mkdir");
    fs::write(f.dir.path().join("outside.txt"), "x").expect("write");
    let id = f
        .store
        .add_lesson(NewLesson {
            provenance_file: Some("../outside.txt".to_owned()),
            provenance_hash: Some(fnv1a64(b"x")),
            ..active("tokenizer outside note", LessonKind::Fact)
        })
        .expect("add");
    assert!(f
        .store
        .recall("tokenizer", Some(&repo), 5)
        .expect("recall")
        .is_empty());
    assert_eq!(state_of(&f.store, id), LessonState::Stale);
}

#[test]
fn scoped_lessons_only_apply_inside_their_repo() {
    let f = fixture();
    let repo = f.dir.path().join("repo");
    f.store
        .add_lesson(NewLesson {
            scope: Some(repo.to_string_lossy().into_owned()),
            ..active("tokenizer scoped note", LessonKind::Fact)
        })
        .expect("add");
    let elsewhere = f.dir.path().join("repo-two");
    assert!(f
        .store
        .recall("tokenizer", Some(&elsewhere), 5)
        .expect("recall")
        .is_empty());
    assert!(f
        .store
        .recall("tokenizer", None, 5)
        .expect("recall")
        .is_empty());
    assert_eq!(
        f.store
            .recall("tokenizer", Some(&repo), 5)
            .expect("recall")
            .len(),
        1
    );
    let nested = repo.join("crates").join("x");
    assert_eq!(
        f.store
            .recall("tokenizer", Some(&nested), 5)
            .expect("recall")
            .len(),
        1
    );
}

#[test]
fn long_prompts_need_two_matching_terms() {
    let f = fixture();
    f.store
        .add_lesson(active("tokenizer rejects tabs", LessonKind::Fact))
        .expect("add");
    let long = "please update tokenizer readme wording section";
    assert!(visible(&f.store, long).lessons.is_empty());
    let both = "please update tokenizer tabs wording section";
    assert_eq!(visible(&f.store, both).lessons.len(), 1);
}

#[test]
fn score_decays_by_half_after_one_half_life() {
    let f = fixture();
    let fact = f
        .store
        .add_lesson(active("alpha fact note", LessonKind::Fact))
        .expect("add");
    let pref = f
        .store
        .add_lesson(active("gamma pref note", LessonKind::Preference))
        .expect("add");
    let score = |query: &str| f.store.recall(query, None, 1).expect("recall")[0].score;
    let fresh_fact = score("alpha");
    let fresh_pref = score("gamma");

    f.age_lesson(fact, 90);
    f.age_lesson(pref, 90);

    assert!((score("alpha") / fresh_fact - 0.5).abs() < 0.01);
    let expected = 0.5f64.powf(90.0 / 365.0);
    assert!((score("gamma") / fresh_pref - expected).abs() < 0.01);
}

#[test]
fn decayed_lessons_rank_below_fresh_ones_and_evidence_raises_score() {
    let f = fixture();
    let old = f
        .store
        .add_lesson(active("tokenizer old fact", LessonKind::Fact))
        .expect("add");
    let new = f
        .store
        .add_lesson(active("tokenizer new fact", LessonKind::Fact))
        .expect("add");
    f.age_lesson(old, 400);
    let ranked = f.store.recall("tokenizer", None, 2).expect("recall");
    assert_eq!(ranked[0].id, new);

    let before = ranked[0].score;
    let mut n = 0;
    for i in 0..3 {
        let (id, next) = run_id_with_holdout(&f.store, false, &format!("evidence{i}"), n);
        n = next;
        f.store
            .select_injection(&id, "tokenizer", None, 400)
            .expect("select");
        f.store.record_run(&run(&id, true)).expect("record");
    }
    let after = f.store.recall("tokenizer", None, 2).expect("recall");
    let updated = after.iter().find(|l| l.id == new).expect("still active");
    assert!(updated.score > before, "{} > {before}", updated.score);
    assert!(updated.alpha > 3.9);
}

#[test]
fn lesson_retires_after_repeated_failures_and_not_before_five() {
    let f = fixture();
    let id = f
        .store
        .add_lesson(active("tokenizer bad advice", LessonKind::Fact))
        .expect("add");
    let mut n = 0;
    for attempt in 1..=5 {
        assert_eq!(
            state_of(&f.store, id),
            LessonState::Active,
            "attempt {attempt}"
        );
        let (run_id, next) = run_id_with_holdout(&f.store, false, "retire", n);
        n = next;
        let got = f
            .store
            .select_injection(&run_id, "tokenizer", None, 400)
            .expect("select");
        assert_eq!(got.lessons.len(), 1);
        f.store.record_run(&run(&run_id, false)).expect("record");
    }
    assert_eq!(state_of(&f.store, id), LessonState::Retired);
    assert!(visible(&f.store, "tokenizer").lessons.is_empty());
}

#[test]
fn a_user_correction_counts_against_a_lesson_even_on_success() {
    let f = fixture();
    let id = f
        .store
        .add_lesson(active("tokenizer advice", LessonKind::Fact))
        .expect("add");
    let (run_id, _) = run_id_with_holdout(&f.store, false, "corr", 0);
    f.store
        .select_injection(&run_id, "tokenizer", None, 400)
        .expect("select");
    f.store
        .record_run(&RunRecord {
            corrected: true,
            ..run(&run_id, true)
        })
        .expect("record");
    let lesson = f.store.recall("tokenizer", None, 1).expect("recall");
    assert_eq!(lesson[0].id, id);
    assert!((lesson[0].beta - 2.0).abs() < 1e-9);
}

#[test]
fn outcome_applies_once_across_repeated_records() {
    let f = fixture();
    f.store
        .add_lesson(active("tokenizer advice", LessonKind::Fact))
        .expect("add");
    let (run_id, _) = run_id_with_holdout(&f.store, false, "once", 0);
    f.store
        .select_injection(&run_id, "tokenizer", None, 400)
        .expect("select");

    f.store
        .record_run(&RunRecord {
            outcome: None,
            ..run(&run_id, true)
        })
        .expect("pending");
    f.store.record_run(&run(&run_id, true)).expect("first");
    f.store.record_run(&run(&run_id, true)).expect("second");

    let lesson = &f.store.recall("tokenizer", None, 1).expect("recall")[0];
    assert!((lesson.alpha - 2.0).abs() < 1e-9, "alpha {}", lesson.alpha);
    assert_eq!(f.store.stats().expect("stats").runs, 1);
}

#[test]
fn holdout_failures_promote_a_candidate_but_successes_do_not() {
    let f = fixture();
    let useful = f
        .store
        .add_lesson(candidate("tokenizer crashes on tabs"))
        .expect("add");
    let useless = f
        .store
        .add_lesson(candidate("parser hates semicolons"))
        .expect("add");
    let mut n = 0;
    for _ in 0..6 {
        let (id, next) = run_id_with_holdout(&f.store, true, "promote", n);
        n = next;
        f.store
            .select_injection(&id, "tokenizer", None, 400)
            .expect("select");
        f.store.record_run(&run(&id, false)).expect("record");

        let (id, next) = run_id_with_holdout(&f.store, true, "promote", n);
        n = next;
        f.store
            .select_injection(&id, "parser", None, 400)
            .expect("select");
        f.store.record_run(&run(&id, true)).expect("record");
    }
    assert_eq!(state_of(&f.store, useful), LessonState::Active);
    assert_eq!(state_of(&f.store, useless), LessonState::Candidate);
    assert_eq!(visible(&f.store, "tokenizer").lessons.len(), 1);
}

#[test]
fn candidate_evidence_is_not_collected_from_runs_with_memory() {
    let f = fixture();
    let id = f
        .store
        .add_lesson(candidate("tokenizer crashes on tabs"))
        .expect("add");
    let mut n = 0;
    for _ in 0..8 {
        let (run_id, next) = run_id_with_holdout(&f.store, false, "memory", n);
        n = next;
        f.store
            .select_injection(&run_id, "tokenizer", None, 400)
            .expect("select");
        f.store.record_run(&run(&run_id, false)).expect("record");
    }
    assert_eq!(state_of(&f.store, id), LessonState::Candidate);
}

#[test]
fn reopening_keeps_data_and_rejects_newer_schema() {
    let f = fixture();
    let path = f.db_path();
    let id = f
        .store
        .add_lesson(active("durable note", LessonKind::Fact))
        .expect("add");
    let reopened = Store::open(&path).expect("reopen");
    assert_eq!(reopened.lessons(None, 10).expect("lessons")[0].id, id);

    f.raw()
        .execute_batch("PRAGMA user_version = 999")
        .expect("bump");
    assert!(Store::open(&path).is_err());
}

#[test]
fn fts_syntax_in_prompts_is_treated_as_plain_text() {
    let f = fixture();
    f.store
        .add_lesson(active("tokenizer quoting note", LessonKind::Fact))
        .expect("add");
    for hostile in [
        "tokenizer\" OR \"",
        "NEAR(tokenizer",
        "tokenizer*",
        "text:tokenizer",
        "\"'^-",
    ] {
        f.store
            .recall(hostile, None, 5)
            .expect("recall must not error");
        f.store
            .search_history(hostile, 5)
            .expect("search must not error");
    }
}
