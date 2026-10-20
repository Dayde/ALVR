mod lobby;
mod staging;
mod stream;

pub use lobby::*;
pub use stream::*;

use alvr_common::{
    glam::{Mat4, UVec2, Vec4},
    Fov,
};
use glow::{self as gl, HasContext};
use khronos_egl as egl;
use std::{ffi::c_void, num::NonZeroU32, ptr};
use wgpu::{
    hal::{self, api, MemoryFlags, TextureUses},
    Adapter, Device, Extent3d, Instance, Queue, Texture, TextureDescriptor, TextureDimension,
    TextureFormat, TextureUsages, TextureView,
};

pub const SDR_FORMAT: TextureFormat = TextureFormat::Rgba8Unorm;
pub const SDR_FORMAT_GL: u32 = gl::RGBA8;
pub const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;
pub const MAX_PUSH_CONSTANTS_SIZE: u32 = 128;

#[cfg(target_os = "android")]
const EGL_LIBRARY_NAME: &str = "libEGL.so";
#[cfg(not(target_os = "android"))]
const EGL_LIBRARY_NAME: &str = "libEGL.so.1";

type CreateImageFn = unsafe extern "C" fn(
    egl::EGLDisplay,
    egl::EGLContext,
    egl::Enum,
    egl::EGLClientBuffer,
    *const egl::Int,
) -> egl::EGLImage;
type DestroyImageFn = unsafe extern "C" fn(egl::EGLDisplay, egl::EGLImage) -> egl::Boolean;
type GetNativeClientBufferFn = unsafe extern "C" fn(*const c_void) -> egl::EGLClientBuffer;
type ImageTargetTexture2DFn = unsafe extern "C" fn(egl::Enum, egl::EGLImage);
pub type EglGetProcAddressFn = unsafe extern "C" fn(*const std::ffi::c_char) -> *const c_void;

pub const MAX_DMABUF_PLANES: usize = 4;

