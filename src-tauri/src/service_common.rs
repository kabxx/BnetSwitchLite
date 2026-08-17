use std::{collections::HashSet, time::Duration};

use crate::{
    contracts::{
        AccountKey, AccountSnapshot, DiscoveredAccount, OperationEvent, OperationKind,
        SnapshotStatus,
    },
    data_store::HiddenAccountKey,
    error::AppError,
};

pub(crate) fn emit(
    report: &impl Fn(OperationEvent),
    kind: OperationKind,
    phase: &str,
    title: &str,
    detail: &str,
    progress: u8,
) {
    report(OperationEvent {
        kind,
        phase: phase.into(),
        title: title.into(),
        detail: detail.into(),
        progress,
    });
}

pub(crate) fn account_snapshots(
    accounts: Vec<DiscoveredAccount>,
    hidden_accounts: &HashSet<HiddenAccountKey>,
    snapshot_status: impl Fn(&AccountKey) -> (SnapshotStatus, Option<u64>, Option<String>),
) -> Vec<AccountSnapshot> {
    accounts
        .into_iter()
        .filter(|account| !hidden_accounts.contains(&HiddenAccountKey::from(&account.key)))
        .map(|account| {
            let (snapshot_status, last_saved_at, note) = snapshot_status(&account.key);
            AccountSnapshot {
                id: account.key.stable_id(),
                region: region_label(&account.key.environment).to_owned(),
                environment: account.key.environment.clone(),
                key: account.key,
                battle_tag: if account.battle_tag.trim().is_empty() {
                    account.internal_name
                } else {
                    account.battle_tag
                },
                snapshot_status,
                last_saved_at,
                note,
            }
        })
        .collect()
}

pub(crate) fn login_evidence_ready(
    authenticated: bool,
    current: Option<&AccountKey>,
    target: &AccountKey,
) -> bool {
    authenticated && current == Some(target)
}

pub(crate) fn region_label(environment: &str) -> &'static str {
    let normalized = environment.trim().to_ascii_lowercase();
    if normalized == "cn"
        || normalized.starts_with("cn.")
        || normalized.contains("battlenet.com.cn")
        || normalized.contains("battle.net.cn")
    {
        "国服"
    } else {
        match normalized.split('.').next().unwrap_or_default() {
            "us" => "美服",
            "eu" => "欧服",
            "kr" => "亚服",
            "tw" => "台服",
            "sea" => "东南亚服",
            "global" => "国际服",
            _ => "未知区服",
        }
    }
}

pub(crate) fn wait_for_stable<T>(
    mut previous: T,
    timeout: Duration,
    interval: Duration,
    mut fingerprint: impl FnMut() -> Result<T, AppError>,
    error: AppError,
) -> Result<(), AppError>
where
    T: PartialEq,
{
    let started = std::time::Instant::now();
    let mut stable_samples = 0_u8;
    while started.elapsed() < timeout {
        std::thread::sleep(interval);
        let current = fingerprint()?;
        if current == previous {
            stable_samples += 1;
            if stable_samples >= 2 {
                return Ok(());
            }
        } else {
            stable_samples = 0;
            previous = current;
        }
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::{login_evidence_ready, region_label};
    use crate::contracts::AccountKey;

    fn account(environment: &str, account_id: &str) -> AccountKey {
        AccountKey {
            environment: environment.into(),
            account_id: account_id.into(),
        }
    }

    #[test]
    fn login_evidence_requires_authentication_and_exact_target() {
        let expected = account("us.actual.battle.net", "2");
        assert!(login_evidence_ready(true, Some(&expected), &expected));
        assert!(!login_evidence_ready(false, Some(&expected), &expected));
        assert!(!login_evidence_ready(true, None, &expected));
        assert!(!login_evidence_ready(
            true,
            Some(&account("us.actual.battle.net", "3")),
            &expected
        ));
        assert!(!login_evidence_ready(
            true,
            Some(&account("eu.actual.battle.net", "2")),
            &expected
        ));
    }

    #[test]
    fn region_labels_cover_shared_battle_net_environments() {
        assert_eq!(region_label("cn.actual.battlenet.com.cn"), "国服");
        assert_eq!(region_label("cn.actual.battle.net.cn"), "国服");
        assert_eq!(region_label("global"), "国际服");
        assert_eq!(region_label("kr.actual.battle.net"), "亚服");
        assert_eq!(region_label("us.actual.battle.net"), "美服");
        assert_eq!(region_label("eu.actual.battle.net"), "欧服");
        assert_eq!(region_label("tw.actual.battle.net"), "台服");
        assert_eq!(region_label("unknown.example"), "未知区服");
    }
}
