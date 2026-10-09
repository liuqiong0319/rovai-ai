use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    agent_profile::{
        AgentProfileService, CreateAgentProfileCommand, MemberCapabilityClaim,
        validate_member_identity_input,
    },
    command::{
        ActorRef, CommandEnvelope, CommandExecution, CommandGatewayError, CommandResultStatus,
    },
    current_user::CURRENT_USER_ID,
    db::Database,
    member_avatar::{
        MemberAvatarImportError, MemberAvatarImportErrorKind, hash_selected_avatar,
        import_managed_member_avatar, remove_managed_member_avatar, validate_avatar_file,
    },
    runtime::resolve_agent_local_path,
    team_tool::{AuthenticatedTeamToolRun, TeamToolService},
};

pub const MEMBER_CREATE_TOOL_NAME: &str = "member.create";
pub const MEMBER_CREATE_PREVIEW_TOOL_NAME: &str = "member.create.preview";
pub const MEMBER_CREATE_CONFIRM_TOOL_NAME: &str = "member.create.confirm";

/// Confirmation credentials issued by a preview expire after 24 hours.
const CONFIRMATION_TTL_SECONDS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemberCreateDraft {
    pub creation_key: String,
    pub display_name: String,
    #[serde(default)]
    pub team_role: String,
    #[serde(default)]
    pub professional_responsibilities: String,
    #[serde(default)]
    pub personality_traits: Vec<String>,
    #[serde(default)]
    pub working_principles: String,
    #[serde(default)]
    pub growth_topic: String,
    #[serde(default)]
    pub avatar_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberCreateInput {
    /// Opaque server-side handle returned by member.create.preview. The
    /// confirmation token never leaves the server; create claims the confirmed
    /// capability by previewId + the authenticated run.
    pub preview_id: String,
    #[serde(flatten)]
    pub draft: MemberCreateDraft,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemberCreateConfirmInput {
    pub preview_id: String,
    pub card_digest: String,
}

#[derive(Debug)]
pub struct MemberCreateOutcome {
    pub execution: CommandExecution,
    pub avatar_ref: Option<String>,
}

#[derive(Debug)]
pub struct MemberCreatePreviewOutcome {
    pub preview_id: String,
    pub card: Value,
    pub preview_expires_at: String,
}

#[derive(Debug)]
pub struct MemberCreateConfirmOutcome {
    pub credential_id: String,
    pub token_expires_at: String,
}

#[derive(Debug)]
pub struct MemberOperationError {
    pub code: &'static str,
    pub message: &'static str,
    pub details: Option<Value>,
}

impl std::fmt::Display for MemberOperationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for MemberOperationError {}

pub fn member_create_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["creationKey", "displayName", "previewId"],
        "properties": {
            "previewId": {
                "type": "string",
                "format": "uuid",
                "description": "Opaque handle from member.create.preview. The confirmed capability is claimed server-side by this previewId plus the authenticated run; the confirmation token never leaves the server."
            },
            "creationKey": {
                "type": "string",
                "format": "uuid",
                "description": "A new canonical lowercase UUID. Reuse it only when retrying this exact confirmed member creation."
            },
            "displayName": {"type": "string", "minLength": 1, "maxLength": 80},
            "teamRole": {"type": "string", "maxLength": 120, "default": ""},
            "professionalResponsibilities": {"type": "string", "maxLength": 300, "default": ""},
            "personalityTraits": {
                "type": "array", "maxItems": 6, "default": [],
                "items": {"type": "string", "minLength": 1, "maxLength": 16}
            },
            "workingPrinciples": {"type": "string", "maxLength": 300, "default": ""},
            "growthTopic": {"type": "string", "maxLength": 300, "default": ""},
            "avatarFile": {
                "type": "string", "minLength": 1, "maxLength": 4096,
                "description": "Optional run-readable local PNG or JPEG path. Rovai normalizes and imports it; the path is never persisted."
            }
        }
    })
}

pub fn member_create_preview_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["creationKey", "displayName"],
        "properties": {
            "creationKey": {
                "type": "string",
                "format": "uuid",
                "description": "A new canonical lowercase UUID. Reuse it only when retrying this exact confirmed member creation."
            },
            "displayName": {"type": "string", "minLength": 1, "maxLength": 80},
            "teamRole": {"type": "string", "maxLength": 120, "default": ""},
            "professionalResponsibilities": {"type": "string", "maxLength": 300, "default": ""},
            "personalityTraits": {
                "type": "array", "maxItems": 6, "default": [],
                "items": {"type": "string", "minLength": 1, "maxLength": 16}
            },
            "workingPrinciples": {"type": "string", "maxLength": 300, "default": ""},
            "growthTopic": {"type": "string", "maxLength": 300, "default": ""},
            "avatarFile": {
                "type": "string", "minLength": 1, "maxLength": 4096,
                "description": "Optional run-readable local PNG or JPEG path; only validated during preview, never published."
            }
        }
    })
}

