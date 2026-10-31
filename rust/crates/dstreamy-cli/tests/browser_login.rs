use std::time::{Duration, Instant};

use dstreamy_cli::login_capture::{
    Action, BrowserLoginOutcome, BrowserState, Command, Event, Failure, Job, SIGNED_IN_GRACE,
    SeenCandidates, Validation, View, parse_event,
};

fn event(message: &str) -> Event {
    parse_event(message).expect("valid IPC message")
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
    state.validated(job.generation, job.seen, result)
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

fn signed_in_past_grace(state: &mut BrowserState) -> Instant {
    let start = Instant::now();
    assert!(state.event(event("signed_in"), start).is_none());
    let later = start + SIGNED_IN_GRACE + Duration::from_millis(1);
    state.tick(later);
    later
}

#[test]
fn failed_capture_keeps_dialog_open_and_exposes_recovery_actions() {
    let mut state = BrowserState::default();
    signed_in_past_grace(&mut state);
    assert!(state.is_open());
    assert_eq!(state.view(), View::Recovery(Failure::NoCandidate));
    assert_eq!(
        state.actions(),
        [
            Action::Retry,
            Action::UseQr,
            Action::UseToken,
            Action::Cancel
        ]
    );
}

#[test]
fn discord_page_stays_while_waiting_and_before_the_grace_period() {
    let mut state = BrowserState::default();
    assert_eq!(state.view(), View::Discord);
    assert!(state.actions().is_empty());
    let start = Instant::now();
    assert!(state.event(event("signed_in"), start).is_none());
    assert_eq!(state.deadline(), Some(start + SIGNED_IN_GRACE));
    state.tick(start + SIGNED_IN_GRACE - Duration::from_millis(1));
    assert_eq!(state.view(), View::Discord);
}

#[test]
fn close_and_escape_produce_cancelled_without_token() {
    assert_eq!(
        exit(BrowserState::default().close()),
        BrowserLoginOutcome::Cancelled
    );
    assert_eq!(
        exit(BrowserState::default().event(event("close"), Instant::now())),
        BrowserLoginOutcome::Cancelled
    );
    let mut state = BrowserState::default();
    signed_in_past_grace(&mut state);
    assert_eq!(
        exit(state.event(event("action:cancel"), Instant::now())),
        BrowserLoginOutcome::Cancelled
    );
    assert!(!state.is_open());
}

#[test]
fn malformed_and_unknown_ipc_leave_the_state_waiting() {
    for message in [
        "",
        "candidate:",
        "candidate: ",
        "token:a.b.c",
        "action:",
        "action:nope",
    ] {
        assert!(parse_event(message).is_none(), "{message:?} must not parse");
    }
    let state = BrowserState::default();
    assert_eq!(state.view(), View::Discord);
    assert!(state.is_open());
    assert!(!state.is_validating());
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
    let mut state = BrowserState::default();
    for message in ["action:retry", "action:qr", "action:token"] {
        assert!(state.event(event(message), Instant::now()).is_none());
        assert!(state.is_open());
        assert_eq!(state.view(), View::Discord);
    }
}

#[test]
fn rejected_candidate_does_not_close_and_shows_recovery_after_sign_in() {
    let mut state = BrowserState::default();
    let now = Instant::now();
    state.event(event("signed_in"), now);
    let j = job(state.event(event("candidate:bad.tok.en"), now));
    assert!(state.is_validating());
    assert!(run_validation(&mut state, j, false).is_none());
    assert!(state.is_open());
    assert_eq!(state.view(), View::Recovery(Failure::Rejected));
    assert_eq!(state.deadline(), None);
}

#[test]
fn rejected_candidate_before_sign_in_keeps_the_discord_page() {
    let mut state = BrowserState::default();
    let j = job(state.event(event("candidate:bad.tok.en"), Instant::now()));
    assert!(run_validation(&mut state, j, false).is_none());
    assert!(state.is_open());
    assert_eq!(state.view(), View::Discord);
}

#[test]
fn transient_failure_does_not_close_and_offers_retry() {
    let mut state = BrowserState::default();
    let j = job(state.event(event("candidate:good.tok.en"), Instant::now()));
    assert!(finish(&mut state, j, Validation::Transient).is_none());
    assert!(state.is_open());
    assert_eq!(state.view(), View::Recovery(Failure::Unreachable));
    assert_eq!(state.actions()[0], Action::Retry);
}

#[test]
fn pending_result_is_not_presented_as_a_failure() {
    let mut state = BrowserState::default();
    let j = job(state.event(event("candidate:dup.tok.en"), Instant::now()));
    assert!(finish(&mut state, j, Validation::Pending).is_none());
    assert_eq!(state.view(), View::Discord);
    assert!(!state.is_validating());
}

#[test]
fn accepted_candidate_becomes_the_token() {
    let mut state = BrowserState::default();
    let j = job(state.event(event("candidate:good.tok.en"), Instant::now()));
    assert_eq!(
        exit(run_validation(&mut state, j, true)),
        BrowserLoginOutcome::Token("good.tok.en".to_owned())
    );
    assert!(!state.is_open());
}

#[test]
fn candidates_arriving_during_validation_are_queued_then_validated() {
    let mut state = BrowserState::default();
    let now = Instant::now();
    state.event(event("signed_in"), now);
    let first = job(state.event(event("candidate:bad.tok.en"), now));
    assert!(state.event(event("candidate:good.tok.en"), now).is_none());
    let second = job(run_validation(&mut state, first, false));
    assert_eq!(second.candidate.as_str(), "good.tok.en");
    assert_eq!(state.view(), View::Discord);
    assert_eq!(
        exit(run_validation(&mut state, second, true)),
        BrowserLoginOutcome::Token("good.tok.en".to_owned())
    );
}

#[test]
fn grace_period_waits_for_an_in_flight_validation() {
    let mut state = BrowserState::default();
    let start = Instant::now();
    state.event(event("signed_in"), start);
    let j = job(state.event(event("candidate:slow.tok.en"), start));
    state.tick(start + SIGNED_IN_GRACE * 2);
    assert_eq!(state.view(), View::Discord);
    assert_eq!(
        exit(run_validation(&mut state, j, true)),
        BrowserLoginOutcome::Token("slow.tok.en".to_owned())
    );
}

#[test]
fn cancel_while_validating_waits_and_discards_the_result() {
    let mut state = BrowserState::default();
    let j = job(state.event(event("candidate:good.tok.en"), Instant::now()));
    assert!(state.close().is_none());
    assert!(state.is_open());
    assert!(state.is_validating());
    assert_eq!(state.view(), View::Closing);
    assert!(
        state
            .event(event("candidate:other.tok.en"), Instant::now())
            .is_none()
    );
    assert_eq!(
        exit(run_validation(&mut state, j, true)),
        BrowserLoginOutcome::Cancelled
    );
    assert!(!state.is_open());
}

#[test]
fn qr_and_token_choices_wait_for_validation_too() {
    for (message, outcome) in [
        ("action:qr", BrowserLoginOutcome::UseQr),
        ("action:token", BrowserLoginOutcome::UseToken),
    ] {
        let mut state = BrowserState::default();
        let now = signed_in_past_grace(&mut state);
        let j = job(state.event(event("candidate:late.tok.en"), now));
        assert!(state.event(event(message), now).is_none());
        assert_eq!(state.view(), View::Closing);
        assert_eq!(exit(run_validation(&mut state, j, true)), outcome);
    }
}

#[test]
fn qr_and_token_choices_exit_immediately_when_idle() {
    for (message, outcome) in [
        ("action:qr", BrowserLoginOutcome::UseQr),
        ("action:token", BrowserLoginOutcome::UseToken),
    ] {
        let mut state = BrowserState::default();
        let now = signed_in_past_grace(&mut state);
        assert_eq!(exit(state.event(event(message), now)), outcome);
    }
}

#[test]
fn retry_resets_deadline_seen_candidates_and_view() {
    let mut state = BrowserState::default();
    let now = Instant::now();
    state.event(event("signed_in"), now);
    let j = job(state.event(event("candidate:same.tok.en"), now));
    assert!(run_validation(&mut state, j, false).is_none());
    assert_eq!(state.view(), View::Recovery(Failure::Rejected));

    assert!(state.event(event("action:retry"), now).is_none());
    assert_eq!(state.view(), View::Discord);
    assert_eq!(state.deadline(), None);
    state.tick(now + SIGNED_IN_GRACE * 2);
    assert_eq!(state.view(), View::Discord);

    let again = job(state.event(event("candidate:same.tok.en"), now));
    let mut seen = again.seen;
    assert!(
        seen.insert(&again.candidate),
        "retry must forget seen candidates"
    );
}

#[test]
fn retry_during_validation_discards_the_stale_result() {
    let mut state = BrowserState::default();
    let now = signed_in_past_grace(&mut state);
    let stale = job(state.event(event("candidate:old.tok.en"), now));
    assert!(state.event(event("action:retry"), now).is_none());
    assert_eq!(state.view(), View::Discord);
    assert!(state.event(event("candidate:new.tok.en"), now).is_none());
    let next = job(run_validation(&mut state, stale, true));
    assert!(state.is_open());
    assert_eq!(next.candidate.as_str(), "new.tok.en");
    let mut seen = next.seen;
    assert!(seen.insert(&next.candidate));
}

#[test]
fn lost_validation_restores_a_usable_state() {
    let mut state = BrowserState::default();
    let j = job(state.event(event("candidate:good.tok.en"), Instant::now()));
    assert!(
        state
            .validated(
                j.generation,
                SeenCandidates::default(),
                Validation::Transient
            )
            .is_none()
    );
    assert!(!state.is_validating());
    assert!(
        state
            .event(event("candidate:good.tok.en"), Instant::now())
            .is_some()
    );
}

#[test]
fn outcome_debug_never_shows_the_token() {
    let shown = format!(
        "{:?}",
        BrowserLoginOutcome::Token("secret.tok.en".to_owned())
    );
    assert!(!shown.contains("secret"));
}
