use std::time::{Duration, Instant};

use dstreamy_cli::CliError;
use dstreamy_cli::discord::BRIEF_VALIDATION_TIMEOUT as VALIDATION_TIMEOUT;
use dstreamy_cli::login_capture::{
    Action, BrowserLoginOutcome, BrowserState, Command, Event, Failure, FollowUp, Job,
    PAGE_LOAD_TIMEOUT, SIGNED_IN_GRACE, SeenCandidates, Validation, ValidationError, View,
    follow_up, parse_event, validate_job,
};
use futures_util::FutureExt;

const MS: Duration = Duration::from_millis(1);

fn loaded(now: Instant) -> BrowserState {
    let mut state = BrowserState::new(now);
    state.page_loaded();
    state
}

fn job(command: Option<Command>) -> Job {
    match command {
        Some(Command::Validate(job)) => job,
        _ => panic!("expected a validation job"),
    }
}

fn exit(command: Option<Command>) -> BrowserLoginOutcome {
    match command {
        Some(Command::Exit(outcome)) => outcome,
        _ => panic!("expected an exit"),
    }
}

fn finish(state: &mut BrowserState, job: Job, result: Validation<String>) -> Option<Command> {
    finish_at(state, job, result, Instant::now())
}

fn finish_at(
    state: &mut BrowserState,
    job: Job,
    result: Validation<String>,
    now: Instant,
) -> Option<Command> {
    let Job {
        generation,
        seen,
        candidate,
    } = job;
    let checked = validate_job(seen, candidate, |_| async move {
        match result {
            Validation::Accepted(token) => Ok(token),
            Validation::Rejected => Err(ValidationError::Rejected),
            Validation::Transient => Err(ValidationError::Transient(())),
        }
    });
    let (seen, result) = checked.now_or_never().expect("the check completes at once");
    state.validated(generation, seen, result, now)
}

fn run_validation(state: &mut BrowserState, job: Job, accept: bool) -> Option<Command> {
    let token = job.candidate.as_str().to_owned();
    let result = if accept {
        Validation::Accepted(token)
    } else {
        Validation::Rejected
    };
    finish(state, job, result)
}

fn signed_in_past_grace(state: &mut BrowserState, start: Instant) -> Instant {
    assert!(state.ipc("signed_in", start).is_none());
    let later = start + SIGNED_IN_GRACE + MS;
    state.tick(later);
    later
}

fn no_past_deadline(state: &BrowserState, now: Instant) {
    if let Some(deadline) = state.deadline() {
        assert!(
            deadline > now,
            "deadline in the past would spin the event loop"
        );
    }
}

#[test]
fn failed_capture_keeps_the_dialog_open_on_recovery() {
    let start = Instant::now();
    let mut state = loaded(start);
    signed_in_past_grace(&mut state, start);
    assert!(state.is_open());
    assert_eq!(state.view(), View::Recovery(Failure::NoCandidate));
    assert_eq!(state.deadline(), None);
}

#[test]
fn discord_page_stays_while_waiting_and_before_the_grace_period() {
    let start = Instant::now();
    let mut state = loaded(start);
    assert_eq!(state.view(), View::Discord);
    assert_eq!(state.deadline(), None);
    assert!(state.ipc("signed_in", start).is_none());
    assert_eq!(state.deadline(), Some(start + SIGNED_IN_GRACE));
    state.tick(start + SIGNED_IN_GRACE - MS);
    assert_eq!(state.view(), View::Discord);
}

#[test]
fn close_and_escape_produce_cancelled_without_token() {
    let now = Instant::now();
    assert_eq!(
        exit(BrowserState::new(now).close()),
        BrowserLoginOutcome::Cancelled
    );
    assert_eq!(
        exit(BrowserState::new(now).ipc("close", now)),
        BrowserLoginOutcome::Cancelled
    );
    let mut state = loaded(now);
    let later = signed_in_past_grace(&mut state, now);
    assert_eq!(
        exit(state.ipc("action:cancel", later)),
        BrowserLoginOutcome::Cancelled
    );
    assert!(!state.is_open());
}

