// Desktop Linux client: runs as a regular OpenXR app on a local runtime (Monado), which drives the
// headset, and streams from an ALVR streamer over the network.
fn main() {
    // The client never opens a window, so steer wgpu's EGL setup away from Wayland and X11 to the
    // surfaceless platform, which supports the offscreen pbuffer the renderer needs.
    if std::env::var_os("ALVR_KEEP_WINDOW_SYSTEM").is_none() {
        std::env::set_var("WAYLAND_DISPLAY", "alvr-no-wayland");
        std::env::remove_var("DISPLAY");
    }

    alvr_client_openxr::entry_point();
}
