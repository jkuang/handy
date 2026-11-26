//! Match modifier-only shortcuts at the FlagsChanged event, before any side
//! information can be lost. Keyed shortcuts still use the handy-keys manager.
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;

use handy_keys::Modifiers;
use objc2_core_foundation::{kCFRunLoopDefaultMode, CFMachPort, CFRetained, CFRunLoop};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags as Flags, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};

// Carbon virtual key codes and IOKit's device-dependent NX_DEVICE*KEYMASK
// bits (IOLLEvent.h). Some remappers supply only the group flag.
const GROUPS: [(Flags, u16, u16, u64, u64, Modifiers, Modifiers); 4] = [
    (
        Flags::MaskCommand,
        55,
        54,
        0x08,
        0x10,
        Modifiers::CMD_LEFT,
        Modifiers::CMD_RIGHT,
    ),
    (
        Flags::MaskShift,
        56,
        60,
        0x02,
        0x04,
        Modifiers::SHIFT_LEFT,
        Modifiers::SHIFT_RIGHT,
    ),
    (
        Flags::MaskControl,
        59,
        62,
        0x01,
        0x2000,
        Modifiers::CTRL_LEFT,
        Modifiers::CTRL_RIGHT,
    ),
    (
        Flags::MaskAlternate,
        58,
        61,
        0x20,
        0x40,
        Modifiers::OPT_LEFT,
        Modifiers::OPT_RIGHT,
    ),
];

fn modifiers(previous: Modifiers, key: u16, flags: Flags) -> (Modifiers, Option<Modifiers>) {
    let mut current = Modifiers::empty();
    let mut changed = (key == 63).then_some(Modifiers::FN);
    current.set(Modifiers::FN, flags.contains(Flags::MaskSecondaryFn));
    for (mask, lcode, rcode, lbit, rbit, left, right) in GROUPS {
        let side = if key == lcode {
            Some(left)
        } else if key == rcode {
            Some(right)
        } else {
            None
        };
        changed = changed.or(side);
        if !flags.contains(mask) {
            continue;
        }
        if flags.0 & (lbit | rbit) != 0 {
            current.set(left, flags.0 & lbit != 0);
            current.set(right, flags.0 & rbit != 0);
        } else {
            let mut held = previous & (left | right);
            if let Some(side) = side {
                // A release with the group still held requires the other side.
                held.set(side, !held.contains(left | right));
            } else if held.is_empty() {
                held = left;
            }
            current |= held;
        }
    }
    (current, changed)
}

struct Binding {
    id: String,
    shortcut: String,
    held: bool,
}

#[derive(Default)]
struct State {
    tracked: Option<Modifiers>,
    bindings: HashMap<Modifiers, Binding>,
    pending: Vec<(String, String, bool)>,
}

impl State {
    fn flags_changed(&mut self, key: u16, flags: Flags, own: bool) -> bool {
        if own {
            return false;
        }
        let before = self.tracked.unwrap_or_else(Modifiers::empty);
        let (after, changed) = modifiers(before, key, flags);
        self.tracked = Some(after);
        let expected = before ^ changed.unwrap_or_else(Modifiers::empty);
        if after != expected {
            // Only known modifier key codes may appear in the log.
            log::debug!("Reconciled modifiers {before:?} -> {after:?}, event=FlagsChanged keycode={:?} own={own}", changed.map(|_| key));
        }
        let pressed = changed.is_some_and(|side| after.contains(side));
        let mut block = false;
        for (&pattern, binding) in &mut self.bindings {
            let matched = pressed && pattern.matches(after);
            block |= matched;
            // Holding an unrelated modifier during push-to-talk is fine.
            let held = matched || (binding.held && pattern.matches(after & pattern));
            if held != binding.held {
                binding.held = held;
                self.pending
                    .push((binding.id.clone(), binding.shortcut.clone(), held));
            }
        }
        block
    }

    fn reset(&mut self) {
        self.tracked = None;
        for binding in self.bindings.values_mut().filter(|b| b.held) {
            binding.held = false;
            self.pending
                .push((binding.id.clone(), binding.shortcut.clone(), false));
        }
    }
}

struct Context {
    state: RefCell<State>,
    port: Cell<Option<NonNull<CFMachPort>>>,
}

unsafe extern "C-unwind" fn on_event(
    _: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    info: *mut c_void,
) -> *mut CGEvent {
    // The port only runs on the owning manager thread. Drop invalidates it
    // before freeing the context.
    let context = unsafe { &*info.cast::<Context>() };
    if matches!(
        kind,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    ) {
        context.state.borrow_mut().reset();
        if let Some(port) = context.port.get() {
            CGEvent::tap_enable(unsafe { port.as_ref() }, true);
        }
    } else if kind == CGEventType::FlagsChanged {
        let event = Some(unsafe { event.as_ref() });
        let key = CGEvent::integer_value_field(event, CGEventField::KeyboardEventKeycode) as u16;
        let own = CGEvent::integer_value_field(event, CGEventField::EventSourceUnixProcessID)
            == i64::from(std::process::id());
        if context
            .state
            .borrow_mut()
            .flags_changed(key, CGEvent::flags(event), own)
        {
            return std::ptr::null_mut();
        }
    }
    event.as_ptr()
}

pub(super) struct ModifierTap {
    port: CFRetained<CFMachPort>,
    context: Box<Context>,
}

