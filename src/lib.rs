use core::fmt;
use std::{
    cmp::Ordering,
    ffi::{c_void, CStr},
    fs::{self, File},
    io::{BufWriter, Cursor},
    mem,
    os::raw::{c_char, c_int},
    path::{Path, PathBuf},
    ptr,
    sync::{Arc, Mutex, RwLock, Weak},
};

mod ffi;
mod ffi_types;

use ffi::{
    blog, gs_draw_sprite, gs_effect_get_param_by_name, gs_effect_get_technique,
    gs_effect_set_texture, gs_effect_t, gs_technique_begin, gs_technique_begin_pass,
    gs_technique_end, gs_technique_end_pass, gs_texture_create, gs_texture_destroy,
    gs_texture_set_image, gs_texture_t, obs_data_get_int, obs_data_get_string,
    obs_data_set_default_int, obs_data_t, obs_enter_graphics, obs_get_base_effect, obs_hotkey_id,
    obs_hotkey_register_source, obs_hotkey_t, obs_leave_graphics, obs_module_t, obs_mouse_event,
    obs_properties_add_button, obs_properties_add_int, obs_properties_add_path,
    obs_properties_create, obs_properties_t, obs_property_t, obs_register_source_s,
    obs_source_info, obs_source_t, GS_DYNAMIC, GS_RGBA, LOG_WARNING,
    OBS_EFFECT_PREMULTIPLIED_ALPHA, OBS_ICON_TYPE_GAME_CAPTURE, OBS_PATH_FILE,
    OBS_SOURCE_CONTROLLABLE_MEDIA, OBS_SOURCE_CUSTOM_DRAW, OBS_SOURCE_INTERACTION,
    OBS_SOURCE_TYPE_INPUT, OBS_SOURCE_VIDEO,
};
use ffi_types::{
    obs_media_state, LOG_DEBUG, LOG_ERROR, LOG_INFO, OBS_MEDIA_STATE_ENDED, OBS_MEDIA_STATE_PAUSED,
    OBS_MEDIA_STATE_PLAYING, OBS_MEDIA_STATE_STOPPED,
};
#[cfg(feature = "auto-splitting")]
use livesplit_core::auto_splitting;
use livesplit_core::{
    layout::{self, LayoutSettings, LayoutState},
    rendering::software::Renderer,
    run::{
        parser::{composite, TimerKind},
        saver::livesplit::{save_timer, IoWrite},
    },
    Layout, Run, Segment, SharedTimer, Timer, TimerPhase,
};
use log::{Level, LevelFilter, Log, Metadata, Record};

macro_rules! cstr {
    ($f:literal) => {
        concat!($f, '\0').as_ptr().cast()
    };
}

static mut OBS_MODULE_POINTER: *mut obs_module_t = ptr::null_mut();

#[no_mangle]
pub extern "C" fn obs_module_set_pointer(module: *mut obs_module_t) {
    unsafe {
        OBS_MODULE_POINTER = module;
    }
}

#[no_mangle]
pub extern "C" fn obs_module_ver() -> u32 {
    (26 << 24) | (1 << 16) | 1
}

struct UnsafeMultiThread<T>(T);

unsafe impl<T> Sync for UnsafeMultiThread<T> {}
unsafe impl<T> Send for UnsafeMultiThread<T> {}

static TIMERS: Mutex<Vec<(PathBuf, Weak<RwLock<Timer>>)>> = Mutex::new(Vec::new());

struct State {
    timer: SharedTimer,
    splits_path: PathBuf,
    can_save_splits: bool,
    #[cfg(feature = "auto-splitting")]
    auto_splitter: auto_splitting::Runtime,
    layout: Layout,
    state: LayoutState,
    renderer: Renderer,
    texture: *mut gs_texture_t,
    width: u32,
    height: u32,
}

struct Settings {
    run: Run,
    splits_path: PathBuf,
    can_save_splits: bool,
    layout: Layout,
    #[cfg(feature = "auto-splitting")]
    auto_splitter_path: String,
    width: u32,
    height: u32,
}

