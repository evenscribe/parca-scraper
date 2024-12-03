mod debuginfod;
mod fetcher;
mod metadata;

use self::debuginfopb::{
    debuginfo_upload::State, upload_instructions::UploadStrategy, upload_request, DebuginfoType,
    DebuginfoUpload, ShouldInitiateUploadRequest, UploadInstructions,
};
use crate::debuginfopb::{
    self, debuginfo::Source, debuginfo_service_server::DebuginfoService, BuildIdType, Debuginfo,
    InitiateUploadRequest, InitiateUploadResponse, MarkUploadFinishedRequest,
    MarkUploadFinishedResponse, ShouldInitiateUploadResponse, UploadRequest, UploadResponse,
};
use chrono::{DateTime, Duration, TimeZone, Utc};
pub use debuginfod::DebugInfod;
pub use fetcher::DebuginfoFetcher;
pub use metadata::MetadataStore;
use object_store::ObjectStore;
use std::collections::HashMap;
use std::result::Result;
use std::sync::{Arc, Mutex};
use tokio_stream::StreamExt;
use tonic::{async_trait, Request, Response, Status, Streaming};

const REASON_DEBUGINFO_IN_DEBUGINFOD: &str =
    "Debuginfo exists in debuginfod, therefore no upload is necessary.";
const REASON_FIRST_TIME_SEEN: &str = "First time we see this Build ID, and it does not exist in debuginfod, therefore please upload!";
const REASON_UPLOAD_STALE: &str =
    "A previous upload was started but not finished and is now stale, so it can be retried.";
const REASON_UPLOAD_IN_PROGRESS: &str =
    "A previous upload is still in-progress and not stale yet (only stale uploads can be retried).";
const REASON_DEBUGINFO_ALREADY_EXISTS: &str =
    "Debuginfo already exists and is not marked as invalid, therefore no new upload is needed.";
const REASON_DEBUGINFO_ALREADY_EXISTS_BUT_FORCED: &str = "Debuginfo already exists and is not marked as invalid, therefore wouldn't have accepted a new upload, but accepting it because it's requested to be forced.";
const REASON_DEBUGINFO_INVALID: &str = "Debuginfo already exists but is marked as invalid, therefore a new upload is needed. Hash the debuginfo and initiate the upload.";
const REASON_DEBUGINFO_EQUAL: &str = "Debuginfo already exists and is marked as invalid, but the proposed hash is the same as the one already available, therefore the upload is not accepted as it would result in the same invalid debuginfos.";
const REASON_DEBUGINFO_NOT_EQUAL: &str =
    "Debuginfo already exists but is marked as invalid, therefore a new upload will be accepted.";
const REASON_DEBUGINFOD_SOURCE: &str = "Debuginfo is available from debuginfod already and not marked as invalid, therefore no new upload is needed.";
const REASON_DEBUGINFOD_INVALID: &str = "Debuginfo is available from debuginfod already but is marked as invalid, therefore a new upload is needed.";

pub struct UploadRequestInfo {
    buildid: String,
    upload_id: String,
    debuginfo_type: DebuginfoType,
}

impl TryFrom<upload_request::Data> for UploadRequestInfo {
    type Error = Status;
    fn try_from(data: upload_request::Data) -> Result<Self, Self::Error> {
        match data {
            upload_request::Data::Info(upload_info) => Ok(Self {
                buildid: upload_info.build_id,
                upload_id: upload_info.upload_id,
                debuginfo_type: match DebuginfoType::try_from(upload_info.r#type) {
                    Ok(t) => t,
                    Err(_) => return Err(Status::invalid_argument("Invalid debuginfo type.")),
                },
            }),
            _ => Err(Status::invalid_argument("Invalid data type.")),
        }
    }
}

pub struct DebuginfoStore {
    pub(crate) metadata: MetadataStore,
    pub(crate) debuginfod: DebugInfod,
    pub(crate) max_upload_duration: Duration,
    pub(crate) max_upload_size: i64,
    pub(crate) bucket: Arc<dyn ObjectStore>,
}

