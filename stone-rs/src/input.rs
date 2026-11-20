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

const PAD_DEVICE_CLAIM_THRESHOLD: i32 = 16_384;
const PAD_AXIS_COUNT: usize = sdl2::sys::SDL_GameControllerAxis::SDL_CONTROLLER_AXIS_MAX as usize;
const PAD_BUTTON_COUNT: usize =
    sdl2::sys::SDL_GameControllerButton::SDL_CONTROLLER_BUTTON_MAX as usize;
const VIRTUAL_CONTROLLER_MAPPING: &CStr = c"03000000000000000000000000000000,Stone Virtual Pad,a:b0,b:b1,x:b2,y:b3,back:b4,start:b5,leftshoulder:b6,rightshoulder:b7,leftstick:b8,rightstick:b9,dpup:b10,dpdown:b11,dpleft:b12,dpright:b13,leftx:a0,lefty:a1,rightx:a2,righty:a3,lefttrigger:a4,righttrigger:a5";

thread_local! {
    static TEST_VIRTUAL_PAD_INDICES: RefCell<Vec<c_int>> = const { RefCell::new(Vec::new()) };
}

/// Every button, then every axis; a button's value is 1 while held.
type ControlValues = [i16; PAD_BUTTON_COUNT + PAD_AXIS_COUNT];

fn button_slot(button: Button) -> usize {
    button as usize
}

fn axis_slot(axis: Axis) -> usize {
    PAD_BUTTON_COUNT + axis as usize
}

/// Controller identity and state policy without SDL handles: the last pad to
/// press a button or push an axis past the claim threshold becomes active, and
/// only the active pad's input reaches the game.
pub(crate) struct ControllerManager {
    controllers: HashMap<u32, ControlValues>,
    active_instance: Option<u32>,
    pending: VecDeque<StoneEvent>,
}

impl ControllerManager {
    pub(crate) fn new() -> Self {
        Self {
            controllers: HashMap::new(),
            active_instance: None,
            pending: VecDeque::new(),
        }
    }

    fn push(&mut self, event_type: StoneEventType, instance_id: Option<u32>) -> &mut StoneEvent {
        self.pending.push_back(StoneEvent {
            event_type,
            pad_instance_id: instance_id.map_or(-1, |id| id as c_int),
            ..StoneEvent::default()
        });
        self.pending.back_mut().unwrap()
    }

    fn push_control(&mut self, instance_id: u32, slot: usize, value: i16, sync: bool) {
        let event = if slot < PAD_BUTTON_COUNT {
            let event_type = if value != 0 {
                StoneEventType::PadButtonDown
            } else {
                StoneEventType::PadButtonUp
            };
            let event = self.push(event_type, Some(instance_id));
            let button = unsafe {
                std::mem::transmute::<i32, sdl2::sys::SDL_GameControllerButton>(slot as i32)
            };
            event.pad_button = unsafe { sdl2::sys::SDL_GameControllerGetStringForButton(button) };
            event
        } else {
            let event = self.push(StoneEventType::PadAxisMotion, Some(instance_id));
            let axis = unsafe {
                std::mem::transmute::<i32, sdl2::sys::SDL_GameControllerAxis>(
                    (slot - PAD_BUTTON_COUNT) as i32,
                )
            };
            event.pad_axis = unsafe { sdl2::sys::SDL_GameControllerGetStringForAxis(axis) };
            event.pad_value = value as c_int;
            event
        };
        event.pad_sync = sync;
    }

    pub(crate) fn add_controller(&mut self, instance_id: u32) {
        self.controllers
            .insert(instance_id, [0; PAD_BUTTON_COUNT + PAD_AXIS_COUNT]);
    }

    pub(crate) fn remove_controller(&mut self, instance_id: u32) {
        if self.controllers.remove(&instance_id).is_none() {
            return;
        }
        self.push(StoneEventType::PadDeviceRemoved, Some(instance_id));
        if self.active_instance == Some(instance_id) {
            self.active_instance = None;
            self.push(StoneEventType::PadActiveChanged, None);
        }
    }

