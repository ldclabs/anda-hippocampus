//! Eval run orchestration: hosts run-scoped spaces, drives scenario suites
//! and the shared-formation experiment, and owns the zero-score failure
//! boundary. The CLI (`bin/main.rs`) only parses flags into these entry
//! points and writes the resulting reports to disk.

use anda_core::BoxError;
use anda_engine::rfc3339_datetime_now;
use futures::StreamExt;
use object_store::{ObjectStore, memory::InMemory};
use std::sync::Arc;

use super::{
    EvalExperimentReport, EvalFinding, EvalFindingKind, EvalProfile, EvalReport, EvalScenario,
    EvalSuiteReport, EvalTurnReport, run_formation_phase, run_policy_phase, run_scenario,
};
use crate::{
    space::{AppState, Space},
    types::ModelConfig,
};

/// An eval profile paired with the stable id used in space names and report
/// attribution (from the profile file's `id`, else its filename).
#[derive(Clone)]
pub struct NamedEvalProfile {
    pub id: String,
    pub profile: EvalProfile,
}

/// Shared plumbing every eval run needs; cheap to clone (AppState is
/// internally shared).
#[derive(Clone)]
pub struct EvalRunEnv {
    pub app_state: AppState,
    pub auto_create_tier: u32,
    pub run_id: u64,
    pub keep_spaces: bool,
    /// Independent judge model (plan M9), installed on every run-scoped
    /// space (including shared-formation forks).
    pub judge_model: Option<ModelConfig>,
}

/// One throwaway eval-space run (see [`EvalRunEnv::with_eval_space`]).
pub struct EvalSpaceRun<T> {
    pub output: T,
    /// The `AppState` hosting the space — kept so shared-formation can fork
    /// the closed snapshot out of its store.
    pub host: AppState,
    pub space_id: String,
    /// The close result, folded by the caller at its own severity: a suite
    /// scenario only warns (its in-memory host drops right after), while a
    /// shared-formation base snapshot must fail the scenario because every
    /// profile fork reads the closed store.
    pub close: Result<(), BoxError>,
}

impl EvalRunEnv {
    /// Host for one run-scoped eval space. Default: a sibling `AppState`
    /// over a fresh in-memory store, so the space is fully isolated (no
    /// leftover memories can leak into scores) and cleanup is simply
    /// dropping the fork. `--keep-spaces`: the real store, with the run id
    /// appended so the kept space cannot collide with earlier runs.
    pub fn space_host(&self, parts: &[&str]) -> (AppState, String) {
        if self.keep_spaces {
            let run_id = self.run_id.to_string();
            let mut parts = parts.to_vec();
            parts.push(&run_id);
            (self.app_state.clone(), compose_space_id(&parts))
        } else {
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            (
                self.app_state.fork_with_store(store),
                compose_space_id(parts),
            )
        }
    }

    /// Acquires a run-scoped eval space (host → create → load → judge
    /// install), runs `f` on it, and always closes the space before
    /// returning. `Err` is an acquisition failure; otherwise the caller
    /// folds the close result at its own severity (see [`EvalSpaceRun`]).
    pub async fn with_eval_space<T, F, Fut>(
        &self,
        id_parts: &[&str],
        f: F,
    ) -> Result<EvalSpaceRun<T>, BoxError>
    where
        F: FnOnce(Arc<Space>) -> Fut,
        Fut: Future<Output = T>,
    {
        let (host, space_id) = self.space_host(id_parts);
        let space = load_eval_space(self, &host, &space_id).await?;
        let output = f(space.clone()).await;
        let close = space.close().await;
        Ok(EvalSpaceRun {
            output,
            host,
            space_id,
            close,
        })
    }
}

/// Creates and loads a run-scoped eval space inside `state` (a throwaway
/// in-memory fork by default, the real store under `--keep-spaces`; see
/// [`EvalRunEnv::space_host`]). The space id is unique per host, so creation
/// must succeed. The env's independent judge model, when configured, is
/// installed on every space this loads.
pub async fn load_eval_space(
    env: &EvalRunEnv,
    state: &AppState,
    space_id: &str,
) -> Result<Arc<Space>, BoxError> {
    state
        .admin_create_space(
            crate::agents::SELF_USER_ID,
            crate::agents::SELF_USER_ID,
            space_id.to_string(),
            env.auto_create_tier,
            anda_engine::unix_ms(),
        )
        .await?;

    let space = state.load_space(space_id, true).await?;
    if let Some(judge) = &env.judge_model {
        space.set_judge_model(judge.clone())?;
    }
    Ok(space)
}

