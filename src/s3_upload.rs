//! Explicit, create-only uploads to an existing Amazon S3 bucket.
//!
//! Credentials stay outside serializable drafts. The transport never creates a
//! bucket, grants public access, or turns an S3 location into a public URL.

use std::fs::{self, File};
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime};

use aws_config::profile::ProfileFileCredentialsProvider;
use aws_config::provider_config::ProviderConfig;
use aws_credential_types::provider::{
    ProvideCredentials, SharedCredentialsProvider, error::CredentialsError,
    future::ProvideCredentials as CredentialsFuture,
};
use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};
use aws_sdk_s3::config::{
    BehaviorVersion, Credentials, Region, retry::RetryConfig, timeout::TimeoutConfig,
};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{ChecksumAlgorithm, ChecksumType, CompletedMultipartUpload, CompletedPart};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Absolute prepared-file bound, independent of each media preparer's limits.
pub const MAX_S3_UPLOAD_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const PART_BYTES: usize = 8 * 1024 * 1024;
const READ_BYTES: usize = 64 * 1024;
const PROFILE_BYTES: u64 = 1024 * 1024;
const CREDENTIAL_TIMEOUT: Duration = Duration::from_secs(30);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(180);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);
const TOTAL_TIMEOUT: Duration = Duration::from_hours(2);

/// Reviewed destination and media preference; this contains no credentials.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct S3UploadDraft {
    /// Explicit AWS region containing the existing destination bucket.
    pub region: String,
    /// Existing general-purpose S3 bucket name, never an endpoint or ARN.
    pub bucket: String,
    /// Exact new object key, including any prefix and file extension.
    pub object_key: String,
    /// AWS shared profile; empty selects `AWS_PROFILE` or `default`.
    pub profile: String,
    /// Whether media preparation should retain an available video stream.
    pub upload_video: bool,
}

impl S3UploadDraft {
    /// Validates the reviewed destination without network or credential access.
    ///
    /// # Errors
    /// Rejects malformed regions, bucket names, keys or profile names.
    pub fn validate(&self) -> Result<(), String> {
        if self.region.len() < 5
            || self.region.len() > 63
            || !self
                .region
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || !self
                .region
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_lowercase)
            || !self
                .region
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_digit)
            || self.region.split('-').count() < 3
            || self.region.split('-').any(str::is_empty)
        {
            return Err("Enter an explicit AWS region such as eu-central-1".into());
        }
        if !(3..=63).contains(&self.bucket.len())
            || !self.bucket.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
            })
            || !self
                .bucket
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !self
                .bucket
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            || self.bucket.contains("..")
            || self.bucket.contains(".-")
            || self.bucket.contains("-.")
            || self.bucket.parse::<std::net::Ipv4Addr>().is_ok()
            || ["xn--", "sthree-", "amzn-s3-demo-"]
                .iter()
                .any(|prefix| self.bucket.starts_with(prefix))
            || ["-s3alias", "--ol-s3", ".mrap", "--x-s3", "--table-s3"]
                .iter()
                .any(|suffix| self.bucket.ends_with(suffix))
        {
            return Err("Enter an existing general-purpose S3 bucket name, not a URL, ARN or access-point alias".into());
        }
        if self.object_key.is_empty()
            || self.object_key.len() > 1024
            || self.object_key.trim() != self.object_key
            || self
                .object_key
                .chars()
                .any(|character| character.is_control() || character == '\\')
            || self
                .object_key
                .split('/')
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
        {
            return Err("S3 object key must contain 1–1024 UTF-8 bytes with no empty, dot, parent or control-character path components".into());
        }
        if !valid_profile_name(&self.profile) {
            return Err("AWS profile name must fit within 128 characters without whitespace or control characters".into());
        }
        Ok(())
    }
}

/// Session-only signing credentials, intentionally not serializable.
#[derive(Clone)]
pub struct S3UploadCredentials {
    access: String,
    secret: String,
    session_token: Option<String>,
}

impl std::fmt::Debug for S3UploadCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("S3UploadCredentials([REDACTED])")
    }
}

impl S3UploadCredentials {
    /// Accepts permanent or temporary credentials without persisting them.
    ///
    /// # Errors
    /// Rejects missing, oversized or header-unsafe credential values.
    pub fn new(
        access: String,
        secret: String,
        session_token: Option<String>,
    ) -> Result<Self, String> {
        let session_token = session_token.filter(|value| !value.is_empty());
        if [&access, &secret].iter().any(|value| {
            value.is_empty()
                || value.len() > 512
                || !value.bytes().all(|byte| byte.is_ascii_graphic())
        }) || session_token.as_ref().is_some_and(|value| {
            value.len() > 16 * 1024 || !value.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            return Err("AWS access key, secret key and optional session token must be bounded nonempty ASCII tokens".into());
        }
        Ok(Self {
            access,
            secret,
            session_token,
        })
    }

    fn sdk_credentials(&self) -> Credentials {
        Credentials::new(
            &self.access,
            &self.secret,
            self.session_token.clone(),
            None,
            "Youta session",
        )
    }
}

/// Transfer progress in bytes acknowledged by S3, not merely read locally.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct S3UploadProgress {
    /// Successfully transferred bytes; completion is reported separately.
    pub sent_bytes: u64,
    /// Exact prepared file size.
    pub total_bytes: u64,
}

/// Successful create-only upload; the location does not imply public access.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct S3UploadResult {
    /// Exact `s3://bucket/key` destination for display, not browser opening.
    pub location: String,
}

/// Redacted failure categories suitable for opening the masked credential UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum S3UploadError {
    /// No usable configured credentials; manual session keys can be requested.
    CredentialsRequired,
    /// Non-secret validation, cancellation, transfer or cleanup explanation.
    Failed(String),
}