#[test]
fn cancelled_outcome_never_reaches_the_saving_login() {
    match follow_up(BrowserLoginOutcome::Cancelled) {
        Err(CliError::BrowserLogin(reason)) => assert_eq!(reason, "cancelled"),
        _ => panic!("cancel must end the command with an error"),
    }
    assert!(matches!(
        follow_up(BrowserLoginOutcome::Token("good.tok.en".to_owned())),
        Ok(FollowUp::Login(token)) if token == "good.tok.en"
    ));
    assert!(matches!(
        follow_up(BrowserLoginOutcome::UseQr),
        Ok(FollowUp::Qr)
    ));
    assert!(matches!(
        follow_up(BrowserLoginOutcome::UseToken),
        Ok(FollowUp::ReadToken)
    ));
}

#[test]
fn malformed_and_unknown_ipc_leave_the_state_waiting() {
    let start = Instant::now();
    let mut state = loaded(start);
    state.ipc("signed_in", start);
    let deadline = state.deadline();
    assert!(deadline.is_some());
    for message in [
        "",
        "candidate:",
        "candidate: ",
        "candidate:a b",
        "token:a.b.c",
        "action:",
        "action:nope",
        "signed_in:1",
        "page_loaded:1",
        "page_loaded ",
    ] {
        assert!(parse_event(message).is_none(), "{message:?} must not parse");
        assert!(state.ipc(message, start + MS).is_none(), "{message:?}");
        assert_eq!(state.view(), View::Discord, "{message:?}");
        assert_eq!(state.deadline(), deadline, "{message:?}");
        assert!(!state.is_validating(), "{message:?}");
        assert!(state.is_open(), "{message:?}");
    }
}

#[test]
fn recovery_actions_parse_from_the_app_page() {
    let cases = [
        ("action:retry", Action::Retry),
        ("action:qr", Action::UseQr),
        ("action:token", Action::UseToken),
        ("action:cancel", Action::Cancel),
    ];
    for (message, action) in cases {
        assert!(matches!(parse_event(message), Some(Event::Action(a)) if a == action));
    }
}

#[test]
fn actions_are_ignored_while_the_discord_page_is_shown() {
    let now = Instant::now();
    let mut state = loaded(now);
    for message in ["action:retry", "action:qr", "action:token"] {
        assert!(state.ipc(message, now).is_none());
        assert!(state.is_open());
        assert_eq!(state.view(), View::Discord);
    }
}

#[test]
fn rejected_candidate_after_sign_in_keeps_discord_until_the_grace_period() {
    let start = Instant::now();
    let mut state = loaded(start);
    state.ipc("signed_in", start);
    let j = job(state.ipc("candidate:bad.tok.en", start));
    assert!(state.is_validating());
    assert!(run_validation(&mut state, j, false).is_none());
    assert!(state.is_open());
    assert_eq!(state.view(), View::Discord);
    state.tick(start + SIGNED_IN_GRACE + MS);
    assert_eq!(state.view(), View::Recovery(Failure::Rejected));
}

#[test]
fn later_valid_candidate_wins_after_a_rejection() {
    let start = Instant::now();
    let mut state = loaded(start);
    state.ipc("signed_in", start);
    let bad = job(state.ipc("candidate:bad.tok.en", start));
    assert!(run_validation(&mut state, bad, false).is_none());
    let good = job(state.ipc("candidate:good.tok.en", start + MS));
    assert_eq!(
        exit(run_validation(&mut state, good, true)),
        BrowserLoginOutcome::Token("good.tok.en".to_owned())
    );
}

#[test]
fn rejected_candidate_before_sign_in_keeps_the_discord_page() {
    let now = Instant::now();
    let mut state = loaded(now);
    let j = job(state.ipc("candidate:bad.tok.en", now));
    assert!(run_validation(&mut state, j, false).is_none());
    assert!(state.is_open());
    assert_eq!(state.view(), View::Discord);
    assert_eq!(state.deadline(), None);
}

#[test]
fn transient_failure_keeps_discord_and_shows_retry_after_the_grace_period() {
    let start = Instant::now();
    let mut state = loaded(start);
    let j = job(state.ipc("candidate:good.tok.en", start));
    assert!(finish(&mut state, j, Validation::Transient).is_none());
    assert!(state.is_open());
    assert_eq!(state.view(), View::Discord);
    signed_in_past_grace(&mut state, start + MS);
    assert_eq!(state.view(), View::Recovery(Failure::Unreachable));
}

