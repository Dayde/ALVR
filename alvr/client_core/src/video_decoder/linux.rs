// Hardware video decoder for desktop Linux, using FFmpeg with VAAPI. Decoded surfaces are exported
// as dma-bufs (DRM PRIME) and imported by the renderer as EGL images, so frames stay on the GPU.

use super::{DecoderCapabilities, VideoDecoderConfig};
use alvr_common::{
    RelaxedAtomic,
    anyhow::{Result, anyhow, bail},
    dbg_decoder, error, info,
    parking_lot::Mutex,
    warn,
};
use alvr_graphics::{
    ChromaLocation, DRM_FORMAT_GR88, DRM_FORMAT_GR1616, DRM_FORMAT_R8, DRM_FORMAT_R16, DmabufFrame,
    DmabufImage, DmabufPlane, MAX_DMABUF_PLANES, YuvColorSpace, YuvFormat,
};
use alvr_session::CodecType;
use ffmpeg_sys_next as ff;
use std::{
    collections::VecDeque,
    ffi::{CStr, CString, c_char, c_int, c_void},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError, TrySendError},
    },
    thread,
    time::Duration,
};

// libva, from va/va.h. FFmpeg's VAAPI support already depends on it. Only used to query what the
// driver can decode.
#[link(name = "va")]
unsafe extern "C" {
    fn vaMaxNumProfiles(display: *mut c_void) -> c_int;
    fn vaQueryConfigProfiles(
        display: *mut c_void,
        profiles: *mut c_int,
        profile_count: *mut c_int,
    ) -> c_int;
    fn vaMaxNumEntrypoints(display: *mut c_void) -> c_int;
    fn vaQueryConfigEntrypoints(
        display: *mut c_void,
        profile: c_int,
        entrypoints: *mut c_int,
        entrypoint_count: *mut c_int,
    ) -> c_int;
    fn vaGetConfigAttributes(
        display: *mut c_void,
        profile: c_int,
        entrypoint: c_int,
        attributes: *mut VAConfigAttrib,
        attribute_count: c_int,
    ) -> c_int;
    fn vaErrorStr(status: c_int) -> *const c_char;
}

// Surfaces allocated on top of what the decoder needs: the queue plus the one being rendered
const EXTRA_HARDWARE_FRAMES: c_int = 4;
const MAX_QUEUED_FRAMES: usize = 2;
// Packets waiting for the decoder thread. When full, the sink rejects packets, which makes the
// connection request an IDR (like when MediaCodec has no free input buffer).
const MAX_QUEUED_PACKETS: usize = 4;
// Packets rejected in a row by the decoder before giving up and restarting the connection. Single
// rejections (corrupt packets) are recovered by requesting an IDR. 90 is about one second at 90
// fps, since every packet reaches the decoder (with the default avoid_video_glitching off).
const MAX_CONSECUTIVE_PACKET_ERRORS: u32 = 90;
// How often the decoder thread checks if the source has been dropped while no packet arrives
const RUNNING_CHECK_INTERVAL: Duration = Duration::from_millis(100);
// Overrides the VAAPI device, for example /dev/dri/renderD129. By default FFmpeg picks the first
// render node that works.
const VAAPI_DEVICE_ENV_VAR: &str = "ALVR_VAAPI_DEVICE";

// From va/va.h
const VA_STATUS_SUCCESS: c_int = 0;
const VA_PROFILE_HEVC_MAIN_10: c_int = 18;
const VA_PROFILE_AV1_PROFILE_0: c_int = 32;
const VA_ENTRYPOINT_VLD: c_int = 1;
const VA_CONFIG_ATTRIBUTE_RT_FORMAT: c_int = 0;
const VA_RT_FORMAT_YUV420_10: u32 = 0x100;
const VA_ATTRIBUTE_NOT_SUPPORTED: u32 = 0x8000_0000;

// From libavutil/hwcontext_drm.h, which ffmpeg-sys-next does not bind. These structs are part of
// FFmpeg's public ABI.
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

#[repr(C)]
struct VAConfigAttrib {
    attribute_type: c_int,
    value: u32,
}

struct DecodedFrame {
    timestamp: Duration,
    // Keeps the VAAPI surface alive while the renderer samples it
    vaapi_frame: *mut ff::AVFrame,
    // Owns the dma-buf file descriptors
    drm_frame: *mut ff::AVFrame,
    dmabuf: DmabufFrame,
}

// The frames are reference counted handles that can be released from any thread
unsafe impl Send for DecodedFrame {}

impl Drop for DecodedFrame {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.drm_frame);
            ff::av_frame_free(&mut self.vaapi_frame);
        }
    }
}

struct Decoder {
    codec_context: *mut ff::AVCodecContext,
    packet: *mut ff::AVPacket,
    // Set by get_vaapi_format() through the codec context's opaque pointer
    hardware_format_rejected: Box<AtomicBool>,
}

