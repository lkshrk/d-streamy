use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use dstreamy_discord::CurrentUser;
use futures_util::FutureExt;

use crate::CliError;
use crate::discord::BRIEF_VALIDATION_TIMEOUT;

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
    PageLoaded,
    LoadFailed,
    LeftDiscord,
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

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::UseQr => "qr",
            Self::UseToken => "token",
            Self::Cancel => "cancel",
        }
    }
}

pub const RECOVERY_ACTIONS: [Action; 4] = [
    Action::Retry,
    Action::UseQr,
    Action::UseToken,
    Action::Cancel,
];

/// What the recovery page offers: the QR and token logins need a terminal to run in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    WithTerminal,
    WindowOnly,
}

impl Recovery {
    pub fn actions(self) -> &'static [Action] {
        match self {
            Self::WithTerminal => &RECOVERY_ACTIONS,
            Self::WindowOnly => &[Action::Retry, Action::Cancel],
        }
    }
}

/// `dstreamy login --browser --window-only` exits with this when the window was closed without a login.
pub const CANCELLED_EXIT: u8 = 3;

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
        "page_loaded" => Some(Event::PageLoaded),
        "close" => Some(Event::Close),
        _ => match message.strip_prefix("action:") {
            Some(name) => RECOVERY_ACTIONS
                .into_iter()
                .find(|action| action.name() == name)
                .map(Event::Action),
            None => parse_candidate(message).map(Event::Candidate),
        },
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

    fn contains(&self, candidate: &Candidate) -> bool {
        self.0.contains(&candidate.0)
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
    Rejected,
    Transient,
    Accepted(T),
}

impl<T> fmt::Debug for Validation<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
    seen.insert(&candidate);
    match validator(candidate.as_str().to_owned()).await {
        Ok(value) => Validation::Accepted(value),
        Err(ValidationError::Rejected) => Validation::Rejected,
        Err(ValidationError::Transient(_)) => {
            seen.forget(&candidate);
            Validation::Transient
        }
    }
}

/// Like `validate_candidate`, but a panicking validator yields a retryable `Transient` result.
pub async fn validate_job<T, E, F, Fut>(
    mut seen: SeenCandidates,
    candidate: Candidate,
    validator: F,
) -> (SeenCandidates, Validation<T>)
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<T, ValidationError<E>>>,
{
    let retry = candidate.clone();
    let checked = AssertUnwindSafe(validate_candidate(&mut seen, candidate, validator))
        .catch_unwind()
        .await;
    match checked {
        Ok(result) => (seen, result),
        Err(_) => {
            tracing::error!("checking the browser login session panicked");
            seen.forget(&retry);
            (seen, Validation::Transient)
        }
    }
}

/// How long after Discord reports a signed-in page a valid candidate may still arrive.
pub const SIGNED_IN_GRACE: Duration = Duration::from_secs(20);
pub const PAGE_LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// A token the gateway accepted, with the account it belongs to.
#[derive(PartialEq, Eq)]
pub struct VerifiedLogin {
    pub token: String,
    pub user: CurrentUser,
}

impl fmt::Debug for VerifiedLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifiedLogin")
            .field("token", &"<redacted>")
            .field("user", &self.user)
            .finish()
    }
}

