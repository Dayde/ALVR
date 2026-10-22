mod c_api;
mod extra_extensions;
mod graphics;
mod interaction;
mod lobby;
mod passthrough;
mod stream;

use crate::stream::ParsedStreamConfig;
use alvr_client_core::{ClientCapabilities, ClientCoreContext, ClientCoreEvent};
use alvr_common::settings_schema::Switch;
use alvr_common::{
    error,
    glam::{Quat, UVec2, Vec3},
    info,
    parking_lot::RwLock,
    Fov, Pose, HAND_LEFT_ID,
};
use alvr_graphics::GraphicsContext;
use alvr_session::{BodyTrackingBDConfig, BodyTrackingSourcesConfig};
use alvr_system_info::Platform;
use extra_extensions::{
    BD_BODY_TRACKING_EXTENSION_NAME, BD_MOTION_TRACKING_EXTENSION_NAME,
    META_BODY_TRACKING_FULL_BODY_EXTENSION_NAME, META_DETACHED_CONTROLLERS_EXTENSION_NAME,
    META_SIMULTANEOUS_HANDS_AND_CONTROLLERS_EXTENSION_NAME, PICO_CONFIGURATION_EXTENSION_NAME,
};
use interaction::{InteractionContext, InteractionSourcesConfig};
use lobby::Lobby;
use openxr as xr;
use passthrough::PassthroughLayer;
use std::{
    path::Path,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
use stream::StreamContext;

fn from_xr_vec3(v: xr::Vector3f) -> Vec3 {
    Vec3::new(v.x, v.y, v.z)
}

fn to_xr_vec3(v: Vec3) -> xr::Vector3f {
    xr::Vector3f {
        x: v.x,
        y: v.y,
        z: v.z,
    }
}

fn from_xr_quat(q: xr::Quaternionf) -> Quat {
    Quat::from_xyzw(q.x, q.y, q.z, q.w)
}

fn to_xr_quat(q: Quat) -> xr::Quaternionf {
    xr::Quaternionf {
        x: q.x,
        y: q.y,
        z: q.z,
        w: q.w,
    }
}

fn from_xr_pose(p: xr::Posef) -> Pose {
    Pose {
        orientation: from_xr_quat(p.orientation),
        position: from_xr_vec3(p.position),
    }
}

fn to_xr_pose(p: Pose) -> xr::Posef {
    xr::Posef {
        orientation: to_xr_quat(p.orientation),
        position: to_xr_vec3(p.position),
    }
}

fn from_xr_fov(f: xr::Fovf) -> Fov {
    Fov {
        left: f.angle_left,
        right: f.angle_right,
        up: f.angle_up,
        down: f.angle_down,
    }
}

fn to_xr_fov(f: Fov) -> xr::Fovf {
    xr::Fovf {
        angle_left: f.left,
        angle_right: f.right,
        angle_up: f.up,
        angle_down: f.down,
    }
}

fn from_xr_time(timestamp: xr::Time) -> Duration {
    Duration::from_nanos(timestamp.as_nanos() as _)
}

fn to_xr_time(timestamp: Duration) -> xr::Time {
    xr::Time::from_nanos(timestamp.as_nanos() as _)
}

fn default_view() -> xr::View {
    xr::View {
        pose: xr::Posef {
            orientation: xr::Quaternionf {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            },
            position: xr::Vector3f::default(),
        },
        fov: xr::Fovf {
            angle_left: -1.0,
            angle_right: 1.0,
            angle_up: 1.0,
            angle_down: -1.0,
        },
    }
}

// This exists to circumvent dead-code analysis
fn create_session(
    xr_instance: &xr::Instance,
    xr_system: xr::SystemId,
    graphics_context: &GraphicsContext,
) -> (
    xr::Session<xr::OpenGlEs>,
    xr::FrameWaiter,
    xr::FrameStream<xr::OpenGlEs>,
) {
    #[cfg(target_os = "android")]
    unsafe {
        xr_instance
            .create_session(xr_system, &graphics::session_create_info(graphics_context))
            .unwrap()
    }
    #[cfg(not(target_os = "android"))]
    unsafe {
        graphics::create_egl_session(xr_instance, xr_system, graphics_context)
    }
}

static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

// Asks the frame loop to end the OpenXR session and return from entry_point(). Only stores an
// atomic, so it is safe to call from a signal handler.
pub fn request_exit() {
    EXIT_REQUESTED.store(true, Ordering::Relaxed);
}

// Counts submitted frames, for the watchdog below
static FRAMES_SUBMITTED: AtomicU64 = AtomicU64::new(0);

// If Monado started badly (display not set up yet), the session can stay idle or xrWaitFrame can
// block forever: no frames, black headset. Exit so the service restarts the client.
#[cfg(not(target_os = "android"))]
fn spawn_frame_watchdog() {
    thread::spawn(|| {
        let mut last_count = 0;
        let mut stalled_secs = 0;
        loop {
            thread::sleep(Duration::from_secs(1));
            let count = FRAMES_SUBMITTED.load(Ordering::Relaxed);
            if count != last_count || EXIT_REQUESTED.load(Ordering::Relaxed) {
                last_count = count;
                stalled_secs = 0;
            } else {
                stalled_secs += 1;
                if stalled_secs >= 8 {
                    error!("No frame submitted for 8 s, exiting so the client is restarted");
                    std::process::exit(1);
                }
            }
        }
    });
}

pub fn entry_point() {
    alvr_client_core::init_logging();

    #[cfg(not(target_os = "android"))]
    spawn_frame_watchdog();

    let platform = alvr_system_info::platform();

    let loader_suffix = match platform {
        Platform::Quest1 => "_quest1",
        Platform::PicoNeo3
        | Platform::PicoG3
        | Platform::Pico4
        | Platform::Pico4Pro
        | Platform::Pico4Enterprise => "_pico_old",
        p if p.is_yvr() => "_yvr",
        Platform::Lynx => "_lynx",
        _ => "",
    };
    #[cfg(target_os = "android")]
    let loader_name = format!("libopenxr_loader{loader_suffix}.so");
    // On desktop Linux the loader comes from the system "openxr" package
    #[cfg(not(target_os = "android"))]
    let loader_name = format!("libopenxr_loader{loader_suffix}.so.1");
    let xr_entry = unsafe { xr::Entry::load_from(Path::new(&loader_name)).unwrap() };

    #[cfg(target_os = "android")]
    xr_entry.initialize_android_loader().unwrap();

    let available_extensions = xr_entry.enumerate_extensions().unwrap();
    alvr_common::info!("OpenXR available extensions: {available_extensions:#?}");

    // todo: switch to vulkan
    assert!(available_extensions.khr_opengl_es_enable);

    let mut exts = xr::ExtensionSet::default();
    exts.bd_controller_interaction = available_extensions.bd_controller_interaction;
    exts.ext_eye_gaze_interaction = available_extensions.ext_eye_gaze_interaction;
    exts.ext_hand_tracking = available_extensions.ext_hand_tracking;
    exts.ext_local_floor = available_extensions.ext_local_floor;
    exts.fb_body_tracking = available_extensions.fb_body_tracking;
    exts.fb_color_space = available_extensions.fb_color_space;
    exts.fb_composition_layer_settings = available_extensions.fb_composition_layer_settings;
    exts.fb_display_refresh_rate = available_extensions.fb_display_refresh_rate;
    exts.fb_eye_tracking_social = available_extensions.fb_eye_tracking_social;
    exts.fb_face_tracking2 = available_extensions.fb_face_tracking2;
    exts.fb_foveation = available_extensions.fb_foveation;
    exts.fb_foveation_configuration = available_extensions.fb_foveation_configuration;
    exts.fb_passthrough = available_extensions.fb_passthrough;
    exts.fb_swapchain_update_state = available_extensions.fb_swapchain_update_state;
    exts.htc_facial_tracking = available_extensions.htc_facial_tracking;
    exts.htc_passthrough = available_extensions.htc_passthrough;
    exts.htc_vive_focus3_controller_interaction =
        available_extensions.htc_vive_focus3_controller_interaction;
    #[cfg(target_os = "android")]
    {
        exts.khr_android_create_instance = true;
    }
    // Desktop runtimes (Monado) take the EGL context through XR_MNDX_egl_enable
    #[cfg(not(target_os = "android"))]
    {
        assert!(available_extensions.mndx_egl_enable);
        exts.mndx_egl_enable = true;
    }
    exts.khr_convert_timespec_time = true;
    exts.khr_opengl_es_enable = true;
    exts.other = available_extensions
        .other
        .into_iter()
        .filter(|ext| {
            [
                META_BODY_TRACKING_FULL_BODY_EXTENSION_NAME,
                META_SIMULTANEOUS_HANDS_AND_CONTROLLERS_EXTENSION_NAME,
                META_DETACHED_CONTROLLERS_EXTENSION_NAME,
                BD_BODY_TRACKING_EXTENSION_NAME,
                BD_MOTION_TRACKING_EXTENSION_NAME,
                PICO_CONFIGURATION_EXTENSION_NAME,
            ]
            .contains(&ext.as_str())
        })
        .collect();

    let available_layers = xr_entry.enumerate_layers().unwrap();
    alvr_common::info!("OpenXR available layers: {available_layers:#?}");

    let xr_instance = xr_entry
        .create_instance(
            &xr::ApplicationInfo {
                application_name: "ALVR Client",
                application_version: 0,
                engine_name: "ALVR",
                engine_version: 0,
            },
            &exts,
            &[],
        )
        .unwrap();

    let graphics_context = Rc::new(GraphicsContext::new_gl());

    let mut last_lobby_message = String::new();

    'session_loop: loop {
        let xr_system = xr_instance
            .system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)
            .unwrap();

        // mandatory call
        let _ = xr_instance
            .graphics_requirements::<xr::OpenGlEs>(xr_system)
            .unwrap();

        let (xr_session, mut xr_frame_waiter, mut xr_frame_stream) =
            create_session(&xr_instance, xr_system, &graphics_context);

        let views_config = xr_instance
            .enumerate_view_configuration_views(
                xr_system,
                xr::ViewConfigurationType::PRIMARY_STEREO,
            )
            .unwrap();
        assert_eq!(views_config.len(), 2);

        let default_view_resolution = UVec2::new(
            views_config[0].recommended_image_rect_width,
            views_config[0].recommended_image_rect_height,
        );

        let refresh_rates = if exts.fb_display_refresh_rate {
            xr_session.enumerate_display_refresh_rates().unwrap()
        } else {
            vec![90.0]
        };

        if exts.fb_color_space {
            xr_session
                .set_color_space(xr::ColorSpaceFB::REC709)
                .unwrap();
        }

        let capabilities = ClientCapabilities {
            default_view_resolution,
            refresh_rates,
            foveated_encoding: platform != Platform::Unknown,
            encoder_high_profile: platform != Platform::Unknown,
            encoder_10_bits: platform != Platform::Unknown,
            encoder_av1: matches!(
                platform,
                Platform::Quest3 | Platform::Quest3S | Platform::Pico4Ultra | Platform::LinuxPc
            ),
            prefer_10bit: false,
            prefer_full_range: true,
            preferred_encoding_gamma: 1.0,
            prefer_hdr: false,
        };
        let core_context = Arc::new(ClientCoreContext::new(capabilities));

        let interaction_context = Arc::new(RwLock::new(InteractionContext::new(
            xr_session.clone(),
            exts.other.clone(),
            xr_system,
            platform,
        )));

        let mut lobby = Lobby::new(
            xr_session.clone(),
            Rc::clone(&graphics_context),
            Arc::clone(&interaction_context),
            platform,
            default_view_resolution,
            &last_lobby_message,
        );
        let lobby_body_tracking_config = BodyTrackingSourcesConfig {
            body_tracking_fb: Switch::Disabled,
            body_tracking_bd: Switch::Enabled(BodyTrackingBDConfig::BodyTracking {
                high_accuracy: true,
                prompt_calibration_on_start: false,
            }),
        };
        let lobby_interaction_sources = InteractionSourcesConfig {
            face_tracking: None,
            body_tracking: Some(lobby_body_tracking_config),
            prefers_multimodal_input: true,
        };
        interaction_context
            .write()
            .select_sources(&lobby_interaction_sources);

        let mut session_running = false;
        let mut exit_sent = false;
        let mut stream_context = None::<StreamContext>;
        let mut passthrough_layer = None;

        #[cfg(not(target_os = "android"))]
        let mut frame_stats = FrameLoopStats::new();

        let mut event_storage = xr::EventDataBuffer::new();
        'render_loop: loop {
            while let Some(event) = xr_instance.poll_event(&mut event_storage).unwrap() {
                match event {
                    xr::Event::EventsLost(event) => {
                        error!("OpenXR: lost {} events!", event.lost_event_count());
                    }
                    xr::Event::InstanceLossPending(_) => break 'session_loop,
                    xr::Event::SessionStateChanged(event) => match {
                        info!("OpenXR session state: {:?}", event.state());
                        event.state()
                    } {
                        xr::SessionState::READY => {
                            xr_session
                                .begin(xr::ViewConfigurationType::PRIMARY_STEREO)
                                .unwrap();

                            core_context.resume();

                            passthrough_layer = PassthroughLayer::new(&xr_session, platform).ok();

                            session_running = true;
                        }
                        xr::SessionState::STOPPING => {
                            session_running = false;

                            passthrough_layer = None;

                            core_context.pause();

                            xr_session.end().unwrap();
                        }
                        xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => {
                            if EXIT_REQUESTED.load(Ordering::Relaxed) {
                                break 'session_loop;
                            }
                            break 'render_loop;
                        }
                        _ => (),
                    },
                    xr::Event::ReferenceSpaceChangePending(event) => {
                        info!(
                            "ReferenceSpaceChangePending type: {:?}",
                            event.reference_space_type()
                        );

                        lobby.update_reference_space();

                        if let Some(stream) = &mut stream_context {
                            stream.update_reference_space();
                        }
                    }
                    xr::Event::PerfSettingsEXT(event) => {
                        info!(
                            "Perf: from {:?} to {:?}, domain: {:?}/{:?}",
                            event.from_level(),
                            event.to_level(),
                            event.domain(),
                            event.sub_domain(),
                        );
                    }
                    xr::Event::InteractionProfileChanged(_)
                    | xr::Event::PassthroughStateChangedFB(_) => {
                        // todo
                    }
                    _ => (),
                }
            }

            if EXIT_REQUESTED.load(Ordering::Relaxed) {
                if !session_running {
                    break 'session_loop;
                } else if !exit_sent {
                    info!("Exit requested, ending the OpenXR session");
                    xr_session.request_exit().ok();
                    exit_sent = true;
                }
            }

            if !session_running {
                thread::sleep(Duration::from_millis(100));
                continue;
            }

            while let Some(event) = core_context.poll_event() {
                match event {
                    ClientCoreEvent::UpdateHudMessage(message) => {
                        last_lobby_message.clone_from(&message);
                        lobby.update_hud_message(&message);
                    }
                    ClientCoreEvent::StreamingStarted(config) => {
                        let config = ParsedStreamConfig::new(&config);

                        let context = StreamContext::new(
                            Arc::clone(&core_context),
                            xr_session.clone(),
                            Rc::clone(&graphics_context),
                            Arc::clone(&interaction_context),
                            platform,
                            config,
                        );

                        if !context.uses_passthrough() {
                            passthrough_layer = None;
                        }

                        stream_context = Some(context);
                    }
                    ClientCoreEvent::StreamingStopped => {
                        if passthrough_layer.is_none() {
                            passthrough_layer = PassthroughLayer::new(&xr_session, platform).ok();
                        }

                        interaction_context
                            .write()
                            .select_sources(&lobby_interaction_sources);

                        stream_context = None;
                    }
                    ClientCoreEvent::Haptics {
                        device_id,
                        duration,
                        frequency,
                        amplitude,
                    } => {
                        let idx = if device_id == *HAND_LEFT_ID { 0 } else { 1 };
                        let action =
                            &interaction_context.read().hands_interaction[idx].vibration_action;

                        action
                            .apply_feedback(
                                &xr_session,
                                xr::Path::NULL,
                                &xr::HapticVibration::new()
                                    .amplitude(amplitude.clamp(0.0, 1.0))
                                    .frequency(frequency.max(0.0))
                                    .duration(xr::Duration::from_nanos(duration.as_nanos() as _)),
                            )
                            .unwrap();
                    }
                    ClientCoreEvent::DecoderConfig { codec, config_nal } => {
                        eprintln!("Decoder config: {codec:?}, {} bytes", config_nal.len());
                        if let Some(stream) = &mut stream_context {
                            stream.maybe_initialize_decoder(codec, config_nal);
                        }
                    }
                    ClientCoreEvent::RealTimeConfig(config) => {
                        if config.passthrough.is_some() && passthrough_layer.is_none() {
                            passthrough_layer = PassthroughLayer::new(&xr_session, platform).ok();
                        } else if config.passthrough.is_none() && passthrough_layer.is_some() {
                            passthrough_layer = None;
                        }

                        if let Some(stream) = &mut stream_context {
                            stream.update_real_time_config(&config);
                        }
                    }
                }
            }

            #[cfg(not(target_os = "android"))]
            let wait_start = std::time::Instant::now();
            let frame_state = match xr_frame_waiter.wait() {
                Ok(state) => state,
                Err(e) => {
                    error!("{e}");
                    panic!();
                }
            };
            let frame_interval =
                Duration::from_nanos(frame_state.predicted_display_period.as_nanos() as _);
            let vsync_time =
                Duration::from_nanos(frame_state.predicted_display_time.as_nanos() as _);
            #[cfg(not(target_os = "android"))]
            {
                frame_stats.waits += 1;
                frame_stats.wait_time += wait_start.elapsed();
                frame_stats.skipped += u32::from(!frame_state.should_render);
                frame_stats.period = frame_interval;
            }

            xr_frame_stream.begin().unwrap();

            if !frame_state.should_render {
                xr_frame_stream
                    .end(
                        frame_state.predicted_display_time,
                        xr::EnvironmentBlendMode::OPAQUE,
                        &[],
                    )
                    .unwrap();

                continue;
            }

            #[cfg(not(target_os = "android"))]
            let render_start = std::time::Instant::now();
            // todo: allow rendering lobby and stream layers at the same time and add cross fade
            let (layer, display_time) = if let Some(stream) = &mut stream_context {
                stream.render(frame_interval, vsync_time)
            } else {
                (lobby.render(vsync_time), vsync_time)
            };
            #[cfg(not(target_os = "android"))]
            let end_start = {
                frame_stats.render_time += render_start.elapsed();
                std::time::Instant::now()
            };

            let layers: &[&xr::CompositionLayerBase<_>] =
                if let Some(passthrough_layer) = &passthrough_layer {
                    &[passthrough_layer, &layer.build()]
                } else {
                    &[&layer.build()]
                };

            graphics_context.make_current();
            let res = xr_frame_stream.end(
                to_xr_time(display_time),
                xr::EnvironmentBlendMode::OPAQUE,
                layers,
            );

            #[cfg(not(target_os = "android"))]
            {
                frame_stats.end_time += end_start.elapsed();
                frame_stats.maybe_print();
                FRAMES_SUBMITTED.fetch_add(1, Ordering::Relaxed);
            }

            if let Err(e) = res {
                let time = to_xr_time(display_time);
                error!("End frame failed! {e}, timestamp: {display_time:?}, time: {time:?}");

                if !platform.is_vive() {
                    xr_frame_stream
                        .end(
                            frame_state.predicted_display_time,
                            xr::EnvironmentBlendMode::OPAQUE,
                            &[],
                        )
                        .unwrap();
                }
            }
        }
    }

    // grapics_context is dropped here
}

