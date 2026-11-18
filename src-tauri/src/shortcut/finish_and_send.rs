use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2_app_kit::NSWorkspace;
use objc2_core_foundation::{kCFRunLoopCommonModes, CFMachPort, CFRetained, CFRunLoop};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};
use tauri::{AppHandle, Manager};

use crate::TranscriptionCoordinator;

// Carbon virtual key codes for the main Return and keypad Enter keys.
const RETURN: u16 = 36;
const KEYPAD_ENTER: u16 = 76;
const SUBMIT_EVENT_MARKER: i64 = 0x4841_4e44_5953_454e;
static RECORDING: AtomicBool = AtomicBool::new(false);

struct Context {
    app: AppHandle,
    tap: Cell<Option<NonNull<CFMachPort>>>,
    // Keep swallowing the physical key's repeats and release after recording ends.
    swallowed: Cell<Option<u16>>,
}

struct Tap {
    port: CFRetained<CFMachPort>,
    _context: Box<Context>,
}

impl Drop for Tap {
    fn drop(&mut self) {
        // Stop callbacks before we free the context.
        CFMachPort::invalidate(&self.port);
    }
}

thread_local! {
    static TAP: RefCell<Option<Tap>> = const { RefCell::new(None) };
}

pub(super) fn set_recording(active: bool) {
    RECORDING.store(active, Ordering::SeqCst);
}

pub(crate) fn frontmost_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|app| app.processIdentifier())
        .filter(|pid| *pid > 0)
}

pub(crate) fn send_return(pid: i32) -> Result<(), String> {
    let down = CGEvent::new_keyboard_event(None, RETURN, true)
        .ok_or("Could not create Return key press")?;
    let up = CGEvent::new_keyboard_event(None, RETURN, false)
        .ok_or("Could not create Return key release")?;
    for event in [&*down, &*up] {
        // Right Option may still be physically held. Send an unmodified Return.
        CGEvent::set_flags(Some(event), CGEventFlags::empty());
        CGEvent::set_integer_value_field(
            Some(event),
            CGEventField::EventSourceUserData,
            SUBMIT_EVENT_MARKER,
        );
        CGEvent::post_to_pid(pid, Some(event));
    }
    Ok(())
}

unsafe extern "C-unwind" fn on_event(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    // The tap and callback run on the main run loop. Tap::drop invalidates
    // the port before dropping this context.
    let context = unsafe { &*user_info.cast::<Context>() };
    if matches!(
        kind,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    ) {
        context.swallowed.set(None);
        if let Some(port) = context.tap.get() {
            CGEvent::tap_enable(unsafe { port.as_ref() }, true);
        }
        return event.as_ptr();
    }

    let event_ref = Some(unsafe { event.as_ref() });
    if CGEvent::integer_value_field(event_ref, CGEventField::EventSourceUserData)
        == SUBMIT_EVENT_MARKER
    {
        return event.as_ptr();
    }
    let key = CGEvent::integer_value_field(event_ref, CGEventField::KeyboardEventKeycode) as u16;
    if key != RETURN && key != KEYPAD_ENTER {
        return event.as_ptr();
    }

    if context.swallowed.get() == Some(key) {
        if kind == CGEventType::KeyUp {
            context.swallowed.set(None);
        }
        return std::ptr::null_mut();
    }

    // Accept plain Return or Return while the dictation Option key is held.
    let other_modifiers =
        CGEventFlags::MaskCommand | CGEventFlags::MaskControl | CGEventFlags::MaskShift;
    if kind != CGEventType::KeyDown
        || CGEvent::flags(event_ref).intersects(other_modifiers)
        || CGEvent::integer_value_field(event_ref, CGEventField::KeyboardEventAutorepeat) != 0
        || !RECORDING.swap(false, Ordering::SeqCst)
    {
        return event.as_ptr();
    }

    // Swallow the user's press even if the target disappeared. Forwarding it
    // could send whatever text happened to be in the input before dictation.
    context.swallowed.set(Some(key));
    if let Some(coordinator) = context.app.try_state::<TranscriptionCoordinator>() {
        // PID zero will fail the focus check and prevent an accidental submit.
        coordinator.request_finish_and_send(frontmost_pid().unwrap_or(0));
    }
    std::ptr::null_mut()
}

pub(super) fn init(app: &AppHandle) {
    let context_app = app.clone();
    if let Err(error) = app.run_on_main_thread(move || {
        TAP.with(|slot| {
            if slot.borrow().is_some() {
                return;
            }
            let mut context = Box::new(Context {
                app: context_app,
                tap: Cell::new(None),
                swallowed: Cell::new(None),
            });
            let mask = (1 << CGEventType::KeyDown.0) | (1 << CGEventType::KeyUp.0);
            // The boxed context lives at a stable address until the port
            // has been invalidated.
            let Some(port) = (unsafe {
                CGEvent::tap_create(
                    CGEventTapLocation::SessionEventTap,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::Default,
                    mask,
                    Some(on_event),
                    (&mut *context as *mut Context).cast(),
                )
            }) else {
                log::warn!("Could not register Return to finish and send");
                return;
            };
            context.tap.set(Some(NonNull::from(&*port)));
            let tap = Tap {
                port,
                _context: context,
            };
            if let (Some(source), Some(run_loop)) = (
                CFMachPort::new_run_loop_source(None, Some(&tap.port), 0),
                CFRunLoop::current(),
            ) {
                run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
                CGEvent::tap_enable(&tap.port, true);
                slot.replace(Some(tap));
            } else {
                log::warn!("Could not attach the finish and send shortcut to the run loop");
            }
        });
    }) {
        log::warn!("Could not initialize the finish and send shortcut: {error}");
    }
}