impl std::fmt::Display for S3UploadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CredentialsRequired => formatter.write_str("AWS credentials are required; select a configured profile or enter session credentials"),
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for S3UploadError {}

/// Factory for bounded AWS-only upload sessions; construction performs no I/O.
#[derive(Clone, Debug, Default)]
pub struct S3UploadClient;

/// Resolved signing session retained across preparation and uploading.
pub struct ResolvedS3Upload {
    runtime: NetworkRuntime,
    client: aws_sdk_s3::Client,
    draft: S3UploadDraft,
    part_bytes: usize,
}

impl std::fmt::Debug for ResolvedS3Upload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ResolvedS3Upload([REDACTED])")
    }
}

impl S3UploadClient {
    /// Constructs an upload factory without reading credentials.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Resolves credentials before potentially expensive media preparation.
    ///
    /// # Errors
    /// Returns redacted credential, validation or cancellation failures.
    pub fn resolve_credentials(
        &self,
        draft: &S3UploadDraft,
        credentials: Option<&S3UploadCredentials>,
        cancellation: &Arc<AtomicBool>,
    ) -> Result<ResolvedS3Upload, S3UploadError> {
        draft.validate().map_err(S3UploadError::Failed)?;
        check_cancel(cancellation)?;
        let runtime = new_runtime()?;
        let provider = match credentials {
            Some(credentials) => SharedCredentialsProvider::new(credentials.sdk_credentials()),
            None => discover_provider(draft)?,
        };
        runtime.block_on(async {
            bounded(
                provider.provide_credentials(),
                cancellation,
                Instant::now() + CREDENTIAL_TIMEOUT,
            )
            .await?
            .map_err(credential_error)?;
            Ok::<_, S3UploadError>(())
        })?;
        let client = build_client(draft, provider);
        Ok(ResolvedS3Upload {
            runtime,
            client,
            draft: draft.clone(),
            part_bytes: PART_BYTES,
        })
    }

    /// Convenience wrapper for callers that already have prepared media.
    ///
    /// # Errors
    /// Returns the same failures as resolving and then uploading the file.
    pub fn upload_file(
        &self,
        draft: &S3UploadDraft,
        credentials: Option<&S3UploadCredentials>,
        path: &Path,
        cancellation: &Arc<AtomicBool>,
        progress: impl FnMut(S3UploadProgress),
    ) -> Result<S3UploadResult, S3UploadError> {
        self.resolve_credentials(draft, credentials, cancellation)?
            .upload_file(path, cancellation, progress)
    }
}

impl ResolvedS3Upload {
    #[cfg(test)]
    fn for_test(draft: S3UploadDraft, endpoint: &str, part_bytes: usize) -> Self {
        let config = client_config(
            &draft,
            SharedCredentialsProvider::new(Credentials::new(
                "fake-access",
                "fake-secret",
                Some("fake-session".into()),
                None,
                "fixture",
            )),
        )
        .endpoint_url(endpoint)
        .force_path_style(true)
        .build();
        Self {
            runtime: new_runtime().expect("runtime"),
            client: aws_sdk_s3::Client::from_conf(config),
            draft,
            part_bytes,
        }
    }

    /// Creates an object without replacing an existing current object.
    ///
    /// # Errors
    /// Reports transfer failures and any unconfirmed multipart cleanup.
    pub fn upload_file(
        &self,
        path: &Path,
        cancellation: &Arc<AtomicBool>,
        mut progress: impl FnMut(S3UploadProgress),
    ) -> Result<S3UploadResult, S3UploadError> {
        check_cancel(cancellation)?;
        let (mut file, snapshot) = open_media(path)?;
        progress(S3UploadProgress {
            sent_bytes: 0,
            total_bytes: snapshot.len,
        });
        check_cancel(cancellation)?;
        let deadline = Instant::now() + TOTAL_TIMEOUT;
        self.runtime.block_on(async {
            if snapshot.len <= self.part_bytes as u64 {
                let bytes = read_part(&mut file, usize::try_from(snapshot.len).map_err(|_| failed("Prepared file is too large"))?, cancellation, deadline)?;
                snapshot.check(path, &file)?;
                let checksum = STANDARD.encode(Sha256::digest(&bytes));
                let request = self.client.put_object().bucket(&self.draft.bucket).key(&self.draft.object_key)
                    .if_none_match("*").content_length(i64::try_from(snapshot.len).map_err(|_| failed("Prepared file is too large"))?)
                    .content_type(media_type(path)).checksum_algorithm(ChecksumAlgorithm::Sha256)
                    .checksum_sha256(&checksum).body(ByteStream::from(bytes)).send();
                let output = bounded(request, cancellation, deadline).await.map_err(uncertain)?
                    .map_err(|error| upload_error(&error, true))?;
                if output.checksum_sha256().is_some_and(|value| value != checksum) {
                    return Err(failed("S3 returned a different object checksum; inspect the destination before retrying"));
                }
                snapshot.check(path, &file).map_err(uncertain)?;
                progress(S3UploadProgress { sent_bytes: snapshot.len, total_bytes: snapshot.len });
            } else {
                self.multipart(path, &mut file, &snapshot, cancellation, deadline, &mut progress).await?;
            }
            Ok(S3UploadResult { location: format!("s3://{}/{}", self.draft.bucket, self.draft.object_key) })
        })
    }