    pub(crate) fn active_instance_id(&self) -> Option<u32> {
        self.active_instance
    }

    #[cfg(test)]
    fn effective(&self, slot: usize) -> i16 {
        self.active_instance
            .and_then(|instance_id| self.controllers.get(&instance_id))
            .map_or(0, |values| values[slot])
    }

    /// A pad that takes over resends what it already holds as sync events, so
    /// the game sees its state without a fresh edge.
    fn control(&mut self, instance_id: u32, slot: usize, value: i16) {
        let Some(values) = self.controllers.get_mut(&instance_id) else {
            return;
        };
        values[slot] = value;
        let values = *values;
        if self.active_instance == Some(instance_id) {
            self.push_control(instance_id, slot, value, false);
            return;
        }
        let claims = if slot < PAD_BUTTON_COUNT {
            value != 0
        } else {
            (value as i32).abs() > PAD_DEVICE_CLAIM_THRESHOLD
        };
        if !claims {
            return;
        }
        self.active_instance = Some(instance_id);
        self.push(StoneEventType::PadActiveChanged, Some(instance_id));
        self.push_control(instance_id, slot, value, false);
        for (held, &held_value) in values.iter().enumerate() {
            if held != slot && held_value != 0 {
                self.push_control(instance_id, held, held_value, true);
            }
        }
    }

    pub(crate) fn button_event(&mut self, instance_id: u32, button: Button, down: bool) {
        self.control(instance_id, button_slot(button), down as i16);
    }

    pub(crate) fn axis_event(&mut self, instance_id: u32, axis: Axis, value: i16) {
        self.control(instance_id, axis_slot(axis), value);
    }

    pub(crate) fn pop_stone_event(&mut self) -> Option<StoneEvent> {
        self.pending.pop_front()
    }
}

pub(crate) struct SdlControllerManager {
    subsystem: GameControllerSubsystem,
    controllers: HashMap<u32, GameController>,
    pub(crate) manager: ControllerManager,
}

impl SdlControllerManager {
    pub(crate) fn new(subsystem: GameControllerSubsystem) -> Self {
        subsystem.set_event_state(true);
        let mut manager = Self {
            subsystem,
            controllers: HashMap::new(),
            manager: ControllerManager::new(),
        };
        for index in 0..manager.subsystem.num_joysticks().unwrap_or(0) {
            manager.open(index);
        }
        manager
    }

    fn open(&mut self, index: u32) -> Option<u32> {
        if !self.subsystem.is_game_controller(index) {
            return None;
        }
        let controller = self.subsystem.open(index).ok()?;
        let instance_id = controller.instance_id();
        self.controllers.insert(instance_id, controller);
        self.manager.add_controller(instance_id);
        Some(instance_id)
    }

    pub(crate) fn handle_sdl_event(&mut self, event: &Event) {
        match *event {
            Event::ControllerDeviceAdded { which, .. } => {
                if let Some(instance_id) = self.open(which) {
                    self.manager
                        .push(StoneEventType::PadDeviceAdded, Some(instance_id));
                }
            }
            Event::ControllerDeviceRemoved { which, .. } => {
                self.controllers.remove(&which);
                self.manager.remove_controller(which);
            }
            Event::ControllerButtonDown { which, button, .. } => {
                self.manager.button_event(which, button, true)
            }
            Event::ControllerButtonUp { which, button, .. } => {
                self.manager.button_event(which, button, false)
            }
            Event::ControllerAxisMotion {
                which, axis, value, ..
            } => self.manager.axis_event(which, axis, value),
            _ => {}
        }
    }

