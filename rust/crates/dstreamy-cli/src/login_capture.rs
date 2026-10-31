use std::collections::HashSet;
use std::fmt;

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

    #[test]
    fn candidate_debug_and_into_string() {
        let c = parse_candidate("candidate:secret.value.here").unwrap();
        let shown = format!("{c:?}");
        assert_eq!(shown, "Candidate(<redacted>)");
        assert_eq!(c.into_string(), "secret.value.here");
    }
}
