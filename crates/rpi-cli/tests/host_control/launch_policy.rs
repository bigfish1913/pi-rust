// Unsupported platforms still reject daemon before opening any resources.

#[cfg(not(target_os = "macos"))]
#[tokio::test(flavor = "current_thread")]
async fn actual_binary_rejects_unimplemented_launch_policy_before_profiles_or_catalog() {
    for restore in [false, true] {
        for restart in [false, true] {
            let mut fixture = Fixture::new();
            fixture.value["mode"] = json!("daemon");
            fixture.value["recovery"] = json!({
                "restore_on_start": restore, "restart_on_crash": restart
            });
            fixture.value["agents"] = json!([fixture.root.path().join("must-not-read.json")]);
            fixture.save();
            let output = fixture.call("serve", &[]).await;
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("shared Host daemon mode is currently supported on macOS"),
                "{stderr}"
            );
            assert!(!stderr.contains("invalid Agent profile"));
            assert!(!stderr.contains(fixture.value["token"].as_str().unwrap()));
            assert!(!stderr.contains("must-not-read"));
            assert!(!fixture.directory().exists());
            assert!(!fixture.socket().exists());
            fixture.no_resources();
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_rejects_invalid_launch_fields_without_startup_side_effects() {
    for extra in [
        json!({"mode": "background"}),
        json!({"mode": "Daemon"}),
        json!({"mode": null}),
        json!({"mode": {"foreground": null}}),
        json!({"mode": {"daemon": null}}),
        json!({"mode": []}),
        json!({"recovery": null}),
        json!({"recovery": []}),
        json!({"recovery": [true, true]}),
        json!({"daemon": true}),
        json!({"recovery": {"restore_on_start": "true"}}),
        json!({"recovery": {"restart_on_crash": null}}),
        json!({"recovery": {"restartOnCrash": true}}),
        json!({"recovery": {"restart_on_crash": true}}),
        json!({"mode": "foreground", "recovery": {"restore_on_start": true, "restart_on_crash": true}}),
    ] {
        let mut fixture = Fixture::new();
        fixture
            .value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        fixture.save();
        for operation in ["serve", "status"] {
            let output = fixture.call(operation, &[]).await;
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("invalid Host config"), "{extra}: {stderr}");
            assert!(!stderr.contains(fixture.value["token"].as_str().unwrap()));
        }
        assert!(!fixture.directory().exists());
        assert!(!fixture.socket().exists());
        fixture.no_resources();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_binary_queries_with_future_launch_policy_only_contact_existing_host() {
    let mut fixture = Fixture::new();
    let (child, ready) = fixture.start().await; // Old config: foreground, no recovery.
    fixture.value["agents"] = json!([fixture.root.path().join("must-not-read.json")]);
    for mode in ["foreground", "daemon"] {
        for restore in [false, true] {
            for restart in [false, true] {
                if restart && mode == "foreground" {
                    continue;
                }
                fixture.value["mode"] = json!(mode);
                fixture.value["recovery"] = json!({
                    "restore_on_start": restore, "restart_on_crash": restart
                });
                fixture.save();
                let status = fixture.data("status", &[]).await;
                assert_eq!(status["epoch"], ready["control"]["epoch"]);
                fixture.data("list", &[]).await;
                fixture.no_resources();
            }
        }
    }
    fixture.stop(child, nix::sys::signal::Signal::SIGTERM).await;
}
