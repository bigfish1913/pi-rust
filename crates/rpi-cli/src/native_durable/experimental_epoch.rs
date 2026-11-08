//! Explicit CLI selection of the bounded v2 ledger, not of a storage backend.
use super::{capabilities, AgentHarness, Value};
use crate::args::Args;
use rpi_harness::durable::{experimental_ledger as ledger, ExperimentalDurablePolicy};

pub(crate) const FLAG: &str = "--experimental-durable-epoch";
const OWNER: &str = "native-cli";

// Match the reserved flag before early subcommands/remote dispatch. A remote
// credential/address consumed unconditionally is not a local option.
fn present(argv: &[String], flag: &str) -> bool {
    let remote = argv
        .iter()
        .enumerate()
        .any(|(i, a)| a.starts_with("--connect=") || (a == "--connect" && i + 1 < argv.len()));
    let mut tokens = argv.iter();
    while let Some(arg) = tokens.next() {
        if arg == flag || arg.strip_prefix(flag).is_some_and(|s| s.starts_with('=')) {
            return true;
        }
        if remote && matches!(arg.as_str(), "--connect" | "--token") {
            tokens.next();
        }
    }
    false
}

pub(crate) fn early_check(argv: &[String]) -> Result<(), String> {
    if !present(argv, FLAG) {
        return Ok(());
    }
    if matches!(
        argv.first().map(String::as_str),
        Some(
            "__rpi_dev_cleanup"
                | "dev"
                | "dev-local"
                | "auth"
                | "events"
                | "update"
                | "install"
                | "uninstall"
                | "extension"
                | "resource"
                | "package"
                | "pi-package"
                | "pi-update"
                | "install-pi"
                | "uninstall-pi"
        )
    ) || [
        "--connect",
        "--server",
        "--web",
        "--resource-capabilities",
        "--resource-session-read",
    ]
    .iter()
    .any(|flag| present(argv, flag))
    {
        return Err(format!("{FLAG} is only available in a local native CLI session, not subcommands, remote, server, Web or saved-session inspection"));
    }
    Ok(())
}

/// The owner is host-defined, never supplied by JSON or inferred from history.
/// Revalidate for embedding callers that construct Args without parse_args.
pub(crate) fn policy(args: &Args) -> Result<Option<ExperimentalDurablePolicy>, String> {
    let Some(epoch) = &args.experimental_durable_epoch else {
        return Ok(None);
    };
    if !args.native_durable {
        return Err(format!("{FLAG} requires --native-durable"));
    }
    if !args.errors.is_empty() {
        return Err(format!("{FLAG} cannot start from invalid arguments"));
    }
    if args.export.is_some()
        || args.list_models.is_some()
        || [
            "connect",
            "server",
            "web",
            "resource-capabilities",
            "resource-session-read",
        ]
        .iter()
        .any(|flag| args.unknown_flags.contains_key(*flag))
    {
        return Err(format!("{FLAG} cannot be combined with export, list-models, remote, server, Web or saved-session inspection"));
    }
    ExperimentalDurablePolicy::new(epoch.clone(), vec![OWNER.to_owned()])
        .map(Some)
        .map_err(|_| format!("{FLAG} requires a valid nonempty bounded ASCII identity"))
}

