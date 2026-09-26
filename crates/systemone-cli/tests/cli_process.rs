//! Process-level tests for `s1` that need no model, network or native build.

use std::path::Path;

use serde_json::Value;

fn run(
    args: &[&str],
    stdin: &str,
    env: &[(&str, &str)],
    cwd: Option<&Path>,
) -> (i32, Value, String) {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_s1"));
    command.args(args);
    for (name, _) in std::env::vars_os().filter(|(name, _)| {
        name.to_str()
            .is_some_and(|name| name.starts_with("SYSTEMONE_"))
    }) {
        command.env_remove(name);
    }
    for (name, value) in env {
        command.env(name, value);
    }
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.stdin(if stdin.is_empty() {
        // Empty stdin stands in for "no piped input"; s1 treats a
        // closed empty pipe like a TTY for validation purposes only when
        // no --input is given, so tests use an explicit marker instead.
        std::process::Stdio::null()
    } else {
        std::process::Stdio::piped()
    });
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    if !stdin.is_empty() {
        use std::io::Write as _;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    let code = output.status.code().unwrap_or(-1);
    let stdout = if output.stdout.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&output.stdout).expect("stdout is JSON")
    };
    (code, stdout, String::from_utf8(output.stderr).unwrap())
}

fn stderr_json(stderr: &str) -> Value {
    serde_json::from_str(stderr.lines().last().unwrap_or("null")).unwrap()
}

#[test]
fn help_and_version_are_json_only() {
    let (code, stdout, stderr) = run(&["--help"], "", &[], None);
    assert_eq!(code, 0);
    assert_eq!(stdout["schema"], "systemone-help-v1");
    assert!(stdout["text"].as_str().unwrap().contains("s1 serve"));
    assert!(stderr.is_empty());
    let (code, stdout, _) = run(&["--version"], "", &[], None);
    assert_eq!(code, 0);
    assert_eq!(stdout["schema"], "systemone-version-v1");
    assert_eq!(stdout["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn missing_command_and_bad_set_are_usage_errors() {
    let (code, stdout, stderr) = run(&["--no-config"], "", &[], None);
    assert_eq!(code, 2);
    assert!(stdout.is_null());
    assert_eq!(stderr_json(&stderr)["error"]["code"], "usage");
    let (code, _, stderr) = run(
        &["--no-config", "--set", "novalue", "backends"],
        "",
        &[],
        None,
    );
    assert_eq!(code, 2);
    assert_eq!(stderr_json(&stderr)["error"]["code"], "configuration");
    let (code, _, stderr) = run(
        &["--no-config", "--set", "server.port=0", "backends"],
        "",
        &[],
        None,
    );
    assert_eq!(code, 2);
    assert!(
        stderr_json(&stderr)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("port")
    );
}

#[test]
fn backends_lists_builtin_local_instance_without_loading() {
    let (code, stdout, _) = run(&["--no-config", "backends"], "", &[], None);
    assert_eq!(code, 0);
    assert_eq!(stdout["schema"], "systemone-backends-v1");
    assert_eq!(stdout["default_backend"], "local");
    let local = &stdout["backends"][0];
    assert_eq!(local["id"], "local");
    assert_eq!(local["kind"], "openjev");
    assert_eq!(local["enabled"], true);
    assert_eq!(local["extensions"]["model_store"]["supported"], true);
    assert_eq!(local["available"], cfg!(feature = "native"));
}

#[test]
fn config_show_and_check_reflect_project_file_env_and_cli_layers() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("systemone.config.toml"),
        r#"
[server]
port = 9001

[backends.cloud]
kind = "openrouter"
enabled = false
model = "jev-latest"

[backends.cloud.settings]
api_key_env = "OPENROUTER_API_KEY"
"#,
    )
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    let env = [
        ("HOME", home.path().to_str().unwrap()),
        ("SYSTEMONE_BACKENDS__LOCAL__SETTINGS__THREADS", "3"),
    ];
    let (code, stdout, _) = run(
        &["--set", "server.request_timeout_secs=7", "config", "show"],
        "",
        &env,
        Some(root.path()),
    );
    assert_eq!(code, 0);
    assert_eq!(stdout["schema"], "systemone-config-show-v1");
    assert_eq!(stdout["config"]["server"]["port"], 9001);
    assert_eq!(stdout["config"]["server"]["request_timeout_secs"], 7);
    assert_eq!(
        stdout["config"]["backends"]["local"]["settings"]["threads"],
        3
    );
    assert!(
        stdout["provenance"]["server.port"]
            .as_str()
            .unwrap()
            .ends_with("systemone.config.toml")
    );
    assert_eq!(
        stdout["provenance"]["backends.local.settings.threads"],
        "SYSTEMONE_BACKENDS__LOCAL__SETTINGS__THREADS"
    );
    assert_eq!(stdout["provenance"]["server.request_timeout_secs"], "cli");
    // A disabled hosted backend does not change `config check`.
    let (code, stdout, _) = run(&["config", "check"], "", &env, Some(root.path()));
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(stdout["enabled_backends"], serde_json::json!(["local"]));
    // Enabling it passes: a missing key is an availability state reported
    // by `s1 backends`, not a configuration error.
    let (code, stdout, _) = run(
        &["--set", "backends.cloud.enabled=true", "config", "check"],
        "",
        &env,
        Some(root.path()),
    );
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(
        stdout["enabled_backends"],
        serde_json::json!(["cloud", "local"])
    );
    // Invalid hosted settings are a clear failure that names the backend.
    let (code, _, stderr) = run(
        &[
            "--set",
            "backends.cloud.enabled=true",
            "--set",
            "backends.cloud.settings.api_key_env=not a name",
            "config",
            "check",
        ],
        "",
        &env,
        Some(root.path()),
    );
    assert_eq!(code, 2);
    assert!(
        stderr_json(&stderr)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cloud")
    );
    // Malformed project config fails every command except help/version.
    std::fs::write(root.path().join("systemone.config.toml"), "server = 1\n").unwrap();
    let (code, _, stderr) = run(&["backends"], "", &env, Some(root.path()));
    assert_eq!(code, 2);
    assert_eq!(stderr_json(&stderr)["error"]["code"], "configuration");
    let (code, _, _) = run(&["--help"], "", &env, Some(root.path()));
    assert_eq!(code, 0);
}