    /// Sequential parts retain only one bounded data buffer and a bounded part list.
    async fn multipart(
        &self,
        path: &Path,
        file: &mut File,
        snapshot: &FileSnapshot,
        cancellation: &Arc<AtomicBool>,
        deadline: Instant,
        progress: &mut impl FnMut(S3UploadProgress),
    ) -> Result<(), S3UploadError> {
        let created = bounded(self.client.create_multipart_upload().bucket(&self.draft.bucket).key(&self.draft.object_key)
            .content_type(media_type(path)).checksum_algorithm(ChecksumAlgorithm::Sha256).checksum_type(ChecksumType::Composite).send(), cancellation, deadline).await
            .map_err(|error| failed(format!("{error}; multipart initiation may have reached S3. Inspect incomplete multipart uploads for this key before retrying")))?
            .map_err(|error| upload_error(&error, false))?;
        let upload_id = created.upload_id().filter(|value| !value.is_empty() && value.len() <= 4096)
            .ok_or_else(|| failed("S3 did not return a usable upload identifier; inspect incomplete multipart uploads for this key"))?;
        let transfer = async {
            let mut parts = Vec::new();
            let mut sent_bytes = 0_u64;
            let mut composite = Sha256::new();
            while sent_bytes < snapshot.len {
                check_cancel(cancellation)?;
                snapshot.check(path, file)?;
                let length = usize::try_from((snapshot.len - sent_bytes).min(self.part_bytes as u64)).map_err(|_| failed("Part size overflow"))?;
                let bytes = read_part(file, length, cancellation, deadline)?;
                let digest = Sha256::digest(&bytes);
                let checksum = STANDARD.encode(digest);
                composite.update(digest);
                let number = i32::try_from(parts.len() + 1).map_err(|_| failed("Too many S3 parts"))?;
                if number > 10_000 { return Err(failed("Too many S3 parts")); }
                let uploaded = bounded(self.client.upload_part().bucket(&self.draft.bucket).key(&self.draft.object_key)
                    .upload_id(upload_id).part_number(number).content_length(i64::try_from(length).map_err(|_| failed("Part size overflow"))?)
                    .checksum_algorithm(ChecksumAlgorithm::Sha256).checksum_sha256(&checksum).body(ByteStream::from(bytes)).send(), cancellation, deadline).await?
                    .map_err(|error| upload_error(&error, false))?;
                if uploaded.checksum_sha256().is_some_and(|value| value != checksum) {
                    return Err(failed("S3 returned a different part checksum"));
                }
                let etag = uploaded.e_tag().filter(|value| !value.is_empty() && value.len() <= 1024)
                    .ok_or_else(|| failed("S3 omitted the uploaded part's ETag"))?;
                parts.push(CompletedPart::builder().part_number(number).e_tag(etag).checksum_sha256(checksum).build());
                sent_bytes += length as u64;
                progress(S3UploadProgress { sent_bytes, total_bytes: snapshot.len });
            }
            check_cancel(cancellation)?;
            snapshot.check(path, file)?;
            let checksum = format!("{}-{}", STANDARD.encode(composite.finalize()), parts.len());
            let completed = bounded(self.client.complete_multipart_upload().bucket(&self.draft.bucket).key(&self.draft.object_key)
                .upload_id(upload_id).if_none_match("*").checksum_type(ChecksumType::Composite).checksum_sha256(&checksum)
                .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build()).send(), cancellation, deadline).await.map_err(uncertain)?
                .map_err(|error| upload_error(&error, true))?;
            if completed.checksum_sha256().is_some_and(|value| value != checksum) {
                return Err(failed("S3 returned a different complete-object checksum; inspect the destination before retrying"));
            }
            snapshot.check(path, file).map_err(uncertain)?;
            Ok(())
        }.await;
        if let Err(error) = transfer {
            // Cleanup ignores the user's cancelled flag, but has its own short deadline.
            let aborted = tokio::time::timeout(
                CLEANUP_TIMEOUT,
                self.client
                    .abort_multipart_upload()
                    .bucket(&self.draft.bucket)
                    .key(&self.draft.object_key)
                    .upload_id(upload_id)
                    .send(),
            )
            .await;
            let cleaned = match aborted {
                Ok(Ok(_)) => true,
                Ok(Err(error)) => {
                    error
                        .as_service_error()
                        .and_then(ProvideErrorMetadata::code)
                        == Some("NoSuchUpload")
                }
                Err(_) => false,
            };
            if !cleaned {
                return Err(failed(format!(
                    "{error}; cleanup could not be confirmed. Remove incomplete multipart uploads for this bucket/key in AWS to avoid storage charges"
                )));
            }
            return Err(error);
        }
        Ok(())
    }
}

fn failed(message: impl Into<String>) -> S3UploadError {
    S3UploadError::Failed(message.into())
}

fn uncertain(error: S3UploadError) -> S3UploadError {
    failed(format!(
        "{error}; the object may already exist in S3. Inspect the destination before retrying; Youta will not overwrite it"
    ))
}

/// Only fixed classifications reach the UI: never AWS response text or debug data.
fn upload_error<E: ProvideErrorMetadata>(error: &SdkError<E>, committing: bool) -> S3UploadError {
    let code = error
        .as_service_error()
        .and_then(ProvideErrorMetadata::code);
    let status = error
        .raw_response()
        .map(|response| response.status().as_u16());
    let message = match (code, status) {
        (Some("PreconditionFailed"), _) | (_, Some(412)) => {
            "An object already exists at this S3 key; it was not overwritten"
        }
        (Some("ConditionalRequestConflict"), _) | (_, Some(409)) => {
            "Another S3 write or deletion conflicted with this upload; inspect the destination before retrying"
        }
        (Some("AuthorizationHeaderMalformed" | "PermanentRedirect" | "IncorrectEndpoint"), _)
        | (_, Some(301 | 307)) => {
            "S3 rejected the bucket region; verify the configured region. No redirect was followed"
        }
        (
            Some("ExpiredToken" | "InvalidToken" | "InvalidAccessKeyId" | "SignatureDoesNotMatch"),
            _,
        ) => "AWS rejected the signing credentials; refresh the profile or session credentials",
        (Some("AccessDenied"), _) | (_, Some(403)) => {
            "AWS denied this operation; check credentials, bucket permissions and encryption-key permissions"
        }
        (Some("BadDigest"), _) => {
            "S3 rejected the upload checksum; the transfer did not pass integrity verification"
        }
        (Some("NoSuchBucket"), _) => {
            "The configured S3 bucket does not exist; Youta does not create buckets"
        }
        _ => "S3 upload failed or its response could not be confirmed",
    };
    let result = failed(message);
    if committing && !matches!(status, Some(301 | 307 | 400 | 403 | 404 | 409 | 412)) {
        uncertain(result)
    } else {
        result
    }
}

