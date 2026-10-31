use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::time::{Duration, Instant};

use crate::CliError;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Candidate(String);

impl Candidate {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Candidate(<redacted>)")
    }
}

#[derive(Debug)]
pub enum Event {
    Candidate(Candidate),
    SignedIn,
    Close,
    Action(Action),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Retry,
    UseQr,
    UseToken,
    Cancel,
}

pub const RECOVERY_ACTIONS: [Action; 4] = [
    Action::Retry,
    Action::UseQr,
    Action::UseToken,
    Action::Cancel,
];

pub fn parse_candidate(message: &str) -> Option<Candidate> {
    let value = message.strip_prefix("candidate:")?;
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return None;
    }
    Some(Candidate(value.to_owned()))
}

pub fn parse_event(message: &str) -> Option<Event> {
    match message {
        "signed_in" => Some(Event::SignedIn),
        "close" => Some(Event::Close),
        "action:retry" => Some(Event::Action(Action::Retry)),
        "action:qr" => Some(Event::Action(Action::UseQr)),
        "action:token" => Some(Event::Action(Action::UseToken)),
        "action:cancel" => Some(Event::Action(Action::Cancel)),
        _ => parse_candidate(message).map(Event::Candidate),
    }
}

#[derive(Default)]
pub struct SeenCandidates(HashSet<String>);

impl SeenCandidates {
    pub fn insert(&mut self, candidate: &Candidate) -> bool {
        self.0.insert(candidate.0.clone())
    }

    fn forget(&mut self, candidate: &Candidate) {
        self.0.remove(&candidate.0);
    }
}

pub enum ValidationError<E> {
    Rejected,
    Transient(E),
}

impl<E> fmt::Debug for ValidationError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected => f.write_str("ValidationError::Rejected"),
            Self::Transient(_) => f.write_str("ValidationError::Transient(<redacted>)"),
        }
    }
}

#[must_use]
#[derive(PartialEq, Eq)]
pub enum Validation<T> {
    Pending,
    Rejected,
    Transient,
    Accepted(T),
}

impl<T> fmt::Debug for Validation<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => f.write_str("Validation::Pending"),
            Self::Rejected => f.write_str("Validation::Rejected"),
            Self::Transient => f.write_str("Validation::Transient"),
            Self::Accepted(_) => f.write_str("Validation::Accepted(<redacted>)"),
        }
    }
}

pub async fn validate_candidate<T, E, F, Fut>(
    seen: &mut SeenCandidates,
    candidate: Candidate,
    validator: F,
) -> Validation<T>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<T, ValidationError<E>>>,
{
    if !seen.insert(&candidate) {
        return Validation::Pending;
    }
    match validator(candidate.as_str().to_owned()).await {
        Ok(value) => Validation::Accepted(value),
        Err(ValidationError::Rejected) => Validation::Rejected,
        Err(ValidationError::Transient(_)) => {
            seen.forget(&candidate);
            Validation::Transient
        }
    }
}

/// How long after Discord reports a signed-in page a valid candidate may still arrive.
pub const SIGNED_IN_GRACE: Duration = Duration::from_secs(20);
pub const PAGE_LOAD_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(PartialEq, Eq)]
pub enum BrowserLoginOutcome {
    Token(String),
    Cancelled,
    UseQr,
    UseToken,
}

impl fmt::Debug for BrowserLoginOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Token(_) => "BrowserLoginOutcome::Token(<redacted>)",
            Self::Cancelled => "BrowserLoginOutcome::Cancelled",
            Self::UseQr => "BrowserLoginOutcome::UseQr",
            Self::UseToken => "BrowserLoginOutcome::UseToken",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    NoCandidate,
    Rejected,
    Unreachable,
    PageNotLoaded,
}

/// What the `login` command does after the browser window has closed.
pub enum FollowUp {
    Login(String),
    Qr,
    ReadToken,
}

