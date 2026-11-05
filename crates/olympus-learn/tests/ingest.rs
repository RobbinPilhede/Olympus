mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{append_raw, fixture, write_lines, Fixture};
use olympus_learn::{IngestProgress, LessonKind, LessonState};
use serde_json::{json, Value};

const CWD: &str = "C:/work/app";

fn base(kind: &str, uuid: &str, session: &str) -> Value {
    json!({
        "type": kind, "uuid": uuid, "parentUuid": null, "isSidechain": false,
        "sessionId": session, "timestamp": "2026-10-01T10:00:00.000Z", "cwd": CWD,
        "gitBranch": "main", "version": "2.0.0"
    })
}

fn user_text(uuid: &str, session: &str, text: &str) -> String {
    let mut line = base("user", uuid, session);
    line["message"] = json!({"role": "user", "content": text});
    line.to_string()
}

fn assistant_usage(uuid: &str, request: &str, model: &str, input: u64, output: u64) -> String {
    let mut line = base("assistant", uuid, "s1");
    line["requestId"] = json!(request);
    line["message"] = json!({
        "model": model, "stop_reason": "tool_use", "content": [],
        "usage": {
            "input_tokens": input, "cache_read_input_tokens": 1000,
            "cache_creation_input_tokens": 100, "output_tokens": output
        }
    });
    line.to_string()
}

fn tool_use(uuid: &str, session: &str, id: &str, name: &str, input: Value) -> String {
    let mut line = base("assistant", uuid, session);
    line["requestId"] = json!(format!("req-{uuid}"));
    line["message"] = json!({
        "model": "claude-x", "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]
    });
    line.to_string()
}

fn tool_result(uuid: &str, session: &str, id: &str, is_error: bool, text: &str) -> String {
    let mut line = base("user", uuid, session);
    line["message"] = json!({
        "role": "user",
        "content": [{"type": "tool_result", "tool_use_id": id, "is_error": is_error, "content": text}]
    });
    line["toolUseResult"] = json!({"stdout": "", "stderr": text, "interrupted": false});
    line.to_string()
}

fn session_path(root: &Path, session: &str) -> PathBuf {
    root.join("projects")
        .join("C--work-app")
        .join(format!("{session}.jsonl"))
}

fn signal_count(f: &Fixture, kind: &str) -> u64 {
    let stats = f.store.stats().expect("stats");
    stats
        .signal_counts
        .iter()
        .find(|s| s.kind == kind)
        .map_or(0, |s| s.count)
}

fn usage_row(f: &Fixture, model: &str) -> olympus_learn::ModelTokens {
    f.store
        .stats()
        .expect("stats")
        .per_model
        .into_iter()
        .find(|m| m.model == model)
        .expect("model row")
}

#[test]
fn incremental_ingest_resumes_past_a_torn_last_line() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    let path = session_path(&root, "s1");
    write_lines(
        &path,
        &[user_text("u1", "s1", "first prompt about walruses")],
    );
    let torn = user_text("u2", "s1", "second prompt about narwhals");
    let (head, tail) = torn.split_at(torn.len() / 2);
    append_raw(&path, head);

    let first = f.ingest_claude(&root);
    assert_eq!(first.files_updated, 1);
    assert_eq!(first.lines, 1);
    assert_eq!(first.prompts, 1);
    assert!(f
        .store
        .search_history("narwhals", 5)
        .expect("search")
        .is_empty());

    append_raw(&path, tail);
    append_raw(&path, "\n");
    append_raw(
        &path,
        &format!("{}\n", user_text("u3", "s1", "third prompt about otters")),
    );

    let second = f.ingest_claude(&root);
    assert_eq!(second.lines, 2, "resumes at the torn line, not after it");
    assert_eq!(second.prompts, 2);
    assert_eq!(
        f.store.search_history("narwhals", 5).expect("search").len(),
        1
    );
    assert_eq!(
        f.store.search_history("walruses", 5).expect("search").len(),
        1
    );
    assert_eq!(f.store.stats().expect("stats").prompts, 3);
}

#[test]
fn second_pass_over_unchanged_files_reads_nothing() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    write_lines(
        &session_path(&root, "s1"),
        &[user_text("u1", "s1", "hello walrus")],
    );
    f.ingest_claude(&root);

    let again = f.ingest_claude(&root);
    assert_eq!(again.files_scanned, 1);
    assert_eq!(again.files_updated, 0);
    assert_eq!(again.bytes, 0);
    assert_eq!(again.lines, 0);
}

