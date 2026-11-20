use sdl2::controller::{Axis, Button, GameController};
use sdl2::event::Event;
use sdl2::keyboard::Keycode;
use sdl2::keyboard::Scancode;
use sdl2::GameControllerSubsystem;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_int, CStr, CString};
use std::ptr;

use crate::{with_window, StoneEvent, StoneEventType, StoneWindow};

const PAD_DEVICE_CLAIM_THRESHOLD: i16 = 16_384;
const PAD_AXIS_COUNT: usize = sdl2::sys::SDL_GameControllerAxis::SDL_CONTROLLER_AXIS_MAX as usize;
const PAD_BUTTON_COUNT: usize =
    sdl2::sys::SDL_GameControllerButton::SDL_CONTROLLER_BUTTON_MAX as usize;
const VIRTUAL_CONTROLLER_MAPPING: &str = "03000000000000000000000000000000,Stone Virtual Pad,a:b0,b:b1,x:b2,y:b3,back:b4,start:b5,leftshoulder:b6,rightshoulder:b7,leftstick:b8,rightstick:b9,dpup:b10,dpdown:b11,dpleft:b12,dpright:b13,leftx:a0,lefty:a1,rightx:a2,righty:a3,lefttrigger:a4,righttrigger:a5";

thread_local! {
    static TEST_VIRTUAL_PAD_INDICES: RefCell<Vec<c_int>> = const { RefCell::new(Vec::new()) };
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ControllerManagerEvent {
    DeviceAdded {
        instance_id: u32,
    },
    DeviceRemoved {
        instance_id: u32,
    },
    ActiveChanged {
        from: Option<u32>,
        to: Option<u32>,
    },
    Button {
        instance_id: u32,
        button: Button,
        down: bool,
        sync: bool,
    },
    Axis {
        instance_id: u32,
        axis: Axis,
        value: i16,
        sync: bool,
    },
}

#[derive(Default)]
struct ControllerState {
    buttons: [bool; PAD_BUTTON_COUNT],
    axes: [i16; PAD_AXIS_COUNT],
}

/// Pure controller identity/state policy. SDL handles and event translation
/// live in `SdlControllerManager`; this type is the testable authority rule.
pub(crate) struct ControllerManager {
    controllers: HashMap<u32, ControllerState>,
    active_instance: Option<u32>,
}

impl ControllerManager {
    pub(crate) fn new() -> Self {
        Self {
            controllers: HashMap::new(),
            active_instance: None,
        }
    }

    pub(crate) fn add_controller(&mut self, instance_id: u32) -> Vec<ControllerManagerEvent> {
        self.controllers
            .insert(instance_id, ControllerState::default());
        vec![ControllerManagerEvent::DeviceAdded { instance_id }]
    }

    pub(crate) fn remove_controller(&mut self, instance_id: u32) -> Vec<ControllerManagerEvent> {
        if self.controllers.remove(&instance_id).is_none() {
            return Vec::new();
        }

        let mut events = vec![ControllerManagerEvent::DeviceRemoved { instance_id }];
        if self.active_instance == Some(instance_id) {
            let previous = self.active_instance.take();
            events.push(ControllerManagerEvent::ActiveChanged {
                from: previous,
                to: None,
            });
        }
        events
    }

    pub(crate) fn active_instance_id(&self) -> Option<u32> {
        self.active_instance
    }

    #[cfg(test)]
    pub(crate) fn effective_button(&self, button: Button) -> bool {
        self.active_instance
            .and_then(|instance_id| self.controllers.get(&instance_id))
            .map(|state| state.buttons[button_index(button)])
            .unwrap_or(false)
    }

    #[cfg(test)]
    pub(crate) fn effective_axis(&self, axis: Axis) -> i16 {
        self.active_instance
            .and_then(|instance_id| self.controllers.get(&instance_id))
            .map(|state| state.axes[axis_index(axis)])
            .unwrap_or(0)
    }

    pub(crate) fn button_event(
        &mut self,
        instance_id: u32,
        button: Button,
        down: bool,
    ) -> Vec<ControllerManagerEvent> {
        let Some(state) = self.controllers.get_mut(&instance_id) else {
            return Vec::new();
        };
        state.buttons[button_index(button)] = down;

        if down && self.active_instance != Some(instance_id) {
            let mut events = self.activate(instance_id);
            events.push(ControllerManagerEvent::Button {
                instance_id,
                button,
                down,
                sync: false,
            });
            self.append_resync_events(&mut events, instance_id, Some(button), None);
            return events;
        }

        if self.active_instance == Some(instance_id) {
            return vec![ControllerManagerEvent::Button {
                instance_id,
                button,
                down,
                sync: false,
            }];
        }
        Vec::new()
    }

    pub(crate) fn axis_event(
        &mut self,
        instance_id: u32,
        axis: Axis,
        value: i16,
    ) -> Vec<ControllerManagerEvent> {
        let Some(state) = self.controllers.get_mut(&instance_id) else {
            return Vec::new();
        };
        state.axes[axis_index(axis)] = value;

        let claims_pad = (value as i32).abs() > PAD_DEVICE_CLAIM_THRESHOLD as i32;
        if self.active_instance != Some(instance_id) && !claims_pad {
            return Vec::new();
        }

        if self.active_instance != Some(instance_id) {
            let mut events = self.activate(instance_id);
            events.push(ControllerManagerEvent::Axis {
                instance_id,
                axis,
                value,
                sync: false,
            });
            self.append_resync_events(&mut events, instance_id, None, Some(axis));
            return events;
        }

        vec![ControllerManagerEvent::Axis {
            instance_id,
            axis,
            value,
            sync: false,
        }]
    }

    fn activate(&mut self, instance_id: u32) -> Vec<ControllerManagerEvent> {
        let previous = self.active_instance.replace(instance_id);
        if previous == Some(instance_id) {
            return Vec::new();
        }
        vec![ControllerManagerEvent::ActiveChanged {
            from: previous,
            to: Some(instance_id),
        }]
    }

    fn append_resync_events(
        &self,
        events: &mut Vec<ControllerManagerEvent>,
        instance_id: u32,
        cause_button: Option<Button>,
        cause_axis: Option<Axis>,
    ) {
        let Some(state) = self.controllers.get(&instance_id) else {
            return;
        };
        for (index, &down) in state.buttons.iter().enumerate() {
            if !down {
                continue;
            }
            let Some(button) = button_from_index(index) else {
                continue;
            };
            if cause_button == Some(button) {
                continue;
            }
            events.push(ControllerManagerEvent::Button {
                instance_id,
                button,
                down: true,
                sync: true,
            });
        }
        for (index, &value) in state.axes.iter().enumerate() {
            if value == 0 {
                continue;
            }
            let Some(axis) = axis_from_index(index) else {
                continue;
            };
            if cause_axis == Some(axis) {
                continue;
            }
            events.push(ControllerManagerEvent::Axis {
                instance_id,
                axis,
                value,
                sync: true,
            });
        }
    }
}

fn button_index(button: Button) -> usize {
    button as usize
}

fn axis_index(axis: Axis) -> usize {
    axis as usize
}

fn button_from_index(index: usize) -> Option<Button> {
    if index >= PAD_BUTTON_COUNT {
        return None;
    }
    let raw =
        unsafe { std::mem::transmute::<i32, sdl2::sys::SDL_GameControllerButton>(index as i32) };
    Button::from_ll(raw)
}

fn axis_from_index(index: usize) -> Option<Axis> {
    if index >= PAD_AXIS_COUNT {
        return None;
    }
    let raw =
        unsafe { std::mem::transmute::<i32, sdl2::sys::SDL_GameControllerAxis>(index as i32) };
    Axis::from_ll(raw)
}

pub(crate) struct SdlControllerManager {
    subsystem: GameControllerSubsystem,
    controllers: HashMap<u32, GameController>,
    manager: ControllerManager,
    pending: VecDeque<ControllerManagerEvent>,
}

impl SdlControllerManager {
    pub(crate) fn new(subsystem: GameControllerSubsystem) -> Self {
        subsystem.set_event_state(true);
        let mut manager = Self {
            subsystem,
            controllers: HashMap::new(),
            manager: ControllerManager::new(),
            pending: VecDeque::new(),
        };
        manager.open_existing_controllers();
        manager
    }

    fn open_existing_controllers(&mut self) {
        let count = self.subsystem.num_joysticks().unwrap_or(0);
        for index in 0..count {
            if !self.subsystem.is_game_controller(index) {
                continue;
            }
            let Ok(controller) = self.subsystem.open(index) else {
                continue;
            };
            let instance_id = controller.instance_id();
            self.controllers.insert(instance_id, controller);
            self.manager.add_controller(instance_id);
        }
    }

    pub(crate) fn handle_sdl_event(&mut self, event: &Event) -> Vec<ControllerManagerEvent> {
        match event {
            Event::ControllerDeviceAdded { which, .. } => {
                let index = *which;
                if !self.subsystem.is_game_controller(index) {
                    return Vec::new();
                }
                let controller = match self.subsystem.open(index) {
                    Ok(controller) => controller,
                    Err(_) => return Vec::new(),
                };
                let instance_id = controller.instance_id();
                self.controllers.insert(instance_id, controller);
                self.manager.add_controller(instance_id)
            }
            Event::ControllerDeviceRemoved { which, .. } => {
                let instance_id = *which;
                self.controllers.remove(&instance_id);
                self.manager.remove_controller(instance_id)
            }
            Event::ControllerButtonDown { which, button, .. } => {
                self.manager.button_event(*which, *button, true)
            }
            Event::ControllerButtonUp { which, button, .. } => {
                self.manager.button_event(*which, *button, false)
            }
            Event::ControllerAxisMotion {
                which, axis, value, ..
            } => self.manager.axis_event(*which, *axis, *value),
            _ => Vec::new(),
        }
    }

    pub(crate) fn enqueue(&mut self, events: Vec<ControllerManagerEvent>) {
        self.pending.extend(events);
    }

    pub(crate) fn pop_stone_event(&mut self) -> Option<StoneEvent> {
        self.pending
            .pop_front()
            .and_then(|event| controller_manager_event_to_stone_event(&event))
    }

    pub(crate) fn connected(&self) -> bool {
        !self.controllers.is_empty()
    }

    pub(crate) fn active_type_name(&self) -> Option<&'static str> {
        let instance_id = self.manager.active_instance_id()?;
        let controller = unsafe {
            sdl2::sys::SDL_GameControllerFromInstanceID(instance_id as sdl2::sys::SDL_JoystickID)
        };
        if controller.is_null() {
            return None;
        }
        Some(match unsafe { sdl2::sys::SDL_GameControllerGetType(controller) } {
            sdl2::sys::SDL_GameControllerType::SDL_CONTROLLER_TYPE_XBOX360
            | sdl2::sys::SDL_GameControllerType::SDL_CONTROLLER_TYPE_XBOXONE => "xbox",
            sdl2::sys::SDL_GameControllerType::SDL_CONTROLLER_TYPE_PS3
            | sdl2::sys::SDL_GameControllerType::SDL_CONTROLLER_TYPE_PS4
            | sdl2::sys::SDL_GameControllerType::SDL_CONTROLLER_TYPE_PS5 => "playstation",
            sdl2::sys::SDL_GameControllerType::SDL_CONTROLLER_TYPE_NINTENDO_SWITCH_PRO
            | sdl2::sys::SDL_GameControllerType::SDL_CONTROLLER_TYPE_NINTENDO_SWITCH_JOYCON_LEFT
            | sdl2::sys::SDL_GameControllerType::SDL_CONTROLLER_TYPE_NINTENDO_SWITCH_JOYCON_RIGHT
            | sdl2::sys::SDL_GameControllerType::SDL_CONTROLLER_TYPE_NINTENDO_SWITCH_JOYCON_PAIR => "nintendo",
            sdl2::sys::SDL_GameControllerType::SDL_CONTROLLER_TYPE_VIRTUAL => "virtual",
            _ => "unknown",
        })
    }

    #[cfg(test)]
    fn is_open(&self, instance_id: u32) -> bool {
        self.controllers.contains_key(&instance_id)
    }
}