#[test]
fn accepted_candidate_becomes_the_token() {
    let now = Instant::now();
    let mut state = loaded(now);
    let j = job(state.ipc("candidate:good.tok.en", now));
    assert_eq!(
        exit(run_validation(&mut state, j, true)),
        BrowserLoginOutcome::Token("good.tok.en".to_owned())
    );
    assert!(!state.is_open());
}

#[test]
fn candidates_arriving_during_validation_are_queued_then_validated() {
    let now = Instant::now();
    let mut state = loaded(now);
    state.ipc("signed_in", now);
    let first = job(state.ipc("candidate:bad.tok.en", now));
    assert!(state.ipc("candidate:good.tok.en", now).is_none());
    let second = job(run_validation(&mut state, first, false));
    assert_eq!(second.candidate.as_str(), "good.tok.en");
    assert_eq!(state.view(), View::Discord);
    assert_eq!(
        exit(run_validation(&mut state, second, true)),
        BrowserLoginOutcome::Token("good.tok.en".to_owned())
    );
}

#[test]
fn expired_grace_during_validation_clears_the_deadline_and_still_accepts() {
    let start = Instant::now();
    let mut state = loaded(start);
    state.ipc("signed_in", start);
    let j = job(state.ipc("candidate:slow.tok.en", start));
    let late = start + SIGNED_IN_GRACE + MS;
    state.tick(late);
    no_past_deadline(&state, late);
    assert_eq!(state.view(), View::Discord);
    assert_eq!(
        exit(run_validation(&mut state, j, true)),
        BrowserLoginOutcome::Token("slow.tok.en".to_owned())
    );
}

#[test]
fn expired_grace_during_validation_opens_recovery_once_the_check_fails() {
    let start = Instant::now();
    let mut state = loaded(start);
    state.ipc("signed_in", start);
    let j = job(state.ipc("candidate:slow.tok.en", start));
    let late = start + SIGNED_IN_GRACE + MS;
    state.tick(late);
    assert_eq!(state.deadline(), Some(start + VALIDATION_TIMEOUT));
    assert_eq!(state.view(), View::Discord);
    assert!(run_validation(&mut state, j, false).is_none());
    assert_eq!(state.view(), View::Recovery(Failure::Rejected));
    assert_eq!(state.deadline(), None);
}

#[test]
fn expired_grace_during_validation_then_cancel_exits_cancelled() {
    let start = Instant::now();
    let mut state = loaded(start);
    state.ipc("signed_in", start);
    let j = job(state.ipc("candidate:slow.tok.en", start));
    let late = start + SIGNED_IN_GRACE + MS;
    state.tick(late);
    assert_eq!(exit(state.close()), BrowserLoginOutcome::Cancelled);
    assert_eq!(state.deadline(), None);
    assert!(!state.is_open());
    assert!(
        state
            .validated(j.generation, j.seen, Validation::Transient, start)
            .is_none()
    );
}

#[test]
fn closing_while_a_deadline_is_pending_clears_it() {
    let start = Instant::now();
    let mut state = loaded(start);
    state.ipc("signed_in", start);
    let _j = job(state.ipc("candidate:slow.tok.en", start));
    assert!(state.deadline().is_some());
    assert_eq!(exit(state.close()), BrowserLoginOutcome::Cancelled);
    assert_eq!(state.deadline(), None);
}

#[test]
fn validation_timeout_recovers_with_a_retryable_error() {
    let start = Instant::now();
    let mut state = loaded(start);
    let _j = job(state.ipc("candidate:slow.tok.en", start));
    let timeout = start + VALIDATION_TIMEOUT;
    assert_eq!(state.deadline(), Some(timeout));
    state.tick(timeout);
    assert_eq!(state.view(), View::Recovery(Failure::Unreachable));
}

#[test]
fn close_during_validation_exits_immediately_and_discards_a_stale_success() {
    let now = Instant::now();
    let mut state = loaded(now);
    let j = job(state.ipc("candidate:good.tok.en", now));
    assert_eq!(exit(state.close()), BrowserLoginOutcome::Cancelled);
    assert!(!state.is_open());
    assert!(state.ipc("candidate:other.tok.en", now).is_none());
    assert!(
        state
            .validated(
                j.generation,
                j.seen,
                Validation::Accepted("good.tok.en".to_owned()),
                now
            )
            .is_none()
    );
}