fn parse_run(path: &Path) -> Option<(Run, bool)> {
    let file_data = fs::read(path).ok()?;
    let run = composite::parse(&file_data, Some(Path::new(path))).ok()?;
    if run.run.is_empty() {
        return None;
    }
    Some((run.run, run.kind == TimerKind::LiveSplit))
}

fn log(level: Level, target: &str, args: &fmt::Arguments<'_>) {
    let str = format!("[LiveSplit One][{target}] {args}\0");
    let level = match level {
        Level::Error => LOG_ERROR,
        Level::Warn => LOG_WARNING,
        Level::Info => LOG_INFO,
        Level::Debug | Level::Trace => LOG_DEBUG,
    };
    unsafe {
        blog(level as _, b"%s\0".as_ptr().cast(), str.as_ptr());
    }
}

fn parse_layout(path: &CStr) -> Option<Layout> {
    let path = path.to_str().ok()?;
    if path.is_empty() {
        return None;
    }
    let file_data = fs::read_to_string(path).ok()?;

    if let Ok(settings) = LayoutSettings::from_json(Cursor::new(file_data.as_bytes())) {
        return Some(Layout::from_settings(settings));
    }

    layout::parser::parse(&file_data).ok()
}

unsafe fn parse_settings(settings: *mut obs_data_t) -> Settings {
    let splits_path = CStr::from_ptr(obs_data_get_string(settings, SETTINGS_SPLITS_PATH).cast());
    let splits_path = PathBuf::from(splits_path.to_string_lossy().into_owned());
    let (run, can_save_splits) = parse_run(&splits_path).unwrap_or_else(default_run);

    let layout_path = CStr::from_ptr(obs_data_get_string(settings, SETTINGS_LAYOUT_PATH).cast());
    let layout = parse_layout(layout_path).unwrap_or_else(Layout::default_layout);

    #[cfg(feature = "auto-splitting")]
    let auto_splitter_path = CStr::from_ptr(obs_data_get_string(
        settings,
        SETTINGS_AUTO_SPLITTER_PATH.cast(),
    ))
    .to_str()
    .unwrap_or_default()
    .to_owned();

    let width = obs_data_get_int(settings, SETTINGS_WIDTH) as u32;
    let height = obs_data_get_int(settings, SETTINGS_HEIGHT) as u32;

    Settings {
        run,
        splits_path,
        can_save_splits,
        layout,
        #[cfg(feature = "auto-splitting")]
        auto_splitter_path,
        width,
        height,
    }
}

impl State {
    unsafe fn new(
        Settings {
            run,
            splits_path,
            can_save_splits,
            layout,
            #[cfg(feature = "auto-splitting")]
            auto_splitter_path,
            width,
            height,
        }: Settings,
    ) -> Self {
        log::info!("Loading settings.");

        let timer = {
            let mut timers = TIMERS.lock().unwrap();
            timers.retain(|(_, timer)| timer.strong_count() > 0);
            if let Some(timer) = timers.iter().find_map(|(path, timer)| {
                if path == &splits_path {
                    timer.upgrade()
                } else {
                    None
                }
            }) {
                log::debug!("Found timer to reuse.");
                timer
            } else {
                log::debug!("Storing timer for reuse.");
                let timer = Timer::new(run).unwrap().into_shared();
                timers.push((splits_path.clone(), Arc::downgrade(&timer)));
                timer
            }
        };

        #[cfg(feature = "auto-splitting")]
        let auto_splitter = auto_splitting::Runtime::new(timer.clone());
        #[cfg(feature = "auto-splitting")]
        if !auto_splitter_path.is_empty() {
            auto_splitter
                .load_script_blocking(PathBuf::from(auto_splitter_path))
                .ok();
        }

        let state = LayoutState::default();
        let renderer = Renderer::new();

        obs_enter_graphics();
        let texture = gs_texture_create(width, height, GS_RGBA, 1, ptr::null_mut(), GS_DYNAMIC);
        obs_leave_graphics();

        Self {
            timer,
            splits_path,
            can_save_splits,
            layout,
            #[cfg(feature = "auto-splitting")]
            auto_splitter,
            state,
            renderer,
            texture,
            width,
            height,
        }
    }

