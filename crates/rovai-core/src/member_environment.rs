//! Host-private environment versions. Only opaque, immutable plan references enter Runs.
use crate::{
    agent_profile::{AdapterKind, FrozenAgentRuntimeConfig, ModelSelection},
    platform::private_storage,
    runtime_startup::{RuntimeEnvironmentVariable, RuntimeStartupConfiguration},
};
use anyhow::{Context, Result, ensure};
use ring::aead;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{
    Deserialize, Serialize,
    de::{MapAccess, Visitor},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::PathBuf,
};

pub const SAVED: &str = "<saved>";
type Environment = BTreeMap<String, String>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentEdit {
    pub expected_revision: i64,
    pub json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentUpdateReceipt {
    pub expected_revision: i64,
}

pub struct PreparedEnvironment {
    pub receipt: EnvironmentUpdateReceipt,
    ciphertext: Vec<u8>,
    member_id: String,
    kind: AdapterKind,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenEnvironment {
    pub plan_id: String,
    pub revision: i64,
    // Read-only compatibility for snapshots written by the unreleased Run-scoped implementation.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "identity")]
    pub legacy_identity: Option<String>,
    #[serde(skip)]
    pub values: Option<Environment>,
    #[serde(skip)]
    pub overlay: Option<Environment>,
}
impl std::fmt::Debug for FrozenEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrozenEnvironment")
            .field("plan_id", &self.plan_id)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

fn key(connection: &Connection) -> Result<Vec<u8>> {
    let path = PathBuf::from(
        connection
            .path()
            .context("Private environment requires a Host database")?,
    );
    let root = path
        .parent()
        .context("Private environment storage is unavailable")?
        .join("member-environment");
    private_storage::prepare_private_directory(&root)?;
    let path = root.join("key");
    if !path.exists() {
        let has_values: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM member_runtime_environment)
            OR EXISTS(SELECT 1 FROM runtime_environment_plan) OR EXISTS(SELECT 1 FROM runtime_environment_legacy)", [], |r| r.get(0))?;
        ensure!(
            !has_values,
            "Private environment key is missing; restore the original Host key"
        );
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|_| anyhow::anyhow!("Private environment key generation failed"))?;
        if let Err(error) = private_storage::create_private_new_file(&path).and_then(|mut file| {
            file.write_all(&bytes)?;
            file.sync_all()?;
            Ok(())
        }) {
            if !path.exists() {
                return Err(error);
            }
        }
    }
    let mut bytes = Vec::new();
    private_storage::open_private_read_file(&path)?
        .take(33)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() == 32, "Private environment key is invalid");
    Ok(bytes)
}
fn encrypt(connection: &Connection, context: &str, value: &impl Serialize) -> Result<Vec<u8>> {
    let secret = aead::UnboundKey::new(&aead::AES_256_GCM, &key(connection)?)
        .map_err(|_| anyhow::anyhow!("Private environment encryption failed"))?;
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("Private environment nonce generation failed"))?;
    let mut bytes = serde_json::to_vec(value)?;
    aead::LessSafeKey::new(secret)
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(context.as_bytes()),
            &mut bytes,
        )
        .map_err(|_| anyhow::anyhow!("Private environment encryption failed"))?;
    Ok([nonce.as_slice(), bytes.as_slice()].concat())
}
fn decrypt<T: serde::de::DeserializeOwned>(
    connection: &Connection,
    context: &str,
    bytes: &[u8],
) -> Result<T> {
    ensure!(bytes.len() >= 28, "Private environment storage is damaged");
    let secret = aead::UnboundKey::new(&aead::AES_256_GCM, &key(connection)?)
        .map_err(|_| anyhow::anyhow!("Private environment decryption failed"))?;
    let mut buffer = bytes[12..].to_vec();
    let data = aead::LessSafeKey::new(secret)
        .open_in_place(
            aead::Nonce::try_assume_unique_for_key(&bytes[..12]).unwrap(),
            aead::Aad::from(context.as_bytes()),
            &mut buffer,
        )
        .map_err(|_| {
            anyhow::anyhow!("Private environment cannot be read; restore the original Host key")
        })?;
    serde_json::from_slice(data).context("Private environment storage is damaged")
}