#[test]
fn rewritten_smaller_file_is_read_from_the_start() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    let path = session_path(&root, "s1");
    write_lines(
        &path,
        &[
            user_text("u1", "s1", "a long opening prompt about walruses and more"),
            user_text("u2", "s1", "another long prompt about walruses and more"),
        ],
    );
    f.ingest_claude(&root);

    write_lines(&path, &[user_text("u9", "s1", "tiny otter")]);
    let again = f.ingest_claude(&root);
    assert_eq!(again.lines, 1);
    assert_eq!(f.store.search_history("otter", 5).expect("search").len(), 1);
}

#[test]
fn usage_is_deduped_by_request_across_lines_and_files() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    write_lines(
        &session_path(&root, "s1"),
        &[
            assistant_usage("a1", "req-1", "claude-x", 10, 5),
            assistant_usage("a2", "req-1", "claude-x", 10, 40),
            assistant_usage("a3", "req-1", "claude-x", 10, 20),
            assistant_usage("a4", "req-2", "claude-x", 7, 3),
            assistant_usage("a5", "req-3", "<synthetic>", 7, 3),
        ],
    );
    // A resumed session copies earlier lines into a new file.
    write_lines(
        &session_path(&root, "s2"),
        &[assistant_usage("a1", "req-1", "claude-x", 10, 40)],
    );
    f.ingest_claude(&root);

    let row = usage_row(&f, "claude-x");
    assert_eq!(row.input_tokens, 17);
    assert_eq!(row.output_tokens, 43);
    assert_eq!(row.cache_read, 2000);
    assert_eq!(row.cache_write, 200);
    let expected = 17.0 + 200.0 + 250.0 + 215.0;
    assert!((row.weighted_tokens - expected).abs() < 1e-9);
    assert_eq!(f.store.stats().expect("stats").per_model.len(), 1);
}

#[test]
fn history_search_finds_prompts_but_not_wrappers_or_sidechains() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    let mut meta = base("user", "m1", "s1");
    meta["isMeta"] = json!(true);
    meta["message"] = json!({"role": "user", "content": "metabanana"});
    let mut summary = base("user", "c1", "s1");
    summary["isCompactSummary"] = json!(true);
    summary["message"] = json!({"role": "user", "content": "compactbanana"});
    let mut notified = base("user", "n1", "s1");
    notified["origin"] = json!({"kind": "task-notification"});
    notified["message"] = json!({"role": "user", "content": "notifybanana"});
    let mut block = base("user", "b1", "s1");
    block["message"] = json!({"role": "user", "content": [
        {"type": "text", "text": "<system-reminder>reminderbanana</system-reminder>real blockbanana"}
    ]});
    write_lines(
        &session_path(&root, "s1"),
        &[
            user_text("u1", "s1", "<system-reminder>onlybanana</system-reminder>"),
            user_text("u2", "s1", "<command-name>/commandbanana</command-name>"),
            user_text("u3", "s1", "real plainbanana question"),
            meta.to_string(),
            summary.to_string(),
            notified.to_string(),
            block.to_string(),
        ],
    );
    let agent = root
        .join("projects/C--work-app/s1/subagents/workflows/wf_1")
        .join("agent-abc.jsonl");
    write_lines(
        &agent,
        &[user_text("x1", "s1", "agentbanana delegated prompt")],
    );
    fs::create_dir_all(root.join("projects/C--work-app/s1/tool-results")).expect("mkdir");
    fs::write(
        root.join("projects/C--work-app/s1/tool-results/other.jsonl"),
        format!("{}\n", user_text("x2", "s1", "toolresultbanana")),
    )
    .expect("write");

    let stats = f.ingest_claude(&root);
    assert_eq!(stats.files_scanned, 2);

    let hits = f.store.search_history("plainbanana", 5).expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session, "s1");
    assert_eq!(hits[0].cwd, CWD);
    assert_eq!(hits[0].timestamp, "2026-10-01T10:00:00.000Z");
    assert!(
        hits[0].snippet.contains("[plainbanana]"),
        "{}",
        hits[0].snippet
    );

    let kept = f.store.search_history("blockbanana", 5).expect("search");
    assert_eq!(kept[0].text, "real blockbanana");

    for absent in [
        "onlybanana",
        "commandbanana",
        "metabanana",
        "compactbanana",
        "notifybanana",
        "reminderbanana",
        "agentbanana",
        "toolresultbanana",
    ] {
        assert!(
            f.store
                .search_history(absent, 5)
                .expect("search")
                .is_empty(),
            "{absent}"
        );
    }
}

