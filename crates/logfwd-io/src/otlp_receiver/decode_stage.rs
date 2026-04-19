use std::io;
use std::sync::Arc;

use arrow::record_batch::RecordBatch;
#[cfg(any(feature = "otlp-research", test))]
use bytes::Bytes;

use crate::InputError;
use crate::blocking_stage::{BlockingWorker, BoundedBlockingStage};

use super::OtlpProtobufDecodeMode;
use super::decode::{
    decode_otlp_json, decode_otlp_protobuf_with_prost, decompress_gzip, decompress_zstd,
};
#[cfg(any(feature = "otlp-research", test))]
use super::projection::{ProjectedOtlpDecoder, ProjectionError};

pub(super) type OtlpRequestCpuStage = BoundedBlockingStage<OtlpRequestCpuWorker>;

pub(super) struct OtlpRequestCpuJob {
    pub(super) body: Vec<u8>,
    pub(super) content: OtlpRequestContent,
    pub(super) encoding: OtlpContentEncoding,
    pub(super) max_body_size: usize,
}

pub(super) enum OtlpRequestContent {
    Json,
    Protobuf,
}

#[derive(Clone, Copy)]
pub(super) enum OtlpContentEncoding {
    Identity,
    Gzip,
    Zstd,
}

pub(super) struct OtlpRequestCpuOutput {
    pub(super) batch: RecordBatch,
    pub(super) outcome: OtlpRequestCpuOutcome,
}

pub(super) enum OtlpRequestCpuOutcome {
    Json,
    Prost,
    #[cfg(any(feature = "otlp-research", test))]
    ProjectedSuccess,
    #[cfg(any(feature = "otlp-research", test))]
    ProjectedFallback,
}

#[derive(Debug)]
pub(super) enum OtlpRequestCpuError {
    Decompression(InputError),
    Payload(InputError),
    #[cfg(any(feature = "otlp-research", test))]
    ProjectionInvalid(InputError),
}

pub(super) struct OtlpRequestCpuWorker {
    resource_prefix: Arc<str>,
    mode: OtlpProtobufDecodeMode,
    #[cfg(any(feature = "otlp-research", test))]
    projected_decoder: Option<ProjectedOtlpDecoder>,
}

impl OtlpRequestCpuWorker {
    fn new(resource_prefix: Arc<str>, mode: OtlpProtobufDecodeMode) -> Self {
        #[cfg(any(feature = "otlp-research", test))]
        let projected_decoder = if mode == OtlpProtobufDecodeMode::Prost {
            None
        } else {
            Some(ProjectedOtlpDecoder::new(resource_prefix.as_ref()))
        };
        Self {
            resource_prefix,
            mode,
            #[cfg(any(feature = "otlp-research", test))]
            projected_decoder,
        }
    }

    fn decode_json(&self, body: &[u8]) -> Result<OtlpRequestCpuOutput, OtlpRequestCpuError> {
        decode_otlp_json(body, self.resource_prefix.as_ref())
            .map(|batch| OtlpRequestCpuOutput {
                batch,
                outcome: OtlpRequestCpuOutcome::Json,
            })
            .map_err(OtlpRequestCpuError::Payload)
    }

    fn decode_prost(&self, body: &[u8]) -> Result<OtlpRequestCpuOutput, OtlpRequestCpuError> {
        decode_otlp_protobuf_with_prost(body, self.resource_prefix.as_ref())
            .map(|batch| OtlpRequestCpuOutput {
                batch,
                outcome: OtlpRequestCpuOutcome::Prost,
            })
            .map_err(OtlpRequestCpuError::Payload)
    }