/// Runtime destruction has a deadline even when OS DNS remains blocked in the
/// SDK's blocking pool. Detached DNS can finish later, but no request is resumed.
struct NetworkRuntime(Option<tokio::runtime::Runtime>);

impl NetworkRuntime {
    fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.0
            .as_ref()
            .expect("runtime exists until drop")
            .block_on(future)
    }
}

impl Drop for NetworkRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_timeout(Duration::from_millis(100));
        }
    }
}

fn credential_error(error: CredentialsError) -> S3UploadError {
    match error {
        CredentialsError::CredentialsNotLoaded(_) => S3UploadError::CredentialsRequired,
        _ => failed(
            "AWS credentials could not be loaded; check the selected profile or refresh its login, or enter session credentials",
        ),
    }
}

fn new_runtime() -> Result<NetworkRuntime, S3UploadError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map(|runtime| NetworkRuntime(Some(runtime)))
        .map_err(|_| failed("Could not start the S3 network worker"))
}

fn timeouts() -> TimeoutConfig {
    TimeoutConfig::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .operation_timeout(OPERATION_TIMEOUT)
        .operation_attempt_timeout(OPERATION_TIMEOUT)
        .build()
}

/// Build directly, without inheriting environment/profile endpoint overrides.
fn client_config(
    draft: &S3UploadDraft,
    provider: SharedCredentialsProvider,
) -> aws_sdk_s3::config::Builder {
    aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(draft.region.clone()))
        .credentials_provider(provider)
        .retry_config(RetryConfig::standard().with_max_attempts(1))
        .timeout_config(timeouts())
}

fn build_client(draft: &S3UploadDraft, provider: SharedCredentialsProvider) -> aws_sdk_s3::Client {
    aws_sdk_s3::Client::from_conf(client_config(draft, provider).build())
}

fn check_cancel(cancellation: &AtomicBool) -> Result<(), S3UploadError> {
    if cancellation.load(Ordering::Relaxed) {
        Err(failed("S3 upload cancelled"))
    } else {
        Ok(())
    }
}

/// Dropping the selected request future stops its local work; remote commit may
/// still have happened, so commit callers explicitly report ambiguous outcomes.
async fn bounded<T>(
    future: impl Future<Output = T>,
    cancellation: &AtomicBool,
    deadline: Instant,
) -> Result<T, S3UploadError> {
    check_cancel(cancellation)?;
    tokio::pin!(future);
    loop {
        if Instant::now() >= deadline {
            return Err(failed("S3 operation exceeded its time limit"));
        }
        tokio::select! {
            biased;
            () = tokio::time::sleep(Duration::from_millis(25)) => check_cancel(cancellation)?,
            result = &mut future => return Ok(result),
        }
    }
}

/// An immutable descriptor is used for all reads; replacements and modifications
/// are checked before requests and before final multipart publication.
struct FileSnapshot {
    len: u64,
    modified: Option<SystemTime>,
    identity: Option<crate::file_identity::FilesystemIdentity>,
}

impl FileSnapshot {
    fn check(&self, path: &Path, file: &File) -> Result<(), S3UploadError> {
        for metadata in [file.metadata(), fs::symlink_metadata(path)] {
            let metadata =
                metadata.map_err(|_| failed("Prepared S3 media is no longer accessible"))?;
            if !metadata.is_file()
                || metadata.len() != self.len
                || metadata.modified().ok() != self.modified
                || crate::file_identity::filesystem_identity(path, &metadata) != self.identity
            {
                return Err(failed("Prepared S3 media changed during the upload"));
            }
        }
        Ok(())
    }
}

fn open_media(path: &Path) -> Result<(File, FileSnapshot), S3UploadError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| failed("Prepared S3 media is not accessible"))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_S3_UPLOAD_BYTES {
        return Err(failed(
            "Prepared S3 media must be a nonempty regular file no larger than 20 GiB",
        ));
    }
    let snapshot = FileSnapshot {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        identity: crate::file_identity::filesystem_identity(path, &metadata),
    };
    let file = File::open(path).map_err(|_| failed("Could not open prepared S3 media"))?;
    snapshot.check(path, &file)?;
    Ok((file, snapshot))
}

fn read_part(
    file: &mut File,
    length: usize,
    cancellation: &AtomicBool,
    deadline: Instant,
) -> Result<Vec<u8>, S3UploadError> {
    let mut bytes = vec![0; length];
    for chunk in bytes.chunks_mut(READ_BYTES) {
        check_cancel(cancellation)?;
        if Instant::now() >= deadline {
            return Err(failed("S3 upload exceeded its time limit"));
        }
        file.read_exact(chunk)
            .map_err(|_| failed("Could not read the complete prepared S3 media"))?;
    }
    Ok(bytes)
}

fn media_type(path: &Path) -> &'static str {
    match path.extension().and_then(|value| value.to_str()) {
        Some("opus" | "ogg") => "audio/ogg",
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("m4a") => "audio/mp4",
        Some("wav") => "audio/wav",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        _ => "application/octet-stream",
    }
}

