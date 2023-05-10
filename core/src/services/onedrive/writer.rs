// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use async_trait::async_trait;
use bytes::{Buf, Bytes};
use http::StatusCode;
use serde::{Deserialize, Serialize};

use super::backend::OnedriveBackend;
use super::error::parse_error;
use crate::ops::OpWrite;
use crate::raw::*;
use crate::*;

pub struct OneDriveWriter {
    backend: OnedriveBackend,

    op: OpWrite,
    path: String,
}

impl OneDriveWriter {
    const MAX_SIMPLE_SIZE: usize = 4 * 1024 * 1024;
    // If your app splits a file into multiple byte ranges, the size of each byte range MUST be a multiple of 320 KiB (327,680 bytes). Using a fragment size that does not divide evenly by 320 KiB will result in errors committing some files.
    // https://learn.microsoft.com/en-us/onedrive/developer/rest-api/api/driveitem_createuploadsession?view=odsp-graph-online#upload-bytes-to-the-upload-session
    const CHUNK_SIZE_FACTOR: usize = 327_680;
    pub fn new(backend: OnedriveBackend, op: OpWrite, path: String) -> Self {
        OneDriveWriter { backend, op, path }
    }
}

#[async_trait]
impl oio::Write for OneDriveWriter {
    async fn write(&mut self, bs: Bytes) -> Result<()> {
        let size = bs.len();

        if size <= Self::MAX_SIMPLE_SIZE {
            self.write_simple(bs).await
        } else {
            self.write_chunked(bs).await
        }
    }

    async fn abort(&mut self) -> Result<()> {
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

impl OneDriveWriter {
    async fn write_simple(&mut self, bs: Bytes) -> Result<()> {
        let resp = self
            .backend
            .onedrive_put(
                &self.path,
                Some(bs.len()),
                self.op.content_type(),
                AsyncBody::Bytes(bs),
            )
            .await?;

        let status = resp.status();

        match status {
            // Typical response code: 201 Created
            // Reference: https://learn.microsoft.com/en-us/onedrive/developer/rest-api/api/driveitem_put_content?view=odsp-graph-online#response
            StatusCode::CREATED | StatusCode::OK => {
                resp.into_body().consume().await?;
                Ok(())
            }
            _ => Err(parse_error(resp).await?),
        }
    }

    pub(crate) async fn write_chunked(&self, total_bytes: Bytes) -> Result<()> {
        // Upload large files via sessions: https://learn.microsoft.com/en-us/onedrive/developer/rest-api/api/driveitem_createuploadsession?view=odsp-graph-online#upload-bytes-to-the-upload-session
        // 1. Create an upload session
        // 2. Upload the bytes of each chunk
        // 3. Commit the session

        let session_response = self.create_upload_session().await?;

        let mut offset = 0;

        let iter = total_bytes.chunks(OneDriveWriter::CHUNK_SIZE_FACTOR);

        for chunk in iter {
            let mut end = offset + OneDriveWriter::CHUNK_SIZE_FACTOR;
            if end > total_bytes.len() {
                end = total_bytes.len();
            }
            let total_len = total_bytes.len();
            let chunk_end = end - 1;

            let resp = self
                .backend
                .onedrive_chunked_upload(
                    &session_response.upload_url,
                    None,
                    offset,
                    chunk_end,
                    total_len,
                    AsyncBody::Bytes(Bytes::copy_from_slice(chunk)),
                )
                .await?;

            let status = resp.status();

            match status {
                // Typical response code: 202 Accepted
                // Reference: https://learn.microsoft.com/en-us/onedrive/developer/rest-api/api/driveitem_put_content?view=odsp-graph-online#response
                StatusCode::ACCEPTED | StatusCode::CREATED | StatusCode::OK => {
                    resp.into_body().consume().await?;
                }
                _ => return Err(parse_error(resp).await?),
            }

            offset += OneDriveWriter::CHUNK_SIZE_FACTOR;
        }

        Ok(())
    }

    async fn create_upload_session(&self) -> Result<OneDriveUploadSessionCreationResponseBody> {
        let file_name_from_path = self.path.split('/').last().ok_or_else(|| {
            Error::new(
                ErrorKind::Unexpected,
                "connection string must have AccountName",
            )
        })?;
        let url = format!(
            "{}/drive/root:{}:/createUploadSession",
            OnedriveBackend::BASE_URL,
            percent_encode_path(&self.path)
        );
        let body = OneDriveUploadSessionCreationRequestBody::new(file_name_from_path.to_string());
        let body_bytes = serde_json::to_vec(&body).map_err(new_json_serialize_error)?;
        let asyn_body = AsyncBody::Bytes(Bytes::from(body_bytes));
        let resp = self.backend.onedrive_post(&url, asyn_body).await?;

        let status = resp.status();

        match status {
            // Reference: https://learn.microsoft.com/en-us/onedrive/developer/rest-api/api/driveitem_createuploadsession?view=odsp-graph-online#response
            StatusCode::OK => {
                // serde to parse body to OneDriveUploadSessionCreationResponseBody
                let bs = resp.into_body().bytes().await?;
                let result: OneDriveUploadSessionCreationResponseBody =
                    serde_json::from_reader(bs.reader()).map_err(new_json_deserialize_error)?;
                Ok(result)
            }
            _ => Err(parse_error(resp).await?),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OneDriveUploadSessionCreationRequestBody {
    item: Item,
}

impl OneDriveUploadSessionCreationRequestBody {
    fn new(path: String) -> Self {
        OneDriveUploadSessionCreationRequestBody {
            item: Item {
                odata_type: "microsoft.graph.driveItemUploadableProperties".to_string(),
                microsoft_graph_conflict_behavior: "replace".to_string(),
                name: path,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Item {
    #[serde(rename = "@odata.type")]
    odata_type: String,
    #[serde(rename = "@microsoft.graph.conflictBehavior")]
    microsoft_graph_conflict_behavior: String,
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OneDriveUploadSessionCreationResponseBody {
    #[serde(rename = "uploadUrl")]
    upload_url: String,
    #[serde(rename = "expirationDateTime")]
    expiration_date_time: String,
}
