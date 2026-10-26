// Decoded video frames shared as Linux dma-bufs, for example VAAPI surfaces exported by FFmpeg as
// DRM PRIME. This is what the Linux decoder hands to the renderer in place of an AHardwareBuffer.
//
// wgpu creates a desktop GL context on Linux, which cannot sample multi-planar external images.
// Instead the luma and the chroma of the frame are imported as two images, each one bound as a one
// or two channel GL_TEXTURE_2D, and the YUV to RGB conversion is done in the staging shader, using
// the colour space and range of the stream.

use crate::GraphicsContext;
use alvr_common::anyhow::{Result, anyhow, bail};
use glow::{self as gl, HasContext};
use khronos_egl as egl;
use std::ptr;

const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
// FD, offset, pitch, modifier low and high bits of each plane
const EGL_DMA_BUF_PLANE_ATTRIBUTES: [[i32; 5]; MAX_DMABUF_PLANES] = [
    [0x3272, 0x3273, 0x3274, 0x3443, 0x3444],
    [0x3275, 0x3276, 0x3277, 0x3445, 0x3446],
    [0x3278, 0x3279, 0x327A, 0x3447, 0x3448],
    [0x3440, 0x3441, 0x3442, 0x3449, 0x344A],
];

const DMA_BUF_IMPORT_EXTENSION: &str = "EGL_EXT_image_dma_buf_import";
const DMA_BUF_IMPORT_MODIFIERS_EXTENSION: &str = "EGL_EXT_image_dma_buf_import_modifiers";
const EGL_IMAGE_TEXTURE_EXTENSION: &str = "GL_OES_EGL_image";

const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

// glGetError() reports one error flag per call. Implementations have only a few flags, this bound
// just protects against one that never clears them.
const MAX_STALE_GL_ERRORS: usize = 16;

pub const DRM_FORMAT_R8: u32 = fourcc(b"R8  ");
pub const DRM_FORMAT_R16: u32 = fourcc(b"R16 ");
pub const DRM_FORMAT_GR88: u32 = fourcc(b"GR88");
pub const DRM_FORMAT_GR1616: u32 = fourcc(b"GR32");
pub const DRM_FORMAT_NV12: u32 = fourcc(b"NV12");
pub const DRM_FORMAT_P010: u32 = fourcc(b"P010");

/// Planes of one dma-buf image: the image data, plus the auxiliary planes required by some
/// modifiers (for example the compression metadata of Intel's CCS modifiers)
pub const MAX_DMABUF_PLANES: usize = 4;

// EGL and GL extensions needed to import dma-bufs, queried once when the context is created
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DmabufImportSupport {
    import: bool,
    modifiers: bool,
    image_texture: bool,
}

