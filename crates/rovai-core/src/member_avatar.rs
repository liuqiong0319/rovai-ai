use std::{
    fs::{self, OpenOptions},
    io::{BufReader, Cursor, Read, Write},
    path::Path,
};

#[cfg(not(windows))]
use std::fs::File;

use anyhow::Result;
use gif::{
    DecodeOptions, Decoder as GifDecoder, DisposalMethod, Encoder as GifEncoder, Frame as GifFrame,
    Repeat,
};
use image::{
    DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader, Limits, RgbaImage,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

const SELECTED_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MINIMUM_DECODED_EDGE: u32 = 256;
const MAXIMUM_DECODED_EDGE: u32 = 8192;
const MAXIMUM_DECODED_PIXELS: u64 = 32_000_000;
const NORMALIZED_MAXIMUM_EDGE: u32 = 2048;
const NORMALIZED_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const ICON_EDGE: u32 = 192;
const ICON_BYTES: usize = 1024 * 1024;
const MANIFEST_BYTES: usize = 16 * 1024;
const MAXIMUM_DECODE_ALLOCATION: u64 = 256 * 1024 * 1024;
/// Hard ceiling on decoded frames for an animated GIF member avatar.
const MAXIMUM_GIF_FRAMES: u32 = 200;
/// Cumulative decoded frame pixels across the whole animation; bounds decode
/// allocation and prevents a multi-frame bomb (e.g. 200 × 32M px).
const MAXIMUM_GIF_CUMULATIVE_DECODED_PIXELS: u64 = 128_000_000;

pub const LUOKE_AVATAR_REF: &str = "rovai://member-avatar/builtin/luoke/v1";
pub const MUWA_AVATAR_REF: &str = "rovai://member-avatar/builtin/muwa/v1";
pub const MIANZHI_AVATAR_REF: &str = "rovai://member-avatar/builtin/mianzhi/v1";
pub const QILU_AVATAR_REF: &str = "rovai://member-avatar/builtin/qilu/v1";

pub const BUILTIN_PROFILE_AVATARS: [(&str, &str); 4] = [
    ("agent_1", LUOKE_AVATAR_REF),
    ("agent_2", MUWA_AVATAR_REF),
    ("agent_3", MIANZHI_AVATAR_REF),
    ("agent_4", QILU_AVATAR_REF),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinMemberAvatar {
    Luoke,
    Muwa,
    Mianzhi,
    Qilu,
}

impl BuiltinMemberAvatar {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Luoke => "luoke",
            Self::Muwa => "muwa",
            Self::Mianzhi => "mianzhi",
            Self::Qilu => "qilu",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberAvatarReference {
    Builtin(BuiltinMemberAvatar),
    Managed(Uuid),
}

pub fn parse_member_avatar_ref(value: &str) -> Result<MemberAvatarReference> {
    let url = Url::parse(value).map_err(|_| invalid_avatar_ref())?;
    if url.scheme() != "rovai"
        || url.host_str() != Some("member-avatar")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid_avatar_ref());
    }
    let segments = url
        .path_segments()
        .ok_or_else(invalid_avatar_ref)?
        .collect::<Vec<_>>();
    match segments.as_slice() {
        ["builtin", role, "v1"] => {
            let builtin = match *role {
                "luoke" => BuiltinMemberAvatar::Luoke,
                "muwa" => BuiltinMemberAvatar::Muwa,
                "mianzhi" => BuiltinMemberAvatar::Mianzhi,
                "qilu" => BuiltinMemberAvatar::Qilu,
                _ => return Err(invalid_avatar_ref()),
            };
            let canonical = format!("rovai://member-avatar/builtin/{}/v1", builtin.as_str());
            if value != canonical {
                return Err(invalid_avatar_ref());
            }
            Ok(MemberAvatarReference::Builtin(builtin))
        }
        ["managed", asset_id] => {
            let asset_id = Uuid::parse_str(asset_id).map_err(|_| invalid_avatar_ref())?;
            let canonical = format!("rovai://member-avatar/managed/{}", asset_id.hyphenated());
            if value != canonical {
                return Err(invalid_avatar_ref());
            }
            Ok(MemberAvatarReference::Managed(asset_id))
        }
        _ => Err(invalid_avatar_ref()),
    }
}

pub fn validate_new_member_avatar_ref(value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        parse_member_avatar_ref(value)?;
    }
    Ok(())
}