#[derive(PartialEq, Eq)]
pub enum BrowserLoginOutcome {
    Token(VerifiedLogin),
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
    LeftDiscord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginMode {
    Qr,
    Browser,
    Token,
}

pub fn login_mode(token_flag: bool, browser_flag: bool, stdin_is_terminal: bool) -> LoginMode {
    if browser_flag {
        LoginMode::Browser
    } else if token_flag || !stdin_is_terminal {
        LoginMode::Token
    } else {
        LoginMode::Qr
    }
}

/// What the `login` command does after the browser window has closed.
pub enum FollowUp {
    Login(VerifiedLogin),
    Qr,
    ReadToken,
}

pub fn follow_up(outcome: BrowserLoginOutcome) -> Result<FollowUp, CliError> {
    match outcome {
        BrowserLoginOutcome::Token(verified) => Ok(FollowUp::Login(verified)),
        BrowserLoginOutcome::UseQr => Ok(FollowUp::Qr),
        BrowserLoginOutcome::UseToken => Ok(FollowUp::ReadToken),
        BrowserLoginOutcome::Cancelled => Err(CliError::BrowserLoginCancelled),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Discord,
    Recovery(Failure),
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
    validation_deadline: Option<Instant>,
    page_failure: Option<Failure>,
    capture_expired: bool,
    validation_expired: bool,
    last_failure: Option<Failure>,
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
            validation_deadline: None,
            page_failure: None,
            capture_expired: false,
            validation_expired: false,
            last_failure: None,
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
        !self.finished && self.seen.is_none()
    }

    /// Never in the past after `tick`, so the event loop can always wait until it.
    pub fn deadline(&self) -> Option<Instant> {
        [
            self.load_deadline,
            self.capture_deadline,
            self.validation_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub fn verifying(&self) -> bool {
        self.view == View::Discord && self.is_validating()
    }

    /// Discord's full client after sign-in does not fit the login window, so it stays hidden.
    pub fn window_visible(&self) -> bool {
        !self.finished && !(self.view == View::Discord && self.signed_in)
    }

    pub fn ipc(&mut self, message: &str, now: Instant) -> Option<Command> {
        let event = parse_event(message)?;
        self.event(event, now)
    }

    pub fn page_loaded(&mut self) {
        if self.view == View::Discord {
            self.load_deadline = None;
            self.page_failure = None;
        }
    }

    pub fn event(&mut self, event: Event, now: Instant) -> Option<Command> {
        if self.finished {
            return None;
        }
        match event {
            Event::Candidate(candidate) => {
                self.page_loaded();
                self.candidate(candidate, now)
            }
            Event::SignedIn => {
                self.page_loaded();
                if !self.signed_in && self.view == View::Discord {
                    self.signed_in = true;
                    self.capture_deadline = Some(now + SIGNED_IN_GRACE);
                }
                None
            }
            Event::PageLoaded => {
                self.page_loaded();
                None
            }
            Event::LoadFailed => {
                self.page_failed(Failure::PageNotLoaded);
                None
            }
            Event::LeftDiscord => {
                self.page_failed(Failure::LeftDiscord);
                None
            }
            Event::Close => self.close(),
            Event::Action(action) => self.action(action, now),
        }
    }

    fn action(&mut self, action: Action, now: Instant) -> Option<Command> {
        match action {
            Action::Cancel => self.close(),
            _ if self.view == View::Discord => None,
            Action::Retry => {
                self.generation += 1;
                self.seen = Some(SeenCandidates::default());
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
        }
    }

    pub fn close(&mut self) -> Option<Command> {
        if self.finished {
            return None;
        }
        self.request_exit(BrowserLoginOutcome::Cancelled)
    }

    fn page_failed(&mut self, failure: Failure) {
        if self.view == View::Discord {
            self.load_deadline = None;
            self.page_failure = Some(failure);
            self.last_failure = None;
            self.settle();
        }
    }

    pub fn tick(&mut self, now: Instant) {
        if self.finished {
            return;
        }
        if self.load_deadline.is_some_and(|d| now >= d) {
            self.load_deadline = None;
            self.page_failure = Some(Failure::PageNotLoaded);
        }
        if self.capture_deadline.is_some_and(|d| now >= d) {
            self.capture_deadline = None;
            self.capture_expired = true;
        }
        if self.validation_deadline.is_some_and(|d| now >= d) {
            self.validation_deadline = None;
            self.validation_expired = true;
            self.generation += 1;
            self.seen = Some(SeenCandidates::default());
            self.queue.clear();
            self.last_failure = Some(Failure::Unreachable);
        }
        self.settle();
    }

    pub fn validated(
        &mut self,
        generation: u64,
        seen: SeenCandidates,
        result: Validation<VerifiedLogin>,
        now: Instant,
    ) -> Option<Command> {
        if self.finished {
            return None;
        }
        if generation != self.generation {
            return None;
        }
        self.validation_deadline = None;
        self.seen = Some(seen);
        match result {
            Validation::Accepted(verified) => {
                return Some(self.exit(BrowserLoginOutcome::Token(verified)));
            }
            Validation::Rejected => self.last_failure = Some(Failure::Rejected),
            Validation::Transient => self.last_failure = Some(Failure::Unreachable),
        }
        self.start_next(now).or_else(|| {
            self.settle();
            None
        })
    }

    fn candidate(&mut self, candidate: Candidate, now: Instant) -> Option<Command> {
        if self.view != View::Discord {
            return None;
        }
        self.queue.push_back(candidate);
        if self.is_validating() {
            return None;
        }
        self.start_next(now)
    }

    fn already_seen(&self, candidate: &Candidate) -> bool {
        self.seen
            .as_ref()
            .is_some_and(|seen| seen.contains(candidate))
    }

    fn start_next(&mut self, now: Instant) -> Option<Command> {
        let candidate = loop {
            let candidate = self.queue.pop_front()?;
            if !self.already_seen(&candidate) {
                break candidate;
            }
        };
        let seen = self.seen.take()?;
        self.validation_deadline = Some(now + BRIEF_VALIDATION_TIMEOUT);
        Some(Command::Validate(Job {
            generation: self.generation,
            seen,
            candidate,
        }))
    }

    /// Shows recovery for an expired deadline once no candidate check is left.
    fn settle(&mut self) {
        if self.view != View::Discord || self.is_validating() || !self.queue.is_empty() {
            return;
        }
        let failure = if let Some(page_failure) = self.page_failure {
            self.last_failure.unwrap_or(page_failure)
        } else if self.capture_expired || self.validation_expired {
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
        self.validation_deadline = None;
        self.page_failure = None;
        self.capture_expired = false;
        self.validation_expired = false;
    }

    fn request_exit(&mut self, outcome: BrowserLoginOutcome) -> Option<Command> {
        self.generation += 1;
        self.queue.clear();
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
    fn login_mode_defaults_to_qr_on_a_terminal() {
        assert_eq!(login_mode(false, false, true), LoginMode::Qr);
    }

    #[test]
    fn login_mode_browser_flag_selects_the_browser() {
        assert_eq!(login_mode(false, true, true), LoginMode::Browser);
        assert_eq!(login_mode(false, true, false), LoginMode::Browser);
    }

    #[test]
    fn login_mode_token_flag_or_piped_stdin_selects_token_input() {
        assert_eq!(login_mode(true, false, true), LoginMode::Token);
        assert_eq!(login_mode(true, false, false), LoginMode::Token);
        assert_eq!(login_mode(false, false, false), LoginMode::Token);
    }

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
    async fn rejected_and_accepted_candidates_are_remembered_as_seen() {
        let c = parse_candidate("candidate:aaa.bbb.ccc").unwrap();
        let mut seen = SeenCandidates::default();
        let rejected = validate_candidate(&mut seen, c.clone(), |_: String| async {
            Err::<String, ValidationError<()>>(ValidationError::Rejected)
        })
        .await;
        assert_eq!(rejected, Validation::Rejected);
        assert!(!seen.insert(&c));
        let mut seen = SeenCandidates::default();
        let accepted = validate_candidate(&mut seen, c.clone(), |t| async move {
            Ok::<_, ValidationError<()>>(t)
        })
        .await;
        assert!(matches!(accepted, Validation::Accepted(_)));
        assert!(!seen.insert(&c));
    }

    #[tokio::test]
    async fn a_panicking_check_is_a_retryable_transient_result() {
        async fn explode(_: String) -> Result<String, ValidationError<()>> {
            panic!("validator bug")
        }
        let c = parse_candidate("candidate:aaa.bbb.ccc").unwrap();
        let (mut seen, result) = validate_job(SeenCandidates::default(), c.clone(), explode).await;
        assert_eq!(result, Validation::Transient);
        assert!(seen.insert(&c), "a panicked check must stay retryable");
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
    fn the_window_hides_once_discord_reports_sign_in_and_until_recovery() {
        let now = Instant::now();
        let mut state = BrowserState::new(now);
        state.page_loaded();
        assert!(state.window_visible());
        assert!(state.ipc("signed_in", now).is_none());
        assert!(!state.window_visible());
        assert!(state.ipc("candidate:aaa.bbb.ccc", now).is_some());
        assert!(!state.window_visible());
        state.tick(now + BRIEF_VALIDATION_TIMEOUT);
        assert_eq!(state.view(), View::Recovery(Failure::Unreachable));
        assert!(state.window_visible());
        assert!(state.ipc("action:retry", now).is_none());
        assert!(state.window_visible());
    }

    #[test]
    fn a_check_before_sign_in_keeps_the_window_until_sign_in_is_reported() {
        let now = Instant::now();
        let mut state = BrowserState::new(now);
        state.page_loaded();
        assert!(state.ipc("candidate:aaa.bbb.ccc", now).is_some());
        assert!(state.verifying() && state.window_visible());
        assert!(state.ipc("signed_in", now).is_none());
        assert!(!state.window_visible());
    }

    #[test]
    fn sign_in_without_a_session_shows_the_window_again_after_the_grace() {
        let now = Instant::now();
        let mut state = BrowserState::new(now);
        state.page_loaded();
        assert!(state.ipc("signed_in", now).is_none());
        state.tick(now + SIGNED_IN_GRACE);
        assert_eq!(state.view(), View::Recovery(Failure::NoCandidate));
        assert!(state.window_visible());
    }

    #[test]
    fn a_finished_login_has_no_window() {
        let now = Instant::now();
        let mut state = BrowserState::new(now);
        assert!(state.close().is_some());
        assert!(!state.window_visible());
    }

    #[test]
    fn candidate_debug_and_into_string() {
        let c = parse_candidate("candidate:secret.value.here").unwrap();
        let shown = format!("{c:?}");
        assert_eq!(shown, "Candidate(<redacted>)");
        assert_eq!(c.into_string(), "secret.value.here");
    }
}