fn valid_profile_name(value: &str) -> bool {
    value.len() <= 128
        && value.chars().all(|character| {
            !character.is_whitespace() && !character.is_control() && !matches!(character, '[' | ']')
        })
}

/// A desktop export must never probe instance/container metadata as a fallback.
#[derive(Debug)]
struct DisabledMetadataCredentials;

impl ProvideCredentials for DisabledMetadataCredentials {
    fn provide_credentials<'a>(&'a self) -> CredentialsFuture<'a>
    where
        Self: 'a,
    {
        CredentialsFuture::new(async {
            Err(CredentialsError::not_loaded(
                "Desktop metadata credential sources are disabled",
            ))
        })
    }
}

/// Explicit profiles override ambient static keys. Otherwise environment keys
/// precede the standard shared profile, matching the SDK's usual precedence.
fn discover_provider(draft: &S3UploadDraft) -> Result<SharedCredentialsProvider, S3UploadError> {
    if draft.profile.is_empty() {
        let access = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let secret = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
        let token = std::env::var("AWS_SESSION_TOKEN").ok();
        if access.is_some() || secret.is_some() || token.is_some() {
            let credentials = S3UploadCredentials::new(
                access.unwrap_or_default(),
                secret.unwrap_or_default(),
                token,
            )
            .map_err(|_| failed("AWS environment credentials are incomplete or invalid"))?;
            return Ok(SharedCredentialsProvider::new(
                credentials.sdk_credentials(),
            ));
        }
    }
    // The SDK's STS/SSO profile clients can inherit custom endpoints. Refuse
    // those settings rather than sending signing credentials outside AWS.
    if std::env::vars_os().any(|(key, _)| {
        key.to_str()
            .is_some_and(|key| key == "AWS_ENDPOINT_URL" || key.starts_with("AWS_ENDPOINT_URL_"))
    }) {
        return Err(failed(
            "Custom AWS endpoint overrides are not supported for S3 uploads; remove them or use manual session credentials",
        ));
    }
    let profile = if draft.profile.is_empty() {
        std::env::var("AWS_PROFILE").unwrap_or_else(|_| "default".into())
    } else {
        draft.profile.clone()
    };
    if profile.is_empty() || !valid_profile_name(&profile) {
        return Err(failed("The configured AWS profile name is invalid"));
    }
    let home = directories::BaseDirs::new().map(|base| base.home_dir().to_path_buf());
    let config = profile_path("AWS_CONFIG_FILE", home.as_deref(), "config");
    let credentials = profile_path(
        "AWS_SHARED_CREDENTIALS_FILE",
        home.as_deref(),
        "credentials",
    );
    profile_provider(
        draft,
        &profile,
        read_profile(config.as_deref())?,
        read_profile(credentials.as_deref())?,
    )
}

fn profile_path(variable: &str, home: Option<&Path>, filename: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".aws").join(filename)))
}

fn read_profile(path: Option<&Path>) -> Result<String, S3UploadError> {
    let Some(path) = path else {
        return Ok(String::new());
    };
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(_) => return Err(failed("Could not access the AWS shared configuration")),
    };
    if !metadata.is_file() || metadata.len() > PROFILE_BYTES {
        return Err(failed(
            "AWS shared configuration must be a regular file no larger than 1 MiB",
        ));
    }
    let mut contents = String::new();
    File::open(path)
        .map_err(|_| failed("Could not open the AWS shared configuration"))?
        .take(PROFILE_BYTES + 1)
        .read_to_string(&mut contents)
        .map_err(|_| failed("Could not read the AWS shared configuration as UTF-8"))?;
    if contents.len() as u64 > PROFILE_BYTES {
        return Err(failed("AWS shared configuration exceeds 1 MiB"));
    }
    Ok(contents)
}

