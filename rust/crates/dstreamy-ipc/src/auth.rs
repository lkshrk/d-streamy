/// Whether a peer may talk to the service: only the user the service runs as, root included.
pub fn authorize<P: PartialEq + ?Sized>(own: &P, peer: &P) -> bool {
    own == peer
}

#[cfg(test)]
mod tests {
    use super::authorize;

    #[test]
    fn only_the_same_uid_is_authorized() {
        let cases: [(u32, u32, bool); 8] = [
            (1000, 1000, true),
            (0, 0, true),
            (1000, 1001, false),
            (1001, 1000, false),
            (1000, 0, false),
            (0, 1000, false),
            (1000, 65534, false),
            (u32::MAX, 0, false),
        ];
        for (own, peer, expected) in cases {
            assert_eq!(authorize(&own, &peer), expected, "own {own}, peer {peer}");
        }
    }
}
