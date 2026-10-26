#[cfg(target_os = "linux")]
use alvr_common::anyhow::{Result, bail};
use alvr_common::glam::UVec2;
use alvr_graphics::GraphicsContext;
use alvr_session::ClientsidePostProcessingConfig;
use openxr as xr;
use std::ptr;

#[allow(unused)]
pub fn session_create_info(ctx: &GraphicsContext) -> xr::opengles::SessionCreateInfo {
    #[cfg(target_os = "android")]
    {
        xr::opengles::SessionCreateInfo::Android {
            display: ctx.egl_display.as_ptr(),
            config: ctx.egl_config.as_ptr(),
            context: ctx.egl_context.as_ptr(),
        }
    }
    #[cfg(not(target_os = "android"))]
    unimplemented!()
}

/// Creates a session on desktop runtimes (Monado) by handing them our EGL context through
/// XR_MNDX_egl_enable. The openxr crate only exposes the Android binding of
/// XR_KHR_opengl_es_enable, so the session is created through the raw API.
///
/// # Safety
/// - `instance` must have been created with XR_MNDX_egl_enable enabled, and
///   xrGetOpenGLESGraphicsRequirementsKHR must have been called for `system`, as the spec requires
///   before xrCreateSession.
/// - The session is wrapped as a `Session<OpenGlEs>`, which is only correct because MNDX sessions
///   use the OpenGL ES swapchain image type.
/// - `ctx` must outlive the session.
#[cfg(target_os = "linux")]
pub unsafe fn create_egl_session(
    instance: &xr::Instance,
    system: xr::SystemId,
    ctx: &GraphicsContext,
) -> Result<(
    xr::Session<xr::OpenGlEs>,
    xr::FrameWaiter,
    xr::FrameStream<xr::OpenGlEs>,
)> {
    let Some(egl_get_proc_address) = ctx.egl_get_proc_address else {
        bail!("Cannot load eglGetProcAddress, needed by XR_MNDX_egl_enable");
    };

    let binding = xr::sys::GraphicsBindingEGLMNDX {
        ty: xr::sys::GraphicsBindingEGLMNDX::TYPE,
        next: ptr::null(),
        get_proc_address: Some(egl_get_proc_address),
        display: ctx.egl_display.as_ptr(),
        config: ctx.egl_config.as_ptr(),
        context: ctx.egl_context.as_ptr(),
    };
    let info = xr::sys::SessionCreateInfo {
        ty: xr::sys::SessionCreateInfo::TYPE,
        next: ptr::from_ref(&binding).cast(),
        create_flags: xr::SessionCreateFlags::EMPTY,
        system_id: system,
    };

    ctx.make_current();

    let mut handle = <xr::sys::Session as xr::sys::Handle>::NULL;
    let result = unsafe { (instance.fp().create_session)(instance.as_raw(), &info, &mut handle) };
    if result.into_raw() < 0 {
        bail!("xrCreateSession with XR_MNDX_egl_enable failed: {result}");
    }

    Ok(unsafe { xr::Session::from_raw(instance.clone(), handle, Box::new(())) })
}

pub fn swapchain_format(
    gfx_ctx: &GraphicsContext,
    session: &xr::Session<xr::OpenGlEs>,
    enable_hdr: bool,
) -> u32 {
    gfx_ctx.make_current();

    let formats = session.enumerate_swapchain_formats().unwrap();
    alvr_graphics::choose_swapchain_format(&formats, enable_hdr)
}

#[allow(unused_variables)]
pub fn create_swapchain(
    session: &xr::Session<xr::OpenGlEs>,
    gfx_ctx: &GraphicsContext,
    resolution: UVec2,
    format: u32,
    foveation: Option<&xr::FoveationProfileFB>,
) -> xr::Swapchain<xr::OpenGlEs> {
    gfx_ctx.make_current();

    let swapchain_info = xr::SwapchainCreateInfo {
        create_flags: xr::SwapchainCreateFlags::EMPTY,
        usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT | xr::SwapchainUsageFlags::SAMPLED,
        format,
        sample_count: 1,
        width: resolution.x,
        height: resolution.y,
        face_count: 1,
        array_size: 1,
        mip_count: 1,
    };

    session.create_swapchain(&swapchain_info).unwrap()
}

pub struct ProjectionLayerAlphaConfig {
    pub premultiplied: bool,
}

// This is needed to work around lifetime limitations. Deref cannot be implemented because there are
// nested references, and in a way or the other I would get `cannot return reference to temporary
// value`
pub struct ProjectionLayerBuilder<'a> {
    reference_space: &'a xr::Space,
    layers: [xr::CompositionLayerProjectionView<'a, xr::OpenGlEs>; 2],
    alpha: Option<ProjectionLayerAlphaConfig>,
    composition_layer_settings: Option<xr::sys::CompositionLayerSettingsFB>,
}

impl<'a> ProjectionLayerBuilder<'a> {
    pub fn new(
        reference_space: &'a xr::Space,
        layers: [xr::CompositionLayerProjectionView<'a, xr::OpenGlEs>; 2],
        alpha: Option<ProjectionLayerAlphaConfig>,
        clientside_post_processing_config: Option<ClientsidePostProcessingConfig>,
    ) -> Self {
        let composition_layer_settings = clientside_post_processing_config
            .map(|post_processing| {
                xr::CompositionLayerSettingsFlagsFB::from_raw(
                    (post_processing.sharpening as u64) | (post_processing.super_sampling as u64),
                )
            })
            .filter(|&flags| flags != xr::CompositionLayerSettingsFlagsFB::EMPTY)
            .map(|flags| xr::sys::CompositionLayerSettingsFB {
                ty: xr::StructureType::COMPOSITION_LAYER_SETTINGS_FB,
                next: std::ptr::null(),
                layer_flags: flags,
            });
        Self {
            reference_space,
            layers,
            alpha,
            composition_layer_settings,
        }
    }

    pub fn build(&self) -> xr::CompositionLayerProjection<'_, xr::OpenGlEs> {
        let mut flags = xr::CompositionLayerFlags::EMPTY;

        if let Some(alpha) = &self.alpha {
            flags |= xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA;

            if !alpha.premultiplied {
                flags |= xr::CompositionLayerFlags::UNPREMULTIPLIED_ALPHA;
            }
        }

        let layer = xr::CompositionLayerProjection::new()
            .layer_flags(flags)
            .space(self.reference_space)
            .views(&self.layers);

        if let Some(composition_layer_settings) = self.composition_layer_settings.as_ref() {
            unsafe {
                xr::CompositionLayerProjection::from_raw(xr::sys::CompositionLayerProjection {
                    next: ptr::from_ref(composition_layer_settings).cast(),
                    ..layer.into_raw()
                })
            }
        } else {
            layer
        }
    }
}