// Frame loop timings, printed periodically on desktop to see where frame time goes
#[cfg(not(target_os = "android"))]
struct FrameLoopStats {
    start: std::time::Instant,
    waits: u32,
    skipped: u32,
    period: Duration,
    wait_time: Duration,
    render_time: Duration,
    end_time: Duration,
}

#[cfg(not(target_os = "android"))]
impl FrameLoopStats {
    fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            waits: 0,
            skipped: 0,
            period: Duration::ZERO,
            wait_time: Duration::ZERO,
            render_time: Duration::ZERO,
            end_time: Duration::ZERO,
        }
    }

    fn maybe_print(&mut self) {
        let elapsed = self.start.elapsed();
        if elapsed < Duration::from_secs(5) {
            return;
        }

        use std::sync::atomic::Ordering;

        let poll_time =
            Duration::from_nanos(stream::FRAME_POLL_NANOS.swap(0, Ordering::Relaxed));
        let without_new_image = stream::FRAMES_WITHOUT_NEW_IMAGE.swap(0, Ordering::Relaxed);

        let per_frame = |total: Duration| total.as_secs_f32() * 1000.0 / self.waits.max(1) as f32;
        eprintln!(
            "Frame loop: {:.1} frames/s (runtime period {:.2} ms), {} not rendered, \
            {without_new_image} without a new image, avg ms: wait {:.2}, render {:.2} \
            (of which polling the decoder {:.2}), end {:.2}",
            self.waits as f32 / elapsed.as_secs_f32(),
            self.period.as_secs_f32() * 1000.0,
            self.skipped,
            per_frame(self.wait_time),
            per_frame(self.render_time),
            per_frame(poll_time),
            per_frame(self.end_time),
        );

        *self = Self::new();
    }
}

#[allow(unused)]
fn xr_runtime_now(xr_instance: &xr::Instance) -> Option<xr::Time> {
    xr_instance
        .now()
        .ok()
        .filter(|&time_nanos| time_nanos.as_nanos() > 0)
}

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: android_activity::AndroidApp) {
    use android_activity::{InputStatus, MainEvent, PollEvent};

    let rendering_thread = thread::spawn(|| {
        // workaround for the Pico runtime
        let context = ndk_context::android_context();
        let vm = unsafe { jni::JavaVM::from_raw(context.vm().cast()) }.unwrap();
        let _env = vm.attach_current_thread().unwrap();

        entry_point();
    });

    let mut should_quit = false;
    while !should_quit {
        app.poll_events(Some(Duration::from_millis(100)), |event| match event {
            PollEvent::Main(MainEvent::Destroy) => {
                should_quit = true;
            }
            PollEvent::Main(MainEvent::InputAvailable) => {
                if let Ok(mut iter) = app.input_events_iter() {
                    while iter.next(|_| InputStatus::Unhandled) {}
                }
            }
            _ => (),
        });
    }

    // Note: the quit event is sent from OpenXR too, this will return rather quicly.
    rendering_thread.join().unwrap();
}