pub fn member_create_confirm_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["previewId", "cardDigest"],
        "properties": {
            "previewId": {"type": "string", "format": "uuid"},
            "cardDigest": {"type": "string", "minLength": 16}
        }
    })
}

/// Called by Core after authenticating the Run, before create/update import.
pub fn resolve_member_avatar_input(
    database: &Database,
    run: &AuthenticatedTeamToolRun,
    operation: &str,
    input: &mut Value,
) -> Result<()> {
    if !matches!(
        operation,
        MEMBER_CREATE_TOOL_NAME | crate::member_tool::MEMBER_UPDATE_TOOL_NAME
    ) {
        return Ok(());
    }
    let Some(Value::String(path)) = input.get_mut("avatarFile") else {
        return Ok(());
    };
    let (_, workspace) = TeamToolService::default()
        .agent_file_ingress_scope(database, &run.agent_run_id, run.execution_epoch)?
        .context("AgentRun file ingress is unavailable")?;
    *path = resolve_agent_local_path(Path::new(path), workspace.path())
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("AgentRun avatar path must be UTF-8"))?;
    Ok(())
}

/// Stage A: validate a draft card without persisting any business entity,
/// persist an immutable server-side preview record, and return the opaque
/// previewId handle. The confirmation credential is only issued by confirm.
pub fn preview_member_create(
    database: &mut Database,
    data_dir: &Path,
    authenticated_run: &AuthenticatedTeamToolRun,
    input: MemberCreateDraft,
) -> Result<MemberCreatePreviewOutcome> {
    require_direct_user_trigger(database, authenticated_run)?;
    Uuid::parse_str(&input.creation_key)
        .ok()
        .filter(|parsed| parsed.hyphenated().to_string() == input.creation_key)
        .ok_or_else(invalid_creation_key)?;
    validate_member_identity_input(
        &input.display_name,
        &input.team_role,
        &input.professional_responsibilities,
        &input.personality_traits,
        &input.working_principles,
        &input.growth_topic,
    )
    .map_err(|_| MemberOperationError {
        code: "member.invalid_identity",
        message: "One or more member identity fields are invalid; fix the confirmed card and try again",
        details: None,
    })?;
    if AgentProfileService::default().display_name_exists(database, &input.display_name)? {
        return Err(MemberOperationError {
            code: "agent_profile.display_name_conflict",
            message: "A member with this display name already exists; fix the card and run member.create.preview again",
            details: None,
        }
        .into());
    }

    // Validate-only: no asset is published during preview.
    let avatar_sha = input
        .avatar_file
        .as_deref()
        .map(Path::new)
        .map(|path| {
            validate_avatar_file(path)?;
            hash_selected_avatar(path)
        })
        .transpose()
        .map_err(map_avatar_error)?;
    let digest = member_card_digest(&input, avatar_sha.as_ref())?;

    let preview_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let issued_at = now.to_rfc3339();
    let expires_at = (now + chrono::Duration::seconds(CONFIRMATION_TTL_SECONDS)).to_rfc3339();
    let user_id = CURRENT_USER_ID.to_string();
    // Trusted immutable source binding: from the authenticated run for camp
    // scope; never accepted from the request body.
    let source_run_id = authenticated_run.agent_run_id.to_string();
    database.connection_mut().execute(
        r#"
        INSERT INTO member_create_preview(
            preview_id, creation_key, user_id, scope_kind, scope_id,
            card_digest, avatar_sha256, identity_json, status,
            superseded_by, confirmation_event_id, source_run_id,
            issued_at, expires_at,
            confirmed_at, consumed_at, superseded_at, cancelled_at
        ) VALUES (?1, ?2, ?3, 'camp', ?4, ?5, ?6, ?7, 'active',
                  NULL, NULL, ?8, ?9, ?10,
                  NULL, NULL, NULL, NULL)
        ON CONFLICT(creation_key) DO UPDATE SET
            preview_id = excluded.preview_id,
            user_id = excluded.user_id,
            scope_kind = excluded.scope_kind,
            scope_id = excluded.scope_id,
            card_digest = excluded.card_digest,
            avatar_sha256 = excluded.avatar_sha256,
            identity_json = excluded.identity_json,
            status = 'active',
            superseded_by = NULL,
            confirmation_event_id = NULL,
            source_run_id = excluded.source_run_id,
            issued_at = excluded.issued_at,
            expires_at = excluded.expires_at,
            confirmed_at = NULL,
            consumed_at = NULL,
            superseded_at = NULL,
            cancelled_at = NULL
        "#,
        params![
            preview_id,
            input.creation_key,
            user_id,
            authenticated_run.camp_id,
            digest,
            avatar_sha,
            serde_json::to_string(&member_card_json(&input, avatar_sha.clone(), &digest))?,
            source_run_id,
            issued_at,
            expires_at,
        ],
    )?;
    // Supersede any prior active/confirmed capability for this key+user+scope.
    database.connection_mut().execute(
        r#"
        UPDATE member_create_capability
        SET status='revoked', revoked_at=?3, invalidated_reason='superseded_by_new_preview'
        WHERE creation_key=?1 AND user_id=?2 AND status='issued'
        "#,
        params![input.creation_key, user_id, now.to_rfc3339()],
    )?;

    Ok(MemberCreatePreviewOutcome {
        preview_id,
        card: member_card_json(&input, avatar_sha, &digest),
        preview_expires_at: expires_at,
    })
}