    unsafe fn update(&mut self) {
        self.layout
            .update_state(&mut self.state, &self.timer.read().unwrap().snapshot());

        self.renderer.render(&self.state, [self.width, self.height]);
        gs_texture_set_image(
            self.texture,
            self.renderer.image_data().as_ptr(),
            self.width * 4,
            false,
        );
    }
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    cstr!("LiveSplit One")
}

unsafe extern "C" fn split(
    data: *mut c_void,
    _: obs_hotkey_id,
    _: *mut obs_hotkey_t,
    pressed: bool,
) {
    if pressed {
        let state: &mut State = &mut *data.cast();
        state.timer.write().unwrap().split_or_start();
    }
}

unsafe extern "C" fn reset(
    data: *mut c_void,
    _: obs_hotkey_id,
    _: *mut obs_hotkey_t,
    pressed: bool,
) {
    if pressed {
        let state: &mut State = &mut *data.cast();
        state.timer.write().unwrap().reset(true);
    }
}

unsafe extern "C" fn undo(
    data: *mut c_void,
    _: obs_hotkey_id,
    _: *mut obs_hotkey_t,
    pressed: bool,
) {
    if pressed {
        let state: &mut State = &mut *data.cast();
        state.timer.write().unwrap().undo_split();
    }
}

unsafe extern "C" fn skip(
    data: *mut c_void,
    _: obs_hotkey_id,
    _: *mut obs_hotkey_t,
    pressed: bool,
) {
    if pressed {
        let state: &mut State = &mut *data.cast();
        state.timer.write().unwrap().skip_split();
    }
}

unsafe extern "C" fn pause(
    data: *mut c_void,
    _: obs_hotkey_id,
    _: *mut obs_hotkey_t,
    pressed: bool,
) {
    if pressed {
        let state: &mut State = &mut *data.cast();
        state.timer.write().unwrap().toggle_pause_or_start();
    }
}

unsafe extern "C" fn undo_all_pauses(
    data: *mut c_void,
    _: obs_hotkey_id,
    _: *mut obs_hotkey_t,
    pressed: bool,
) {
    if pressed {
        let state: &mut State = &mut *data.cast();
        state.timer.write().unwrap().undo_all_pauses();
    }
}

unsafe extern "C" fn previous_comparison(
    data: *mut c_void,
    _: obs_hotkey_id,
    _: *mut obs_hotkey_t,
    pressed: bool,
) {
    if pressed {
        let state: &mut State = &mut *data.cast();
        state.timer.write().unwrap().switch_to_previous_comparison();
    }
}

unsafe extern "C" fn next_comparison(
    data: *mut c_void,
    _: obs_hotkey_id,
    _: *mut obs_hotkey_t,
    pressed: bool,
) {
    if pressed {
        let state: &mut State = &mut *data.cast();
        state.timer.write().unwrap().switch_to_next_comparison();
    }
}

unsafe extern "C" fn toggle_timing_method(
    data: *mut c_void,
    _: obs_hotkey_id,
    _: *mut obs_hotkey_t,
    pressed: bool,
) {
    if pressed {
        let state: &mut State = &mut *data.cast();
        state.timer.write().unwrap().toggle_timing_method();
    }
}