#[test]
fn prompts_are_truncated_and_scrubbed_of_credentials() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    let long = format!("zebra {} sk-supersecretvalue", "å".repeat(5000));
    let secret = "deploy with ghp_abcdefghijklmnop please";
    write_lines(
        &session_path(&root, "s1"),
        &[user_text("u1", "s1", &long), user_text("u2", "s1", secret)],
    );
    f.ingest_claude(&root);

    let hits = f.store.search_history("zebra", 5).expect("search");
    assert_eq!(hits[0].text.chars().count(), 2000);
    let scrubbed = f.store.search_history("deploy", 5).expect("search");
    assert!(!scrubbed[0].text.contains("ghp_"), "{}", scrubbed[0].text);
}

#[test]
fn lines_over_the_limit_are_skipped_and_counted() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    let path = session_path(&root, "s1");
    let huge = user_text("u1", "s1", &format!("hugeword {}", "y ".repeat(2_500_000)));
    write_lines(&path, &[huge, user_text("u2", "s1", "after the huge line")]);

    let stats = f.ingest_claude(&root);
    assert_eq!(stats.skipped_large_lines, 1);
    assert_eq!(stats.lines, 1);
    assert!(f
        .store
        .search_history("hugeword", 5)
        .expect("search")
        .is_empty());
    assert_eq!(
        f.store
            .search_history("after huge", 5)
            .expect("search")
            .len(),
        1
    );
}

#[test]
fn unknown_and_malformed_lines_are_tolerated() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    let mut odd_result = base("user", "o1", "s1");
    odd_result["message"] = json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "nope", "content": 42}
    ]});
    odd_result["toolUseResult"] = json!("a bare string");
    write_lines(
        &session_path(&root, "s1"),
        &[
            "{not json".to_owned(),
            json!({"type": "brand-new-type", "whatever": [1, 2]}).to_string(),
            json!({"type": "user", "message": "unexpected shape"}).to_string(),
            "[1,2,3]".to_owned(),
            odd_result.to_string(),
            user_text("u1", "s1", "still parsed walrus"),
        ],
    );
    let stats = f.ingest_claude(&root);
    assert_eq!(stats.lines, 6);
    assert_eq!(stats.prompts, 1);
}

#[test]
fn error_then_fix_pairs_and_failing_commands_become_signals() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    let bash = |command: &str| json!({"command": command});
    write_lines(
        &session_path(&root, "s1"),
        &[
            tool_use("a1", "s1", "t1", "Bash", bash("cd x && cargo test --all")),
            tool_result(
                "r1",
                "s1",
                "t1",
                true,
                "Exit code 101\nerror[E0308]: mismatched types",
            ),
            tool_use(
                "a2",
                "s1",
                "t2",
                "Edit",
                json!({"file_path": "C:/work/app/src/lib.rs"}),
            ),
            tool_result("r2", "s1", "t2", false, "ok"),
            tool_use("a3", "s1", "t3", "Bash", bash("cargo test --all")),
            tool_result("r3", "s1", "t3", false, "test result: ok"),
            tool_use("a4", "s1", "t4", "Bash", bash("grep -r needle .")),
            tool_result("r4", "s1", "t4", true, "Exit code 1"),
            tool_use("a5", "s1", "t5", "Read", json!({"file_path": "missing"})),
            tool_result("r5", "s1", "t5", true, "file not found"),
        ],
    );
    f.ingest_claude(&root);

    assert_eq!(signal_count(&f, "failing_command"), 1);
    assert_eq!(signal_count(&f, "error_fix"), 1);
    assert_eq!(
        signal_count(&f, "tool_error"),
        2,
        "grep and Read are not command failures"
    );

    let raw = f.raw();
    let (key, detail): (String, String) = raw
        .query_row(
            "SELECT sig_key, detail FROM signals WHERE kind = 'error_fix'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("row");
    assert_eq!(key, "cargo test");
    assert!(
        detail.contains("E0308") && detail.contains("lib.rs"),
        "{detail}"
    );
}