/// Stage B: trusted User-only confirmation. Records a structured confirm event
/// and atomically confirms the preview (one confirm per preview), then issues
/// the one-time capability bound to the preview's trusted source run.
pub fn confirm_member_create(
    database: &mut Database,
    data_dir: &Path,
    authenticated_run: &AuthenticatedTeamToolRun,
    input: MemberCreateConfirmInput,
) -> Result<MemberCreateConfirmOutcome> {
    require_direct_user_trigger(database, authenticated_run)?;
    let now = chrono::Utc::now();
    let now_str = now.to_rfc3339();
    let event_id = Uuid::new_v4().to_string();
    let user_id = CURRENT_USER_ID.to_string();

    // Atomically confirm the preview (prevents replay/double-confirm).
    let confirmed = database.connection_mut().execute(
        r#"
        UPDATE member_create_preview
        SET status='confirmed', confirmed_at=?2, confirmation_event_id=?3
        WHERE preview_id=?1 AND status='active' AND confirmed_at IS NULL
        "#,
        params![input.preview_id, now_str, event_id],
    )?;
    if confirmed != 1 {
        return Err(MemberOperationError {
            code: "member.confirmation_invalid",
            message: "Preview is not in an active state that can be confirmed",
            details: None,
        }
        .into());
    }

    // Insert the structured confirmation event (event_id PK blocks replay).
    let preview = database
        .connection()
        .query_row(
            r#"
            SELECT user_id, scope_kind, scope_id, card_digest, source_run_id,
                   creation_key, expires_at
            FROM member_create_preview WHERE preview_id = ?1
            "#,
            params![input.preview_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((
        owner,
        scope_kind,
        scope_id,
        card_digest,
        source_run_id,
        creation_key,
        preview_expires,
    )) = preview
    else {
        return Err(MemberOperationError {
            code: "member.confirmation_invalid",
            message: "Preview record disappeared",
            details: None,
        }
        .into());
    };
    if owner != user_id || card_digest != input.card_digest {
        return Err(MemberOperationError {
            code: "member.card_mismatch",
            message: "Member card changed after preview; run member.create.preview again",
            details: None,
        }
        .into());
    }
    let token = format!("{}{}", Uuid::new_v4(), Uuid::new_v4());
    let token_hash = sha256_hex(token.as_bytes());
    let credential_id = Uuid::new_v4().to_string();
    let affected = database.connection_mut().execute(
        r#"
        INSERT INTO member_confirmation_event(
            event_id, message_id, preview_id, action, user_id, camp_id, created_at, event_json
        ) VALUES (?1, NULL, ?2, 'confirm', ?3, ?4, ?5, ?6)
        "#,
        params![
            event_id,
            input.preview_id,
            user_id,
            scope_id,
            now_str,
            json!({"schemaVersion": 1, "action": "confirm"}).to_string(),
        ],
    )?;
    if affected != 1 {
        return Err(MemberOperationError {
            code: "member.confirmation_already_consumed",
            message: "This preview was already confirmed",
            details: None,
        }
        .into());
    }
    // Issue the one-time capability, deriving authorized_run_id from the
    // trusted preview source_run_id via a JOIN (no client-asserted values).
    let issued = database.connection_mut().execute(
        r#"
        INSERT INTO member_create_capability(
            credential_id, preview_id, creation_key, user_id, scope_kind, scope_id,
            card_digest, token_hash, confirmation_event_id, authorized_run_id,
            status, invalidated_reason, issued_at, expires_at, consumed_at, revoked_at
        )
        SELECT ?1, p.preview_id, p.creation_key, p.user_id, p.scope_kind, p.scope_id,
               p.card_digest, ?2, e.event_id, p.source_run_id,
               'issued', NULL, ?3, p.expires_at, NULL, NULL
        FROM member_create_preview p
        JOIN member_confirmation_event e
          ON e.preview_id = p.preview_id AND e.action = 'confirm'
        WHERE p.preview_id = ?4 AND p.status = 'confirmed'
          AND e.user_id = p.user_id
          AND (e.camp_id IS p.scope_id OR (e.camp_id IS NULL AND p.scope_kind = 'global_ui'))
          AND ?3 <= p.expires_at
        "#,
        params![credential_id, token_hash, now_str, input.preview_id],
    )?;
    if issued != 1 {
        return Err(MemberOperationError {
            code: "member.capability_issuance_invalid",
            message: "Confirmed capability could not be issued for this preview",
            details: None,
        }
        .into());
    }
    let _ = data_dir;
    let _ = scope_kind;
    let _ = preview_expires;
    let _ = source_run_id;
    Ok(MemberCreateConfirmOutcome {
        credential_id,
        token_expires_at: preview_expires,
    })
}

/// Stage C: create consumes the confirmed capability. The capability is
/// claimed atomically by previewId + the authenticated run's authorized_run_id
/// + scope; the confirmation token never leaves the server.
pub fn create_member(
    database: &mut Database,
    data_dir: &Path,
    authenticated_run: &AuthenticatedTeamToolRun,
    input: MemberCreateInput,
) -> Result<MemberCreateOutcome> {
    require_direct_user_trigger(database, authenticated_run)?;
    let creation_id = Uuid::parse_str(&input.draft.creation_key)
        .ok()
        .filter(|parsed| parsed.hyphenated().to_string() == input.draft.creation_key)
        .ok_or_else(invalid_creation_key)?;
    validate_member_identity_input(
        &input.draft.display_name,
        &input.draft.team_role,
        &input.draft.professional_responsibilities,
        &input.draft.personality_traits,
        &input.draft.working_principles,
        &input.draft.growth_topic,
    )
    .map_err(|_| MemberOperationError {
        code: "member.invalid_identity",
        message: "One or more member identity fields are invalid; fix the confirmed card and try again",
        details: None,
    })?;

    let avatar_sha = input
        .draft
        .avatar_file
        .as_deref()
        .map(Path::new)
        .map(hash_selected_avatar)
        .transpose()
        .map_err(map_avatar_error)?;
    let digest = member_card_digest(&input.draft, avatar_sha.as_ref())?;

    // Pre-check the confirmed capability (no claim yet): the row's immutable
    // card_digest must match the card presented now; the run/scope must be
    // authorized to claim it.
    let pre: Option<(String,)> = database
        .connection()
        .query_row(
            r#"
            SELECT card_digest FROM member_create_capability
            WHERE preview_id=?1 AND authorized_run_id=?2
              AND scope_kind='camp' AND (scope_id IS ?3)
              AND status='issued'
            "#,
            params![
                input.preview_id,
                authenticated_run.agent_run_id.to_string(),
                authenticated_run.camp_id,
            ],
            |row| Ok((row.get::<_, String>(0)?,)),
        )
        .optional()?;
    let Some((stored_digest,)) = pre else {
        return Err(MemberOperationError {
            code: "member.confirmation_required",
            message: "No confirmed capability is claimable for this preview by this run; run member.create.preview, then confirm, then retry",
            details: None,
        }
        .into());
    };
    if stored_digest != digest {
        return Err(MemberOperationError {
            code: "member.card_mismatch",
            message: "Member card changed after confirmation; run member.create.preview again",
            details: None,
        }
        .into());
    }

    // Import (publish) the avatar only after the capability verifies.
    let avatar_ref = input
        .draft
        .avatar_file
        .as_deref()
        .map(Path::new)
        .map(|path| import_managed_member_avatar(data_dir, creation_id, path))
        .transpose()
        .map_err(map_avatar_error)?
        .map(|summary| summary.avatar_ref);

    let envelope = CommandEnvelope {
        command_id: format!("member-create:{creation_id}"),
        actor: ActorRef::User {
            user_id: CURRENT_USER_ID.to_string(),
        },
        camp_id: None,
        expected_versions: Vec::new(),
        execution_epoch: None,
        payload: CreateAgentProfileCommand {
            display_name: input.draft.display_name,
            avatar_ref: avatar_ref.clone(),
            team_role: input.draft.team_role,
            professional_responsibilities: input.draft.professional_responsibilities,
            personality_traits: input.draft.personality_traits,
            working_principles: input.draft.working_principles,
            growth_topic: input.draft.growth_topic,
        },
    };
    let claim = MemberCapabilityClaim {
        preview_id: input.preview_id,
        authorized_run_id: authenticated_run.agent_run_id.to_string(),
        scope_kind: "camp",
        scope_id: Some(authenticated_run.camp_id.clone()),
    };
    let execution = AgentProfileService::default()
        .create_confirmed_profile(database, &envelope, &claim, authenticated_run)
        .map_err(|error| {
            if error.downcast_ref::<CommandGatewayError>().is_some() {
                anyhow::Error::new(MemberOperationError {
                    code: "member.creation_key_conflict",
                    message: "creationKey was already used with different member details",
                    details: None,
                })
            } else {
                error
            }
        })?;

    // A rejected creation must not orphan the published avatar.
    if execution.result.status != CommandResultStatus::Applied {
        if let Some(ref_avatar) = &avatar_ref {
            let _ = remove_managed_member_avatar(data_dir, ref_avatar);
        }
    }
    Ok(MemberCreateOutcome {
        execution,
        avatar_ref,
    })
}

/// Canonical member-card fingerprint that a confirmation credential binds to.
fn member_card_digest(draft: &MemberCreateDraft, avatar_sha: Option<&String>) -> Result<String> {
    let canonical = serde_json::to_string(&json!({
        "creationKey": draft.creation_key,
        "displayName": draft.display_name,
        "teamRole": draft.team_role,
        "professionalResponsibilities": draft.professional_responsibilities,
        "personalityTraits": draft.personality_traits,
        "workingPrinciples": draft.working_principles,
        "growthTopic": draft.growth_topic,
        "avatarSha256": avatar_sha,
    }))?;
    Ok(sha256_hex(canonical.as_bytes()))
}

fn member_card_json(draft: &MemberCreateDraft, avatar_sha: Option<String>, digest: &str) -> Value {
    json!({
        "displayName": draft.display_name,
        "teamRole": draft.team_role,
        "professionalResponsibilities": draft.professional_responsibilities,
        "personalityTraits": draft.personality_traits,
        "workingPrinciples": draft.working_principles,
        "growthTopic": draft.growth_topic,
        "avatarSha256": avatar_sha,
        "cardDigest": digest,
        "impact": {
            "avatarRequested": draft.avatar_file.is_some(),
            "runtimeNotConfigured": true,
        },
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Immutable product receipt. This is not a CampMessage or model input.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberCreationView {
    pub creation_id: String,
    #[serde(default)]
    pub source_agent_run_id: Option<String>,
    pub agent_id: String,
    pub display_name: String,
    pub avatar_ref: Option<String>,
    pub team_role: String,
    pub professional_responsibilities: String,
    pub personality_traits: Vec<String>,
    pub creator_agent_id: String,
    pub creator_display_name: String,
    pub created_at: String,
}

pub(crate) fn record_member_creation(
    transaction: &Transaction<'_>,
    source: &AuthenticatedTeamToolRun,
    receipt: &MemberCreationView,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO member_creation(creation_id, camp_id, snapshot_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![receipt.creation_id, source.camp_id, serde_json::to_string(receipt)?, receipt.created_at],
    )?;
    transaction.execute(
        "INSERT INTO member_creation_preference(singleton, helper_agent_id) VALUES (1, ?1)
         ON CONFLICT(singleton) DO UPDATE SET helper_agent_id=excluded.helper_agent_id",
        [&source.agent_id],
    )?;
    Ok(())
}

pub(crate) fn last_creation_helper(connection: &Connection) -> Result<Option<String>> {
    Ok(connection
        .query_row(
            "SELECT helper_agent_id FROM member_creation_preference WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()?)
}

pub(crate) fn list_member_creations(
    connection: &Connection,
    camp_id: &str,
) -> Result<Vec<MemberCreationView>> {
    let mut statement = connection.prepare("SELECT snapshot_json FROM member_creation WHERE camp_id=?1 ORDER BY created_at, creation_id")?;
    let rows = statement
        .query_map([camp_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|row| Ok(serde_json::from_str(&row)?))
        .collect()
}

fn require_direct_user_trigger(
    database: &Database,
    authenticated_run: &AuthenticatedTeamToolRun,
) -> Result<()> {
    if has_direct_user_input(database.connection(), authenticated_run)? {
        return Ok(());
    }
    Err(MemberOperationError {
        code: "member.user_confirmation_required",
        message: "Create a member only from a direct user-triggered run after showing the final member card and receiving confirmation",
        details: None,
    }
    .into())
}

pub(crate) fn has_direct_user_input(
    connection: &Connection,
    authenticated_run: &AuthenticatedTeamToolRun,
) -> Result<bool> {
    let trigger = connection
        .query_row(
            r#"
            SELECT EXISTS(
                SELECT 1
                FROM agent_run_input
                JOIN camp_message ON camp_message.id = agent_run_input.message_id
                WHERE agent_run_input.agent_run_id = agent_run.id
                  AND camp_message.author_type = 'user'
                  AND camp_message.camp_id = ?3
            )
            FROM agent_run
            WHERE agent_run.id = ?1
              AND agent_run.execution_epoch = ?2
            "#,
            params![
                authenticated_run.agent_run_id,
                authenticated_run.execution_epoch,
                authenticated_run.camp_id,
            ],
            |row| row.get::<_, bool>(0),
        )
        .optional()?;
    Ok(trigger == Some(true))
}

fn invalid_creation_key() -> MemberOperationError {
    MemberOperationError {
        code: "member.invalid_creation_key",
        message: "creationKey must be a canonical lowercase UUID",
        details: None,
    }
}

fn map_avatar_error(error: MemberAvatarImportError) -> MemberOperationError {
    match error.kind {
        MemberAvatarImportErrorKind::Invalid => MemberOperationError {
            code: "member.avatar_invalid",
            message: "The avatar file could not be safely imported; fix the image or retry without --avatar-file",
            details: None,
        },
        MemberAvatarImportErrorKind::CreationKeyConflict => MemberOperationError {
            code: "member.creation_key_conflict",
            message: "creationKey is already bound to a different avatar",
            details: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_create_schema_requires_preview_id() {
        let schema = member_create_input_schema();
        assert_eq!(
            schema["required"],
            json!(["creationKey", "displayName", "previewId"])
        );
        assert_eq!(schema["properties"]["previewId"]["type"], "string");
    }

    #[test]
    fn member_tool_schemas_reject_unknown_or_invalid_fields() {
        let schema = member_create_input_schema();
        assert_eq!(schema["properties"]["avatarFile"]["type"], "string");
        use crate::team_tool_catalog::validate_builtin_tool_input as validate;
        let base = json!({"agentId":"agent_1","requestId":"51d668e1-6dc7-4f39-80b2-0555f823715a","expectedVersion":1,"teamRole":""});
        assert!(validate("member.update", &base).is_ok());
        for (field, value) in [
            ("teamRole", Value::Null),
            ("displayName", json!("")),
            ("expectedVersion", json!(0)),
            ("requestId", json!("51D668E1-6DC7-4F39-80B2-0555F823715A")),
            ("runtime", json!({})),
            ("portraitRef", json!("x")),
            ("avatarCenterX", json!(0.5)),
        ] {
            let mut input = base.clone();
            input[field] = value;
            assert!(validate("member.update", &input).is_err(), "{input}");
        }
        for patch in [
            json!({}),
            json!({"clearAvatar":false}),
            json!({"clearAvatar":true,"avatarFile":"source.png"}),
            json!({"clearAvatar":true,"avatarCenterX":0.5,"avatarCenterY":0.5,"avatarSize":0.5}),
        ] {
            let mut input = base.clone();
            input.as_object_mut().unwrap().remove("teamRole");
            input
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            assert!(validate("member.update", &input).is_err(), "{input}");
        }
        assert!(validate("member.list", &json!({"agentId":"agent_1"})).is_err());
        assert!(validate("member.get", &json!({"agentId":"agent_1","runtime":true})).is_err());
    }
}