unsafe extern "C" fn create(settings: *mut obs_data_t, source: *mut obs_source_t) -> *mut c_void {
    let data = Box::into_raw(Box::new(State::new(parse_settings(settings)))).cast();

    obs_hotkey_register_source(
        source,
        cstr!("hotkey_split"),
        cstr!("Split"),
        Some(split),
        data,
    );

    obs_hotkey_register_source(
        source,
        cstr!("hotkey_reset"),
        cstr!("Reset"),
        Some(reset),
        data,
    );

    obs_hotkey_register_source(
        source,
        cstr!("hotkey_undo"),
        cstr!("Undo Split"),
        Some(undo),
        data,
    );

    obs_hotkey_register_source(
        source,
        cstr!("hotkey_skip"),
        cstr!("Skip Split"),
        Some(skip),
        data,
    );

    obs_hotkey_register_source(
        source,
        cstr!("hotkey_pause"),
        cstr!("Pause"),
        Some(pause),
        data,
    );

    obs_hotkey_register_source(
        source,
        cstr!("hotkey_undo_all_pauses"),
        cstr!("Undo All Pauses"),
        Some(undo_all_pauses),
        data,
    );

    obs_hotkey_register_source(
        source,
        cstr!("hotkey_previous_comparison"),
        cstr!("Previous Comparison"),
        Some(previous_comparison),
        data,
    );

    obs_hotkey_register_source(
        source,
        cstr!("hotkey_next_comparison"),
        cstr!("Next Comparison"),
        Some(next_comparison),
        data,
    );

    obs_hotkey_register_source(
        source,
        cstr!("hotkey_toggle_timing_method"),
        cstr!("Toggle Timing Method"),
        Some(toggle_timing_method),
        data,
    );

    data
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let state: Box<State> = Box::from_raw(data.cast());
    obs_enter_graphics();
    gs_texture_destroy(state.texture);
    obs_leave_graphics();
}

unsafe extern "C" fn get_width(data: *mut c_void) -> u32 {
    let state: &mut State = &mut *data.cast();
    state.width
}

unsafe extern "C" fn get_height(data: *mut c_void) -> u32 {
    let state: &mut State = &mut *data.cast();
    state.height
}

unsafe extern "C" fn video_render(data: *mut c_void, _: *mut gs_effect_t) {
    let state: &mut State = &mut *data.cast();
    state.update();

    let effect = obs_get_base_effect(OBS_EFFECT_PREMULTIPLIED_ALPHA);
    let tech = gs_effect_get_technique(effect, cstr!("Draw"));

    gs_technique_begin(tech);
    gs_technique_begin_pass(tech, 0);

    gs_effect_set_texture(
        gs_effect_get_param_by_name(effect, cstr!("image")),
        state.texture,
    );
    gs_draw_sprite(state.texture, 0, 0, 0);

    gs_technique_end_pass(tech);
    gs_technique_end(tech);
}

unsafe extern "C" fn mouse_wheel(
    data: *mut c_void,
    _: *const obs_mouse_event,
    _: c_int,
    y_delta: c_int,
) {
    let state: &mut State = &mut *data.cast();
    match y_delta.cmp(&0) {
        Ordering::Less => state.layout.scroll_down(),
        Ordering::Equal => {}
        Ordering::Greater => state.layout.scroll_up(),
    }
}

unsafe extern "C" fn save_splits(
    _: *mut obs_properties_t,
    _: *mut obs_property_t,
    data: *mut c_void,
) -> bool {
    let state: &mut State = &mut *data.cast();
    if state.can_save_splits {
        let timer = state.timer.read().unwrap();
        if let Ok(file) = File::create(&state.splits_path) {
            let _ = save_timer(&timer, IoWrite(BufWriter::new(file)));
        }
    }
    false
}

unsafe extern "C" fn media_get_state(data: *mut c_void) -> obs_media_state {
    let state: &mut State = &mut *data.cast();
    let phase = state.timer.read().unwrap().current_phase();
    match phase {
        TimerPhase::NotRunning => OBS_MEDIA_STATE_STOPPED,
        TimerPhase::Running => OBS_MEDIA_STATE_PLAYING,
        TimerPhase::Ended => OBS_MEDIA_STATE_ENDED,
        TimerPhase::Paused => OBS_MEDIA_STATE_PAUSED,
    }
}

unsafe extern "C" fn media_play_pause(data: *mut c_void, pause: bool) {
    let state: &mut State = &mut *data.cast();
    let mut timer = state.timer.write().unwrap();
    match timer.current_phase() {
        TimerPhase::NotRunning => {
            if !pause {
                timer.start()
            }
        }
        TimerPhase::Running => {
            if pause {
                timer.pause()
            }
        }
        TimerPhase::Ended => {}
        TimerPhase::Paused => {
            if !pause {
                timer.resume()
            }
        }
    }
}

