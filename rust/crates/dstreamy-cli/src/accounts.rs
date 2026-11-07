//! Choosing the account a command uses, and moving the legacy token into one (ADR 0005).

use std::path::PathBuf;
use std::time::Duration;

use dstreamy_core::{AccountUser, ResolvedToken, TokenSource};
use dstreamy_discord::CurrentUser;

use crate::discord::{self, Persistence};
use crate::lock::{LockError, SessionLock};
use crate::{CliError, Ctx};

/// The longest the migration may take, its lock wait and user lookup together, before the legacy token is used as is.
pub const MIGRATION_LIMIT: Duration = Duration::from_secs(15);
/// For `serve`, which shouldn't hold back its socket, and `daemon`, whose app gives `logged_in` 10 s.
pub const SHORT_MIGRATION_LIMIT: Duration = Duration::from_secs(5);
/// The user lookup gets at least this, apart from the lock wait.
const MIN_LOOKUP: Duration = Duration::from_secs(3);

/// The token and account `ctx` stands for: `DSTREAMY_TOKEN`, `--account`, the default account, or the legacy file.
pub fn resolve(ctx: &Ctx) -> Result<ResolvedToken, CliError> {
    ctx.paths
        .resolve_token(ctx.account.as_deref())?
        .ok_or(CliError::NotLoggedIn)
}

pub fn lock_dir(ctx: &Ctx, source: &TokenSource) -> PathBuf {
    ctx.paths.accounts().lock_dir(source)
}

/// The session lock of the account a connection made from `source` streams with; never recreates a removed account.
pub fn session_lock(ctx: &Ctx, source: &TokenSource) -> Result<SessionLock, LockError> {
    let accounts = ctx.paths.accounts();
    let dir = lock_dir(ctx, source);
    let Some(name) = source.account() else {
        std::fs::create_dir_all(&dir).map_err(|e| LockError::Io {
            path: dir.clone(),
            source: e,
        })?;
        return SessionLock::acquire(&dir);
    };
    let gone = || LockError::AccountGone(name.to_owned());
    if !accounts.exists(name) {
        return Err(gone());
    }
    let lock = SessionLock::acquire(&dir).map_err(|e| match e {
        LockError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => gone(),
        e => e,
    })?;
    if accounts.load_token(name).ok().flatten().is_none() {
        return Err(gone());
    }
    Ok(lock)
}

/// The lock that keeps an account from being removed while it streams; works without a token.
pub fn removal_lock(ctx: &Ctx, name: &str) -> Result<SessionLock, LockError> {
    let dir = ctx.paths.accounts().account_dir(name);
    SessionLock::acquire(&dir).map_err(|e| match e {
        LockError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            LockError::AccountGone(name.to_owned())
        }
        e => e,
    })
}

/// How long account changes wait for `accounts.lock`.
pub const STORE_LOCK_WAIT: Duration = Duration::from_secs(2);

/// Holds `accounts.lock` for a change of the saved accounts or the default.
pub fn store_lock(ctx: &Ctx) -> Result<dstreamy_core::accounts::StoreLock, CliError> {
    let accounts = ctx.paths.accounts();
    accounts.store_lock(STORE_LOCK_WAIT)?.ok_or_else(|| {
        dstreamy_core::ConfigError::StoreBusy(accounts.config_dir().join("accounts.lock")).into()
    })
}

pub fn account_user(user: &CurrentUser) -> AccountUser {
    AccountUser {
        id: user.id.clone(),
        username: user.username.clone(),
        global_name: user.global_name.clone(),
    }
}

/// Fills in the user of an account that lacks it, as after an offline migration.
pub fn remember_user(ctx: &Ctx, resolved: &ResolvedToken, user: &CurrentUser) {
    let Some(name) = resolved.source.account() else {
        return;
    };
    if let Err(e) = ctx
        .paths
        .accounts()
        .remember_user(name, &resolved.token, &account_user(user))
    {
        tracing::warn!("saving the user of account {name}: {e}");
    }
}

/// Moves `<config-dir>/token` into an account once; a failure leaves it in place, warns and is retried next time.
pub async fn migrate(ctx: &Ctx, limit: Duration) {
    let accounts = ctx.paths.accounts();
    if !accounts.legacy_token_file().exists() {
        return;
    }
    let migrated = async {
        let started = std::time::Instant::now();
        let lock_wait = limit
            .saturating_sub(MIN_LOOKUP)
            .max(Duration::from_millis(500));
        let Some(migration) = accounts.begin_migration(lock_wait)? else {
            return Ok(None);
        };
        let left = limit.saturating_sub(started.elapsed()).max(MIN_LOOKUP);
        let user = lookup(ctx, migration.token(), left).await;
        migration.finish(user.as_ref()).map(Some)
    };
    match migrated.await {
        Ok(Some(name)) => tracing::info!("moved the saved token into account {name}"),
        Ok(None) => {}
        Err(e) => tracing::warn!(
            "the saved token stays in {} for now: {e}",
            accounts.legacy_token_file().display()
        ),
    }
}

async fn lookup(ctx: &Ctx, token: &str, limit: Duration) -> Option<AccountUser> {
    let connect = discord::connect_with(ctx, token.to_owned(), Persistence::Brief);
    match tokio::time::timeout(limit, connect).await {
        Ok(Ok(client)) => {
            let user = account_user(client.user());
            client.close().await;
            Some(user)
        }
        Ok(Err(e)) => {
            tracing::warn!("looking up the saved token's user: {e}");
            None
        }
        Err(_) => {
            tracing::warn!("looking up the saved token's user timed out");
            None
        }
    }
}
