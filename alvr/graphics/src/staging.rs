use super::{ck, GraphicsContext};
#[cfg(not(target_os = "linux"))]
use crate::GL_TEXTURE_EXTERNAL_OES;
use alvr_common::glam::{IVec2, UVec2};
use glow::{self as gl, HasContext};
use std::{ffi::c_void, rc::Rc};

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

// Returns the matrix (column major) and offset that turn sampled YUV into RGB
#[cfg(target_os = "linux")]
fn yuv_to_rgb(color_space: crate::YuvColorSpace, full_range: bool) -> ([f32; 9], [f32; 3]) {
    use crate::YuvColorSpace;

    let (kr, kb) = match color_space {
        YuvColorSpace::Bt601 => (0.299, 0.114),
        YuvColorSpace::Bt709 => (0.2126, 0.0722),
        YuvColorSpace::Bt2020 => (0.2627, 0.0593),
    };
    let kg = 1.0 - kr - kb;

    let (y_scale, c_scale, y_offset) = if full_range {
        (1.0, 1.0, 0.0)
    } else {
        (255.0 / 219.0, 255.0 / 224.0, 16.0 / 255.0)
    };

    let r_cr = 2.0 * (1.0 - kr);
    let b_cb = 2.0 * (1.0 - kb);
    let g_cb = 2.0 * kb * (1.0 - kb) / kg;
    let g_cr = 2.0 * kr * (1.0 - kr) / kg;

    (
        [
            // Y column
            y_scale,
            y_scale,
            y_scale,
            // Cb column
            0.0,
            -g_cb * c_scale,
            b_cb * c_scale,
            // Cr column
            r_cr * c_scale,
            -g_cr * c_scale,
            0.0,
        ],
        [y_offset, 128.0 / 255.0, 128.0 / 255.0],
    )
}

pub struct StagingRenderer {
    context: Rc<GraphicsContext>,
    program: gl::Program,
    view_idx_uloc: gl::UniformLocation,
    #[cfg(not(target_os = "linux"))]
    surface_texture: gl::Texture,
    // Luma and chroma planes of the decoded frame
    #[cfg(target_os = "linux")]
    plane_textures: [gl::Texture; 2],
    #[cfg(target_os = "linux")]
    yuv_matrix_uloc: gl::UniformLocation,
    #[cfg(target_os = "linux")]
    yuv_offset_uloc: gl::UniformLocation,
    // Frames drawn since stats_start, printed periodically
    #[cfg(target_os = "linux")]
    stats: std::cell::Cell<(std::time::Instant, u32)>,
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

        #[cfg(not(target_os = "linux"))]
        let frag_source = include_str!("../resources/staging_fragment.glsl");
        #[cfg(target_os = "linux")]
        let frag_source = include_str!("../resources/staging_fragment_linux.glsl");

        // Add #defines into the shader after the first line
        let mut frag_lines: Vec<&str> = frag_source.lines().collect();
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

            // Storage comes from the decoder's dma-bufs each frame
            #[cfg(target_os = "linux")]
            let plane_textures = [(); 2].map(|_| {
                let texture = ck!(gl.create_texture().unwrap());
                ck!(gl.bind_texture(gl::TEXTURE_2D, Some(texture)));
                for (param, value) in [
                    (gl::TEXTURE_MIN_FILTER, gl::LINEAR),
                    (gl::TEXTURE_MAG_FILTER, gl::LINEAR),
                    (gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE),
                    (gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE),
                ] {
                    ck!(gl.tex_parameter_i32(gl::TEXTURE_2D, param, value as i32));
                }
                ck!(gl.bind_texture(gl::TEXTURE_2D, None));

                texture
            });

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

            #[cfg(target_os = "linux")]
            let (yuv_matrix_uloc, yuv_offset_uloc) = {
                ck!(gl.use_program(Some(program)));
                for (name, unit) in [("y_tex", 0), ("uv_tex", 1)] {
                    let location = ck!(gl.get_uniform_location(program, name)).unwrap();
                    ck!(gl.uniform_1_i32(Some(&location), unit));
                }
                ck!(gl.use_program(None));

                (
                    ck!(gl.get_uniform_location(program, "yuv_matrix")).unwrap(),
                    ck!(gl.get_uniform_location(program, "yuv_offset")).unwrap(),
                )
            };