impl ModifierTap {
    pub(super) fn new() -> Option<Self> {
        let context = Box::new(Context {
            state: RefCell::default(),
            port: Cell::new(None),
        });
        let run_loop = CFRunLoop::current()?;
        // Append after handy-keys so its keyed shortcuts still see modifiers
        // even when our modifier-only shortcut consumes the press.
        let port = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::TailAppendEventTap,
                CGEventTapOptions::Default,
                1 << CGEventType::FlagsChanged.0,
                Some(on_event),
                (&raw const *context).cast_mut().cast(),
            )
        }?;
        let source = CFMachPort::new_run_loop_source(None, Some(&port), 0)?;
        context.port.set(Some(NonNull::from(&*port)));
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopDefaultMode });
        CGEvent::tap_enable(&port, true);
        Some(Self { port, context })
    }

    pub(super) fn register(
        &self,
        modifiers: Modifiers,
        id: &str,
        shortcut: &str,
    ) -> Result<(), String> {
        let mut state = self.context.state.borrow_mut();
        if state.bindings.contains_key(&modifiers) || state.bindings.values().any(|b| b.id == id) {
            return Err(format!("Shortcut already registered: {shortcut}"));
        }
        state.bindings.insert(
            modifiers,
            Binding {
                id: id.into(),
                shortcut: shortcut.into(),
                held: false,
            },
        );
        Ok(())
    }

    pub(super) fn unregister(&self, id: &str) {
        self.context
            .state
            .borrow_mut()
            .bindings
            .retain(|_, b| b.id != id);
    }

    pub(super) fn poll(&self, timeout: std::time::Duration) -> Vec<(String, String, bool)> {
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            timeout.as_secs_f64(),
            true,
        );
        std::mem::take(&mut self.context.state.borrow_mut().pending)
    }
}

impl Drop for ModifierTap {
    fn drop(&mut self) {
        CFMachPort::invalidate(&self.port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn right_option() -> State {
        let mut state = State::default();
        state.bindings.insert(
            Modifiers::OPT_RIGHT,
            Binding {
                id: "transcribe".into(),
                shortcut: "option_right".into(),
                held: false,
            },
        );
        state
    }

    #[test]
    fn next_right_option_recovers_from_a_missed_command_release() {
        let mut state = right_option();
        state.flags_changed(55, Flags::MaskCommand, false);
        assert!(state.flags_changed(61, Flags::MaskAlternate, false));
        assert_eq!(state.tracked, Some(Modifiers::OPT_RIGHT));
        assert_eq!(state.pending.len(), 1);
        assert!(state.pending[0].2);
    }

    #[test]
    fn own_paste_events_do_not_reconcile_or_trigger_shortcuts() {
        let mut state = right_option();
        state.flags_changed(61, Flags::MaskAlternate, false);
        for (key, flags) in [
            (55, Flags::MaskCommand),
            (55, Flags::empty()),
            (61, Flags::empty()),
        ] {
            assert!(!state.flags_changed(key, flags, true));
        }
        assert_eq!(state.tracked, Some(Modifiers::OPT_RIGHT));
        assert_eq!(state.pending.len(), 1);
    }

    #[test]
    fn absence_of_alternate_releases_both_sides_once() {
        let mut state = right_option();
        state.flags_changed(61, Flags::MaskAlternate, false);
        state.flags_changed(58, Flags::MaskAlternate, false);
        assert_eq!(state.tracked, Some(Modifiers::OPT));
        for _ in 0..2 {
            assert!(!state.flags_changed(61, Flags::empty(), false));
        }
        assert_eq!(state.tracked, Some(Modifiers::empty()));
        assert_eq!(
            state.pending.iter().map(|e| e.2).collect::<Vec<_>>(),
            [true, false]
        );
    }

    #[test]
    fn native_callback_passes_own_keys_and_blocks_only_a_matching_press() {
        let context = Context {
            state: RefCell::new(right_option()),
            port: Cell::new(None),
        };
        let deliver = |key, flags, pid| {
            let event = CGEvent::new_keyboard_event(None, key, true).unwrap();
            CGEvent::set_type(Some(&event), CGEventType::FlagsChanged);
            CGEvent::set_flags(Some(&event), flags);
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::EventSourceUnixProcessID,
                pid,
            );
            unsafe {
                on_event(
                    std::ptr::null_mut(),
                    CGEventType::FlagsChanged,
                    NonNull::from(&*event),
                    (&raw const context).cast_mut().cast(),
                )
                .is_null()
            }
        };
        assert!(!deliver(55, Flags::MaskCommand, 0));
        assert!(!deliver(55, Flags::empty(), i64::from(std::process::id())));
        assert!(deliver(61, Flags::MaskAlternate, 0));
        assert!(!deliver(61, Flags::empty(), 0));
        let state = context.state.borrow();
        assert_eq!(
            state.pending.iter().map(|e| e.2).collect::<Vec<_>>(),
            [true, false]
        );
    }

    #[test]
    fn real_command_prevents_match_and_device_flags_identify_each_side() {
        let mut state = right_option();
        assert!(!state.flags_changed(61, Flags::MaskAlternate | Flags::MaskCommand, false));
        assert!(!state.flags_changed(61, Flags::MaskAlternate | Flags(0x20), false));
        assert_eq!(state.tracked, Some(Modifiers::OPT_LEFT));
        assert!(state.pending.is_empty());
    }
}