pub fn follow_up(outcome: BrowserLoginOutcome) -> Result<FollowUp, CliError> {
    match outcome {
        BrowserLoginOutcome::Token(token) => Ok(FollowUp::Login(token)),
        BrowserLoginOutcome::UseQr => Ok(FollowUp::Qr),
        BrowserLoginOutcome::UseToken => Ok(FollowUp::ReadToken),
        BrowserLoginOutcome::Cancelled => Err(CliError::BrowserLogin("cancelled".into())),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Discord,
    Recovery(Failure),
    Closing,
}

/// One candidate check; `seen` travels with it so no other check can run meanwhile.
pub struct Job {
    pub generation: u64,
    pub seen: SeenCandidates,
    pub candidate: Candidate,
}

pub enum Command {
    Validate(Job),
    Exit(BrowserLoginOutcome),
}

pub struct BrowserState {
    view: View,
    seen: Option<SeenCandidates>,
    generation: u64,
    queue: VecDeque<Candidate>,
    signed_in: bool,
    load_deadline: Option<Instant>,
    capture_deadline: Option<Instant>,
    load_expired: bool,
    capture_expired: bool,
    last_failure: Option<Failure>,
    exit_after_validation: Option<BrowserLoginOutcome>,
    finished: bool,
}

impl BrowserState {
    pub fn new(now: Instant) -> Self {
        Self {
            view: View::Discord,
            seen: Some(SeenCandidates::default()),
            generation: 0,
            queue: VecDeque::new(),
            signed_in: false,
            load_deadline: Some(now + PAGE_LOAD_TIMEOUT),
            capture_deadline: None,
            load_expired: false,
            capture_expired: false,
            last_failure: None,
            exit_after_validation: None,
            finished: false,
        }
    }

    pub fn view(&self) -> View {
        self.view
    }

    pub fn is_open(&self) -> bool {
        !self.finished
    }

    pub fn is_validating(&self) -> bool {
        self.seen.is_none()
    }

    pub fn actions(&self) -> &'static [Action] {
        match self.view {
            View::Recovery(_) => &RECOVERY_ACTIONS,
            View::Discord | View::Closing => &[],
        }
    }

    /// Never in the past after `tick`, so the event loop can always wait until it.
    pub fn deadline(&self) -> Option<Instant> {
        match (self.load_deadline, self.capture_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn verifying(&self) -> bool {
        self.view == View::Discord && self.is_validating()
    }

    pub fn ipc(&mut self, message: &str, now: Instant) -> Option<Command> {
        let event = parse_event(message)?;
        self.event(event, now)
    }

    pub fn page_loaded(&mut self) {
        if self.view == View::Discord {
            self.load_deadline = None;
        }
    }

    pub fn event(&mut self, event: Event, now: Instant) -> Option<Command> {
        if self.finished {
            return None;
        }
        match event {
            Event::Candidate(candidate) => {
                self.page_loaded();
                self.candidate(candidate)
            }
            Event::SignedIn => {
                self.page_loaded();
                if !self.signed_in && self.view == View::Discord {
                    self.signed_in = true;
                    self.capture_deadline = Some(now + SIGNED_IN_GRACE);
                }
                None
            }
            Event::Close => self.close(),
            Event::Action(action) => self.action(action, now),
        }
    }

    pub fn action(&mut self, action: Action, now: Instant) -> Option<Command> {
        if self.finished {
            return None;
        }
        if action == Action::Cancel {
            return self.close();
        }
        if !matches!(self.view, View::Recovery(_)) {
            return None;
        }
        match action {
            Action::Retry => {
                self.generation += 1;
                if self.seen.is_some() {
                    self.seen = Some(SeenCandidates::default());
                }
                self.queue.clear();
                self.signed_in = false;
                self.clear_deadlines();
                self.load_deadline = Some(now + PAGE_LOAD_TIMEOUT);
                self.last_failure = None;
                self.view = View::Discord;
                None
            }
            Action::UseQr => self.request_exit(BrowserLoginOutcome::UseQr),
            Action::UseToken => self.request_exit(BrowserLoginOutcome::UseToken),
            Action::Cancel => self.close(),
        }
    }

    pub fn close(&mut self) -> Option<Command> {
        if self.finished {
            return None;
        }
        self.request_exit(BrowserLoginOutcome::Cancelled)
    }

    pub fn tick(&mut self, now: Instant) {
        if self.finished {
            return;
        }
        if self.load_deadline.is_some_and(|d| now >= d) {
            self.load_deadline = None;
            self.load_expired = true;
        }
        if self.capture_deadline.is_some_and(|d| now >= d) {
            self.capture_deadline = None;
            self.capture_expired = true;
        }
        self.settle();
    }

    pub fn validated(
        &mut self,
        generation: u64,
        seen: SeenCandidates,
        result: Validation<String>,
    ) -> Option<Command> {
        if self.finished {
            return None;
        }
        let stale = generation != self.generation;
        self.seen = Some(if stale {
            SeenCandidates::default()
        } else {
            seen
        });
        if let Some(outcome) = self.exit_after_validation.take() {
            return Some(self.exit(outcome));
        }
        match result {
            Validation::Accepted(token) if !stale => {
                return Some(self.exit(BrowserLoginOutcome::Token(token)));
            }
            Validation::Rejected if !stale => self.last_failure = Some(Failure::Rejected),
            Validation::Transient if !stale => self.last_failure = Some(Failure::Unreachable),
            _ => {}
        }
        self.next().or_else(|| {
            self.settle();
            None
        })
    }

    fn candidate(&mut self, candidate: Candidate) -> Option<Command> {
        if self.exit_after_validation.is_some() {
            return None;
        }
        self.queue.push_back(candidate);
        if self.is_validating() {
            return None;
        }
        self.next()
    }

    fn next(&mut self) -> Option<Command> {
        let candidate = self.queue.pop_front()?;
        let seen = self.seen.take()?;
        Some(Command::Validate(Job {
            generation: self.generation,
            seen,
            candidate,
        }))
    }

    /// Shows recovery for an expired deadline once no candidate check is left.
    fn settle(&mut self) {
        if self.view != View::Discord
            || self.is_validating()
            || !self.queue.is_empty()
            || self.exit_after_validation.is_some()
        {
            return;
        }
        let failure = if self.load_expired {
            Failure::PageNotLoaded
        } else if self.capture_expired {
            self.last_failure.unwrap_or(Failure::NoCandidate)
        } else {
            return;
        };
        self.clear_deadlines();
        self.view = View::Recovery(failure);
    }

    fn clear_deadlines(&mut self) {
        self.load_deadline = None;
        self.capture_deadline = None;
        self.load_expired = false;
        self.capture_expired = false;
    }

    fn request_exit(&mut self, outcome: BrowserLoginOutcome) -> Option<Command> {
        self.clear_deadlines();
        if self.is_validating() {
            self.queue.clear();
            self.exit_after_validation = Some(outcome);
            self.view = View::Closing;
            return None;
        }
        Some(self.exit(outcome))
    }

    fn exit(&mut self, outcome: BrowserLoginOutcome) -> Command {
        self.finished = true;
        self.clear_deadlines();
        Command::Exit(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_protocol_accepts_authorization_values_and_rejects_controls() {
        assert!(parse_candidate("candidate:abc.def.ghi").is_some());
        assert!(parse_candidate("candidate:").is_none());
        assert!(parse_candidate("close").is_none());
        assert!(parse_candidate("candidate:two words").is_none());
        assert!(parse_candidate("candidate:tab\tsep").is_none());
    }

    #[test]
    fn parse_event_maps_the_three_messages() {
        assert!(matches!(parse_event("signed_in"), Some(Event::SignedIn)));
        assert!(matches!(parse_event("close"), Some(Event::Close)));
        match parse_event("candidate:abc.def.ghi") {
            Some(Event::Candidate(c)) => assert_eq!(c.as_str(), "abc.def.ghi"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parse_event_rejects_unknown_and_malformed() {
        assert!(parse_event("").is_none());
        assert!(parse_event("token:abc.def.ghi").is_none());
        assert!(parse_event("candidate:").is_none());
        assert!(parse_event("candidate:a b").is_none());
        assert!(parse_event("close ").is_none());
        assert!(parse_event("signed_in:1").is_none());
    }

    #[test]
    fn seen_candidates_reports_only_first_insert() {
        let mut seen = SeenCandidates::default();
        let a = parse_candidate("candidate:aaa.bbb.ccc").unwrap();
        let b = parse_candidate("candidate:ddd.eee.fff").unwrap();
        assert!(seen.insert(&a));
        assert!(!seen.insert(&a));
        assert!(seen.insert(&b));
    }

    #[tokio::test]
    async fn invalid_candidate_does_not_end_login_and_later_valid_candidate_wins() {
        let mut seen = SeenCandidates::default();
        let validator = |token: String| async move {
            if token == "good.tok.en" {
                Ok(token)
            } else {
                Err(ValidationError::<()>::Rejected)
            }
        };
        let bad = parse_candidate("candidate:bad.tok.en").unwrap();
        let good = parse_candidate("candidate:good.tok.en").unwrap();
        assert_eq!(
            validate_candidate(&mut seen, bad, validator).await,
            Validation::Rejected
        );
        assert_eq!(
            validate_candidate(&mut seen, good, validator).await,
            Validation::Accepted("good.tok.en".to_owned())
        );
    }

    #[tokio::test]
    async fn permanent_rejection_is_deduplicated() {
        let mut seen = SeenCandidates::default();
        let calls = std::cell::Cell::new(0);
        let validator = |_: String| {
            calls.set(calls.get() + 1);
            async { Err::<String, ValidationError<()>>(ValidationError::Rejected) }
        };
        let c = parse_candidate("candidate:aaa.bbb.ccc").unwrap();
        assert_eq!(
            validate_candidate(&mut seen, c.clone(), validator).await,
            Validation::Rejected
        );
        assert_eq!(
            validate_candidate(&mut seen, c, validator).await,
            Validation::Pending
        );
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test]
    async fn accepted_candidate_is_deduplicated() {
        let mut seen = SeenCandidates::default();
        let c = parse_candidate("candidate:aaa.bbb.ccc").unwrap();
        let ok = |t: String| async move { Ok::<_, ValidationError<()>>(t) };
        assert!(matches!(
            validate_candidate(&mut seen, c.clone(), ok).await,
            Validation::Accepted(_)
        ));
        assert_eq!(
            validate_candidate(&mut seen, c, ok).await,
            Validation::Pending
        );
    }

    #[tokio::test]
    async fn transient_failure_is_retryable_with_the_same_candidate() {
        let mut seen = SeenCandidates::default();
        let calls = std::cell::Cell::new(0);
        let validator = |token: String| {
            calls.set(calls.get() + 1);
            let first = calls.get() == 1;
            async move {
                if first {
                    Err(ValidationError::Transient("network down"))
                } else {
                    Ok(token)
                }
            }
        };
        let c = parse_candidate("candidate:good.tok.en").unwrap();
        let first: Validation<String> = validate_candidate(&mut seen, c.clone(), validator).await;
        assert_eq!(first, Validation::Transient);
        assert_eq!(
            validate_candidate(&mut seen, c, validator).await,
            Validation::Accepted("good.tok.en".to_owned())
        );
        assert_eq!(calls.get(), 2);
    }

    #[tokio::test]
    async fn control_character_candidate_reaches_validator_and_stays_recoverable() {
        let mut seen = SeenCandidates::default();
        let received = std::cell::RefCell::new(String::new());
        let weird = parse_candidate("candidate:abc\u{7}.def.ghi").unwrap();
        let rejected = validate_candidate(&mut seen, weird, |t| {
            *received.borrow_mut() = t;
            async { Err::<String, ValidationError<()>>(ValidationError::Rejected) }
        })
        .await;
        assert_eq!(rejected, Validation::Rejected);
        assert_eq!(*received.borrow(), "abc\u{7}.def.ghi");
        let good = parse_candidate("candidate:good.tok.en").unwrap();
        let accepted = validate_candidate(&mut seen, good, |t| async move {
            Ok::<_, ValidationError<()>>(t)
        })
        .await;
        assert_eq!(accepted, Validation::Accepted("good.tok.en".to_owned()));
    }

    #[test]
    fn validation_and_error_debug_never_show_payloads() {
        let accepted = Validation::Accepted("secret.tok.en".to_owned());
        assert_eq!(format!("{accepted:?}"), "Validation::Accepted(<redacted>)");
        let err: ValidationError<&str> = ValidationError::Transient("secret.tok.en");
        assert!(!format!("{err:?}").contains("secret"));
    }

    #[test]
    fn candidate_debug_and_into_string() {
        let c = parse_candidate("candidate:secret.value.here").unwrap();
        let shown = format!("{c:?}");
        assert_eq!(shown, "Candidate(<redacted>)");
        assert_eq!(c.into_string(), "secret.value.here");
    }
}
