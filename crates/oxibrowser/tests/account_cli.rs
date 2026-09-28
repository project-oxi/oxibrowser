//! Account/credential CLI integration tests — offline paths only.
//!
//! Every test runs the real binary against a throwaway `HOME`, so the
//! account registry, audit log, and consent store all land in a temp
//! directory and nothing touches the user's keychain. Keychain-backed
//! paths (`credential put/get/totp/rm`, `account status --probe`) are
//! deliberately not exercised here.

use std::process::Command;

fn oxibrowser() -> &'static str {
    env!("CARGO_BIN_EXE_oxibrowser")
}

/// Run the binary with an isolated HOME; returns (exit_code, stdout, stderr).
fn run(home: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(oxibrowser())
        .args(args)
        .env("HOME", home)
        .output()
        .expect("failed to run oxibrowser");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn temp_home(tag: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "oxi-account-cli-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn json_stdout(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout).expect("valid CLI JSON")
}

#[test]
fn account_add_list_status_rm_round_trip() {
    let home = temp_home("roundtrip");

    let (code, stdout, _) = run(
        &home,
        &[
            "account",
            "add",
            "--site",
            "github.com",
            "--id",
            "gh-work",
            "--login",
            "garden@corp.io",
            "--display",
            "Garden (work)",
            "--json",
        ],
    );
    assert_eq!(code, 0, "add failed: {stdout}");
    let resp = json_stdout(&stdout);
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["data"]["account_id"], "gh-work");
    assert_eq!(resp["data"]["scope"], "github.com");
    assert_eq!(resp["data"]["state"], "needs_login");
    assert_eq!(resp["data"]["login_hint"], "garden@corp.io");

    // list shows the account
    let (code, stdout, _) = run(&home, &["account", "list", "--json"]);
    assert_eq!(code, 0);
    let resp = json_stdout(&stdout);
    let accounts = resp["data"]["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["account_id"], "gh-work");

    // status reflects the record
    let (code, stdout, _) = run(&home, &["account", "status", "gh-work", "--json"]);
    assert_eq!(code, 0);
    let resp = json_stdout(&stdout);
    assert_eq!(resp["data"]["account"]["state"], "needs_login");
    assert_eq!(
        resp["data"]["account"]["session_summary"]["cookie_count"],
        0
    );

    // rm removes it
    let (code, stdout, _) = run(&home, &["account", "rm", "gh-work", "--json"]);
    assert_eq!(code, 0, "rm failed: {stdout}");
    let resp = json_stdout(&stdout);
    assert_eq!(resp["data"]["removed"], "gh-work");

    let (code, stdout, _) = run(&home, &["account", "list", "--json"]);
    assert_eq!(code, 0);
    assert!(
        json_stdout(&stdout)["data"]["accounts"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn account_id_derivation_and_conflicts() {
    let home = temp_home("derive");

    // no --id: derived from scope
    let (code, stdout, _) = run(&home, &["account", "add", "--site", "github.com", "--json"]);
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(json_stdout(&stdout)["data"]["account_id"], "github-com");

    // same site again → github-com-2
    let (code, stdout, _) = run(&home, &["account", "add", "--site", "github.com", "--json"]);
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(json_stdout(&stdout)["data"]["account_id"], "github-com-2");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn account_validation_errors() {
    let home = temp_home("validation");

    // bad slug
    let (code, stdout, _) = run(
        &home,
        &[
            "account",
            "add",
            "--site",
            "github.com",
            "--id",
            "Bad_ID",
            "--json",
        ],
    );
    assert_eq!(code, 2, "invalid slug must exit 2: {stdout}");
    assert_eq!(json_stdout(&stdout)["error_code"], "INPUT_VALIDATION");

    // duplicate explicit id
    let (code, _, _) = run(
        &home,
        &[
            "account",
            "add",
            "--site",
            "github.com",
            "--id",
            "gh",
            "--json",
        ],
    );
    assert_eq!(code, 0);
    let (code, stdout, _) = run(
        &home,
        &[
            "account",
            "add",
            "--site",
            "github.com",
            "--id",
            "gh",
            "--json",
        ],
    );
    assert_ne!(code, 0);
    assert_eq!(json_stdout(&stdout)["error_code"], "ACCOUNT_EXISTS");

    // unknown account status/rm
    let (code, stdout, _) = run(&home, &["account", "status", "ghost", "--json"]);
    assert_ne!(code, 0);
    assert_eq!(json_stdout(&stdout)["error_code"], "ACCOUNT_NOT_FOUND");
    let (code, stdout, _) = run(&home, &["account", "rm", "ghost", "--json"]);
    assert_ne!(code, 0);
    assert_eq!(json_stdout(&stdout)["error_code"], "ACCOUNT_NOT_FOUND");

    // credential put refuses argv values and missing origins
    let (code, _, _) = run(
        &home,
        &[
            "credential",
            "put",
            "--agent",
            "main",
            "--site",
            "github.com",
            "--kind",
            "password",
        ],
    );
    assert_ne!(code, 0, "put without --origin must fail");
    let (code, _, stderr) = run(
        &home,
        &[
            "credential",
            "put",
            "--agent",
            "main",
            "--site",
            "github.com",
            "--kind",
            "bogus",
            "--origin",
            "https://github.com/",
            "--stdin",
        ],
    );
    assert_ne!(code, 0, "unknown kind must fail");
    assert!(stderr.contains("kind"));

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn account_registry_directory_layout_and_audit() {
    let home = temp_home("layout");

    run(
        &home,
        &[
            "account",
            "add",
            "--site",
            "example.com",
            "--id",
            "a1",
            "--json",
        ],
    );
    let accounts = home.join(".oxibrowser").join("accounts");
    assert!(accounts.join("a1").join("account.json").is_file());
    assert!(accounts.join("a1").join("sessions").is_dir());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&accounts), 0o700, "accounts dir 0700");
        assert_eq!(mode(&accounts.join("a1")), 0o700, "account dir 0700");
        assert_eq!(
            mode(&accounts.join("a1").join("account.json")),
            0o600,
            "record 0600"
        );
    }

    // audit log was written under the temp HOME; `account rm` records a
    // sensitive_action event
    run(&home, &["account", "rm", "a1", "--json"]);
    let audit = home.join(".oxibrowser").join("audit.jsonl");
    assert!(audit.is_file());
    let content = std::fs::read_to_string(&audit).unwrap();
    assert!(
        content.contains("sensitive_action") && content.contains("account=a1"),
        "rm must audit: {content}"
    );

    std::fs::remove_dir_all(&home).ok();
}