unsafe extern "C" fn media_restart(data: *mut c_void) {
    let state: &mut State = &mut *data.cast();
    let mut timer = state.timer.write().unwrap();
    timer.reset(true);
    timer.start();
}

unsafe extern "C" fn media_stop(data: *mut c_void) {
    let state: &mut State = &mut *data.cast();
    state.timer.write().unwrap().reset(true);
}

unsafe extern "C" fn media_next(data: *mut c_void) {
    let state: &mut State = &mut *data.cast();
    state.timer.write().unwrap().split();
}

unsafe extern "C" fn media_previous(data: *mut c_void) {
    let state: &mut State = &mut *data.cast();
    state.timer.write().unwrap().undo_split();
}

unsafe extern "C" fn media_get_time(data: *mut c_void) -> i64 {
    let state: &mut State = &mut *data.cast();
    let timer = state.timer.read().unwrap();
    let time = timer.snapshot().current_time()[timer.current_timing_method()].unwrap_or_default();
    let (secs, nanos) = time.to_seconds_and_subsec_nanoseconds();
    secs * 1000 + (nanos / 1_000_000) as i64
}

unsafe extern "C" fn media_get_duration(data: *mut c_void) -> i64 {
    let state: &mut State = &mut *data.cast();
    let timer = state.timer.read().unwrap();
    let time = timer
        .run()
        .segments()
        .last()
        .unwrap()
        .personal_best_split_time()[timer.current_timing_method()]
    .unwrap_or_default();
    let (secs, nanos) = time.to_seconds_and_subsec_nanoseconds();
    secs * 1000 + (nanos / 1_000_000) as i64
}

const SETTINGS_WIDTH: *const c_char = cstr!("width");
const SETTINGS_HEIGHT: *const c_char = cstr!("height");
const SETTINGS_SPLITS_PATH: *const c_char = cstr!("splits_path");
const SETTINGS_LAYOUT_PATH: *const c_char = cstr!("layout_path");
#[cfg(feature = "auto-splitting")]
const SETTINGS_AUTO_SPLITTER_PATH: *const c_char = cstr!("auto_splitter_path");
const SETTINGS_SAVE_SPLITS: *const c_char = cstr!("save_splits");

unsafe extern "C" fn get_properties(_: *mut c_void) -> *mut obs_properties_t {
    let props = obs_properties_create();
    obs_properties_add_int(props, SETTINGS_WIDTH, cstr!("Width"), 10, 8200, 10);
    obs_properties_add_int(props, SETTINGS_HEIGHT, cstr!("Height"), 10, 8200, 10);
    obs_properties_add_path(
        props,
        SETTINGS_SPLITS_PATH,
        cstr!("Splits"),
        OBS_PATH_FILE,
        cstr!("LiveSplit Splits (*.lss)"),
        ptr::null(),
    );
    obs_properties_add_path(
        props,
        SETTINGS_LAYOUT_PATH,
        cstr!("Layout"),
        OBS_PATH_FILE,
        cstr!("LiveSplit Layouts (*.lsl *.ls1l)"),
        ptr::null(),
    );
    #[cfg(feature = "auto-splitting")]
    obs_properties_add_path(
        props,
        SETTINGS_AUTO_SPLITTER_PATH,
        cstr!("Auto Splitter"),
        OBS_PATH_FILE,
        cstr!("LiveSplit One Auto Splitter (*.wasm)"),
        ptr::null(),
    );
    obs_properties_add_button(
        props,
        SETTINGS_SAVE_SPLITS,
        cstr!("Save Splits"),
        Some(save_splits),
    );
    props
}

unsafe extern "C" fn get_defaults(settings: *mut obs_data_t) {
    obs_data_set_default_int(settings, SETTINGS_WIDTH, 300);
    obs_data_set_default_int(settings, SETTINGS_HEIGHT, 500);
}