#[test]
fn qr_and_token_choices_exit_immediately_when_idle() {
    for (message, outcome) in [
        ("action:qr", BrowserLoginOutcome::UseQr),
        ("action:token", BrowserLoginOutcome::UseToken),
    ] {
        let start = Instant::now();
        let mut state = loaded(start);
        let now = signed_in_past_grace(&mut state, start);
        assert_eq!(exit(state.ipc(message, now)), outcome);
    }
}

#[test]
fn page_that_never_loads_shows_recovery_after_the_load_timeout() {
    let start = Instant::now();
    let mut state = BrowserState::new(start);
    assert_eq!(state.deadline(), Some(start + PAGE_LOAD_TIMEOUT));
    state.tick(start + PAGE_LOAD_TIMEOUT - MS);
    assert_eq!(state.view(), View::Discord);
    let late = start + PAGE_LOAD_TIMEOUT;
    state.tick(late);
    assert_eq!(state.view(), View::Recovery(Failure::PageNotLoaded));
    no_past_deadline(&state, late);
}

#[test]
fn page_loaded_ipc_message_clears_the_load_timeout() {
    let start = Instant::now();
    let mut state = BrowserState::new(start);
    assert!(matches!(
        parse_event("page_loaded"),
        Some(Event::PageLoaded)
    ));
    assert!(state.ipc("page_loaded", start + MS).is_none());
    assert_eq!(state.deadline(), None);
    state.tick(start + PAGE_LOAD_TIMEOUT * 2);
    assert_eq!(state.view(), View::Discord);
}

#[test]
fn page_loaded_ipc_message_is_ignored_on_the_recovery_page() {
    let start = Instant::now();
    let mut state = BrowserState::new(start);
    state.tick(start + PAGE_LOAD_TIMEOUT + MS);
    let view = state.view();
    assert!(matches!(view, View::Recovery(Failure::PageNotLoaded)));
    assert!(
        state
            .ipc("page_loaded", start + PAGE_LOAD_TIMEOUT + MS)
            .is_none()
    );
    assert_eq!(state.view(), view);
}

#[test]
fn page_load_clears_the_load_timeout() {
    let start = Instant::now();
    let mut state = BrowserState::new(start);
    state.page_loaded();
    assert_eq!(state.deadline(), None);
    state.tick(start + PAGE_LOAD_TIMEOUT * 2);
    assert_eq!(state.view(), View::Discord);
}

#[test]
fn retry_after_load_failure_restarts_the_load_timeout() {
    let start = Instant::now();
    let mut state = BrowserState::new(start);
    state.tick(start + PAGE_LOAD_TIMEOUT);
    let retry_at = start + PAGE_LOAD_TIMEOUT + MS;
    assert!(state.ipc("action:retry", retry_at).is_none());
    assert_eq!(state.view(), View::Discord);
    assert_eq!(state.deadline(), Some(retry_at + PAGE_LOAD_TIMEOUT));
}

#[test]
fn retry_resets_deadlines_seen_candidates_and_view() {
    let start = Instant::now();
    let mut state = loaded(start);
    state.ipc("signed_in", start);
    let j = job(state.ipc("candidate:same.tok.en", start));
    assert!(run_validation(&mut state, j, false).is_none());
    let now = start + SIGNED_IN_GRACE + MS;
    state.tick(now);
    assert_eq!(state.view(), View::Recovery(Failure::Rejected));

    assert!(state.ipc("action:retry", now).is_none());
    assert_eq!(state.view(), View::Discord);
    assert_eq!(state.deadline(), Some(now + PAGE_LOAD_TIMEOUT));
    state.page_loaded();
    assert_eq!(state.deadline(), None);
    state.tick(now + SIGNED_IN_GRACE * 2);
    assert_eq!(state.view(), View::Discord);

    let again = job(state.ipc("candidate:same.tok.en", now));
    let mut seen = again.seen;
    assert!(
        seen.insert(&again.candidate),
        "retry must forget seen candidates"
    );
}

