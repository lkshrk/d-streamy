use std::collections::HashSet;
use std::fmt;
use std::future::Future;

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
}

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