pub fn sensitive(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    ["TOKEN", "KEY", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"]
        .iter()
        .any(|word| name.contains(word))
}

// Mask the editor conservatively, but only credential-bearing names contribute
// values to output scrubbing. AUTH_ENABLED, TOKEN_LIMIT and KEY_PATH are settings,
// not secrets; their values must not become global replacement patterns.
fn credential_variable(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "CREDENTIAL",
        "CREDENTIALS",
        "AUTHORIZATION",
    ]
    .iter()
    .any(|suffix| name == *suffix || name.ends_with(&format!("_{suffix}")))
        || name.ends_with("_ACCESS_KEY_ID")
}
fn context(member: &str, kind: AdapterKind, revision: i64) -> String {
    format!("member:{member}:{}:{revision}", kind.as_str())
}
fn latest(connection: &Connection, member: &str, kind: AdapterKind) -> Result<i64> {
    Ok(connection.query_row("SELECT COALESCE(MAX(revision),0) FROM member_runtime_environment WHERE member_id=?1 AND runtime_kind=?2",
        params![member,kind.as_str()], |r| r.get(0))?)
}
fn read_version(
    connection: &Connection,
    member: &str,
    kind: AdapterKind,
    revision: i64,
) -> Result<Environment> {
    if revision == 0 {
        return Ok(Environment::new());
    }
    let bytes: Vec<u8> = connection.query_row("SELECT ciphertext FROM member_runtime_environment WHERE member_id=?1 AND runtime_kind=?2 AND revision=?3",
        params![member,kind.as_str(),revision], |r| r.get(0)).context("Member environment revision is unavailable")?;
    decrypt(connection, &context(member, kind, revision), &bytes)
}
fn require_member(connection: &Connection, member: &str) -> Result<()> {
    ensure!(
        connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_profile WHERE id=?1 AND profile_status!='removed')",
            [member],
            |r| r.get::<_, bool>(0)
        )?,
        "Member is unavailable"
    );
    Ok(())
}
fn public_values(values: &Environment, reveal: bool) -> Environment {
    values
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                if !reveal && sensitive(name) && !value.is_empty() {
                    SAVED.into()
                } else {
                    value.clone()
                },
            )
        })
        .collect()
}
pub fn get(
    connection: &Connection,
    member: &str,
    kind: AdapterKind,
    reveal: bool,
) -> Result<Value> {
    require_member(connection, member)?;
    let revision = latest(connection, member, kind)?;
    let values = read_version(connection, member, kind, revision)?;
    Ok(json!({"revision":revision,"environment":public_values(&values,reveal)}))
}