#[test]
fn error_fix_pairing_survives_a_resume_between_failure_and_fix() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    let path = session_path(&root, "s1");
    write_lines(
        &path,
        &[
            tool_use(
                "a1",
                "s1",
                "t1",
                "Bash",
                json!({"command": "npm run build"}),
            ),
            tool_result("r1", "s1", "t1", true, "Exit code 1\nboom"),
            tool_use(
                "a2",
                "s1",
                "t2",
                "Bash",
                json!({"command": "npm run build"}),
            ),
        ],
    );
    f.ingest_claude(&root);
    assert_eq!(signal_count(&f, "error_fix"), 0);

    append_raw(
        &path,
        &format!("{}\n", tool_result("r2", "s1", "t2", false, "built")),
    );
    f.ingest_claude(&root);
    assert_eq!(signal_count(&f, "error_fix"), 1);
}

#[test]
fn denials_and_interruptions_are_detected() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    let mut denied = base("user", "r1", "s1");
    denied["message"] = json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "t1", "is_error": true, "content": "denied"}
    ]});
    denied["toolDenialKind"] = json!("permission-rule");
    let mut rejected = base("user", "r2", "s1");
    rejected["message"] = json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "t2", "content": "no"}
    ]});
    rejected["permissionDecision"] = json!({"decision": "reject", "source": "config"});
    let mut stopped = base("user", "r3", "s1");
    stopped["message"] = json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "t3", "content": "x"}
    ]});
    stopped["toolUseResult"] = json!({"interrupted": true});
    write_lines(
        &session_path(&root, "s1"),
        &[
            tool_use(
                "a1",
                "s1",
                "t1",
                "Bash",
                json!({"command": "git push origin main"}),
            ),
            denied.to_string(),
            tool_use("a2", "s1", "t2", "Bash", json!({"command": "git push"})),
            rejected.to_string(),
            tool_use("a3", "s1", "t3", "Bash", json!({"command": "cargo build"})),
            stopped.to_string(),
            user_text("u1", "s1", "[Request interrupted by user]"),
        ],
    );
    f.ingest_claude(&root);

    assert_eq!(signal_count(&f, "permission_denial"), 2);
    assert_eq!(signal_count(&f, "interrupted"), 2);
    assert_eq!(
        signal_count(&f, "failing_command"),
        0,
        "denials are not failures"
    );
    assert!(f
        .store
        .search_history("interrupted", 5)
        .expect("search")
        .is_empty());
}

#[test]
fn corrections_only_count_after_an_assistant_turn() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    write_lines(
        &session_path(&root, "s1"),
        &[
            user_text("u1", "s1", "don't touch the migrations when you start"),
            assistant_usage("a1", "req-1", "claude-x", 1, 1),
            user_text("u2", "s1", "no, that is wrong"),
            user_text("u3", "s1", "now add a settings page for the theme"),
        ],
    );
    f.ingest_claude(&root);
    assert_eq!(signal_count(&f, "correction"), 1);
}

#[test]
fn repeated_signals_derive_candidates_never_active_ones() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    for session in ["s1", "s2", "s3"] {
        let id = format!("t-{session}");
        let uuid = |suffix: &str| format!("{session}-{suffix}");
        write_lines(
            &session_path(&root, session),
            &[
                assistant_usage(&uuid("a0"), &uuid("req"), "claude-x", 1, 1),
                user_text(&uuid("u1"), session, "no, use pnpm for dependency installs"),
                tool_use(
                    &uuid("a1"),
                    session,
                    &id,
                    "Bash",
                    json!({"command": "npm run deploy"}),
                ),
                tool_result(
                    &uuid("r1"),
                    session,
                    &id,
                    true,
                    "Exit code 1\nENOENT missing script",
                ),
            ],
        );
    }

    let stats = f.ingest_claude(&root);
    assert_eq!(stats.candidates, 0, "failures and corrections stay signals, not lessons");
    assert!(stats.signals >= 3);
    assert!(f.store.lessons(None, 10).expect("lessons").is_empty());
}