pub fn validate_member_avatar_update(current: Option<&str>, next: Option<&str>) -> Result<()> {
    if current == next {
        return Ok(());
    }
    validate_new_member_avatar_ref(next)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedMemberAvatarSummary {
    pub avatar_ref: String,
    pub source_width: u32,
    pub source_height: u32,
    pub crop: MemberAvatarCrop,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberAvatarCrop {
    pub center_x: f64,
    pub center_y: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberAvatarImportErrorKind {
    Invalid,
    CreationKeyConflict,
}

#[derive(Debug)]
pub struct MemberAvatarImportError {
    pub kind: MemberAvatarImportErrorKind,
    message: &'static str,
}

impl MemberAvatarImportError {
    fn invalid(message: &'static str) -> Self {
        Self {
            kind: MemberAvatarImportErrorKind::Invalid,
            message,
        }
    }

    fn conflict() -> Self {
        Self {
            kind: MemberAvatarImportErrorKind::CreationKeyConflict,
            message: "creationKey is already bound to a different avatar",
        }
    }
}

impl std::fmt::Display for MemberAvatarImportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for MemberAvatarImportError {}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AvatarManifestFile {
    file: String,
    media_type: String,
    width: u32,
    height: u32,
    byte_length: usize,
    sha256: String,
    /// Present only on schemaVersion 2 GIF renditions.
    #[serde(default)]
    frame_count: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AvatarManifestSource {
    #[serde(flatten)]
    file: AvatarManifestFile,
    orientation_normalized: bool,
    metadata_stripped: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AvatarManifestV1 {
    schema_version: u32,
    asset_id: String,
    created_at: String,
    source: AvatarManifestSource,
    icon: AvatarManifestFile,
    icon_crop: MemberAvatarCrop,
}

/// Imports a run-readable PNG, JPEG or animated GIF into the same compound
/// managed-avatar format used by Electron Main. The caller supplies a
/// deterministic asset ID so a retried member creation can verify rather than
/// duplicate the bytes. GIF is the only supported dynamic format and keeps its
/// animation across normalization and icon derivation.
pub fn import_managed_member_avatar(
    data_dir: &Path,
    asset_id: Uuid,
    selected_file: &Path,
) -> std::result::Result<ManagedMemberAvatarSummary, MemberAvatarImportError> {
    let selected_bytes = read_selected_avatar(selected_file)?;
    let format = image::guess_format(&selected_bytes)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar must be a PNG, JPEG or GIF image"))?;
    if format == ImageFormat::Gif {
        return import_gif_selected(data_dir, asset_id, &selected_bytes);
    }
    let normalized = normalize_selected_avatar(selected_file)?;
    let (source_width, source_height) = normalized.dimensions();
    let (crop_x, crop_y, crop_edge) = default_avatar_crop(source_width, source_height);
    let icon = normalized
        .crop_imm(crop_x, crop_y, crop_edge, crop_edge)
        .resize_exact(ICON_EDGE, ICON_EDGE, image::imageops::FilterType::Lanczos3);
    let source_png = encode_png(&normalized)?;
    let icon_png = encode_png(&icon)?;
    if source_png.len() > NORMALIZED_SOURCE_BYTES || icon_png.len() > ICON_BYTES {
        return Err(MemberAvatarImportError::invalid(
            "Normalized avatar exceeds the managed asset limit",
        ));
    }
    let crop = default_member_avatar_crop(source_width, source_height);
    publish_managed_member_avatar(
        data_dir,
        asset_id,
        &source_png,
        &icon_png,
        source_width,
        source_height,
        crop,
    )
}

fn import_gif_selected(
    data_dir: &Path,
    asset_id: Uuid,
    selected_bytes: &[u8],
) -> std::result::Result<ManagedMemberAvatarSummary, MemberAvatarImportError> {
    let info = parse_gif_info(selected_bytes)?;
    validate_gif_file_info(&info)?;
    let frames = decode_composited_gif_frames(selected_bytes)?;
    let crop = default_member_avatar_crop(info.width, info.height);
    let source_gif = normalize_gif_source_bytes(selected_bytes, &frames, info.loop_count)?;
    let icon_gif =
        crop_gif_frames_to_icon(&frames, info.loop_count, &crop, info.width, info.height)?;
    if source_gif.len() > NORMALIZED_SOURCE_BYTES || icon_gif.len() > ICON_BYTES {
        return Err(MemberAvatarImportError::invalid(
            "Normalized avatar exceeds the managed asset limit",
        ));
    }
    let (normalized_width, normalized_height) = normalized_gif_dimensions(info.width, info.height);
    let crop = normalized_gif_crop(crop, normalized_width, normalized_height);
    publish_managed_member_avatar_gif(
        data_dir,
        asset_id,
        &source_gif,
        &icon_gif,
        normalized_width,
        normalized_height,
        crop,
        info.frame_count,
    )
}

/// Strips non-rendering metadata without changing pixels when the canvas fits;
/// otherwise resizes every composited frame and re-encodes.
fn normalize_gif_source_bytes(
    selected_bytes: &[u8],
    frames: &[CompositedGifFrame],
    loop_count: Option<u16>,
) -> std::result::Result<Vec<u8>, MemberAvatarImportError> {
    let (width, height) = frames
        .first()
        .map(|frame| frame.image.dimensions())
        .ok_or_else(|| invalid_gif_structure())?;
    if width.max(height) <= NORMALIZED_MAXIMUM_EDGE {
        // Copy only rendering blocks: remove comments, unknown application
        // metadata and bytes after the trailer without requantizing the source.
        let info = parse_gif_info(selected_bytes)?;
        let mut normalized = Vec::with_capacity(selected_bytes.len());
        for (start, end) in info.rendering_blocks {
            normalized.extend_from_slice(&selected_bytes[start..end]);
        }
        return Ok(normalized);
    }
    resize_gif_frames(frames, loop_count, NORMALIZED_MAXIMUM_EDGE)
}

fn normalize_selected_avatar(
    selected_file: &Path,
) -> Result<DynamicImage, MemberAvatarImportError> {
    let selected_bytes = read_selected_avatar(selected_file)?;
    let format = image::guess_format(&selected_bytes)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar must be a PNG or JPEG image"))?;
    if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
        return Err(MemberAvatarImportError::invalid(
            "Avatar must be a PNG or JPEG image",
        ));
    }
    if format == ImageFormat::Png {
        inspect_upload_png(&selected_bytes)?;
    }

    let mut reader = ImageReader::with_format(Cursor::new(&selected_bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAXIMUM_DECODED_EDGE);
    limits.max_image_height = Some(MAXIMUM_DECODED_EDGE);
    limits.max_alloc = Some(MAXIMUM_DECODE_ALLOCATION);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| MemberAvatarImportError::invalid("Avatar image could not be decoded"))?;
    let (decoded_width, decoded_height) = decoder.dimensions();
    validate_decoded_dimensions(decoded_width, decoded_height)?;
    if decoder.total_bytes() > MAXIMUM_DECODE_ALLOCATION {
        return Err(MemberAvatarImportError::invalid(
            "Avatar image exceeds the decoded resource limit",
        ));
    }
    let orientation = decoder
        .orientation()
        .map_err(|_| MemberAvatarImportError::invalid("Avatar orientation is invalid"))?;
    let mut decoded = DynamicImage::from_decoder(decoder)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar image could not be decoded"))?;
    decoded.apply_orientation(orientation);
    validate_decoded_dimensions(decoded.width(), decoded.height())?;

    let normalized = if decoded.width() > NORMALIZED_MAXIMUM_EDGE
        || decoded.height() > NORMALIZED_MAXIMUM_EDGE
    {
        decoded.resize(
            NORMALIZED_MAXIMUM_EDGE,
            NORMALIZED_MAXIMUM_EDGE,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        decoded
    };
    let normalized = DynamicImage::ImageRgba8(normalized.to_rgba8());
    Ok(normalized)
}

/// Reuses creation's decoder and normalization, with an optional explicit crop.
pub fn prepare_member_avatar(
    data_dir: &Path,
    asset_id: Uuid,
    selected_file: &Path,
    crop: Option<MemberAvatarCrop>,
    verify_existing_only: bool,
) -> Result<ManagedMemberAvatarSummary> {
    let selected_bytes = read_selected_avatar(selected_file)?;
    let format = image::guess_format(&selected_bytes)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar must be a PNG, JPEG or GIF image"))?;
    if format == ImageFormat::Gif {
        return prepare_gif_avatar(
            data_dir,
            asset_id,
            &selected_bytes,
            crop,
            verify_existing_only,
        );
    }
    let source = normalize_selected_avatar(selected_file)?;
    let source_png = encode_png(&source)?;
    save_cropped_source(
        data_dir,
        &source_png,
        source,
        crop,
        Some((asset_id, verify_existing_only)),
    )
}

fn prepare_gif_avatar(
    data_dir: &Path,
    asset_id: Uuid,
    selected_bytes: &[u8],
    crop: Option<MemberAvatarCrop>,
    verify_existing_only: bool,
) -> Result<ManagedMemberAvatarSummary> {
    let info = parse_gif_info(selected_bytes)?;
    validate_gif_file_info(&info)?;
    let frames = decode_composited_gif_frames(selected_bytes)?;
    let crop = crop.unwrap_or_else(|| default_member_avatar_crop(info.width, info.height));
    let source_gif = normalize_gif_source_bytes(selected_bytes, &frames, info.loop_count)?;
    let icon_gif =
        crop_gif_frames_to_icon(&frames, info.loop_count, &crop, info.width, info.height)?;
    anyhow::ensure!(
        source_gif.len() <= NORMALIZED_SOURCE_BYTES && icon_gif.len() <= ICON_BYTES,
        "Avatar exceeds the managed asset limit"
    );
    let (normalized_width, normalized_height) = normalized_gif_dimensions(info.width, info.height);
    let crop = normalized_gif_crop(crop, normalized_width, normalized_height);
    if verify_existing_only {
        let directory = data_dir.join("member-avatars").join(asset_id.to_string());
        if !existing_asset_matches(
            &directory,
            &asset_id.to_string(),
            &gif_manifest_file(
                "source.gif",
                normalized_width,
                normalized_height,
                info.frame_count,
                &source_gif,
            ),
            &gif_manifest_file(
                "icon-192.gif",
                ICON_EDGE,
                ICON_EDGE,
                info.frame_count,
                &icon_gif,
            ),
            &crop,
        )
        .unwrap_or(false)
        {
            return Err(MemberAvatarImportError::conflict().into());
        }
        return Ok(ManagedMemberAvatarSummary {
            avatar_ref: format!("rovai://member-avatar/managed/{asset_id}"),
            source_width: normalized_width,
            source_height: normalized_height,
            crop,
        });
    }
    Ok(publish_managed_member_avatar_gif(
        data_dir,
        asset_id,
        &source_gif,
        &icon_gif,
        normalized_width,
        normalized_height,
        crop,
        info.frame_count,
    )?)
}

pub fn recrop_member_avatar(
    data_dir: &Path,
    avatar_ref: &str,
    crop: MemberAvatarCrop,
) -> Result<ManagedMemberAvatarSummary> {
    let source_bytes = member_avatar_bytes(data_dir, avatar_ref, true)?;
    if parse_gif_info(&source_bytes).is_ok() {
        let info = parse_gif_info(&source_bytes)?;
        return Ok(save_normalized_member_avatar_gif(
            data_dir,
            &source_bytes,
            info.width,
            info.height,
            crop,
            info.frame_count,
        )?);
    }
    let source = image::load_from_memory_with_format(&source_bytes, ImageFormat::Png)?;
    save_cropped_source(data_dir, &source_bytes, source, Some(crop), None)
}

fn default_member_avatar_crop(width: u32, height: u32) -> MemberAvatarCrop {
    let (x, y, edge) = default_avatar_crop(width, height);
    MemberAvatarCrop {
        center_x: (f64::from(x) + f64::from(edge) / 2.0) / f64::from(width),
        center_y: (f64::from(y) + f64::from(edge) / 2.0) / f64::from(height),
        size: f64::from(edge) / f64::from(width.min(height)),
    }
}

fn save_cropped_source(
    data_dir: &Path,
    source_png: &[u8],
    source: DynamicImage,
    crop: Option<MemberAvatarCrop>,
    asset_id: Option<(Uuid, bool)>,
) -> Result<ManagedMemberAvatarSummary> {
    let (width, height) = source.dimensions();
    let crop = crop.unwrap_or_else(|| default_member_avatar_crop(width, height));
    validate_crop(&crop, width, height)?;
    let edge = (crop.size * f64::from(width.min(height))).round().max(1.0) as u32;
    let x = (crop.center_x * f64::from(width) - f64::from(edge) / 2.0)
        .round()
        .max(0.0) as u32;
    let y = (crop.center_y * f64::from(height) - f64::from(edge) / 2.0)
        .round()
        .max(0.0) as u32;
    let icon = source
        .crop_imm(x.min(width - edge), y.min(height - edge), edge, edge)
        .resize_exact(ICON_EDGE, ICON_EDGE, image::imageops::FilterType::Lanczos3);
    let icon_png = encode_png(&icon)?;
    anyhow::ensure!(
        source_png.len() <= NORMALIZED_SOURCE_BYTES && icon_png.len() <= ICON_BYTES,
        "Normalized avatar exceeds the managed asset limit"
    );
    if let Some((asset_id, verify_existing_only)) = asset_id {
        if verify_existing_only {
            let directory = data_dir.join("member-avatars").join(asset_id.to_string());
            if !existing_asset_matches(
                &directory,
                &asset_id.to_string(),
                &manifest_file("source.png", width, height, source_png),
                &manifest_file("icon-192.png", ICON_EDGE, ICON_EDGE, &icon_png),
                &crop,
            )
            .unwrap_or(false)
            {
                return Err(MemberAvatarImportError::conflict().into());
            }
            return Ok(ManagedMemberAvatarSummary {
                avatar_ref: format!("rovai://member-avatar/managed/{asset_id}"),
                source_width: width,
                source_height: height,
                crop,
            });
        }
        Ok(publish_managed_member_avatar(
            data_dir, asset_id, source_png, &icon_png, width, height, crop,
        )?)
    } else {
        save_managed_member_avatar(data_dir, source_png, &icon_png, width, height, crop)
    }
}

/// Content equivalence preserves a Profile reference for a visual no-op even
/// when the upload has its own immutable request-bound preparation asset.
pub(crate) fn member_avatar_content_matches(
    data_dir: &Path,
    left: &str,
    right: &str,
) -> Result<bool> {
    if left == right {
        return Ok(true);
    }
    let (Some(left_source), Some(right_source)) = (
        read_managed_member_avatar(data_dir, left, true)?,
        read_managed_member_avatar(data_dir, right, true)?,
    ) else {
        return Ok(false);
    };
    if left_source.base64 != right_source.base64 || left_source.crop != right_source.crop {
        return Ok(false);
    }
    let (Some(left_icon), Some(right_icon)) = (
        read_managed_member_avatar(data_dir, left, false)?,
        read_managed_member_avatar(data_dir, right, false)?,
    ) else {
        return Ok(false);
    };
    Ok(left_icon.base64 == right_icon.base64)
}

/// Core-packaged renditions of the same built-in artwork as the renderer.
/// PNG sources avoid adding a runtime AVIF decoder to the controlled importer.
fn builtin_avatar_bytes(role: BuiltinMemberAvatar, portrait: bool) -> &'static [u8] {
    macro_rules! artwork {
        ($role:literal) => {
            if portrait {
                include_bytes!(concat!("../resources/member-avatars/", $role, ".png"))
            } else {
                include_bytes!(concat!(
                    "../../../apps/desktop/src/renderer/src/assets/characters/",
                    $role,
                    "/icon-192.png"
                ))
            }
        };
    }
    match role {
        BuiltinMemberAvatar::Luoke => artwork!("luoke"),
        BuiltinMemberAvatar::Muwa => artwork!("muwa"),
        BuiltinMemberAvatar::Mianzhi => artwork!("mianzhi"),
        BuiltinMemberAvatar::Qilu => artwork!("qilu"),
    }
}

/// The caller supplies the authenticated lease's existing Run tmp directory.
/// Files are new/private and share that lease's existing cleanup lifecycle.
/// Managed GIF assets materialize as `.gif` so the animation is preserved for
/// portrait and icon consumers.
pub fn materialize_member_avatar(
    data_dir: &Path,
    run_tmp: &Path,
    avatar_ref: &str,
    portrait: bool,
) -> Result<String> {
    anyhow::ensure!(run_tmp.is_absolute(), "Run tmp must be absolute");
    let metadata = fs::symlink_metadata(run_tmp)?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Run tmp is unavailable"
    );
    let rendition = managed_member_avatar_rendition(data_dir, avatar_ref, portrait)?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(&rendition.base64)?;
    let extension = if rendition.media_type == "image/gif" {
        "gif"
    } else {
        "png"
    };
    let path = run_tmp.join(format!(
        "member-{}-{}.{extension}",
        Uuid::new_v4(),
        if portrait { "portrait" } else { "icon" }
    ));
    if let Err(error) = write_private_file(&path, &bytes) {
        let _ = fs::remove_file(&path);
        return Err(error.into());
    }
    Ok(path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Run tmp path is not UTF-8"))?
        .to_string())
}

fn managed_member_avatar_rendition(
    data_dir: &Path,
    avatar_ref: &str,
    portrait: bool,
) -> Result<ManagedMemberAvatarRendition> {
    use base64::Engine;
    match parse_member_avatar_ref(avatar_ref)? {
        MemberAvatarReference::Builtin(role) => {
            let bytes = builtin_avatar_bytes(role, portrait);
            // PNG IHDR carries big-endian u32 dimensions after the 8-byte
            // signature and the 8-byte IHDR length/type fields.
            let (width, height) = if bytes.len() >= 24 {
                (
                    u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
                    u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
                )
            } else {
                (0, 0)
            };
            Ok(ManagedMemberAvatarRendition {
                base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                media_type: "image/png",
                width,
                height,
                crop: MemberAvatarCrop {
                    center_x: 0.5,
                    center_y: 0.5,
                    size: 1.0,
                },
            })
        }
        MemberAvatarReference::Managed(_) => {
            read_managed_member_avatar(data_dir, avatar_ref, portrait)?
                .ok_or_else(|| anyhow::anyhow!("Avatar rendition is unavailable"))
        }
    }
}

fn member_avatar_bytes(data_dir: &Path, avatar_ref: &str, portrait: bool) -> Result<Vec<u8>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD
        .decode(managed_member_avatar_rendition(data_dir, avatar_ref, portrait)?.base64)?)
}

/// Returns the validated frame count of a GIF member-avatar file. The Core save
/// IPC uses it so the renderer-supplied count stays consistent with the bytes.
pub fn gif_frame_count_for(bytes: &[u8]) -> Result<u32> {
    let info = parse_gif_info(bytes)?;
    validate_gif_file_info(&info)?;
    Ok(info.frame_count)
}

/// Saves a normalized, cropped UI asset through the same compound storage used
/// by member creation. Its content identity makes a lost save reply retryable.
pub fn save_managed_member_avatar(
    data_dir: &Path,
    source_png: &[u8],
    icon_png: &[u8],
    source_width: u32,
    source_height: u32,
    crop: MemberAvatarCrop,
) -> Result<ManagedMemberAvatarSummary> {
    anyhow::ensure!(
        source_png.len() <= NORMALIZED_SOURCE_BYTES && icon_png.len() <= ICON_BYTES,
        "Avatar exceeds the managed asset limit"
    );
    validate_normalized_png(
        source_png,
        source_width,
        source_height,
        NORMALIZED_MAXIMUM_EDGE,
    )?;
    validate_normalized_png(icon_png, ICON_EDGE, ICON_EDGE, ICON_EDGE)?;
    validate_crop(&crop, source_width, source_height)?;
    let mut digest = Sha256::new();
    digest.update(source_png);
    digest.update(icon_png);
    digest.update(serde_json::to_vec(&crop)?);
    let hash = digest.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    let asset_id = Uuid::from_bytes(bytes);
    Ok(publish_managed_member_avatar(
        data_dir,
        asset_id,
        source_png,
        icon_png,
        source_width,
        source_height,
        crop,
    )?)
}

/// Saves an animated GIF member-avatar source. The renderer sends the original
/// GIF plus the shared crop; the icon is derived here frame-by-frame so the
/// animation survives and never degrades to a static first frame. When the
/// logical canvas exceeds the normalized limit, every frame is resized and the
/// animation re-encoded; otherwise only non-rendering metadata is removed.
pub fn save_managed_member_avatar_gif(
    data_dir: &Path,
    source_gif: &[u8],
    source_width: u32,
    source_height: u32,
    crop: MemberAvatarCrop,
    frame_count: u32,
) -> Result<ManagedMemberAvatarSummary> {
    anyhow::ensure!(
        source_gif.len() <= SELECTED_FILE_BYTES as usize,
        "Avatar file must be no larger than 10 MiB"
    );
    let info = parse_gif_info(source_gif)?;
    validate_gif_file_info(&info)?;
    save_normalized_member_avatar_gif(
        data_dir,
        source_gif,
        source_width,
        source_height,
        crop,
        frame_count,
    )
}

fn save_normalized_member_avatar_gif(
    data_dir: &Path,
    source_gif: &[u8],
    source_width: u32,
    source_height: u32,
    crop: MemberAvatarCrop,
    frame_count: u32,
) -> Result<ManagedMemberAvatarSummary> {
    let info = parse_gif_info(source_gif)?;
    anyhow::ensure!(
        info.width == source_width && info.height == source_height,
        "Avatar dimensions do not match"
    );
    anyhow::ensure!(
        info.frame_count == frame_count,
        "Avatar frame count does not match"
    );
    validate_gif_resources(&info)?;
    validate_crop(&crop, source_width, source_height)?;
    let frames = decode_composited_gif_frames(source_gif)?;
    let icon_gif =
        crop_gif_frames_to_icon(&frames, info.loop_count, &crop, info.width, info.height)?;
    let source_gif = normalize_gif_source_bytes(source_gif, &frames, info.loop_count)?;
    anyhow::ensure!(
        source_gif.len() <= NORMALIZED_SOURCE_BYTES && icon_gif.len() <= ICON_BYTES,
        "Avatar exceeds the managed asset limit"
    );
    let (normalized_width, normalized_height) = normalized_gif_dimensions(info.width, info.height);
    let crop = normalized_gif_crop(crop, normalized_width, normalized_height);
    let mut digest = Sha256::new();
    digest.update(&source_gif);
    digest.update(&icon_gif);
    digest.update(serde_json::to_vec(&crop)?);
    let hash = digest.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    let asset_id = Uuid::from_bytes(bytes);
    Ok(publish_managed_member_avatar_gif(
        data_dir,
        asset_id,
        &source_gif,
        &icon_gif,
        normalized_width,
        normalized_height,
        crop,
        frame_count,
    )?)
}

// Rounding either edge while resizing can move a crop that touched the old
// border just outside the new canvas. Store the same crop clipped to that canvas.
fn normalized_gif_crop(mut crop: MemberAvatarCrop, width: u32, height: u32) -> MemberAvatarCrop {
    let edge = crop.size * f64::from(width.min(height));
    let inset_x = edge / 2.0 / f64::from(width);
    let inset_y = edge / 2.0 / f64::from(height);
    crop.center_x = crop.center_x.clamp(inset_x, 1.0 - inset_x);
    crop.center_y = crop.center_y.clamp(inset_y, 1.0 - inset_y);
    crop
}

fn normalized_gif_dimensions(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= NORMALIZED_MAXIMUM_EDGE {
        return (width, height);
    }
    let scale = f64::from(NORMALIZED_MAXIMUM_EDGE) / f64::from(longest);
    (
        (f64::from(width) * scale).round().max(1.0) as u32,
        (f64::from(height) * scale).round().max(1.0) as u32,
    )
}

fn validate_normalized_png(bytes: &[u8], width: u32, height: u32, maximum: u32) -> Result<()> {
    anyhow::ensure!(
        width > 0 && height > 0 && width <= maximum && height <= maximum,
        "Avatar dimensions are invalid"
    );
    reject_animated_png(bytes)?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_image_width = Some(maximum);
    limits.max_image_height = Some(maximum);
    limits.max_alloc = Some(MAXIMUM_DECODE_ALLOCATION);
    reader.limits(limits);
    let image = reader.decode()?;
    anyhow::ensure!(
        image.dimensions() == (width, height),
        "Avatar dimensions do not match"
    );
    Ok(())
}

fn validate_crop(crop: &MemberAvatarCrop, width: u32, height: u32) -> Result<()> {
    let half = crop.size * f64::from(width.min(height)) / 2.0;
    anyhow::ensure!(
        crop.center_x.is_finite()
            && crop.center_y.is_finite()
            && crop.size.is_finite()
            && (0.12..=1.0).contains(&crop.size)
            && (half / f64::from(width)..=1.0 - half / f64::from(width)).contains(&crop.center_x)
            && (half / f64::from(height)..=1.0 - half / f64::from(height)).contains(&crop.center_y),
        "Avatar crop is out of bounds"
    );
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedMemberAvatarRendition {
    pub base64: String,
    pub media_type: &'static str,
    pub width: u32,
    pub height: u32,
    pub crop: MemberAvatarCrop,
}

/// Reads only validated managed references and fixed manifest file names.
pub fn read_managed_member_avatar(
    data_dir: &Path,
    avatar_ref: &str,
    portrait: bool,
) -> Result<Option<ManagedMemberAvatarRendition>> {
    use base64::Engine;
    let MemberAvatarReference::Managed(asset_id) = parse_member_avatar_ref(avatar_ref)? else {
        return Ok(None);
    };
    let root = data_dir.join("member-avatars");
    let directory = root.join(asset_id.hyphenated().to_string());
    if !path_exists(&directory)? {
        return Ok(None);
    }
    for path in [&root, &directory] {
        let metadata = fs::symlink_metadata(path)?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Avatar directory is invalid"
        );
    }
    let manifest: AvatarManifestV1 = serde_json::from_slice(&read_existing_regular_file(
        &directory.join("manifest.json"),
        MANIFEST_BYTES,
    )?)?;
    anyhow::ensure!(
        matches!(manifest.schema_version, 1 | 2)
            && manifest.asset_id == asset_id.hyphenated().to_string(),
        "Avatar manifest is invalid"
    );
    let gif = manifest.schema_version == 2;
    validate_crop(
        &manifest.icon_crop,
        manifest.source.file.width,
        manifest.source.file.height,
    )?;
    let (file, name, maximum) = if portrait {
        (
            &manifest.source.file,
            if gif { "source.gif" } else { "source.png" },
            NORMALIZED_SOURCE_BYTES,
        )
    } else {
        (
            &manifest.icon,
            if gif { "icon-192.gif" } else { "icon-192.png" },
            ICON_BYTES,
        )
    };
    anyhow::ensure!(
        file.file == name && file.media_type == if gif { "image/gif" } else { "image/png" },
        "Avatar rendition is invalid"
    );
    let bytes = read_existing_regular_file(&directory.join(name), maximum)?;
    anyhow::ensure!(
        file.byte_length == bytes.len() && sha256(&bytes) == file.sha256,
        "Avatar content changed"
    );
    if gif {
        validate_normalized_gif(
            &bytes,
            file.width,
            file.height,
            if portrait {
                NORMALIZED_MAXIMUM_EDGE
            } else {
                ICON_EDGE
            },
            file.frame_count,
        )?;
    } else {
        validate_normalized_png(
            &bytes,
            file.width,
            file.height,
            if portrait {
                NORMALIZED_MAXIMUM_EDGE
            } else {
                ICON_EDGE
            },
        )?;
    }
    Ok(Some(ManagedMemberAvatarRendition {
        base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        media_type: if gif { "image/gif" } else { "image/png" },
        width: file.width,
        height: file.height,
        crop: manifest.icon_crop,
    }))
}

/// Channels consume a PNG icon. Select the first composited animation frame
/// here so neither the Agent nor a channel adapter needs frame-selection logic.
pub fn read_managed_member_avatar_static_icon(
    data_dir: &Path,
    avatar_ref: &str,
) -> Result<Option<ManagedMemberAvatarRendition>> {
    use base64::Engine;
    let Some(mut image) = read_managed_member_avatar(data_dir, avatar_ref, false)? else {
        return Ok(None);
    };
    if image.media_type == "image/gif" {
        let bytes = base64::engine::general_purpose::STANDARD.decode(&image.base64)?;
        let frames = decode_composited_gif_frames(&bytes)?;
        let frame = frames
            .into_iter()
            .next()
            .ok_or_else(invalid_gif_structure)?;
        let png = encode_png(&DynamicImage::ImageRgba8(frame.image))?;
        image.base64 = base64::engine::general_purpose::STANDARD.encode(png);
        image.media_type = "image/png";
    }
    Ok(Some(image))
}

fn publish_managed_member_avatar(
    data_dir: &Path,
    asset_id: Uuid,
    source_png: &[u8],
    icon_png: &[u8],
    source_width: u32,
    source_height: u32,
    crop: MemberAvatarCrop,
) -> std::result::Result<ManagedMemberAvatarSummary, MemberAvatarImportError> {
    publish_managed_member_avatar_with(
        data_dir,
        asset_id,
        ManagedAvatarMediaKind::Png,
        source_png,
        icon_png,
        source_width,
        source_height,
        0,
        crop,
    )
}

fn publish_managed_member_avatar_gif(
    data_dir: &Path,
    asset_id: Uuid,
    source_gif: &[u8],
    icon_gif: &[u8],
    source_width: u32,
    source_height: u32,
    crop: MemberAvatarCrop,
    frame_count: u32,
) -> std::result::Result<ManagedMemberAvatarSummary, MemberAvatarImportError> {
    publish_managed_member_avatar_with(
        data_dir,
        asset_id,
        ManagedAvatarMediaKind::Gif,
        source_gif,
        icon_gif,
        source_width,
        source_height,
        frame_count,
        crop,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ManagedAvatarMediaKind {
    Png,
    Gif,
}

fn publish_managed_member_avatar_with(
    data_dir: &Path,
    asset_id: Uuid,
    kind: ManagedAvatarMediaKind,
    source_bytes: &[u8],
    icon_bytes: &[u8],
    source_width: u32,
    source_height: u32,
    frame_count: u32,
    crop: MemberAvatarCrop,
) -> std::result::Result<ManagedMemberAvatarSummary, MemberAvatarImportError> {
    let asset_id = asset_id.hyphenated().to_string();
    let gif = kind == ManagedAvatarMediaKind::Gif;
    let (source_file, icon_file) = if gif {
        ("source.gif", "icon-192.gif")
    } else {
        ("source.png", "icon-192.png")
    };
    let (source_manifest, icon_manifest) = if gif {
        (
            gif_manifest_file(
                source_file,
                source_width,
                source_height,
                frame_count,
                source_bytes,
            ),
            gif_manifest_file(icon_file, ICON_EDGE, ICON_EDGE, frame_count, icon_bytes),
        )
    } else {
        (
            manifest_file(source_file, source_width, source_height, source_bytes),
            manifest_file(icon_file, ICON_EDGE, ICON_EDGE, icon_bytes),
        )
    };
    let root = data_dir.join("member-avatars");
    ensure_private_directory(&root)?;
    let final_directory = root.join(&asset_id);

    if path_exists(&final_directory)? {
        if matches!(
            existing_asset_matches(
                &final_directory,
                &asset_id,
                &source_manifest,
                &icon_manifest,
                &crop,
            ),
            Ok(true)
        ) {
            return Ok(ManagedMemberAvatarSummary {
                avatar_ref: format!("rovai://member-avatar/managed/{asset_id}"),
                source_width,
                source_height,
                crop,
            });
        }
        return Err(MemberAvatarImportError::conflict());
    }

    let temporary_directory = root.join(format!(".tmp-{asset_id}"));
    remove_scoped_temporary_path(&temporary_directory)?;
    create_private_directory(&temporary_directory)?;
    let write_result = (|| {
        write_private_file(&temporary_directory.join(source_file), source_bytes)?;
        write_private_file(&temporary_directory.join(icon_file), icon_bytes)?;
        let manifest = AvatarManifestV1 {
            schema_version: if gif { 2 } else { 1 },
            asset_id: asset_id.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            source: AvatarManifestSource {
                file: source_manifest,
                orientation_normalized: true,
                metadata_stripped: true,
            },
            icon: icon_manifest,
            icon_crop: crop.clone(),
        };
        let mut manifest_bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|_| MemberAvatarImportError::invalid("Avatar manifest could not be formed"))?;
        manifest_bytes.push(b'\n');
        if manifest_bytes.len() > MANIFEST_BYTES {
            return Err(MemberAvatarImportError::invalid(
                "Avatar manifest exceeds the managed asset limit",
            ));
        }
        write_private_file(&temporary_directory.join("manifest.json"), &manifest_bytes)?;
        sync_directory(&temporary_directory)?;
        fs::rename(&temporary_directory, &final_directory)
            .map_err(|_| MemberAvatarImportError::invalid("Avatar asset could not be published"))?;
        sync_directory(&root)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = remove_scoped_temporary_path(&temporary_directory);
    }
    write_result?;

    Ok(ManagedMemberAvatarSummary {
        avatar_ref: format!("rovai://member-avatar/managed/{asset_id}"),
        source_width,
        source_height,
        crop,
    })
}

fn validate_decoded_dimensions(
    width: u32,
    height: u32,
) -> std::result::Result<(), MemberAvatarImportError> {
    if width < MINIMUM_DECODED_EDGE || height < MINIMUM_DECODED_EDGE {
        return Err(MemberAvatarImportError::invalid(
            "Avatar image must be at least 256 by 256 pixels",
        ));
    }
    if width > MAXIMUM_DECODED_EDGE
        || height > MAXIMUM_DECODED_EDGE
        || u64::from(width).saturating_mul(u64::from(height)) > MAXIMUM_DECODED_PIXELS
    {
        return Err(MemberAvatarImportError::invalid(
            "Avatar image exceeds the decoded resource limit",
        ));
    }
    Ok(())
}

fn reject_animated_png(bytes: &[u8]) -> std::result::Result<(), MemberAvatarImportError> {
    if inspect_upload_png(bytes)? != bytes.len() {
        return Err(MemberAvatarImportError::invalid(
            "Avatar PNG has trailing data",
        ));
    }
    Ok(())
}

fn inspect_upload_png(bytes: &[u8]) -> std::result::Result<usize, MemberAvatarImportError> {
    const PNG_SIGNATURE_BYTES: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if !bytes.starts_with(PNG_SIGNATURE_BYTES) {
        return Err(MemberAvatarImportError::invalid(
            "Avatar must be a PNG or JPEG image",
        ));
    }
    let mut offset = PNG_SIGNATURE_BYTES.len();
    while offset < bytes.len() {
        let header_end = offset
            .checked_add(8)
            .ok_or_else(|| MemberAvatarImportError::invalid("Avatar PNG structure is invalid"))?;
        if header_end > bytes.len() {
            return Err(MemberAvatarImportError::invalid(
                "Avatar PNG structure is invalid",
            ));
        }
        let length = u32::from_be_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .map_err(|_| MemberAvatarImportError::invalid("Avatar PNG structure is invalid"))?,
        ) as usize;
        let chunk_type = &bytes[offset + 4..header_end];
        let chunk_end = header_end
            .checked_add(length)
            .and_then(|value| value.checked_add(4))
            .ok_or_else(|| MemberAvatarImportError::invalid("Avatar PNG structure is invalid"))?;
        if chunk_end > bytes.len() {
            return Err(MemberAvatarImportError::invalid(
                "Avatar PNG structure is invalid",
            ));
        }
        if chunk_type == b"acTL" {
            return Err(MemberAvatarImportError::invalid(
                "Animated PNG avatars are not supported",
            ));
        }
        if chunk_type == b"IEND" {
            // IEND must be empty. Bytes after it are vendor metadata that
            // real-world decoders ignore and that the re-encode drops on disk.
            if length != 0 {
                return Err(MemberAvatarImportError::invalid(
                    "Avatar PNG structure is invalid",
                ));
            }
            return Ok(chunk_end);
        }
        offset = chunk_end;
    }
    Err(MemberAvatarImportError::invalid(
        "Avatar PNG structure is invalid",
    ))
}

// ---------------------------------------------------------------------------
// GIF member-avatar support
//
// Animated GIF is the only dynamic member-avatar format. APNG and WebP stay
// unsupported. A GIF is decoded frame-by-frame onto the
// logical canvas (honouring each frame's disposal), the same crop geometry is
// then applied to every frame, and the result is re-encoded as an animated GIF
// so frame order, per-frame delay and the Netscape loop count survive.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct GifFileInfo {
    width: u32,
    height: u32,
    frame_count: u32,
    loop_count: Option<u16>,
    rendering_blocks: Vec<(usize, usize)>,
}

struct CompositedGifFrame {
    image: RgbaImage,
    /// Per-frame delay in centiseconds (GIF's native unit).
    delay_centiseconds: u16,
}

fn parse_gif_info(bytes: &[u8]) -> std::result::Result<GifFileInfo, MemberAvatarImportError> {
    const GIF_SIGNATURES: [&[u8; 6]; 2] = [b"GIF87a", b"GIF89a"];
    if bytes.len() < 13 {
        return Err(MemberAvatarImportError::invalid(
            "Avatar GIF structure is invalid",
        ));
    }
    if !GIF_SIGNATURES
        .iter()
        .any(|signature| bytes.starts_with(signature.as_slice()))
    {
        return Err(MemberAvatarImportError::invalid(
            "Avatar must be a PNG, JPEG or GIF image",
        ));
    }
    let width = u32::from(u16::from_le_bytes([bytes[6], bytes[7]]));
    let height = u32::from(u16::from_le_bytes([bytes[8], bytes[9]]));
    if width == 0 || height == 0 {
        return Err(MemberAvatarImportError::invalid(
            "Avatar GIF dimensions are invalid",
        ));
    }
    let packed = bytes[10];
    let global_table_bytes = if packed & 0x80 != 0 {
        3usize << ((packed & 0x07) as usize + 1)
    } else {
        0
    };
    let mut offset = 13usize.saturating_add(global_table_bytes);
    if offset > bytes.len() {
        return Err(MemberAvatarImportError::invalid(
            "Avatar GIF structure is truncated",
        ));
    }
    let mut frame_count = 0u32;
    let mut loop_count = None;
    let mut saw_end = false;
    let mut rendering_blocks = vec![(0, offset)];
    while offset < bytes.len() {
        let start = offset;
        let mut keep = true;
        match bytes[offset] {
            0x3b => {
                rendering_blocks.push((offset, offset + 1));
                saw_end = true;
                break;
            }
            0x21 => {
                // Extension introducer.
                offset = offset
                    .checked_add(1)
                    .ok_or_else(|| invalid_gif_structure())?;
                let label = *bytes.get(offset).ok_or_else(|| invalid_gif_structure())?;
                keep = label == 0xf9;
                offset = offset
                    .checked_add(1)
                    .ok_or_else(|| invalid_gif_structure())?;
                if label == 0xff {
                    // Application extension: may carry the Netscape loop count.
                    let size = *bytes.get(offset).ok_or_else(|| invalid_gif_structure())? as usize;
                    offset = offset
                        .checked_add(1)
                        .ok_or_else(|| invalid_gif_structure())?;
                    let block = bytes
                        .get(offset..offset.saturating_add(size))
                        .ok_or_else(|| invalid_gif_structure())?;
                    offset = offset
                        .checked_add(size)
                        .ok_or_else(|| invalid_gif_structure())?;
                    if size != 11 {
                        return Err(invalid_gif_structure());
                    }
                    if block == b"NETSCAPE2.0" || block == b"ANIMEXTS1.0" {
                        keep = true;
                        // Peek the loop-count sub-block; skip_gif_sub_blocks
                        // consumes it (and the terminator) afterwards.
                        if let Some(sub) = bytes.get(offset).copied().map(usize::from) {
                            if sub == 3 {
                                let payload = bytes
                                    .get(offset + 1..offset.saturating_add(4))
                                    .ok_or_else(|| invalid_gif_structure())?;
                                if payload[0] == 0x01 {
                                    loop_count = Some(u16::from_le_bytes([payload[1], payload[2]]));
                                }
                            }
                        }
                    }
                }
                offset = skip_gif_sub_blocks(bytes, offset)?;
            }
            0x2c => {
                let descriptor = bytes
                    .get(offset + 1..offset + 10)
                    .ok_or_else(invalid_gif_structure)?;
                let left = u32::from(u16::from_le_bytes([descriptor[0], descriptor[1]]));
                let top = u32::from(u16::from_le_bytes([descriptor[2], descriptor[3]]));
                let frame_width = u32::from(u16::from_le_bytes([descriptor[4], descriptor[5]]));
                let frame_height = u32::from(u16::from_le_bytes([descriptor[6], descriptor[7]]));
                if frame_width == 0
                    || frame_height == 0
                    || left + frame_width > width
                    || top + frame_height > height
                {
                    return Err(MemberAvatarImportError::invalid(
                        "Avatar GIF frame exceeds the canvas bounds",
                    ));
                }
                // Image descriptor: one animated frame.
                frame_count = frame_count
                    .checked_add(1)
                    .ok_or_else(|| invalid_gif_structure())?;
                if frame_count > MAXIMUM_GIF_FRAMES {
                    return Err(MemberAvatarImportError::invalid(
                        "Animated avatar exceeds the frame limit",
                    ));
                }
                offset = offset
                    .checked_add(9)
                    .ok_or_else(|| invalid_gif_structure())?;
                let packed = *bytes.get(offset).ok_or_else(|| invalid_gif_structure())?;
                offset = offset
                    .checked_add(1)
                    .ok_or_else(|| invalid_gif_structure())?;
                let local_table_bytes = if packed & 0x80 != 0 {
                    3usize << ((packed & 0x07) as usize + 1)
                } else {
                    0
                };
                offset = offset
                    .checked_add(local_table_bytes)
                    .and_then(|value| value.checked_add(1)) // LZW min code size
                    .ok_or_else(|| invalid_gif_structure())?;
                if offset > bytes.len() {
                    return Err(MemberAvatarImportError::invalid(
                        "Avatar GIF structure is truncated",
                    ));
                }
                offset = skip_gif_sub_blocks(bytes, offset)?;
            }
            _ => {
                return Err(MemberAvatarImportError::invalid(
                    "Avatar GIF structure is invalid",
                ));
            }
        }
        if keep {
            rendering_blocks.push((start, offset));
        }
    }
    if !saw_end {
        return Err(invalid_gif_structure());
    }
    if frame_count == 0 {
        return Err(MemberAvatarImportError::invalid(
            "Avatar GIF contains no frames",
        ));
    }
    Ok(GifFileInfo {
        width,
        height,
        frame_count,
        loop_count,
        rendering_blocks,
    })
}

fn skip_gif_sub_blocks(
    bytes: &[u8],
    mut offset: usize,
) -> std::result::Result<usize, MemberAvatarImportError> {
    loop {
        let size = *bytes.get(offset).ok_or_else(|| invalid_gif_structure())? as usize;
        offset = offset
            .checked_add(1)
            .and_then(|value| value.checked_add(size))
            .ok_or_else(|| invalid_gif_structure())?;
        if size == 0 {
            return Ok(offset);
        }
        if offset > bytes.len() {
            return Err(MemberAvatarImportError::invalid(
                "Avatar GIF structure is truncated",
            ));
        }
    }
}

fn invalid_gif_structure() -> MemberAvatarImportError {
    MemberAvatarImportError::invalid("Avatar GIF structure is invalid")
}

/// Builds a GIF decoder with RGBA frame output and an explicit decode memory
/// bound, so frame buffers stay within `MAXIMUM_DECODE_ALLOCATION`.
fn gif_decoder(
    bytes: &[u8],
) -> std::result::Result<GifDecoder<Cursor<&[u8]>>, MemberAvatarImportError> {
    let mut options = DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    options.check_frame_consistency(true);
    options.set_memory_limit(gif::MemoryLimit::Bytes(
        std::num::NonZeroU64::new(MAXIMUM_DECODE_ALLOCATION)
            .ok_or_else(|| invalid_gif_structure())?,
    ));
    options
        .read_info(Cursor::new(bytes))
        .map_err(|_| MemberAvatarImportError::invalid("Avatar GIF could not be decoded"))
}

fn validate_gif_file_info(info: &GifFileInfo) -> std::result::Result<(), MemberAvatarImportError> {
    validate_decoded_dimensions(info.width, info.height)?;
    validate_gif_resources(info)
}

fn validate_gif_resources(info: &GifFileInfo) -> std::result::Result<(), MemberAvatarImportError> {
    if info.width > MAXIMUM_DECODED_EDGE
        || info.height > MAXIMUM_DECODED_EDGE
        || u64::from(info.width) * u64::from(info.height) > MAXIMUM_DECODED_PIXELS
    {
        return Err(MemberAvatarImportError::invalid(
            "Avatar GIF exceeds the decoded resource limit",
        ));
    }
    if info.frame_count > MAXIMUM_GIF_FRAMES {
        return Err(MemberAvatarImportError::invalid(
            "Animated avatar exceeds the frame limit",
        ));
    }
    let cumulative = u64::from(info.width)
        .saturating_mul(u64::from(info.height))
        .saturating_mul(u64::from(info.frame_count));
    if cumulative > MAXIMUM_GIF_CUMULATIVE_DECODED_PIXELS {
        return Err(MemberAvatarImportError::invalid(
            "Animated avatar exceeds the cumulative decode resource limit",
        ));
    }
    Ok(())
}

/// Decodes every GIF frame composited onto the logical canvas with the same
/// playback rules a browser uses, honouring frame disposal. Also enforces the
/// frame count and cumulative decode resource limits.
fn decode_composited_gif_frames(
    bytes: &[u8],
) -> std::result::Result<Vec<CompositedGifFrame>, MemberAvatarImportError> {
    let info = parse_gif_info(bytes)?;
    validate_gif_resources(&info)?;
    let mut decoder = gif_decoder(bytes)?;
    let canvas_width = u32::from(decoder.width());
    let canvas_height = u32::from(decoder.height());
    if canvas_width != info.width || canvas_height != info.height {
        return Err(MemberAvatarImportError::invalid(
            "Avatar GIF dimensions are inconsistent",
        ));
    }
    let mut canvas = RgbaImage::new(canvas_width, canvas_height);
    let mut frames = Vec::new();
    let mut decoded_frames = 0u32;
    let mut cumulative_pixels: u64 = 0;
    while let Some(frame) = decoder
        .read_next_frame()
        .map_err(|_| MemberAvatarImportError::invalid("Avatar GIF could not be decoded"))?
    {
        decoded_frames = decoded_frames
            .checked_add(1)
            .ok_or_else(|| invalid_gif_structure())?;
        if decoded_frames > MAXIMUM_GIF_FRAMES {
            return Err(MemberAvatarImportError::invalid(
                "Animated avatar exceeds the frame limit",
            ));
        }
        cumulative_pixels = cumulative_pixels
            .saturating_add(u64::from(canvas_width).saturating_mul(u64::from(canvas_height)));
        if cumulative_pixels > MAXIMUM_GIF_CUMULATIVE_DECODED_PIXELS {
            return Err(MemberAvatarImportError::invalid(
                "Animated avatar exceeds the cumulative decode resource limit",
            ));
        }
        let left = u32::from(frame.left);
        let top = u32::from(frame.top);
        let frame_image = RgbaImage::from_raw(
            u32::from(frame.width),
            u32::from(frame.height),
            frame.buffer.to_vec(),
        )
        .ok_or_else(|| invalid_gif_structure())?;
        let state_before = canvas.clone();
        blit_gif_frame(&mut canvas, &frame_image, left, top);
        frames.push(CompositedGifFrame {
            image: canvas.clone(),
            delay_centiseconds: frame.delay,
        });
        match frame.dispose {
            DisposalMethod::Background => {
                clear_gif_rect(
                    &mut canvas,
                    left,
                    top,
                    u32::from(frame.width),
                    u32::from(frame.height),
                );
            }
            DisposalMethod::Previous => {
                canvas = state_before;
            }
            _ => {}
        }
    }
    if frames.is_empty() {
        return Err(MemberAvatarImportError::invalid(
            "Avatar GIF contains no frames",
        ));
    }
    Ok(frames)
}

fn blit_gif_frame(canvas: &mut RgbaImage, frame: &RgbaImage, left: u32, top: u32) {
    for (y, row) in frame.enumerate_rows() {
        for (x, _, pixel) in row {
            if pixel.0[3] != 0 {
                canvas.put_pixel(left + x, top + y, *pixel);
            }
        }
    }
}

fn clear_gif_rect(canvas: &mut RgbaImage, left: u32, top: u32, width: u32, height: u32) {
    for y in top..top.saturating_add(height).min(canvas.height()) {
        for x in left..left.saturating_add(width).min(canvas.width()) {
            canvas.put_pixel(x, y, image::Rgba([0, 0, 0, 0]));
        }
    }
}

fn encode_animated_gif(
    frames: &[CompositedGifFrame],
    loop_count: Option<u16>,
) -> std::result::Result<Vec<u8>, MemberAvatarImportError> {
    let (width, height) = frames
        .first()
        .map(|frame| frame.image.dimensions())
        .ok_or_else(|| invalid_gif_structure())?;
    let mut output = Cursor::new(Vec::new());
    {
        let mut encoder = GifEncoder::new(&mut output, width as u16, height as u16, &[])
            .map_err(|_| MemberAvatarImportError::invalid("Avatar GIF could not be normalized"))?;
        if let Some(count) = loop_count {
            encoder
                .set_repeat(if count == 0 {
                    Repeat::Infinite
                } else {
                    Repeat::Finite(count)
                })
                .map_err(|_| {
                    MemberAvatarImportError::invalid("Avatar GIF could not be normalized")
                })?;
        }
        for frame in frames {
            let (frame_width, frame_height) = frame.image.dimensions();
            let mut buffer = frame.image.to_vec();
            let mut gif_frame =
                GifFrame::from_rgba_speed(frame_width as u16, frame_height as u16, &mut buffer, 10);
            gif_frame.delay = frame.delay_centiseconds;
            // Every frame is a complete composited canvas. Clear it before the
            // next frame so transparent pixels cannot retain old content.
            gif_frame.dispose = DisposalMethod::Background;
            encoder.write_frame(&gif_frame).map_err(|_| {
                MemberAvatarImportError::invalid("Avatar GIF could not be normalized")
            })?;
        }
    }
    Ok(output.into_inner())
}

/// Resizes every composited frame to at most `maximum_edge` on the longest side
/// and re-encodes the animation.
fn resize_gif_frames(
    frames: &[CompositedGifFrame],
    loop_count: Option<u16>,
    maximum_edge: u32,
) -> std::result::Result<Vec<u8>, MemberAvatarImportError> {
    let (width, height) = frames
        .first()
        .map(|frame| frame.image.dimensions())
        .ok_or_else(|| invalid_gif_structure())?;
    let longest = width.max(height);
    if longest <= maximum_edge {
        return Ok(encode_animated_gif(frames, loop_count)?);
    }
    let scale = f64::from(maximum_edge) / f64::from(longest);
    let target_width = (f64::from(width) * scale).round().max(1.0) as u32;
    let target_height = (f64::from(height) * scale).round().max(1.0) as u32;
    let mut resized = Vec::with_capacity(frames.len());
    for frame in frames {
        let image = image::imageops::resize(
            &frame.image,
            target_width,
            target_height,
            image::imageops::FilterType::Lanczos3,
        );
        resized.push(CompositedGifFrame {
            image,
            delay_centiseconds: frame.delay_centiseconds,
        });
    }
    encode_animated_gif(&resized, loop_count)
}

/// Applies the shared crop geometry to every frame and scales it to the
/// 192×192 icon, preserving frame order, per-frame delay and loop count.
fn crop_gif_frames_to_icon(
    frames: &[CompositedGifFrame],
    loop_count: Option<u16>,
    crop: &MemberAvatarCrop,
    width: u32,
    height: u32,
) -> std::result::Result<Vec<u8>, MemberAvatarImportError> {
    validate_crop(crop, width, height)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar crop is out of bounds"))?;
    let edge = (crop.size * f64::from(width.min(height))).round().max(1.0) as u32;
    let x = (crop.center_x * f64::from(width) - f64::from(edge) / 2.0)
        .round()
        .max(0.0) as u32;
    let y = (crop.center_y * f64::from(height) - f64::from(edge) / 2.0)
        .round()
        .max(0.0) as u32;
    let x = x.min(width.saturating_sub(edge));
    let y = y.min(height.saturating_sub(edge));
    let mut icon_frames = Vec::with_capacity(frames.len());
    for frame in frames {
        let cropped = image::imageops::crop_imm(&frame.image, x, y, edge, edge).to_image();
        let icon = image::imageops::resize(
            &cropped,
            ICON_EDGE,
            ICON_EDGE,
            image::imageops::FilterType::Lanczos3,
        );
        icon_frames.push(CompositedGifFrame {
            image: icon,
            delay_centiseconds: frame.delay_centiseconds,
        });
    }
    encode_animated_gif(&icon_frames, loop_count)
}

/// Validates a stored normalized GIF rendition (logical dimensions, frame count
/// and cumulative decode resources) against the expected manifest values.
fn validate_normalized_gif(
    bytes: &[u8],
    width: u32,
    height: u32,
    maximum_edge: u32,
    expected_frame_count: Option<u32>,
) -> std::result::Result<(), MemberAvatarImportError> {
    if width == 0 || height == 0 || width > maximum_edge || height > maximum_edge {
        return Err(MemberAvatarImportError::invalid(
            "Avatar dimensions are invalid",
        ));
    }
    let info = parse_gif_info(bytes)?;
    validate_gif_resources(&info)?;
    if info.width != width || info.height != height {
        return Err(MemberAvatarImportError::invalid(
            "Avatar dimensions do not match",
        ));
    }
    if expected_frame_count.is_some_and(|expected| expected != info.frame_count) {
        return Err(MemberAvatarImportError::invalid(
            "Avatar frame count does not match",
        ));
    }
    let mut decoder = gif_decoder(bytes)?;
    while let Some(frame) = decoder
        .read_next_frame()
        .map_err(|_| MemberAvatarImportError::invalid("Avatar GIF could not be decoded"))?
    {
        let _ = frame;
    }
    Ok(())
}

fn gif_manifest_file(
    file: &str,
    width: u32,
    height: u32,
    frame_count: u32,
    bytes: &[u8],
) -> AvatarManifestFile {
    AvatarManifestFile {
        file: file.to_string(),
        media_type: "image/gif".to_string(),
        width,
        height,
        byte_length: bytes.len(),
        sha256: sha256(bytes),
        frame_count: Some(frame_count),
    }
}

fn default_avatar_crop(width: u32, height: u32) -> (u32, u32, u32) {
    let edge = width.min(height);
    let x = (width - edge) / 2;
    let available_y = height - edge;
    let preferred_y = ((f64::from(height) * 0.05).round() as u32).min(available_y);
    (x, preferred_y, edge)
}

fn encode_png(image: &DynamicImage) -> std::result::Result<Vec<u8>, MemberAvatarImportError> {
    let mut output = Cursor::new(Vec::new());
    image
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar image could not be normalized"))?;
    Ok(output.into_inner())
}

fn manifest_file(file: &str, width: u32, height: u32, bytes: &[u8]) -> AvatarManifestFile {
    AvatarManifestFile {
        file: file.to_string(),
        media_type: "image/png".to_string(),
        width,
        height,
        byte_length: bytes.len(),
        sha256: sha256(bytes),
        frame_count: None,
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_selected_avatar(path: &Path) -> std::result::Result<Vec<u8>, MemberAvatarImportError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar file is not readable"))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > SELECTED_FILE_BYTES
    {
        return Err(MemberAvatarImportError::invalid(
            "Avatar path must be a regular file no larger than 10 MiB",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar file is not readable"))?;
    let opened_metadata = file
        .metadata()
        .map_err(|_| MemberAvatarImportError::invalid("Avatar file is not readable"))?;
    if !opened_metadata.is_file() || opened_metadata.len() > SELECTED_FILE_BYTES {
        return Err(MemberAvatarImportError::invalid(
            "Avatar path must be a regular file no larger than 10 MiB",
        ));
    }
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    file.take(SELECTED_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar file is not readable"))?;
    if bytes.len() as u64 > SELECTED_FILE_BYTES {
        return Err(MemberAvatarImportError::invalid(
            "Avatar path must be a regular file no larger than 10 MiB",
        ));
    }
    Ok(bytes)
}

fn ensure_private_directory(directory: &Path) -> std::result::Result<(), MemberAvatarImportError> {
    fs::create_dir_all(directory)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar asset directory is unavailable"))?;
    let metadata = fs::symlink_metadata(directory)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar asset directory is unavailable"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(MemberAvatarImportError::invalid(
            "Avatar asset directory is unavailable",
        ));
    }
    #[cfg(unix)]
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .map_err(|_| MemberAvatarImportError::invalid("Avatar asset directory is unavailable"))?;
    Ok(())
}

fn create_private_directory(directory: &Path) -> std::result::Result<(), MemberAvatarImportError> {
    #[cfg(unix)]
    let result = fs::DirBuilder::new()
        .recursive(false)
        .mode(0o700)
        .create(directory);
    #[cfg(not(unix))]
    let result = fs::create_dir(directory);
    result.map_err(|_| MemberAvatarImportError::invalid("Avatar asset could not be staged"))
}

fn write_private_file(
    path: &Path,
    bytes: &[u8],
) -> std::result::Result<(), MemberAvatarImportError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar asset could not be written"))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| MemberAvatarImportError::invalid("Avatar asset could not be written"))
}

fn sync_directory(directory: &Path) -> std::result::Result<(), MemberAvatarImportError> {
    #[cfg(windows)]
    let _ = directory;
    #[cfg(not(windows))]
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|_| MemberAvatarImportError::invalid("Avatar asset could not be committed"))?;
    Ok(())
}

fn path_exists(path: &Path) -> std::result::Result<bool, MemberAvatarImportError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(MemberAvatarImportError::invalid(
            "Avatar asset directory is unavailable",
        )),
    }
}