    fn active_type_name(&self) -> Option<&'static CStr> {
        let instance_id = self.manager.active_instance_id()?;
        let controller = unsafe {
            sdl2::sys::SDL_GameControllerFromInstanceID(instance_id as sdl2::sys::SDL_JoystickID)
        };
        if controller.is_null() {
            return None;
        }
        use sdl2::sys::SDL_GameControllerType as T;
        Some(
            match unsafe { sdl2::sys::SDL_GameControllerGetType(controller) } {
                T::SDL_CONTROLLER_TYPE_XBOX360 | T::SDL_CONTROLLER_TYPE_XBOXONE => c"xbox",
                T::SDL_CONTROLLER_TYPE_PS3
                | T::SDL_CONTROLLER_TYPE_PS4
                | T::SDL_CONTROLLER_TYPE_PS5 => c"playstation",
                T::SDL_CONTROLLER_TYPE_NINTENDO_SWITCH_PRO
                | T::SDL_CONTROLLER_TYPE_NINTENDO_SWITCH_JOYCON_LEFT
                | T::SDL_CONTROLLER_TYPE_NINTENDO_SWITCH_JOYCON_RIGHT
                | T::SDL_CONTROLLER_TYPE_NINTENDO_SWITCH_JOYCON_PAIR => c"nintendo",
                T::SDL_CONTROLLER_TYPE_VIRTUAL => c"virtual",
                _ => c"unknown",
            },
        )
    }
}

#[no_mangle]
pub extern "C" fn stone_pad_connected() -> bool {
    with_window(|window| !window.controller_manager.borrow().controllers.is_empty())
        .unwrap_or(false)
}

#[no_mangle]
pub extern "C" fn stone_pad_type_name() -> *const c_char {
    with_window(|window| window.controller_manager.borrow().active_type_name())
        .flatten()
        .map_or(ptr::null(), CStr::as_ptr)
}