#[test]
fn repeated_denials_become_candidates_never_active_lessons() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    for session in ["s1", "s2", "s3"] {
        let id = format!("t-{session}");
        let mut denied = serde_json::from_str::<Value>(&tool_result(&format!("{session}-r"), session, &id, true, "denied")).expect("json");
        denied["toolDenialKind"] = json!("user");
        write_lines(&session_path(&root, session), &[tool_use(&format!("{session}-a"), session, &id, "Bash", json!({"command": "terraform destroy"})), denied.to_string()]);
    }
    let stats = f.ingest_claude(&root);
    assert_eq!(stats.candidates, 1);
    let candidates = f.store.lessons(Some(LessonState::Candidate), 10).expect("lessons");
    assert_eq!(candidates[0].kind, LessonKind::Constraint);
    assert!(candidates[0].text.contains("terraform"));
    assert!(f.store.lessons(Some(LessonState::Active), 10).expect("lessons").is_empty());
}

#[test]
fn one_off_failures_do_not_become_candidates() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    write_lines(
        &session_path(&root, "s1"),
        &[
            tool_use(
                "a1",
                "s1",
                "t1",
                "Bash",
                json!({"command": "npm run deploy"}),
            ),
            tool_result("r1", "s1", "t1", true, "Exit code 1\nboom"),
        ],
    );
    assert_eq!(f.ingest_claude(&root).candidates, 0);
}

#[test]
fn progress_reports_every_file() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    for session in ["s1", "s2", "s3"] {
        write_lines(
            &session_path(&root, session),
            &[user_text(session, session, "hi there")],
        );
    }
    let mut seen: Vec<IngestProgress> = Vec::new();
    f.store
        .ingest(Some(&root), None, &mut |p| seen.push(p.clone()))
        .expect("ingest");
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[2].files_done, 3);
    assert_eq!(seen[2].files_total, 3);
    assert!(seen[2].current.ends_with("s3.jsonl"));
}

#[test]
fn missing_directories_are_empty_not_errors() {
    let f = fixture();
    let missing = f.dir.path().join("nope");
    let stats = f
        .store
        .ingest(Some(&missing), Some(&missing), &mut |_| {})
        .expect("ingest");
    assert_eq!(stats.files_scanned, 0);
}

fn codex_line(kind: &str, payload: Value) -> String {
    json!({"timestamp": "2026-10-19T18:00:00.000Z", "type": kind, "payload": payload}).to_string()
}

fn codex_meta(id: &str, subagent: bool) -> String {
    let source = if subagent {
        json!({"subagent": {"thread_spawn": {}}})
    } else {
        json!("vscode")
    };
    codex_line(
        "session_meta",
        json!({
            "id": id, "cwd": "C:/work/codex", "cli_version": "1.0", "source": source,
            "git": {"branch": "main"}
        }),
    )
}

fn codex_turn(model: &str) -> String {
    codex_line(
        "turn_context",
        json!({"model": model, "effort": "high", "cwd": "C:/work/codex"}),
    )
}

fn codex_tokens(input: u64, cached: u64, output: u64, used_percent: f64) -> String {
    codex_line(
        "event_msg",
        json!({
            "type": "token_count",
            "info": {"total_token_usage": {
                "input_tokens": input, "cached_input_tokens": cached,
                "cache_write_input_tokens": 0, "output_tokens": output,
                "total_tokens": input + output
            }},
            "rate_limits": {"primary": {
                "used_percent": used_percent, "window_minutes": 300, "resets_at": 1_800_000_000
            }}
        }),
    )
}

fn codex_user(id: &str, text: &str) -> String {
    codex_line(
        "response_item",
        json!({
            "type": "message", "id": id, "role": "user",
            "content": [{"type": "input_text", "text": text}]
        }),
    )
}

fn codex_assistant() -> String {
    codex_line(
        "response_item",
        json!({
            "type": "message", "id": "m-a", "role": "assistant",
            "content": [{"type": "output_text", "text": "done"}]
        }),
    )
}

fn codex_exec(call: &str, command: &str) -> String {
    let script = format!("const r = await tools.exec_command({{cmd:{command:?}}});\ntext(r);\n");
    codex_line(
        "response_item",
        json!({
            "type": "custom_tool_call", "call_id": call, "name": "exec", "input": script
        }),
    )
}

