//! Shared S3-compatible client.
//!
//! Generalizes what used to live only in `cloud/r2.rs`: a thin wrapper over
//! `aws-sdk-s3` bound to one bucket, with an explicit endpoint URL and inline
//! access-key credentials (the shape every S3-compatible service speaks —
//! MinIO, Backblaze B2, Cloudflare R2, Garage, Wasabi, `rclone serve s3`, …).
//!
//! Compiled whenever `s3-backend` is on (which `cloud` implies), so both the
//! self-hosted S3 blob backend (`store::S3Store`) and the cloud R2 client
//! (`cloud::r2::R2Store`) build on the same object primitives instead of
//! duplicating the SDK plumbing. `cloud/r2.rs` keeps only the R2-specific
//! extras on top (presigned URLs, key builders).

use anyhow::{Context, Result};
use aws_credential_types::Credentials;
use aws_sdk_s3::{
    config::{BehaviorVersion, Region},
    primitives::ByteStream,
    Client,
};
use std::path::Path;

/// Connection parameters for an S3-compatible endpoint. Deliberately a plain
/// struct (not tied to any config type) so both the self-hosted
/// `[storage.s3]` block and the cloud `[cloud.r2]` block can build one.
#[derive(Clone)]
pub struct S3Params {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// MinIO, Garage and `rclone serve s3` need path-style addressing
    /// (`endpoint/bucket/key`); most others accept it too. R2 forces it.
    pub force_path_style: bool,
}

/// An `aws-sdk-s3` client bound to a single bucket, plus the small set of
/// object operations Hoard needs. Everything higher-level (presigning, key
/// schemes) lives in the callers.
pub struct S3 {
    client: Client,
    bucket: String,
}

impl S3 {
    /// Build a client. The explicit `endpoint_url` is what makes this speak to
    /// an arbitrary S3-compatible host rather than Amazon.
    pub async fn connect(p: S3Params) -> Result<Self> {
        if p.endpoint.is_empty() || p.bucket.is_empty() {
            anyhow::bail!("s3 endpoint and bucket are required");
        }
        let creds = Credentials::new(
            p.access_key_id,
            p.secret_access_key,
            None,
            None,
            "hoard-s3-static",
        );
        let region = if p.region.is_empty() {
            // Many S3-compatibles ignore region but the SDK requires one.
            "auto".to_string()
        } else {
            p.region
        };
        let sdk_conf = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(region))
            .credentials_provider(creds)
            .endpoint_url(p.endpoint)
            .load()
            .await;

        let s3_conf = aws_sdk_s3::config::Builder::from(&sdk_conf)
            .force_path_style(p.force_path_style)
            .build();

        Ok(Self {
            client: Client::from_conf(s3_conf),
            bucket: p.bucket,
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Direct PUT of an in-memory body — for small objects the server builds
    /// itself (export ZIPs, manifests, the write probe). Buffers the whole
    /// body; use [`Self::put_file`] for anything large.
    pub async fn put_object(&self, key: &str, body: Vec<u8>) -> Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body.into())
            .send()
            .await
            .with_context(|| format!("s3 put_object {key}"))?;
        Ok(())
    }