/// Wire remains v1. Only the bound harness can select experimental metadata;
/// unknown flags, request JSON or a service-local policy cannot change it.
pub(crate) fn capabilities_for(harness: &AgentHarness, enabled: bool) -> Value {
    let mut result = capabilities(enabled);
    if enabled {
        if let Some(epoch) = harness.experimental_durable_epoch() {
            result["profile"] = serde_json::json!("native-durable-ledger-v2-experimental");
            result["ledger"] = serde_json::json!({
                "version": 2, "epochId": epoch, "experimental": true,
                "selection": "explicit_host_epoch", "automaticMigration": false
            });
            let limits = &mut result["limits"];
            limits["receipts"] = serde_json::json!(ledger::chain::MAX_CHAIN_RECORDS);
            limits["recordBytes"] = serde_json::json!(ledger::MAX_RECORD_BYTES);
            limits["ledgerNodes"] = serde_json::json!(ledger::chain::MAX_CHAIN_NODES);
            limits["sourcePathEntries"] = serde_json::json!(ledger::source::MAX_PATH_ENTRIES);
            limits["sourceCustomEntries"] = serde_json::json!(ledger::source::MAX_CUSTOM_ENTRIES);
            limits["sourceBranchBytes"] = serde_json::json!(ledger::source::MAX_BRANCH_BYTES);
            limits["policyActors"] = serde_json::json!(ledger::control::MAX_POLICY_ACTORS);
            limits["nativeRecords"] = serde_json::json!(ledger::native_run::MAX_NATIVE_RECORDS);
            limits["nativeRecordBytes"] =
                serde_json::json!(ledger::native_run::MAX_NATIVE_RECORD_BYTES);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Args {
        crate::args::parse_args(&args.iter().map(|x| x.to_string()).collect::<Vec<_>>())
    }
    #[test]
    fn parsing_is_explicit_order_independent_and_never_an_extension_flag() {
        for args in [
            vec![FLAG, "synthetic-epoch", "--native-durable"],
            vec![
                "--native-durable",
                "--experimental-durable-epoch=synthetic-epoch",
            ],
        ] {
            let args = parse(&args);
            assert!(args.errors.is_empty(), "{:?}", args.errors);
            assert_eq!(
                args.experimental_durable_epoch.as_deref(),
                Some("synthetic-epoch")
            );
            assert!(!args.unknown_flags.contains_key(&FLAG[2..]));
            assert_eq!(
                policy(&args).unwrap().unwrap().epoch_id(),
                "synthetic-epoch"
            );
        }
        assert!(policy(&Args::default()).unwrap().is_none());
    }
    #[test]
    fn invalid_missing_duplicate_and_unsupported_arguments_fail_closed() {
        for args in [
            vec![FLAG],
            vec![FLAG, "epoch"],
            vec!["--native-durable", FLAG, ""],
            vec!["--native-durable", FLAG, "white space"],
            vec!["--native-durable", FLAG, "epoch", FLAG, "epoch"],
            vec!["--native-durable", FLAG, "epoch", "--server"],
            vec!["--native-durable", FLAG, "epoch", "--web"],
            vec![
                "--native-durable",
                FLAG,
                "epoch",
                "--export",
                "synthetic.html",
            ],
            vec!["--native-durable", FLAG, "epoch", "--list-models"],
        ] {
            assert!(!parse(&args).errors.is_empty(), "{args:?}");
        }
    }
    #[test]
    fn early_dispatch_rejected_but_remote_values_not_reinterpreted() {
        for first in [
            "dev",
            "dev-local",
            "auth",
            "resource",
            "package",
            "events",
            "update",
            "install",
            "uninstall",
            "extension",
            "__rpi_dev_cleanup",
        ] {
            assert!(early_check(&[first.into(), FLAG.into(), "epoch".into()]).is_err());
        }
        for flag in [
            "--connect=127.0.0.1:1",
            "--server",
            "--web",
            "--resource-capabilities",
            "--resource-session-read",
        ] {
            assert!(early_check(&[FLAG.into(), "epoch".into(), flag.into()]).is_err());
        }
        assert!(early_check(&[
            "--connect".into(),
            "127.0.0.1:1".into(),
            "--token".into(),
            FLAG.into()
        ])
        .is_ok());
        assert!(early_check(&["--connect".into(), FLAG.into()]).is_ok());
        assert!(early_check(&[
            "--connect=127.0.0.1:1".into(),
            "--token".into(),
            FLAG.into(),
            FLAG.into(),
            "epoch".into()
        ])
        .is_err());
        assert!(early_check(&["--native-durable".into(), FLAG.into(), "epoch".into()]).is_ok());
    }
    #[tokio::test]
    async fn bound_metadata_changes_ledger_not_wire_and_default_is_byte_equivalent() {
        use rpi_harness::types::AgentHarnessOptions;
        let default = AgentHarness::create(AgentHarnessOptions::default())
            .await
            .unwrap();
        assert!(default.experimental_durable_epoch().is_none());
        for enabled in [false, true] {
            assert_eq!(
                serde_json::to_vec(&capabilities_for(&default, enabled)).unwrap(),
                serde_json::to_vec(&capabilities(enabled)).unwrap()
            );
        }
        let h = AgentHarness::create_experimental(
            AgentHarnessOptions {
                require_native_durable: true,
                ..Default::default()
            },
            ExperimentalDurablePolicy::new("unit-epoch", vec![OWNER.into()]).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(capabilities_for(&h, false), capabilities(false));
        let result = capabilities_for(&h, true);
        assert_eq!(result["version"], 1);
        assert_eq!(result["ledger"]["version"], 2);
        assert_eq!(result["ledger"]["epochId"], "unit-epoch");
        assert_eq!(
            result["limits"]["receipts"],
            ledger::chain::MAX_CHAIN_RECORDS
        );
        assert_eq!(result["commands"], capabilities(true)["commands"]);
        assert_eq!(
            result["executionPages"],
            capabilities(true)["executionPages"]
        );
        let upgraded = h.downgrade().upgrade().unwrap();
        assert_eq!(
            upgraded.experimental_durable_epoch(),
            h.experimental_durable_epoch()
        );
    }
}
