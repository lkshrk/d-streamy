//! Choosing the account a command uses, and moving the legacy token into one (ADR 0005).

use std::path::PathBuf;
use std::time::Duration;

use dstreamy_core::{AccountUser, ResolvedToken, TokenSource};
use dstreamy_discord::CurrentUser;

use crate::discord::{self, Persistence};
use crate::lock::{LockError, SessionLock};
use crate::{CliError, Ctx};

/// How long the migration waits to learn the legacy token's user before naming the account `default`.
pub const MIGRATION_LOOKUP: Duration = Duration::from_secs(15);
/// Shorter for `daemon`, whose app gives `logged_in` 10 s.
pub const DAEMON_MIGRATION_LOOKUP: Duration = Duration::from_secs(5);

/// The token and account `ctx` stands for: `DSTREAMY_TOKEN`, `--account`, the default account, or the legacy file.
pub fn resolve(ctx: &Ctx) -> Result<ResolvedToken, CliError> {
    ctx.paths
        .resolve_token(ctx.account.as_deref())?
        .ok_or(CliError::NotLoggedIn)
}

pub fn lock_dir(ctx: &Ctx, source: &TokenSource) -> PathBuf {
    ctx.paths.accounts().lock_dir(source)
}

/// The session lock of the account a connection made from `source` streams with.
pub fn session_lock(ctx: &Ctx, source: &TokenSource) -> Result<SessionLock, LockError> {
    SessionLock::acquire(&lock_dir(ctx, source))
}

pub fn account_user(user: &CurrentUser) -> AccountUser {
    AccountUser {
        id: user.id.clone(),
        username: user.username.clone(),
        global_name: user.global_name.clone(),
    }
}

/// Fills in the user of an account that lacks it, as after an offline migration.
pub fn remember_user(ctx: &Ctx, source: &TokenSource, user: &CurrentUser) {
    let Some(name) = source.account() else { return };
    if let Err(e) = ctx
        .paths
        .accounts()
        .remember_user(name, &account_user(user))
    {
        tracing::warn!("saving the user of account {name}: {e}");
    }
}

/// Moves `<config-dir>/token` into an account once; a failure leaves it in place, warns and is retried next time.
pub async fn migrate(ctx: &Ctx, lookup_limit: Duration) {
    let accounts = ctx.paths.accounts();
    if !accounts.legacy_token_file().exists() {
        return;
    }
    let migrated = async {
        let Some(migration) = accounts.begin_migration()? else {
            return Ok(None);
        };
        let user = lookup(ctx, migration.token(), lookup_limit).await;
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
