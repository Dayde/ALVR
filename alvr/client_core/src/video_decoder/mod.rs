#[cfg(target_os = "android")]
mod android;
#[cfg(all(target_os = "linux", feature = "vaapi"))]
mod linux;

use alvr_common::anyhow::Result;
use alvr_session::{CodecType, MediacodecProperty};
use std::time::Duration;

#[derive(Clone, Default, PartialEq)]
pub struct VideoDecoderConfig {
    pub codec: CodecType,
    pub force_software_decoder: bool,
    pub max_buffering_frames: f32,
    pub buffering_history_weight: f32,
    pub options: Vec<(String, MediacodecProperty)>,
    pub config_buffer: Vec<u8>,
}

/// Optional stream features that the GPU can decode, which vary between desktop GPUs
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecoderCapabilities {
    pub av1: bool,
    /// 10 bit HEVC, and 10 bit AV1 if AV1 is supported
    pub ten_bits: bool,
}

pub struct VideoDecoderSink {
    #[cfg(target_os = "android")]
    inner: android::VideoDecoderSink,
    #[cfg(all(target_os = "linux", feature = "vaapi"))]
    inner: linux::VideoDecoderSink,
}

impl VideoDecoderSink {
    // returns true if frame has been successfully enqueued
    #[allow(unused_variables)]
    pub fn push_nal(&mut self, timestamp: Duration, nal: &[u8]) -> bool {
        #[cfg(any(target_os = "android", all(target_os = "linux", feature = "vaapi")))]
        {
            alvr_common::show_err(self.inner.push_frame_nal(timestamp, nal)).unwrap_or(false)
        }
        #[cfg(not(any(target_os = "android", all(target_os = "linux", feature = "vaapi"))))]
        false
    }
}

pub struct VideoDecoderSource {
    #[cfg(target_os = "android")]
    inner: android::VideoDecoderSource,
    #[cfg(all(target_os = "linux", feature = "vaapi"))]
    inner: linux::VideoDecoderSource,
}

impl VideoDecoderSource {
    /// If a frame is available, return the timestamp and the AHardwareBuffer (a pointer to an
    /// alvr_graphics::DmabufFrame on Linux).
    pub fn get_frame(&mut self) -> Option<(Duration, *mut std::ffi::c_void)> {
        #[cfg(any(target_os = "android", all(target_os = "linux", feature = "vaapi")))]
        {
            self.inner.dequeue_frame()
        }
        #[cfg(not(any(target_os = "android", all(target_os = "linux", feature = "vaapi"))))]
        None
    }
}

/// Queries the VAAPI driver. The streamer only falls back to HEVC and 8 bit when the client does
/// not advertise AV1 and 10 bit, so a feature that cannot be confirmed is not advertised.
#[cfg(target_os = "linux")]
pub fn probe_decoder_capabilities() -> DecoderCapabilities {
    #[cfg(feature = "vaapi")]
    match linux::vaapi_capabilities() {
        Ok(capabilities) => {
            alvr_common::info!(
                "VAAPI decoder capabilities: AV1 {}, 10 bit {}",
                capabilities.av1,
                capabilities.ten_bits
            );

            capabilities
        }
        Err(e) => {
            alvr_common::warn!(
                "Cannot query the VAAPI decoder, AV1 and 10 bit are not advertised: {e:#}"
            );

            DecoderCapabilities::default()
        }
    }
    #[cfg(not(feature = "vaapi"))]
    DecoderCapabilities::default()
}

// report_frame_decoded: (target_timestamp: Duration) -> ()
#[allow(unused_variables)]
pub fn create_decoder(
    config: VideoDecoderConfig,
    report_frame_decoded: impl Fn(Result<Duration>) + Send + Sync + 'static,
) -> (VideoDecoderSink, VideoDecoderSource) {
    #[cfg(target_os = "android")]
    {
        let (sink, source) = android::video_decoder_split(
            config.clone(),
            config.config_buffer,
            report_frame_decoded,
        )
        .unwrap();

        (
            VideoDecoderSink { inner: sink },
            VideoDecoderSource { inner: source },
        )
    }
    #[cfg(all(target_os = "linux", feature = "vaapi"))]
    {
        let (sink, source) = linux::video_decoder_split(config, report_frame_decoded).unwrap();

        (
            VideoDecoderSink { inner: sink },
            VideoDecoderSource { inner: source },
        )
    }
    #[cfg(not(any(target_os = "android", all(target_os = "linux", feature = "vaapi"))))]
    (VideoDecoderSink {}, VideoDecoderSource {})
}
