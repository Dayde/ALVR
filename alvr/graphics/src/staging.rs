use super::{GraphicsContext, ck};
#[cfg(not(target_os = "linux"))]
use crate::GL_TEXTURE_EXTERNAL_OES;
use alvr_common::glam::{IVec2, UVec2};
use glow::{self as gl, HasContext};
#[cfg(target_os = "linux")]
use std::{
    cell::Cell,
    time::{Duration, Instant},
};
use std::{ffi::c_void, rc::Rc};

#[cfg(not(target_os = "linux"))]
const FRAGMENT_SHADER_SOURCE: &str = include_str!("../resources/staging_fragment.glsl");
// Note: this shader does not need FIX_LIMITED_RANGE, it is told the real range of each frame
#[cfg(target_os = "linux")]
const FRAGMENT_SHADER_SOURCE: &str = include_str!("../resources/staging_fragment_linux.glsl");
// A frame that cannot be imported fails the same way on every frame
#[cfg(target_os = "linux")]
const IMPORT_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(5);

fn create_program(
    gl: &gl::Context,
    vertex_shader_source: &str,
    fragment_shader_source: &str,
) -> gl::Program {
    unsafe {
        let vertex_shader = ck!(gl.create_shader(gl::VERTEX_SHADER).unwrap());
        ck!(gl.shader_source(vertex_shader, vertex_shader_source));
        ck!(gl.compile_shader(vertex_shader));
        if !gl.get_shader_compile_status(vertex_shader) {
            panic!(
                "Failed to compile vertex shader: {}",
                gl.get_shader_info_log(vertex_shader)
            );
        }

        let fragment_shader = ck!(gl.create_shader(gl::FRAGMENT_SHADER).unwrap());
        ck!(gl.shader_source(fragment_shader, fragment_shader_source));
        ck!(gl.compile_shader(fragment_shader));
        if !gl.get_shader_compile_status(fragment_shader) {
            panic!(
                "Failed to compile fragment shader: {}",
                gl.get_shader_info_log(fragment_shader)
            );
        }

        let program = ck!(gl.create_program().unwrap());
        ck!(gl.attach_shader(program, vertex_shader));
        ck!(gl.attach_shader(program, fragment_shader));
        ck!(gl.link_program(program));
        if !gl.get_program_link_status(program) {
            panic!(
                "Failed to link program: {}",
                gl.get_program_info_log(program)
            );
        }

        ck!(gl.delete_shader(vertex_shader));
        ck!(gl.delete_shader(fragment_shader));

        program
    }
}

// Textures and uniforms used to sample the luma and chroma planes of the decoded frame
#[cfg(target_os = "linux")]
struct YuvInput {
    plane_textures: [gl::Texture; 2],
    yuv_matrix_uloc: gl::UniformLocation,
    yuv_offset_uloc: gl::UniformLocation,
    chroma_offset_uloc: gl::UniformLocation,
    last_import_error_time: Cell<Option<Instant>>,
}

#[cfg(target_os = "linux")]
impl YuvInput {
    fn new(gl: &gl::Context, program: gl::Program) -> Self {
        unsafe {
            // Storage comes from the decoder's dma-bufs on each frame
            let plane_textures = [(); 2].map(|_| {
                let texture = ck!(gl.create_texture().unwrap());
                ck!(gl.bind_texture(gl::TEXTURE_2D, Some(texture)));
                for (parameter, value) in [
                    (gl::TEXTURE_MIN_FILTER, gl::LINEAR),
                    (gl::TEXTURE_MAG_FILTER, gl::LINEAR),
                    (gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE),
                    (gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE),
                ] {
                    ck!(gl.tex_parameter_i32(gl::TEXTURE_2D, parameter, value as i32));
                }
                ck!(gl.bind_texture(gl::TEXTURE_2D, None));

                texture
            });

            ck!(gl.use_program(Some(program)));
            for (name, texture_unit) in [("luma_tex", 0), ("chroma_tex", 1)] {
                let location = ck!(gl.get_uniform_location(program, name)).unwrap();
                ck!(gl.uniform_1_i32(Some(&location), texture_unit));
            }
            ck!(gl.use_program(None));

            Self {
                plane_textures,
                yuv_matrix_uloc: ck!(gl.get_uniform_location(program, "yuv_matrix")).unwrap(),
                yuv_offset_uloc: ck!(gl.get_uniform_location(program, "yuv_offset")).unwrap(),
                chroma_offset_uloc: ck!(gl.get_uniform_location(program, "chroma_offset")).unwrap(),
                last_import_error_time: Cell::new(None),
            }
        }
    }
}

