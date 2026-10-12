use super::*;
use crate::{
    agent_identity::{LUOKE_AGENT_ID, MUWA_AGENT_ID},
    db::Database,
};

fn database() -> (Database, PathBuf) {
    let root =
        std::env::temp_dir().join(format!("rovai-member-environment-{}", uuid::Uuid::new_v4()));
    let database = crate::test_support::fresh_schema_database_at(&root);
    (database, root)
}

fn runtime(kind: AdapterKind) -> FrozenAgentRuntimeConfig {
    serde_json::from_value(json!({"adapterKind":kind,"installationId":"fixture-installation","installationGeneration":1,
        "searchEnvironmentGeneration":1,"executablePath":"/bin/sh","authScope":"default","reportedVersion":"2.1.287",
        "executableFingerprint":"fixture","capabilities":[],"protocolVersion":"fixture","model":{"source":"runtime_default","modelId":"default","options":{}},
        "permissions":{"adapterKind":kind,"schemaVersion":1,"values":{}},"nativeSessionCompatibilityKey":null,
        "bindingCompatibilityDigest":"binding","hostConfigDigest":"host","configDigest":"fixture"})).unwrap()
}
fn save(connection: &Connection, member: &str, revision: i64, values: Value) -> Result<()> {
    prepare(
        connection,
        member,
        AdapterKind::Pi,
        &ModelSelection::RuntimeDefault,
        EnvironmentEdit {
            expected_revision: revision,
            json: values.to_string(),
        },
    )?
    .commit(connection)
}