fn test_virtual_pad_joystick(index: c_int) -> Option<*mut sdl2::sys::SDL_Joystick> {
    if index < 0 {
        return None;
    }
    let instance = unsafe { sdl2::sys::SDL_JoystickGetDeviceInstanceID(index) };
    if instance < 0 {
        return None;
    }
    let existing_controller = unsafe {
        sdl2::sys::SDL_GameControllerFromInstanceID(instance as sdl2::sys::SDL_JoystickID)
    };
    if existing_controller.is_null() {
        return None;
    }
    let joystick = unsafe { sdl2::sys::SDL_GameControllerGetJoystick(existing_controller) };
    (!joystick.is_null()).then_some(joystick)
}

pub(crate) fn test_virtual_pad_attach() -> c_int {
    if with_window(|_| ()).is_none() {
        return -1;
    }
    let mapping = CString::new(VIRTUAL_CONTROLLER_MAPPING).expect("virtual mapping has no NUL");
    let mapping_result = unsafe { sdl2::sys::SDL_GameControllerAddMapping(mapping.as_ptr()) };
    if mapping_result < 0 {
        return -1;
    }
    let index = unsafe {
        sdl2::sys::SDL_JoystickAttachVirtual(
            sdl2::sys::SDL_JoystickType::SDL_JOYSTICK_TYPE_GAMECONTROLLER,
            PAD_AXIS_COUNT as c_int,
            PAD_BUTTON_COUNT as c_int,
            0,
        )
    };
    if index >= 0 {
        TEST_VIRTUAL_PAD_INDICES.with(|indices| indices.borrow_mut().push(index));
    }
    index
}