pub struct StagingRenderer {
    context: Rc<GraphicsContext>,
    program: gl::Program,
    view_idx_uloc: gl::UniformLocation,
    #[cfg(not(target_os = "linux"))]
    surface_texture: gl::Texture,
    #[cfg(target_os = "linux")]
    yuv_input: YuvInput,
    framebuffers: [gl::Framebuffer; 2],
    viewport_size: IVec2,
}

impl StagingRenderer {
    pub fn new(
        context: Rc<GraphicsContext>,
        staging_textures: [gl::Texture; 2],
        view_resolution: UVec2,
        fix_limited_range: bool,
    ) -> Self {
        let gl = &context.gl_context;
        context.make_current();

        // Add #defines into the shader after the first line
        let mut frag_lines: Vec<&str> = FRAGMENT_SHADER_SOURCE.lines().collect();
        if fix_limited_range {
            frag_lines.insert(1, "#line 0 1\n#define FIX_LIMITED_RANGE");
        }
        let frag_str = frag_lines.join("\n");

        let program = create_program(
            gl,
            include_str!("../resources/staging_vertex.glsl"),
            frag_str.as_str(),
        );

        unsafe {
            // This is an external surface and storage should not be initialized
            #[cfg(not(target_os = "linux"))]
            let surface_texture = ck!(gl.create_texture().unwrap());

            let mut framebuffers = vec![];
            for tex in staging_textures {
                let framebuffer = ck!(gl.create_framebuffer().unwrap());
                ck!(gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, Some(framebuffer)));
                ck!(gl.framebuffer_texture_2d(
                    gl::DRAW_FRAMEBUFFER,
                    gl::COLOR_ATTACHMENT0,
                    gl::TEXTURE_2D,
                    Some(tex),
                    0,
                ));

                framebuffers.push(framebuffer);
            }

            ck!(gl.bind_framebuffer(gl::FRAMEBUFFER, None));

            let view_idx_uloc = ck!(gl.get_uniform_location(program, "view_idx")).unwrap();

            Self {
                #[cfg(target_os = "linux")]
                yuv_input: YuvInput::new(gl, program),
                context,
                program,
                #[cfg(not(target_os = "linux"))]
                surface_texture,
                view_idx_uloc,
                framebuffers: framebuffers.try_into().unwrap(),
                viewport_size: view_resolution.as_ivec2(),
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[allow(unused_variables)]
    pub fn render(&self, hardware_buffer: *mut c_void) {
        let gl = &self.context.gl_context;
        self.context.make_current();

        unsafe {
            self.context.render_ahardwarebuffer_using_texture(
                hardware_buffer,
                self.surface_texture,
                || {
                    ck!(gl.use_program(Some(self.program)));

                    ck!(gl.viewport(0, 0, self.viewport_size.x, self.viewport_size.y));
                    ck!(gl.disable(gl::SCISSOR_TEST));
                    ck!(gl.disable(gl::STENCIL_TEST));

                    for (i, framebuffer) in self.framebuffers.iter().enumerate() {
                        ck!(gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, Some(*framebuffer)));

                        ck!(gl.active_texture(gl::TEXTURE0));
                        ck!(gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(self.surface_texture)));
                        ck!(gl.bind_sampler(0, None));
                        ck!(gl.uniform_1_i32(Some(&self.view_idx_uloc), i as i32));
                        ck!(gl.draw_arrays(gl::TRIANGLE_STRIP, 0, 4));
                    }
                },
            )
        };
    }