impl DmabufImportSupport {
    pub fn new(egl_extensions: &str, gl_context: &gl::Context) -> Self {
        let has_egl_extension = |name| {
            egl_extensions
                .split_ascii_whitespace()
                .any(|extension| extension == name)
        };

        Self {
            import: has_egl_extension(DMA_BUF_IMPORT_EXTENSION),
            modifiers: has_egl_extension(DMA_BUF_IMPORT_MODIFIERS_EXTENSION),
            image_texture: gl_context
                .supported_extensions()
                .contains(EGL_IMAGE_TEXTURE_EXTENSION),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YuvFormat {
    /// 8 bit luma plane followed by an interleaved 8 bit chroma plane, subsampled 2x2
    Nv12,
    /// Like NV12, but with 10 bit samples stored in the high bits of 16 bit words
    P010,
}

impl YuvFormat {
    pub fn from_drm_fourcc(fourcc: u32) -> Option<Self> {
        match fourcc {
            DRM_FORMAT_NV12 => Some(Self::Nv12),
            DRM_FORMAT_P010 => Some(Self::P010),
            _ => None,
        }
    }

    pub fn bit_depth(self) -> u32 {
        match self {
            Self::Nv12 => 8,
            Self::P010 => 10,
        }
    }

    /// Formats used to view the luma and the chroma planes as separate images
    pub fn image_drm_fourccs(self) -> [u32; 2] {
        match self {
            Self::Nv12 => [DRM_FORMAT_R8, DRM_FORMAT_GR88],
            Self::P010 => [DRM_FORMAT_R16, DRM_FORMAT_GR1616],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum YuvColorSpace {
    Bt601,
    #[default]
    Bt709,
    Bt2020,
}

/// Position of the chroma samples relative to the luma samples
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChromaLocation {
    /// Between the luma samples, horizontally and vertically
    #[default]
    Center,
    /// Aligned with the left luma column and vertically centered. Default of H.264 and HEVC.
    Left,
    TopLeft,
    Top,
    BottomLeft,
    Bottom,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DmabufPlane {
    pub fd: i32,
    pub offset: u32,
    pub pitch: u32,
    pub modifier: u64,
}

/// The luma or the chroma of a decoded frame, imported as one EGLImage
#[derive(Clone, Copy, Debug, Default)]
pub struct DmabufImage {
    pub drm_fourcc: u32,
    pub plane_count: usize,
    pub planes: [DmabufPlane; MAX_DMABUF_PLANES],
}

/// A decoded frame with its luma and chroma in dma-bufs. The file descriptors are owned by the
/// decoder and must stay open while the frame is rendered.
#[derive(Clone, Copy, Debug)]
pub struct DmabufFrame {
    pub width: u32,
    pub height: u32,
    pub format: YuvFormat,
    /// Luma and chroma images
    pub images: [DmabufImage; 2],
    pub color_space: YuvColorSpace,
    pub full_range: bool,
    pub chroma_location: ChromaLocation,
}

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

/// Returns the matrix (column major, like GLSL) and the offset that turn the sampled YUV values
/// into RGB: `rgb = matrix * (yuv - offset)`.
///
/// The samples are read as normalized integers: 8 bit formats are divided by 255, 10 bit formats
/// (stored in the high bits of 16 bit words) are divided by 65535 after being shifted left by 6.
pub fn yuv_to_rgb_conversion(
    color_space: YuvColorSpace,
    full_range: bool,
    bit_depth: u32,
) -> ([f32; 9], [f32; 3]) {
    let (kr, kb) = match color_space {
        YuvColorSpace::Bt601 => (0.299, 0.114),
        YuvColorSpace::Bt709 => (0.2126, 0.0722),
        YuvColorSpace::Bt2020 => (0.2627, 0.0593),
    };
    let kg = 1.0 - kr - kb;

    // Value of one integer step once normalized
    let container_bits = if bit_depth > 8 { 16 } else { 8 };
    let step = (1 << (container_bits - bit_depth)) as f32 / ((1 << container_bits) - 1) as f32;

    // Limited range codes are defined for 8 bits and scaled for higher bit depths
    let depth_scale = (1 << (bit_depth - 8)) as f32;
    let (luma_offset, luma_range, chroma_range) = if full_range {
        let max_code = ((1 << bit_depth) - 1) as f32;
        (0.0, max_code, max_code)
    } else {
        (16.0 * depth_scale, 219.0 * depth_scale, 224.0 * depth_scale)
    };
    let chroma_offset = (1 << (bit_depth - 1)) as f32;

    let luma_scale = 1.0 / (luma_range * step);
    let chroma_scale = 1.0 / (chroma_range * step);

    // Chroma is in [-0.5, 0.5] after scaling
    let red_from_cr = 2.0 * (1.0 - kr);
    let blue_from_cb = 2.0 * (1.0 - kb);
    let green_from_cb = 2.0 * kb * (1.0 - kb) / kg;
    let green_from_cr = 2.0 * kr * (1.0 - kr) / kg;

    let matrix = [
        // Y column
        luma_scale,
        luma_scale,
        luma_scale,
        // Cb column
        0.0,
        -green_from_cb * chroma_scale,
        blue_from_cb * chroma_scale,
        // Cr column
        red_from_cr * chroma_scale,
        -green_from_cr * chroma_scale,
        0.0,
    ];
    let offset = [
        luma_offset * step,
        chroma_offset * step,
        chroma_offset * step,
    ];

    (matrix, offset)
}

/// Returns what to add to the texture coordinates of a luma sample (in the whole frame) to sample
/// the chroma texture at the same position. Sampling the subsampled chroma texture with the luma
/// coordinates is only right for centered chroma samples. Chroma samples aligned with the left
/// (or top) luma samples are half a luma sample further left (or up) than that, so the
/// coordinates are moved right (or down) by half a luma sample.
pub fn chroma_sample_offset(location: ChromaLocation, width: u32, height: u32) -> [f32; 2] {
    let half_column = 0.5 / width as f32;
    let half_row = 0.5 / height as f32;

    match location {
        ChromaLocation::Center => [0.0, 0.0],
        ChromaLocation::Left => [half_column, 0.0],
        ChromaLocation::TopLeft => [half_column, half_row],
        ChromaLocation::Top => [0.0, half_row],
        ChromaLocation::BottomLeft => [half_column, -half_row],
        ChromaLocation::Bottom => [0.0, -half_row],
    }
}

impl GraphicsContext {
    /// Fails if EGL or GL cannot import the decoder's dma-bufs as textures
    pub fn check_dmabuf_import_support(&self) -> Result<()> {
        if !self.dmabuf_import_support.import {
            bail!("EGL does not support {DMA_BUF_IMPORT_EXTENSION}");
        }
        if !self.dmabuf_import_support.image_texture {
            bail!("The GL context does not support {EGL_IMAGE_TEXTURE_EXTENSION}");
        }

        Ok(())
    }

    unsafe fn create_dmabuf_image(
        &self,
        frame: &DmabufFrame,
        image_index: usize,
    ) -> Result<egl::EGLImage> {
        let image = &frame.images[image_index];
        let Some(planes) = image.planes.get(..image.plane_count) else {
            bail!("Invalid dma-buf plane count {}", image.plane_count);
        };

        // Chroma is subsampled 2x2 in all supported formats
        let (width, height) = if image_index == 0 {
            (frame.width, frame.height)
        } else {
            (frame.width.div_ceil(2), frame.height.div_ceil(2))
        };

        let mut attributes = vec![
            egl::WIDTH,
            width as i32,
            egl::HEIGHT,
            height as i32,
            EGL_LINUX_DRM_FOURCC_EXT,
            image.drm_fourcc as i32,
        ];
        for (plane, [fd, offset, pitch, modifier_low, modifier_high]) in
            planes.iter().zip(EGL_DMA_BUF_PLANE_ATTRIBUTES)
        {
            attributes.extend([
                fd,
                plane.fd,
                offset,
                plane.offset as i32,
                pitch,
                plane.pitch as i32,
            ]);

            // Without the modifiers extension the driver assumes its default layout, which is
            // only right for linear (or implicit) layouts
            if self.dmabuf_import_support.modifiers {
                if plane.modifier != DRM_FORMAT_MOD_INVALID {
                    attributes.extend([
                        modifier_low,
                        plane.modifier as u32 as i32,
                        modifier_high,
                        (plane.modifier >> 32) as u32 as i32,
                    ]);
                }
            } else if !matches!(
                plane.modifier,
                DRM_FORMAT_MOD_INVALID | DRM_FORMAT_MOD_LINEAR
            ) {
                bail!(
                    "EGL does not support {DMA_BUF_IMPORT_MODIFIERS_EXTENSION}, needed for \
                    modifier {:#x}",
                    plane.modifier
                );
            }
        }
        attributes.push(egl::NONE);

        let egl_image = unsafe {
            (self.create_image)(
                self.egl_display.as_ptr(),
                egl::NO_CONTEXT,
                EGL_LINUX_DMA_BUF_EXT,
                ptr::null_mut(),
                attributes.as_ptr(),
            )
        };
        if egl_image.is_null() {
            bail!(
                "Cannot import the {} of the decoded frame ({:?}, {} planes, modifier {:#x})",
                if image_index == 0 { "luma" } else { "chroma" },
                frame.format,
                image.plane_count,
                image.planes[0].modifier,
            );
        }

        Ok(egl_image)
    }

    /// Binds the luma and the chroma of `frame` to `textures` (as GL_TEXTURE_2D on texture units 0
    /// and 1) for the duration of `render_cb`. If the frame cannot be imported, `render_cb` is not
    /// called and an error is returned.
    ///
    /// # Safety
    /// `frame` must point to a valid DmabufFrame whose file descriptors are open.
    /// `textures` must be valid GL textures.
    pub unsafe fn render_dmabuf_using_textures(
        &self,
        frame: *const DmabufFrame,
        textures: [gl::Texture; 2],
        render_cb: impl FnOnce(&DmabufFrame),
    ) -> Result<()> {
        let Some(frame) = (unsafe { frame.as_ref() }) else {
            return Ok(());
        };

        // Errors left by other users of this context (wgpu, the OpenXR runtime) are not ours
        for _ in 0..MAX_STALE_GL_ERRORS {
            if unsafe { self.gl_context.get_error() } == gl::NO_ERROR {
                break;
            }
        }

        let mut egl_images = Vec::with_capacity(textures.len());
        let mut result = Ok(());
        for (image_index, texture) in textures.into_iter().enumerate() {
            let egl_image = match unsafe { self.create_dmabuf_image(frame, image_index) } {
                Ok(egl_image) => egl_image,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            egl_images.push(egl_image);

            let gl_error = unsafe {
                self.gl_context
                    .active_texture(gl::TEXTURE0 + image_index as u32);
                self.gl_context.bind_texture(gl::TEXTURE_2D, Some(texture));
                (self.image_target_texture_2d)(gl::TEXTURE_2D, egl_image);

                self.gl_context.get_error()
            };
            if gl_error != gl::NO_ERROR {
                result = Err(anyhow!(
                    "glEGLImageTargetTexture2DOES failed with GL error {gl_error:#x}"
                ));
                break;
            }
        }

        if result.is_ok() {
            render_cb(frame);
        }

        for egl_image in egl_images {
            unsafe { (self.destroy_image)(self.egl_display.as_ptr(), egl_image) };
        }
        unsafe { self.gl_context.active_texture(gl::TEXTURE0) };

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLOR_SPACES: [(YuvColorSpace, f32, f32); 3] = [
        (YuvColorSpace::Bt601, 0.299, 0.114),
        (YuvColorSpace::Bt709, 0.2126, 0.0722),
        (YuvColorSpace::Bt2020, 0.2627, 0.0593),
    ];

    // Applies the conversion like the staging shader does, to integer codes
    fn convert(
        color_space: YuvColorSpace,
        full_range: bool,
        bit_depth: u32,
        codes: [f32; 3],
    ) -> [f32; 3] {
        let (matrix, offset) = yuv_to_rgb_conversion(color_space, full_range, bit_depth);

        let container_bits = if bit_depth > 8 { 16 } else { 8 };
        let sampled = codes.map(|code| {
            code * (1 << (container_bits - bit_depth)) as f32 / ((1 << container_bits) - 1) as f32
        });
        let yuv = [0, 1, 2].map(|i| sampled[i] - offset[i]);

        [0, 1, 2]
            .map(|row| matrix[row] * yuv[0] + matrix[3 + row] * yuv[1] + matrix[6 + row] * yuv[2])
    }

    // Reference RGB to YUV encoding, giving unrounded integer codes
    fn encode(kr: f32, kb: f32, full_range: bool, bit_depth: u32, rgb: [f32; 3]) -> [f32; 3] {
        let luma = kr * rgb[0] + (1.0 - kr - kb) * rgb[1] + kb * rgb[2];
        let cb = (rgb[2] - luma) / (2.0 * (1.0 - kb));
        let cr = (rgb[0] - luma) / (2.0 * (1.0 - kr));

        let depth_scale = (1 << (bit_depth - 8)) as f32;
        let middle = (1 << (bit_depth - 1)) as f32;
        if full_range {
            let max_code = ((1 << bit_depth) - 1) as f32;
            [
                luma * max_code,
                middle + cb * max_code,
                middle + cr * max_code,
            ]
        } else {
            [
                (16.0 + 219.0 * luma) * depth_scale,
                middle + 224.0 * depth_scale * cb,
                middle + 224.0 * depth_scale * cr,
            ]
        }
    }

    fn assert_close(actual: [f32; 3], expected: [f32; 3]) {
        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() < 0.005, "{actual:?} != {expected:?}");
        }
    }

    #[test]
    fn drm_fourccs() {
        assert_eq!(DRM_FORMAT_NV12, 0x3231_564e);
        assert_eq!(DRM_FORMAT_P010, 0x3031_3050);
        assert_eq!(
            YuvFormat::from_drm_fourcc(DRM_FORMAT_P010),
            Some(YuvFormat::P010)
        );
        assert_eq!(YuvFormat::from_drm_fourcc(DRM_FORMAT_R8), None);
    }

    // Checks the matrix against the usual published coefficients for 8 bit limited range codes
    // (applied to Y - 16, Cb - 128 and Cr - 128), independently from the formulas above
    #[test]
    fn published_limited_range_coefficients() {
        let assert_coefficient = |actual: f32, expected: f32| {
            assert!((actual - expected).abs() < 1e-5, "{actual} != {expected}");
        };
        // Normalizing the codes and the result by 255 doesn't change the coefficients
        let code_matrix = |color_space| yuv_to_rgb_conversion(color_space, false, 8).0;

        // BT.709: R = 1.164383 Y + 1.792741 Cr, G = 1.164383 Y - 0.213249 Cb - 0.532909 Cr,
        // B = 1.164383 Y + 2.112402 Cb
        let bt709 = code_matrix(YuvColorSpace::Bt709);
        for (actual, expected) in bt709.into_iter().zip([
            1.164383, 1.164383, 1.164383, 0.0, -0.213249, 2.112402, 1.792741, -0.532909, 0.0,
        ]) {
            assert_coefficient(actual, expected);
        }

        // BT.601: R = 1.164383 Y + 1.596027 Cr, G = 1.164383 Y - 0.391762 Cb - 0.812968 Cr,
        // B = 1.164383 Y + 2.017232 Cb
        let bt601 = code_matrix(YuvColorSpace::Bt601);
        for (actual, expected) in bt601.into_iter().zip([
            1.164383, 1.164383, 1.164383, 0.0, -0.391762, 2.017232, 1.596027, -0.812968, 0.0,
        ]) {
            assert_coefficient(actual, expected);
        }

        // BT.2020 (non-constant luminance): R = 1.164383 Y + 1.678674 Cr,
        // G = 1.164383 Y - 0.187326 Cb - 0.650424 Cr, B = 1.164383 Y + 2.141772 Cb
        let bt2020 = code_matrix(YuvColorSpace::Bt2020);
        for (actual, expected) in bt2020.into_iter().zip([
            1.164383, 1.164383, 1.164383, 0.0, -0.187326, 2.141772, 1.678674, -0.650424, 0.0,
        ]) {
            assert_coefficient(actual, expected);
        }

        let (_, offset) = yuv_to_rgb_conversion(YuvColorSpace::Bt709, false, 8);
        for (actual, expected) in offset.into_iter().zip([16.0, 128.0, 128.0]) {
            assert_coefficient(actual, expected / 255.0);
        }
    }

    #[test]
    fn limited_range_bt709() {
        let convert = |codes| convert(YuvColorSpace::Bt709, false, 8, codes);

        assert_close(convert([235.0, 128.0, 128.0]), [1.0; 3]);
        assert_close(convert([16.0, 128.0, 128.0]), [0.0; 3]);
        // Rounded codes of the primaries
        assert_close(convert([63.0, 102.0, 240.0]), [1.0, 0.0, 0.0]);
        assert_close(convert([173.0, 42.0, 26.0]), [0.0, 1.0, 0.0]);
        assert_close(convert([32.0, 240.0, 118.0]), [0.0, 0.0, 1.0]);
    }

    #[test]
    fn full_range_bt709() {
        let convert = |codes| convert(YuvColorSpace::Bt709, true, 8, codes);

        assert_close(convert([255.0, 128.0, 128.0]), [1.0; 3]);
        assert_close(convert([0.0, 128.0, 128.0]), [0.0; 3]);
        // Rounded codes of the primaries. Full range chroma is clipped at 255 instead of 255.5,
        // so the result is less precise
        for (actual, expected) in [
            (convert([54.0, 99.0, 255.0]), [1.0, 0.0, 0.0]),
            (convert([18.0, 255.0, 116.0]), [0.0, 0.0, 1.0]),
        ] {
            for (a, e) in actual.iter().zip(expected) {
                assert!((a - e).abs() < 0.01, "{actual:?} != {expected:?}");
            }
        }
    }

    #[test]
    fn every_color_space_range_and_depth() {
        let colors = [
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [1.0, 1.0, 1.0],
            [0.0, 0.0, 0.0],
            [0.25, 0.5, 0.75],
        ];

        for (color_space, kr, kb) in COLOR_SPACES {
            for full_range in [false, true] {
                for bit_depth in [8, 10] {
                    for rgb in colors {
                        let codes = encode(kr, kb, full_range, bit_depth, rgb);

                        assert_close(convert(color_space, full_range, bit_depth, codes), rgb);
                    }
                }
            }
        }
    }

    #[test]
    fn wrong_color_space_is_visible() {
        // Decoding BT.709 red as BT.601 must not give back pure red
        let codes = encode(0.2126, 0.0722, true, 8, [1.0, 0.0, 0.0]);
        let rgb = convert(YuvColorSpace::Bt601, true, 8, codes);

        assert!((rgb[1] - 0.0).abs() > 0.05, "{rgb:?}");
    }

    #[test]
    fn chroma_sample_offsets() {
        assert_eq!(
            chroma_sample_offset(ChromaLocation::Center, 1000, 500),
            [0.0, 0.0]
        );

        // Left sited chroma column 0 is at the center of luma column 0 (u = 0.5 / 1000), where the
        // center of chroma column 0 must be sampled (u = 0.5 / 500)
        let [u, v] = chroma_sample_offset(ChromaLocation::Left, 1000, 500);
        assert_eq!((0.5 / 1000.0 + u, v), (0.5 / 500.0, 0.0));

        assert_eq!(
            chroma_sample_offset(ChromaLocation::TopLeft, 1000, 500),
            [0.0005, 0.001]
        );
        assert_eq!(
            chroma_sample_offset(ChromaLocation::Bottom, 1000, 500),
            [0.0, -0.001]
        );
    }
}