impl Decoder {
    fn new(config: &VideoDecoderConfig) -> Result<Self> {
        let codec_id = match config.codec {
            CodecType::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
            CodecType::Hevc => ff::AVCodecID::AV_CODEC_ID_HEVC,
            CodecType::AV1 => ff::AVCodecID::AV_CODEC_ID_AV1,
        };

        let maybe_device_path = std::env::var(VAAPI_DEVICE_ENV_VAR).ok();

        unsafe {
            // avcodec_find_decoder() can return a software only decoder, for example libdav1d for
            // AV1
            let Some(codec) = find_vaapi_decoder(codec_id) else {
                bail!(
                    "FFmpeg has no {:?} decoder that supports VAAPI",
                    config.codec
                );
            };

            let mut codec_context = ff::avcodec_alloc_context3(codec);
            if codec_context.is_null() {
                bail!("avcodec_alloc_context3 failed");
            }

            let hardware_device_context = match create_vaapi_device(maybe_device_path.as_deref()) {
                Ok(hardware_device_context) => hardware_device_context,
                Err(e) => {
                    ff::avcodec_free_context(&mut codec_context);

                    return Err(e);
                }
            };
            let hardware_format_rejected = Box::new(AtomicBool::new(false));
            (*codec_context).hw_device_ctx = hardware_device_context;
            (*codec_context).opaque = ptr::from_ref(&*hardware_format_rejected).cast_mut().cast();
            (*codec_context).get_format = Some(get_vaapi_format);
            (*codec_context).extra_hw_frames = EXTRA_HARDWARE_FRAMES;
            (*codec_context).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int;
            (*codec_context).thread_count = 1;

            // The parameter sets (VPS/SPS/PPS or the AV1 sequence header) arrive separately from
            // the frames, so they are given to the decoder as extradata
            if !config.config_buffer.is_empty() {
                let size = config.config_buffer.len();
                let extradata =
                    ff::av_mallocz(size + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize).cast::<u8>();
                if extradata.is_null() {
                    ff::avcodec_free_context(&mut codec_context);
                    bail!("av_mallocz failed");
                }
                ptr::copy_nonoverlapping(config.config_buffer.as_ptr(), extradata, size);
                (*codec_context).extradata = extradata;
                (*codec_context).extradata_size = size as c_int;
            }

            let result = ff::avcodec_open2(codec_context, codec, ptr::null_mut());
            if result < 0 {
                ff::avcodec_free_context(&mut codec_context);
                bail!("avcodec_open2 failed: {}", av_error_string(result));
            }

            let packet = ff::av_packet_alloc();
            if packet.is_null() {
                ff::avcodec_free_context(&mut codec_context);
                bail!("av_packet_alloc failed");
            }

            info!(
                "Opened VAAPI decoder {} for {:?} on {}",
                CStr::from_ptr((*codec).name).to_string_lossy(),
                config.codec,
                maybe_device_path.as_deref().unwrap_or("the default device")
            );

            Ok(Self {
                codec_context,
                packet,
                hardware_format_rejected,
            })
        }
    }