#[async_trait]
impl DebuginfoService for DebuginfoStore {
    /// Upload ingests debug info for a given build_id
    async fn upload(
        &self,
        request: Request<Streaming<UploadRequest>>,
    ) -> anyhow::Result<Response<UploadResponse>, Status> {
        // log::info!("Upload request received");
        let mut stream = request.into_inner();

        let request = match stream.message().await {
            Ok(Some(msg)) => msg,
            Ok(None) => return Err(Status::invalid_argument("Empty request")),
            Err(e) => {
                return Err(Status::internal(format!(
                    "Failed to receive message: {}",
                    e
                )))
            }
        };

        let data = request
            .data
            .ok_or_else(|| Status::invalid_argument("Missing data"))?;
        let upload_info = UploadRequestInfo::try_from(data)?;
        let _ = self.validate_buildid(&upload_info.buildid)?;

        let dbginfo = self
            .metadata
            .fetch(&upload_info.buildid, &upload_info.debuginfo_type)
            .ok_or_else(|| {
                Status::failed_precondition(
                "metadata not found, this indicates that the upload was not previously initiated"
            )
            })?
            .clone();

        let upload = dbginfo.upload.ok_or_else(|| {
            Status::invalid_argument(
                "metadata not found, this indicates that the upload was not previously initiated",
            )
        })?;

        if upload.id.ne(&upload_info.upload_id) {
            return Err(Status::failed_precondition(
            "upload metadata not found, this indicates that the upload was not previously initiated"
        ));
        }

        let mut chunks = Vec::new();
        while let Some(req) = stream.next().await {
            let req = req?;
            match req.data {
                Some(upload_request::Data::ChunkData(chunk)) => {
                    chunks.extend(chunk);
                }
                _ => {
                    return Err(Status::invalid_argument(
                        "provided no value or invalid data",
                    ))
                }
            }
        }

        let size = chunks.len() as u64;

        match self
            .bucket
            .put(
                &object_store::path::Path::from(upload_info.upload_id),
                chunks.into(),
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                return Err(Status::internal(format!(
                    "Failed to store debuginfo: {}",
                    e
                )))
            }
        };

        Ok(Response::new(UploadResponse {
            build_id: upload_info.buildid,
            size,
        }))
    }

    // ShouldInitiateUpload returns whether an upload should be initiated for the
    // given build ID. Checking if an upload should even be initiated allows the
    // parca-agent to avoid extracting debuginfos unnecessarily from a binary.
    async fn should_initiate_upload(
        &self,
        request: Request<ShouldInitiateUploadRequest>,
    ) -> anyhow::Result<Response<ShouldInitiateUploadResponse>, Status> {
        // log::info!("ShouldInitiateUpload request received");
        let request = request.into_inner();
        let _ = self.validate_buildid(&request.build_id)?;

        let debuginfo = self.metadata.fetch(&request.build_id, &request.r#type());

        match debuginfo {
            Some(info) => self.handle_existing_debuginfo(&request, &info),
            None => Box::pin(self.handle_new_build_id(&request)).await,
        }
    }

    /// InitiateUpload returns a strategy and information to upload debug info for a given build_id.
    async fn initiate_upload(
        &self,
        request: Request<InitiateUploadRequest>,
    ) -> anyhow::Result<Response<InitiateUploadResponse>, Status> {
        // log::info!("InitiateUpload request received");

        let request = request.into_inner();

        if request.hash.is_empty() {
            return Err(Status::invalid_argument("Hash is empty"));
        }

        if request.size == 0 {
            return Err(Status::invalid_argument("Size is zero"));
        }

        let siup = ShouldInitiateUploadRequest {
            build_id: request.build_id.clone(),
            hash: request.hash.clone(),
            force: request.force,
            r#type: request.r#type().into(),
            build_id_type: request.build_id_type,
        };

        let should_initiate = self.should_initiate_upload(Request::new(siup)).await?;
        let should_initiate = should_initiate.into_inner();

        if !should_initiate.should_initiate_upload {
            if should_initiate
                .reason
                .eq_ignore_ascii_case(REASON_DEBUGINFO_EQUAL)
            {
                return Err(Status::already_exists("Debuginfo already exists"));
            }
            return Err(Status::failed_precondition(format!( "upload should not have been attempted to be initiated, a previous check should have failed with {}", should_initiate.reason )));
        }

        if request.size > self.max_upload_size {
            return Err(Status::invalid_argument(format!(
                "Upload size {} exceeds the maximum allowed size {}",
                request.size, self.max_upload_size,
            )));
        }

        let upload_id = ulid::Ulid::new().to_string();
        let upload_started = self.time_now();
        // let upload_expired = upload_started + self.max_upload_duration;

        {
            let _ = self
                .metadata
                .mark_as_uploading(
                    &request.build_id,
                    &upload_id,
                    &request.hash,
                    &request.r#type(),
                    upload_started,
                )
                .map_err(|e| {
                    Status::internal(format!(
                        "Failed to mark metadata as uploading. details: {e}"
                    ))
                })?;
        }

        Ok(Response::new(InitiateUploadResponse {
            upload_instructions: Some(UploadInstructions {
                upload_id,
                build_id: request.build_id,
                upload_strategy: UploadStrategy::Grpc.into(),
                signed_url: "".into(),
                r#type: request.r#type,
            }),
        }))
    }
    /// MarkUploadFinished marks the upload as finished for a given build_id.
    async fn mark_upload_finished(
        &self,
        request: Request<MarkUploadFinishedRequest>,
    ) -> Result<Response<MarkUploadFinishedResponse>, Status> {
        // log::info!("MarkUploadFinished request received");

        let request = request.into_inner();
        let _ = self.validate_buildid(&request.build_id)?;
        let _ = self
            .metadata
            .mark_as_uploaded(
                &request.build_id,
                &request.upload_id,
                &request.r#type(),
                self.time_now(),
            )
            .map_err(|e| {
                Status::internal(format!("Failed to mark metadata as uploaded. details: {e}"))
            })?;
        Ok(Response::new(MarkUploadFinishedResponse::default()))
    }
}