/// Deserialize directly from text so duplicate JSON names cannot disappear in a map parser.
pub fn parse(text: &str, windows: bool) -> Result<Environment> {
    struct Unique;
    impl<'de> Visitor<'de> for Unique {
        type Value = Environment;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an object with unique environment names and string values")
        }
        fn visit_map<A: MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Environment, A::Error> {
            let mut result = Environment::new();
            while let Some((name, value)) = map.next_entry::<String, String>()? {
                if result.insert(name, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate environment name"));
                }
            }
            Ok(result)
        }
    }
    let mut reader =
        serde_json::Deserializer::from_str(if text.trim().is_empty() { "{}" } else { text });
    let values = serde::de::Deserializer::deserialize_map(&mut reader, Unique).map_err(|_| {
        anyhow::anyhow!("Environment must be a JSON object with unique names and string values")
    })?;
    reader
        .end()
        .map_err(|_| anyhow::anyhow!("Environment JSON is invalid"))?;
    RuntimeStartupConfiguration {
        environment: values
            .iter()
            .map(|(name, value)| RuntimeEnvironmentVariable {
                name: name.clone(),
                value: value.clone(),
            })
            .collect(),
        ..Default::default()
    }
    .validated(windows)?;
    for name in values.keys() {
        ensure!(
            name.trim() == name
                && ![
                    "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
                    "CLAUDE_CODE_DISABLE_AUTO_MEMORY",
                    "CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS",
                    "PI_TELEMETRY"
                ]
                .iter()
                .any(|n| n.eq_ignore_ascii_case(name)),
            "Environment contains a reserved name"
        );
    }
    Ok(if windows {
        values
            .into_iter()
            .map(|(name, value)| (name.to_ascii_uppercase(), value))
            .collect()
    } else {
        values
    })
}
pub fn prepare(
    connection: &Connection,
    member: &str,
    kind: AdapterKind,
    model: &ModelSelection,
    edit: EnvironmentEdit,
) -> Result<PreparedEnvironment> {
    require_member(connection, member)?;
    ensure!(
        edit.expected_revision >= 0 && edit.expected_revision < i64::MAX,
        "Invalid member environment revision"
    );
    let previous = read_version(connection, member, kind, edit.expected_revision)?;
    let mut values = parse(&edit.json, cfg!(windows))?;
    for (name, value) in &mut values {
        if value == SAVED {
            ensure!(
                sensitive(name),
                "The saved marker is only valid for an existing credential"
            );
            *value = previous
                .get(name)
                .context("The saved credential is unavailable")?
                .clone();
        }
    }
    // Preserve command replay/conflict detection without hashing environment values.
    // A committed revision is immutable, so an old command must carry the same private values.
    if latest(connection, member, kind)? > edit.expected_revision {
        ensure!(
            read_version(connection, member, kind, edit.expected_revision + 1)? == values,
            "Member environment changed; reload the saved configuration"
        );
    }
    validate_model(kind, model, &values)?;
    parse(&serde_json::to_string(&values)?, cfg!(windows))?;
    let receipt = EnvironmentUpdateReceipt {
        expected_revision: edit.expected_revision,
    };
    Ok(PreparedEnvironment {
        ciphertext: encrypt(
            connection,
            &context(member, kind, edit.expected_revision + 1),
            &values,
        )?,
        receipt,
        member_id: member.into(),
        kind,
    })
}
impl PreparedEnvironment {
    pub fn commit(&self, connection: &Connection) -> Result<()> {
        ensure!(
            latest(connection, &self.member_id, self.kind)? == self.receipt.expected_revision,
            "Member environment changed; reload the saved configuration"
        );
        connection.execute("INSERT INTO member_runtime_environment(member_id,runtime_kind,revision,ciphertext) VALUES(?1,?2,?3,?4)",
            params![self.member_id,self.kind.as_str(),self.receipt.expected_revision+1,self.ciphertext])?;
        Ok(())
    }
}

fn saved_session(
    connection: &Connection,
    conversation: &str,
    kind: AdapterKind,
) -> Result<Option<FrozenEnvironment>> {
    let saved: Option<(String, i64)> = connection.query_row(
        "SELECT plan_id, revision FROM conversation_runtime_environment WHERE conversation_id=?1 AND runtime_kind=?2",
        params![conversation, kind.as_str()], |r| Ok((r.get(0)?, r.get(1)?))
    ).optional()?;
    if let Some((plan_id, revision)) = saved {
        let mut environment = FrozenEnvironment {
            plan_id,
            revision,
            legacy_identity: None,
            values: None,
            overlay: None,
        };
        environment.hydrate(connection)?;
        return Ok(Some(environment));
    }
    // An existing native session must not silently adopt today's Host environment on upgrade.
    let has_native: bool = connection.query_row(
        "SELECT native_session_id IS NOT NULL FROM conversation WHERE id=?1",
        [conversation],
        |r| r.get(0),
    )?;
    if has_native {
        let raw: Option<String> = connection.query_row(
            "SELECT json_extract(effective_config_json,'$.runtime.environment') FROM agent_run
             WHERE conversation_id=?1 AND runtime_adapter_kind=?2 ORDER BY created_at DESC, id DESC LIMIT 1",
            params![conversation, kind.as_str()], |r| r.get(0)).optional()?.flatten();
        let mut environment: FrozenEnvironment = serde_json::from_str(raw.as_deref().context(
            "The session environment is unavailable; reset this member's session to use the current configuration")?)?;
        environment.hydrate(connection)?;
        return Ok(Some(environment));
    }
    Ok(None)
}