pub(crate) fn test_virtual_pad_button(index: c_int, token: *const c_char, down: bool) -> bool {
    if token.is_null() {
        return false;
    }
    let token = unsafe { CStr::from_ptr(token) }.to_str().unwrap_or("");
    let Some(button) = Button::from_string(token) else {
        return false;
    };
    let Some(joystick) = test_virtual_pad_joystick(index) else {
        return false;
    };
    unsafe { sdl2::sys::SDL_GameControllerEventState(sdl2::sys::SDL_ENABLE as i32) };
    unsafe { sdl2::sys::SDL_JoystickEventState(sdl2::sys::SDL_ENABLE as i32) };
    let result = unsafe {
        sdl2::sys::SDL_JoystickSetVirtualButton(
            joystick,
            button as c_int,
            if down {
                sdl2::sys::SDL_PRESSED as u8
            } else {
                sdl2::sys::SDL_RELEASED as u8
            },
        )
    } == 0;
    unsafe { sdl2::sys::SDL_JoystickUpdate() };
    unsafe { sdl2::sys::SDL_GameControllerUpdate() };
    unsafe { sdl2::sys::SDL_JoystickUpdate() };
    unsafe { sdl2::sys::SDL_PumpEvents() };
    result
}

pub(crate) fn test_virtual_pad_axis(index: c_int, token: *const c_char, value: c_int) -> bool {
    if token.is_null() {
        return false;
    }
    let token = unsafe { CStr::from_ptr(token) }.to_str().unwrap_or("");
    let Some(axis) = Axis::from_string(token) else {
        return false;
    };
    let Ok(value) = i16::try_from(value) else {
        return false;
    };
    let Some(joystick) = test_virtual_pad_joystick(index) else {
        return false;
    };
    unsafe { sdl2::sys::SDL_GameControllerEventState(sdl2::sys::SDL_ENABLE as i32) };
    unsafe { sdl2::sys::SDL_JoystickEventState(sdl2::sys::SDL_ENABLE as i32) };
    let result =
        unsafe { sdl2::sys::SDL_JoystickSetVirtualAxis(joystick, axis as c_int, value) } == 0;
    unsafe { sdl2::sys::SDL_JoystickUpdate() };
    unsafe { sdl2::sys::SDL_GameControllerUpdate() };
    unsafe { sdl2::sys::SDL_JoystickUpdate() };
    unsafe { sdl2::sys::SDL_PumpEvents() };
    result
}