fn profile_provider(
    draft: &S3UploadDraft,
    profile: &str,
    config: String,
    credentials: String,
) -> Result<SharedCredentialsProvider, S3UploadError> {
    let files = EnvConfigFiles::builder()
        .with_contents(EnvConfigFileKind::Config, config)
        .with_contents(EnvConfigFileKind::Credentials, credentials)
        .build();
    // AWS owns INI parsing/merging, including the different section conventions
    // in config and credentials. Only the selected source-profile chain matters.
    // Both inputs are in-memory; this parser cannot open default profile files.
    let parsed = new_runtime()?
        .block_on(aws_config::profile::load(
            &Default::default(),
            &Default::default(),
            &files,
            Some(profile.to_owned().into()),
        ))
        .map_err(|_| failed("AWS shared configuration could not be parsed"))?;
    let mut selected = Some(profile);
    let mut visited = std::collections::HashSet::new();
    while let Some(name) = selected {
        if visited.len() >= 16 || !visited.insert(name) {
            return Err(failed(
                "The selected AWS role chain is cyclic or exceeds 16 profiles",
            ));
        }
        let Some(entry) = parsed.get_profile(name) else {
            break;
        };
        if [
            "endpoint_url",
            "services",
            "credential_process",
            "web_identity_token_file",
            "login_session",
        ]
        .iter()
        .any(|key| entry.get(key).is_some())
        {
            return Err(failed(
                "The selected AWS profile chain uses an unsupported custom endpoint, process, web identity or console-login provider; use a shared static/assume-role/SSO profile or manual session credentials",
            ));
        }
        selected = entry.get("source_profile");
    }
    let conf = ProviderConfig::without_region()
        .with_region(Some(Region::new(draft.region.clone())))
        .with_retry_config(RetryConfig::standard().with_max_attempts(1))
        .with_timeout_config(timeouts());
    Ok(SharedCredentialsProvider::new(
        ProfileFileCredentialsProvider::builder()
            .configure(&conf)
            .profile_files(files)
            .profile_name(profile)
            .with_custom_provider("Ec2InstanceMetadata", DisabledMetadataCredentials)
            .with_custom_provider("EcsContainer", DisabledMetadataCredentials)
            .build(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> S3UploadDraft {
        S3UploadDraft {
            region: "eu-central-1".into(),
            bucket: "example-audio".into(),
            object_key: "music/recording.opus".into(),
            ..S3UploadDraft::default()
        }
    }

    #[test]
    fn destination_validation_rejects_endpoints_arns_and_ambiguous_keys() {
        for bucket in [
            "",
            "https://example.test",
            "arn:aws:s3:::example",
            "EXAMPLE",
            "127.0.0.1",
            "bucket--x-s3",
        ] {
            assert!(
                S3UploadDraft {
                    bucket: bucket.into(),
                    ..draft()
                }
                .validate()
                .is_err(),
                "{bucket}"
            );
        }
        for region in [
            "",
            "https://example.test",
            "eu-central-1/else",
            " eu-central-1",
        ] {
            assert!(
                S3UploadDraft {
                    region: region.into(),
                    ..draft()
                }
                .validate()
                .is_err(),
                "{region}"
            );
        }
        for object_key in [
            "",
            "/absolute.opus",
            "music/../secret",
            "music//song",
            "music\\song",
            "\0bad",
        ] {
            assert!(
                S3UploadDraft {
                    object_key: object_key.into(),
                    ..draft()
                }
                .validate()
                .is_err(),
                "{object_key}"
            );
        }
    }

    #[test]
    fn credential_validation_and_debug_never_expose_secrets() {
        assert!(S3UploadCredentials::new("".into(), "secret".into(), None).is_err());
        assert!(S3UploadCredentials::new("access".into(), "secret\n".into(), None).is_err());
        assert!(
            S3UploadCredentials::new("access".into(), "secret".into(), Some("token\r".into()))
                .is_err()
        );
        let credentials = S3UploadCredentials::new(
            "fake-access".into(),
            "fake-secret".into(),
            Some("fake-session".into()),
        )
        .expect("fake credentials");
        assert_eq!(
            format!("{credentials:?}"),
            "S3UploadCredentials([REDACTED])"
        );
    }

    #[test]
    fn empty_profile_contents_require_credentials_without_default_file_reads() {
        let runtime = new_runtime().expect("runtime");
        let provider = profile_provider(&draft(), "default", String::new(), String::new())
            .expect("empty profile provider");
        let error = runtime
            .block_on(provider.provide_credentials())
            .expect_err("missing credentials");
        assert!(matches!(
            credential_error(error),
            S3UploadError::CredentialsRequired
        ));
    }

    #[test]
    fn shared_static_profile_preserves_temporary_session_token() {
        let runtime = new_runtime().expect("runtime");
        let provider = profile_provider(&draft(), "selected", "[profile selected]\nregion = us-east-1\n".into(), "[selected]\naws_access_key_id = fixture-profile-access\naws_secret_access_key = fixture-profile-secret\naws_session_token = fixture-profile-token\n".into()).expect("fixture profile");
        let credentials = runtime
            .block_on(provider.provide_credentials())
            .expect("static fixture");
        assert_eq!(credentials.access_key_id(), "fixture-profile-access");
        assert_eq!(credentials.session_token(), Some("fixture-profile-token"));
    }

    #[test]
    fn selected_metadata_sources_are_denied_without_network_or_fallback() {
        let runtime = new_runtime().expect("runtime");
        for source in ["Ec2InstanceMetadata", "EcsContainer"] {
            let provider = profile_provider(&draft(), "selected", format!("[profile selected]\nrole_arn = arn:aws:iam::123456789012:role/test\ncredential_source = {source}\n"), String::new()).expect("configured provider");
            let result = runtime.block_on(async {
                tokio::time::timeout(Duration::from_millis(100), provider.provide_credentials())
                    .await
            });
            assert!(
                matches!(result, Ok(Err(_))),
                "metadata source must fail immediately"
            );
        }
    }

    #[test]
    fn profile_endpoint_and_process_failures_are_redacted() {
        for setting in [
            "endpoint_url = https://secret.example",
            "credential_process = echo secret",
            "web_identity_token_file = /secret-token",
            "login_session = secret-session",
        ] {
            let error = profile_provider(
                &draft(),
                "selected",
                format!("[profile selected]\n{setting}\n"),
                String::new(),
            )
            .expect_err("unsupported")
            .to_string();
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn unrelated_unsupported_profile_does_not_break_selected_static_profile() {
        let runtime = new_runtime().expect("runtime");
        let provider = profile_provider(&draft(), "selected", "[profile unrelated]\nendpoint_url = https://unrelated.invalid\ncredential_process = ignored-command\n".into(), "[selected]\naws_access_key_id = fixture-profile-access\naws_secret_access_key = fixture-profile-secret\n".into()).expect("unrelated settings are inactive");
        assert!(runtime.block_on(provider.provide_credentials()).is_ok());
    }

    #[test]
    fn oversized_profile_and_nonregular_media_fail_before_network() {
        let large = tempfile::NamedTempFile::new().expect("profile fixture");
        large
            .as_file()
            .set_len(PROFILE_BYTES + 1)
            .expect("sparse profile");
        assert!(read_profile(Some(large.path())).is_err());
        let directory = tempfile::tempdir().expect("directory");
        assert!(open_media(directory.path()).is_err());
        let empty = media(b"");
        assert!(open_media(empty.path()).is_err());
        let too_large = tempfile::NamedTempFile::new().expect("large media");
        too_large
            .as_file()
            .set_len(MAX_S3_UPLOAD_BYTES + 1)
            .expect("sparse media");
        assert!(open_media(too_large.path()).is_err());
    }

    #[test]
    fn cancelled_resolution_does_not_discover_credentials_or_read_files() {
        let error = S3UploadClient::new()
            .resolve_credentials(&draft(), None, &Arc::new(AtomicBool::new(true)))
            .expect_err("cancelled before discovery");
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn network_worker_shutdown_does_not_wait_for_a_stuck_blocking_task() {
        let runtime = new_runtime().expect("runtime");
        let (started, ready) = std::sync::mpsc::channel();
        runtime.block_on(async {
            tokio::task::spawn_blocking(move || {
                started.send(()).expect("started");
                std::thread::sleep(Duration::from_millis(800));
            });
        });
        ready
            .recv_timeout(Duration::from_secs(1))
            .expect("blocking task starts");
        let began = Instant::now();
        drop(runtime);
        assert!(began.elapsed() < Duration::from_millis(400));
    }

    /// One-request-per-connection HTTP fixture; it never opens an external socket.
    struct Server {
        endpoint: String,
        requests: Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Server {
        fn new(responses: Vec<(u16, String)>) -> Self {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fixture listener");
            listener.set_nonblocking(true).expect("nonblocking");
            let endpoint = format!("http://{}", listener.local_addr().expect("address"));
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let collected = Arc::clone(&requests);
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = Arc::clone(&stop);
            let thread = std::thread::spawn(move || {
                let mut responses = responses.into_iter();
                let mut stalled = Vec::new();
                while !stopping.load(std::sync::atomic::Ordering::Relaxed) {
                    let Ok((mut socket, _)) = listener.accept() else {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    };
                    socket
                        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                        .expect("timeout");
                    let mut header = Vec::new();
                    let mut byte = [0];
                    while !header.ends_with(b"\r\n\r\n") && header.len() < 64 * 1024 {
                        if socket.read_exact(&mut byte).is_err() {
                            break;
                        }
                        header.push(byte[0]);
                    }
                    let header = String::from_utf8(header).expect("ASCII request header");
                    if header.to_ascii_lowercase().contains("expect: 100-continue") {
                        let _ = socket.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
                    }
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or_default();
                    assert!(length <= 16 * 1024 * 1024, "bounded fixture request");
                    let mut body = vec![0; length];
                    let _ = socket.read_exact(&mut body);
                    collected.lock().expect("requests").push((header, body));
                    let (status, body) = responses
                        .next()
                        .unwrap_or((500, "<Error><Code>UnexpectedRequest</Code></Error>".into()));
                    if status == 0 {
                        // Leave one request active while accepting abort cleanup.
                        stalled.push(socket);
                        continue;
                    }
                    let reply = format!(
                        "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nContent-Type: application/xml\r\nETag: \"part-etag\"\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(reply.as_bytes());
                }
            });
            Self {
                endpoint,
                requests,
                stop,
                thread: Some(thread),
            }
        }

        fn session(&self, part_bytes: usize) -> ResolvedS3Upload {
            ResolvedS3Upload::for_test(draft(), &self.endpoint, part_bytes)
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                thread.join().expect("fixture thread");
            }
        }
    }

    fn media(contents: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().expect("fixture file");
        file.write_all(contents).expect("fixture bytes");
        file
    }

    #[test]
    fn single_put_is_signed_conditional_private_and_exact_with_session_token() {
        let server = Server::new(vec![(200, String::new())]);
        let file = media(b"exact source bytes\0\xff");
        let mut updates = Vec::new();
        let result = server
            .session(1024)
            .upload_file(file.path(), &Arc::new(AtomicBool::new(false)), |update| {
                updates.push(update)
            })
            .expect("upload");
        assert_eq!(result.location, "s3://example-audio/music/recording.opus");
        let requests = server.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        let (header, body) = &requests[0];
        let header = header.to_ascii_lowercase();
        assert!(header.starts_with("put /example-audio/music/recording.opus?x-id=putobject "));
        assert!(header.contains("authorization: aws4-hmac-sha256"));
        assert!(header.contains("/eu-central-1/s3/aws4_request"));
        assert!(header.contains("x-amz-security-token: fake-session"));
        assert!(header.contains("if-none-match: *"));
        assert!(header.contains("x-amz-checksum-sha256:"));
        assert!(!header.contains("x-amz-acl:"));
        assert_eq!(body, b"exact source bytes\0\xff");
        assert_eq!(
            updates.last().expect("progress").sent_bytes,
            body.len() as u64
        );
    }

    #[test]
    fn existing_object_wrong_region_and_authentication_never_retry_or_echo_remote_secrets() {
        for (status, code) in [
            (412, "PreconditionFailed"),
            (301, "PermanentRedirect"),
            (403, "AccessDenied"),
            (400, "AuthorizationHeaderMalformed"),
        ] {
            let server = Server::new(vec![(
                status,
                format!(
                    "<Error><Code>{code}</Code><Message>fake-secret fake-session https://bad.example/</Message><Region>us-east-1</Region></Error>"
                ),
            )]);
            let file = media(b"data");
            let error = server
                .session(1024)
                .upload_file(file.path(), &Arc::new(AtomicBool::new(false)), |_| {})
                .expect_err("refused")
                .to_string();
            assert!(!error.contains("fake-secret"));
            assert!(!error.contains("fake-session"));
            assert!(!error.contains("bad.example"));
            assert_eq!(server.requests.lock().expect("requests").len(), 1);
        }
    }

    #[test]
    fn multipart_success_checksums_orders_parts_and_conditionally_completes() {
        let server = Server::new(vec![(200, "<InitiateMultipartUploadResult><UploadId>fixture-id</UploadId></InitiateMultipartUploadResult>".into()), (200, String::new()), (200, String::new()), (200, "<CompleteMultipartUploadResult><ETag>complete-etag</ETag></CompleteMultipartUploadResult>".into())]);
        let file = media(b"123456789");
        let mut progress = Vec::new();
        server
            .session(5)
            .upload_file(file.path(), &Arc::new(AtomicBool::new(false)), |update| {
                progress.push(update.sent_bytes)
            })
            .expect("multipart");
        let requests = server.requests.lock().expect("requests");
        assert_eq!(requests.len(), 4);
        assert!(requests[0].0.contains("?uploads"));
        assert_eq!(requests[1].1, b"12345");
        assert_eq!(requests[2].1, b"6789");
        assert!(
            requests[1]
                .0
                .to_ascii_lowercase()
                .contains("x-amz-checksum-sha256:")
        );
        assert!(
            requests[3]
                .0
                .to_ascii_lowercase()
                .contains("if-none-match: *")
        );
        let xml = String::from_utf8_lossy(&requests[3].1);
        assert!(xml.contains("<PartNumber>1</PartNumber>"));
        assert!(xml.contains("<PartNumber>2</PartNumber>"));
        assert!(xml.contains("<ChecksumSHA256>"));
        assert_eq!(progress, [0, 5, 9]);
    }

    #[test]
    fn multipart_http_200_embedded_error_is_failure_and_triggers_abort() {
        let server = Server::new(vec![(200, "<InitiateMultipartUploadResult><UploadId>fixture-id</UploadId></InitiateMultipartUploadResult>".into()), (200, String::new()), (200, String::new()), (200, "<Error><Code>InternalError</Code><Message>fake-secret</Message></Error>".into()), (204, String::new())]);
        let file = media(b"123456789");
        let error = server
            .session(5)
            .upload_file(file.path(), &Arc::new(AtomicBool::new(false)), |_| {})
            .expect_err("embedded error")
            .to_string();
        assert!(!error.contains("fake-secret"));
        let requests = server.requests.lock().expect("requests");
        assert_eq!(requests.len(), 5);
        assert!(requests[4].0.starts_with("DELETE "));
    }

    #[test]
    fn active_part_cancellation_is_bounded_and_aborts_without_completion() {
        let server = Server::new(vec![
            (200, "<InitiateMultipartUploadResult><UploadId>fixture-id</UploadId></InitiateMultipartUploadResult>".into()),
            (0, String::new()),
            (204, String::new()),
        ]);
        let cancellation = Arc::new(AtomicBool::new(false));
        let trigger = Arc::clone(&cancellation);
        let requests = Arc::clone(&server.requests);
        let cancelling = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while requests.lock().expect("requests").len() < 2 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            trigger.store(true, Ordering::Relaxed);
        });
        let file = media(b"123456789");
        let began = Instant::now();
        let mut progress = Vec::new();
        let error = server
            .session(5)
            .upload_file(file.path(), &cancellation, |update| {
                progress.push(update.sent_bytes)
            })
            .expect_err("cancelled");
        cancelling.join().expect("cancellation thread");
        assert!(began.elapsed() < Duration::from_secs(1));
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(progress, [0]);
        let requests = server.requests.lock().expect("requests");
        assert_eq!(requests.len(), 3);
        assert!(requests[2].0.starts_with("DELETE "));
    }

    #[test]
    fn rejected_part_checksum_aborts_without_retry_or_completion() {
        let server = Server::new(vec![
            (200, "<InitiateMultipartUploadResult><UploadId>fixture-id</UploadId></InitiateMultipartUploadResult>".into()),
            (400, "<Error><Code>BadDigest</Code><Message>fixture-secret</Message></Error>".into()),
            (204, String::new()),
        ]);
        let file = media(b"123456789");
        let error = server
            .session(5)
            .upload_file(file.path(), &Arc::new(AtomicBool::new(false)), |_| {})
            .expect_err("checksum failure")
            .to_string();
        assert!(error.contains("checksum"));
        assert!(!error.contains("fixture-secret"));
        let requests = server.requests.lock().expect("requests");
        assert_eq!(requests.len(), 3);
        assert!(requests[2].0.starts_with("DELETE "));
    }

    #[test]
    fn changed_media_aborts_before_reading_another_part_or_completing() {
        use std::io::Write;
        let server = Server::new(vec![
            (200, "<InitiateMultipartUploadResult><UploadId>fixture-id</UploadId></InitiateMultipartUploadResult>".into()),
            (200, String::new()),
            (204, String::new()),
        ]);
        let mut file = media(b"123456789");
        let path = file.path().to_path_buf();
        let error = server
            .session(5)
            .upload_file(&path, &Arc::new(AtomicBool::new(false)), |update| {
                if update.sent_bytes > 0 {
                    file.write_all(b"changed").expect("fixture edit");
                }
            })
            .expect_err("source changed")
            .to_string();
        assert!(error.contains("changed"));
        let requests = server.requests.lock().expect("requests");
        assert_eq!(requests.len(), 3);
        assert!(requests[2].0.starts_with("DELETE "));
    }

    #[test]
    fn multipart_cancellation_aborts_without_completing_and_reports_cleanup_failure() {
        for abort_status in [204, 403] {
            let server = Server::new(vec![(200, "<InitiateMultipartUploadResult><UploadId>fixture-id</UploadId></InitiateMultipartUploadResult>".into()), (200, String::new()), (abort_status, String::new())]);
            let file = media(b"123456789");
            let cancel = Arc::new(AtomicBool::new(false));
            let error = server
                .session(5)
                .upload_file(file.path(), &cancel, |update| {
                    if update.sent_bytes > 0 {
                        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                })
                .expect_err("cancelled")
                .to_string();
            assert!(error.to_ascii_lowercase().contains("cancel"));
            assert_eq!(error.contains("incomplete multipart"), abort_status == 403);
            let requests = server.requests.lock().expect("requests");
            assert_eq!(requests.len(), 3);
            assert!(requests[2].0.starts_with("DELETE "));
        }
    }
}