fn default_run() -> (Run, bool) {
    let mut run = Run::new();
    run.push_segment(Segment::new("Time"));
    (run, false)
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut obs_data_t) {
    log::info!("Reloading settings.");

    let state: &mut State = &mut *data.cast();
    let settings = parse_settings(settings);

    let timer = {
        let mut timers = TIMERS.lock().unwrap();
        timers.retain(|(_, timer)| timer.strong_count() > 0);
        if let Some(timer) = timers.iter().find_map(|(path, timer)| {
            if path == &settings.splits_path {
                timer.upgrade()
            } else {
                None
            }
        }) {
            log::debug!("Found timer to reuse.");
            timer
        } else {
            log::debug!("Storing timer for reuse.");
            let timer = Timer::new(settings.run).unwrap().into_shared();
            timers.push((settings.splits_path.clone(), Arc::downgrade(&timer)));
            timer
        }
    };

    state.splits_path = settings.splits_path;
    state.can_save_splits = settings.can_save_splits;
    state.timer = timer;
    state.layout = settings.layout;

    #[cfg(feature = "auto-splitting")]
    if !settings.auto_splitter_path.is_empty() {
        state
            .auto_splitter
            .load_script_blocking(PathBuf::from(settings.auto_splitter_path))
            .ok();
    }

    if state.width != settings.width || state.height != settings.height {
        state.width = settings.width;
        state.height = settings.height;

        obs_enter_graphics();
        let mut texture = gs_texture_create(
            state.width,
            state.height,
            GS_RGBA,
            1,
            ptr::null_mut(),
            GS_DYNAMIC,
        );
        mem::swap(&mut state.texture, &mut texture);
        gs_texture_destroy(texture);
        obs_leave_graphics();
    }
}

struct ObsLog;

impl Log for ObsLog {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            log(record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

#[no_mangle]
pub extern "C" fn obs_module_load() -> bool {
    static SOURCE_INFO: UnsafeMultiThread<obs_source_info> = UnsafeMultiThread(obs_source_info {
        id: cstr!("livesplit-one"),
        type_: OBS_SOURCE_TYPE_INPUT,
        output_flags: OBS_SOURCE_VIDEO
            | OBS_SOURCE_CUSTOM_DRAW
            | OBS_SOURCE_INTERACTION
            | OBS_SOURCE_CONTROLLABLE_MEDIA,
        get_name: Some(get_name),
        create: Some(create),
        destroy: Some(destroy),
        get_width: Some(get_width),
        get_height: Some(get_height),
        video_render: Some(video_render),
        mouse_wheel: Some(mouse_wheel),
        get_properties: Some(get_properties),
        get_defaults: Some(get_defaults),
        update: Some(update),
        icon_type: OBS_ICON_TYPE_GAME_CAPTURE,
        activate: None,
        deactivate: None,
        show: None,
        hide: None,
        video_tick: None,
        filter_video: None,
        filter_audio: None,
        enum_active_sources: None,
        save: None,
        load: None,
        mouse_click: None,
        mouse_move: None,
        focus: None,
        key_click: None,
        filter_remove: None,
        type_data: ptr::null_mut(),
        free_type_data: None,
        audio_render: None,
        enum_all_sources: None,
        transition_start: None,
        transition_stop: None,
        get_defaults2: None,
        get_properties2: None,
        audio_mix: None,
        media_play_pause: Some(media_play_pause),
        media_restart: Some(media_restart),
        media_stop: Some(media_stop),
        media_next: Some(media_next),
        media_previous: Some(media_previous),
        media_get_duration: Some(media_get_duration),
        media_get_time: Some(media_get_time),
        media_set_time: None,
        media_get_state: Some(media_get_state),
        version: 0,
        unversioned_id: ptr::null(),
    });

    let _ = log::set_logger(&ObsLog);
    log::set_max_level(LevelFilter::Debug);

    let source_info: &obs_source_info = &SOURCE_INFO.0;

    unsafe {
        obs_register_source_s(source_info, mem::size_of_val(source_info) as _);
    }
    true
}