#[test]
fn unknown_openjev_settings_are_rejected_at_config_check() {
    let (code, _, stderr) = run(
        &[
            "--no-config",
            "--set",
            "backends.local.settings.bogus=1",
            "config",
            "check",
        ],
        "",
        &[],
        None,
    );
    assert_eq!(code, 2);
    assert!(
        stderr_json(&stderr)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bogus")
    );
}

#[test]
fn run_validates_request_and_selector_before_loading() {
    let (code, _, stderr) = run(&["--no-config", "run"], "{", &[], None);
    assert_eq!(code, 2);
    assert!(
        stderr_json(&stderr)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid_json")
    );
    let request = r#"{"backend":"local","state":"s","questions":{"q":{"type":"noul"}}}"#;
    let (code, _, stderr) = run(
        &["--no-config", "run", "--backend", "other"],
        request,
        &[],
        None,
    );
    assert_eq!(code, 2);
    assert!(
        stderr_json(&stderr)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("conflict")
    );
    let plain = r#"{"state":"s","questions":{"q":{"type":"noul"}}}"#;
    let (code, _, stderr) = run(
        &["--no-config", "run", "--backend", "nope"],
        plain,
        &[],
        None,
    );
    assert_eq!(code, 2);
    assert!(
        stderr_json(&stderr)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown backend")
    );
    let (code, _, stderr) = run(
        &["--no-config", "run", "--input", "/nonexistent/request.json"],
        "",
        &[],
        None,
    );
    assert_eq!(code, 2);
    assert!(
        stderr_json(&stderr)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot read")
    );
}

#[cfg(not(feature = "native"))]
#[test]
fn run_and_pull_fail_explicitly_without_native_build() {
    let request = r#"{"state":"s","questions":{"q":{"type":"noul"}}}"#;
    let (code, stdout, stderr) = run(&["--no-config", "run"], request, &[], None);
    assert_eq!(code, 1);
    assert!(stdout.is_null());
    assert_eq!(stderr_json(&stderr)["error"]["code"], "unavailable");
    let (code, _, stderr) = run(
        &["--no-config", "openjev", "models", "pull", "qwen3-0.6b"],
        "",
        &[],
        None,
    );
    assert_eq!(code, 1);
    assert_eq!(stderr_json(&stderr)["error"]["code"], "unavailable");
    let (code, _, stderr) = run(
        &["--no-config", "openjev", "probe", "--mode", "shared"],
        "",
        &[],
        None,
    );
    assert_eq!(code, 1);
    assert_eq!(stderr_json(&stderr)["error"]["code"], "unavailable");
}

