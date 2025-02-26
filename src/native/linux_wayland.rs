#![allow(dead_code, static_mut_refs)]

mod libwayland_client;
mod libwayland_egl;
mod libxkbcommon;

mod clipboard;
mod decorations;
mod drag_n_drop;
mod extensions;
mod keycodes;
mod shm;

use libwayland_client::*;
use libwayland_egl::*;
use libxkbcommon::*;

use crate::{
    event::{EventHandler, KeyCode, KeyMods, MouseButton},
    native::{egl, NativeDisplayData, Request},
};

use core::time::Duration;

fn wl_fixed_to_double(f: i32) -> f32 {
    (f as f32) / 256.0
}

/// A thing to pass around within *void pointer of wayland's event handler
struct WaylandPayload {
    client: LibWaylandClient,
    display: *mut wl_display,
    registry: *mut wl_registry,
    // this is libwayland-egl.so, a library with ~4 functions
    // not the libEGL.so(which will be loaded, but not here)
    egl: LibWaylandEgl,
    xkb: LibXkbCommon,
    compositor: *mut wl_compositor,
    subcompositor: *mut wl_subcompositor,
    xdg_toplevel: *mut extensions::xdg_shell::xdg_toplevel,
    xdg_wm_base: *mut extensions::xdg_shell::xdg_wm_base,
    xdg_surface: *mut extensions::xdg_shell::xdg_surface,
    surface: *mut wl_surface,
    decoration_manager: *mut extensions::xdg_decoration::zxdg_decoration_manager_v1,
    viewporter: *mut extensions::viewporter::wp_viewporter,
    shm: *mut wl_shm,
    seat: *mut wl_seat,
    data_device_manager: *mut wl_data_device_manager,
    data_device: *mut wl_data_device,
    xkb_context: *mut xkb_context,
    keymap: *mut xkb_keymap,
    xkb_state: *mut xkb_state,

    egl_window: *mut wl_egl_window,
    pointer: *mut wl_pointer,
    keyboard: *mut wl_keyboard,
    focused_window: *mut wl_surface,
    //xkb_state: xkb::XkbState,
    decorations: Option<decorations::Decorations>,

    keyboard_context: KeyboardContext,
    drag_n_drop: drag_n_drop::WaylandDnD,
    update_requested: bool,
    event_handler: Option<Box<dyn EventHandler>>,
    closed: bool,
}

