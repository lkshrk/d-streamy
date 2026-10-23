use std::future::Future;
use std::time::Duration;

pub const SOCKET_TEST_TIMEOUT: Duration = Duration::from_secs(20);

pub async fn within<F: Future>(test: F) -> F::Output {
    tokio::time::timeout(SOCKET_TEST_TIMEOUT, test)
        .await
        .expect("socket test exceeded its 20 s limit")
}