fn virtual_pad_joystick(index: c_int) -> Option<*mut sdl2::sys::SDL_Joystick> {
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

fn token_str<'a>(token: *const c_char) -> Option<&'a str> {
    (!token.is_null()).then(|| unsafe { CStr::from_ptr(token) }.to_str().unwrap_or(""))
}

/// Set one virtual control and pump SDL so its controller event is queued.
fn drive_virtual_pad(
    index: c_int,
    control: Option<c_int>,
    set: impl FnOnce(*mut sdl2::sys::SDL_Joystick, c_int) -> c_int,
) -> bool {
    let Some(control) = control else {
        return false;
    };
    let Some(joystick) = virtual_pad_joystick(index) else {
        return false;
    };
    unsafe {
        sdl2::sys::SDL_GameControllerEventState(sdl2::sys::SDL_ENABLE as i32);
        sdl2::sys::SDL_JoystickEventState(sdl2::sys::SDL_ENABLE as i32);
        let result = set(joystick, control) == 0;
        sdl2::sys::SDL_JoystickUpdate();
        sdl2::sys::SDL_GameControllerUpdate();
        sdl2::sys::SDL_JoystickUpdate();
        sdl2::sys::SDL_PumpEvents();
        result
    }
}

#[no_mangle]
pub extern "C" fn stone_test_virtual_pad_attach() -> c_int {
    if with_window(|_| ()).is_none() {
        return -1;
    }
    if unsafe { sdl2::sys::SDL_GameControllerAddMapping(VIRTUAL_CONTROLLER_MAPPING.as_ptr()) } < 0 {
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

#[no_mangle]
pub extern "C" fn stone_test_virtual_pad_button(
    index: c_int,
    token: *const c_char,
    down: bool,
) -> bool {
    let button = token_str(token).and_then(Button::from_string);
    drive_virtual_pad(
        index,
        button.map(|b| b as c_int),
        |joystick, button| unsafe {
            sdl2::sys::SDL_JoystickSetVirtualButton(joystick, button, down as u8)
        },
    )
}

#[no_mangle]
pub extern "C" fn stone_test_virtual_pad_axis(
    index: c_int,
    token: *const c_char,
    value: c_int,
) -> bool {
    let (axis, value) = (
        token_str(token).and_then(Axis::from_string),
        i16::try_from(value),
    );
    let control = axis.filter(|_| value.is_ok()).map(|a| a as c_int);
    drive_virtual_pad(index, control, |joystick, axis| unsafe {
        sdl2::sys::SDL_JoystickSetVirtualAxis(joystick, axis, value.unwrap_or(0))
    })
}

#[no_mangle]
pub extern "C" fn stone_test_virtual_pad_detach(index: c_int) -> bool {
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
        crate::write_out(x, mouse.x());
        crate::write_out(y, mouse.y());
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

    fn describe(event: &StoneEvent) -> String {
        let id = event.pad_instance_id;
        let token = |ptr: *const c_char| unsafe { CStr::from_ptr(ptr) }.to_str().unwrap();
        let sync = if event.pad_sync { " sync" } else { "" };
        match event.event_type {
            StoneEventType::PadDeviceAdded => format!("added {id}"),
            StoneEventType::PadDeviceRemoved => format!("removed {id}"),
            StoneEventType::PadActiveChanged => format!("active {id}"),
            StoneEventType::PadButtonDown => format!("{id} {} down{sync}", token(event.pad_button)),
            StoneEventType::PadButtonUp => format!("{id} {} up{sync}", token(event.pad_button)),
            StoneEventType::PadAxisMotion => {
                format!("{id} {} {}{sync}", token(event.pad_axis), event.pad_value)
            }
            other => format!("{other:?}"),
        }
    }

    fn drain(manager: &mut ControllerManager) -> Vec<String> {
        std::iter::from_fn(|| manager.pop_stone_event())
            .map(|event| describe(&event))
            .collect()
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
        let mut step = |input: &dyn Fn(&mut ControllerManager), expected: &[&str]| {
            input(&mut manager);
            assert_eq!(drain(&mut manager), expected);
        };

        // Sub-threshold axis noise does not claim a pad.
        step(&|m| m.axis_event(10, Axis::LeftX, 1_000), &[]);
        step(
            &|m| m.axis_event(10, Axis::LeftX, 20_000),
            &["active 10", "10 leftx 20000"],
        );
        step(&|m| m.button_event(10, Button::A, true), &["10 a down"]);
        step(
            &|m| m.axis_event(20, Axis::RightX, -20_000),
            &["active 20", "20 rightx -20000"],
        );
        step(&|m| m.button_event(10, Button::Y, false), &[]);
        step(&|m| m.button_event(20, Button::B, true), &["20 b down"]);
        step(
            &|m| m.button_event(10, Button::B, true),
            &[
                "active 10",
                "10 b down",
                "10 a down sync",
                "10 leftx 20000 sync",
            ],
        );
        step(&|m| m.remove_controller(10), &["removed 10", "active -1"]);
        step(&|m| m.remove_controller(10), &[]);
        assert_eq!(manager.active_instance_id(), None);
        assert_eq!(manager.effective(button_slot(Button::B)), 0);

        // A reused instance value starts idle.
        manager.add_controller(10);
        assert_eq!(manager.active_instance_id(), None);
        manager.button_event(10, Button::X, true);
        assert_eq!(drain(&mut manager), ["active 10", "10 x down"]);
        assert_eq!(manager.effective(button_slot(Button::X)), 1);
        assert_eq!(manager.effective(axis_slot(Axis::LeftX)), 0);
    }

    struct VirtualJoystickGuard {
        indices: Vec<c_int>,
    }

    impl Drop for VirtualJoystickGuard {
        fn drop(&mut self) {
            for index in self.indices.drain(..) {
                unsafe {
                    sdl2::sys::SDL_JoystickDetachVirtual(index);
                }
            }
        }
    }

    fn drain_sdl(manager: &mut SdlControllerManager, pump: &mut sdl2::EventPump) -> Vec<String> {
        for _ in 0..8 {
            let mut polled = false;
            for event in pump.poll_iter() {
                polled = true;
                manager.handle_sdl_event(&event);
            }
            if !polled {
                break;
            }
        }
        drain(&mut manager.manager)
    }

    fn attach_virtual_pad() -> c_int {
        unsafe {
            sdl2::sys::SDL_JoystickAttachVirtual(
                sdl2::sys::SDL_JoystickType::SDL_JOYSTICK_TYPE_GAMECONTROLLER,
                PAD_AXIS_COUNT as c_int,
                PAD_BUTTON_COUNT as c_int,
                0,
            )
        }
    }

    fn added_instance(events: &[String]) -> u32 {
        events
            .iter()
            .find_map(|event| event.strip_prefix("added "))
            .expect("device-added must publish an instance ID")
            .parse()
            .unwrap()
    }

    #[test]
    fn virtual_gamecontroller_exercises_production_sdl_controller_path() {
        if crate::test_support::in_isolated_child(
            "input::tests::virtual_gamecontroller_exercises_production_sdl_controller_path",
            "STONE_VIRTUAL_CONTROLLER_TEST_CHILD",
        ) {
            return;
        }

        let sdl = sdl2::init().expect("SDL must initialize for the virtual controller gate");
        let subsystem = sdl
            .game_controller()
            .expect("SDL game-controller subsystem must initialize");
        let mapping_result =
            unsafe { sdl2::sys::SDL_GameControllerAddMapping(VIRTUAL_CONTROLLER_MAPPING.as_ptr()) };
        assert!(
            mapping_result >= 0,
            "virtual controller mapping must be accepted"
        );

        let mut manager = SdlControllerManager::new(subsystem);
        let mut pump = sdl.event_pump().expect("SDL event pump must initialize");
        let mut virtual_devices = VirtualJoystickGuard {
            indices: Vec::new(),
        };

        let first_index = attach_virtual_pad();
        assert!(first_index >= 0, "SDL virtual joystick attach must succeed");
        virtual_devices.indices.push(first_index);

        let first_instance = added_instance(&drain_sdl(&mut manager, &mut pump));
        assert!(
            manager.controllers.contains_key(&first_instance),
            "device-added must open the controller"
        );

        assert!(stone_test_virtual_pad_button(
            first_index,
            c"a".as_ptr(),
            true
        ));
        let pressed = drain_sdl(&mut manager, &mut pump);
        assert!(pressed.contains(&format!("{first_instance} a down")));
        assert!(stone_test_virtual_pad_button(
            first_index,
            c"a".as_ptr(),
            false
        ));
        assert_eq!(
            drain_sdl(&mut manager, &mut pump),
            [format!("{first_instance} a up")]
        );
        assert!(stone_test_virtual_pad_axis(
            first_index,
            c"leftx".as_ptr(),
            1_000
        ));
        assert_eq!(
            drain_sdl(&mut manager, &mut pump),
            [format!("{first_instance} leftx 1000")]
        );
        assert_eq!(manager.manager.effective(axis_slot(Axis::LeftX)), 1_000);
        assert!(stone_test_virtual_pad_axis(
            first_index,
            c"leftx".as_ptr(),
            22_000
        ));
        drain_sdl(&mut manager, &mut pump);
        assert_eq!(manager.manager.effective(axis_slot(Axis::LeftX)), 22_000);
        assert!(!stone_test_virtual_pad_axis(
            first_index,
            c"leftx".as_ptr(),
            40_000
        ));
        assert!(!stone_test_virtual_pad_button(
            first_index,
            c"nope".as_ptr(),
            true
        ));
        assert!(!stone_test_virtual_pad_button(-1, c"a".as_ptr(), true));

        let removed_index = virtual_devices.indices.pop().unwrap();
        assert_eq!(
            unsafe { sdl2::sys::SDL_JoystickDetachVirtual(removed_index) },
            0
        );
        assert_eq!(
            drain_sdl(&mut manager, &mut pump),
            [
                format!("{first_instance} leftx 0"),
                format!("removed {first_instance}"),
                "active -1".to_string()
            ]
        );

        let second_index = attach_virtual_pad();
        assert_eq!(
            second_index, removed_index,
            "SDL device index must be reused in this gate"
        );
        virtual_devices.indices.push(second_index);
        let second_instance = added_instance(&drain_sdl(&mut manager, &mut pump));
        assert_ne!(
            second_instance, first_instance,
            "SDL instance IDs must not be reused"
        );
        assert!(stone_test_virtual_pad_button(
            second_index,
            c"b".as_ptr(),
            true
        ));
        assert_eq!(
            drain_sdl(&mut manager, &mut pump),
            [
                format!("active {second_instance}"),
                format!("{second_instance} b down")
            ]
        );
    }
}