#[test]
fn models_uses_the_model_store_extension() {
    let home = tempfile::tempdir().unwrap();
    let (code, stdout, _) = run(
        &["--no-config", "models"],
        "",
        &[("HOME", home.path().to_str().unwrap())],
        None,
    );
    assert_eq!(code, 0);
    assert_eq!(stdout["schema"], "systemone-models-v1");
    assert_eq!(stdout["backend"], "local");
    assert_eq!(stdout["models"].as_array().unwrap().len(), 3);
    assert!(
        stdout["models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["cached"] == false)
    );
}

#[test]
fn call_rejects_malformed_input_before_any_network_request() {
    let (code, _, stderr) = run(
        &["--no-config", "call", "--url", "http://127.0.0.1:1"],
        "{",
        &[],
        None,
    );
    assert_eq!(code, 2);
    assert!(
        stderr_json(&stderr)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid_json")
    );
}

#[test]
fn one_shot_flag_commands_validate_before_loading() {
    // Missing state with no piped stdin.
    let (code, _, stderr) = run(
        &["--no-config", "noul", "--question", "Refund?"],
        "",
        &[],
        None,
    );
    assert_eq!(code, 2);
    assert!(
        stderr_json(&stderr)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("state is required")
    );
    // Conflicting state sources are a clap usage error (exit 2, JSON stderr).
    let (code, _, stderr) = run(
        &[
            "--no-config",
            "decide",
            "--state",
            "a",
            "--state-json",
            "1",
            "--question",
            "q",
            "--option",
            "x",
        ],
        "",
        &[],
        None,
    );
    assert_eq!(code, 2);
    assert!(stderr_json(&stderr)["error"].is_object());
    // Misaligned --option-id.
    let (code, _, stderr) = run(
        &[
            "--no-config",
            "decide",
            "--state",
            "a",
            "--question",
            "q",
            "--option",
            "x",
            "--option",
            "y",
            "--option-id",
            "only",
        ],
        "",
        &[],
        None,
    );
    assert_eq!(code, 2);
    assert!(
        stderr_json(&stderr)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--option-id count")
    );
    // A single score level is rejected by the neutral validator.
    let (code, _, stderr) = run(
        &[
            "--no-config",
            "score",
            "--state",
            "a",
            "--question",
            "q",
            "--level",
            "only",
        ],
        "",
        &[],
        None,
    );
    assert_eq!(code, 2);
    assert_eq!(stderr_json(&stderr)["error"]["code"], "validation");
    // --output requires --jsonl.
    let (code, _, stderr) = run(
        &["--no-config", "run", "--output", "/tmp/x.jsonl"],
        r#"{"state":"s","questions":{"q":{"type":"noul"}}}"#,
        &[],
        None,
    );
    assert_eq!(code, 2);
    assert!(stderr_json(&stderr)["error"].is_object());
}

/// A CPU-local default plus hosted gateways selected per request. The
/// gateway keys are unset, so each selected gateway fails with its own
/// "key not set" reason; nothing is downloaded and no network call is made.
#[test]
fn hosted_gateways_are_selected_per_request_beside_a_local_default() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("systemone.config.toml"),
        r#"default_backend = "local"

[backends.local]
kind = "openjev"
enabled = true
model = "qwen3-0.6b"

[backends.local.settings]
device = "cpu"
offline = true

[backends.gateway]
kind = "vercel"
enabled = true

[backends.gateway.settings]
api_key_env = "S1_TEST_UNSET_GATEWAY_KEY"

[backends.router]
kind = "openrouter"
enabled = true

[backends.router.settings]
api_key_env = "S1_TEST_UNSET_ROUTER_KEY"
"#,
    )
    .unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let env = [("HOME", home.to_str().unwrap())];

    let (code, stdout, _) = run(&["backends"], "", &env, Some(root.path()));
    assert_eq!(code, 0);
    let listed: Vec<(String, String, bool)> = stdout["backends"]
        .as_array()
        .unwrap()
        .iter()
        .map(|backend| {
            (
                backend["id"].as_str().unwrap().to_owned(),
                backend["kind"].as_str().unwrap().to_owned(),
                backend["available"].as_bool().unwrap(),
            )
        })
        .collect();
    assert!(listed.contains(&("gateway".into(), "vercel".into(), false)));
    assert!(listed.contains(&("router".into(), "openrouter".into(), false)));

    for (backend, variable, key) in [
        ("gateway", "S1_TEST_UNSET_GATEWAY_KEY", "AI Gateway API key"),
        ("router", "S1_TEST_UNSET_ROUTER_KEY", "OpenRouter API key"),
    ] {
        let (code, _, stderr) = run(
            &[
                "noul",
                "--backend",
                backend,
                "--state",
                "x",
                "--question",
                "y?",
            ],
            "",
            &env,
            Some(root.path()),
        );
        assert_eq!(code, 1, "{stderr}");
        let message = stderr_json(&stderr)["error"]["message"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(message.contains(variable), "{message}");
        assert!(message.contains(key), "{message}");
    }
    // The hosted selections downloaded nothing into the OpenJev cache.
    assert!(!home.join(".cache/openjev").exists());
}