fn codex_output(call: &str, text: &str) -> String {
    codex_line(
        "response_item",
        json!({
            "type": "custom_tool_call_output", "call_id": call,
            "output": [{"type": "input_text", "text": text}]
        }),
    )
}

fn rollout(root: &Path, name: &str) -> PathBuf {
    root.join("sessions/2026/10/19")
        .join(format!("rollout-{name}.jsonl"))
}

#[test]
fn codex_tokens_are_deltas_of_cumulative_totals_split_by_model() {
    let f = fixture();
    let root = f.dir.path().join("codex");
    let path = rollout(&root, "a");
    write_lines(
        &path,
        &[
            codex_meta("thread-a", false),
            codex_turn("gpt-a"),
            codex_tokens(1000, 400, 50, 10.0),
            codex_tokens(1000, 400, 50, 11.0),
            codex_tokens(1500, 600, 80, 12.0),
            codex_turn("gpt-b"),
        ],
    );
    f.ingest_codex(&root);

    let a = usage_row(&f, "gpt-a");
    assert_eq!(a.provider, "codex");
    assert_eq!(a.cache_read, 600);
    assert_eq!(
        a.input_tokens, 900,
        "cached tokens are not billed as input too"
    );
    assert_eq!(a.output_tokens, 80);

    // The baseline and current model must survive the resume to avoid recounting.
    append_raw(&path, &format!("{}\n", codex_tokens(2000, 600, 100, 13.0)));
    f.ingest_codex(&root);
    let a = usage_row(&f, "gpt-a");
    assert_eq!(a.output_tokens, 80);
    let b = usage_row(&f, "gpt-b");
    assert_eq!(b.input_tokens, 500);
    assert_eq!(b.output_tokens, 20);

    let limits = f.store.stats().expect("stats").rate_limits;
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].provider, "codex");
    assert!((limits[0].used_percent - 13.0).abs() < 1e-9);
    assert_eq!(limits[0].window_minutes, 300);
    assert_eq!(limits[0].resets_at, 1_800_000_000);
}

#[test]
fn codex_prompts_skip_wrappers_and_subagent_threads() {
    let f = fixture();
    let root = f.dir.path().join("codex");
    write_lines(
        &rollout(&root, "main"),
        &[
            codex_meta("thread-main", false),
            codex_user(
                "m1",
                "<environment_context>wrapperkiwi</environment_context>",
            ),
            codex_user("m2", "plain realkiwi request"),
            codex_assistant(),
            codex_user("m3", "no, use the other kiwifix instead"),
        ],
    );
    write_lines(
        &rollout(&root, "child"),
        &[
            codex_meta("thread-child", true),
            codex_user("m9", "agentkiwi delegated work"),
        ],
    );
    f.ingest_codex(&root);

    assert_eq!(
        f.store.search_history("realkiwi", 5).expect("search").len(),
        1
    );
    assert!(f
        .store
        .search_history("wrapperkiwi", 5)
        .expect("search")
        .is_empty());
    assert!(f
        .store
        .search_history("agentkiwi", 5)
        .expect("search")
        .is_empty());
    assert_eq!(
        f.store.search_history("realkiwi", 5).expect("search")[0].cwd,
        "C:/work/codex"
    );
    assert_eq!(signal_count(&f, "correction"), 1);
}

