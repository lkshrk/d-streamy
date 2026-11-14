use rand::Rng;

pub(crate) fn random_hex(bytes: usize) -> String {
    let mut rng = rand::rng();
    (0..bytes)
        .map(|_| format!("{:02x}", rng.random::<u8>()))
        .collect()
}

/// A random id in the range of current Discord snowflakes.
pub(crate) fn random_snowflake() -> String {
    rand::rng()
        .random_range(1_000_000_000_000_000_000u64..2_000_000_000_000_000_000)
        .to_string()
}