#[test]
fn retry_forgets_earlier_failures() {
    let start = Instant::now();
    let mut state = loaded(start);
    state.ipc("signed_in", start);
    let j = job(state.ipc("candidate:bad.tok.en", start));
    assert!(run_validation(&mut state, j, false).is_none());
    let now = signed_in_past_grace(&mut state, start);
    assert_eq!(state.view(), View::Recovery(Failure::Rejected));
    state.ipc("action:retry", now);
    state.page_loaded();
    signed_in_past_grace(&mut state, now);
    assert_eq!(state.view(), View::Recovery(Failure::NoCandidate));
}

#[test]
fn timeout_then_retry_gives_the_next_candidate_its_own_timeout() {
    let start = Instant::now();
    let mut state = loaded(start);
    let stale = job(state.ipc("candidate:slow.tok.en", start));
    let expired = start + VALIDATION_TIMEOUT;
    state.tick(expired);
    assert_eq!(state.view(), View::Recovery(Failure::Unreachable));
    assert!(!state.is_validating());

    let retry_at = expired + MS;
    assert!(
        state
            .event(Event::Action(Action::Retry), retry_at)
            .is_none()
    );
    state.page_loaded();
    let candidate_at = retry_at + MS;
    let next = job(state.ipc("candidate:next.tok.en", candidate_at));
    assert_eq!(state.deadline(), Some(candidate_at + VALIDATION_TIMEOUT));
    assert!(
        state
            .validated(
                stale.generation,
                stale.seen,
                Validation::Accepted("slow.tok.en".to_owned()),
                candidate_at
            )
            .is_none()
    );
    assert!(state.is_validating());
    assert_eq!(state.deadline(), Some(candidate_at + VALIDATION_TIMEOUT));
    assert_eq!(
        exit(run_validation(&mut state, next, true)),
        BrowserLoginOutcome::Token("next.tok.en".to_owned())
    );
}

#[test]
fn verifying_status_shows_only_on_discord_while_a_check_runs() {
    let now = Instant::now();
    let mut state = loaded(now);
    assert!(!state.verifying());
    let j = job(state.ipc("candidate:good.tok.en", now));
    assert!(state.verifying());
    assert!(run_validation(&mut state, j, false).is_none());
    assert!(!state.verifying());
    let j = job(state.ipc("candidate:other.tok.en", now));
    assert_eq!(exit(state.close()), BrowserLoginOutcome::Cancelled);
    assert!(!state.verifying());
    let _ = run_validation(&mut state, j, false);
}

#[test]
fn lost_validation_restores_a_usable_state() {
    let now = Instant::now();
    let mut state = loaded(now);
    let j = job(state.ipc("candidate:good.tok.en", now));
    assert!(
        state
            .validated(
                j.generation,
                SeenCandidates::default(),
                Validation::Transient,
                now
            )
            .is_none()
    );
    assert!(!state.is_validating());
    assert!(state.ipc("candidate:good.tok.en", now).is_some());
}

#[test]
fn outcome_debug_never_shows_the_token() {
    let shown = format!(
        "{:?}",
        BrowserLoginOutcome::Token("secret.tok.en".to_owned())
    );
    assert!(!shown.contains("secret"));
}

#[test]
fn load_failure_shows_recovery_immediately_without_waiting_for_the_timeout() {
    let start = Instant::now();
    let mut state = BrowserState::new(start);
    assert!(state.event(Event::LoadFailed, start + MS).is_none());
    assert_eq!(state.view(), View::Recovery(Failure::PageNotLoaded));
    assert_eq!(state.deadline(), None);
}

#[test]
fn load_failure_after_the_page_loaded_still_recovers() {
    let start = Instant::now();
    let mut state = loaded(start);
    state.event(Event::LoadFailed, start + MS);
    assert_eq!(state.view(), View::Recovery(Failure::PageNotLoaded));
}

#[test]
fn load_failure_during_validation_waits_for_the_check_then_recovers() {
    let start = Instant::now();
    let mut state = loaded(start);
    assert!(state.ipc("candidate:aaa.bbb.ccc", start).is_some());
    state.event(Event::LoadFailed, start + MS);
    assert_eq!(state.view(), View::Discord);
}