fn conversation(connection: &Connection, camp: &str, member: &str) -> String {
    connection.execute("INSERT OR IGNORE INTO camp(id,title,project_binding_kind,project_path,last_message_sequence,version,created_at,updated_at)
        VALUES(?1,'Env','directory','/tmp',0,1,datetime('now'),datetime('now'))", [camp]).unwrap();
    let id = format!("{camp}-{member}");
    connection.execute("INSERT OR IGNORE INTO conversation(id,camp_id,agent_id,version,created_at,updated_at) VALUES(?1,?2,?3,1,datetime('now'),datetime('now'))",
        params![id,camp,member]).unwrap();
    id
}

#[test]
fn private_versions_preserve_credentials_and_freeze_retries_across_reopen() {
    let (mut database, root) = database();
    let a = LUOKE_AGENT_ID;
    let b = MUWA_AGENT_ID;
    let c = conversation(database.connection(), "camp-a", a);
    let other = conversation(database.connection(), "camp-a", b);
    let other_camp = conversation(database.connection(), "camp-b", a);
    // URL-only is intentional pass-through, including inherited Host credentials.
    save(
        database.connection(),
        a,
        0,
        json!({"ANTHROPIC_BASE_URL":"https://a.example","CLEARED_TOKEN":""}),
    )
    .unwrap();
    let mut admitted = runtime(AdapterKind::Pi);
    freeze_with_host(database.connection(), &c, a, &mut admitted, || {
        Environment::from([
            ("ANTHROPIC_API_KEY".into(), "fixture-host-a".into()),
            ("HOST_ONLY".into(), "old".into()),
        ])
    })
    .unwrap();
    let snapshot = serde_json::to_string(&admitted).unwrap();
    assert!(
        !snapshot.contains("fixture-host-a")
            && !snapshot.contains("a.example")
            && !snapshot.contains("\"identity\"")
    );
    assert_eq!(admitted.host_config_digest, "host");
    assert_eq!(admitted.binding_compatibility_digest, "binding");
    save(database.connection(),a,1,json!({"ANTHROPIC_BASE_URL":"https://b.example","ANTHROPIC_API_KEY":"fixture-member-b","CLEARED_TOKEN":""})).unwrap();
    let masked = get(database.connection(), a, AdapterKind::Pi, false).unwrap();
    assert_eq!(masked["environment"]["ANTHROPIC_API_KEY"], SAVED);
    assert_eq!(masked["environment"]["CLEARED_TOKEN"], "");
    let raw: Vec<u8> = database
        .connection()
        .query_row(
            "SELECT ciphertext FROM member_runtime_environment ORDER BY revision DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("fixture-member-b"));
    assert!(save(database.connection(), a, 0, json!({})).is_err());
    // A changed private body cannot replay as the same update, without any credential digest.
    assert!(
        prepare(
            database.connection(),
            a,
            AdapterKind::Pi,
            &ModelSelection::RuntimeDefault,
            EnvironmentEdit {
                expected_revision: 1,
                json: json!({"ANTHROPIC_API_KEY":"different"}).to_string()
            }
        )
        .is_err()
    );
    let mut continuation = runtime(AdapterKind::Pi);
    freeze_with_host(database.connection(), &c, a, &mut continuation, || {
        panic!("existing session never reads Host again")
    })
    .unwrap();
    assert_eq!(admitted.environment, continuation.environment);
    // Saving, or removing an overlay, does not mutate either a Run or the session snapshot.
    save(
        database.connection(),
        a,
        2,
        json!({"ANTHROPIC_BASE_URL":"https://c.example","ANTHROPIC_API_KEY":SAVED}),
    )
    .unwrap();
    {
        let tx = database.connection_mut().transaction().unwrap();
        save(&tx, a, 3, json!({"X":"rolled-back"})).unwrap();
    }
    assert_eq!(
        latest(database.connection(), a, AdapterKind::Pi).unwrap(),
        3
    );
    drop(database);
    let mut database = Database::open(&root).unwrap();
    let mut recovered = runtime(AdapterKind::Pi);
    freeze_with_host(database.connection(), &c, a, &mut recovered, || {
        panic!("cold recovery must restore its saved configuration")
    })
    .unwrap();
    let env = recovered.environment.as_mut().unwrap();
    env.hydrate(database.connection()).unwrap();
    assert_eq!(
        env.values.as_ref().unwrap()["ANTHROPIC_API_KEY"],
        "fixture-host-a"
    );
    assert_eq!(
        env.values.as_ref().unwrap()["ANTHROPIC_BASE_URL"],
        "https://a.example"
    );
    assert!(
        !env.redactor()
            .text("failure: fixture-host-a")
            .contains("fixture-host-a")
    );
    // Editor masking stays conservative; output scrubbing only treats credential
    // names as secrets, even when a real credential is a short literal.
    let mut display_env = env.clone();
    let values = display_env.values.as_mut().unwrap();
    for (name, value) in [
        ("AUTH_ENABLED", "1"),
        ("TOKEN_LIMIT", "2048"),
        ("KEY_PATH", "/tmp/keys"),
        ("AUTH_MODE", "bearer"),
        ("XAI_API_KEY", "x"),
    ] {
        values.insert(name.into(), value.into());
    }
    let redactor = display_env.redactor();
    let original = json!({"sessionId":"session-1", "flag":"1", "limit":"2048",
        "path":"/tmp/keys", "mode":"bearer", "secret":"x", "known":"fixture-host-a"});
    let displayed = redactor.redacted_value(&original);
    for field in ["sessionId", "flag", "limit", "path", "mode"] {
        assert_eq!(displayed[field], original[field]);
    }
    assert_eq!(displayed["secret"], "[redacted]");
    assert_eq!(displayed["known"], "[redacted]");
    assert_eq!(original["secret"], "x");
    assert!(sensitive("AUTH_ENABLED"));
    assert!(!format!("{recovered:?}").contains("fixture-host-a"));
    // Same member in a new Camp gets current values, independently from its old Camp.
    let mut fresh = runtime(AdapterKind::Pi);
    freeze_with_host(database.connection(), &other_camp, a, &mut fresh, || {
        Environment::new()
    })
    .unwrap();
    fresh
        .environment
        .as_mut()
        .unwrap()
        .hydrate(database.connection())
        .unwrap();
    assert_eq!(
        fresh.environment.as_ref().unwrap().values.as_ref().unwrap()["ANTHROPIC_BASE_URL"],
        "https://c.example"
    );
    assert_ne!(
        fresh.environment.as_ref().unwrap().plan_id,
        admitted.environment.as_ref().unwrap().plan_id
    );
    let mut another = runtime(AdapterKind::Pi);
    freeze_with_host(database.connection(), &other, b, &mut another, || {
        Environment::from([("ANTHROPIC_API_KEY".into(), "fixture-host-b".into())])
    })
    .unwrap();
    assert!(freeze(database.connection(), &c, b, &mut another).is_err());
    // Existing owner-only restart also clears an environment-only session, before native binding.
    let envelope = crate::command::CommandEnvelope {
        command_id: "reset-environment-session".into(),
        actor: crate::command::ActorRef::User {
            user_id: "local_user".into(),
        },
        camp_id: Some("camp-a".into()),
        expected_versions: vec![],
        execution_epoch: None,
        payload: crate::runtime::RestartNativeSessionCommand {
            conversation_id: c.clone(),
            expected_version: 1,
        },
    };
    let reset = crate::runtime::ExecutionRuntimeService::default()
        .restart_native_session(&mut database, &envelope)
        .unwrap();
    assert_eq!(
        reset.result.status,
        crate::command::CommandResultStatus::Applied
    );
    assert!(
        crate::runtime::ExecutionRuntimeService::default()
            .restart_native_session(&mut database, &envelope)
            .unwrap()
            .replayed
    );
    save(database.connection(), a, 3, json!({})).unwrap();
    let mut reset_session_runtime = runtime(AdapterKind::Pi);
    freeze_with_host(
        database.connection(),
        &c,
        a,
        &mut reset_session_runtime,
        || Environment::from([("ANTHROPIC_API_KEY".into(), "fixture-host-new".into())]),
    )
    .unwrap();
    let updated = reset_session_runtime.environment.as_mut().unwrap();
    updated.hydrate(database.connection()).unwrap();
    assert_eq!(
        updated.values.as_ref().unwrap()["ANTHROPIC_API_KEY"],
        "fixture-host-new"
    );
    assert_ne!(
        updated.plan_id,
        admitted.environment.as_ref().unwrap().plan_id
    );
    let mut retry: FrozenAgentRuntimeConfig = serde_json::from_str(&snapshot).unwrap();
    retry
        .environment
        .as_mut()
        .unwrap()
        .hydrate(database.connection())
        .unwrap();
    assert_eq!(
        retry.environment.as_ref().unwrap().values.as_ref().unwrap()["ANTHROPIC_API_KEY"],
        "fixture-host-a"
    );
    // Corrupt/missing storage blocks reuse; it must not produce a new plan.
    database
        .connection()
        .execute(
            "UPDATE runtime_environment_plan SET ciphertext=x'00' WHERE id=?1",
            [&updated.plan_id],
        )
        .unwrap();
    assert!(freeze(database.connection(), &c, a, &mut runtime(AdapterKind::Pi)).is_err());
    assert_eq!(
        get(database.connection(), a, AdapterKind::ClaudeCodeCli, false).unwrap()["revision"],
        0
    );
    std::fs::remove_file(root.join("member-environment/key")).unwrap();
    assert!(
        retry
            .environment
            .as_mut()
            .unwrap()
            .hydrate(database.connection())
            .is_err()
    );
    drop(database);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn parallel_child_processes_get_literal_frozen_values_without_changing_parent() {
    let (database, root) = database();
    let original = std::env::var_os("ANTHROPIC_API_KEY");
    let literal = "space ' quote \" $HOME ${HOME} ~ $(touch forbidden)\nnext";
    let mut plans = Vec::new();
    for (member, key) in [
        (LUOKE_AGENT_ID, "fixture-key-a"),
        (MUWA_AGENT_ID, "fixture-key-b"),
    ] {
        let mut values = json!({"ANTHROPIC_API_KEY":key,"LITERAL":literal,"PATH":"/member/path/that/does/not/select/the/cli"});
        if member == LUOKE_AGENT_ID {
            values["MEMBER_A_ONLY"] = json!("private-a");
        }
        save(database.connection(), member, 0, values).unwrap();
        let mut frozen = runtime(AdapterKind::Pi);
        freeze(
            database.connection(),
            &conversation(database.connection(), "parallel", member),
            member,
            &mut frozen,
        )
        .unwrap();
        let plan = frozen.environment.as_mut().unwrap();
        plan.hydrate(database.connection()).unwrap();
        plans.push(plan.clone());
    }
    crate::runtime_discovery::with_frozen_environment(Some(plans[0].clone()), async {
        let mut other = runtime(AdapterKind::Pi);
        freeze(
            database.connection(),
            &conversation(database.connection(), "parallel", MUWA_AGENT_ID),
            MUWA_AGENT_ID,
            &mut other,
        )
        .unwrap();
        let other = other.environment.as_mut().unwrap();
        other.hydrate(database.connection()).unwrap();
        assert_eq!(other.plan_id, plans[1].plan_id);
        assert!(
            !other.values.as_ref().unwrap().contains_key("MEMBER_A_ONLY"),
            "admission cannot inherit a different Run's task-local environment"
        );
    })
    .await;
    let child = |plan: FrozenEnvironment| async move {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args([
            "-c",
            "printf '%s\\0%s\\0%s' \"$ANTHROPIC_API_KEY\" \"$LITERAL\" \"$PATH\"",
        ]);
        plan.apply(&mut command).unwrap();
        use crate::managed_process::{
            ManagedProcess, ManagedProcessLaunchSpec, ManagedProcessPurpose, ManagedStdinPolicy,
            ManagedWindowsArgvDialect,
        };
        let spec = ManagedProcessLaunchSpec::capture(
            &command,
            ManagedProcessPurpose::RuntimeHost,
            ManagedStdinPolicy::Piped,
            ManagedWindowsArgvDialect::MicrosoftCrt,
            "member-environment-fixture",
        )
        .unwrap();
        let mut child = ManagedProcess::spawn(spec).unwrap();
        let mut reader = child.take_stdout().unwrap();
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut bytes)
            .await
            .unwrap();
        let status = child.wait().await.unwrap();
        child.force_terminate_tree().unwrap();
        (status, bytes)
    };
    let (a, b) = tokio::join!(child(plans[0].clone()), child(plans[1].clone()));
    for (output, key) in [(a, "fixture-key-a"), (b, "fixture-key-b")] {
        assert!(output.0.success());
        let text = String::from_utf8(output.1).unwrap();
        let fields = text.split('\0').collect::<Vec<_>>();
        assert_eq!(
            fields,
            [key, literal, "/member/path/that/does/not/select/the/cli"]
        );
    }
    // A new PID after saving/reopening still receives the old session values;
    // only an explicit session reset permits the replacement configuration.
    save(database.connection(), LUOKE_AGENT_ID, 1,
        json!({"ANTHROPIC_API_KEY":"fixture-key-new","LITERAL":literal,"PATH":"/member/path/that/does/not/select/the/cli"})).unwrap();
    drop(database);
    let database = Database::open(&root).unwrap();
    let conversation = conversation(database.connection(), "parallel", LUOKE_AGENT_ID);
    for (reset, expected) in [(false, "fixture-key-a"), (true, "fixture-key-new")] {
        if reset {
            reset_session(database.connection(), &conversation).unwrap();
        }
        let mut frozen = runtime(AdapterKind::Pi);
        freeze(
            database.connection(),
            &conversation,
            LUOKE_AGENT_ID,
            &mut frozen,
        )
        .unwrap();
        let plan = frozen.environment.as_mut().unwrap();
        plan.hydrate(database.connection()).unwrap();
        let (status, bytes) = child(plan.clone()).await;
        assert!(status.success());
        assert_eq!(
            String::from_utf8(bytes).unwrap().split('\0').next(),
            Some(expected)
        );
    }
    assert_eq!(std::env::var_os("ANTHROPIC_API_KEY"), original);
    drop(database);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_archive_is_inert_read_only_and_acknowledgment_survives_path_edits() {
    let (database, root) = database();
    database.connection().execute("INSERT INTO runtime_startup_setting(runtime_kind,revision,configuration_json,updated_at) VALUES('claude-code-cli',1,?1,datetime('now'))",
        [json!({"programPath":null,"environment":[{"name":"ANTHROPIC_API_KEY","value":"fixture-old-key"},{"name":"HTTP_PROXY","value":"http://legacy.example"}]}).to_string()]).unwrap();
    // Exercise the actual upgrade: schema, archive, old source and receipt are atomic.
    database.connection().execute_batch("DROP TABLE conversation_runtime_environment; DROP TABLE member_runtime_environment;
        DROP TABLE runtime_environment_plan; DROP TABLE runtime_environment_legacy;
        DELETE FROM schema_migration WHERE version IN (191,192);
        UPDATE rovai_data_contract SET projection_schema_version=140 WHERE singleton=1;
        CREATE TRIGGER fail_member_environment_receipt BEFORE INSERT ON schema_migration WHEN NEW.version=191
        BEGIN SELECT RAISE(ABORT,'fixture environment receipt failure'); END;").unwrap();
    drop(database);
    assert!(Database::open(&root).is_err());
    let raw = Connection::open(root.join("rovai.sqlite")).unwrap();
    assert_eq!(
        raw.query_row(
            "SELECT projection_schema_version FROM rovai_data_contract",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        140
    );
    assert_eq!(
        raw.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='runtime_environment_legacy'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert!(raw.query_row("SELECT configuration_json FROM runtime_startup_setting WHERE runtime_kind='claude-code-cli'", [], |r| r.get::<_, String>(0)).unwrap().contains("fixture-old-key"));
    raw.execute_batch("DROP TRIGGER fail_member_environment_receipt;")
        .unwrap();
    drop(raw);
    let raw = Connection::open(root.join("rovai.sqlite")).unwrap();
    raw.execute_batch("CREATE TRIGGER fail_session_environment_receipt BEFORE INSERT ON schema_migration WHEN NEW.version=192
        BEGIN SELECT RAISE(ABORT,'fixture session receipt failure'); END;").unwrap();
    drop(raw);
    assert!(Database::open(&root).is_err());
    let raw = Connection::open(root.join("rovai.sqlite")).unwrap();
    assert_eq!(
        raw.query_row(
            "SELECT projection_schema_version FROM rovai_data_contract",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        141
    );
    assert_eq!(
        raw.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='conversation_runtime_environment'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        legacy_get(&raw, AdapterKind::ClaudeCodeCli, true).unwrap()["ANTHROPIC_API_KEY"],
        "fixture-old-key"
    );
    raw.execute_batch("DROP TRIGGER fail_session_environment_receipt;")
        .unwrap();
    drop(raw);
    let mut database = Database::open(&root).unwrap();
    let identity = legacy_list(database.connection()).unwrap()[0]["identity"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        legacy_get(database.connection(), AdapterKind::ClaudeCodeCli, false).unwrap()["ANTHROPIC_API_KEY"],
        SAVED
    );
    legacy_acknowledge(database.connection(), AdapterKind::ClaudeCodeCli, &identity).unwrap();
    assert!(
        crate::runtime_startup::load(&database, AdapterKind::ClaudeCodeCli)
            .unwrap()
            .configuration
            .environment
            .is_empty()
    );
    crate::runtime_startup::save(
        &mut database,
        AdapterKind::ClaudeCodeCli,
        1,
        RuntimeStartupConfiguration {
            program_path: Some(root.join("claude").to_string_lossy().into()),
            ..Default::default()
        },
        2,
    )
    .unwrap();
    assert_eq!(
        legacy_list(database.connection()).unwrap()[0]["handled"],
        true
    );
    assert_eq!(
        legacy_get(database.connection(), AdapterKind::ClaudeCodeCli, true).unwrap()["ANTHROPIC_API_KEY"],
        "fixture-old-key"
    );
    drop(database);
    let database = Database::open(&root).unwrap();
    assert_eq!(
        legacy_list(database.connection()).unwrap()[0]["handled"],
        true
    );
    assert!(
        legacy_acknowledge(database.connection(), AdapterKind::ClaudeCodeCli, "stale").is_err()
    );
    drop(database);
    std::fs::remove_dir_all(root).unwrap();
}