fn remove_scoped_temporary_path(path: &Path) -> std::result::Result<(), MemberAvatarImportError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {
            return Err(MemberAvatarImportError::invalid(
                "Avatar temporary asset could not be inspected",
            ));
        }
    };
    let result = if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(path)
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        return Err(MemberAvatarImportError::invalid(
            "Avatar temporary asset has an unsupported type",
        ));
    };
    result
        .map_err(|_| MemberAvatarImportError::invalid("Avatar temporary asset could not be reset"))
}

fn existing_asset_matches(
    directory: &Path,
    asset_id: &str,
    expected_source: &AvatarManifestFile,
    expected_icon: &AvatarManifestFile,
    expected_crop: &MemberAvatarCrop,
) -> std::result::Result<bool, MemberAvatarImportError> {
    let metadata = fs::symlink_metadata(directory)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar asset could not be inspected"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(false);
    }
    let manifest_path = directory.join("manifest.json");
    let manifest_bytes = read_existing_regular_file(&manifest_path, MANIFEST_BYTES)?;
    let Ok(manifest) = serde_json::from_slice::<AvatarManifestV1>(&manifest_bytes) else {
        return Ok(false);
    };
    if !matches!(manifest.schema_version, 1 | 2)
        || manifest.asset_id != asset_id
        || manifest.source.file.sha256 != expected_source.sha256
        || manifest.source.file.width != expected_source.width
        || manifest.source.file.height != expected_source.height
        || manifest.icon.sha256 != expected_icon.sha256
        || manifest.source.file.file != expected_source.file
        || manifest.icon.file != expected_icon.file
        || &manifest.icon_crop != expected_crop
    {
        return Ok(false);
    }
    let source = read_existing_regular_file(
        &directory.join(&manifest.source.file.file),
        NORMALIZED_SOURCE_BYTES,
    )?;
    let icon = read_existing_regular_file(&directory.join(&manifest.icon.file), ICON_BYTES)?;
    Ok(sha256(&source) == expected_source.sha256 && sha256(&icon) == expected_icon.sha256)
}