pub(crate) fn session_values(
    connection: &Connection,
    conversation: &str,
    kind: AdapterKind,
) -> Result<Option<Environment>> {
    Ok(saved_session(connection, conversation, kind)?.and_then(|environment| environment.values))
}

pub fn session_has_values(
    connection: &Connection,
    conversation: &str,
    member: &str,
    kind: AdapterKind,
) -> Result<bool> {
    if let Some(environment) = saved_session(connection, conversation, kind)? {
        return Ok(environment.overlay.as_ref().is_some_and(|v| !v.is_empty()));
    }
    Ok(!read_version(connection, member, kind, latest(connection, member, kind)?)?.is_empty())
}

pub fn reset_session(connection: &Connection, conversation: &str) -> Result<usize> {
    Ok(connection.execute(
        "DELETE FROM conversation_runtime_environment WHERE conversation_id=?1",
        [conversation],
    )?)
}

fn validate_model(kind: AdapterKind, model: &ModelSelection, values: &Environment) -> Result<()> {
    ensure!(
        kind != AdapterKind::ClaudeCodeCli
            || !matches!(model, ModelSelection::Explicit { .. })
            || !values.keys().any(|name| if cfg!(windows) {
                name.eq_ignore_ascii_case("ANTHROPIC_MODEL")
            } else {
                name == "ANTHROPIC_MODEL"
            }),
        "Remove ANTHROPIC_MODEL or select the Runtime default model"
    );
    Ok(())
}

pub fn validate_saved_model(
    connection: &Connection,
    member: &str,
    kind: AdapterKind,
    model: &ModelSelection,
) -> Result<()> {
    if kind == AdapterKind::ClaudeCodeCli && matches!(model, ModelSelection::Explicit { .. }) {
        validate_model(
            kind,
            model,
            &read_version(connection, member, kind, latest(connection, member, kind)?)?,
        )?;
    }
    Ok(())
}

pub fn freeze(
    connection: &Connection,
    conversation: &str,
    member: &str,
    runtime: &mut FrozenAgentRuntimeConfig,
) -> Result<()> {
    freeze_with_host(
        connection,
        conversation,
        member,
        runtime,
        crate::runtime_discovery::capture_host_runtime_environment,
    )
}

