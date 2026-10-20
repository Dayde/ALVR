// Hardware video decoder for desktop Linux, using FFmpeg with VAAPI. Decoded surfaces are exported
// as dma-bufs (DRM PRIME) and imported by the renderer as EGL images, so frames never leave the GPU.

use super::VideoDecoderConfig;
use alvr_common::{
    anyhow::{anyhow, bail, Result},
    error,
    parking_lot::Mutex,
    warn,
};
use alvr_graphics::{DmabufFrame, DmabufPlane, YuvColorSpace, MAX_DMABUF_PLANES};
use alvr_session::CodecType;
use ffmpeg_sys_next as ff;
use std::{
    collections::VecDeque,
    ffi::{c_int, c_void, CString},
    ptr,
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};

// Frames kept by us on top of what the decoder needs: the queue plus the one being rendered
const EXTRA_HW_FRAMES: c_int = 4;
const MAX_QUEUED_FRAMES: usize = 2;

// libavutil/hwcontext_drm.h. These structs are part of FFmpeg's stable public ABI.
const AV_DRM_MAX_PLANES: usize = 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct AVDRMObjectDescriptor {
    fd: c_int,
    size: usize,
    format_modifier: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AVDRMPlaneDescriptor {
    object_index: c_int,
    offset: isize,
    pitch: isize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AVDRMLayerDescriptor {
    format: u32,
    nb_planes: c_int,
    planes: [AVDRMPlaneDescriptor; AV_DRM_MAX_PLANES],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AVDRMFrameDescriptor {
    nb_objects: c_int,
    objects: [AVDRMObjectDescriptor; AV_DRM_MAX_PLANES],
    nb_layers: c_int,
    layers: [AVDRMLayerDescriptor; AV_DRM_MAX_PLANES],
}

const fn fourcc(code: &[u8; 4]) -> u32 {
    (code[0] as u32) | (code[1] as u32) << 8 | (code[2] as u32) << 16 | (code[3] as u32) << 24
}

const DRM_FORMAT_R8: u32 = fourcc(b"R8  ");
const DRM_FORMAT_R16: u32 = fourcc(b"R16 ");
const DRM_FORMAT_NV12: u32 = fourcc(b"NV12");
const DRM_FORMAT_P010: u32 = fourcc(b"P010");

fn av_error(code: c_int) -> String {
    let mut buf = [0 as std::ffi::c_char; 256];
    unsafe { ff::av_strerror(code, buf.as_mut_ptr(), buf.len()) };

    unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn eagain() -> c_int {
    -(ff::EAGAIN as c_int)
}

struct DecodedFrame {
    timestamp: Duration,
    // Keeps the VAAPI surface alive while the renderer samples it
    va_frame: *mut ff::AVFrame,
    drm_frame: *mut ff::AVFrame,
    dmabuf: DmabufFrame,
}

// The frames are only reference counted handles, they can be released from any thread
unsafe impl Send for DecodedFrame {}

impl Drop for DecodedFrame {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.drm_frame);
            ff::av_frame_free(&mut self.va_frame);
        }
    }
}

unsafe extern "C" fn get_vaapi_format(
    _: *mut ff::AVCodecContext,
    mut formats: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    while *formats != ff::AVPixelFormat::AV_PIX_FMT_NONE {
        if *formats == ff::AVPixelFormat::AV_PIX_FMT_VAAPI {
            return ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
        }
        formats = formats.add(1);
    }

    error!("VAAPI cannot decode this stream");

    ff::AVPixelFormat::AV_PIX_FMT_NONE
}

struct Decoder {
    codec_ctx: *mut ff::AVCodecContext,
    packet: *mut ff::AVPacket,
}

impl Decoder {
    fn new(config: &VideoDecoderConfig) -> Result<Self> {
        let codec_id = match config.codec {
            CodecType::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
            CodecType::Hevc => ff::AVCodecID::AV_CODEC_ID_HEVC,
            CodecType::AV1 => ff::AVCodecID::AV_CODEC_ID_AV1,
        };

        unsafe {
            let codec = ff::avcodec_find_decoder(codec_id);
            if codec.is_null() {
                bail!("FFmpeg has no decoder for {:?}", config.codec);
            }

            let mut codec_ctx = ff::avcodec_alloc_context3(codec);
            if codec_ctx.is_null() {
                bail!("avcodec_alloc_context3 failed");
            }

            let device_path = std::env::var("ALVR_VAAPI_DEVICE")
                .unwrap_or_else(|_| "/dev/dri/renderD128".into());
            let device_path_c = CString::new(device_path.clone())?;
            let mut hw_device_ctx = ptr::null_mut();
            let res = ff::av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                device_path_c.as_ptr(),
                ptr::null_mut(),
                0,
            );
            if res < 0 {
                ff::avcodec_free_context(&mut codec_ctx);
                bail!("Cannot open VAAPI device {device_path}: {}", av_error(res));
            }
            (*codec_ctx).hw_device_ctx = hw_device_ctx;
            (*codec_ctx).get_format = Some(get_vaapi_format);
            (*codec_ctx).extra_hw_frames = EXTRA_HW_FRAMES;
            (*codec_ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int;
            (*codec_ctx).thread_count = 1;

            // The config NALs (VPS/SPS/PPS or the AV1 sequence header) arrive separately from
            // the frames, so they are given to the decoder as extradata
            if !config.config_buffer.is_empty() {
                let size = config.config_buffer.len();
                let extradata =
                    ff::av_mallocz(size + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize) as *mut u8;
                ptr::copy_nonoverlapping(config.config_buffer.as_ptr(), extradata, size);
                (*codec_ctx).extradata = extradata;
                (*codec_ctx).extradata_size = size as c_int;
            }

            let res = ff::avcodec_open2(codec_ctx, codec, ptr::null_mut());
            if res < 0 {
                ff::avcodec_free_context(&mut codec_ctx);
                bail!("avcodec_open2 failed: {}", av_error(res));
            }

            let packet = ff::av_packet_alloc();

            // Printed directly: once connected, info logs are filtered by the streamer settings
            eprintln!(
                "Opened VAAPI decoder for {:?} on {device_path}",
                config.codec
            );

            Ok(Self { codec_ctx, packet })
        }
    }

    fn send_nal(&mut self, timestamp: Duration, nal: &[u8]) -> Result<()> {
        unsafe {
            let res = ff::av_new_packet(self.packet, nal.len() as c_int);
            if res < 0 {
                bail!("av_new_packet failed: {}", av_error(res));
            }
            ptr::copy_nonoverlapping(nal.as_ptr(), (*self.packet).data, nal.len());
            (*self.packet).pts = timestamp.as_nanos() as i64;

            let res = ff::avcodec_send_packet(self.codec_ctx, self.packet);
            ff::av_packet_unref(self.packet);

            if res == eagain() {
                // Should not happen since all frames are drained after every packet
                warn!("Decoder is full, dropping frame");
            } else if res < 0 {
                // A corrupt frame is not fatal, the stream recovers at the next IDR
                warn!("avcodec_send_packet failed: {}", av_error(res));
            }

            Ok(())
        }
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        unsafe {
            let mut va_frame = ff::av_frame_alloc();
            let res = ff::avcodec_receive_frame(self.codec_ctx, va_frame);
            if res == eagain() || res == ff::AVERROR_EOF {
                ff::av_frame_free(&mut va_frame);
                return Ok(None);
            } else if res < 0 {
                ff::av_frame_free(&mut va_frame);
                bail!("avcodec_receive_frame failed: {}", av_error(res));
            }

            let mut drm_frame = ff::av_frame_alloc();
            (*drm_frame).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            let res = ff::av_hwframe_map(
                drm_frame,
                va_frame,
                ff::AV_HWFRAME_MAP_READ as c_int,
            );
            if res < 0 {
                ff::av_frame_free(&mut drm_frame);
                ff::av_frame_free(&mut va_frame);
                bail!("Cannot export VAAPI frame as dma-buf: {}", av_error(res));
            }

            let desc = &*((*drm_frame).data[0] as *const AVDRMFrameDescriptor);
            let dmabuf = dmabuf_from_descriptor(desc, &*va_frame)?;

            Ok(Some(DecodedFrame {
                timestamp: Duration::from_nanos((*va_frame).pts as u64),
                va_frame,
                drm_frame,
                dmabuf,
            }))
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_packet_free(&mut self.packet);
            ff::avcodec_free_context(&mut self.codec_ctx);
        }
    }
}

fn dmabuf_from_descriptor(
    desc: &AVDRMFrameDescriptor,
    frame: &ff::AVFrame,
) -> Result<DmabufFrame> {
    // FFmpeg exports VAAPI surfaces with one layer per plane (R8 + GR88 for NV12). EGL wants a
    // single multi-planar image, so the planes of all layers are merged.
    let layers = &desc.layers[..desc.nb_layers as usize];
    let drm_format = match (layers.len(), layers.first().map(|l| l.format)) {
        (1, Some(format)) => format,
        (2, Some(DRM_FORMAT_R8)) => DRM_FORMAT_NV12,
        (2, Some(DRM_FORMAT_R16)) => DRM_FORMAT_P010,
        (count, format) => bail!("Unsupported dma-buf layout: {count} layers, format {format:?}"),
    };

    let mut planes = [DmabufPlane::default(); MAX_DMABUF_PLANES];
    let mut plane_count = 0;
    for layer in layers {
        for plane in &layer.planes[..layer.nb_planes as usize] {
            if plane_count == MAX_DMABUF_PLANES {
                bail!("Too many dma-buf planes");
            }
            let object = desc
                .objects
                .get(plane.object_index as usize)
                .ok_or_else(|| anyhow!("Invalid dma-buf object index"))?;
            planes[plane_count] = DmabufPlane {
                fd: object.fd,
                offset: plane.offset as u32,
                pitch: plane.pitch as u32,
                modifier: object.format_modifier,
            };
            plane_count += 1;
        }
    }

    let color_space = match frame.colorspace {
        ff::AVColorSpace::AVCOL_SPC_BT470BG | ff::AVColorSpace::AVCOL_SPC_SMPTE170M => {
            YuvColorSpace::Bt601
        }
        ff::AVColorSpace::AVCOL_SPC_BT2020_NCL | ff::AVColorSpace::AVCOL_SPC_BT2020_CL => {
            YuvColorSpace::Bt2020
        }
        _ => YuvColorSpace::Bt709,
    };

    Ok(DmabufFrame {
        width: frame.width as u32,
        height: frame.height as u32,
        drm_format,
        plane_count,
        planes,
        color_space,
        full_range: frame.color_range == ff::AVColorRange::AVCOL_RANGE_JPEG,
    })
}

type FrameQueue = Arc<Mutex<VecDeque<DecodedFrame>>>;

pub struct VideoDecoderSink {
    nal_sender: mpsc::Sender<(Duration, Vec<u8>)>,
}

impl VideoDecoderSink {
    pub fn push_frame_nal(&mut self, timestamp: Duration, nal: &[u8]) -> Result<bool> {
        // Fails only if the decoder thread stopped, which has been reported already
        Ok(self.nal_sender.send((timestamp, nal.to_vec())).is_ok())
    }
}

pub struct VideoDecoderSource {
    frames: FrameQueue,
    current_frame: Option<DecodedFrame>,
}

impl VideoDecoderSource {
    /// Returns the newest decoded frame. The pointer is a DmabufFrame which stays valid until the
    /// next call that returns a frame.
    pub fn dequeue_frame(&mut self) -> Option<(Duration, *mut c_void)> {
        // Skip to the newest frame, older ones would only add latency
        let frame = self.frames.lock().drain(..).last()?;
        let current_frame = self.current_frame.insert(frame);

        Some((
            current_frame.timestamp,
            ptr::from_mut(&mut current_frame.dmabuf).cast(),
        ))
    }
}

fn decoder_loop(
    mut decoder: Decoder,
    nal_receiver: mpsc::Receiver<(Duration, Vec<u8>)>,
    frames: FrameQueue,
    report_frame_decoded: impl Fn(Result<Duration>),
) {
    // Ends when the sink is dropped
    while let Ok((timestamp, nal)) = nal_receiver.recv() {
        if let Err(e) = decoder.send_nal(timestamp, &nal) {
            report_frame_decoded(Err(e));
            return;
        }

        loop {
            match decoder.receive_frame() {
                Ok(Some(frame)) => {
                    let timestamp = frame.timestamp;

                    let mut frames = frames.lock();
                    // Drop stale frames instead of adding latency
                    while frames.len() >= MAX_QUEUED_FRAMES {
                        frames.pop_front();
                    }
                    frames.push_back(frame);
                    drop(frames);

                    report_frame_decoded(Ok(timestamp));
                }
                Ok(None) => break,
                Err(e) => {
                    report_frame_decoded(Err(e));
                    return;
                }
            }
        }
    }
}

pub fn video_decoder_split(
    config: VideoDecoderConfig,
    report_frame_decoded: impl Fn(Result<Duration>) + Send + Sync + 'static,
) -> Result<(VideoDecoderSink, VideoDecoderSource)> {
    let (nal_sender, nal_receiver) = mpsc::channel();
    let frames = FrameQueue::default();

    thread::Builder::new()
        .name("VAAPI decoder".into())
        .spawn({
            let frames = Arc::clone(&frames);
            move || match Decoder::new(&config) {
                Ok(decoder) => decoder_loop(decoder, nal_receiver, frames, report_frame_decoded),
                Err(e) => report_frame_decoded(Err(e)),
            }
        })?;

    Ok((
        VideoDecoderSink { nal_sender },
        VideoDecoderSource {
            frames,
            current_frame: None,
        },
    ))
}