/// Folds an operation's result with its space-close result where a failed
/// close invalidates the produced value (shared-formation base snapshots
/// and profile forks).
fn fail_on_close<T>(
    output: Result<T, BoxError>,
    close: Result<(), BoxError>,
) -> Result<T, BoxError> {
    match (output, close) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(err), _) => Err(err),
        (_, Err(err)) => Err(err),
    }
}

/// Concurrent-scenario budget for one suite run. Scenarios are fully
/// isolated (each in its own run-scoped space), so this only bounds how many
/// model conversations are in flight at once — enough to hide LLM latency
/// (the optimize loop re-runs the whole suite every generation) without
/// driving provider rate limits into the zero-score fallback reports.
const EVAL_SCENARIO_CONCURRENCY: usize = 4;

pub async fn run_eval_suite(
    env: &EvalRunEnv,
    base_space_id: &str,
    profile: &NamedEvalProfile,
    scenarios: &[EvalScenario],
) -> Result<EvalSuiteReport, BoxError> {
    // Each scenario runs in its own run-scoped space (see `space_host`), so
    // leftover memories from a previous run can never leak into scores and
    // scenarios can run concurrently; the default in-memory host vanishes
    // when `state` drops. `buffered` keeps the reports in scenario order.
    // The futures are collected eagerly (futures are inert until polled): a
    // lazy `map` closure inside the stream would poison this function's
    // future with rustc's "implementation of FnOnce is not general enough"
    // when it is later boxed 'static (the optimize loop does exactly that).
    let scenario_futures: Vec<_> = scenarios
        .iter()
        .map(|scenario| run_suite_scenario(env, base_space_id, profile, scenario))
        .collect();
    let reports: Vec<EvalReport> = futures::stream::iter(scenario_futures)
        .buffered(EVAL_SCENARIO_CONCURRENCY)
        .collect()
        .await;

    Ok(EvalSuiteReport::from_reports(profile.id.clone(), reports))
}

async fn run_suite_scenario(
    env: &EvalRunEnv,
    base_space_id: &str,
    profile: &NamedEvalProfile,
    scenario: &EvalScenario,
) -> EvalReport {
    let run = env
        .with_eval_space(
            &[base_space_id, &profile.id, &scenario.id],
            |space| async move { run_scenario(space.as_ref(), scenario, &profile.profile).await },
        )
        .await;
    let result = match run {
        Ok(run) => {
            // Close even when the scenario fails so `--keep-spaces` leaves a
            // flushed, inspectable space. Close failures only warn: on the
            // default path the store is dropped right after, so there is
            // nothing durable to lose.
            if let Err(err) = run.close {
                eprintln!(
                    "warning: failed to close eval space {}: {err}",
                    run.space_id
                );
            }
            run.output
        }
        Err(err) => Err(err),
    };
    // One scenario's failure must not discard the other (paid) scenario
    // reports: record it as a zero-score report with a finding so the suite
    // mean is not silently inflated either, and keep going.
    match result {
        Ok(report) => report,
        Err(err) => {
            eprintln!("warning: eval scenario `{}` aborted: {err}", scenario.id);
            failed_scenario_report(scenario, &profile.id, err.as_ref())
        }
    }
}

/// Zero-score stand-in for a scenario that aborted before producing a
/// report. Dropping the scenario instead would inflate the suite mean —
/// poisonous when the suite is an optimizer fitness function or gated.
fn failed_scenario_report(
    scenario: &EvalScenario,
    profile_id: &str,
    err: &(dyn std::error::Error + Send + Sync),
) -> EvalReport {
    let mut report = EvalReport {
        scenario_id: scenario.id.clone(),
        description: scenario.description.clone(),
        profile_id: Some(profile_id.to_string()),
        started_at: Some(rfc3339_datetime_now()),
        ..Default::default()
    };
    report.turns.push(EvalTurnReport {
        findings: vec![EvalFinding {
            kind: EvalFindingKind::JudgeError,
            expectation_id: None,
            message: format!("scenario aborted before completion: {err}"),
        }],
        ..Default::default()
    });
    report.attribution.judge_error = 1;
    report
}