    /// Streaming PUT from a file on disk. Keeps the body off the heap — the SDK
    /// reads the file incrementally. Used for blob/chunk finalization and
    /// account-export ZIPs, which can be GB-sized.
    pub async fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        let body = ByteStream::from_path(path)
            .await
            .with_context(|| format!("s3 put_file open {}", path.display()))?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body)
            .send()
            .await
            .with_context(|| format!("s3 put_file {key}"))?;
        Ok(())
    }

    /// GET the whole object into memory. Only for objects known to be small
    /// (export ZIPs re-read by the cloud worker). Snapshot downloads must use
    /// [`Self::get_to_file`] so a multi-GB save never lands on the heap.
    pub async fn get_object(&self, key: &str) -> Result<Vec<u8>> {
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("s3 get_object {key}"))?;
        let bytes = out
            .body
            .collect()
            .await
            .with_context(|| format!("s3 read body {key}"))?
            .into_bytes()
            .to_vec();
        Ok(bytes)
    }

    /// Stream the object to a local file, bounded memory (the SDK body is
    /// piped straight to disk, never fully buffered). This is the primitive
    /// the S3 blob backend uses to spool a remote blob/chunk into `tmp/`
    /// before it's fed into a snapshot download tarball.
    pub async fn get_to_file(&self, key: &str, dest: &Path) -> Result<()> {
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("s3 get_object {key}"))?;
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("s3 spool mkdir {}", parent.display()))?;
        }
        let mut reader = out.body.into_async_read();
        let mut file = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("s3 spool create {}", dest.display()))?;
        tokio::io::copy(&mut reader, &mut file)
            .await
            .with_context(|| format!("s3 spool {key}"))?;
        tokio::io::AsyncWriteExt::flush(&mut file).await.ok();
        Ok(())
    }

    /// Open the object as a bounded-memory async reader. The caller streams
    /// it wherever it needs (a decompressor, a response body) without the
    /// object ever landing whole on the heap.
    pub async fn get_reader(&self, key: &str) -> Result<impl tokio::io::AsyncBufRead + Unpin> {
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("s3 get_object {key}"))?;
        Ok(out.body.into_async_read())
    }

    /// Streaming PUT from an arbitrary reader, bounded memory (one part
    /// buffer at a time). Bodies that fit in one part go out as a single
    /// PUT (one Class A op); larger ones use multipart. Returns the total
    /// bytes written. Either way the upload is atomic: the key serves its
    /// previous content until the PUT/`complete` succeeds, and an error
    /// path aborts the multipart so no half-written object ever becomes
    /// visible.
    pub async fn put_from_reader<R>(&self, key: &str, mut reader: R) -> Result<i64>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
        use tokio::io::AsyncReadExt;

        // S3 minimum part size is 5 MiB (except the last part); 8 MiB keeps
        // part counts low without hurting the 512 MB machine.
        const PART_SIZE: usize = 8 * 1024 * 1024;

        // Buffer the first part up front: a body that fits in one part goes
        // out as a single PUT — one Class A op instead of multipart's three
        // (create + part + complete), and most save files are far below the
        // part size. Only genuinely multi-part bodies pay for multipart.
        let mut first = Vec::with_capacity(PART_SIZE);
        while first.len() < PART_SIZE {
            let n = (&mut reader)
                .take((PART_SIZE - first.len()) as u64)
                .read_to_end(&mut first)
                .await
                .context("s3 put_from_reader read")?;
            if n == 0 {
                break;
            }
        }
        if first.len() < PART_SIZE {
            let total = first.len() as i64;
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(first.into())
                .send()
                .await
                .with_context(|| format!("s3 put_from_reader single put {key}"))?;
            return Ok(total);
        }

        let mp = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("s3 create_multipart {key}"))?;
        let upload_id = mp
            .upload_id()
            .context("s3 create_multipart returned no upload id")?
            .to_string();

        let upload = async {
            let mut parts: Vec<CompletedPart> = Vec::new();
            let mut total: i64 = 0;
            let mut part_number = 1i32;
            // Part 1 is the buffer already read above — a full part; smaller
            // bodies (including empty) took the single-PUT path.
            let mut buf = first;
            loop {
                // An empty part means the reader drained on a part boundary.
                if buf.is_empty() {
                    break;
                }
                let done = buf.len() < PART_SIZE;
                total += buf.len() as i64;
                let out = self
                    .client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .part_number(part_number)
                    .body(buf.into())
                    .send()
                    .await
                    .with_context(|| format!("s3 upload_part {part_number} {key}"))?;
                parts.push(
                    CompletedPart::builder()
                        .part_number(part_number)
                        .set_e_tag(out.e_tag().map(str::to_string))
                        .build(),
                );
                part_number += 1;
                if done {
                    break;
                }
                buf = Vec::with_capacity(PART_SIZE);
                while buf.len() < PART_SIZE {
                    let n = (&mut reader)
                        .take((PART_SIZE - buf.len()) as u64)
                        .read_to_end(&mut buf)
                        .await
                        .context("s3 multipart read")?;
                    if n == 0 {
                        break;
                    }
                }
            }
            self.client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(parts))
                        .build(),
                )
                .send()
                .await
                .with_context(|| format!("s3 complete_multipart {key}"))?;
            Ok(total)
        }
        .await;

        if upload.is_err() {
            // Best-effort: leave no dangling multipart charging storage.
            let _ = self
                .client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .send()
                .await;
        }
        upload
    }

    /// `Some(size)` if the object exists, `None` on a clean 404.
    pub async fn head(&self, key: &str) -> Result<Option<i64>> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(out) => Ok(out.content_length()),
            Err(e) => {
                // 404 is the expected miss. Detect it via the typed service
                // error rather than the Display string — MinIO, R2 and S3 all
                // render the message differently, but `is_not_found()` is
                // uniform. Anything else (auth, outage) must bubble up: treating
                // a transient error as "absent" would let callers re-create
                // objects the user already has.
                if e.as_service_error().map(|se| se.is_not_found()) == Some(true) {
                    Ok(None)
                } else {
                    Err(e).context("s3 head_object")
                }
            }
        }
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("s3 delete_object {key}"))?;
        Ok(())
    }
}