            Self {
                context,
                program,
                #[cfg(not(target_os = "linux"))]
                surface_texture,
                #[cfg(target_os = "linux")]
                plane_textures,
                #[cfg(target_os = "linux")]
                yuv_matrix_uloc,
                #[cfg(target_os = "linux")]
                yuv_offset_uloc,
                #[cfg(target_os = "linux")]
                stats: std::cell::Cell::new((std::time::Instant::now(), 0)),
                view_idx_uloc,
                framebuffers: framebuffers.try_into().unwrap(),
                viewport_size: view_resolution.as_ivec2(),
            }
        }
    }

    /// # Safety
    /// `buffer` must point to a valid DmabufFrame.
    #[cfg(target_os = "linux")]
    pub unsafe fn render(&self, buffer: *mut c_void) {
        let gl = &self.context.gl_context;
        self.context.make_current();

        let frame = &*(buffer as *const crate::DmabufFrame);

        let (stats_start, count) = self.stats.get();
        if stats_start.elapsed().as_secs() >= 5 {
            eprintln!(
                "Renderer: {:.1} new frames/s, {}x{} format {:#x}, {:?} {} range",
                count as f32 / stats_start.elapsed().as_secs_f32(),
                frame.width,
                frame.height,
                frame.drm_format,
                frame.color_space,
                if frame.full_range { "full" } else { "limited" },
            );
            self.stats.set((std::time::Instant::now(), 1));
        } else {
            self.stats.set((stats_start, count + 1));
        }

        self.context
            .render_dmabuf_using_textures(frame, self.plane_textures, || unsafe {
                ck!(gl.use_program(Some(self.program)));

                let (matrix, offset) = yuv_to_rgb(frame.color_space, frame.full_range);
                ck!(gl.uniform_matrix_3_f32_slice(
                    Some(&self.yuv_matrix_uloc),
                    false,
                    &matrix
                ));
                ck!(gl.uniform_3_f32_slice(Some(&self.yuv_offset_uloc), &offset));

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
            });
    }

    #[cfg(not(target_os = "linux"))]
    #[allow(unused_variables)]
    pub unsafe fn render(&self, hardware_buffer: *mut c_void) {
        let gl = &self.context.gl_context;
        self.context.make_current();

        self.context.render_ahardwarebuffer_using_texture(
            hardware_buffer,
            self.surface_texture,
            || unsafe {
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
        );
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
            for texture in self.plane_textures {
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
    use crate::YuvColorSpace;

    fn convert(color_space: YuvColorSpace, full_range: bool, yuv: [f32; 3]) -> [f32; 3] {
        let (m, o) = yuv_to_rgb(color_space, full_range);
        let v = [yuv[0] - o[0], yuv[1] - o[1], yuv[2] - o[2]];
        // Column major, like GLSL
        [0, 1, 2].map(|row| m[row] * v[0] + m[3 + row] * v[1] + m[6 + row] * v[2])
    }

    fn assert_close(a: [f32; 3], b: [f32; 3]) {
        for (x, y) in a.iter().zip(b) {
            assert!((x - y).abs() < 0.01, "{a:?} != {b:?}");
        }
    }

    #[test]
    fn limited_range_bt709() {
        let n = |v: f32| v / 255.0;
        assert_close(convert(YuvColorSpace::Bt709, false, [n(235.0), n(128.0), n(128.0)]), [1.0; 3]);
        assert_close(convert(YuvColorSpace::Bt709, false, [n(16.0), n(128.0), n(128.0)]), [0.0; 3]);
        // BT.709 limited range red: Y 63, Cb 102, Cr 240
        assert_close(
            convert(YuvColorSpace::Bt709, false, [n(63.0), n(102.0), n(240.0)]),
            [1.0, 0.0, 0.0],
        );
    }

    #[test]
    fn full_range_bt709() {
        let n = |v: f32| v / 255.0;
        assert_close(convert(YuvColorSpace::Bt709, true, [1.0, n(128.0), n(128.0)]), [1.0; 3]);
        // BT.709 full range blue: Y 18, Cb 255, Cr 116
        assert_close(
            convert(YuvColorSpace::Bt709, true, [n(18.0), n(255.0), n(116.0)]),
            [0.0, 0.0, 1.0],
        );
    }

    // Needs a working EGL implementation (llvmpipe is fine)
    #[test]
    #[ignore]
    fn staging_shader_compiles() {
        std::env::set_var("WAYLAND_DISPLAY", "alvr-no-wayland");
        std::env::remove_var("DISPLAY");

        let context = Rc::new(GraphicsContext::new_gl());
        context.make_current();
        let gl = &context.gl_context;
        let targets = unsafe {
            [(); 2].map(|_| {
                let texture = gl.create_texture().unwrap();
                gl.bind_texture(gl::TEXTURE_2D, Some(texture));
                gl.tex_storage_2d(gl::TEXTURE_2D, 1, gl::RGBA8, 64, 64);
                texture
            })
        };

        // Needed to attach the decoder's dma-buf images to GL_TEXTURE_2D
        assert!(gl.supported_extensions().contains("GL_OES_EGL_image"));

        let _renderer = StagingRenderer::new(context, targets, UVec2::new(64, 64), false);
    }
}