#[test]
fn a_rejection_after_a_load_failure_is_what_recovery_reports() {
    let start = Instant::now();
    let mut state = loaded(start);
    let j = job(state.ipc("candidate:bad.tok.en", start));
    state.event(Event::LoadFailed, start + MS);
    assert!(finish_at(&mut state, j, Validation::Rejected, start + 2 * MS).is_none());
    assert_eq!(state.view(), View::Recovery(Failure::Rejected));
}

#[test]
fn a_load_failure_after_an_earlier_rejection_reports_the_page() {
    let start = Instant::now();
    let mut state = loaded(start);
    let j = job(state.ipc("candidate:bad.tok.en", start));
    assert!(finish_at(&mut state, j, Validation::Rejected, start + MS).is_none());
    state.event(Event::LoadFailed, start + 2 * MS);
    assert_eq!(state.view(), View::Recovery(Failure::PageNotLoaded));
}

#[test]
fn a_late_result_from_a_timed_out_check_is_ignored() {
    let start = Instant::now();
    let mut state = loaded(start);
    let stale = job(state.ipc("candidate:slow.tok.en", start));
    let expired = start + VALIDATION_TIMEOUT;
    state.tick(expired);
    assert_eq!(state.view(), View::Recovery(Failure::Unreachable));
    assert!(
        finish_at(
            &mut state,
            stale,
            Validation::Accepted("slow.tok.en".to_owned()),
            expired + MS
        )
        .is_none()
    );
    assert_eq!(state.view(), View::Recovery(Failure::Unreachable));
    assert!(state.is_open());
}

#[test]
fn a_validation_timeout_drops_queued_candidates() {
    let start = Instant::now();
    let mut state = loaded(start);
    let _first = job(state.ipc("candidate:slow.tok.en", start));
    assert!(state.ipc("candidate:queued.tok.en", start + MS).is_none());
    let expired = start + VALIDATION_TIMEOUT;
    state.tick(expired);
    assert_eq!(state.view(), View::Recovery(Failure::Unreachable));
    assert!(!state.is_validating());
    assert_eq!(state.deadline(), None);
}

#[test]
fn the_next_queued_candidate_gets_a_full_timeout_from_when_it_starts() {
    let start = Instant::now();
    let mut state = loaded(start);
    let first = job(state.ipc("candidate:bad.tok.en", start));
    assert!(state.ipc("candidate:good.tok.en", start + MS).is_none());
    let rejected_at = start + Duration::from_secs(100);
    let second = job(finish_at(
        &mut state,
        first,
        Validation::Rejected,
        rejected_at,
    ));
    assert_eq!(second.candidate.as_str(), "good.tok.en");
    assert_eq!(state.deadline(), Some(rejected_at + VALIDATION_TIMEOUT));
}

#[test]
fn a_reposted_rejected_value_never_starts_a_job() {
    let start = Instant::now();
    let mut state = loaded(start);
    state.ipc("signed_in", start);
    let j = job(state.ipc("candidate:bad.tok.en", start));
    assert!(finish_at(&mut state, j, Validation::Rejected, start + MS).is_none());
    assert!(state.ipc("candidate:bad.tok.en", start + 2 * MS).is_none());
    assert!(!state.is_validating());
    let good = job(state.ipc("candidate:good.tok.en", start + 3 * MS));
    assert_eq!(
        exit(run_validation(&mut state, good, true)),
        BrowserLoginOutcome::Token("good.tok.en".to_owned())
    );
}

#[test]
fn a_rejected_value_reposted_during_a_check_is_skipped() {
    let start = Instant::now();
    let mut state = loaded(start);
    let bad = job(state.ipc("candidate:bad.tok.en", start));
    assert!(finish_at(&mut state, bad, Validation::Rejected, start + MS).is_none());
    let other = job(state.ipc("candidate:other.tok.en", start + 2 * MS));
    assert!(state.ipc("candidate:bad.tok.en", start + 3 * MS).is_none());
    assert!(finish_at(&mut state, other, Validation::Rejected, start + 4 * MS).is_none());
    assert!(!state.is_validating());
}