/// Shared-formation experiment: replay formation once per scenario into a
/// base space, snapshot its objects, then fork the snapshot into a fresh
/// in-memory store per profile and run only maintenance + checkpoints there.
/// Every profile is judged on the identical encoded memory, so differences
/// between suites measure the policy — not formation's LLM variance — and
/// the most expensive phase runs once instead of once per profile.
///
/// Requires at least one profile: the shared formation phase is replayed
/// under `profiles[0]`'s timeouts and attributed to its id.
pub async fn run_shared_formation_experiment(
    env: &EvalRunEnv,
    base_space_id: &str,
    profiles: &[NamedEvalProfile],
    scenarios: &[EvalScenario],
) -> Result<EvalExperimentReport, BoxError> {
    if profiles.is_empty() {
        return Err("the shared-formation experiment requires at least one profile".into());
    }
    let mut shared_reports = Vec::with_capacity(scenarios.len());
    let mut profile_reports: Vec<Vec<EvalReport>> = vec![Vec::new(); profiles.len()];

    for scenario in scenarios {
        // Scenario-level failure isolation, matching `run_eval_suite`: one
        // scenario's abort must not discard the other scenarios' already-paid
        // formation and policy results.
        let mut base = None;
        let formation_result =
            match env
                .with_eval_space(&[base_space_id, "form", &scenario.id], |space| {
                    // The formation phase only reads timeouts from the profile.
                    async move {
                        run_formation_phase(space.as_ref(), scenario, &profiles[0].profile).await
                    }
                })
                .await
            {
                Ok(run) => {
                    base = Some((run.host, run.space_id));
                    // The base snapshot must be flushed and closed before the
                    // profiles fork it; a close failure poisons every fork, so
                    // it fails the scenario rather than the experiment.
                    fail_on_close(run.output, run.close)
                }
                Err(err) => Err(err),
            };
        match formation_result {
            Ok(report) => shared_reports.push(report),
            Err(err) => {
                eprintln!(
                    "warning: shared formation for scenario `{}` aborted: {err}",
                    scenario.id
                );
                // No profile can fork a snapshot that never materialized:
                // every suite gets the zero-score stand-in for this scenario.
                shared_reports.push(failed_scenario_report(
                    scenario,
                    &profiles[0].id,
                    err.as_ref(),
                ));
                for (index, profile) in profiles.iter().enumerate() {
                    profile_reports[index].push(failed_scenario_report(
                        scenario,
                        &profile.id,
                        err.as_ref(),
                    ));
                }
                continue;
            }
        }
        let Some((base_state, base_id)) = base else {
            continue;
        };

        // Forks are fully isolated — each lives in its own in-memory store —
        // so every profile's policy phase can replay concurrently. Failures
        // stay per-profile: `join_all` (not `try_join_all`) so one fork's
        // abort cannot discard its siblings' finished reports.
        // `fork_space` owns the fork protocol, including loading without
        // background autostart: the fork inherits the base snapshot's
        // formation cursor and wiki-digest backlog and must not resume them.
        let fork_results = futures::future::join_all(profiles.iter().map(|profile| {
            let base_id = base_id.clone();
            let base_state = base_state.clone();
            async move {
                let fork_space = base_state.fork_space(&base_id, None).await?;
                if let Some(judge) = &env.judge_model {
                    fork_space.set_judge_model(judge.clone())?;
                }
                let result =
                    run_policy_phase(fork_space.as_ref(), scenario, &profile.profile).await;
                let close_result = fork_space.close().await;
                fail_on_close(result, close_result)
            }
        }))
        .await;
        // The base snapshot is only needed until every profile has forked
        // it: `base_state` (and, unless --keep-spaces, its store) drops at
        // the end of this iteration.
        for ((index, profile), result) in profiles.iter().enumerate().zip(fork_results) {
            profile_reports[index].push(match result {
                Ok(report) => report,
                Err(err) => {
                    eprintln!(
                        "warning: policy phase for scenario `{}` profile `{}` aborted: {err}",
                        scenario.id, profile.id
                    );
                    failed_scenario_report(scenario, &profile.id, err.as_ref())
                }
            });
        }
    }

    let suites: Vec<EvalSuiteReport> = profiles
        .iter()
        .zip(profile_reports)
        .map(|(profile, reports)| EvalSuiteReport::from_reports(profile.id.clone(), reports))
        .collect();
    let mut experiment = EvalExperimentReport::from_suites(base_space_id.to_string(), suites);
    experiment.shared_formation = shared_reports;
    Ok(experiment)
}