pub(crate) fn test_virtual_pad_detach(index: c_int) -> bool {
    let result = unsafe { sdl2::sys::SDL_JoystickDetachVirtual(index) } == 0;
    if result {
        TEST_VIRTUAL_PAD_INDICES.with(|indices| {
            indices.borrow_mut().retain(|known| *known != index);
        });
    }
    result
}

pub(crate) fn test_virtual_pad_cleanup() {
    TEST_VIRTUAL_PAD_INDICES.with(|indices| {
        for index in indices.borrow_mut().drain(..) {
            if index >= 0 {
                unsafe { sdl2::sys::SDL_JoystickDetachVirtual(index) };
            }
        }
    });
}

fn controller_manager_event_to_stone_event(event: &ControllerManagerEvent) -> Option<StoneEvent> {
    let mut translated = StoneEvent::default();
    match event {
        ControllerManagerEvent::DeviceAdded { instance_id } => {
            translated.event_type = StoneEventType::PadDeviceAdded;
            translated.pad_instance_id = *instance_id as c_int;
        }
        ControllerManagerEvent::DeviceRemoved { instance_id } => {
            translated.event_type = StoneEventType::PadDeviceRemoved;
            translated.pad_instance_id = *instance_id as c_int;
        }
        ControllerManagerEvent::ActiveChanged { to, .. } => {
            translated.event_type = StoneEventType::PadActiveChanged;
            translated.pad_instance_id = to.map(|id| id as c_int).unwrap_or(-1);
        }
        ControllerManagerEvent::Button {
            instance_id,
            button,
            down,
            sync,
        } => {
            translated.event_type = if *down {
                StoneEventType::PadButtonDown
            } else {
                StoneEventType::PadButtonUp
            };
            translated.pad_instance_id = *instance_id as c_int;
            translated.pad_button = controller_button_token(*button);
            translated.pad_sync = *sync;
        }
        ControllerManagerEvent::Axis {
            instance_id,
            axis,
            value,
            sync,
        } => {
            translated.event_type = StoneEventType::PadAxisMotion;
            translated.pad_instance_id = *instance_id as c_int;
            translated.pad_axis = controller_axis_token(*axis);
            translated.pad_value = *value as c_int;
            translated.pad_sync = *sync;
        }
    }
    Some(translated)
}

fn controller_button_token(button: Button) -> *const c_char {
    let raw =
        unsafe { std::mem::transmute::<i32, sdl2::sys::SDL_GameControllerButton>(button as i32) };
    unsafe { sdl2::sys::SDL_GameControllerGetStringForButton(raw) }
}

fn controller_axis_token(axis: Axis) -> *const c_char {
    let raw = unsafe { std::mem::transmute::<i32, sdl2::sys::SDL_GameControllerAxis>(axis as i32) };
    unsafe { sdl2::sys::SDL_GameControllerGetStringForAxis(raw) }
}

struct ScancodeToken {
    token: &'static [u8],
    scancode: Scancode,
}

macro_rules! scancode_tokens {
    ($($token:tt => $scancode:ident,)*) => {
        &[$(ScancodeToken {
            token: concat!(stringify!($token), "\0").as_bytes(),
            scancode: Scancode::$scancode,
        },)*]
    };
}