    /// `frame` must point to a valid DmabufFrame
    #[cfg(target_os = "linux")]
    pub fn render(&self, frame: *mut c_void) {
        self.context.make_current();

        let result = unsafe {
            self.context.render_dmabuf_using_textures(
                frame.cast(),
                self.yuv_input.plane_textures,
                |frame| self.draw_views(frame),
            )
        };

        // The previous content of the staging textures is shown instead
        if let Err(e) = result
            && self
                .yuv_input
                .last_import_error_time
                .get()
                .is_none_or(|time| time.elapsed() >= IMPORT_ERROR_LOG_INTERVAL)
        {
            alvr_common::error!("Cannot render the decoded frame: {e:#}");
            self.yuv_input
                .last_import_error_time
                .set(Some(Instant::now()));
        }
    }

    // Converts the planes bound to texture units 0 and 1 into the staging textures
    #[cfg(target_os = "linux")]
    fn draw_views(&self, frame: &crate::DmabufFrame) {
        let gl = &self.context.gl_context;

        let (matrix, offset) = crate::yuv_to_rgb_conversion(
            frame.color_space,
            frame.full_range,
            frame.format.bit_depth(),
        );
        let chroma_offset =
            crate::chroma_sample_offset(frame.chroma_location, frame.width, frame.height);

        unsafe {
            ck!(gl.use_program(Some(self.program)));
            ck!(gl.uniform_matrix_3_f32_slice(
                Some(&self.yuv_input.yuv_matrix_uloc),
                false,
                &matrix
            ));
            ck!(gl.uniform_3_f32_slice(Some(&self.yuv_input.yuv_offset_uloc), &offset));
            ck!(gl.uniform_2_f32_slice(Some(&self.yuv_input.chroma_offset_uloc), &chroma_offset));

            ck!(gl.viewport(0, 0, self.viewport_size.x, self.viewport_size.y));
            ck!(gl.disable(gl::SCISSOR_TEST));
            ck!(gl.disable(gl::STENCIL_TEST));
            ck!(gl.bind_sampler(0, None));
            ck!(gl.bind_sampler(1, None));

            for (i, framebuffer) in self.framebuffers.iter().enumerate() {
                ck!(gl.bind_framebuffer(gl::DRAW_FRAMEBUFFER, Some(*framebuffer)));
                ck!(gl.uniform_1_i32(Some(&self.view_idx_uloc), i as i32));
                ck!(gl.draw_arrays(gl::TRIANGLE_STRIP, 0, 4));
            }
        }
    }
}