fn freeze_with_host(
    connection: &Connection,
    conversation: &str,
    member: &str,
    runtime: &mut FrozenAgentRuntimeConfig,
    capture: impl FnOnce() -> Environment,
) -> Result<()> {
    ensure!(
        connection.query_row(
            "SELECT agent_id=?2 FROM conversation WHERE id=?1",
            params![conversation, member],
            |r| r.get::<_, bool>(0)
        )?,
        "Session environment belongs to another member"
    );
    let environment = match saved_session(connection, conversation, runtime.adapter_kind)? {
        Some(environment) => environment,
        None => {
            let revision = latest(connection, member, runtime.adapter_kind)?;
            let overlay = read_version(connection, member, runtime.adapter_kind, revision)?;
            let mut values = capture();
            for (name, value) in &overlay {
                if cfg!(windows) {
                    values.retain(|key, _| !key.eq_ignore_ascii_case(name));
                }
                values.insert(name.clone(), value.clone());
            }
            if runtime.adapter_kind == AdapterKind::ClaudeCodeCli
                && claude_routing_override(&overlay)
            {
                values.insert("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST".into(), "1".into());
            }
            // Random snapshot reference, not an environment/credential digest. The old identity
            // column remains the encryption context for compatibility with encrypted Run plans.
            let plan_id = uuid::Uuid::new_v4().to_string();
            let ciphertext = encrypt(
                connection,
                &plan_id,
                &json!({"values":values,"overlay":overlay}),
            )?;
            connection.execute(
                "INSERT INTO runtime_environment_plan(id,identity,ciphertext) VALUES(?1,?1,?2)",
                params![plan_id, ciphertext],
            )?;
            FrozenEnvironment {
                plan_id,
                revision,
                legacy_identity: None,
                values: Some(values),
                overlay: Some(overlay),
            }
        }
    };
    connection.execute("INSERT OR IGNORE INTO conversation_runtime_environment(conversation_id,runtime_kind,plan_id,revision) VALUES(?1,?2,?3,?4)",
        params![conversation,runtime.adapter_kind.as_str(),environment.plan_id,environment.revision])?;
    if environment
        .overlay
        .as_ref()
        .is_some_and(|values| !values.is_empty())
    {
        runtime.custom_api = None;
    }
    runtime.environment = Some(FrozenEnvironment {
        values: None,
        overlay: None,
        ..environment
    });
    runtime.refresh_config_digest()
}
pub(crate) fn configure_values(command: &mut tokio::process::Command, values: &Environment) {
    // ManagedProcess captures explicit env/remove operations; stable Command APIs
    // do not expose env_clear. Preserve that boundary for both pipes and PTYs.
    for (key, _) in std::env::vars_os() {
        let present = key.to_str().is_some_and(|name| {
            values.keys().any(|candidate| {
                if cfg!(windows) {
                    candidate.eq_ignore_ascii_case(name)
                } else {
                    candidate == name
                }
            })
        });
        if !present {
            command.env_remove(key);
        }
    }
    command.envs(values);
}

fn claude_routing_override(overlay: &Environment) -> bool {
    overlay.keys().any(|name| {
        matches!(
            name.as_str(),
            "ANTHROPIC_BASE_URL"
                | "ANTHROPIC_API_KEY"
                | "ANTHROPIC_AUTH_TOKEN"
                | "CLAUDE_CODE_OAUTH_TOKEN"
        ) || name.starts_with("CLAUDE_CODE_USE_")
    })
}

impl FrozenEnvironment {
    pub fn validate_claude_routing(&self, version: Option<&str>) -> Result<()> {
        if !self.overlay.as_ref().is_some_and(claude_routing_override) {
            return Ok(());
        }
        // Compatibility acceptance: actual parallel requests with conflicting native settings,
        // Claude Code 2.1.287. Older/unknown releases do not silently use native credentials.
        let version = version
            .unwrap_or_default()
            .split(|c: char| !c.is_ascii_digit() && c != '.')
            .find_map(|part| {
                let numbers = part
                    .split('.')
                    .map(str::parse::<u32>)
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .ok()?;
                (numbers.len() == 3).then(|| (numbers[0], numbers[1], numbers[2]))
            });
        ensure!(
            version.is_some_and(|v| v >= (2, 1, 287) && v.0 == 2),
            "Member provider routing requires Claude Code 2.1.287 or a newer compatible 2.x release"
        );
        Ok(())
    }

    pub fn hydrate(&mut self, connection: &Connection) -> Result<()> {
        let (plan_identity,bytes):(String,Vec<u8>)=connection.query_row("SELECT identity,ciphertext FROM runtime_environment_plan WHERE id=?1",params![self.plan_id],|r|Ok((r.get(0)?,r.get(1)?)))
            .context("Frozen member environment is unavailable; the Run cannot use another configuration")?;
        #[derive(Deserialize)]
        struct Plan {
            values: Environment,
            overlay: Environment,
        }
        let plan: Plan = decrypt(connection, &plan_identity, &bytes)?;
        self.values = Some(plan.values);
        self.overlay = Some(plan.overlay);
        Ok(())
    }
    pub fn apply(&self, command: &mut tokio::process::Command) -> Result<()> {
        configure_values(
            command,
            self.values
                .as_ref()
                .context("Frozen environment was not resolved")?,
        );
        Ok(())
    }
    pub fn redactor(&self) -> crate::runtime_custom_api::CredentialRedactor {
        crate::runtime_custom_api::CredentialRedactor::from_values(
            self.values
                .iter()
                .flat_map(|values| values.iter())
                .filter(|(name, _)| credential_variable(name))
                .map(|(_, value)| value.clone())
                .collect(),
        )
    }
}