// These tokens are the persisted identity. SDL names are consulted only by
// stone_scancode_label, because they describe the active keyboard layout.
#[rustfmt::skip]
static SCANCODE_TOKENS: &[ScancodeToken] = scancode_tokens! {
    a => A, b => B, c => C, d => D, e => E, f => F, g => G, h => H, i => I, j => J, k => K,
    l => L, m => M, n => N, o => O, p => P, q => Q, r => R, s => S, t => T, u => U, v => V,
    w => W, x => X, y => Y, z => Z, 1 => Num1, 2 => Num2, 3 => Num3, 4 => Num4, 5 => Num5,
    6 => Num6, 7 => Num7, 8 => Num8, 9 => Num9, 0 => Num0, return => Return, escape => Escape,
    backspace => Backspace, tab => Tab, space => Space, minus => Minus, equals => Equals,
    leftbracket => LeftBracket, rightbracket => RightBracket, backslash => Backslash,
    nonushash => NonUsHash, semicolon => Semicolon, apostrophe => Apostrophe, grave => Grave,
    comma => Comma, period => Period, slash => Slash, capslock => CapsLock, f1 => F1, f2 => F2,
    f3 => F3, f4 => F4, f5 => F5, f6 => F6, f7 => F7, f8 => F8, f9 => F9, f10 => F10,
    f11 => F11, f12 => F12, f13 => F13, f14 => F14, f15 => F15, f16 => F16, f17 => F17,
    f18 => F18, f19 => F19, f20 => F20, f21 => F21, f22 => F22, f23 => F23, f24 => F24,
    printscreen => PrintScreen, scrolllock => ScrollLock, pause => Pause, insert => Insert,
    home => Home, pageup => PageUp, delete => Delete, end => End, pagedown => PageDown,
    right => Right, left => Left, down => Down, up => Up, numlockclear => NumLockClear,
    kpdivide => KpDivide, kpmultiply => KpMultiply, kpminus => KpMinus, kpplus => KpPlus,
    kpenter => KpEnter, kp1 => Kp1, kp2 => Kp2, kp3 => Kp3, kp4 => Kp4, kp5 => Kp5, kp6 => Kp6,
    kp7 => Kp7, kp8 => Kp8, kp9 => Kp9, kp0 => Kp0, kpperiod => KpPeriod,
    nonusbackslash => NonUsBackslash, application => Application, power => Power,
    kpequals => KpEquals, lctrl => LCtrl, lshift => LShift, lalt => LAlt, lgui => LGui,
    rctrl => RCtrl, rshift => RShift, ralt => RAlt, rgui => RGui,
};

fn scancode_token_entry(token: &[u8]) -> Option<&'static ScancodeToken> {
    SCANCODE_TOKENS.iter().find(|entry| entry.token == token)
}

fn scancode_value_entry(scancode: c_int) -> Option<&'static ScancodeToken> {
    SCANCODE_TOKENS
        .iter()
        .find(|entry| entry.scancode as c_int == scancode)
}

#[no_mangle]
pub extern "C" fn stone_scancode_token_at(index: c_int) -> *const c_char {
    if index < 0 {
        return ptr::null();
    }
    SCANCODE_TOKENS
        .get(index as usize)
        .map(|entry| entry.token.as_ptr() as *const c_char)
        .unwrap_or(ptr::null())
}

#[no_mangle]
pub extern "C" fn stone_scancode_token_for(scancode: c_int) -> *const c_char {
    scancode_value_entry(scancode)
        .map(|entry| entry.token.as_ptr() as *const c_char)
        .unwrap_or(ptr::null())
}

#[no_mangle]
pub extern "C" fn stone_scancode_value(token: *const c_char) -> c_int {
    if token.is_null() {
        return -1;
    }
    let token = unsafe { CStr::from_ptr(token) }.to_bytes_with_nul();
    scancode_token_entry(token)
        .map(|entry| entry.scancode as c_int)
        .unwrap_or(-1)
}

thread_local! {
    static SCANCODE_LABEL: RefCell<CString> = RefCell::new(CString::new("").unwrap());
}

#[no_mangle]
pub extern "C" fn stone_scancode_label(token: *const c_char) -> *const c_char {
    if token.is_null() {
        return ptr::null();
    }
    let token = unsafe { CStr::from_ptr(token) }.to_bytes_with_nul();
    let Some(entry) = scancode_token_entry(token) else {
        return ptr::null();
    };
    let label = Keycode::from_scancode(entry.scancode)
        .map(|keycode| keycode.name())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| entry.scancode.name().to_string());
    // SDL leaves both names empty for some physical keys, including the ISO key.
    if label.is_empty() {
        return entry.token.as_ptr().cast();
    }
    SCANCODE_LABEL.with(|slot| {
        *slot.borrow_mut() = CString::new(label).expect("SDL key labels contain no NUL");
        slot.borrow().as_ptr()
    })
}

#[no_mangle]
pub extern "C" fn stone_is_key_down(key: *const c_char) -> bool {
    if key.is_null() {
        return false;
    }
    let key_str = unsafe { CStr::from_ptr(key).to_str().unwrap_or("") };

    with_window(|window: &StoneWindow| {
        let pump = window.event_pump.borrow();
        let keyboard = pump.keyboard_state();

        // Stone's `sdl_keycode_to_string` (lib.rock) emits canonical
        // short names like "lshift", "rshift", "left", etc. SDL's own
        // `Keycode::from_name` expects the human-readable "Left Shift"
        // form -- so a round trip via name fails for modifiers.
        // Translate the short names we emit ourselves.
        if let Some(scancode) = canonical_scancode(key_str) {
            return keyboard.is_scancode_pressed(scancode);
        }
        if let Some(keycode) = Keycode::from_name(key_str) {
            if let Some(scancode) = sdl2::keyboard::Scancode::from_keycode(keycode) {
                return keyboard.is_scancode_pressed(scancode);
            }
        }
        false
    })
    .unwrap_or(false)
}