    // Returns false if the decoder rejected the packet, which can be recovered with an IDR. Errors
    // are fatal.
    fn send_nal(&mut self, timestamp: Duration, nal: &[u8]) -> Result<bool> {
        let result = unsafe {
            let result = ff::av_new_packet(self.packet, nal.len() as c_int);
            if result < 0 {
                bail!("av_new_packet failed: {}", av_error_string(result));
            }
            ptr::copy_nonoverlapping(nal.as_ptr(), (*self.packet).data, nal.len());
            (*self.packet).pts = timestamp.as_nanos() as i64;

            let result = ff::avcodec_send_packet(self.codec_context, self.packet);
            ff::av_packet_unref(self.packet);

            result
        };

        if self.hardware_format_rejected.load(Ordering::Relaxed) {
            bail!(
                "VAAPI cannot decode this stream, the GPU may not support its codec, profile or \
                bit depth"
            );
        }

        if result == eagain() {
            // Should not happen since all frames are drained after every packet
            warn!("VAAPI decoder is full, dropping frame");

            Ok(false)
        } else if result < 0 {
            warn!("avcodec_send_packet failed: {}", av_error_string(result));

            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        unsafe {
            let mut vaapi_frame = ff::av_frame_alloc();
            if vaapi_frame.is_null() {
                bail!("av_frame_alloc failed");
            }

            // Frames without timestamp cannot be matched with their metadata and are skipped
            loop {
                let result = ff::avcodec_receive_frame(self.codec_context, vaapi_frame);
                if result == eagain() || result == ff::AVERROR_EOF {
                    ff::av_frame_free(&mut vaapi_frame);

                    return Ok(None);
                } else if result < 0 {
                    ff::av_frame_free(&mut vaapi_frame);
                    bail!("avcodec_receive_frame failed: {}", av_error_string(result));
                }

                if (*vaapi_frame).best_effort_timestamp != ff::AV_NOPTS_VALUE {
                    break;
                }

                dbg_decoder!("Skipped decoded frame without timestamp");
                ff::av_frame_unref(vaapi_frame);
            }

            let mut drm_frame = ff::av_frame_alloc();
            if drm_frame.is_null() {
                ff::av_frame_free(&mut vaapi_frame);
                bail!("av_frame_alloc failed");
            }
            (*drm_frame).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            let result =
                ff::av_hwframe_map(drm_frame, vaapi_frame, ff::AV_HWFRAME_MAP_READ as c_int);
            if result < 0 {
                ff::av_frame_free(&mut drm_frame);
                ff::av_frame_free(&mut vaapi_frame);
                bail!(
                    "Cannot export VAAPI frame as dma-buf: {}",
                    av_error_string(result)
                );
            }

            let descriptor = &*(*drm_frame).data[0].cast::<AVDRMFrameDescriptor>();
            let dmabuf = match dmabuf_from_descriptor(descriptor, &*vaapi_frame) {
                Ok(dmabuf) => dmabuf,
                Err(e) => {
                    ff::av_frame_free(&mut drm_frame);
                    ff::av_frame_free(&mut vaapi_frame);

                    return Err(e);
                }
            };

            Ok(Some(DecodedFrame {
                timestamp: Duration::from_nanos((*vaapi_frame).best_effort_timestamp as u64),
                vaapi_frame,
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
            ff::avcodec_free_context(&mut self.codec_context);
        }
    }
}

fn av_error_string(code: c_int) -> String {
    let mut buffer = [0 as c_char; 256];

    unsafe {
        ff::av_strerror(code, buffer.as_mut_ptr(), buffer.len());

        CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}

fn eagain() -> c_int {
    -(ff::EAGAIN as c_int)
}

// Opens the given DRM render node, or FFmpeg's default VAAPI device. The returned
// AVHWDeviceContext reference must be released with av_buffer_unref().
fn create_vaapi_device(maybe_device_path: Option<&str>) -> Result<*mut ff::AVBufferRef> {
    let maybe_device_path_c = maybe_device_path.map(CString::new).transpose()?;

    let mut hardware_device_context = ptr::null_mut();
    let result = unsafe {
        ff::av_hwdevice_ctx_create(
            &mut hardware_device_context,
            ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            maybe_device_path_c
                .as_ref()
                .map_or(ptr::null(), |path| path.as_ptr()),
            ptr::null_mut(),
            0,
        )
    };
    if result < 0 {
        bail!(
            "Cannot open VAAPI device {}: {}",
            maybe_device_path.unwrap_or("(default)"),
            av_error_string(result)
        );
    }

    Ok(hardware_device_context)
}

fn check_va_status(status: c_int, function_name: &str) -> Result<()> {
    if status == VA_STATUS_SUCCESS {
        Ok(())
    } else {
        // Safety: vaErrorStr() returns a static string for any status
        let message = unsafe { CStr::from_ptr(vaErrorStr(status)) };
        bail!("{function_name} failed: {}", message.to_string_lossy())
    }
}

// Lists the profiles the driver can decode (VLD entrypoint), with their render target formats
// (VA_RT_FORMAT_*, 0 if unknown)
unsafe fn query_decode_profiles(display: *mut c_void) -> Result<Vec<(c_int, u32)>> {
    let mut profiles = vec![0; unsafe { vaMaxNumProfiles(display) }.max(0) as usize];
    let mut profile_count = 0;
    check_va_status(
        unsafe { vaQueryConfigProfiles(display, profiles.as_mut_ptr(), &mut profile_count) },
        "vaQueryConfigProfiles",
    )?;
    profiles.truncate(profile_count.max(0) as usize);

    let max_entrypoint_count = unsafe { vaMaxNumEntrypoints(display) }.max(0) as usize;
    let mut decode_profiles = vec![];
    for profile in profiles {
        let mut entrypoints = vec![0; max_entrypoint_count];
        let mut entrypoint_count = 0;
        let status = unsafe {
            vaQueryConfigEntrypoints(
                display,
                profile,
                entrypoints.as_mut_ptr(),
                &mut entrypoint_count,
            )
        };
        entrypoints.truncate(entrypoint_count.max(0) as usize);
        if status != VA_STATUS_SUCCESS || !entrypoints.contains(&VA_ENTRYPOINT_VLD) {
            continue;
        }

        let mut attribute = VAConfigAttrib {
            attribute_type: VA_CONFIG_ATTRIBUTE_RT_FORMAT,
            value: 0,
        };
        let status = unsafe {
            vaGetConfigAttributes(display, profile, VA_ENTRYPOINT_VLD, &mut attribute, 1)
        };
        let render_target_formats =
            if status == VA_STATUS_SUCCESS && attribute.value != VA_ATTRIBUTE_NOT_SUPPORTED {
                attribute.value
            } else {
                0
            };

        decode_profiles.push((profile, render_target_formats));
    }

    Ok(decode_profiles)
}

// ALVR's 10 bit setting applies to whichever of HEVC or AV1 the streamer selects, so 10 bit is
// only advertised if both advertised codecs can be decoded in 10 bit. AV1 also needs an FFmpeg AV1
// decoder with VAAPI support.
fn capabilities_from_decode_profiles(
    decode_profiles: &[(c_int, u32)],
    av1_decoder_available: bool,
) -> DecoderCapabilities {
    let render_target_formats = |profile| {
        decode_profiles
            .iter()
            .find(|(decode_profile, _)| *decode_profile == profile)
            .map(|(_, formats)| *formats)
    };

    let maybe_av1_formats =
        render_target_formats(VA_PROFILE_AV1_PROFILE_0).filter(|_| av1_decoder_available);
    let av1_10_bits =
        maybe_av1_formats.is_some_and(|formats| formats & VA_RT_FORMAT_YUV420_10 != 0);
    let hevc_10_bits = render_target_formats(VA_PROFILE_HEVC_MAIN_10).is_some();

    DecoderCapabilities {
        av1: maybe_av1_formats.is_some(),
        ten_bits: hevc_10_bits && (maybe_av1_formats.is_none() || av1_10_bits),
    }
}

// Returns the first decoder for `codec_id` that can decode with a VAAPI device
unsafe fn find_vaapi_decoder(codec_id: ff::AVCodecID) -> Option<*const ff::AVCodec> {
    let mut iteration_state = ptr::null_mut();
    loop {
        let codec = unsafe { ff::av_codec_iterate(&mut iteration_state) };
        if codec.is_null() {
            return None;
        }

        if unsafe { (*codec).id == codec_id && ff::av_codec_is_decoder(codec) != 0 }
            && unsafe { supports_vaapi(codec) }
        {
            return Some(codec);
        }
    }
}

unsafe fn supports_vaapi(codec: *const ff::AVCodec) -> bool {
    for index in 0.. {
        let config = unsafe { ff::avcodec_get_hw_config(codec, index) };
        let Some(config) = (unsafe { config.as_ref() }) else {
            return false;
        };

        if config.device_type == ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI
            && config.pix_fmt == ff::AVPixelFormat::AV_PIX_FMT_VAAPI
            && config.methods & ff::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int != 0
        {
            return true;
        }
    }

    false
}

// FFmpeg only offers the VAAPI format if the driver supports the stream's profile, and offers the
// list again without it if the hardware decoder fails to initialize
unsafe extern "C" fn get_vaapi_format(
    codec_context: *mut ff::AVCodecContext,
    mut formats: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    unsafe {
        while *formats != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *formats == ff::AVPixelFormat::AV_PIX_FMT_VAAPI {
                return ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            }
            formats = formats.add(1);
        }

        // Safety: opaque is set by Decoder::new to a flag that outlives the codec context
        if let Some(hardware_format_rejected) =
            (*codec_context).opaque.cast::<AtomicBool>().as_ref()
        {
            hardware_format_rejected.store(true, Ordering::Relaxed);
        }
    }

    error!("VAAPI cannot decode this stream");

    ff::AVPixelFormat::AV_PIX_FMT_NONE
}

fn layer_planes(layer: &AVDRMLayerDescriptor) -> Result<&[AVDRMPlaneDescriptor]> {
    layer
        .planes
        .get(..layer.nb_planes as usize)
        .ok_or_else(|| anyhow!("Invalid dma-buf plane count"))
}

fn dmabuf_from_descriptor(
    descriptor: &AVDRMFrameDescriptor,
    frame: &ff::AVFrame,
) -> Result<DmabufFrame> {
    let objects = descriptor
        .objects
        .get(..descriptor.nb_objects as usize)
        .ok_or_else(|| anyhow!("Invalid dma-buf object count"))?;
    let layers = descriptor
        .layers
        .get(..descriptor.nb_layers as usize)
        .ok_or_else(|| anyhow!("Invalid dma-buf layer count"))?;

    let image = |drm_fourcc, planes: &[AVDRMPlaneDescriptor]| -> Result<DmabufImage> {
        let mut image = DmabufImage {
            drm_fourcc,
            plane_count: planes.len().min(MAX_DMABUF_PLANES),
            ..Default::default()
        };
        for (image_plane, plane) in image.planes.iter_mut().zip(planes) {
            let object = objects
                .get(plane.object_index as usize)
                .ok_or_else(|| anyhow!("Invalid dma-buf object index {}", plane.object_index))?;

            *image_plane = DmabufPlane {
                fd: object.fd,
                offset: plane.offset as u32,
                pitch: plane.pitch as u32,
                modifier: object.format_modifier,
            };
        }

        Ok(image)
    };

    let (format, images) = match layers {
        // One multi-planar layer (NV12, P010), split in two images
        [layer] => {
            let Some(format) = YuvFormat::from_drm_fourcc(layer.format) else {
                bail!("Unsupported decoded frame format {:#x}", layer.format);
            };
            let [luma_plane, chroma_plane] = layer_planes(layer)? else {
                bail!(
                    "Expected 2 planes in the {format:?} layer, got {}",
                    layer.nb_planes
                );
            };
            let [luma_fourcc, chroma_fourcc] = format.image_drm_fourccs();

            (
                format,
                [
                    image(luma_fourcc, &[*luma_plane])?,
                    image(chroma_fourcc, &[*chroma_plane])?,
                ],
            )
        }
        // One layer per plane (R8 + GR88, R16 + GR1616), each with all its planes, which include
        // the auxiliary planes of compressed modifiers
        [luma_layer, chroma_layer] => {
            let format = match (luma_layer.format, chroma_layer.format) {
                (DRM_FORMAT_R8, DRM_FORMAT_GR88) => YuvFormat::Nv12,
                (DRM_FORMAT_R16, DRM_FORMAT_GR1616) => YuvFormat::P010,
                (luma_fourcc, chroma_fourcc) => bail!(
                    "Unsupported decoded frame layer formats {luma_fourcc:#x} and \
                    {chroma_fourcc:#x}"
                ),
            };

            (
                format,
                [
                    image(luma_layer.format, layer_planes(luma_layer)?)?,
                    image(chroma_layer.format, layer_planes(chroma_layer)?)?,
                ],
            )
        }
        _ => bail!("Unsupported decoded frame with {} layers", layers.len()),
    };

    let color_space = match frame.colorspace {
        ff::AVColorSpace::AVCOL_SPC_BT470BG | ff::AVColorSpace::AVCOL_SPC_SMPTE170M => {
            YuvColorSpace::Bt601
        }
        ff::AVColorSpace::AVCOL_SPC_BT2020_NCL | ff::AVColorSpace::AVCOL_SPC_BT2020_CL => {
            YuvColorSpace::Bt2020
        }
        // Includes unspecified
        _ => YuvColorSpace::Bt709,
    };

    // FFmpeg reports the H.264 and HEVC default (left) when the stream doesn't specify it. AV1
    // streams usually leave it unspecified, which is also rendered as left sited, the usual 4:2:0
    // siting and what mpv/libplacebo assume for unknown locations.
    let chroma_location = match frame.chroma_location {
        ff::AVChromaLocation::AVCHROMA_LOC_CENTER => ChromaLocation::Center,
        ff::AVChromaLocation::AVCHROMA_LOC_TOPLEFT => ChromaLocation::TopLeft,
        ff::AVChromaLocation::AVCHROMA_LOC_TOP => ChromaLocation::Top,
        ff::AVChromaLocation::AVCHROMA_LOC_BOTTOMLEFT => ChromaLocation::BottomLeft,
        ff::AVChromaLocation::AVCHROMA_LOC_BOTTOM => ChromaLocation::Bottom,
        // Includes left and unspecified
        _ => ChromaLocation::Left,
    };

    Ok(DmabufFrame {
        width: frame.width as u32,
        height: frame.height as u32,
        format,
        images,
        color_space,
        full_range: frame.color_range == ff::AVColorRange::AVCOL_RANGE_JPEG,
        chroma_location,
    })
}

type FrameQueue = Arc<Mutex<VecDeque<DecodedFrame>>>;

pub struct VideoDecoderSink {
    nal_sender: mpsc::SyncSender<(Duration, Vec<u8>)>,
    running: Arc<RelaxedAtomic>,
    idr_needed: Arc<AtomicBool>,
}

impl VideoDecoderSink {
    // Returning false makes the connection request an IDR. This happens when the decoder rejected
    // a packet or is saturated, and once the source has been dropped (like the MediaCodec sink),
    // so that the streamer sends a fresh decoder config for the next stream.
    pub fn push_frame_nal(&mut self, timestamp: Duration, nal: &[u8]) -> Result<bool> {
        if !self.running.value() {
            return Ok(false);
        }

        // The packet is still decoded when an IDR is requested, it may be the IDR the decoder
        // waits for
        let idr_needed = self.idr_needed.swap(false, Ordering::Relaxed);

        match self.nal_sender.try_send((timestamp, nal.to_vec())) {
            Ok(()) => Ok(!idr_needed),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => Ok(false),
        }
    }
}

pub struct VideoDecoderSource {
    frames: FrameQueue,
    current_frame: Option<DecodedFrame>,
    running: Arc<RelaxedAtomic>,
}

impl VideoDecoderSource {
    /// Returns the newest decoded frame, as a pointer to an alvr_graphics::DmabufFrame which stays
    /// valid until the next call that returns a frame.
    pub fn dequeue_frame(&mut self) -> Option<(Duration, *mut c_void)> {
        let frame = {
            let mut frames = self.frames.lock();
            let frame = frames.pop_back()?;
            // Older frames would only add latency
            frames.clear();

            frame
        };
        let current_frame = self.current_frame.insert(frame);

        Some((
            current_frame.timestamp,
            ptr::from_mut(&mut current_frame.dmabuf).cast(),
        ))
    }
}

impl Drop for VideoDecoderSource {
    fn drop(&mut self) {
        // The decoder thread exits and releases the VAAPI device
        self.running.set(false);
    }
}

fn decoder_loop(
    mut decoder: Decoder,
    nal_receiver: mpsc::Receiver<(Duration, Vec<u8>)>,
    frames: FrameQueue,
    running: &RelaxedAtomic,
    idr_needed: &AtomicBool,
    report_frame_decoded: impl Fn(Result<Duration>),
) {
    let mut consecutive_packet_errors = 0;
    while running.value() {
        let (timestamp, nal) = match nal_receiver.recv_timeout(RUNNING_CHECK_INTERVAL) {
            Ok(packet) => packet,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };

        match decoder.send_nal(timestamp, &nal) {
            Ok(true) => consecutive_packet_errors = 0,
            Ok(false) => {
                consecutive_packet_errors += 1;
                if consecutive_packet_errors >= MAX_CONSECUTIVE_PACKET_ERRORS {
                    report_frame_decoded(Err(anyhow!(
                        "VAAPI decoder rejected {consecutive_packet_errors} packets in a row"
                    )));
                    return;
                }

                idr_needed.store(true, Ordering::Relaxed);
            }
            Err(e) => {
                report_frame_decoded(Err(e));
                return;
            }
        }

        loop {
            match decoder.receive_frame() {
                Ok(Some(frame)) => {
                    let timestamp = frame.timestamp;

                    {
                        let mut frames = frames.lock();
                        // Drop stale frames instead of adding latency
                        while frames.len() >= MAX_QUEUED_FRAMES {
                            frames.pop_front();
                            dbg_decoder!("Dropped stale decoded frame");
                        }
                        frames.push_back(frame);
                    }

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

/// Asks the VAAPI driver of the decoding device which optional stream features it can decode
pub fn vaapi_capabilities() -> Result<DecoderCapabilities> {
    let mut hardware_device_context =
        create_vaapi_device(std::env::var(VAAPI_DEVICE_ENV_VAR).ok().as_deref())?;

    let maybe_decode_profiles = unsafe {
        let device_context = &*(*hardware_device_context)
            .data
            .cast::<ff::AVHWDeviceContext>();
        // AVVAAPIDeviceContext (libavutil/hwcontext_vaapi.h) starts with the VADisplay
        let display = *device_context.hwctx.cast::<*mut c_void>();

        query_decode_profiles(display)
    };
    unsafe { ff::av_buffer_unref(&mut hardware_device_context) };

    let av1_decoder_available =
        unsafe { find_vaapi_decoder(ff::AVCodecID::AV_CODEC_ID_AV1) }.is_some();

    Ok(capabilities_from_decode_profiles(
        &maybe_decode_profiles?,
        av1_decoder_available,
    ))
}

pub fn video_decoder_split(
    config: VideoDecoderConfig,
    report_frame_decoded: impl Fn(Result<Duration>) + Send + 'static,
) -> Result<(VideoDecoderSink, VideoDecoderSource)> {
    let (nal_sender, nal_receiver) = mpsc::sync_channel(MAX_QUEUED_PACKETS);
    let frames = FrameQueue::default();
    let running = Arc::new(RelaxedAtomic::new(true));
    let idr_needed = Arc::new(AtomicBool::new(false));

    thread::Builder::new().name("VAAPI decoder".into()).spawn({
        let frames = Arc::clone(&frames);
        let running = Arc::clone(&running);
        let idr_needed = Arc::clone(&idr_needed);
        move || match Decoder::new(&config) {
            Ok(decoder) => decoder_loop(
                decoder,
                nal_receiver,
                frames,
                &running,
                &idr_needed,
                report_frame_decoded,
            ),
            Err(e) => report_frame_decoded(Err(e)),
        }
    })?;

    Ok((
        VideoDecoderSink {
            nal_sender,
            running: Arc::clone(&running),
            idr_needed,
        },
        VideoDecoderSource {
            frames,
            current_frame: None,
            running,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alvr_graphics::{DRM_FORMAT_NV12, DRM_FORMAT_P010};

    const PLANE: AVDRMPlaneDescriptor = AVDRMPlaneDescriptor {
        object_index: 0,
        offset: 0,
        pitch: 0,
    };
    const LAYER: AVDRMLayerDescriptor = AVDRMLayerDescriptor {
        format: 0,
        nb_planes: 0,
        planes: [PLANE; AV_DRM_MAX_PLANES],
    };

    // Object index, offset and pitch
    type PlaneParameters = (c_int, isize, isize);

    // Object i has fd 40 + i and modifier 7 + i
    fn descriptor(
        object_count: c_int,
        layers: &[(u32, &[PlaneParameters])],
    ) -> AVDRMFrameDescriptor {
        let mut descriptor = AVDRMFrameDescriptor {
            nb_objects: object_count,
            objects: [0, 1, 2, 3].map(|index| AVDRMObjectDescriptor {
                fd: 40 + index,
                size: 0,
                format_modifier: 7 + index as u64,
            }),
            nb_layers: layers.len() as c_int,
            layers: [LAYER; AV_DRM_MAX_PLANES],
        };
        for (layer, (format, planes)) in descriptor.layers.iter_mut().zip(layers) {
            layer.format = *format;
            layer.nb_planes = planes.len() as c_int;
            for (plane, (object_index, offset, pitch)) in layer.planes.iter_mut().zip(*planes) {
                plane.object_index = *object_index;
                plane.offset = *offset;
                plane.pitch = *pitch;
            }
        }

        descriptor
    }

    fn frame(colorspace: ff::AVColorSpace, color_range: ff::AVColorRange) -> Box<ff::AVFrame> {
        // Safety: AVFrame is plain data, all zeroes is a valid value
        let mut frame = Box::new(unsafe { std::mem::zeroed::<ff::AVFrame>() });
        frame.width = 3840;
        frame.height = 1920;
        frame.colorspace = colorspace;
        frame.color_range = color_range;
        frame.chroma_location = ff::AVChromaLocation::AVCHROMA_LOC_LEFT;

        frame
    }

    #[test]
    fn single_layer_nv12() {
        let descriptor = descriptor(
            1,
            &[(DRM_FORMAT_NV12, &[(0, 0, 4096), (0, 8_000_000, 4096)])],
        );
        let frame = frame(
            ff::AVColorSpace::AVCOL_SPC_BT709,
            ff::AVColorRange::AVCOL_RANGE_JPEG,
        );

        let dmabuf = dmabuf_from_descriptor(&descriptor, &frame).unwrap();

        assert_eq!(dmabuf.format, YuvFormat::Nv12);
        assert_eq!((dmabuf.width, dmabuf.height), (3840, 1920));
        let [luma, chroma] = dmabuf.images;
        assert_eq!((luma.drm_fourcc, luma.plane_count), (DRM_FORMAT_R8, 1));
        assert_eq!(
            (chroma.drm_fourcc, chroma.plane_count),
            (DRM_FORMAT_GR88, 1)
        );
        assert_eq!(chroma.planes[0].offset, 8_000_000);
        assert_eq!(chroma.planes[0].pitch, 4096);
        assert_eq!((luma.planes[0].fd, luma.planes[0].modifier), (40, 7));
        assert_eq!(dmabuf.color_space, YuvColorSpace::Bt709);
        assert!(dmabuf.full_range);
        assert_eq!(dmabuf.chroma_location, ChromaLocation::Left);
    }

    #[test]
    fn separate_layers_p010() {
        let descriptor = descriptor(
            1,
            &[
                (DRM_FORMAT_R16, &[(0, 0, 8192)]),
                (DRM_FORMAT_GR1616, &[(0, 16_000_000, 8192)]),
            ],
        );
        let frame = frame(
            ff::AVColorSpace::AVCOL_SPC_BT2020_NCL,
            ff::AVColorRange::AVCOL_RANGE_MPEG,
        );

        let dmabuf = dmabuf_from_descriptor(&descriptor, &frame).unwrap();

        assert_eq!(dmabuf.format, YuvFormat::P010);
        assert_eq!(dmabuf.images[1].drm_fourcc, DRM_FORMAT_GR1616);
        assert_eq!(dmabuf.images[1].planes[0].offset, 16_000_000);
        assert_eq!(dmabuf.color_space, YuvColorSpace::Bt2020);
        assert!(!dmabuf.full_range);
    }

    // Compressed modifiers (for example Intel's CCS) add an auxiliary plane to each layer
    #[test]
    fn separate_layers_with_auxiliary_planes() {
        let descriptor = descriptor(
            2,
            &[
                (DRM_FORMAT_R8, &[(0, 0, 4096), (1, 0, 512)]),
                (
                    DRM_FORMAT_GR88,
                    &[(0, 8_000_000, 4096), (1, 1_000_000, 512)],
                ),
            ],
        );
        let frame = frame(
            ff::AVColorSpace::AVCOL_SPC_BT709,
            ff::AVColorRange::AVCOL_RANGE_MPEG,
        );

        let dmabuf = dmabuf_from_descriptor(&descriptor, &frame).unwrap();

        assert_eq!(dmabuf.format, YuvFormat::Nv12);
        let [luma, chroma] = dmabuf.images;
        assert_eq!((luma.drm_fourcc, luma.plane_count), (DRM_FORMAT_R8, 2));
        assert_eq!(
            (chroma.drm_fourcc, chroma.plane_count),
            (DRM_FORMAT_GR88, 2)
        );
        assert_eq!((luma.planes[1].fd, luma.planes[1].pitch), (41, 512));
        assert_eq!(
            (chroma.planes[1].offset, chroma.planes[1].modifier),
            (1_000_000, 8)
        );
    }

    #[test]
    fn unspecified_color_space_is_bt709() {
        let descriptor = descriptor(
            1,
            &[
                (DRM_FORMAT_R8, &[(0, 0, 4096)]),
                (DRM_FORMAT_GR88, &[(0, 8_000_000, 4096)]),
            ],
        );
        let mut frame = frame(
            ff::AVColorSpace::AVCOL_SPC_UNSPECIFIED,
            ff::AVColorRange::AVCOL_RANGE_UNSPECIFIED,
        );
        frame.chroma_location = ff::AVChromaLocation::AVCHROMA_LOC_UNSPECIFIED;

        let dmabuf = dmabuf_from_descriptor(&descriptor, &frame).unwrap();

        assert_eq!(dmabuf.format, YuvFormat::Nv12);
        assert_eq!(dmabuf.color_space, YuvColorSpace::Bt709);
        assert!(!dmabuf.full_range);
        assert_eq!(dmabuf.chroma_location, ChromaLocation::Left);

        frame.chroma_location = ff::AVChromaLocation::AVCHROMA_LOC_CENTER;
        let dmabuf = dmabuf_from_descriptor(&descriptor, &frame).unwrap();
        assert_eq!(dmabuf.chroma_location, ChromaLocation::Center);
    }

    #[test]
    fn capabilities_from_driver_profiles() {
        const VA_PROFILE_HEVC_MAIN: c_int = 17;
        const VA_RT_FORMAT_YUV420: u32 = 0x1;
        let both_depths = VA_RT_FORMAT_YUV420 | VA_RT_FORMAT_YUV420_10;
        let capabilities = |profiles: &[(c_int, u32)], av1_decoder_available| {
            let capabilities = capabilities_from_decode_profiles(profiles, av1_decoder_available);

            (capabilities.av1, capabilities.ten_bits)
        };

        // AV1 and HEVC Main10 (AMD RDNA2+, Intel Gen12+)
        let modern = [
            (VA_PROFILE_HEVC_MAIN, VA_RT_FORMAT_YUV420),
            (VA_PROFILE_HEVC_MAIN_10, VA_RT_FORMAT_YUV420_10),
            (VA_PROFILE_AV1_PROFILE_0, both_depths),
        ];
        assert_eq!(capabilities(&modern, true), (true, true));
        // FFmpeg without a VAAPI AV1 decoder
        assert_eq!(capabilities(&modern, false), (false, true));

        // HEVC Main10 without AV1 (AMD before RDNA2, Intel Gen9.5-11)
        let without_av1 = [
            (VA_PROFILE_HEVC_MAIN, VA_RT_FORMAT_YUV420),
            (VA_PROFILE_HEVC_MAIN_10, VA_RT_FORMAT_YUV420_10),
        ];
        assert_eq!(capabilities(&without_av1, true), (false, true));

        // 8 bit only HEVC
        let hevc_main = [(VA_PROFILE_HEVC_MAIN, VA_RT_FORMAT_YUV420)];
        assert_eq!(capabilities(&hevc_main, true), (false, false));

        // AV1 without 10 bit render targets: AV1 is kept, 10 bit is not offered for either codec
        let av1_8_bits = [
            (VA_PROFILE_HEVC_MAIN_10, VA_RT_FORMAT_YUV420_10),
            (VA_PROFILE_AV1_PROFILE_0, VA_RT_FORMAT_YUV420),
        ];
        assert_eq!(capabilities(&av1_8_bits, true), (true, false));
        // Unknown render target formats count as 8 bit only
        let av1_unknown_formats = [(VA_PROFILE_HEVC_MAIN_10, 0), (VA_PROFILE_AV1_PROFILE_0, 0)];
        assert_eq!(capabilities(&av1_unknown_formats, true), (true, false));

        assert_eq!(capabilities(&[], true), (false, false));
    }

    #[test]
    fn unsupported_layouts() {
        let frame = frame(
            ff::AVColorSpace::AVCOL_SPC_BT709,
            ff::AVColorRange::AVCOL_RANGE_MPEG,
        );

        // RGB frame
        let rgb = descriptor(1, &[(u32::from_le_bytes(*b"XR24"), &[(0, 0, 4096)])]);
        assert!(dmabuf_from_descriptor(&rgb, &frame).is_err());

        // NV12 without its chroma plane
        let missing_plane = descriptor(1, &[(DRM_FORMAT_NV12, &[(0, 0, 4096)])]);
        assert!(dmabuf_from_descriptor(&missing_plane, &frame).is_err());

        // Mismatched layer formats
        let mismatched = descriptor(
            1,
            &[
                (DRM_FORMAT_R8, &[(0, 0, 4096)]),
                (DRM_FORMAT_GR1616, &[(0, 8_000_000, 4096)]),
            ],
        );
        assert!(dmabuf_from_descriptor(&mismatched, &frame).is_err());

        // Plane referencing an object past nb_objects
        let bad_object = descriptor(
            1,
            &[(DRM_FORMAT_P010, &[(0, 0, 4096), (1, 8_000_000, 4096)])],
        );
        assert!(dmabuf_from_descriptor(&bad_object, &frame).is_err());
    }
}