fn read_existing_regular_file(
    path: &Path,
    maximum_bytes: usize,
) -> std::result::Result<Vec<u8>, MemberAvatarImportError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar asset could not be inspected"))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > maximum_bytes as u64
    {
        return Err(MemberAvatarImportError::invalid(
            "Avatar asset could not be inspected",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar asset could not be inspected"))?;
    let opened_metadata = file
        .metadata()
        .map_err(|_| MemberAvatarImportError::invalid("Avatar asset could not be inspected"))?;
    if !opened_metadata.is_file() || opened_metadata.len() > maximum_bytes as u64 {
        return Err(MemberAvatarImportError::invalid(
            "Avatar asset could not be inspected",
        ));
    }
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    BufReader::new(file)
        .take(maximum_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| MemberAvatarImportError::invalid("Avatar asset could not be inspected"))?;
    if bytes.len() > maximum_bytes {
        return Err(MemberAvatarImportError::invalid(
            "Avatar asset could not be inspected",
        ));
    }
    Ok(bytes)
}

fn invalid_avatar_ref() -> anyhow::Error {
    anyhow::anyhow!(
        "AgentProfile avatarRef must be null or a supported rovai://member-avatar reference"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use std::path::PathBuf;

    use serde_json::Value;

    fn temporary_directory() -> PathBuf {
        let directory = std::env::temp_dir().join(format!("member-avatar-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn write_test_png(path: &Path, color: [u8; 4]) {
        let image =
            DynamicImage::ImageRgba8(image::ImageBuffer::from_pixel(400, 500, image::Rgba(color)));
        fs::write(path, encode_png(&image).unwrap()).unwrap();
    }

    fn write_bytes(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
    }

    fn solid_rgba(width: u32, height: u32, color: [u8; 4]) -> RgbaImage {
        image::ImageBuffer::from_pixel(width, height, image::Rgba(color))
    }

    fn write_test_gif(path: &Path, frames: &[RgbaImage]) {
        let file = fs::File::create(path).unwrap();
        let (width, height) = frames[0].dimensions();
        let mut encoder = gif::Encoder::new(file, width as u16, height as u16, &[]).unwrap();
        encoder.set_repeat(gif::Repeat::Infinite).unwrap();
        for frame in frames {
            let (frame_width, frame_height) = frame.dimensions();
            let mut buffer = frame.to_vec();
            let mut gif_frame = gif::Frame::from_rgba_speed(
                frame_width as u16,
                frame_height as u16,
                &mut buffer,
                10,
            );
            gif_frame.delay = 10;
            encoder.write_frame(&gif_frame).unwrap();
        }
    }

    /// Structure-only GIF used to exercise the block-walking validator without
    /// requiring a decodable LZW image stream.
    fn structure_gif(width: u16, height: u16, frame_count: usize, loop_count: u16) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"GIF89a");
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.push(0x80 | 0x00); // global color table flag, 2 entries
        bytes.push(0); // background color index
        bytes.push(0); // pixel aspect ratio
        bytes.extend_from_slice(&[0, 0, 0, 255, 255, 255]);
        if loop_count != 0 {
            bytes.extend_from_slice(&[0x21, 0xff, 0x0b]);
            bytes.extend_from_slice(b"NETSCAPE2.0");
            bytes.extend_from_slice(&[0x03, 0x01, loop_count as u8, (loop_count >> 8) as u8, 0x00]);
        }
        for _ in 0..frame_count {
            bytes.extend_from_slice(&[0x21, 0xf9, 0x04, 0x00, 0x0a, 0x00, 0x00, 0x00]);
            bytes.push(0x2c);
            bytes.extend_from_slice(&[0, 0, 0, 0]);
            bytes.extend_from_slice(&width.to_le_bytes());
            bytes.extend_from_slice(&height.to_le_bytes());
            bytes.push(0x00);
            bytes.extend_from_slice(&[0x08, 0x02, 0x00, 0x00, 0x00]);
        }
        bytes.push(0x3b);
        bytes
    }

    fn iend_chunk_end(bytes: &[u8]) -> Option<usize> {
        const PNG_SIGNATURE_BYTES: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
        if !bytes.starts_with(PNG_SIGNATURE_BYTES) {
            return None;
        }
        let mut offset = PNG_SIGNATURE_BYTES.len();
        while offset < bytes.len() {
            if offset + 8 > bytes.len() {
                return None;
            }
            let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().ok()?) as usize;
            let chunk_end = offset.checked_add(8)?.checked_add(length)?.checked_add(4)?;
            if chunk_end > bytes.len() {
                return None;
            }
            if &bytes[offset + 4..offset + 8] == b"IEND" {
                return Some(chunk_end);
            }
            offset = chunk_end;
        }
        None
    }

    fn iend_chunk_offset(bytes: &[u8]) -> usize {
        const PNG_SIGNATURE_BYTES: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
        let mut offset = PNG_SIGNATURE_BYTES.len();
        loop {
            let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            let chunk_end = offset + 8 + length + 4;
            if &bytes[offset + 4..offset + 8] == b"IEND" {
                return offset;
            }
            offset = chunk_end;
        }
    }

    #[test]
    fn parses_builtin_and_managed_references() {
        assert_eq!(
            parse_member_avatar_ref(LUOKE_AVATAR_REF).unwrap(),
            MemberAvatarReference::Builtin(BuiltinMemberAvatar::Luoke)
        );
        let asset_id = Uuid::new_v4();
        let value = format!("rovai://member-avatar/managed/{asset_id}");
        assert_eq!(
            parse_member_avatar_ref(&value).unwrap(),
            MemberAvatarReference::Managed(asset_id)
        );
    }

    #[test]
    fn rejects_non_canonical_or_unsafe_references() {
        for value in [
            "",
            "builtin://camp-companions/luoke/v1",
            "managed://member-avatars/2b945f3f-4b45-4ae5-92b2-739fce600338",
            "file:///tmp/avatar.png",
            "https://example.com/avatar.png",
            "data:image/png;base64,AAAA",
            "/tmp/avatar.png",
            "rovai://member-avatar/builtin/unknown/v1",
            "rovai://member-avatar/builtin/luoke/v2",
            "rovai://member-avatar/builtin/luoke/v1/",
            "rovai://member-avatar/builtin/luoke/v1?size=32",
            "rovai://member-avatar/builtin/luoke/v1#preview",
            "rovai://member-avatar/managed/------------------------------------",
            "rovai://member-avatar/managed/2B945F3F-4B45-4AE5-92B2-739FCE600338",
            "ROVAI://MEMBER-AVATAR/builtin/luoke/v1",
            "rovai://member-avatar/managed/2b945f3f-4b45-4ae5-92b2-739fce600338/extra",
            "rovai://member-avatar/managed/2b945f3f-4b45-4ae5-92b2-739fce600338%2Fextra",
        ] {
            assert!(parse_member_avatar_ref(value).is_err(), "{value}");
        }
    }

    #[test]
    fn permits_unchanged_legacy_values_but_not_new_legacy_values() {
        let legacy = Some("legacy://avatar");
        assert!(validate_member_avatar_update(legacy, legacy).is_ok());
        assert!(validate_member_avatar_update(None, legacy).is_err());
        assert!(validate_member_avatar_update(legacy, None).is_ok());
        assert!(validate_member_avatar_update(legacy, Some(MUWA_AVATAR_REF)).is_ok());
    }

    #[test]
    fn imports_four_by_five_image_with_lightweight_crop_and_deterministic_identity() {
        let directory = temporary_directory();
        let input = directory.join("portrait.png");
        write_test_png(&input, [24, 48, 96, 255]);
        let asset_id = Uuid::new_v4();

        let first = import_managed_member_avatar(&directory, asset_id, &input).unwrap();
        assert_eq!(first.source_width, 400);
        assert_eq!(first.source_height, 500);
        assert_eq!(first.crop.center_x, 0.5);
        assert!((first.crop.center_y - 0.45).abs() < f64::EPSILON);
        assert_eq!(first.crop.size, 1.0);
        assert_eq!(
            first.avatar_ref,
            format!("rovai://member-avatar/managed/{asset_id}")
        );
        let replay = import_managed_member_avatar(&directory, asset_id, &input).unwrap();
        assert_eq!(replay, first);

        let asset = directory.join("member-avatars").join(asset_id.to_string());
        let manifest: Value =
            serde_json::from_slice(&fs::read(asset.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["source"]["orientationNormalized"], true);
        assert_eq!(manifest["source"]["metadataStripped"], true);
        assert_eq!(manifest["icon"]["width"], ICON_EDGE);
        assert_eq!(manifest["icon"]["height"], ICON_EDGE);

        // Existing imported assets and normalized UI saves share the same reader.
        let portrait = read_managed_member_avatar(&directory, &first.avatar_ref, true)
            .unwrap()
            .unwrap();
        assert_eq!((portrait.width, portrait.height), (400, 500));
        let source = fs::read(
            directory
                .join("member-avatars")
                .join(asset_id.to_string())
                .join("source.png"),
        )
        .unwrap();
        let icon = fs::read(
            directory
                .join("member-avatars")
                .join(asset_id.to_string())
                .join("icon-192.png"),
        )
        .unwrap();
        let saved =
            save_managed_member_avatar(&directory, &source, &icon, 400, 500, first.crop.clone())
                .unwrap();
        let repeated =
            save_managed_member_avatar(&directory, &source, &icon, 400, 500, first.crop.clone())
                .unwrap();
        assert_eq!(saved, repeated);
        assert_eq!(
            read_managed_member_avatar(&directory, &saved.avatar_ref, false)
                .unwrap()
                .unwrap()
                .width,
            192
        );
        assert!(
            save_managed_member_avatar(&directory, &source, &icon, 500, 400, first.crop.clone())
                .is_err()
        );
        assert!(
            save_managed_member_avatar(
                &directory,
                &source,
                &icon,
                400,
                500,
                MemberAvatarCrop {
                    size: 2.0,
                    ..first.crop.clone()
                }
            )
            .is_err()
        );
        let cropped = recrop_member_avatar(
            &directory,
            &saved.avatar_ref,
            MemberAvatarCrop {
                center_x: 0.5,
                center_y: 0.5,
                size: 0.5,
            },
        )
        .unwrap();
        assert_ne!(cropped.avatar_ref, saved.avatar_ref);
        assert_eq!(
            member_avatar_bytes(&directory, &cropped.avatar_ref, true).unwrap(),
            source
        );
        assert!(
            recrop_member_avatar(
                &directory,
                &saved.avatar_ref,
                MemberAvatarCrop {
                    center_x: 0.0,
                    center_y: 0.5,
                    size: 1.0
                }
            )
            .is_err()
        );
        for avatar_ref in [
            LUOKE_AVATAR_REF,
            MUWA_AVATAR_REF,
            MIANZHI_AVATAR_REF,
            QILU_AVATAR_REF,
            &saved.avatar_ref,
        ] {
            for portrait in [false, true] {
                let path = materialize_member_avatar(&directory, &directory, avatar_ref, portrait)
                    .unwrap();
                assert!(Path::new(&path).is_file());
                assert!(Path::new(&path).starts_with(&directory));
                assert!(image::load_from_memory(&fs::read(path).unwrap()).is_ok());
            }
        }
        let builtin_crop = recrop_member_avatar(
            &directory,
            LUOKE_AVATAR_REF,
            MemberAvatarCrop {
                center_x: 0.5,
                center_y: 0.5,
                size: 0.5,
            },
        )
        .unwrap();
        assert_eq!(
            member_avatar_bytes(&directory, &builtin_crop.avatar_ref, true).unwrap(),
            member_avatar_bytes(&directory, LUOKE_AVATAR_REF, true).unwrap()
        );
        assert!(
            materialize_member_avatar(
                &directory,
                &directory.join("missing-run-tmp"),
                &saved.avatar_ref,
                true
            )
            .is_err()
        );
        let saved_id = saved.avatar_ref.rsplit('/').next().unwrap();
        fs::write(
            directory
                .join("member-avatars")
                .join(saved_id)
                .join("source.png"),
            b"changed",
        )
        .unwrap();
        assert!(read_managed_member_avatar(&directory, &saved.avatar_ref, true).is_err());

        write_test_png(&input, [96, 48, 24, 255]);
        let conflict = import_managed_member_avatar(&directory, asset_id, &input).unwrap_err();
        assert_eq!(
            conflict.kind,
            MemberAvatarImportErrorKind::CreationKeyConflict
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn animated_png_is_rejected_before_decode() {
        let directory = temporary_directory();
        let input = directory.join("animated.png");
        let mut bytes = encode_png(&DynamicImage::new_rgba8(300, 300)).unwrap();
        let animation_control = [
            0, 0, 0, 8, b'a', b'c', b'T', b'L', 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        bytes.splice(33..33, animation_control);
        fs::write(&input, bytes).unwrap();

        let error = import_managed_member_avatar(&directory, Uuid::new_v4(), &input).unwrap_err();

        assert_eq!(error.kind, MemberAvatarImportErrorKind::Invalid);
        assert!(error.to_string().contains("Animated PNG"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn upload_png_accepts_trailing_metadata_but_managed_assets_reject_it() {
        let mut bytes = encode_png(&DynamicImage::new_rgba8(300, 300)).unwrap();
        let iend_end = iend_chunk_end(&bytes).unwrap();
        assert_eq!(iend_end, bytes.len());
        // Vendor metadata appended after IEND (vivo screenshot case).
        for index in 0..281 {
            bytes.push(((index * 37) % 251) as u8);
        }
        assert_eq!(inspect_upload_png(&bytes).unwrap(), iend_end);
        assert!(reject_animated_png(&bytes).is_err());
        assert!(validate_normalized_png(&bytes, 300, 300, NORMALIZED_MAXIMUM_EDGE).is_err());
    }

    #[test]
    fn malformed_iend_and_truncated_chunks_are_still_rejected() {
        // IEND with a non-zero length must still be rejected.
        let mut inflated = encode_png(&DynamicImage::new_rgba8(300, 300)).unwrap();
        let iend = iend_chunk_offset(&inflated);
        inflated[iend..iend + 4].copy_from_slice(&4_u32.to_be_bytes());
        inflated.extend_from_slice(b"DATA");
        inflated.extend_from_slice(&0_u32.to_be_bytes());
        assert!(reject_animated_png(&inflated).is_err());

        // A chunk cut short by the file end must still be rejected.
        let mut truncated = encode_png(&DynamicImage::new_rgba8(300, 300)).unwrap();
        truncated.truncate(truncated.len() - 4);
        assert!(reject_animated_png(&truncated).is_err());
    }

    #[test]
    fn trailing_metadata_is_imported_and_reencoded_without_residue() {
        let directory = temporary_directory();
        let input = directory.join("vivo-trailing.png");
        let mut bytes = encode_png(&DynamicImage::new_rgba8(582, 584)).unwrap();
        bytes.extend_from_slice(br#"{"vendor":"vivo","meta":"screenshot"}"#);
        bytes.extend_from_slice(&[0u8; 281 - 35]);
        fs::write(&input, bytes.clone()).unwrap();

        // End-to-end import must succeed and re-encode so no tail reaches disk.
        let asset_id = Uuid::new_v4();
        let summary = import_managed_member_avatar(&directory, asset_id, &input).unwrap();
        let asset = directory.join("member-avatars").join(asset_id.to_string());
        let source = fs::read(asset.join("source.png")).unwrap();
        let icon = fs::read(asset.join("icon-192.png")).unwrap();
        assert_eq!(iend_chunk_end(&source), Some(source.len()));
        assert_eq!(iend_chunk_end(&icon), Some(icon.len()));
        assert!(reject_animated_png(&source).is_ok());
        let portrait = read_managed_member_avatar(&directory, &summary.avatar_ref, true)
            .unwrap()
            .unwrap();
        assert_eq!((portrait.width, portrait.height), (582, 584));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn trailing_bytes_cannot_bypass_the_selected_file_byte_cap() {
        let directory = temporary_directory();
        let input = directory.join("oversized-tail.png");
        let mut bytes = encode_png(&DynamicImage::new_rgba8(512, 512)).unwrap();
        bytes.resize(SELECTED_FILE_BYTES as usize + 1024, 0u8);
        fs::write(&input, bytes).unwrap();

        let error = import_managed_member_avatar(&directory, Uuid::new_v4(), &input).unwrap_err();
        assert_eq!(error.kind, MemberAvatarImportErrorKind::Invalid);
        assert!(error.to_string().contains("no larger than 10 MiB"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn imports_animated_gif_and_preserves_frames_delays_and_loop() {
        let directory = temporary_directory();
        let input = directory.join("animated.gif");
        write_test_gif(
            &input,
            &[
                solid_rgba(400, 500, [24, 48, 96, 255]),
                solid_rgba(400, 500, [96, 48, 24, 255]),
            ],
        );
        let asset_id = Uuid::new_v4();
        let summary = import_managed_member_avatar(&directory, asset_id, &input).unwrap();
        assert_eq!((summary.source_width, summary.source_height), (400, 500));
        let asset = directory.join("member-avatars").join(asset_id.to_string());
        let source = fs::read(asset.join("source.gif")).unwrap();
        // A ≤2048-edge GIF source is stored verbatim so no animation semantics change.
        assert_eq!(source, fs::read(&input).unwrap());
        let icon = fs::read(asset.join("icon-192.gif")).unwrap();

        let manifest: Value =
            serde_json::from_slice(&fs::read(asset.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["schemaVersion"], 2);
        assert_eq!(manifest["source"]["mediaType"], "image/gif");
        assert_eq!(manifest["source"]["frameCount"], 2);
        assert_eq!(manifest["icon"]["mediaType"], "image/gif");
        assert_eq!(manifest["icon"]["frameCount"], 2);

        let portrait = read_managed_member_avatar(&directory, &summary.avatar_ref, true)
            .unwrap()
            .unwrap();
        assert_eq!(portrait.media_type, "image/gif");
        assert_eq!((portrait.width, portrait.height), (400, 500));
        let icon_rendition = read_managed_member_avatar(&directory, &summary.avatar_ref, false)
            .unwrap()
            .unwrap();
        assert_eq!(icon_rendition.media_type, "image/gif");
        assert_eq!((icon_rendition.width, icon_rendition.height), (192, 192));

        // Both renditions stay animated and the icon keeps every frame.
        assert_eq!(parse_gif_info(&source).unwrap().frame_count, 2);
        assert_eq!(parse_gif_info(&icon).unwrap().frame_count, 2);
        let icon_frames = decode_composited_gif_frames(&icon).unwrap();
        assert_eq!(icon_frames.len(), 2);
        assert!(
            icon_frames[0].image.get_pixel(96, 96) != icon_frames[1].image.get_pixel(96, 96),
            "icon must keep both animation frames, not just the first"
        );
        assert_eq!(icon_frames[0].delay_centiseconds, 10);
        assert_eq!(icon_frames[1].delay_centiseconds, 10);

        let static_icon = read_managed_member_avatar_static_icon(&directory, &summary.avatar_ref)
            .unwrap()
            .unwrap();
        assert_eq!(static_icon.media_type, "image/png");
        let png = base64::engine::general_purpose::STANDARD
            .decode(static_icon.base64)
            .unwrap();
        let cover = image::load_from_memory_with_format(&png, ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        assert_eq!(cover, icon_frames[0].image);

        // Materialization exposes a .gif file so consumers keep the animation.
        let portrait_path =
            materialize_member_avatar(&directory, &directory, &summary.avatar_ref, true).unwrap();
        assert!(portrait_path.ends_with(".gif"));
        assert!(Path::new(&portrait_path).is_file());
        fs::remove_dir_all(directory).unwrap();
    }

    // Pure codec owner: disposal, partial frames and absent/finite/infinite
    // loops must survive cropping; no filesystem or complete Core fixture.
    #[test]
    fn gif_crop_preserves_transparency_disposal_and_loop_semantics() {
        use std::borrow::Cow;
        for loop_count in [None, Some(0), Some(3)] {
            for disposal in [
                DisposalMethod::Keep,
                DisposalMethod::Background,
                DisposalMethod::Previous,
            ] {
                let mut bytes = Vec::new();
                {
                    let mut encoder =
                        GifEncoder::new(&mut bytes, 256, 256, &[0, 0, 0, 255, 0, 0, 0, 255, 0])
                            .unwrap();
                    if let Some(count) = loop_count {
                        encoder
                            .set_repeat(if count == 0 {
                                Repeat::Infinite
                            } else {
                                Repeat::Finite(count)
                            })
                            .unwrap();
                    }
                    for (left, color, delay, dispose) in
                        [(0, 1, 7, disposal), (128, 2, 13, DisposalMethod::Keep)]
                    {
                        encoder
                            .write_frame(&GifFrame {
                                width: 128,
                                height: 256,
                                left,
                                delay,
                                dispose,
                                transparent: Some(0),
                                buffer: Cow::Owned(vec![color; 128 * 256]),
                                ..GifFrame::default()
                            })
                            .unwrap();
                    }
                }
                let frames = decode_composited_gif_frames(&bytes).unwrap();
                let icon = crop_gif_frames_to_icon(
                    &frames,
                    loop_count,
                    &default_member_avatar_crop(256, 256),
                    256,
                    256,
                )
                .unwrap();
                let output = decode_composited_gif_frames(&icon).unwrap();
                assert_eq!(parse_gif_info(&icon).unwrap().loop_count, loop_count);
                assert_eq!(
                    gif_decoder(&icon).unwrap().repeat(),
                    match loop_count {
                        None => Repeat::Finite(0),
                        Some(0) => Repeat::Infinite,
                        Some(n) => Repeat::Finite(n),
                    }
                );
                assert_eq!(
                    output
                        .iter()
                        .map(|f| f.delay_centiseconds)
                        .collect::<Vec<_>>(),
                    vec![7, 13]
                );
                assert_eq!(
                    output[1].image.get_pixel(48, 96).0,
                    if disposal == DisposalMethod::Keep {
                        [255, 0, 0, 255]
                    } else {
                        [0, 0, 0, 0]
                    }
                );
                assert_eq!(output[1].image.get_pixel(144, 96).0, [0, 255, 0, 255]);
                // Strip comments, unrelated applications and trailing bytes,
                // keeping the rendering stream (including the loop extension).
                let canonical = bytes.clone();
                let mut metadata = vec![0x21, 0xfe, 6];
                metadata.extend_from_slice(b"SECRET");
                metadata.push(0);
                metadata.extend_from_slice(&[0x21, 0xff, 11]);
                metadata.extend_from_slice(b"VENDOR01abc");
                metadata.extend_from_slice(&[3, 1, 2, 3, 0]);
                bytes.splice(25..25, metadata); // header + four-entry global palette
                bytes.extend_from_slice(b"TRAILING_METADATA");
                let normalized = normalize_gif_source_bytes(&bytes, &frames, loop_count).unwrap();
                assert_eq!(normalized, canonical);
            }
        }
    }

    #[test]
    fn recrops_gif_avatar_with_animation_and_idempotent_save() {
        let directory = temporary_directory();
        let input = directory.join("animated.gif");
        write_test_gif(
            &input,
            &[
                solid_rgba(400, 500, [24, 48, 96, 255]),
                solid_rgba(400, 500, [96, 48, 24, 255]),
            ],
        );
        let asset_id = Uuid::new_v4();
        let summary = import_managed_member_avatar(&directory, asset_id, &input).unwrap();
        let crop = MemberAvatarCrop {
            center_x: 0.5,
            center_y: 0.5,
            size: 0.5,
        };
        let recropped =
            recrop_member_avatar(&directory, &summary.avatar_ref, crop.clone()).unwrap();
        assert_ne!(recropped.avatar_ref, summary.avatar_ref);
        let icon = read_managed_member_avatar(&directory, &recropped.avatar_ref, false)
            .unwrap()
            .unwrap();
        assert_eq!(icon.media_type, "image/gif");
        let icon_bytes = base64::engine::general_purpose::STANDARD
            .decode(&icon.base64)
            .unwrap();
        assert_eq!(parse_gif_info(&icon_bytes).unwrap().frame_count, 2);

        // The content-idempotent GIF save is deterministic on replay.
        let source_bytes = member_avatar_bytes(&directory, &summary.avatar_ref, true).unwrap();
        let first =
            save_managed_member_avatar_gif(&directory, &source_bytes, 400, 500, crop.clone(), 2)
                .unwrap();
        let second =
            save_managed_member_avatar_gif(&directory, &source_bytes, 400, 500, crop.clone(), 2)
                .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            read_managed_member_avatar(&directory, &first.avatar_ref, false)
                .unwrap()
                .unwrap()
                .media_type,
            "image/gif"
        );
        for (width, height) in [(4096, 256), (256, 4096), (4097, 256)] {
            write_test_gif(&input, &[solid_rgba(width, height, [24, 48, 96, 255])]);
            let left_edge_crop = MemberAvatarCrop {
                center_x: f64::from(width.min(height)) / 2.0 / f64::from(width),
                center_y: 0.5,
                size: 1.0,
            };
            let original = prepare_member_avatar(
                &directory,
                Uuid::new_v4(),
                &input,
                Some(left_edge_crop),
                false,
            )
            .unwrap();
            assert!(
                read_managed_member_avatar(&directory, &original.avatar_ref, true)
                    .unwrap()
                    .is_some()
            );
            assert!(original.source_width.min(original.source_height) < MINIMUM_DECODED_EDGE);
            let recropped =
                recrop_member_avatar(&directory, &original.avatar_ref, crop.clone()).unwrap();
            assert!(
                read_managed_member_avatar(&directory, &recropped.avatar_ref, false)
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                recrop_member_avatar(&directory, &original.avatar_ref, crop.clone()).unwrap(),
                recropped
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn gif_frame_and_cumulative_decode_limits_are_enforced() {
        let directory = temporary_directory();
        let too_many = directory.join("too-many-frames.gif");
        write_bytes(&too_many, &structure_gif(256, 256, 201, 0));
        let error =
            import_managed_member_avatar(&directory, Uuid::new_v4(), &too_many).unwrap_err();
        assert_eq!(error.kind, MemberAvatarImportErrorKind::Invalid);
        assert!(error.to_string().contains("frame limit"));

        let heavy = directory.join("heavy-frames.gif");
        write_bytes(&heavy, &structure_gif(4096, 4096, 10, 0));
        let error = import_managed_member_avatar(&directory, Uuid::new_v4(), &heavy).unwrap_err();
        assert_eq!(error.kind, MemberAvatarImportErrorKind::Invalid);
        assert!(error.to_string().contains("cumulative"));

        let tiny = directory.join("tiny.gif");
        write_bytes(&tiny, &structure_gif(100, 100, 1, 0));
        let error = import_managed_member_avatar(&directory, Uuid::new_v4(), &tiny).unwrap_err();
        assert!(error.to_string().contains("256 by 256"));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn gif_frame_count_helper_matches_import_and_rejects_static_only_bytes() {
        let gif = structure_gif(512, 512, 3, 2);
        assert_eq!(gif_frame_count_for(&gif).unwrap(), 3);
        assert_eq!(parse_gif_info(&gif).unwrap().loop_count, Some(2));
        let png = encode_png(&DynamicImage::new_rgba8(512, 512)).unwrap();
        assert!(gif_frame_count_for(&png).is_err());
        assert_eq!(
            parse_gif_info(&structure_gif(256, 256, 1, 0))
                .unwrap()
                .loop_count,
            None
        );
        for end in 0..gif.len() {
            assert!(parse_gif_info(&gif[..end]).is_err(), "truncation at {end}");
        }
        let mut outside = structure_gif(256, 256, 1, 0);
        // Header + palette + graphic control = 27 bytes, then image descriptor.
        outside[28..30].copy_from_slice(&256_u16.to_le_bytes());
        assert!(decode_composited_gif_frames(&outside).is_err());
        let mut oversized = gif.clone();
        oversized.resize(SELECTED_FILE_BYTES as usize + 1, 0);
        assert!(
            save_managed_member_avatar_gif(
                Path::new("unused"),
                &oversized,
                512,
                512,
                default_member_avatar_crop(512, 512),
                3
            )
            .unwrap_err()
            .to_string()
            .contains("10 MiB")
        );
    }
}
