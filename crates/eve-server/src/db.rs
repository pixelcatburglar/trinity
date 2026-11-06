//! Accounts: logging in, accounts made at their first login, and the config
//! file's accounts.

use tracing::info;

use crate::store::{Store, StoreError, Tx, Value};

#[derive(Debug, Clone)]
pub struct Account {
    pub account_id: i64,
    pub name: String,
    pub role: i64,
    pub language_id: Option<String>,
}

fn account(tx_row: &crate::store::Row) -> Account {
    Account {
        account_id: tx_row.int("accountID").unwrap_or(0),
        name: tx_row.text("accountName").unwrap_or_default().to_string(),
        // bigint: roles above 2^31 exist (ROLE_TRANSLATIONEDITOR = 2^32).
        role: tx_row.int("role").unwrap_or(0),
        language_id: tx_row.text("languageID").map(String::from),
    }
}

/// The account `username` logs in to with `password`; None for a wrong
/// password, a banned account, or (unless the config makes accounts at their
/// first login) an unknown name.
pub fn authenticate(store: &Store, username: &str, password: &str) -> Result<Option<Account>, StoreError> {
    let found = store.read(|db| {
        db.table("account").find_one(&["accountName"], &[Value::Text(username.into())]).map(|r| {
            let banned = r.int("banned").unwrap_or(0) != 0;
            let matches = r.text("password") == Some(password);
            (account(&r), banned, matches)
        })
    });
    match found {
        Some((_, true, _)) => {
            info!("{username:?} is banned");
            Ok(None)
        }
        Some((account, false, true)) => Ok(Some(account)),
        Some((_, false, false)) => Ok(None),
        None if crate::config::auto_create_accounts() && !username.trim().is_empty() && !password.is_empty() => {
            create_account(store, username, password)
        }
        None => Ok(None),
    }
}

/// A player account made at its first login (`autoCreateAccounts`). Two
/// first logins with one name make one account: the second is checked
/// against the first's password like any login.
fn create_account(store: &Store, username: &str, password: &str) -> Result<Option<Account>, StoreError> {
    store.write(|tx| {
        let accounts = tx.table("account");
        if let Some(r) = accounts.find_one(&["accountName"], &[Value::Text(username.into())]) {
            let ok = r.text("password") == Some(password) && r.int("banned") == Some(0);
            return Ok(ok.then(|| account(&r)));
        }
        let id = tx.insert(
            "account",
            &[
                ("accountName", Value::Text(username.into())),
                ("password", Value::Text(password.into())),
                ("role", Value::Int(0)),
                ("createdAt", Value::Int(crate::bulkdata::filetime_now())),
            ],
        )?;
        info!("account {username:?} made at its first login");
        Ok(tx.table("account").row(id).map(|r| account(&r)))
    })
}

/// Makes the config file's accounts, or brings them into line with it:
/// password, GM roles and ban. Returns their accountIDs, in the file's order.
pub fn sync_accounts(store: &Store, accounts: &[crate::config::Account]) -> Result<Vec<i64>, StoreError> {
    store.write(|tx: &mut Tx| {
        // Accounts once made with IDs of their own leave the sequence behind.
        let max = tx.table("account").rows().filter_map(|r| r.int("accountID")).max().unwrap_or(1).max(1);
        if tx.db().sequence("account_accountID_seq").is_none_or(|v| v < max) {
            tx.setval("account_accountID_seq", max);
        }
        let now = crate::bulkdata::filetime_now();
        let mut ids = Vec::with_capacity(accounts.len());
        for a in accounts {
            let banned = Value::Int(i64::from(a.banned));
            let id = tx.upsert(
                "account",
                &["accountName"],
                &[
                    ("accountName", Value::Text(a.username.as_str().into())),
                    ("password", Value::Text(a.password.as_str().into())),
                    ("role", Value::Int(a.role())),
                    ("banned", banned.clone()),
                    ("createdAt", Value::Int(now)),
                ],
                &[
                    ("password", Value::Text(a.password.as_str().into())),
                    ("passwordAlgo", Value::Text("plain".into())),
                    ("role", Value::Int(a.role())),
                    ("banned", banned),
                ],
            )?;
            ids.push(tx.table("account").row(id).and_then(|r| r.int("accountID")).unwrap_or(0));
        }
        Ok(ids)
    })
}