    #[cfg(any(feature = "otlp-research", test))]
    fn decode_projected_fallback(
        &mut self,
        body: Vec<u8>,
    ) -> Result<OtlpRequestCpuOutput, OtlpRequestCpuError> {
        let body = Bytes::from(body);
        match self.decode_projected_view(body.clone()) {
            Ok(batch) => Ok(OtlpRequestCpuOutput {
                batch,
                outcome: OtlpRequestCpuOutcome::ProjectedSuccess,
            }),
            Err(ProjectionError::Unsupported(_)) => {
                decode_otlp_protobuf_with_prost(&body, self.resource_prefix.as_ref())
                    .map(|batch| OtlpRequestCpuOutput {
                        batch,
                        outcome: OtlpRequestCpuOutcome::ProjectedFallback,
                    })
                    .map_err(OtlpRequestCpuError::Payload)
            }
            Err(err) => Err(OtlpRequestCpuError::ProjectionInvalid(
                err.into_input_error(),
            )),
        }
    }

    #[cfg(any(feature = "otlp-research", test))]
    fn decode_projected_only(
        &mut self,
        body: Vec<u8>,
    ) -> Result<OtlpRequestCpuOutput, OtlpRequestCpuError> {
        match self.decode_projected_view(Bytes::from(body)) {
            Ok(batch) => Ok(OtlpRequestCpuOutput {
                batch,
                outcome: OtlpRequestCpuOutcome::ProjectedSuccess,
            }),
            Err(ProjectionError::Unsupported(err)) => Err(OtlpRequestCpuError::Payload(
                ProjectionError::Unsupported(err).into_input_error(),
            )),
            Err(err) => Err(OtlpRequestCpuError::ProjectionInvalid(
                err.into_input_error(),
            )),
        }
    }

    #[cfg(any(feature = "otlp-research", test))]
    fn decode_projected_view(&mut self, body: Bytes) -> Result<RecordBatch, ProjectionError> {
        let Some(decoder) = self.projected_decoder.as_mut() else {
            return Err(ProjectionError::Invalid(
                "projected decoder not initialized",
            ));
        };
        decoder.try_decode_view_bytes(body)
    }
}

impl BlockingWorker for OtlpRequestCpuWorker {
    type Job = OtlpRequestCpuJob;
    type Output = OtlpRequestCpuOutput;
    type Error = OtlpRequestCpuError;

    fn process(&mut self, job: Self::Job) -> Result<Self::Output, Self::Error> {
        #[cfg(test)]
        {
            if job.body == b"__panic_worker__" {
                panic!("otlp decode worker panic requested by test");
            }
            if job.body == b"__sleep_worker__" {
                std::thread::sleep(std::time::Duration::from_millis(200));
                return self.decode_prost(&[]);
            }
        }

        let body = decompress_request_body(job.body, job.encoding, job.max_body_size)
            .map_err(OtlpRequestCpuError::Decompression)?;

        match job.content {
            OtlpRequestContent::Json => self.decode_json(&body),
            OtlpRequestContent::Protobuf => match self.mode {
                OtlpProtobufDecodeMode::Prost => self.decode_prost(&body),
                #[cfg(any(feature = "otlp-research", test))]
                OtlpProtobufDecodeMode::ProjectedFallback => self.decode_projected_fallback(body),
                #[cfg(any(feature = "otlp-research", test))]
                OtlpProtobufDecodeMode::ProjectedOnly => self.decode_projected_only(body),
            },
        }
    }
}

fn decompress_request_body(
    body: Vec<u8>,
    encoding: OtlpContentEncoding,
    max_body_size: usize,
) -> Result<Vec<u8>, InputError> {
    match encoding {
        OtlpContentEncoding::Identity => Ok(body),
        OtlpContentEncoding::Gzip => decompress_gzip(&body, max_body_size),
        OtlpContentEncoding::Zstd => decompress_zstd(&body, max_body_size),
    }
}

pub(super) fn build_otlp_request_cpu_stage(
    worker_count: usize,
    max_outstanding: usize,
    resource_prefix: Arc<str>,
    mode: OtlpProtobufDecodeMode,
) -> io::Result<OtlpRequestCpuStage> {
    BoundedBlockingStage::new(worker_count, max_outstanding, move |_| {
        OtlpRequestCpuWorker::new(Arc::clone(&resource_prefix), mode)
    })
}