#[test]
fn codex_failures_fixes_and_aborts_are_detected() {
    let f = fixture();
    let root = f.dir.path().join("codex");
    let patch = codex_line(
        "response_item",
        json!({
            "type": "custom_tool_call", "call_id": "p1", "name": "exec",
            "input": "await tools.apply_patch(`*** Begin Patch\n*** Update File: src/main.go\n*** End Patch`);"
        }),
    );
    write_lines(
        &rollout(&root, "a"),
        &[
            codex_meta("thread-a", false),
            codex_exec("c1", "go test ./..."),
            codex_output("c1", "Script completed\nWall time 1 seconds\nOutput:\nProcess exited with code 2\nFAIL pkg"),
            patch,
            codex_output("p1", "Script completed\nOutput:\nok"),
            codex_exec("c2", "go test ./..."),
            codex_output("c2", "Script completed\nOutput:\nexit code: 0\nok"),
            codex_exec("c4", "npm run lint"),
            codex_output(
                "c4",
                "Script completed\nOutput:\n{\"chunk_id\":\"a\",\"exit_code\":1,\"output\":\"lint: bad import\\nmore\"}",
            ),
            codex_exec("c3", "git status"),
            codex_output("c3", "Script failed\nWall time 0.0 seconds\nOutput:\nScript error:\nboom"),
            codex_line("event_msg", json!({"type": "turn_aborted", "turn_id": "t1", "reason": "interrupted"})),
            codex_line("event_msg", json!({"type": "turn_aborted", "turn_id": "t2", "reason": "replaced"})),
        ],
    );
    f.ingest_codex(&root);

    assert_eq!(signal_count(&f, "failing_command"), 3);
    assert_eq!(signal_count(&f, "error_fix"), 1);
    assert_eq!(signal_count(&f, "interrupted"), 1);
    let lint: String = f
        .raw()
        .query_row(
            "SELECT detail FROM signals WHERE sig_key = 'npm run'",
            [],
            |r| r.get(0),
        )
        .expect("lint signal");
    assert_eq!(lint, "lint: bad import");

    let raw = f.raw();
    let (key, detail): (String, String) = raw
        .query_row(
            "SELECT sig_key, detail FROM signals WHERE kind = 'error_fix'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("row");
    assert_eq!(key, "go test");
    assert!(detail.contains("main.go"), "{detail}");
}

fn compressed(lines: &[String]) -> Vec<u8> {
    let mut text = lines.join("\n");
    text.push('\n');
    ruzstd::encoding::compress_to_vec(text.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest)
}

#[test]
fn compressed_rollouts_are_read_once() {
    let f = fixture();
    let dir = f.dir.path().join("codex").join("sessions").join("2026");
    std::fs::create_dir_all(&dir).expect("dir");
    let lines = vec![codex_meta("zst-1", false), codex_user("u1", "archived rollout prompt about walruses"), codex_assistant()];
    std::fs::write(dir.join("rollout-a.jsonl.zst"), compressed(&lines)).expect("write");
    let root = f.dir.path().join("codex");
    let first = f.ingest_codex(&root);
    assert_eq!(first.files_updated, 1);
    assert_eq!(f.store.search_history("walruses", 5).expect("search").len(), 1);
    assert_eq!(f.ingest_codex(&root).files_updated, 0);
}

#[test]
fn a_rollout_compressed_after_ingest_is_not_counted_twice() {
    let f = fixture();
    let dir = f.dir.path().join("codex").join("sessions").join("2026");
    std::fs::create_dir_all(&dir).expect("dir");
    let lines = vec![codex_meta("zst-2", false), codex_user("u2", "plain then archived narwhals"), codex_assistant()];
    write_lines(&dir.join("rollout-b.jsonl"), &lines);
    let root = f.dir.path().join("codex");
    f.ingest_codex(&root);
    std::fs::remove_file(dir.join("rollout-b.jsonl")).expect("remove");
    std::fs::write(dir.join("rollout-b.jsonl.zst"), compressed(&lines)).expect("write");
    assert_eq!(f.ingest_codex(&root).files_updated, 0);
    assert_eq!(f.store.search_history("narwhals", 5).expect("search").len(), 1);
}

#[test]
fn a_corrupt_archive_is_skipped_without_stopping_the_pass() {
    let f = fixture();
    let dir = f.dir.path().join("codex").join("sessions").join("2026");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("rollout-c.jsonl.zst"), b"not zstd at all").expect("write");
    write_lines(&dir.join("rollout-d.jsonl"), &[codex_meta("ok", false), codex_user("u3", "still ingested penguins"), codex_assistant()]);
    f.ingest_codex(&f.dir.path().join("codex"));
    assert_eq!(f.store.search_history("penguins", 5).expect("search").len(), 1);
}

#[test]
fn corrections_can_be_listed_for_distilling() {
    let f = fixture();
    let root = f.dir.path().join("claude");
    let path = session_path(&root, "fix");
    write_lines(&path, &[
        user_text("a", "fix", "make the button"),
        assistant_usage("b", "r1", "m", 1, 1),
        user_text("c", "fix", "no, don't use round corners"),
    ]);
    f.ingest_claude(&root);
    let corrections = f.store.recent_corrections(5).expect("list");
    assert_eq!(corrections.len(), 1);
    assert!(corrections[0].contains("round corners"));
}