impl DebuginfoStore {
    fn validate_buildid(&self, id: &str) -> Result<(), Status> {
        if id.len() <= 2 {
            return Err(Status::invalid_argument("unexpectedly short input"));
        }

        Ok(())
    }

    fn is_upload_stale(&self, upload: &DebuginfoUpload) -> bool {
        match upload.started_at {
            Some(ts) => {
                let started_at = Utc
                    .timestamp_opt(ts.seconds, ts.nanos as u32)
                    .earliest()
                    .unwrap_or(Utc::now());

                started_at + (self.max_upload_duration + Duration::minutes(2)) < self.time_now()
            }
            None => false,
        }
    }

    fn time_now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn handle_existing_debuginfo(
        &self,
        request: &ShouldInitiateUploadRequest,
        debuginfo: &Debuginfo,
    ) -> Result<Response<ShouldInitiateUploadResponse>, Status> {
        match Source::try_from(debuginfo.source) {
            Ok(Source::Debuginfod) => self.handle_debuginfod_source(debuginfo),
            Ok(Source::Upload) => self.handle_upload_source(request, debuginfo),
            _ => Err(Status::internal("Inconsistent metadata: unknown source")),
        }
    }

    fn handle_upload_source(
        &self,
        request: &ShouldInitiateUploadRequest,
        debuginfo: &Debuginfo,
    ) -> Result<Response<ShouldInitiateUploadResponse>, Status> {
        let upload = debuginfo
            .upload
            .as_ref()
            .ok_or_else(|| Status::internal("Inconsistent metadata: missing upload info"))?;

        match State::try_from(upload.state) {
            Ok(State::Uploading) => self.handle_uploading_state(upload),
            Ok(State::Uploaded) => self.handle_uploaded_state(request, debuginfo),
            _ => Err(Status::internal(
                "Inconsistent metadata: unknown upload state",
            )),
        }
    }

    fn handle_uploading_state(
        &self,
        upload: &DebuginfoUpload,
    ) -> Result<Response<ShouldInitiateUploadResponse>, Status> {
        if self.is_upload_stale(upload) {
            Ok(Response::new(ShouldInitiateUploadResponse {
                should_initiate_upload: true,
                reason: REASON_UPLOAD_STALE.into(),
            }))
        } else {
            Ok(Response::new(ShouldInitiateUploadResponse {
                should_initiate_upload: false,
                reason: REASON_UPLOAD_IN_PROGRESS.into(),
            }))
        }
    }