pub(crate) fn canonical_scancode(name: &str) -> Option<sdl2::keyboard::Scancode> {
    use sdl2::keyboard::Scancode as S;
    if let [byte] = name.as_bytes() {
        let value = match byte {
            b'a'..=b'z' => S::A as c_int + c_int::from(*byte - b'a'),
            b'1'..=b'9' => S::Num1 as c_int + c_int::from(*byte - b'1'),
            b'0' => S::Num0 as c_int,
            _ => -1,
        };
        if let Some(scancode) = S::from_i32(value) {
            return Some(scancode);
        }
    }
    Some(match name {
        "lshift" => S::LShift,
        "rshift" => S::RShift,
        "lctrl" => S::LCtrl,
        "rctrl" => S::RCtrl,
        "lalt" => S::LAlt,
        "ralt" => S::RAlt,
        "lgui" | "lmeta" => S::LGui,
        "rgui" | "rmeta" => S::RGui,
        "left" => S::Left,
        "right" => S::Right,
        "up" => S::Up,
        "down" => S::Down,
        "return" | "enter" => S::Return,
        "escape" => S::Escape,
        "tab" => S::Tab,
        "space" => S::Space,
        "backspace" => S::Backspace,
        _ => return None,
    })
}

#[no_mangle]
pub extern "C" fn stone_get_mouse_position(x: *mut c_int, y: *mut c_int) {
    with_window(|window: &StoneWindow| {
        let pump = window.event_pump.borrow();
        let mouse = pump.mouse_state();
        unsafe {
            if !x.is_null() {
                *x = mouse.x();
            }
            if !y.is_null() {
                *y = mouse.y();
            }
        }
    });
}