impl Drop for StagingRenderer {
    fn drop(&mut self) {
        let gl = &self.context.gl_context;
        self.context.make_current();

        unsafe {
            ck!(gl.delete_program(self.program));
            #[cfg(not(target_os = "linux"))]
            ck!(gl.delete_texture(self.surface_texture));
            #[cfg(target_os = "linux")]
            for texture in self.yuv_input.plane_textures {
                ck!(gl.delete_texture(texture));
            }
            for framebuffer in &self.framebuffers {
                ck!(gl.delete_framebuffer(*framebuffer));
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::{
        ChromaLocation, DRM_FORMAT_GR88, DRM_FORMAT_R8, DmabufFrame, DmabufImage, DmabufPlane,
        YuvColorSpace, YuvFormat,
    };
    use std::{fs::File, io::Write, os::fd::AsRawFd, path::Path, ptr};

    const VIEW_SIZE: i32 = 4;
    const FRAME_WIDTH: i32 = 2 * VIEW_SIZE;
    // Limited range BT.709 red
    const RED_LUMA: u8 = 63;
    const RED_CHROMA: [u8; 2] = [102, 240];

    fn create_renderer() -> (Rc<GraphicsContext>, StagingRenderer) {
        let context = Rc::new(GraphicsContext::new_gl());
        context.make_current();
        let gl = &context.gl_context;

        let targets = [(); 2].map(|_| unsafe {
            let texture = gl.create_texture().unwrap();
            gl.bind_texture(gl::TEXTURE_2D, Some(texture));
            gl.tex_storage_2d(gl::TEXTURE_2D, 1, gl::RGBA8, VIEW_SIZE, VIEW_SIZE);

            texture
        });
        let renderer = StagingRenderer::new(
            Rc::clone(&context),
            targets,
            UVec2::splat(VIEW_SIZE as u32),
            false,
        );

        (context, renderer)
    }

    fn frame(images: [DmabufImage; 2]) -> DmabufFrame {
        DmabufFrame {
            width: FRAME_WIDTH as u32,
            height: VIEW_SIZE as u32,
            format: YuvFormat::Nv12,
            images,
            color_space: YuvColorSpace::Bt709,
            full_range: false,
            chroma_location: ChromaLocation::Left,
        }
    }

    // A linear NV12 frame stored in a shared memory file, like a decoder dma-buf. The file must
    // outlive the frame.
    fn shared_memory_frame(luma: &[u8], chroma: &[u8]) -> (File, DmabufFrame) {
        let mut file = create_shared_memory_file();
        file.write_all(luma).unwrap();
        file.write_all(chroma).unwrap();

        let fd = file.as_raw_fd();
        let image = |drm_fourcc, offset| DmabufImage {
            drm_fourcc,
            plane_count: 1,
            planes: [DmabufPlane {
                fd,
                offset,
                pitch: FRAME_WIDTH as u32,
                // Linear without explicit modifier, which needs no modifiers extension
                modifier: 0x00ff_ffff_ffff_ffff,
            }; 4],
        };
        let frame = frame([
            image(DRM_FORMAT_R8, 0),
            image(DRM_FORMAT_GR88, luma.len() as u32),
        ]);

        (file, frame)
    }

    // RGBA rows of a view, from the bottom
    fn read_view(gl: &gl::Context, framebuffer: gl::Framebuffer) -> Vec<u8> {
        let mut pixels = vec![0_u8; (VIEW_SIZE * VIEW_SIZE * 4) as usize];
        unsafe {
            gl.bind_framebuffer(gl::READ_FRAMEBUFFER, Some(framebuffer));
            gl.read_pixels(
                0,
                0,
                VIEW_SIZE,
                VIEW_SIZE,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                gl::PixelPackData::Slice(Some(&mut pixels)),
            );
            assert_eq!(gl.get_error(), gl::NO_ERROR);
        }

        pixels
    }

    fn assert_views_are_red(gl: &gl::Context, renderer: &StagingRenderer) {
        for framebuffer in renderer.framebuffers {
            for pixel in read_view(gl, framebuffer).chunks(4) {
                assert!(
                    pixel[0] >= 252 && pixel[1] <= 3 && pixel[2] <= 3,
                    "{pixel:?}"
                );
            }
        }
    }

    // These tests need a working EGL implementation. Mesa's software renderer works:
    // EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 cargo test -p alvr_graphics -- --ignored
    #[test]
    #[ignore = "needs EGL"]
    fn linux_staging_shader_converts_yuv() {
        let (context, renderer) = create_renderer();
        let gl = &context.gl_context;

        // Both views side by side
        let luma = vec![RED_LUMA; (FRAME_WIDTH * VIEW_SIZE) as usize];
        let chroma = RED_CHROMA.repeat((FRAME_WIDTH * VIEW_SIZE / 4) as usize);
        unsafe {
            for (unit, format, width, height, data) in [
                (0, gl::R8, FRAME_WIDTH, VIEW_SIZE, &luma[..]),
                (1, gl::RG8, FRAME_WIDTH / 2, VIEW_SIZE / 2, &chroma[..]),
            ] {
                gl.active_texture(gl::TEXTURE0 + unit);
                gl.bind_texture(
                    gl::TEXTURE_2D,
                    Some(renderer.yuv_input.plane_textures[unit as usize]),
                );
                gl.pixel_store_i32(gl::UNPACK_ALIGNMENT, 1);
                gl.tex_image_2d(
                    gl::TEXTURE_2D,
                    0,
                    format as i32,
                    width,
                    height,
                    0,
                    if format == gl::R8 { gl::RED } else { gl::RG },
                    gl::UNSIGNED_BYTE,
                    gl::PixelUnpackData::Slice(Some(data)),
                );
            }
        }

        renderer.draw_views(&frame([DmabufImage::default(); 2]));

        assert_views_are_red(gl, &renderer);
    }

    // Imports a linear NV12 frame stored in a shared memory file, like a decoder dma-buf
    #[test]
    #[ignore = "needs EGL"]
    fn linux_staging_renders_dmabuf_frame() {
        let (context, renderer) = create_renderer();
        let gl = &context.gl_context;
        context.check_dmabuf_import_support().unwrap();

        let luma_size = (FRAME_WIDTH * VIEW_SIZE) as usize;
        let (_file, mut frame) = shared_memory_frame(
            &vec![RED_LUMA; luma_size],
            &RED_CHROMA.repeat(luma_size / 4),
        );

        renderer.render(ptr::from_mut(&mut frame).cast());

        assert_views_are_red(gl, &renderer);

        // A frame that cannot be imported is an error, not a crash
        frame.images[1].planes[0].fd = -1;
        let mut rendered = false;
        let result = unsafe {
            context.render_dmabuf_using_textures(&frame, renderer.yuv_input.plane_textures, |_| {
                rendered = true
            })
        };
        assert!(result.is_err() && !rendered);
    }

    // With left sited chroma, even luma columns are at a chroma sample and odd ones halfway between
    // two. Chroma columns that alternate between two values tell this apart from centered sampling
    // and from an offset with the wrong sign.
    #[test]
    #[ignore = "needs EGL"]
    fn linux_staging_samples_left_sited_chroma() {
        const GRAY_LUMA: u8 = 126;
        const NEUTRAL_CHROMA: u8 = 128;
        const ALTERNATING_CR: [u8; 2] = [96, 160];
        let chroma_width = FRAME_WIDTH / 2;

        let (context, renderer) = create_renderer();
        let gl = &context.gl_context;

        let chroma_row = (0..chroma_width)
            .flat_map(|column| [NEUTRAL_CHROMA, ALTERNATING_CR[column as usize % 2]])
            .collect::<Vec<_>>();
        let (_file, mut frame) = shared_memory_frame(
            &vec![GRAY_LUMA; (FRAME_WIDTH * VIEW_SIZE) as usize],
            &chroma_row.repeat((VIEW_SIZE / 2) as usize),
        );

        renderer.render(ptr::from_mut(&mut frame).cast());

        // Linear filtering at chroma texel x / 2, clamped to the last column
        let expected_cr = |luma_column: i32| {
            let cr = |chroma_column: i32| {
                ALTERNATING_CR[chroma_column.min(chroma_width - 1) as usize % 2] as f32
            };
            let chroma_column = luma_column / 2;

            if luma_column % 2 == 0 {
                cr(chroma_column)
            } else {
                (cr(chroma_column) + cr(chroma_column + 1)) / 2.0
            }
        };
        let (matrix, offset) = crate::yuv_to_rgb_conversion(YuvColorSpace::Bt709, false, 8);
        let expected_red = |luma_column| {
            let yuv = [
                GRAY_LUMA as f32,
                NEUTRAL_CHROMA as f32,
                expected_cr(luma_column),
            ];
            let yuv = [0, 1, 2].map(|i| yuv[i] / 255.0 - offset[i]);

            (matrix[0] * yuv[0] + matrix[3] * yuv[1] + matrix[6] * yuv[2]) * 255.0
        };

        for (view_index, framebuffer) in renderer.framebuffers.into_iter().enumerate() {
            let pixels = read_view(gl, framebuffer);
            for (pixel_index, pixel) in pixels.chunks(4).enumerate() {
                let luma_column = view_index as i32 * VIEW_SIZE + pixel_index as i32 % VIEW_SIZE;
                let expected = expected_red(luma_column);

                assert!(
                    (pixel[0] as f32 - expected).abs() <= 2.0,
                    "luma column {luma_column}: red {} != {expected}",
                    pixel[0]
                );
            }
        }
    }

    fn create_shared_memory_file() -> File {
        let path = Path::new("/dev/shm").join(format!("alvr_dmabuf_test_{}", std::process::id()));
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();

        file
    }
}