    fn handle_uploaded_state(
        &self,
        request: &ShouldInitiateUploadRequest,
        debuginfo: &Debuginfo,
    ) -> Result<Response<ShouldInitiateUploadResponse>, Status> {
        if !self.is_valid_elf(debuginfo) {
            return self.handle_invalid_elf(request);
        }

        if request.hash.is_empty() {
            return Ok(Response::new(ShouldInitiateUploadResponse {
                should_initiate_upload: true,
                reason: REASON_DEBUGINFO_INVALID.into(),
            }));
        }

        self.compare_hash(request, debuginfo)
    }

    fn is_valid_elf(&self, debuginfo: &Debuginfo) -> bool {
        debuginfo
            .quality
            .as_ref()
            .map_or(false, |q| !q.not_valid_elf)
    }

    fn handle_invalid_elf(
        &self,
        request: &ShouldInitiateUploadRequest,
    ) -> Result<Response<ShouldInitiateUploadResponse>, Status> {
        Ok(Response::new(ShouldInitiateUploadResponse {
            should_initiate_upload: request.force,
            reason: if request.force {
                REASON_DEBUGINFO_ALREADY_EXISTS_BUT_FORCED.into()
            } else {
                REASON_DEBUGINFO_ALREADY_EXISTS.into()
            },
        }))
    }

    fn compare_hash(
        &self,
        request: &ShouldInitiateUploadRequest,
        debuginfo: &Debuginfo,
    ) -> Result<Response<ShouldInitiateUploadResponse>, Status> {
        match &debuginfo.upload {
            Some(upload) if upload.hash.eq(&request.hash) => {
                Ok(Response::new(ShouldInitiateUploadResponse {
                    should_initiate_upload: false,
                    reason: REASON_DEBUGINFO_EQUAL.into(),
                }))
            }
            Some(_) => Ok(Response::new(ShouldInitiateUploadResponse {
                should_initiate_upload: true,
                reason: REASON_DEBUGINFO_NOT_EQUAL.into(),
            })),
            None => Ok(Response::new(ShouldInitiateUploadResponse {
                should_initiate_upload: true,
                reason: REASON_DEBUGINFO_INVALID.into(),
            })),
        }
    }

    fn handle_debuginfod_source(
        &self,
        debuginfo: &Debuginfo,
    ) -> Result<Response<ShouldInitiateUploadResponse>, Status> {
        Ok(Response::new(ShouldInitiateUploadResponse {
            should_initiate_upload: true,
            reason: if !self.is_valid_elf(debuginfo) {
                REASON_DEBUGINFOD_SOURCE.into()
            } else {
                REASON_DEBUGINFOD_INVALID.into()
            },
        }))
    }

    async fn handle_new_build_id(
        &self,
        request: &ShouldInitiateUploadRequest,
    ) -> Result<Response<ShouldInitiateUploadResponse>, Status> {
        if !matches!(
            request.build_id_type(),
            BuildIdType::Gnu | BuildIdType::UnknownUnspecified
        ) {
            return Ok(Response::new(ShouldInitiateUploadResponse {
                should_initiate_upload: true,
                reason: REASON_FIRST_TIME_SEEN.into(),
            }));
        }

        // Check existence outside of the lock
        let build_id = request.build_id.clone();
        let exists = self.debuginfod.exists(&build_id).await;

        if !exists.is_empty() {
            let _ = self
                .metadata
                .mark_as_debuginfod_source(exists, &build_id, &request.r#type());
            Ok(Response::new(ShouldInitiateUploadResponse {
                should_initiate_upload: false,
                reason: REASON_DEBUGINFO_IN_DEBUGINFOD.into(),
            }))
        } else {
            Ok(Response::new(ShouldInitiateUploadResponse {
                should_initiate_upload: true,
                reason: REASON_FIRST_TIME_SEEN.into(),
            }))
        }
    }
}