#[no_mangle]
pub extern "C" fn stone_get_global_mouse_position(x: *mut c_int, y: *mut c_int) {
    // SDL2 crate wraps this differently, using regular position for now
    stone_get_mouse_position(x, y);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, CString};

    fn run_virtual_controller_test_in_child() -> bool {
        crate::test_support::in_isolated_child(
            "input::tests::virtual_gamecontroller_exercises_production_sdl_controller_path",
            "STONE_VIRTUAL_CONTROLLER_TEST_CHILD",
        )
    }

    fn event_has_button(
        events: &[ControllerManagerEvent],
        button: Button,
        down: bool,
        sync: bool,
    ) -> bool {
        events.iter().any(|event| {
            matches!(
                event,
                ControllerManagerEvent::Button {
                    button: actual,
                    down: actual_down,
                    sync: actual_sync,
                    ..
                } if *actual == button && *actual_down == down && *actual_sync == sync
            )
        })
    }

    fn has_active_changed(
        events: &[ControllerManagerEvent],
        expected_from: Option<u32>,
        expected_to: Option<u32>,
    ) -> bool {
        events.iter().any(|event| {
            matches!(
                event,
                ControllerManagerEvent::ActiveChanged { from, to }
                    if *from == expected_from && *to == expected_to
            )
        })
    }

    fn has_axis(
        events: &[ControllerManagerEvent],
        expected_axis: Axis,
        expected_value: i16,
        expected_sync: bool,
    ) -> bool {
        events.iter().any(|event| {
            matches!(
                event,
                ControllerManagerEvent::Axis { axis, value, sync, .. }
                    if *axis == expected_axis && *value == expected_value && *sync == expected_sync
            )
        })
    }

    #[test]
    fn supported_scancode_labels_are_never_empty() {
        for entry in SCANCODE_TOKENS {
            let label = stone_scancode_label(entry.token.as_ptr().cast());
            assert!(
                !label.is_null(),
                "supported token has a label: {:?}",
                entry.token
            );
            let label = unsafe { CStr::from_ptr(label) };
            assert!(
                !label.to_bytes().is_empty(),
                "supported token has visible text: {:?}",
                entry.token
            );
        }
        assert!(stone_scancode_label(c"not-a-scancode".as_ptr()).is_null());
        assert!(stone_scancode_label(ptr::null()).is_null());
    }

    #[test]
    fn controller_manager_selects_activity_and_resyncs_without_inherited_edges() {
        let mut manager = ControllerManager::new();
        manager.add_controller(10);
        manager.add_controller(20);

        let noise = manager.axis_event(10, Axis::LeftX, 1_000);
        assert!(
            noise.is_empty(),
            "sub-threshold axis noise must not claim a pad"
        );
        assert_eq!(manager.active_instance_id(), None);

        let first_activity = manager.axis_event(10, Axis::LeftX, 20_000);
        assert!(has_active_changed(&first_activity, None, Some(10)));
        assert!(has_axis(&first_activity, Axis::LeftX, 20_000, false));
        assert_eq!(manager.active_instance_id(), Some(10));

        let held = manager.button_event(10, Button::A, true);
        assert!(event_has_button(&held, Button::A, true, false));
        assert!(manager.effective_button(Button::A));

        let switched = manager.axis_event(20, Axis::RightX, -20_000);
        assert!(has_active_changed(&switched, Some(10), Some(20)));
        assert!(has_axis(&switched, Axis::RightX, -20_000, false));
        assert!(!event_has_button(&switched, Button::A, true, false));
        assert!(!manager.effective_button(Button::A));
        assert_eq!(manager.effective_axis(Axis::LeftX), 0);

        let second_press = manager.button_event(20, Button::B, true);
        assert!(event_has_button(&second_press, Button::B, true, false));

        let removed = manager.remove_controller(20);
        assert!(has_active_changed(&removed, Some(20), None));
        assert_eq!(manager.active_instance_id(), None);
        assert!(!manager.effective_button(Button::B));

        manager.add_controller(20);
        assert_eq!(
            manager.active_instance_id(),
            None,
            "a reused instance value starts idle"
        );
        let reused = manager.button_event(20, Button::X, true);
        assert!(event_has_button(&reused, Button::X, true, false));
        assert_eq!(manager.active_instance_id(), Some(20));
    }

    struct VirtualJoystickGuard {
        indices: Vec<c_int>,
    }

    impl Drop for VirtualJoystickGuard {
        fn drop(&mut self) {
            for index in self.indices.drain(..) {
                if index >= 0 {
                    unsafe {
                        sdl2::sys::SDL_JoystickDetachVirtual(index);
                    }
                }
            }
        }
    }

    fn drain_sdl_controller_events(
        manager: &mut SdlControllerManager,
        pump: &mut sdl2::EventPump,
    ) -> Vec<ControllerManagerEvent> {
        let mut events = Vec::new();
        for _ in 0..8 {
            let mut polled = false;
            for event in pump.poll_iter() {
                polled = true;
                events.extend(manager.handle_sdl_event(&event));
            }
            if !polled {
                break;
            }
        }
        events
    }

    fn virtual_mapping() -> CString {
        CString::new(
            "03000000000000000000000000000000,Stone Virtual Pad,a:b0,b:b1,x:b2,y:b3,back:b4,start:b5,leftshoulder:b6,rightshoulder:b7,leftstick:b8,rightstick:b9,dpup:b10,dpdown:b11,dpleft:b12,dpright:b13,leftx:a0,lefty:a1,rightx:a2,righty:a3,lefttrigger:a4,righttrigger:a5",
        )
        .unwrap()
    }

    #[test]
    fn virtual_gamecontroller_exercises_production_sdl_controller_path() {
        if run_virtual_controller_test_in_child() {
            return;
        }

        let sdl = sdl2::init().expect("SDL must initialize for the virtual controller gate");
        let subsystem = sdl
            .game_controller()
            .expect("SDL game-controller subsystem must initialize");
        subsystem.set_event_state(true);

        let mapping = virtual_mapping();
        let mapping_result = unsafe { sdl2::sys::SDL_GameControllerAddMapping(mapping.as_ptr()) };
        assert!(
            mapping_result >= 0,
            "virtual controller mapping must be accepted"
        );

        let mut manager = SdlControllerManager::new(subsystem.clone());
        let mut pump = sdl.event_pump().expect("SDL event pump must initialize");
        let mut virtual_devices = VirtualJoystickGuard {
            indices: Vec::new(),
        };

        let axis_count = sdl2::sys::SDL_GameControllerAxis::SDL_CONTROLLER_AXIS_MAX as c_int;
        let button_count = sdl2::sys::SDL_GameControllerButton::SDL_CONTROLLER_BUTTON_MAX as c_int;
        let first_index = unsafe {
            sdl2::sys::SDL_JoystickAttachVirtual(
                sdl2::sys::SDL_JoystickType::SDL_JOYSTICK_TYPE_GAMECONTROLLER,
                axis_count,
                button_count,
                0,
            )
        };
        assert!(first_index >= 0, "SDL virtual joystick attach must succeed");
        virtual_devices.indices.push(first_index);
        assert!(unsafe {
            sdl2::sys::SDL_JoystickIsVirtual(first_index) == sdl2::sys::SDL_bool::SDL_TRUE
        });

        let initial_events = drain_sdl_controller_events(&mut manager, &mut pump);
        let added_count = initial_events
            .iter()
            .filter(|event| matches!(event, ControllerManagerEvent::DeviceAdded { .. }))
            .count();
        assert!(
            added_count > 0,
            "production SDL path must observe device-added"
        );
        let first_instance = initial_events
            .iter()
            .find_map(|event| match event {
                ControllerManagerEvent::DeviceAdded { instance_id } => Some(*instance_id),
                _ => None,
            })
            .expect("device-added must publish an instance ID");
        assert!(
            manager.is_open(first_instance),
            "device-added must open the controller"
        );
        assert!(unsafe {
            sdl2::sys::SDL_IsGameController(first_index) != sdl2::sys::SDL_bool::SDL_FALSE
        });

        let joystick = unsafe { sdl2::sys::SDL_JoystickFromInstanceID(first_instance as i32) };
        assert!(
            !joystick.is_null(),
            "production open must expose the joystick handle"
        );

        let mut observed = Vec::new();
        assert!(test_virtual_pad_button(first_index, c"a".as_ptr(), true));
        observed.extend(drain_sdl_controller_events(&mut manager, &mut pump));
        assert!(manager.manager.effective_button(Button::A));

        assert!(test_virtual_pad_button(first_index, c"a".as_ptr(), false));
        observed.extend(drain_sdl_controller_events(&mut manager, &mut pump));

        for value in [1_000_i16, 22_000_i16] {
            assert!(test_virtual_pad_axis(
                first_index,
                c"leftx".as_ptr(),
                value as c_int
            ));
            observed.extend(drain_sdl_controller_events(&mut manager, &mut pump));
            if value == 1_000 {
                assert_eq!(manager.manager.effective_axis(Axis::LeftX), 1_000);
            }
        }
        assert_eq!(manager.manager.effective_axis(Axis::LeftX), 22_000);

        let button_down_count = observed
            .iter()
            .filter(|event| matches!(event, ControllerManagerEvent::Button { down: true, .. }))
            .count();
        let button_up_count = observed
            .iter()
            .filter(|event| matches!(event, ControllerManagerEvent::Button { down: false, .. }))
            .count();
        let observed_axis_count = observed
            .iter()
            .filter(|event| matches!(event, ControllerManagerEvent::Axis { .. }))
            .count();
        assert!(
            button_down_count > 0,
            "button press must cross the production boundary"
        );
        assert!(
            button_up_count > 0,
            "button release must cross the production boundary"
        );
        assert!(
            observed_axis_count >= 2,
            "both below- and above-threshold axes must cross the boundary"
        );

        let translated = observed
            .iter()
            .filter_map(controller_manager_event_to_stone_event)
            .collect::<Vec<_>>();
        assert!(translated
            .iter()
            .any(|event| event.event_type == StoneEventType::PadButtonDown));
        assert!(translated
            .iter()
            .any(|event| event.event_type == StoneEventType::PadButtonUp));
        assert!(translated
            .iter()
            .any(|event| event.event_type == StoneEventType::PadAxisMotion));
        let button_event = translated
            .iter()
            .find(|event| event.event_type == StoneEventType::PadButtonDown)
            .expect("button event must carry a stable token");
        assert!(!button_event.pad_button.is_null());
        let button_token = unsafe { CStr::from_ptr(button_event.pad_button) }
            .to_str()
            .unwrap();
        assert_eq!(button_token, "a");

        let removed_index = virtual_devices.indices.pop().unwrap();
        assert_eq!(
            unsafe { sdl2::sys::SDL_JoystickDetachVirtual(removed_index) },
            0
        );
        let removed_events = drain_sdl_controller_events(&mut manager, &mut pump);
        assert!(removed_events.iter().any(|event| matches!(
            event,
            ControllerManagerEvent::DeviceRemoved { instance_id } if *instance_id == first_instance
        )));
        assert_eq!(
            manager.manager.active_instance_id(),
            None,
            "removal must clear the active instance"
        );

        let second_index = unsafe {
            sdl2::sys::SDL_JoystickAttachVirtual(
                sdl2::sys::SDL_JoystickType::SDL_JOYSTICK_TYPE_GAMECONTROLLER,
                axis_count,
                button_count,
                0,
            )
        };
        assert_eq!(
            second_index, removed_index,
            "SDL device index must be reused in this gate"
        );
        virtual_devices.indices.push(second_index);
        let reconnect_events = drain_sdl_controller_events(&mut manager, &mut pump);
        let second_instance = reconnect_events
            .iter()
            .find_map(|event| match event {
                ControllerManagerEvent::DeviceAdded { instance_id } => Some(*instance_id),
                _ => None,
            })
            .expect("reconnect must publish a new device-added event");
        assert_ne!(
            second_instance, first_instance,
            "SDL instance IDs must not be reused"
        );
        assert!(manager.is_open(second_instance));
        let second_joystick =
            unsafe { sdl2::sys::SDL_JoystickFromInstanceID(second_instance as i32) };
        assert!(!second_joystick.is_null());
        assert_eq!(
            unsafe {
                sdl2::sys::SDL_JoystickSetVirtualButton(
                    second_joystick,
                    Button::B as c_int,
                    sdl2::sys::SDL_PRESSED as u8,
                )
            },
            0
        );
        let reconnect_input = drain_sdl_controller_events(&mut manager, &mut pump);
        assert!(event_has_button(&reconnect_input, Button::B, true, false));
        assert_eq!(manager.manager.active_instance_id(), Some(second_instance));
    }
}