pub(crate) fn archive_legacy(connection: &Connection) -> Result<()> {
    let mut statement = connection
        .prepare("SELECT runtime_kind,configuration_json FROM runtime_startup_setting")?;
    let rows = statement
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (kind, raw) in rows {
        let mut config: Value = serde_json::from_str(&raw)?;
        let entries: Vec<RuntimeEnvironmentVariable> =
            serde_json::from_value(config.get("environment").cloned().unwrap_or(json!([])))?;
        if entries.is_empty() {
            continue;
        }
        let values = entries
            .into_iter()
            .map(|row| (row.name, row.value))
            .collect::<Environment>();
        let previous: Option<(String, Vec<u8>)> = connection
            .query_row(
                "SELECT identity,ciphertext FROM runtime_environment_legacy WHERE runtime_kind=?1",
                [&kind],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let identity = match previous {
            Some((id, bytes))
                if decrypt::<Environment>(connection, &format!("legacy:{kind}"), &bytes)?
                    == values =>
            {
                id
            }
            _ => uuid::Uuid::new_v4().to_string(),
        };
        let encrypted = encrypt(connection, &format!("legacy:{kind}"), &values)?;
        connection.execute("INSERT INTO runtime_environment_legacy(runtime_kind,identity,ciphertext,variable_count) VALUES(?1,?2,?3,?4)
            ON CONFLICT(runtime_kind) DO UPDATE SET identity=excluded.identity,ciphertext=excluded.ciphertext,variable_count=excluded.variable_count",params![kind,identity,encrypted,values.len() as i64])?;
        config["environment"] = json!([]);
        connection.execute(
            "UPDATE runtime_startup_setting SET configuration_json=?2 WHERE runtime_kind=?1",
            params![kind, serde_json::to_string(&config)?],
        )?;
    }
    Ok(())
}
pub fn legacy_list(connection: &Connection) -> Result<Value> {
    let mut query=connection.prepare("SELECT runtime_kind,identity,variable_count,identity=COALESCE(acknowledged_identity,'') FROM runtime_environment_legacy ORDER BY runtime_kind")?;
    let rows=query.query_map([],|r|Ok(json!({"runtimeKind":r.get::<_,String>(0)?,"identity":r.get::<_,String>(1)?,"count":r.get::<_,i64>(2)?,"handled":r.get::<_,bool>(3)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!(rows))
}
pub fn legacy_get(connection: &Connection, kind: AdapterKind, reveal: bool) -> Result<Value> {
    let bytes: Vec<u8> = connection.query_row(
        "SELECT ciphertext FROM runtime_environment_legacy WHERE runtime_kind=?1",
        [kind.as_str()],
        |r| r.get(0),
    )?;
    let values: Environment = decrypt(connection, &format!("legacy:{}", kind.as_str()), &bytes)?;
    Ok(json!(public_values(&values, reveal)))
}
pub fn legacy_acknowledge(
    connection: &Connection,
    kind: AdapterKind,
    expected: &str,
) -> Result<()> {
    ensure!(connection.execute("UPDATE runtime_environment_legacy SET acknowledged_identity=identity WHERE runtime_kind=?1 AND identity=?2",params![kind.as_str(),expected])?==1,"Legacy environment changed; refresh diagnostics");
    Ok(())
}

#[cfg(all(test, feature = "extended-tests"))]
mod tests;
