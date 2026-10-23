// SSH agent tools, split by responsibility: config/profile storage and the
// datas.md parser (config), argv construction + validation (command),
// askpass/temp-key handling (askpass), process exec (session), read-only and
// destructive-command policy (policy), and the tool definitions (tools).
// `agent::ssh_tools` re-exports this module's public API unchanged.
mod askpass;
mod command;
mod config;
mod policy;
mod session;
mod tools;

pub use tools::{
    SshCopyTool, SshExecTool, SshGenerateKeyTool, SshInstallKeyTool, SshListProfilesTool,
    SshSaveProfileToDatasTool,
};
pub(crate) use askpass::purge_stale_askpass_dirs;

#[cfg(test)]
mod tests {
    use super::askpass::{purge_stale_askpass_dirs, Askpass};
    use super::command::{
        check_remote_path, clamp_timeout, install_key_command, resolve_local_path, resolve_target,
        scp_argv, shell_quote, ssh_argv, ssh_exec_argv, ExecOpts, Target,
    };
    use super::config::{auth_kind, datas_profiles, load_config, parse_datas_ssh};
    use super::policy::destructive_reasons;
    use super::session::{run_process, SSH_OUTPUT_CAP};
    use super::tools::{
        SshCopyTool, SshExecTool, SshGenerateKeyTool, SshInstallKeyTool, SshListProfilesTool,
        SshSaveProfileToDatasTool,
    };
    use crate::agent::result::read_capped;
    use crate::agent::tool::{validate_args, PermissionPolicy, Tool, ToolClass, ToolCtx};
    use crate::config::{self, ConfigKind};
    use crate::db;
    use crate::journal::mask_secrets;
    use crate::onboarding::temp_test_dir;
    use crate::state::AppState;
    use crate::{paths, projects};
    use serde_json::{json, Value};
    use std::fs;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};

    fn harness_ctx(dir: &Path) -> ToolCtx {
        ToolCtx {
            working_dir: Some(dir.to_path_buf()),
            ..Default::default()
        }
    }

    #[test]
    fn run_process_cancel_kills_the_child_promptly() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let wd = temp_test_dir("ssh-run-cancel");
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flag.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let err = match run_process(
            "sh",
            &["-c".to_string(), "sleep 30".to_string()],
            &wd,
            Duration::from_secs(120),
            &[],
            Some(&cancel),
        ) {
            Err(e) => e,
            Ok(_) => panic!("cancelled run_process must not succeed"),
        };
        let elapsed = started.elapsed();
        setter.join().unwrap();
        assert!(err.to_string().contains("cancelled"), "{err}");
        // The child dies on cancel, not after the full command timeout.
        assert!(elapsed < Duration::from_secs(5), "cancel took {elapsed:?}");
        fs::remove_dir_all(&wd).unwrap();
    }

    // Project mode: a real CRM project under Projects/.
    fn project_env(tag: &str) -> (PathBuf, ToolCtx) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        let state = Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(conn);
        let ctx = ToolCtx {
            cancel: None,
            project_id: Some(dto.id),
            session_id: None,
            chat_id: None,
            working_dir: Some(wd.clone()),
            state: Some(state),
            notes_root: None,
        };
        (paths::projects_dir(&wd).join("proj"), ctx)
    }

    fn write_ssh_config(wd: &Path, text: &str) {
        fs::create_dir_all(paths::config_dir(wd)).unwrap();
        fs::write(paths::config_dir(wd).join("ssh.json"), text).unwrap();
    }

    fn target(port: u16, key: Option<&str>) -> Target {
        Target {
            host: "example.ru".into(),
            user: "deploy".into(),
            port,
            key_path: key.map(PathBuf::from),
            password: None,
            strict_host_key: "accept-new",
        }
    }

    fn target_pw(port: u16, key: Option<&str>, password: Option<&str>) -> Target {
        let mut t = target(port, key);
        t.password = password.map(str::to_string);
        t
    }

    fn opts(login_shell: bool, cwd: Option<&str>, pty: bool, timeout_secs: Option<i64>) -> ExecOpts {
        ExecOpts {
            login_shell,
            cwd: cwd.map(str::to_string),
            pty,
            timeout: clamp_timeout(timeout_secs),
        }
    }

    #[test]
    fn ssh_exec_argv_default_uses_login_shell_and_120s() {
        let o = opts(true, None, false, None);
        let argv = ssh_exec_argv(&target(2222, Some("/keys/id_ed25519")), "ls -l", &o);
        assert_eq!(
            argv,
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ConnectTimeout=10",
                "-p",
                "2222",
                "-i",
                "/keys/id_ed25519",
                "deploy@example.ru",
                "bash -lc 'ls -l'",
            ]
        );
        assert_eq!(o.timeout, Duration::from_secs(120));
        // the command is the last argument and there is no "--" anywhere
        assert_eq!(argv[argv.len() - 1], "bash -lc 'ls -l'");
        assert_eq!(argv[argv.len() - 2], "deploy@example.ru");
        assert!(!argv.iter().any(|a| a == "--"), "{argv:?}");
        // no key: no -i flag at all
        let plain = ssh_argv(&target(22, None));
        assert!(!plain.iter().any(|a| a == "-i"), "{plain:?}");
    }

    #[test]
    fn login_shell_false_passes_command_verbatim() {
        let o = opts(false, None, false, None);
        assert_eq!(
            ssh_exec_argv(&target(22, None), "echo hi", &o).pop().unwrap(),
            "echo hi"
        );
        // a bare leading-dash command is still wrapped so the remote shell does
        // not read it as its own option
        assert_eq!(ssh_exec_argv(&target(22, None), "-l", &o).pop().unwrap(), "sh -c '-l'");
        // loginShell=true wraps even a leading-dash command in bash -lc
        let o = opts(true, None, false, None);
        assert_eq!(
            ssh_exec_argv(&target(22, None), "-l", &o).pop().unwrap(),
            "bash -lc '-l'"
        );
        // shell_quote escapes embedded single quotes as '\''
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn cwd_is_shell_quoted_and_prepended() {
        let o = opts(true, Some("/var/www/my dir"), false, None);
        assert_eq!(
            ssh_exec_argv(&target(22, None), "ls", &o).pop().unwrap(),
            "cd '/var/www/my dir' && bash -lc 'ls'"
        );
        // an embedded single quote in cwd is escaped as '\''
        let o = opts(false, Some("/it's"), false, None);
        assert_eq!(
            ssh_exec_argv(&target(22, None), "ls", &o).pop().unwrap(),
            "cd '/it'\\''s' && ls"
        );
        // empty cwd is ignored
        let o = opts(false, Some(""), false, None);
        assert_eq!(ssh_exec_argv(&target(22, None), "ls", &o).pop().unwrap(), "ls");
    }

    #[test]
    fn pty_adds_tt_before_the_destination() {
        let o = opts(true, None, true, None);
        let argv = ssh_exec_argv(&target(22, None), "top", &o);
        let tt = argv.iter().position(|a| a == "-tt").expect("-tt present");
        let host = argv.iter().position(|a| a == "deploy@example.ru").unwrap();
        assert!(tt < host, "-tt must precede the destination: {argv:?}");
        assert_eq!(argv[argv.len() - 1], "bash -lc 'top'");
        assert!(!argv.iter().any(|a| a == "--"), "{argv:?}");
        // pty=false: no -tt at all
        let o = opts(true, None, false, None);
        assert!(!ssh_exec_argv(&target(22, None), "top", &o).iter().any(|a| a == "-tt"));
    }

    #[test]
    fn timeout_is_clamped_to_10_1800() {
        assert_eq!(clamp_timeout(None), Duration::from_secs(120));
        assert_eq!(clamp_timeout(Some(120)), Duration::from_secs(120));
        assert_eq!(clamp_timeout(Some(5)), Duration::from_secs(10));
        assert_eq!(clamp_timeout(Some(0)), Duration::from_secs(10));
        assert_eq!(clamp_timeout(Some(-3)), Duration::from_secs(10));
        assert_eq!(clamp_timeout(Some(1800)), Duration::from_secs(1800));
        assert_eq!(clamp_timeout(Some(99999)), Duration::from_secs(1800));
    }

    #[test]
    fn command_stays_last_element_with_no_double_dash() {
        let o = opts(true, Some("/srv/app"), true, Some(600));
        let argv = ssh_exec_argv(&target(22, None), "apt-get install -y nginx", &o);
        assert_eq!(
            argv[argv.len() - 1],
            "cd '/srv/app' && bash -lc 'apt-get install -y nginx'"
        );
        assert!(!argv.iter().any(|a| a == "--"), "{argv:?}");
    }

    #[test]
    fn scp_argv_carries_port_key_recursive_and_path_order() {
        let up = scp_argv(&target(2222, Some("/keys/id")), "/local/a.txt", "/remote/b.txt", true);
        let joined = up.join(" ");
        assert!(joined.contains("-P 2222"), "{joined}");
        assert!(joined.contains("-i /keys/id"), "{joined}");
        assert!(joined.contains(" -r "), "{joined}");
        assert_eq!(up[up.len() - 2], "/local/a.txt");
        assert_eq!(up[up.len() - 1], "/remote/b.txt");
        let down = scp_argv(&target(22, None), "/remote/b.txt", "/local/a.txt", false);
        assert!(!down.iter().any(|a| a == "-r"), "{down:?}");
        assert_eq!(down[down.len() - 2], "/remote/b.txt");
        assert_eq!(down[down.len() - 1], "/local/a.txt");
    }

    #[test]
    fn profile_resolution_wins_and_unknown_profile_lists_known_names() {
        let wd = temp_test_dir("st-profile");
        let key = wd.join("id_ed25519");
        fs::write(&key, "not a real key").unwrap();
        write_ssh_config(
            &wd,
            &json!({ "profiles": [{
                "name": "prod", "host": "example.ru", "user": "deploy",
                "port": 2222, "keyPath": key.display().to_string(),
                "hostKeyPolicy": "strict"
            }] })
            .to_string(),
        );
        // profile wins over explicit fields
        let t = resolve_target(
            &json!({ "profile": "prod", "host": "ignored.ru", "user": "ignored" }),
            &harness_ctx(&wd),
        )
        .unwrap();
        assert_eq!(t.host, "example.ru");
        assert_eq!(t.user, "deploy");
        assert_eq!(t.port, 2222);
        assert_eq!(t.strict_host_key, "yes");
        assert_eq!(t.key_path.as_deref(), Some(key.as_path()));
        assert_eq!(t.password, None);
        // unknown profile: not_found, message lists the known profiles
        let err = resolve_target(&json!({ "profile": "ghost" }), &harness_ctx(&wd)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found") && msg.contains("prod"), "{msg}");
        let r = SshExecTool
            .execute(&json!({ "profile": "ghost", "command": "ls" }), &harness_ctx(&wd))
            .unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn host_key_policy_values_are_mapped_or_rejected() {
        let wd = temp_test_dir("st-hkp");
        write_ssh_config(
            &wd,
            r#"{ "profiles": [
                 { "name": "off", "host": "example.ru", "user": "u", "hostKeyPolicy": "off" },
                 { "name": "bad", "host": "example.ru", "user": "u", "hostKeyPolicy": "trust-me" }
               ] }"#,
        );
        assert_eq!(
            resolve_target(&json!({ "profile": "off" }), &harness_ctx(&wd))
                .unwrap()
                .strict_host_key,
            "no"
        );
        assert_eq!(
            resolve_target(
                &json!({ "host": "example.ru", "user": "u" }),
                &harness_ctx(&wd)
            )
            .unwrap()
            .strict_host_key,
            "accept-new"
        );
        let err = resolve_target(&json!({ "profile": "bad" }), &harness_ctx(&wd)).unwrap_err();
        assert!(err.to_string().contains("hostKeyPolicy"), "{err}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn explicit_target_requires_host_and_user_and_valid_fields() {
        let wd = temp_test_dir("st-explicit");
        let cases = [
            (json!({ "host": "example.ru" }), "host and user"),
            (json!({ "user": "deploy" }), "host and user"),
            (json!({ "host": "bad host", "user": "u" }), "host"),
            (json!({ "host": "example.ru", "user": "bad user" }), "user"),
            (json!({ "host": "-oProxyCommand=x", "user": "u" }), "host"),
            (json!({ "host": "examp@le.ru", "user": "u" }), "host"),
            (json!({ "host": "example.ru", "user": "u", "port": 0 }), "range"),
            (json!({ "host": "example.ru", "user": "u", "port": 70000 }), "range"),
        ];
        for (args, needle) in cases {
            let err = resolve_target(&args, &harness_ctx(&wd)).unwrap_err();
            assert!(err.to_string().contains(needle), "{args}: {err}");
        }
        let t = resolve_target(
            &json!({ "host": "example.ru", "user": "deploy" }),
            &harness_ctx(&wd),
        )
        .unwrap();
        assert_eq!(t.port, 22);
        assert_eq!(t.strict_host_key, "accept-new");
        assert_eq!(t.key_path, None);
        assert_eq!(t.password, None);
        // an explicit ad-hoc password is carried on the target, empty ones dropped
        let t = resolve_target(
            &json!({ "host": "example.ru", "user": "deploy", "password": "hunter2" }),
            &harness_ctx(&wd),
        )
        .unwrap();
        assert_eq!(t.password.as_deref(), Some("hunter2"));
        let t = resolve_target(
            &json!({ "host": "example.ru", "user": "deploy", "password": "" }),
            &harness_ctx(&wd),
        )
        .unwrap();
        assert_eq!(t.password, None);
        // a missing keyPath is a clear not-found error, home shortened to ~
        let err = resolve_target(
            &json!({ "host": "example.ru", "user": "u", "keyPath": "~/definitely-missing-key" }),
            &harness_ctx(&wd),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found") && msg.starts_with("invalid input: ssh key file not found: ~/"), "{msg}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn destructive_labeling_flags_patterns_and_wrappers() {
        let cases = [
            ("rm -rf /tmp/x", true),
            ("rm -fr dir", true),
            ("rm --recursive dir", true),
            ("rm dir/file", false),
            ("sudo rm -rf /srv", true),
            ("timeout 5 dd if=/dev/zero of=/dev/sdb", true),
            ("mkfs.ext4 /dev/sdb1", true),
            ("sudo shutdown -h now", true),
            ("reboot", true),
            ("nohup reboot &", true),
            ("env FOO=1 shutdown", true),
            (":(){ :|:& };:", true),
            ("echo x > /dev/sda", true),
            ("git reset --hard HEAD~1", true),
            ("git -C repo reset --hard", true),
            ("git commit -m x", false),
            ("chmod -R 777 /var/www", true),
            ("chmod 644 f", false),
            ("pkill nginx", true),
            ("killall node", true),
            ("ls -la && echo done", false),
            ("\"rm\" -rf x", true),
            ("echo 'deploy the app'", false),
        ];
        for (cmd, expect) in cases {
            let reasons = destructive_reasons(cmd);
            assert_eq!(!reasons.is_empty(), expect, "{cmd:?} -> {reasons:?}");
            for r in &reasons {
                assert!(!r.is_empty());
            }
        }
        // reasons name the matched pattern
        assert!(destructive_reasons("rm -rf /")[0].contains("rm"));
        assert!(destructive_reasons(":(){ :|:& };:").contains(&"fork-бомба".to_string()));
    }

    #[test]
    fn exec_danger_reason_includes_destructive_analysis() {
        let wd = temp_test_dir("st-danger");
        let ctx = harness_ctx(&wd);
        let r = SshExecTool
            .danger_reason(
                &json!({ "host": "example.ru", "user": "deploy", "command": "sudo rm -rf /srv" }),
                &ctx,
            )
            .unwrap();
        assert!(r.contains("example.ru"), "{r}");
        assert!(r.contains("rm"), "{r}");
        // not on the read-only allowlist: confirms, without destructive flags
        let safe = SshExecTool
            .danger_reason(&json!({ "host": "example.ru", "user": "deploy", "command": "echo hi" }), &ctx)
            .unwrap();
        assert!(!safe.contains("деструктив"), "{safe}");
        // read-only commands never confirm, even with an unresolvable target
        assert!(SshExecTool
            .danger_reason(&json!({ "command": "ls" }), &ToolCtx::default())
            .is_none());
        // unresolvable target still yields a reason for non-read-only commands
        assert!(SshExecTool
            .danger_reason(&json!({ "command": "echo hi" }), &ToolCtx::default())
            .is_some());
        assert!(SshListProfilesTool
            .danger_reason(&json!({}), &ctx)
            .is_none());
        assert!(SshCopyTool::upload()
            .danger_reason(&json!({ "localPath": "a", "remotePath": "b" }), &ctx)
            .is_some());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn ssh_exec_read_only_commands_skip_confirmation() {
        let wd = temp_test_dir("st-readonly");
        let ctx = harness_ctx(&wd);
        let reason = |command: &str| {
            SshExecTool.danger_reason(
                &json!({ "host": "example.ru", "user": "deploy", "command": command }),
                &ctx,
            )
        };
        for safe in [
            "ls",
            "ls -la /var/log",
            "cat /etc/hosts",
            "head -n 5 f",
            "tail -f log",
            "grep -i x f",
            "find . -name x",
            "ps aux",
            "top -bn1",
            "df -h",
            "du -sh .",
            "free -m",
            "uptime",
            "whoami",
            "id",
            "pwd",
            "uname -a",
            "hostname",
            "date",
            "ss -tlnp",
            "netstat -tuln",
            "wc -l f",
            "stat f",
            "file f",
            "git status",
            "git log --oneline -5",
            "git diff HEAD",
            "docker ps -a",
            "systemctl status nginx",
            "ip addr show",
        ] {
            assert!(reason(safe).is_none(), "read-only {safe:?} must not confirm");
        }
        for unsafe_case in [
            "ls | wc -l",
            "ls > out.txt",
            "cat < in.txt",
            "ls && echo done",
            "false || ls",
            "ls &",
            "echo $(whoami)",
            "ls `id`",
            "ls\nrm -rf /",
            "rm -rf /tmp/x",
            "sudo ls",
            "apt-get update",
            "git push",
            "git -C repo status",
            "docker ps | wc -l",
            "ip route",
            "unknowncmd",
            "",
            "   ",
        ] {
            assert!(reason(unsafe_case).is_some(), "{unsafe_case:?} must confirm");
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn local_paths_stay_inside_the_project_in_project_mode() {
        let (root, ctx) = project_env("st-local");
        let wd = ctx.working_dir.clone().unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        assert!(resolve_local_path("sub", &ctx, &wd, true).is_ok());
        for bad in ["../x", "/etc", "a/../../x", ".."] {
            let err = resolve_local_path(bad, &ctx, &wd, false).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("escapes") || msg.contains("not project-relative"),
                "{bad}: {msg}"
            );
        }
        // download destinations may not exist yet, uploads must
        assert!(resolve_local_path("out.txt", &ctx, &wd, false).is_ok());
        assert!(resolve_local_path("out.txt", &ctx, &wd, true)
            .unwrap_err()
            .to_string()
            .contains("not found"));
        // harness mode: absolute paths are accepted
        let hwd = temp_test_dir("st-local-h");
        fs::write(hwd.join("f.txt"), "x").unwrap();
        let abs = hwd.join("f.txt").display().to_string();
        assert_eq!(
            resolve_local_path(&abs, &harness_ctx(&hwd), &hwd, true).unwrap(),
            hwd.join("f.txt").canonicalize().unwrap()
        );
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&hwd).unwrap();
    }

    #[test]
    fn remote_paths_reject_newlines_and_nul_but_pass_through() {
        assert!(check_remote_path("/var/www/app/file.tar.gz").is_ok());
        assert!(check_remote_path("path with spaces").is_ok());
        assert!(check_remote_path("").is_err());
        assert!(check_remote_path("a\nb").is_err());
        assert!(check_remote_path("a\u{0}b").is_err());
        // the tool surfaces it as invalid_args before any local resolution
        let wd = temp_test_dir("st-remote");
        let r = SshCopyTool::upload()
            .execute(
                &json!({ "host": "example.ru", "user": "deploy", "localPath": "missing.txt", "remotePath": "a\nb" }),
                &harness_ctx(&wd),
            )
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
        // missing upload source is not_found (and never reaches scp)
        let r = SshCopyTool::upload()
            .execute(
                &json!({ "host": "example.ru", "user": "deploy", "localPath": "missing.txt", "remotePath": "/tmp/x" }),
                &harness_ctx(&wd),
            )
            .unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn output_cap_marks_truncation() {
        let (text, truncated) = read_capped(Cursor::new("x".repeat(30_000)), SSH_OUTPUT_CAP);
        assert_eq!(text.len(), SSH_OUTPUT_CAP);
        assert!(truncated);
        let (text, truncated) = read_capped(Cursor::new("short"), SSH_OUTPUT_CAP);
        assert_eq!(text, "short");
        assert!(!truncated);
    }

    #[test]
    fn list_profiles_never_exposes_key_paths() {
        let wd = temp_test_dir("st-list");
        write_ssh_config(
            &wd,
            r#"{ "profiles": [{ "name": "prod", "host": "example.ru", "user": "deploy", "port": 2222, "keyPath": "/home/deploy/.ssh/id_ed25519" }] }"#,
        );
        let r = SshListProfilesTool
            .execute(&json!({}), &harness_ctx(&wd))
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["profiles"][0]["name"], "prod");
        assert_eq!(data["profiles"][0]["port"], 2222);
        let text = data.to_string();
        assert!(!text.contains("id_ed25519"), "{text}");
        assert!(!text.contains("keyPath"), "{text}");
        // missing ssh.json: empty list, no error
        let empty_wd = temp_test_dir("st-list-empty");
        let r = SshListProfilesTool
            .execute(&json!({}), &harness_ctx(&empty_wd))
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["profiles"], json!([]));
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&empty_wd).unwrap();
    }

    #[test]
    fn ssh_config_default_shape_and_unknown_keys_survive() {
        let dir = temp_test_dir("st-config");
        config::ensure_defaults(&dir).unwrap();
        assert_eq!(
            config::read_config(&dir, ConfigKind::Ssh).unwrap(),
            json!({ "profiles": [] })
        );
        config::write_config(
            &dir,
            ConfigKind::Ssh,
            json!({ "profiles": [{
                "name": "prod", "host": "example.ru", "user": "deploy", "port": 22,
                "unknownInProfile": true
            }], "futureTop": true }),
        )
        .unwrap();
        let cfg = load_config(&dir).unwrap();
        assert_eq!(cfg.profiles[0].name, "prod");
        assert_eq!(cfg.profiles[0].port, 22);
        assert_eq!(cfg.extra["futureTop"], true);
        assert_eq!(cfg.profiles[0].extra["unknownInProfile"], true);
        // typed round-trip keeps the unknown keys
        config::write_config(&dir, ConfigKind::Ssh, serde_json::to_value(&cfg).unwrap()).unwrap();
        let raw = config::read_config(&dir, ConfigKind::Ssh).unwrap();
        assert_eq!(raw["futureTop"], true);
        assert_eq!(raw["profiles"][0]["unknownInProfile"], true);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn classes_policies_and_schemas_match_the_spec() {
        assert_eq!(SshListProfilesTool.class(), ToolClass::ReadOnly);
        assert_eq!(SshListProfilesTool.policy(), PermissionPolicy::Skip);
        // ssh_exec is per-call: read-only commands skip, the rest confirm
        assert_eq!(SshExecTool.class(), ToolClass::Mutating);
        assert_eq!(SshExecTool.policy(), PermissionPolicy::CheckConditional);
        assert!(SshExecTool
            .danger_reason(&json!({ "host": "example.ru", "user": "u", "command": "ls" }), &ToolCtx::default())
            .is_none());
        assert!(SshExecTool
            .danger_reason(&json!({ "host": "example.ru", "user": "u", "command": "rm -rf /tmp/x" }), &ToolCtx::default())
            .is_some());
        let mutating: Vec<Box<dyn Tool>> = vec![
            Box::new(SshCopyTool::upload()),
            Box::new(SshCopyTool::download()),
            Box::new(SshGenerateKeyTool),
            Box::new(SshInstallKeyTool),
        ];
        for t in mutating {
            assert_eq!(t.class(), ToolClass::Mutating, "{}", t.name());
            assert_eq!(t.policy(), PermissionPolicy::Check, "{}", t.name());
            assert!(
                t.danger_reason(&json!({ "command": "ls", "localPath": "a", "remotePath": "b" }), &ToolCtx::default())
                    .is_some(),
                "{} danger_reason",
                t.name()
            );
        }
        let exec = SshExecTool.parameters();
        assert!(validate_args(&exec, &json!({ "command": "ls" })).is_ok());
        assert!(validate_args(&exec, &json!({})).unwrap_err().contains("command"));
        assert!(validate_args(&exec, &json!({ "command": "ls", "bogus": 1 }))
            .unwrap_err()
            .contains("bogus"));
        assert!(validate_args(&exec, &json!({ "command": "ls", "port": "x" }))
            .unwrap_err()
            .contains("integer"));
        // new ssh_exec options validate against the schema
        assert!(validate_args(
            &exec,
            &json!({ "command": "ls", "timeoutSec": 300, "loginShell": false, "cwd": "/srv", "pty": true })
        )
        .is_ok());
        assert!(validate_args(&exec, &json!({ "command": "ls", "timeoutSec": "x" }))
            .unwrap_err()
            .contains("integer"));
        assert!(validate_args(&exec, &json!({ "command": "ls", "pty": "yes" }))
            .unwrap_err()
            .contains("boolean"));
        assert!(validate_args(&exec, &json!({ "command": "ls", "cwd": 5 }))
            .unwrap_err()
            .contains("string"));
        let copy = SshCopyTool::upload().parameters();
        assert!(validate_args(&copy, &json!({ "localPath": "a", "remotePath": "b" })).is_ok());
        assert!(validate_args(&copy, &json!({ "localPath": "a" }))
            .unwrap_err()
            .contains("remotePath"));
        assert!(validate_args(&copy, &json!({ "localPath": "a", "remotePath": "b", "recursive": "yes" }))
            .unwrap_err()
            .contains("boolean"));
        // password is an accepted ad-hoc auth field on exec and copy
        assert!(validate_args(&exec, &json!({ "command": "ls", "password": "p" })).is_ok());
        assert!(validate_args(&copy, &json!({ "localPath": "a", "remotePath": "b", "password": "p" })).is_ok());
        // key generation: everything optional, extras refused
        let gen = SshGenerateKeyTool.parameters();
        assert!(validate_args(&gen, &json!({})).is_ok());
        assert!(validate_args(&gen, &json!({ "path": "p", "comment": "c" })).is_ok());
        assert!(validate_args(&gen, &json!({ "bogus": 1 })).unwrap_err().contains("bogus"));
        // install: keyPath required, target fields optional
        let inst = SshInstallKeyTool.parameters();
        assert!(validate_args(&inst, &json!({ "keyPath": "k" })).is_ok());
        assert!(validate_args(&inst, &json!({ "profile": "p", "password": "x", "keyPath": "k" })).is_ok());
        assert!(validate_args(&inst, &json!({ "host": "h" })).unwrap_err().contains("keyPath"));
        // save-to-datas: the four core fields are required
        assert_eq!(SshSaveProfileToDatasTool.class(), ToolClass::Mutating);
        assert_eq!(SshSaveProfileToDatasTool.policy(), PermissionPolicy::Skip);
        let save = SshSaveProfileToDatasTool.parameters();
        assert!(validate_args(&save, &json!({ "name": "n", "host": "h", "user": "u", "port": 22 })).is_ok());
        assert!(validate_args(&save, &json!({ "name": "n", "host": "h", "user": "u", "port": 22, "password": "p", "keyPath": "k" })).is_ok());
        assert!(validate_args(&save, &json!({ "name": "n" })).unwrap_err().contains("host"));
        assert!(validate_args(&save, &json!({ "name": "n", "host": "h", "user": "u", "port": "x" }))
            .unwrap_err()
            .contains("integer"));
    }

    #[test]
    fn parse_datas_ssh_reads_valid_sections() {
        let text = "# Project example.ru\n\nplain notes, host: not-a-profile\n\n\
             ## SSH prod\nhost: example.ru\nuser: deploy\nport: 2222\npassword: s3cret\nkey: ~/.ssh/id_ed25519\nextra prose here\n\n\
             ## FTP notes\nhost: ftp.example.ru\nlogin: user-1\n\n\
             ## ssh staging\nHOST: stage.example.ru\nUser: web\nKeyPath: ~/keys/stage\n";
        let ps = parse_datas_ssh(text);
        assert_eq!(ps.len(), 2, "{ps:?}");
        let prod = &ps[0];
        assert_eq!(prod.name, "prod");
        assert_eq!(prod.host, "example.ru");
        assert_eq!(prod.user, "deploy");
        assert_eq!(prod.port, 2222);
        assert_eq!(prod.password.as_deref(), Some("s3cret"));
        assert_eq!(prod.key_path.as_deref(), Some("~/.ssh/id_ed25519"));
        assert_eq!(prod.source, "datas");
        let staging = &ps[1];
        assert_eq!(staging.name, "staging");
        assert_eq!(staging.host, "stage.example.ru");
        assert_eq!(staging.port, 22);
        assert_eq!(staging.password, None);
        assert_eq!(staging.key_path.as_deref(), Some("~/keys/stage"));
    }

    #[test]
    fn parse_datas_ssh_accepts_russian_freeform_sections() {
        let text = "# notes\n\n## Доступ к серверу\n\n- Хост: 192.0.2.10\n- Пользователь: ivan\n- Порт: 2222\n- Пароль: secret-1!\n- Ключ: ~/.ssh/id_ed25519\n- SSH: `ssh ivan@192.0.2.10` (порт по умолчанию 22)\n\n## Notes\njust prose, no fields\n";
        let ps = parse_datas_ssh(text);
        assert_eq!(ps.len(), 1, "{ps:?}");
        let p = &ps[0];
        assert_eq!(p.name, "Доступ к серверу");
        assert_eq!(p.host, "192.0.2.10");
        assert_eq!(p.user, "ivan");
        assert_eq!(p.port, 2222);
        assert_eq!(p.password.as_deref(), Some("secret-1!"));
        assert_eq!(p.key_path.as_deref(), Some("~/.ssh/id_ed25519"));
        assert_eq!(p.source, "datas");
    }

    #[test]
    fn parse_datas_ssh_skips_malformed_entries() {
        let text = "## SSH nohost\nuser: u\n\n\
             ## SSH nokey\nhost: example.ru\n\n\
             ## SSH badport\nhost: example.ru\nuser: u\nport: abc\n\n\
             ## SSH bigport\nhost: example.ru\nuser: u\nport: 99999\n\n\
             ## SSH\nhost: example.ru\nuser: u\n\n\
             ## SSH badhost\nhost: bad host\nuser: u\n\n\
             ## SSH empty\n\n\
             ## SSH ok\nhost: example.ru\nuser: u\n";
        let ps = parse_datas_ssh(text);
        assert_eq!(ps.len(), 1, "{ps:?}");
        assert_eq!(ps[0].name, "ok");
        // empty text: no profiles
        assert!(parse_datas_ssh("").is_empty());
    }

    #[test]
    fn list_profiles_merges_config_and_datas_and_never_leaks_secrets() {
        let (root, ctx) = project_env("st-merge");
        let wd = ctx.working_dir.clone().unwrap();
        write_ssh_config(
            &wd,
            &json!({ "profiles": [
                { "name": "web", "host": "web.example.ru", "user": "deploy", "port": 22, "keyPath": "/keys/web" },
                { "name": "cfg", "host": "cfg.example.ru", "user": "u", "port": 2222 }
            ] })
            .to_string(),
        );
        fs::write(
            root.join(paths::DATAS_MD),
            "## SSH db\nhost: db.example.ru\nuser: admin\nport: 33\npassword: hunter2\n\n## SSH web\nhost: web2.example.ru\nuser: admin\npassword: datas-pw\n",
        )
        .unwrap();
        let r = SshListProfilesTool.execute(&json!({}), &ctx).unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let text = data.to_string();
        assert!(!text.contains("hunter2") && !text.contains("datas-pw"), "{text}");
        assert!(!text.contains("keyPath") && !text.contains("/keys/web"), "{text}");
        let by_name = |name: &str| -> Value {
            data["profiles"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"] == name)
                .cloned()
                .unwrap_or(Value::Null)
        };
        // a datas entry replaces the config entry of the same name
        let web = by_name("web");
        assert_eq!(web["source"], "datas");
        assert_eq!(web["host"], "web2.example.ru");
        assert_eq!(web["hasPassword"], true);
        assert_eq!(web["auth"], "password");
        let dupes = data["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["name"] == "web")
            .count();
        assert_eq!(dupes, 1, "{text}");
        let db = by_name("db");
        assert_eq!(db["source"], "datas");
        assert_eq!(db["port"], 33);
        assert_eq!(db["auth"], "password");
        let cfg = by_name("cfg");
        assert_eq!(cfg["source"], "config");
        assert_eq!(cfg["hasPassword"], false);
        // no explicit key/password: still key auth via ssh-agent
        assert_eq!(cfg["auth"], "key");
        // resolution from datas carries the password and key invisibly
        let t = resolve_target(&json!({ "profile": "db" }), &ctx).unwrap();
        assert_eq!(t.password.as_deref(), Some("hunter2"));
        assert_eq!(t.port, 33);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn password_auth_switches_ssh_options_and_keeps_the_secret_out_of_argv() {
        let o = opts(true, None, false, None);
        // password only: BatchMode=no, exactly one prompt, no secret in argv
        let argv = ssh_exec_argv(&target_pw(22, None, Some("hunter2")), "ls", &o);
        let joined = argv.join(" ");
        assert!(joined.contains("BatchMode=no"), "{joined}");
        assert!(joined.contains("NumberOfPasswordPrompts=1"), "{joined}");
        assert!(!joined.contains("BatchMode=yes"), "{joined}");
        assert!(!joined.contains("hunter2"), "{joined}");
        // key + password: -i stays, publickey is tried before the password
        let argv = ssh_exec_argv(&target_pw(2222, Some("/keys/id"), Some("hunter2")), "ls", &o);
        let joined = argv.join(" ");
        assert!(joined.contains("-i /keys/id"), "{joined}");
        assert!(
            joined.contains("PreferredAuthentications=publickey,password,keyboard-interactive"),
            "{joined}"
        );
        assert!(joined.contains("BatchMode=no"), "{joined}");
        assert!(!joined.contains("hunter2"), "{joined}");
        // key only: BatchMode=yes, no prompt plumbing at all
        let joined = ssh_exec_argv(&target(22, Some("/keys/id")), "ls", &o).join(" ");
        assert!(joined.contains("BatchMode=yes"), "{joined}");
        assert!(!joined.contains("NumberOfPasswordPrompts"), "{joined}");
        assert!(!joined.contains("PreferredAuthentications"), "{joined}");
        // scp gets the same option set through common_opts
        let sp = scp_argv(&target_pw(2222, None, Some("hunter2")), "a", "b", false);
        let joined = sp.join(" ");
        assert!(joined.contains("BatchMode=no"), "{joined}");
        assert!(!joined.contains("hunter2"), "{joined}");
    }

    #[cfg(unix)]
    #[test]
    fn askpass_files_are_private_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let wd = temp_test_dir("st-askpass");
        let a = Askpass::create(&wd, "hunter2").unwrap();
        assert!(a.dir.starts_with(paths::harness_tmp_dir(&wd)), "{:?}", a.dir);
        let pass = a.dir.join("pass");
        assert_eq!(fs::metadata(&a.dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&pass).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&a.script).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::read_to_string(&pass).unwrap(), "hunter2");
        // the script itself never embeds the secret, it cats the 0600 file
        let script_text = fs::read_to_string(&a.script).unwrap();
        assert!(!script_text.contains("hunter2"), "{script_text}");
        let out = Command::new(&a.script).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hunter2");
        let envs = a.envs();
        let get = |k: &str| {
            envs.iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("SSH_ASKPASS_REQUIRE").as_deref(), Some("force"));
        assert_eq!(get("DISPLAY").as_deref(), Some(":0"));
        assert_eq!(get("SSH_ASKPASS").as_deref(), a.script.to_str());
        // success or failure alike: Drop removes the whole private dir
        let dir = a.dir.clone();
        drop(a);
        assert!(!dir.exists() && !pass.exists());
        fs::remove_dir_all(&wd).unwrap();
    }

    // Backdate a path's mtime (futimens through a read-only dir fd works
    // on macOS; the ops tmp-cleanup tests use the same trick for files).
    fn backdate(path: &Path, age: Duration) {
        let file = fs::File::open(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    #[test]
    fn purge_stale_askpass_dirs_removes_only_stale_leftovers() {
        let wd = temp_test_dir("st-askpass-purge");
        let tmp = paths::harness_tmp_dir(&wd);
        fs::create_dir_all(&tmp).unwrap();
        let two_hours = Duration::from_secs(7200);

        // stale: dead pid + old mtime, with a nested password file
        let stale = tmp.join("ssh-askpass-999999-1");
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("pass"), "hunter2").unwrap();
        backdate(&stale, two_hours);

        // live pid: our own process, even with an old mtime
        let live = tmp.join(format!("ssh-askpass-{}-2", std::process::id()));
        fs::create_dir(&live).unwrap();
        backdate(&live, two_hours);

        // young: dead pid but created just now (a run of another instance)
        let young = tmp.join("ssh-askpass-999998-3");
        fs::create_dir(&young).unwrap();

        // not our naming scheme: malformed names and foreign entries survive
        let malformed = tmp.join("ssh-askpass-notapid-4");
        fs::create_dir(&malformed).unwrap();
        backdate(&malformed, two_hours);
        let no_nonce = tmp.join("ssh-askpass-999997");
        fs::create_dir(&no_nonce).unwrap();
        backdate(&no_nonce, two_hours);
        fs::write(tmp.join("other.txt"), "keep").unwrap();

        // a same-named stale dir outside Harness/tmp is never touched
        let outside_tmp = paths::harness_dir(&wd).join("ssh-askpass-999996-5");
        fs::create_dir(&outside_tmp).unwrap();
        backdate(&outside_tmp, two_hours);

        assert_eq!(purge_stale_askpass_dirs(&wd), 1);
        assert!(!stale.exists());
        assert!(live.exists(), "live-pid dir was purged");
        assert!(young.exists(), "young dir was purged");
        assert!(malformed.exists() && no_nonce.exists());
        assert!(tmp.join("other.txt").is_file());
        assert!(outside_tmp.is_dir(), "purge reached outside Harness/tmp");
        // idempotent: nothing left to remove
        assert_eq!(purge_stale_askpass_dirs(&wd), 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn purge_stale_askpass_dirs_never_follows_symlinks_out_of_tmp() {
        use std::os::unix::fs::symlink;
        let wd = temp_test_dir("st-askpass-purge-link");
        let tmp = paths::harness_tmp_dir(&wd);
        fs::create_dir_all(&tmp).unwrap();
        let two_hours = Duration::from_secs(7200);

        // outside Harness/tmp: must survive in both cases
        let outside = wd.join("outside");
        fs::create_dir_all(outside.join("secret")).unwrap();
        let leak = outside.join("secret/leak.txt");
        fs::write(&leak, "leak").unwrap();

        // a stale dir whose inside entry is a symlink out of tmp
        let stale = tmp.join("ssh-askpass-999999-1");
        fs::create_dir(&stale).unwrap();
        symlink(&outside, stale.join("link")).unwrap();
        backdate(&stale, two_hours);

        // a matching symlink entry itself: unlinked, target untouched
        let link = tmp.join("ssh-askpass-999998-2");
        symlink(&outside, &link).unwrap();

        assert_eq!(purge_stale_askpass_dirs(&wd), 2);
        assert!(!stale.exists() && !link.exists());
        assert!(leak.is_file(), "the purge followed a symlink out of Harness/tmp");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn install_key_command_is_idempotent_and_quoted() {
        let cmd = install_key_command("ssh-ed25519 AAAAC3NzaC1lZDI11 key comment");
        assert!(cmd.contains("mkdir -p ~/.ssh"), "{cmd}");
        assert!(cmd.contains("chmod 700 ~/.ssh"), "{cmd}");
        assert!(
            cmd.contains("grep -qxF 'ssh-ed25519 AAAAC3NzaC1lZDI11 key comment' ~/.ssh/authorized_keys"),
            "{cmd}"
        );
        assert!(
            cmd.contains("|| echo 'ssh-ed25519 AAAAC3NzaC1lZDI11 key comment' >> ~/.ssh/authorized_keys"),
            "{cmd}"
        );
        assert!(cmd.ends_with("chmod 600 ~/.ssh/authorized_keys"), "{cmd}");
        // an embedded single quote in the key comment is shell-escaped
        let cmd = install_key_command("ssh-ed25519 AAA it's");
        assert!(cmd.contains("'ssh-ed25519 AAA it'\\''s'"), "{cmd}");
    }

    #[test]
    fn generate_key_creates_a_real_keypair_and_refuses_overwrite() {
        let wd = temp_test_dir("st-keygen");
        let ctx = harness_ctx(&wd);
        let path = wd.join("keys/id_ed25519");
        let r = SshGenerateKeyTool
            .execute(
                &json!({ "path": path.display().to_string(), "comment": "test@s" }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let public = data["publicKey"].as_str().unwrap();
        assert!(public.starts_with("ssh-ed25519 "), "{public}");
        assert!(public.contains("test@s"), "{public}");
        assert!(path.is_file());
        assert!(Path::new(&format!("{}.pub", path.display())).is_file());
        assert_eq!(data["privateKeyPath"], path.display().to_string());
        // an existing key file is never overwritten
        let r = SshGenerateKeyTool
            .execute(&json!({ "path": path.display().to_string() }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
        // harness default: Harness/tmp/ssh/id_ed25519
        let r = SshGenerateKeyTool.execute(&json!({}), &ctx).unwrap();
        let p = r.data.unwrap()["privateKeyPath"].as_str().unwrap().to_string();
        assert!(
            p.starts_with(&paths::harness_tmp_dir(&wd).join("ssh").display().to_string()),
            "{p}"
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn generate_key_defaults_inside_the_project_folder() {
        let (root, ctx) = project_env("st-keygen-p");
        let wd = ctx.working_dir.clone().unwrap();
        let r = SshGenerateKeyTool.execute(&json!({}), &ctx).unwrap();
        assert!(r.ok, "{:?}", r.error);
        let p = r.data.unwrap()["privateKeyPath"].as_str().unwrap().to_string();
        // project_root is canonicalized (/private/var on macOS), the raw
        // project_env root is not: compare through canonicalize().
        assert_eq!(
            p,
            root.canonicalize().unwrap().join(".ssh/id_ed25519").display().to_string()
        );
        assert!(root.join(".ssh/id_ed25519").is_file());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn save_profile_to_datas_appends_and_updates_in_place() {
        let (root, ctx) = project_env("st-save");
        let wd = ctx.working_dir.clone().unwrap();
        let datas = root.join(paths::DATAS_MD);
        let r = SshSaveProfileToDatasTool
            .execute(
                &json!({ "name": "prod", "host": "example.ru", "user": "deploy", "port": 2222, "password": "hunter2", "keyPath": "~/.ssh/id_ed25519" }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.as_ref().unwrap()["updated"], false);
        // the result never echoes the password
        assert!(!r.data.unwrap().to_string().contains("hunter2"));
        let text = fs::read_to_string(&datas).unwrap();
        assert!(text.contains("## SSH prod"), "{text}");
        assert!(text.contains("password: hunter2"), "{text}");
        assert!(text.contains("key: ~/.ssh/id_ed25519"), "{text}");
        // update replaces the section in place, the old password is gone
        let r = SshSaveProfileToDatasTool
            .execute(
                &json!({ "name": "prod", "host": "example.ru", "user": "deploy", "port": 22, "password": "newpass" }),
                &ctx,
            )
            .unwrap();
        assert_eq!(r.data.unwrap()["updated"], true);
        let text = fs::read_to_string(&datas).unwrap();
        assert_eq!(text.matches("## SSH prod").count(), 1, "{text}");
        assert!(!text.contains("hunter2"), "{text}");
        let ps = parse_datas_ssh(&text);
        assert_eq!(ps.len(), 1, "{ps:?}");
        assert_eq!(ps[0].password.as_deref(), Some("newpass"));
        assert_eq!(ps[0].port, 22);
        assert_eq!(ps[0].key_path, None);
        // a hand-written lowercase heading is updated in place, not duplicated
        fs::write(&datas, "## ssh legacy\nhost: old.ru\nuser: old\nport: 22\n").unwrap();
        let r = SshSaveProfileToDatasTool
            .execute(&json!({ "name": "legacy", "host": "new.ru", "user": "u", "port": 22 }), &ctx)
            .unwrap();
        assert_eq!(r.data.unwrap()["updated"], true);
        let text = fs::read_to_string(&datas).unwrap();
        assert!(text.contains("## ssh legacy"), "{text}");
        let ps = parse_datas_ssh(&text);
        assert_eq!(ps.len(), 1, "{ps:?}");
        assert_eq!(ps[0].host, "new.ru");
        // missing datas.md is created
        fs::remove_file(&datas).unwrap();
        let r = SshSaveProfileToDatasTool
            .execute(&json!({ "name": "fresh", "host": "f.example.ru", "user": "u", "port": 22 }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let ps = parse_datas_ssh(&fs::read_to_string(&datas).unwrap());
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].name, "fresh");
        // bad args are refused
        let r = SshSaveProfileToDatasTool
            .execute(&json!({ "name": "#x", "host": "example.ru", "user": "u", "port": 22 }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
        let r = SshSaveProfileToDatasTool
            .execute(&json!({ "name": "n", "host": "example.ru", "user": "u", "port": 0 }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn secrets_never_reach_danger_reasons_or_masked_output() {
        let (root, ctx) = project_env("st-mask");
        let wd = ctx.working_dir.clone().unwrap();
        fs::write(
            root.join(paths::DATAS_MD),
            "## SSH db\nhost: db.example.ru\nuser: admin\npassword: hunter2\n",
        )
        .unwrap();
        let reason = SshExecTool
            .danger_reason(&json!({ "profile": "db", "command": "apt-get update" }), &ctx)
            .unwrap();
        assert!(reason.contains("db.example.ru"), "{reason}");
        assert!(!reason.contains("hunter2"), "{reason}");
        let reason = SshInstallKeyTool
            .danger_reason(&json!({ "profile": "db", "keyPath": "/keys/id" }), &ctx)
            .unwrap();
        assert!(!reason.contains("hunter2"), "{reason}");
        // the journal masking covers datas.md-style password lines
        assert_eq!(mask_secrets("password: hunter2"), "password: ***");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    #[ignore = "live ssh: needs a reachable sshd and a working key/ssh-agent"]
    fn live_ssh_exec_localhost() {
        let wd = temp_test_dir("st-live");
        let user = std::env::var("USER").unwrap_or_default();
        let r = SshExecTool
            .execute(
                &json!({ "host": "localhost", "user": user, "command": "echo hi" }),
                &harness_ctx(&wd),
            )
            .unwrap();
        let data = r.data.expect("live run must return data");
        assert_eq!(data["stdout"].as_str().unwrap().trim(), "hi");
        fs::remove_dir_all(&wd).unwrap();
    }

    // Full live path over a hand-written Russian datas.md section: profile
    // discovery, askpass password auth, ssh_exec and an scp upload/download
    // round-trip under /tmp with remote cleanup. Credentials come from the
    // environment only: SSH_TEST_HOST, SSH_TEST_USER, SSH_TEST_PASSWORD and
    // the optional SSH_TEST_PORT (default 22).
    #[test]
    #[ignore = "live ssh: set SSH_TEST_HOST/SSH_TEST_USER/SSH_TEST_PASSWORD (optional SSH_TEST_PORT)"]
    fn live_ssh_profile_exec_and_scp_roundtrip() {
        let host = std::env::var("SSH_TEST_HOST").expect("SSH_TEST_HOST");
        let user = std::env::var("SSH_TEST_USER").expect("SSH_TEST_USER");
        let password = std::env::var("SSH_TEST_PASSWORD").expect("SSH_TEST_PASSWORD");
        let port: u16 = std::env::var("SSH_TEST_PORT")
            .ok()
            .map(|p| p.parse().expect("SSH_TEST_PORT must be a number"))
            .unwrap_or(22);
        let (root, ctx) = project_env("st-live-profile");
        let wd = ctx.working_dir.clone().unwrap();
        fs::write(
            root.join(paths::DATAS_MD),
            format!(
                "## Доступ к серверу\n\n- Хост: {host}\n- Пользователь: {user}\n- Порт: {port}\n- Пароль: {password}\n- SSH: `ssh {user}@{host}`\n"
            ),
        )
        .unwrap();
        // the hand-written section is discovered as a password profile
        let ps = datas_profiles(&ctx);
        assert_eq!(ps.len(), 1, "{ps:?}");
        assert_eq!(ps[0].name, "Доступ к серверу");
        assert_eq!(auth_kind(&ps[0]), "password");
        // ssh_exec through the profile: askpass auth, login shell
        let r = SshExecTool
            .execute(
                &json!({ "profile": "Доступ к серверу", "command": "echo SSH_OK; uname -a; whoami; df -h / | tail -1" }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["exitCode"], 0, "{data}");
        let stdout = data["stdout"].as_str().unwrap();
        assert!(stdout.contains("SSH_OK"), "{stdout}");
        assert!(!data.to_string().contains(&password), "secret leaked in result");
        // scp round-trip under /tmp, then remote cleanup
        let marker = format!("project-s-live-{}", std::process::id());
        fs::write(root.join("live-upload.txt"), &marker).unwrap();
        let remote = format!("/tmp/{marker}.txt");
        let r = SshCopyTool::upload()
            .execute(
                &json!({ "profile": "Доступ к серверу", "localPath": "live-upload.txt", "remotePath": remote }),
                &ctx,
            )
            .unwrap();
        let data = r.data.unwrap();
        assert_eq!(data["exitCode"], 0, "{data}");
        let r = SshCopyTool::download()
            .execute(
                &json!({ "profile": "Доступ к серверу", "localPath": "live-readback.txt", "remotePath": remote }),
                &ctx,
            )
            .unwrap();
        let data = r.data.unwrap();
        assert_eq!(data["exitCode"], 0, "{data}");
        assert_eq!(fs::read_to_string(root.join("live-readback.txt")).unwrap(), marker);
        let r = SshExecTool
            .execute(
                &json!({ "profile": "Доступ к серверу", "command": format!("rm -f {remote}") }),
                &ctx,
            )
            .unwrap();
        assert_eq!(r.data.unwrap()["exitCode"], 0);
        fs::remove_dir_all(&wd).unwrap();
    }
}