#[test]
fn a_value_posted_twice_during_a_check_is_checked_once() {
    let start = Instant::now();
    let mut state = loaded(start);
    let first = job(state.ipc("candidate:first.tok.en", start));
    assert!(state.ipc("candidate:next.tok.en", start).is_none());
    assert!(state.ipc("candidate:next.tok.en", start).is_none());
    let next = job(finish_at(
        &mut state,
        first,
        Validation::Rejected,
        start + MS,
    ));
    assert!(finish_at(&mut state, next, Validation::Rejected, start + 2 * MS).is_none());
    assert!(!state.is_validating());
}

#[test]
fn retry_after_a_load_failure_loads_discord_again_with_a_fresh_timeout() {
    let start = Instant::now();
    let mut state = BrowserState::new(start);
    state.event(Event::LoadFailed, start + MS);
    assert!(
        state
            .event(Event::Action(Action::Retry), start + 2 * MS)
            .is_none()
    );
    assert_eq!(state.view(), View::Discord);
    assert!(state.deadline().is_some());
    state.event(Event::LoadFailed, start + 3 * MS);
    assert_eq!(state.view(), View::Recovery(Failure::PageNotLoaded));
}

#[test]
fn load_failure_recovery_offers_qr_and_token_fallbacks() {
    let start = Instant::now();
    let mut state = BrowserState::new(start);
    state.event(Event::LoadFailed, start + MS);
    let outcome = exit(state.event(Event::Action(Action::UseQr), start + 2 * MS));
    assert!(matches!(outcome, BrowserLoginOutcome::UseQr));
}

#[test]
fn leaving_discord_without_a_check_reports_the_page_not_the_network() {
    let start = Instant::now();
    let mut state = loaded(start);
    assert!(state.event(Event::LeftDiscord, start + MS).is_none());
    assert_eq!(state.view(), View::Recovery(Failure::LeftDiscord));
    assert!(
        state
            .event(Event::Action(Action::Retry), start + 2 * MS)
            .is_none()
    );
    assert_eq!(state.view(), View::Discord);
}

#[test]
fn page_loaded_after_leaving_discord_restores_the_grace() {
    let start = Instant::now();
    let mut state = loaded(start);
    state.ipc("signed_in", start);
    let j = job(state.ipc("candidate:bad.tok.en", start));
    state.event(Event::LeftDiscord, start + MS);
    state.ipc("page_loaded", start + 2 * MS);
    assert!(finish_at(&mut state, j, Validation::Rejected, start + 3 * MS).is_none());
    assert_eq!(state.view(), View::Discord);
    assert_eq!(state.deadline(), Some(start + SIGNED_IN_GRACE));
    state.tick(start + SIGNED_IN_GRACE);
    assert_eq!(state.view(), View::Recovery(Failure::Rejected));
}

#[test]
fn a_check_that_fails_after_leaving_discord_reports_its_own_failure() {
    let start = Instant::now();
    let mut state = loaded(start);
    let j = job(state.ipc("candidate:bad.tok.en", start));
    state.event(Event::LeftDiscord, start + MS);
    assert_eq!(state.view(), View::Discord);
    assert!(finish_at(&mut state, j, Validation::Rejected, start + 2 * MS).is_none());
    assert_eq!(state.view(), View::Recovery(Failure::Rejected));
}

#[test]
fn sign_in_during_a_check_does_not_start_recovery_early() {
    let start = Instant::now();
    let mut state = loaded(start);
    let j = job(state.ipc("candidate:bad.tok.en", start));
    let signed_in_at = start + MS;
    assert!(state.ipc("signed_in", signed_in_at).is_none());
    assert!(finish_at(&mut state, j, Validation::Rejected, start + 2 * MS).is_none());
    assert_eq!(state.view(), View::Discord);
    assert_eq!(state.deadline(), Some(signed_in_at + SIGNED_IN_GRACE));
}

#[test]
fn candidates_on_the_recovery_screen_start_no_check() {
    let start = Instant::now();
    let mut state = loaded(start);
    let now = signed_in_past_grace(&mut state, start);
    assert_eq!(state.view(), View::Recovery(Failure::NoCandidate));
    assert!(state.ipc("candidate:late.tok.en", now).is_none());
    assert!(!state.is_validating());
    assert!(state.ipc("action:retry", now).is_none());
    let j = job(state.ipc("candidate:late.tok.en", now));
    assert_eq!(j.candidate.as_str(), "late.tok.en");
}