#[derive(Clone, Copy, Debug, Default)]
pub struct DmabufPlane {
    pub fd: i32,
    pub offset: u32,
    pub pitch: u32,
    pub modifier: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum YuvColorSpace {
    Bt601,
    #[default]
    Bt709,
    Bt2020,
}

/// A decoded video frame exported as a Linux dma-buf (for example a VAAPI surface). This is what
/// the Linux decoder hands to the renderer in place of an AHardwareBuffer.
#[derive(Clone, Copy, Debug, Default)]
pub struct DmabufFrame {
    pub width: u32,
    pub height: u32,
    /// DRM fourcc of the whole (multi-planar) image, for example NV12 or P010
    pub drm_format: u32,
    pub plane_count: usize,
    pub planes: [DmabufPlane; MAX_DMABUF_PLANES],
    /// DRM fourcc of each plane viewed on its own, for example R8 and GR88 for NV12
    pub plane_formats: [u32; MAX_DMABUF_PLANES],
    pub color_space: YuvColorSpace,
    pub full_range: bool,
}

pub fn check_error(gl: &gl::Context, message_context: &str) {
    let err = unsafe { gl.get_error() };
    if err != glow::NO_ERROR {
        alvr_common::error!("gl error {message_context} -> {err}");
        std::process::abort();
    }
}

macro_rules! ck {
    ($gl_ctx:ident.$($gl_cmd:tt)*) => {{
        let res = $gl_ctx.$($gl_cmd)*;

        #[cfg(debug_assertions)]
        crate::check_error(&$gl_ctx, &format!("{}:{}: {}", file!(), line!(), stringify!($($gl_cmd)*)));

        res
    }};
}
pub(crate) use ck;

fn projection_from_fov(fov: Fov) -> Mat4 {
    const NEAR: f32 = 0.1;

    let tanl = f32::tan(fov.left);
    let tanr = f32::tan(fov.right);
    let tanu = f32::tan(fov.up);
    let tand = f32::tan(fov.down);
    let a = 2.0 / (tanr - tanl);
    let b = 2.0 / (tanu - tand);
    let c = (tanr + tanl) / (tanr - tanl);
    let d = (tanu + tand) / (tanu - tand);

    // note: for wgpu compatibility, the b and d components should be flipped. Maybe a bug in the
    // viewport handling in wgpu?
    Mat4::from_cols(
        Vec4::new(a, 0.0, c, 0.0),
        Vec4::new(0.0, -b, -d, 0.0),
        Vec4::new(0.0, 0.0, -1.0, -NEAR),
        Vec4::new(0.0, 0.0, -1.0, 0.0),
    )
    .transpose()
}

pub fn choose_swapchain_format(supported_formats: &[u32], enable_hdr: bool) -> u32 {
    // Priority-sorted list of swapchain formats we'll accept--
    let mut app_supported_swapchain_formats = vec![gl::SRGB8_ALPHA8, gl::RGBA8];

    // float16 is required for HDR output. However, float16 swapchains
    // have a high perf cost, so only use these if HDR is enabled.
    if enable_hdr {
        app_supported_swapchain_formats.insert(0, gl::RGBA16F);
    }

    for format in app_supported_swapchain_formats {
        if supported_formats.contains(&format) {
            return format;
        }
    }

    // If we can't enumerate, default to a required format
    gl::RGBA8
}

pub fn gl_format_to_wgpu(format: u32) -> TextureFormat {
    match format {
        gl::SRGB8_ALPHA8 => TextureFormat::Rgba8UnormSrgb,
        gl::RGBA8 => TextureFormat::Rgba8Unorm,
        gl::RGBA16F => TextureFormat::Rgba16Float,
        _ => panic!("Unsupported GL format: {format}"),
    }
}

pub fn create_texture(device: &Device, resolution: UVec2, format: TextureFormat) -> Texture {
    device.create_texture(&TextureDescriptor {
        label: None,
        size: Extent3d {
            width: resolution.x,
            height: resolution.y,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::COPY_DST | TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

fn create_texture_from_gles(
    device: &Device,
    texture: u32,
    resolution: UVec2,
    format: TextureFormat,
) -> Texture {
    let size = Extent3d {
        width: resolution.x,
        height: resolution.y,
        depth_or_array_layers: 1,
    };

    unsafe {
        let hal_texture = device.as_hal::<api::Gles, _, _>(|device| {
            device.unwrap().texture_from_raw(
                NonZeroU32::new(texture).unwrap(),
                &hal::TextureDescriptor {
                    label: None,
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: TextureDimension::D2,
                    format,
                    usage: TextureUses::COLOR_TARGET,
                    memory_flags: MemoryFlags::empty(),
                    view_formats: vec![],
                },
                Some(Box::new(|| ())),
            )
        });

        device.create_texture_from_hal::<api::Gles>(
            hal_texture,
            &TextureDescriptor {
                label: None,
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format,
                usage: TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            },
        )
    }
}

// This is used to convert OpenXR swapchains to wgpu
pub fn create_gl_swapchain(
    device: &Device,
    gl_textures: &[u32],
    resolution: UVec2,
    format: TextureFormat,
) -> Vec<TextureView> {
    gl_textures
        .iter()
        .map(|gl_tex| {
            create_texture_from_gles(device, *gl_tex, resolution, format)
                .create_view(&Default::default())
        })
        .collect()
}

pub struct GraphicsContext {
    _instance: Instance,

    #[cfg_attr(windows, expect(dead_code))]
    adapter: Adapter,

    device: Device,
    queue: Queue,
    pub egl_display: egl::Display,
    pub egl_config: egl::Config,
    pub egl_context: egl::Context,
    pub gl_context: gl::Context,

    #[cfg_attr(windows, expect(dead_code))]
    dummy_surface: egl::Surface,

    create_image: CreateImageFn,
    destroy_image: DestroyImageFn,
    // Only present on Android
    #[cfg_attr(target_os = "linux", expect(dead_code))]
    get_native_client_buffer: Option<GetNativeClientBufferFn>,
    image_target_texture_2d: ImageTargetTexture2DFn,

    /// eglGetProcAddress of the EGL library used by this context, needed by XR_MNDX_egl_enable
    pub egl_get_proc_address: EglGetProcAddressFn,
}

impl GraphicsContext {
    #[cfg(not(windows))]
    pub fn new_gl() -> Self {
        use std::mem;
        use wgpu::{
            Backends, DeviceDescriptor, Features, InstanceDescriptor, InstanceFlags, Limits,
            MemoryHints,
        };

        const CREATE_IMAGE_FN_STR: &str = "eglCreateImageKHR";
        const DESTROY_IMAGE_FN_STR: &str = "eglDestroyImageKHR";
        const GET_NATIVE_CLIENT_BUFFER_FN_STR: &str = "eglGetNativeClientBufferANDROID";
        const IMAGE_TARGET_TEXTURE_2D_FN_STR: &str = "glEGLImageTargetTexture2DOES";

        let flags = if cfg!(debug_assertions) {
            InstanceFlags::DEBUG | InstanceFlags::VALIDATION
        } else {
            InstanceFlags::empty()
        };

        let instance = Instance::new(&InstanceDescriptor {
            backends: Backends::GL,
            flags,
            backend_options: Default::default(),
        });

        let adapter = instance.enumerate_adapters(Backends::GL).remove(0);
        let (device, queue) = pollster::block_on(adapter.request_device(
            &DeviceDescriptor {
                label: None,
                required_features: Features::PUSH_CONSTANTS,
                required_limits: Limits {
                    max_push_constant_size: MAX_PUSH_CONSTANTS_SIZE,
                    ..adapter.limits()
                },
                memory_hints: MemoryHints::Performance,
            },
            None,
        ))
        .unwrap();

        let raw_instance = unsafe { instance.as_hal::<api::Gles>() }.unwrap();

        let egl_display = raw_instance.raw_display();
        let egl_config = raw_instance.egl_config();

        let (
            egl_context,
            gl_context,
            dummy_surface,
            create_image,
            destroy_image,
            get_native_client_buffer,
            image_target_texture_2d,
        ) = unsafe {
            adapter.as_hal::<api::Gles, _, _>(|raw_adapter| {
                let adapter_context = raw_adapter.unwrap().adapter_context();
                let egl_instance = adapter_context.egl_instance().unwrap();

                let egl_context = egl::Context::from_ptr(adapter_context.raw_context());

                const PBUFFER_ATTRIBS: [i32; 5] = [egl::WIDTH, 16, egl::HEIGHT, 16, egl::NONE];
                let dummy_surface = egl_instance
                    .create_pbuffer_surface(egl_display, egl_config, &PBUFFER_ATTRIBS)
                    .unwrap();

                egl_instance
                    .make_current(
                        egl_display,
                        Some(dummy_surface),
                        Some(dummy_surface),
                        Some(egl_context),
                    )
                    .unwrap();

                let gl_context = gl::Context::from_loader_function(|fn_name| {
                    egl_instance
                        .get_proc_address(fn_name)
                        .map_or(ptr::null(), |f| f as *const c_void)
                });

                let get_fn_ptr = |fn_name| {
                    egl_instance
                        .get_proc_address(fn_name)
                        .map_or(ptr::null(), |f| f as *const c_void)
                };

                let create_image: CreateImageFn = mem::transmute(get_fn_ptr(CREATE_IMAGE_FN_STR));
                let destroy_image: DestroyImageFn =
                    mem::transmute(get_fn_ptr(DESTROY_IMAGE_FN_STR));
                let get_native_client_buffer: Option<GetNativeClientBufferFn> =
                    mem::transmute(get_fn_ptr(GET_NATIVE_CLIENT_BUFFER_FN_STR));
                let image_target_texture_2d: ImageTargetTexture2DFn =
                    mem::transmute(get_fn_ptr(IMAGE_TARGET_TEXTURE_2D_FN_STR));

                (
                    egl_context,
                    gl_context,
                    dummy_surface,
                    create_image,
                    destroy_image,
                    get_native_client_buffer,
                    image_target_texture_2d,
                )
            })
        };

        // wgpu opens the same library, so this resolves to the same EGL implementation
        let egl_get_proc_address = unsafe {
            let lib = libloading::Library::new(EGL_LIBRARY_NAME).unwrap();
            let func = *lib
                .get::<EglGetProcAddressFn>(b"eglGetProcAddress\0")
                .unwrap();
            mem::forget(lib);

            func
        };

        Self {
            _instance: instance,
            adapter,
            device,
            queue,
            egl_display,
            egl_config,
            egl_context,
            gl_context,
            dummy_surface,
            create_image,
            destroy_image,
            get_native_client_buffer,
            image_target_texture_2d,
            egl_get_proc_address,
        }
    }

    #[cfg(windows)]
    pub fn new_gl() -> Self {
        unimplemented!()
    }

    pub fn make_current(&self) {
        #[cfg(not(windows))]
        unsafe {
            self.adapter.as_hal::<api::Gles, _, _>(|raw_adapter| {
                let egl_instance = raw_adapter
                    .unwrap()
                    .adapter_context()
                    .egl_instance()
                    .unwrap();

                egl_instance
                    .make_current(
                        self.egl_display,
                        Some(self.dummy_surface),
                        Some(self.dummy_surface),
                        Some(self.egl_context),
                    )
                    .unwrap();
            })
        };
    }

    #[cfg(not(target_os = "linux"))]
    unsafe fn create_image_from_buffer(&self, buffer: *const c_void) -> egl::EGLImage {
        const EGL_NATIVE_BUFFER_ANDROID: u32 = 0x3140;

        let client_buffer = (self.get_native_client_buffer.unwrap())(buffer);
        check_error(&self.gl_context, "get_native_client_buffer");

        (self.create_image)(
            self.egl_display.as_ptr(),
            egl::NO_CONTEXT,
            EGL_NATIVE_BUFFER_ANDROID,
            client_buffer,
            ptr::null(),
        )
    }

    /// Imports one plane of a dma-buf frame as a single-channel or two-channel image (R8/GR88 or
    /// R16/GR1616). wgpu creates a desktop GL context on Linux, which cannot sample multi-planar
    /// external images, so the planes are sampled as plain 2D textures and converted in the
    /// shader.
    #[cfg(target_os = "linux")]
    unsafe fn create_plane_image(&self, frame: &DmabufFrame, plane_idx: usize) -> egl::EGLImage {
        const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
        const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
        const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
        const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
        const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
        const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
        const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;
        const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

        let plane = &frame.planes[plane_idx];
        // Chroma is subsampled 2x2 in both NV12 and P010
        let (width, height) = if plane_idx == 0 {
            (frame.width, frame.height)
        } else {
            (frame.width.div_ceil(2), frame.height.div_ceil(2))
        };

        let mut attribs = vec![
            egl::WIDTH,
            width as i32,
            egl::HEIGHT,
            height as i32,
            EGL_LINUX_DRM_FOURCC_EXT,
            frame.plane_formats[plane_idx] as i32,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            plane.fd,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            plane.offset as i32,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            plane.pitch as i32,
        ];
        if plane.modifier != DRM_FORMAT_MOD_INVALID {
            attribs.extend([
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                plane.modifier as u32 as i32,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (plane.modifier >> 32) as u32 as i32,
            ]);
        }
        attribs.push(egl::NONE);

        (self.create_image)(
            self.egl_display.as_ptr(),
            egl::NO_CONTEXT,
            EGL_LINUX_DMA_BUF_EXT,
            ptr::null_mut(),
            attribs.as_ptr(),
        )
    }

    /// Binds the luma and chroma planes of `frame` to `textures` (as GL_TEXTURE_2D on texture
    /// units 0 and 1) for the duration of `render_cb`.
    ///
    /// # Safety
    /// `frame` must describe valid dma-bufs and `textures` must be valid GL textures.
    #[cfg(target_os = "linux")]
    pub unsafe fn render_dmabuf_using_textures(
        &self,
        frame: &DmabufFrame,
        textures: [gl::Texture; 2],
        render_cb: impl FnOnce(),
    ) {
        if frame.plane_count < 2 {
            alvr_common::error!("Expected 2 planes in decoded frame, got {}", frame.plane_count);
            return;
        }

        // Errors left by other GL users of this context (wgpu, the OpenXR runtime) are not ours
        while self.gl_context.get_error() != gl::NO_ERROR {}

        let mut images = vec![];
        for (idx, texture) in textures.iter().enumerate() {
            let image = self.create_plane_image(frame, idx);
            if image.is_null() {
                alvr_common::error!(
                    "eglCreateImageKHR failed for plane {idx} (format {:#x}, modifier {:#x})",
                    frame.plane_formats[idx],
                    frame.planes[idx].modifier
                );
                for image in images {
                    (self.destroy_image)(self.egl_display.as_ptr(), image);
                }
                return;
            }

            self.gl_context.active_texture(gl::TEXTURE0 + idx as u32);
            self.gl_context.bind_texture(gl::TEXTURE_2D, Some(*texture));
            (self.image_target_texture_2d)(gl::TEXTURE_2D, image);
            check_error(&self.gl_context, "image_target_texture_2d");

            images.push(image);
        }

        render_cb();

        for image in images {
            (self.destroy_image)(self.egl_display.as_ptr(), image);
        }
        self.gl_context.active_texture(gl::TEXTURE0);
        check_error(&self.gl_context, "destroy_image");
    }

    /// # Safety
    /// `buffer` must be a valid AHardwareBuffer.
    /// `texture` must be a valid GL texture.
    #[cfg(not(target_os = "linux"))]
    pub unsafe fn render_ahardwarebuffer_using_texture(
        &self,
        buffer: *const c_void,
        texture: gl::Texture,
        render_cb: impl FnOnce(),
    ) {
        if !buffer.is_null() {
            let image = self.create_image_from_buffer(buffer);
            check_error(&self.gl_context, "create_image");
            if image.is_null() {
                alvr_common::error!("eglCreateImageKHR failed for decoded frame");
                return;
            }

            self.gl_context
                .bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(texture));
            check_error(&self.gl_context, "bind texture OES");

            (self.image_target_texture_2d)(GL_TEXTURE_EXTERNAL_OES, image);
            check_error(&self.gl_context, "image_target_texture_2d");

            render_cb();

            (self.destroy_image)(self.egl_display.as_ptr(), image);
            check_error(&self.gl_context, "destroy_image");
        }
    }
}

#[cfg(not(windows))]
impl Default for GraphicsContext {
    fn default() -> Self {
        Self::new_gl()
    }
}
