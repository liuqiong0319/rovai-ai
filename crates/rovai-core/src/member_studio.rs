use std::path::Path;

use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    agent_profile::{
        AgentProfileService, CreateAgentProfileCommand, MemberConfirmationCredential,
        validate_member_identity_input,
    },
    command::{ActorRef, CommandEnvelope, CommandExecution, CommandGatewayError, CommandResultStatus},
    current_user::CURRENT_USER_ID,
    db::Database,
    member_avatar::{
        MemberAvatarImportError, MemberAvatarImportErrorKind, hash_selected_avatar,
        import_managed_member_avatar, remove_managed_member_avatar, validate_avatar_file,
    },
    team_tool::AuthenticatedTeamToolRun,
};

pub const MEMBER_CREATE_TOOL_NAME: &str = "member.create";
pub const MEMBER_CREATE_PREVIEW_TOOL_NAME: &str = "member.create.preview";

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
    pub confirmation_token: String,
    #[serde(flatten)]
    pub draft: MemberCreateDraft,
}

#[derive(Debug)]
pub struct MemberCreateOutcome {
    pub execution: CommandExecution,
    pub avatar_ref: Option<String>,
}

/// Result of `member.create.preview`: the member card and the one-time
/// confirmation credential that must accompany the subsequent create call.
#[derive(Debug)]
pub struct MemberCreatePreviewOutcome {
    pub card: Value,
    pub confirmation_token: String,
    pub token_expires_at: String,
}

#[derive(Debug)]
pub struct MemberCreateError {
    pub code: &'static str,
    pub message: &'static str,
    pub details: Option<Value>,
}

impl std::fmt::Display for MemberCreateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for MemberCreateError {}