impl WaylandPayload {
    /// block until a new event is available
    // needs to combine both the Wayland events and the key repeat events
    // the implementation is translated from glfw
    unsafe fn block_on_new_event(&mut self) {
        let mut fds = [
            libc::pollfd {
                fd: (self.client.wl_display_get_fd)(self.display),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.keyboard_context.timerfd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        (self.client.wl_display_flush)(self.display);
        while (self.client.wl_display_prepare_read)(self.display) != 0 {
            (self.client.wl_display_dispatch_pending)(self.display);
        }
        if !self.update_requested && libc::poll(fds.as_mut_ptr(), 2, i32::MAX) > 0 {
            // if the Wayland display has events available
            if fds[0].revents & libc::POLLIN == 1 {
                (self.client.wl_display_read_events)(self.display);
                (self.client.wl_display_dispatch_pending)(self.display);
            } else {
                (self.client.wl_display_cancel_read)(self.display);
            }
            // if key repeat takes place
            if fds[1].revents & libc::POLLIN == 1 {
                let mut count: [libc::size_t; 1] = [0];
                let n_bits = core::mem::size_of::<libc::size_t>();
                assert_eq!(
                    libc::read(
                        self.keyboard_context.timerfd,
                        count.as_mut_ptr() as _,
                        n_bits
                    ),
                    n_bits as _
                );
                if let Some(key) = self.keyboard_context.repeated_key {
                    for _ in 0..count[0] {
                        EVENTS.push(WaylandEvent::KeyboardKey {
                            key,
                            state: WaylandKeyState::Repeat,
                        });
                    }
                }
            }
        } else {
            (self.client.wl_display_cancel_read)(self.display);
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WaylandKeyState {
    Released = 0,
    Pressed = 1,
    Repeat = 2,
}

impl From<WaylandKeyState> for bool {
    fn from(value: WaylandKeyState) -> bool {
        match value {
            WaylandKeyState::Released => false,
            WaylandKeyState::Pressed | WaylandKeyState::Repeat => true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RepeatInfo {
    Repeat { delay: Duration, gap: Duration },
    NoRepeat,
}

impl Default for RepeatInfo {
    // default value copied from winit
    fn default() -> Self {
        Self::Repeat {
            delay: Duration::from_millis(200),
            gap: Duration::from_millis(40),
        }
    }
}

// key repeat in Wayland needs to be handled by the client
// `KeyboardContext` is mostly for tracking the currently repeated key
// Note that apparently `timerfd` is not unix compliant and only available on linux
struct KeyboardContext {
    enter_serial: Option<core::ffi::c_uint>,
    repeat_info: RepeatInfo,
    repeated_key: Option<core::ffi::c_uint>,
    timerfd: core::ffi::c_int,
}

fn new_itimerspec() -> libc::itimerspec {
    libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
    }
}

impl KeyboardContext {
    fn new() -> Self {
        Self {
            enter_serial: None,
            repeat_info: Default::default(),
            repeated_key: None,
            timerfd: unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC) },
        }
    }
    fn key_down(&mut self, key: core::ffi::c_uint) {
        let mut timer = new_itimerspec();
        match self.repeat_info {
            RepeatInfo::Repeat { delay, gap } => {
                self.repeated_key = Some(key);
                timer.it_interval.tv_sec = gap.as_secs() as _;
                timer.it_interval.tv_nsec = gap.subsec_nanos() as _;
                timer.it_value.tv_sec = delay.as_secs() as _;
                timer.it_value.tv_nsec = delay.subsec_nanos() as _;
            }
            RepeatInfo::NoRepeat => self.repeated_key = None,
        }
        unsafe {
            libc::timerfd_settime(self.timerfd, 0, &timer, core::ptr::null_mut());
        }
    }
    fn key_up(&mut self, key: core::ffi::c_uint) {
        if self.repeated_key == Some(key) {
            self.repeated_key = None;
            unsafe {
                libc::timerfd_settime(self.timerfd, 0, &new_itimerspec(), core::ptr::null_mut());
            }
        }
    }
}

#[macro_export]
macro_rules! wl_request_constructor {
    ($libwayland:expr, $instance:expr, $request_name:expr, $interface:expr) => {
        wl_request_constructor!($libwayland, $instance, $request_name, $interface, ())
    };

    ($libwayland:expr, $instance:expr, $request_name:expr, $interface:expr, $($arg:expr),*) => {{
        let id: *mut wl_proxy;

        id = ($libwayland.wl_proxy_marshal_constructor)(
            $instance as _,
            $request_name,
            $interface as _,
            std::ptr::null_mut::<std::ffi::c_void>(),
            $($arg,)*
        );

        id as *mut _
    }};
}

#[macro_export]
macro_rules! wl_request {
    ($libwayland:expr, $instance:expr, $request_name:expr) => {
        wl_request!($libwayland, $instance, $request_name, ())
    };

    ($libwayland:expr, $instance:expr, $request_name:expr, $($arg:expr),*) => {{
        ($libwayland.wl_proxy_marshal)(
            $instance as _,
            $request_name,
            $($arg,)*
        )
    }};
}

static mut SEAT_LISTENER: wl_seat_listener = wl_seat_listener {
    capabilities: Some(seat_handle_capabilities),
    name: Some(seat_handle_name),
};

unsafe extern "C" fn seat_handle_capabilities(
    data: *mut std::ffi::c_void,
    seat: *mut wl_seat,
    caps: wl_seat_capability,
) {
    let display: &mut WaylandPayload = &mut *(data as *mut _);

    if caps & wl_seat_capability_WL_SEAT_CAPABILITY_POINTER != 0 {
        display.pointer = wl_request_constructor!(
            display.client,
            seat,
            WL_SEAT_GET_POINTER,
            display.client.wl_pointer_interface
        );
        assert!(!display.pointer.is_null());
        (display.client.wl_proxy_add_listener)(
            display.pointer as _,
            &POINTER_LISTENER as *const _ as _,
            data,
        );
    }

    if caps & wl_seat_capability_WL_SEAT_CAPABILITY_KEYBOARD != 0 {
        display.keyboard = wl_request_constructor!(
            display.client,
            seat,
            WL_SEAT_GET_KEYBOARD,
            display.client.wl_keyboard_interface
        );
        assert!(!display.keyboard.is_null());
        (display.client.wl_proxy_add_listener)(
            display.keyboard as _,
            &KEYBOARD_LISTENER as *const _ as _,
            data,
        );
    }
}

enum WaylandEvent {
    KeyboardLeave,
    KeyboardKey {
        key: core::ffi::c_uint,
        state: WaylandKeyState,
    },
    PointerMotion(f32, f32),
    PointerButton(MouseButton, bool),
    PointerAxis(f32, f32),
    FilesDropped(String),
}

static mut EVENTS: Vec<WaylandEvent> = Vec::new();

static mut KEYBOARD_LISTENER: wl_keyboard_listener = wl_keyboard_listener {
    keymap: Some(keyboard_handle_keymap),
    enter: Some(keyboard_handle_enter),
    leave: Some(keyboard_handle_leave),
    key: Some(keyboard_handle_key),
    modifiers: Some(keyboard_handle_modifiers),
    repeat_info: Some(keyboard_handle_repeat_info),
};

unsafe extern "C" fn keyboard_handle_keymap(
    data: *mut ::core::ffi::c_void,
    _wl_keyboard: *mut wl_keyboard,
    _format: u32,
    fd: i32,
    size: u32,
) {
    let display: &mut WaylandPayload = &mut *(data as *mut _);
    let map_shm = libc::mmap(
        std::ptr::null_mut::<std::ffi::c_void>(),
        size as usize,
        libc::PROT_READ,
        libc::MAP_PRIVATE,
        fd,
        0,
    );
    assert!(map_shm != libc::MAP_FAILED);
    (display.xkb.xkb_keymap_unref)(display.keymap);
    display.keymap = (display.xkb.xkb_keymap_new_from_string)(
        display.xkb_context,
        map_shm as *mut libc::FILE,
        1,
        0,
    );
    libc::munmap(map_shm, size as usize);
    libc::close(fd);
    (display.xkb.xkb_state_unref)(display.xkb_state);
    display.xkb_state = (display.xkb.xkb_state_new)(display.keymap);
}
unsafe extern "C" fn keyboard_handle_enter(
    data: *mut ::core::ffi::c_void,
    _wl_keyboard: *mut wl_keyboard,
    serial: ::core::ffi::c_uint,
    _surface: *mut wl_surface,
    _keys: *mut wl_array,
) {
    let display: &mut WaylandPayload = &mut *(data as *mut _);
    // Needed for setting the clipboard
    display.keyboard_context.enter_serial = Some(serial);
}
unsafe extern "C" fn keyboard_handle_leave(
    data: *mut ::core::ffi::c_void,
    _wl_keyboard: *mut wl_keyboard,
    _serial: u32,
    _surface: *mut wl_surface,
) {
    // Clear modifiers
    let display: &mut WaylandPayload = &mut *(data as *mut _);
    (display.xkb.xkb_state_update_mask)(display.xkb_state, 0, 0, 0, 0, 0, 0);
    // keyboard leave event must be handled here to stop key repeat, otherwise repeat events could
    // be pushed into EVENTS before the leave event is handled by the `event_handler`
    display.keyboard_context.repeated_key = None;
    display.keyboard_context.enter_serial = None;
    EVENTS.push(WaylandEvent::KeyboardLeave);
}
unsafe extern "C" fn keyboard_handle_key(
    data: *mut ::core::ffi::c_void,
    _wl_keyboard: *mut wl_keyboard,
    _serial: u32,
    _time: u32,
    key: u32,
    state: wl_keyboard_key_state,
) {
    let state = match state {
        0 => WaylandKeyState::Released,
        1 => WaylandKeyState::Pressed,
        2 => WaylandKeyState::Repeat,
        _ => {
            eprintln!("Unknown wl_keyboard::key_state");
            return;
        }
    };
    let display: &mut WaylandPayload = &mut *(data as *mut _);
    // release event must be handled here to stop key repeat, otherwise repeat events could be
    // pushed into EVENTS before the release event is handled by the `event_handler`
    if state == WaylandKeyState::Released {
        display.keyboard_context.key_up(key);
    }
    EVENTS.push(WaylandEvent::KeyboardKey { key, state });
}
unsafe extern "C" fn keyboard_handle_modifiers(
    data: *mut ::core::ffi::c_void,
    _wl_keyboard: *mut wl_keyboard,
    _serial: u32,
    mods_depressed: u32,
    mods_latched: u32,
    mods_locked: u32,
    group: u32,
) {
    let display: &mut WaylandPayload = &mut *(data as *mut _);
    (display.xkb.xkb_state_update_mask)(
        display.xkb_state,
        mods_depressed,
        mods_latched,
        mods_locked,
        0,
        0,
        group,
    );
}
unsafe extern "C" fn keyboard_handle_repeat_info(
    data: *mut ::core::ffi::c_void,
    _wl_keyboard: *mut wl_keyboard,
    rate: i32,
    delay: i32,
) {
    let display: &mut WaylandPayload = &mut *(data as *mut _);
    display.keyboard_context.repeat_info = if rate == 0 {
        RepeatInfo::NoRepeat
    } else {
        RepeatInfo::Repeat {
            delay: Duration::from_millis(delay as u64),
            gap: Duration::from_micros(1_000_000 / rate as u64),
        }
    };
}

static mut POINTER_LISTENER: wl_pointer_listener = wl_pointer_listener {
    enter: Some(pointer_handle_enter),
    leave: Some(pointer_handle_leave),
    motion: Some(pointer_handle_motion),
    button: Some(pointer_handle_button),
    axis: Some(pointer_handle_axis),
    frame: Some(pointer_handle_frame),
    axis_source: Some(pointer_handle_axis_source),
    axis_stop: Some(pointer_handle_axis_stop),
    axis_discrete: Some(pointer_handle_axis_discrete),
    axis_value120: Some(pointer_handle_axis_value120),
    axis_relative_direction: Some(pointer_handle_axis_relative_direction),
};

unsafe extern "C" fn pointer_handle_enter(
    _data: *mut ::core::ffi::c_void,
    _wl_pointer: *mut wl_pointer,
    _serial: u32,
    _surface: *mut wl_surface,
    _surface_x: i32,
    _surface_y: i32,
) {
}
unsafe extern "C" fn pointer_handle_leave(
    _data: *mut ::core::ffi::c_void,
    _wl_pointer: *mut wl_pointer,
    _serial: u32,
    _surface: *mut wl_surface,
) {
}
unsafe extern "C" fn pointer_handle_motion(
    _data: *mut ::core::ffi::c_void,
    _wl_pointer: *mut wl_pointer,
    _time: u32,
    surface_x: i32,
    surface_y: i32,
) {
    // From wl_fixed_to_double(), it simply divides by 256
    let (x, y) = (wl_fixed_to_double(surface_x), wl_fixed_to_double(surface_y));
    EVENTS.push(WaylandEvent::PointerMotion(x, y));
}
unsafe extern "C" fn pointer_handle_button(
    _data: *mut ::core::ffi::c_void,
    _wl_pointer: *mut wl_pointer,
    _serial: u32,
    _time: u32,
    button: u32,
    state: u32,
) {
    // The code is defined in the kernel's linux/input-event-codes.h header file, e.g. BTN_LEFT
    let button = match button {
        272 => MouseButton::Left,
        273 => MouseButton::Right,
        274 => MouseButton::Middle,
        _ => MouseButton::Unknown,
    };
    EVENTS.push(WaylandEvent::PointerButton(button, state == 1));
}
unsafe extern "C" fn pointer_handle_axis(
    _data: *mut ::core::ffi::c_void,
    _wl_pointer: *mut wl_pointer,
    _time: u32,
    axis: u32,
    value: i32,
) {
    let mut value = wl_fixed_to_double(value);
    // Normalize the value to {-1, 0, 1}
    value /= value.abs();

    // https://wayland-book.com/seat/pointer.html
    if axis == 0 {
        // Vertical scroll
        // Wayland defines the direction differently to miniquad so lets flip it
        value = -value;
        EVENTS.push(WaylandEvent::PointerAxis(0.0, value));
    } else if axis == 1 {
        // Horizontal scroll
        EVENTS.push(WaylandEvent::PointerAxis(value, 0.0));
    }
}
unsafe extern "C" fn pointer_handle_frame(
    _data: *mut ::core::ffi::c_void,
    _wl_pointer: *mut wl_pointer,
) {
}
unsafe extern "C" fn pointer_handle_axis_source(
    _data: *mut ::core::ffi::c_void,
    _wl_pointer: *mut wl_pointer,
    _axis_source: u32,
) {
}
unsafe extern "C" fn pointer_handle_axis_stop(
    _data: *mut ::core::ffi::c_void,
    _wl_pointer: *mut wl_pointer,
    _time: u32,
    _axis: u32,
) {
}
unsafe extern "C" fn pointer_handle_axis_discrete(
    _data: *mut ::core::ffi::c_void,
    _wl_pointer: *mut wl_pointer,
    _axis: u32,
    _discrete: i32,
) {
}
unsafe extern "C" fn pointer_handle_axis_value120(
    _data: *mut ::core::ffi::c_void,
    _wl_pointer: *mut wl_pointer,
    _axis: u32,
    _value120: i32,
) {
}
unsafe extern "C" fn pointer_handle_axis_relative_direction(
    _data: *mut ::core::ffi::c_void,
    _wl_pointer: *mut wl_pointer,
    _axis: u32,
    _direction: u32,
) {
}

extern "C" fn seat_handle_name(
    _data: *mut std::ffi::c_void,
    _seat: *mut wl_seat,
    _name: *const ::core::ffi::c_char,
) {
}

unsafe extern "C" fn registry_add_object(
    data: *mut std::ffi::c_void,
    registry: *mut wl_registry,
    name: u32,
    interface: *const ::core::ffi::c_char,
    version: u32,
) {
    let display: &mut WaylandPayload = &mut *(data as *mut _);

    let interface = std::ffi::CStr::from_ptr(interface).to_str().unwrap();
    match interface {
        "wl_compositor" => {
            display.compositor = display.client.wl_registry_bind(
                registry,
                name,
                display.client.wl_compositor_interface,
                1,
            ) as _;
            assert!(!display.compositor.is_null());
        }
        "wl_subcompositor" => {
            display.subcompositor = display.client.wl_registry_bind(
                registry,
                name,
                display.client.wl_subcompositor_interface,
                1,
            ) as _;
            assert!(!display.subcompositor.is_null());
        }
        "xdg_wm_base" => {
            display.xdg_wm_base = display.client.wl_registry_bind(
                registry,
                name,
                &extensions::xdg_shell::xdg_wm_base_interface,
                1,
            ) as _;
            assert!(!display.xdg_wm_base.is_null());
        }
        "zxdg_decoration_manager" | "zxdg_decoration_manager_v1" => {
            display.decoration_manager = display.client.wl_registry_bind(
                registry,
                name,
                &extensions::xdg_decoration::zxdg_decoration_manager_v1_interface,
                1,
            ) as _;
        }
        "wp_viewporter" => {
            display.viewporter = display.client.wl_registry_bind(
                registry,
                name,
                &extensions::viewporter::wp_viewporter_interface,
                1,
            ) as _;
        }
        "wl_shm" => {
            display.shm =
                display
                    .client
                    .wl_registry_bind(registry, name, display.client.wl_shm_interface, 1)
                    as _;
        }
        "wl_seat" => {
            let seat_version = 4.min(version);
            display.seat = display.client.wl_registry_bind(
                registry,
                name,
                display.client.wl_seat_interface,
                seat_version,
            ) as _;
            assert!(!display.seat.is_null());
            (display.client.wl_proxy_add_listener)(
                display.seat as _,
                &SEAT_LISTENER as *const _ as _,
                data,
            );
        }
        "wl_data_device_manager" => {
            display.data_device_manager = display.client.wl_registry_bind(
                registry,
                name,
                display.client.wl_data_device_manager_interface,
                3,
            ) as _;
            assert!(!display.data_device_manager.is_null());
        }

        _ => {}
    }
}

unsafe extern "C" fn registry_remove_object(
    _data: *mut std::ffi::c_void,
    _registry: *mut wl_registry,
    _name: u32,
) {
}

unsafe extern "C" fn xdg_surface_handle_configure(
    data: *mut std::ffi::c_void,
    xdg_surface: *mut extensions::xdg_shell::xdg_surface,
    serial: u32,
) {
    assert!(!data.is_null());
    let payload: &mut WaylandPayload = &mut *(data as *mut _);

    wl_request!(
        payload.client,
        xdg_surface,
        extensions::xdg_shell::xdg_surface::ack_configure,
        serial
    );
    wl_request!(payload.client, payload.surface, WL_SURFACE_COMMIT)
}

unsafe extern "C" fn xdg_toplevel_handle_close(
    data: *mut std::ffi::c_void,
    _xdg_toplevel: *mut extensions::xdg_shell::xdg_toplevel,
) {
    assert!(!data.is_null());
    let payload: &mut WaylandPayload = &mut *(data as *mut _);
    payload.closed = true;
}

unsafe extern "C" fn xdg_toplevel_handle_configure(
    data: *mut std::ffi::c_void,
    _toplevel: *mut extensions::xdg_shell::xdg_toplevel,
    width: i32,
    height: i32,
    _states: *mut wl_array,
) {
    assert!(!data.is_null());
    let payload: &mut WaylandPayload = &mut *(data as *mut _);
    let mut d = crate::native_display().lock().unwrap();

    if width != 0 && height != 0 {
        let (egl_w, egl_h) = if payload.decorations.is_some() {
            // Otherwise window will resize iteself on sway
            // I have no idea why
            (
                width - decorations::Decorations::WIDTH * 2,
                height - decorations::Decorations::BAR_HEIGHT - decorations::Decorations::WIDTH,
            )
        } else {
            (width, height)
        };
        (payload.egl.wl_egl_window_resize)(payload.egl_window, egl_w, egl_h, 0, 0);

        d.screen_width = width;
        d.screen_height = height;
        drop(d);

        if let Some(ref decorations) = payload.decorations {
            decorations.resize(&mut payload.client, width, height);
        }

        if let Some(ref mut event_handler) = payload.event_handler {
            event_handler.resize_event(width as _, height as _);
        }
    }
}

unsafe extern "C" fn xdg_wm_base_handle_ping(
    data: *mut std::ffi::c_void,
    toplevel: *mut extensions::xdg_shell::xdg_wm_base,
    serial: u32,
) {
    assert!(!data.is_null());
    let payload: &mut WaylandPayload = &mut *(data as *mut _);

    wl_request!(
        payload.client,
        toplevel,
        extensions::xdg_shell::xdg_wm_base::pong,
        serial
    );
}

static mut DATA_OFFER_LISTENER: wl_data_offer_listener = wl_data_offer_listener {
    offer: Some(data_offer_handle_offer),
    source_actions: Some(drag_n_drop::data_offer_handle_source_actions),
    action: Some(data_offer_handle_action),
};

unsafe extern "C" fn data_offer_handle_offer(
    _data: *mut ::core::ffi::c_void,
    _data_offer: *mut wl_data_offer,
    _mime_type: *const ::core::ffi::c_char,
) {
}

unsafe extern "C" fn data_offer_handle_action(
    _data: *mut ::core::ffi::c_void,
    _data_offer: *mut wl_data_offer,
    _action: ::core::ffi::c_uint,
) {
}

unsafe extern "C" fn data_device_handle_data_offer(
    data: *mut ::core::ffi::c_void,
    data_device: *mut wl_data_device,
    data_offer: *mut wl_data_offer,
) {
    let display: &mut WaylandPayload = &mut *(data as *mut _);
    assert_eq!(data_device, display.data_device);
    (display.client.wl_proxy_add_listener)(
        data_offer as _,
        &DATA_OFFER_LISTENER as *const _ as _,
        data,
    );
}

pub fn run<F>(conf: &crate::conf::Conf, f: &mut Option<F>) -> Option<()>
where
    F: 'static + FnOnce() -> Box<dyn EventHandler>,
{
    unsafe {
        let client = LibWaylandClient::try_load().ok()?;
        let egl = LibWaylandEgl::try_load().ok()?;
        let xkb = LibXkbCommon::try_load().ok()?;

        let wdisplay = (client.wl_display_connect)(std::ptr::null_mut());
        if wdisplay.is_null() {
            eprintln!("Failed to connect to Wayland display.");
            return None;
        }

        let registry: *mut wl_registry = wl_request_constructor!(
            client,
            wdisplay,
            WL_DISPLAY_GET_REGISTRY,
            client.wl_registry_interface
        );
        assert!(!registry.is_null());

        let registry_listener = wl_registry_listener {
            global: Some(registry_add_object),
            global_remove: Some(registry_remove_object),
        };

        let xkb_context = (xkb.xkb_context_new)(0);

        let mut display = WaylandPayload {
            client,
            display: wdisplay,
            registry,
            egl,
            xkb,
            compositor: std::ptr::null_mut(),
            subcompositor: std::ptr::null_mut(),
            xdg_toplevel: std::ptr::null_mut(),
            xdg_wm_base: std::ptr::null_mut(),
            xdg_surface: std::ptr::null_mut(),
            surface: std::ptr::null_mut(),
            decoration_manager: std::ptr::null_mut(),
            viewporter: std::ptr::null_mut(),
            shm: std::ptr::null_mut(),
            seat: std::ptr::null_mut(),
            data_device_manager: std::ptr::null_mut(),
            data_device: std::ptr::null_mut(),
            xkb_context,
            keymap: std::ptr::null_mut(),
            xkb_state: std::ptr::null_mut(),
            egl_window: std::ptr::null_mut(),
            pointer: std::ptr::null_mut(),
            keyboard: std::ptr::null_mut(),
            focused_window: std::ptr::null_mut(),
            decorations: None,
            keyboard_context: KeyboardContext::new(),
            drag_n_drop: Default::default(),
            update_requested: true,
            event_handler: None,
            closed: false,
        };

        let (tx, rx) = std::sync::mpsc::channel();
        let clipboard = Box::new(clipboard::WaylandClipboard::new(&mut display as *mut _));
        crate::set_display(NativeDisplayData {
            ..NativeDisplayData::new(conf.window_width, conf.window_height, tx, clipboard)
        });

        (display.client.wl_proxy_add_listener)(
            display.registry as _,
            &registry_listener as *const _ as _,
            &mut display as *mut _ as _,
        );
        (display.client.wl_display_dispatch)(display.display);

        //assert!(!display.keymap.is_null());
        //assert!(!display.xkb_state.is_null());

        let xdg_wm_base_listener = extensions::xdg_shell::xdg_wm_base_listener {
            ping: Some(xdg_wm_base_handle_ping),
        };

        (display.client.wl_proxy_add_listener)(
            display.xdg_wm_base as _,
            &xdg_wm_base_listener as *const _ as _,
            &mut display as *mut _ as _,
        );

        if display.decoration_manager.is_null() && conf.platform.wayland_use_fallback_decorations {
            eprintln!("Decoration manager not found, will draw fallback decorations");
        }

        let mut libegl = egl::LibEgl::try_load().ok()?;
        let (context, config, egl_display) = egl::create_egl_context(
            &mut libegl,
            wdisplay as *mut _,
            conf.platform.framebuffer_alpha,
            conf.sample_count,
        )
        .unwrap();

        display.surface = wl_request_constructor!(
            display.client,
            display.compositor,
            WL_COMPOSITOR_CREATE_SURFACE,
            display.client.wl_surface_interface
        );
        assert!(!display.surface.is_null());

        display.xdg_surface = wl_request_constructor!(
            display.client,
            display.xdg_wm_base,
            extensions::xdg_shell::xdg_wm_base::get_xdg_surface,
            &extensions::xdg_shell::xdg_surface_interface,
            display.surface
        );
        assert!(!display.xdg_surface.is_null());

        let xdg_surface_listener = extensions::xdg_shell::xdg_surface_listener {
            configure: Some(xdg_surface_handle_configure),
        };

        (display.client.wl_proxy_add_listener)(
            display.xdg_surface as _,
            &xdg_surface_listener as *const _ as _,
            &mut display as *mut _ as _,
        );

        display.xdg_toplevel = wl_request_constructor!(
            display.client,
            display.xdg_surface,
            extensions::xdg_shell::xdg_surface::get_toplevel,
            &extensions::xdg_shell::xdg_toplevel_interface
        );
        assert!(!display.xdg_toplevel.is_null());

        let xdg_toplevel_listener = extensions::xdg_shell::xdg_toplevel_listener {
            configure: Some(xdg_toplevel_handle_configure),
            close: Some(xdg_toplevel_handle_close),
        };

        (display.client.wl_proxy_add_listener)(
            display.xdg_toplevel as _,
            &xdg_toplevel_listener as *const _ as _,
            &mut display as *mut _ as _,
        );

        let title = std::ffi::CString::new(conf.window_title.as_str()).unwrap();

        wl_request!(
            display.client,
            display.xdg_toplevel,
            extensions::xdg_shell::xdg_toplevel::set_title,
            title.as_ptr()
        );

        let wm_class = std::ffi::CString::new(conf.platform.linux_wm_class).unwrap();

        wl_request!(
            display.client,
            display.xdg_toplevel,
            extensions::xdg_shell::xdg_toplevel::set_app_id,
            wm_class.as_ptr()
        );

        wl_request!(display.client, display.surface, WL_SURFACE_COMMIT);
        (display.client.wl_display_dispatch)(display.display);

        display.egl_window = (display.egl.wl_egl_window_create)(
            display.surface as _,
            conf.window_width as _,
            conf.window_height as _,
        );

        let egl_surface = (libegl.eglCreateWindowSurface)(
            egl_display,
            config,
            display.egl_window as _,
            std::ptr::null_mut(),
        );

        if egl_surface.is_null() {
            // == EGL_NO_SURFACE
            panic!("surface creation failed");
        }
        if (libegl.eglMakeCurrent)(egl_display, egl_surface, egl_surface, context) == 0 {
            panic!("eglMakeCurrent failed");
        }

        if (libegl.eglSwapInterval)(egl_display, conf.platform.swap_interval.unwrap_or(1)) == 0 {
            eprintln!("eglSwapInterval failed");
        }

        // For some reason, setting fullscreen before egl_window is created leads
        // to segfault because wl_egl_window_create returns NULL.
        if conf.fullscreen {
            wl_request!(
                display.client,
                display.xdg_toplevel,
                extensions::xdg_shell::xdg_toplevel::set_fullscreen,
                std::ptr::null_mut::<*mut wl_output>()
            )
        }

        crate::native::gl::load_gl_funcs(|proc| {
            let name = std::ffi::CString::new(proc).unwrap();
            (libegl.eglGetProcAddress)(name.as_ptr() as _)
        });

        if !display.decoration_manager.is_null() {
            let server_decoration: *mut extensions::xdg_decoration::zxdg_toplevel_decoration_v1 = wl_request_constructor!(
                display.client,
                display.decoration_manager,
                extensions::xdg_decoration::zxdg_decoration_manager_v1::get_toplevel_decoration,
                &extensions::xdg_decoration::zxdg_toplevel_decoration_v1_interface,
                display.xdg_toplevel
            );
            assert!(!server_decoration.is_null());

            wl_request!(
                display.client,
                server_decoration,
                extensions::xdg_decoration::zxdg_toplevel_decoration_v1::set_mode,
                extensions::xdg_decoration::ZXDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE
            );
        } else if conf.platform.wayland_use_fallback_decorations {
            display.decorations = Some(decorations::Decorations::new(
                &mut display,
                conf.window_width,
                conf.window_height,
            ));
        }

        let event_handler = (f.take().unwrap())();
        display.event_handler = Some(event_handler);

        let mut keymods = KeyMods {
            shift: false,
            ctrl: false,
            alt: false,
            logo: false,
        };
        let (mut last_mouse_x, mut last_mouse_y) = (0.0, 0.0);

        display.data_device = wl_request_constructor!(
            display.client,
            display.data_device_manager,
            WL_DATA_DEVICE_MANAGER_GET_DATA_DEVICE,
            display.client.wl_data_device_interface,
            display.seat
        ) as _;
        assert!(!display.data_device.is_null());

        let data_device_listener = wl_data_device_listener {
            data_offer: Some(data_device_handle_data_offer),
            enter: Some(drag_n_drop::data_device_handle_enter),
            leave: Some(drag_n_drop::data_device_handle_leave),
            motion: Some(drag_n_drop::data_device_handle_motion),
            drop: Some(drag_n_drop::data_device_handle_drop),
            selection: Some(clipboard::data_device_handle_selection),
        };
        (display.client.wl_proxy_add_listener)(
            display.data_device as _,
            &data_device_listener as *const _ as _,
            &mut display as *mut _ as _,
        );

        while !(display.closed || crate::native_display().try_lock().unwrap().quit_ordered) {
            while let Ok(request) = rx.try_recv() {
                match request {
                    Request::SetFullscreen(full) => {
                        if full {
                            wl_request!(
                                display.client,
                                display.xdg_toplevel,
                                extensions::xdg_shell::xdg_toplevel::set_fullscreen,
                                std::ptr::null_mut::<*mut wl_output>()
                            );
                        } else {
                            wl_request!(
                                display.client,
                                display.xdg_toplevel,
                                extensions::xdg_shell::xdg_toplevel::unset_fullscreen
                            );
                        }
                    }
                    Request::ScheduleUpdate => display.update_requested = true,
                    // TODO: implement the other events
                    _ => (),
                }
            }

            display.block_on_new_event();

            if let Some(ref mut event_handler) = display.event_handler {
                for event in EVENTS.drain(..) {
                    match event {
                        WaylandEvent::KeyboardLeave => {
                            keymods.shift = false;
                            keymods.ctrl = false;
                            keymods.logo = false;
                            keymods.alt = false;
                        }
                        WaylandEvent::KeyboardKey { key, state } => {
                            // https://wayland-book.com/seat/keyboard.html
                            // To translate this to an XKB scancode, you must add 8 to the evdev scancode.
                            let keysym =
                                (display.xkb.xkb_state_key_get_one_sym)(display.xkb_state, key + 8);
                            let keycode = keycodes::translate(keysym);
                            let should_repeat =
                                (display.xkb.xkb_keymap_key_repeats)(display.keymap, key + 8) == 1;

                            match keycode {
                                KeyCode::LeftShift | KeyCode::RightShift => {
                                    keymods.shift = state.into()
                                }
                                KeyCode::LeftControl | KeyCode::RightControl => {
                                    keymods.ctrl = state.into()
                                }
                                KeyCode::LeftAlt | KeyCode::RightAlt => keymods.alt = state.into(),
                                KeyCode::LeftSuper | KeyCode::RightSuper => {
                                    keymods.logo = state.into()
                                }
                                _ => {}
                            }

                            if state.into() {
                                let repeat = matches!(state, WaylandKeyState::Repeat);
                                if !repeat && should_repeat {
                                    display.keyboard_context.key_down(key);
                                }

                                event_handler.key_down_event(keycode, keymods, repeat);

                                let chr = keycodes::keysym_to_unicode(&mut display.xkb, keysym);
                                if chr > 0 {
                                    if let Some(chr) = char::from_u32(chr as u32) {
                                        event_handler.char_event(chr, keymods, repeat);
                                    }
                                }
                            } else {
                                event_handler.key_up_event(keycode, keymods);
                            }
                        }
                        WaylandEvent::PointerMotion(x, y) => {
                            event_handler.mouse_motion_event(x, y);
                            (last_mouse_x, last_mouse_y) = (x, y);
                        }
                        WaylandEvent::PointerButton(button, state) => {
                            if state {
                                event_handler.mouse_button_down_event(
                                    button,
                                    last_mouse_x,
                                    last_mouse_y,
                                );
                            } else {
                                event_handler.mouse_button_up_event(
                                    button,
                                    last_mouse_x,
                                    last_mouse_y,
                                );
                            }
                        }
                        WaylandEvent::PointerAxis(x, y) => event_handler.mouse_wheel_event(x, y),
                        WaylandEvent::FilesDropped(filenames) => {
                            let mut d = crate::native_display().try_lock().unwrap();
                            d.dropped_files = Default::default();
                            for filename in filenames.lines() {
                                let path = std::path::PathBuf::from(filename);
                                if let Ok(bytes) = std::fs::read(&path) {
                                    d.dropped_files.paths.push(path);
                                    d.dropped_files.bytes.push(bytes);
                                }
                            }
                            // drop d since files_dropped_event is likely to need access to it
                            drop(d);
                            event_handler.files_dropped_event();
                        }
                    }
                }

                {
                    let d = crate::native_display().try_lock().unwrap();
                    if d.quit_requested && !d.quit_ordered {
                        drop(d);
                        event_handler.quit_requested_event();
                        let mut d = crate::native_display().try_lock().unwrap();
                        if d.quit_requested {
                            d.quit_ordered = true
                        }
                    }
                }

                if !conf.platform.blocking_event_loop || display.update_requested {
                    display.update_requested = false;
                    event_handler.update();
                    event_handler.draw();
                    (libegl.eglSwapBuffers)(egl_display, egl_surface);
                }
            }
        }
    }

    Some(())
}