/// AndaDB space names must match `[a-z0-9_]` (max 64 chars); anything else
/// fails at space creation. Lowercase and map every other character to `_`.
pub fn sanitize_space_id_part(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }

    let out = out.trim_matches('_');
    if out.is_empty() {
        "part".to_string()
    } else {
        out.to_string()
    }
}

/// AndaDB rejects database names longer than 64 characters.
const MAX_SPACE_ID_LEN: usize = 64;

/// Joins sanitized parts into a space id and caps it at AndaDB's name limit.
/// Over-long ids keep a readable prefix plus a hash of the full id, so two
/// distinct long ids can never collide after truncation.
fn compose_space_id(parts: &[&str]) -> String {
    let id = parts
        .iter()
        .map(|part| sanitize_space_id_part(part))
        .collect::<Vec<_>>()
        .join("_");
    if id.len() <= MAX_SPACE_ID_LEN {
        return id;
    }
    let suffix = format!("_{:016x}", fnv1a(id.as_bytes()));
    // The sanitized id is pure ASCII, so byte slicing cannot split a char.
    format!("{}{suffix}", &id[..MAX_SPACE_ID_LEN - suffix.len()])
}

/// FNV-1a: a tiny stable hash for id truncation; not security-sensitive.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::super::EvalTurnType;
    use super::*;
    use crate::testkit::{app_state_core, models_with_completer};
    use anda_core::{AgentOutput, BoxPinFut, CompletionRequest, Message};
    use anda_engine::{model::CompletionFeaturesDyn, unix_ms};

    #[derive(Debug)]
    struct EvalCompleter;

    impl CompletionFeaturesDyn for EvalCompleter {
        fn model_name(&self) -> String {
            "eval-run-test-model".to_string()
        }

        fn completion(&self, req: CompletionRequest) -> BoxPinFut<Result<AgentOutput, BoxError>> {
            Box::pin(async move {
                Ok(AgentOutput {
                    content: "done".to_string(),
                    chat_history: vec![Message {
                        role: "assistant".to_string(),
                        content: vec![format!("processed: {}", req.prompt).into()],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
            })
        }
    }

    fn test_env(name: &str) -> EvalRunEnv {
        EvalRunEnv {
            app_state: app_state_core(
                name,
                models_with_completer(EvalCompleter),
                vec![],
                "test",
                0,
            ),
            auto_create_tier: 1,
            run_id: unix_ms(),
            keep_spaces: false,
            judge_model: None,
        }
    }

    fn normal_turn(turn: u64, user: Option<&str>) -> super::super::EvalTurn {
        super::super::EvalTurn {
            turn,
            turn_type: EvalTurnType::Normal,
            timestamp: None,
            context: None,
            user: user.map(str::to_string),
            messages: vec![],
            query: None,
            intent: None,
            evaluation: None,
            maintenance: None,
            noise: false,
        }
    }

    fn scenario(id: &str, turns: Vec<super::super::EvalTurn>) -> EvalScenario {
        EvalScenario {
            id: id.to_string(),
            description: None,
            hidden_profile: serde_json::Value::Null,
            default_context: None,
            noise: None,
            timeline: turns,
        }
    }

    fn fast_profile(id: &str) -> NamedEvalProfile {
        NamedEvalProfile {
            id: id.to_string(),
            profile: EvalProfile {
                id: Some(id.to_string()),
                wait_timeout_ms: 3_000,
                poll_interval_ms: 10,
                ..Default::default()
            },
        }
    }

    fn is_zero_score_fallback(report: &EvalReport) -> bool {
        report.attribution.judge_error == 1
            && report.turns.len() == 1
            && report.turns[0].findings.iter().any(|finding| {
                matches!(finding.kind, EvalFindingKind::JudgeError)
                    && finding
                        .message
                        .contains("scenario aborted before completion")
            })
    }

    #[tokio::test]
    async fn run_eval_suite_isolates_scenario_failures() {
        let env = test_env("eval_run_suite_isolation");
        let profile = fast_profile("default");
        // The second scenario aborts synchronously (a turn with neither
        // messages nor user text); the first must keep its paid report.
        let scenarios = vec![
            scenario("good", vec![normal_turn(1, Some("Alice likes tea"))]),
            scenario("bad", vec![normal_turn(1, None)]),
        ];

        let suite = run_eval_suite(&env, "suite_base", &profile, &scenarios)
            .await
            .unwrap();

        assert_eq!(suite.reports.len(), 2);
        assert_eq!(suite.reports[0].scenario_id, "good");
        assert!(!is_zero_score_fallback(&suite.reports[0]));
        assert_eq!(suite.reports[0].turns.len(), 1);
        assert_eq!(suite.reports[1].scenario_id, "bad");
        assert!(is_zero_score_fallback(&suite.reports[1]));
        assert_eq!(suite.reports[1].profile_id.as_deref(), Some("default"));
        assert_eq!(suite.reports[1].score.total, 0.0);
    }

    #[tokio::test]
    async fn shared_formation_zero_scores_every_profile_when_formation_aborts() {
        let env = test_env("eval_run_shared_abort");
        let profiles = vec![fast_profile("a"), fast_profile("b")];
        let scenarios = vec![scenario("broken", vec![normal_turn(1, None)])];

        let experiment =
            run_shared_formation_experiment(&env, "shared_base", &profiles, &scenarios)
                .await
                .unwrap();

        assert_eq!(experiment.shared_formation.len(), 1);
        assert!(is_zero_score_fallback(&experiment.shared_formation[0]));
        assert_eq!(experiment.suites.len(), 2);
        for suite in &experiment.suites {
            assert_eq!(suite.reports.len(), 1);
            assert!(is_zero_score_fallback(&suite.reports[0]));
        }
    }

    #[tokio::test]
    async fn shared_formation_forks_profiles_from_base_snapshot() {
        let env = test_env("eval_run_shared_forks");
        let profiles = vec![fast_profile("a"), fast_profile("b")];
        let scenarios = vec![scenario(
            "remember",
            vec![normal_turn(1, Some("Bob prefers dark mode"))],
        )];

        let experiment =
            run_shared_formation_experiment(&env, "shared_base", &profiles, &scenarios)
                .await
                .unwrap();

        // The formation phase ran once and produced a real turn report...
        assert_eq!(experiment.shared_formation.len(), 1);
        assert!(!is_zero_score_fallback(&experiment.shared_formation[0]));
        assert_eq!(experiment.shared_formation[0].turns.len(), 1);
        // ...and every profile's policy phase ran on a fork without aborting
        // (no checkpoints or maintenance in the scenario, so no turns).
        assert_eq!(experiment.suites.len(), 2);
        for suite in &experiment.suites {
            assert_eq!(suite.reports.len(), 1);
            assert!(!is_zero_score_fallback(&suite.reports[0]));
        }
    }

    #[test]
    fn sanitize_space_id_part_matches_anda_db_charset() {
        // AndaDB names only allow [a-z0-9_]: lowercase and fold the rest.
        assert_eq!(
            sanitize_space_id_part("Style-Preference"),
            "style_preference"
        );
        assert_eq!(sanitize_space_id_part("__mixed 42__"), "mixed_42");
        assert_eq!(sanitize_space_id_part("汉字"), "part");
    }

    #[test]
    fn compose_space_id_caps_length_without_collisions() {
        let short = compose_space_id(&["eval", "Default-Profile", "scenario", "123"]);
        assert_eq!(short, "eval_default_profile_scenario_123");

        let long_a = compose_space_id(&["eval", &"a".repeat(80), "scenario", "123"]);
        let long_b = compose_space_id(&["eval", &"a".repeat(81), "scenario", "123"]);
        assert_eq!(long_a.len(), MAX_SPACE_ID_LEN);
        assert_eq!(long_b.len(), MAX_SPACE_ID_LEN);
        assert_ne!(long_a, long_b, "hash suffix must keep long ids distinct");
        assert!(
            long_a
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        );
    }
}