pub fn member_create_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["creationKey", "displayName", "confirmationToken"],
        "properties": {
            "creationKey": {
                "type": "string",
                "format": "uuid",
                "description": "A new canonical lowercase UUID. Reuse it only when retrying this exact confirmed member creation."
            },
            "confirmationToken": {
                "type": "string",
                "minLength": 32,
                "description": "One-time confirmation credential issued by member.create.preview for this exact member card. It is consumed atomically when the member is created and can never be reused."
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
    .map_err(|_| MemberCreateError {
        code: "member.invalid_identity",
        message: "One or more member identity fields are invalid; fix the confirmed card and try again",
        details: None,
    })?;

    // 1) Card digest binds the exact identity and avatar file content. The
    // avatar is only read (hashed) here; publishing happens after the
    // confirmation credential verifies so no asset can be orphaned.
    let avatar_sha = input
        .draft
        .avatar_file
        .as_deref()
        .map(Path::new)
        .map(hash_selected_avatar)
        .transpose()
        .map_err(map_avatar_error)?;
    let digest = member_card_digest(&input.draft, avatar_sha.as_ref())?;
    let token_hash = sha256_hex(input.confirmation_token.as_bytes());

    // 2) Verify the server-issued one-time credential without consuming it.
    // The atomic consume happens inside the same transaction that inserts the
    // profile, so a concurrent reuse cannot produce two members.
    verify_confirmation_credential(
        database,
        authenticated_run,
        &input.draft.creation_key,
        &token_hash,
        &digest,
    )?;

    // 3) Import (publish) the avatar only after the credential verifies.
    let avatar_ref = input
        .draft
        .avatar_file
        .as_deref()
        .map(Path::new)
        .map(|path| import_managed_member_avatar(data_dir, creation_id, path))
        .transpose()
        .map_err(map_avatar_error)?
        .map(|summary| summary.avatar_ref);

    // 4) Atomically consume the credential and create the profile.
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
    let confirmation = MemberConfirmationCredential {
        creation_key: input.draft.creation_key,
        token_hash,
        card_digest: digest,
        user_id: CURRENT_USER_ID.to_string(),
        agent_run_id: authenticated_run.agent_run_id.to_string(),
    };
    let execution = match AgentProfileService::default()
        .create_confirmed_profile(database, &envelope, confirmation)
    {
        Ok(execution) => execution,
        Err(error) => {
            // Compensate the published avatar whenever the profile could not be
            // created through an error (e.g. creationKey conflict).
            if let Some(ref_avatar) = &avatar_ref {
                let _ = remove_managed_member_avatar(data_dir, ref_avatar);
            }
            let mapped = if error.downcast_ref::<CommandGatewayError>().is_some() {
                anyhow::Error::new(MemberCreateError {
                    code: "member.creation_key_conflict",
                    message: "creationKey was already used with different member details",
                    details: None,
                })
            } else {
                error
            };
            return Err(mapped);
        }
    };

    // A rejected atomic consume must not orphan the already-published avatar,
    // EXCEPT when the credential was consumed by a concurrent winner: that
    // winner's profile now references this same content-addressed asset, so
    // deleting it would break the member that did get created.
    if execution.result.status != CommandResultStatus::Applied
        && execution.result.code != "member.confirmation_already_consumed"
    {
        if let Some(ref_avatar) = &avatar_ref {
            let _ = remove_managed_member_avatar(data_dir, ref_avatar);
        }
    }
    Ok(MemberCreateOutcome {
        execution,
        avatar_ref,
    })
}

/// Validates a draft member card without persisting anything, then issues the
/// one-time confirmation credential the subsequent create call must present.
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
    .map_err(|_| MemberCreateError {
        code: "member.invalid_identity",
        message: "One or more member identity fields are invalid; fix the confirmed card and try again",
        details: None,
    })?;
    if AgentProfileService::default().display_name_exists(database, &input.display_name)? {
        return Err(MemberCreateError {
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
    let card = member_card_json(&input, avatar_sha, &digest);

    let token = format!("{}{}", Uuid::new_v4(), Uuid::new_v4());
    let token_hash = sha256_hex(token.as_bytes());
    let now = chrono::Utc::now();
    let issued_at = now.to_rfc3339();
    let expires_at = (now + chrono::Duration::seconds(CONFIRMATION_TTL_SECONDS)).to_rfc3339();
    let user_id = CURRENT_USER_ID.to_string();
    let agent_run_id = authenticated_run.agent_run_id.to_string();
    database.connection_mut().execute(
        r#"
        INSERT INTO member_create_credential(
            creation_key, user_id, agent_run_id, card_digest, token_hash,
            issued_at, expires_at, consumed_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)
        ON CONFLICT(creation_key) DO UPDATE SET
            user_id = excluded.user_id,
            agent_run_id = excluded.agent_run_id,
            card_digest = excluded.card_digest,
            token_hash = excluded.token_hash,
            issued_at = excluded.issued_at,
            expires_at = excluded.expires_at,
            consumed_at = NULL
        "#,
        params![
            input.creation_key,
            user_id,
            agent_run_id,
            digest,
            token_hash,
            issued_at,
            expires_at,
        ],
    )?;

    Ok(MemberCreatePreviewOutcome {
        card,
        confirmation_token: token,
        token_expires_at: expires_at,
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

/// Reads the server-issued credential for a creation key and checks it is not
/// consumed, not expired, and matches this user/run/card. Consumption itself
/// happens atomically inside create_confirmed_profile.
fn verify_confirmation_credential(
    database: &Database,
    authenticated_run: &AuthenticatedTeamToolRun,
    creation_key: &str,
    token_hash: &str,
    digest: &str,
) -> Result<()> {
    let row = database
        .connection()
        .query_row(
            r#"
            SELECT token_hash, user_id, agent_run_id, card_digest, consumed_at, expires_at
            FROM member_create_credential
            WHERE creation_key = ?1
            "#,
            params![creation_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((stored_hash, stored_user, stored_run, stored_digest, consumed_at, expires_at)) =
        row
    else {
        return Err(MemberCreateError {
            code: "member.confirmation_invalid",
            message: "Confirmation token was never issued for this creation key; run member.create.preview first",
            details: None,
        }
        .into());
    };
    if stored_hash != token_hash {
        return Err(MemberCreateError {
            code: "member.confirmation_invalid",
            message: "Confirmation token does not match the issued credential",
            details: None,
        }
        .into());
    }
    if stored_user != CURRENT_USER_ID || stored_run != authenticated_run.agent_run_id.to_string() {
        return Err(MemberCreateError {
            code: "member.confirmation_invalid",
            message: "Confirmation credential is bound to a different user or run",
            details: None,
        }
        .into());
    }
    if consumed_at.is_some() {
        return Err(MemberCreateError {
            code: "member.confirmation_already_consumed",
            message: "This confirmation token was already used",
            details: None,
        }
        .into());
    }
    if expires_at.as_str() <= chrono::Utc::now().to_rfc3339() {
        return Err(MemberCreateError {
            code: "member.confirmation_expired",
            message: "Confirmation token expired; run member.create.preview again",
            details: None,
        }
        .into());
    }
    if stored_digest != digest {
        return Err(MemberCreateError {
            code: "member.card_mismatch",
            message: "Member card changed after confirmation; run member.create.preview again",
            details: None,
        }
        .into());
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn require_direct_user_trigger(
    database: &Database,
    authenticated_run: &AuthenticatedTeamToolRun,
) -> Result<()> {
    let trigger = database
        .connection()
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
    if trigger == Some(true) {
        return Ok(());
    }
    Err(MemberCreateError {
        code: "member.user_confirmation_required",
        message: "Create a member only from a direct user-triggered run after showing the final member card and receiving confirmation",
        details: None,
    }
    .into())
}

fn invalid_creation_key() -> MemberCreateError {
    MemberCreateError {
        code: "member.invalid_creation_key",
        message: "creationKey must be a canonical lowercase UUID",
        details: None,
    }
}

fn map_avatar_error(error: MemberAvatarImportError) -> MemberCreateError {
    match error.kind {
        MemberAvatarImportErrorKind::Invalid => MemberCreateError {
            code: "member.avatar_invalid",
            message: "The avatar file could not be safely imported; fix the image or retry without --avatar-file",
            details: None,
        },
        MemberAvatarImportErrorKind::CreationKeyConflict => MemberCreateError {
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
    fn member_create_schema_keeps_avatar_file_optional() {
        let schema = member_create_input_schema();
        assert_eq!(schema["required"], json!(["creationKey", "displayName"]));
        assert_eq!(schema["properties"]["avatarFile"]["type"], "string");
    }
}
