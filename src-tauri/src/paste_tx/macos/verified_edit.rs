//! Identify a paste in the focused editor without reading the surrounding draft.
//! AX ranges and character counts use UTF-16 offsets.

use objc2_core_foundation::{
    CFArray, CFGetTypeID, CFNumber, CFRange, CFRetained, CFString, CFType,
};
use std::ffi::c_void;
use std::ptr::{self, NonNull};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> *mut CFType;
    fn AXUIElementGetTypeID() -> usize;
    fn AXUIElementSetMessagingTimeout(element: &CFType, seconds: f32) -> i32;
    fn AXUIElementCopyAttributeValue(
        element: &CFType,
        attr: &CFString,
        out: *mut *mut CFType,
    ) -> i32;
    fn AXUIElementCopyMultipleAttributeValues(
        element: &CFType,
        attrs: &CFArray<CFString>,
        options: u32,
        out: *mut *mut CFArray<CFType>,
    ) -> i32;
    fn AXUIElementCopyParameterizedAttributeValue(
        element: &CFType,
        attr: &CFString,
        param: &CFType,
        out: *mut *mut CFType,
    ) -> i32;
    fn AXValueCreate(kind: u32, value: *const c_void) -> *mut CFType;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetValue(value: &CFType, kind: u32, out: *mut c_void) -> u8;
}

const AX_VALUE_CF_RANGE: u32 = 4;
// This is an optional shortcut. A busy or unsupported app should promptly fall back.
const AX_TIMEOUT_SECONDS: f32 = 0.010;

fn focused_element(app: &CFType) -> Option<CFRetained<CFType>> {
    let attr = CFString::from_static_str("AXFocusedUIElement");
    let mut out = ptr::null_mut();
    // SAFETY: app came from AXUIElementCreateApplication. Copy returns an owned CF object.
    unsafe {
        if AXUIElementCopyAttributeValue(app, &attr, &mut out) != 0 {
            return None;
        }
        let field = CFRetained::from_raw(NonNull::new(out)?);
        (CFGetTypeID(Some(&field)) == AXUIElementGetTypeID()
            && AXUIElementSetMessagingTimeout(&field, AX_TIMEOUT_SECONDS) == 0)
            .then_some(field)
    }
}

#[derive(Debug, PartialEq)]
struct EditorState {
    length: isize,
    selection: CFRange,
}

impl EditorState {
    fn read(field: &CFType) -> Option<Self> {
        let attrs = [
            "AXRole",
            "AXSubrole",
            "AXNumberOfCharacters",
            "AXSelectedTextRange",
        ]
        .map(CFString::from_static_str);
        let attrs = CFArray::from_retained_objects(&attrs);
        let mut out = ptr::null_mut();
        // SAFETY: field is a checked AXUIElement. With options=0 each slot
        // contains either a CF value or an AX error. Check its type before use.
        let values = unsafe {
            if AXUIElementCopyMultipleAttributeValues(field, &attrs, 0, &mut out) != 0 {
                return None;
            }
            CFRetained::from_raw(NonNull::new(out)?)
        };
        Self::from_attributes(&values)
    }

    fn from_attributes(values: &CFArray<CFType>) -> Option<Self> {
        let role = values.get(0)?.downcast::<CFString>().ok()?;
        if !matches!(role.to_string().as_str(), "AXTextArea" | "AXTextField")
            || values
                .get(1)?
                .downcast::<CFString>()
                .is_ok_and(|s| s.to_string() == "AXSecureTextField")
        {
            return None;
        }
        let length = values.get(2)?.downcast::<CFNumber>().ok()?.as_isize()?;
        let range = values.get(3)?;
        let mut selection = CFRange::new(0, 0);
        // SAFETY: AXValueGetValue validates the requested payload type.
        unsafe {
            if CFGetTypeID(Some(&range)) != AXValueGetTypeID()
                || AXValueGetValue(&range, AX_VALUE_CF_RANGE, (&raw mut selection).cast()) == 0
            {
                return None;
            }
        }
        Some(Self { length, selection })
    }

    fn after_paste(&self, inserted: isize) -> Option<Self> {
        let CFRange { location, length } = self.selection;
        // Equal-length replacements cannot be distinguished from selecting
        // existing text and moving the caret. Use normal receipt timing there.
        if inserted <= 0
            || location < 0
            || length < 0
            || location.checked_add(length)? > self.length
            || inserted == length
        {
            return None;
        }
        Some(Self {
            length: (self.length - length).checked_add(inserted)?,
            selection: CFRange::new(location.checked_add(inserted)?, 0),
        })
    }
}

pub(super) struct ExpectedEdit {
    app: CFRetained<CFType>,
    field: CFRetained<CFType>,
    after: EditorState,
    range: CFRange,
    text: CFRetained<CFString>,
}

impl ExpectedEdit {
    pub(super) fn capture(pid: i32, text: &str) -> Option<Self> {
        // SAFETY: Create returns an owned application AXUIElement.
        let app = unsafe { CFRetained::from_raw(NonNull::new(AXUIElementCreateApplication(pid))?) };
        if unsafe { AXUIElementSetMessagingTimeout(&app, AX_TIMEOUT_SECONDS) } != 0 {
            return None;
        }
        let field = focused_element(&app)?;
        let before = EditorState::read(&field)?;
        let text = CFString::from_str(text);
        let after = before.after_paste(text.length())?;
        let range = CFRange::new(before.selection.location, text.length());
        Some(Self {
            app,
            field,
            after,
            range,
            text,
        })
    }

    pub(super) fn same_focus(&self) -> Option<bool> {
        Some(self.field == focused_element(&self.app)?)
    }

    pub(super) fn is_visible(&self) -> bool {
        let Some(focused) = focused_element(&self.app) else {
            return false;
        };
        self.matches_field(&focused, EditorState::read, text_for_range)
    }

    fn matches_field(
        &self,
        field: &CFType,
        read_state: impl FnOnce(&CFType) -> Option<EditorState>,
        read_text: impl FnOnce(&CFType, CFRange) -> Option<CFRetained<CFString>>,
    ) -> bool {
        *self.field == *field
            && read_state(field).as_ref() == Some(&self.after)
            && read_text(field, self.range).is_some_and(|text| text == self.text)
    }
}

fn text_for_range(field: &CFType, range: CFRange) -> Option<CFRetained<CFString>> {
    let attr = CFString::from_static_str("AXStringForRange");
    let mut out = ptr::null_mut();
    // SAFETY: the range parameter is a CFRange, both Create and Copy
    // return owned CF objects, and the result is checked before comparison.
    unsafe {
        let range = AXValueCreate(AX_VALUE_CF_RANGE, (&raw const range).cast());
        let range = CFRetained::from_raw(NonNull::new(range)?);
        if AXUIElementCopyParameterizedAttributeValue(field, &attr, &range, &mut out) != 0 {
            return None;
        }
        NonNull::new(out)
            .map(|s| CFRetained::from_raw(s))
            .and_then(|s| s.downcast::<CFString>().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_caret_and_length_in_utf16_for_insertion_and_replacement() {
        let text = CFString::from_str("🦊e\u{301}");
        assert_eq!(text.length(), 4);
        for (total, start, selected, expected_len) in [(0, 0, 0, 4), (12, 2, 0, 16), (12, 2, 7, 9)]
        {
            let before = EditorState {
                length: total,
                selection: CFRange::new(start, selected),
            };
            let after = before.after_paste(text.length()).unwrap();
            assert_eq!(after.length, expected_len);
            assert_eq!(after.selection, CFRange::new(start + 4, 0));
        }
    }

    #[test]
    fn only_reads_the_inserted_range_of_the_same_field_after_the_expected_edit() {
        let field = CFString::from_str("first field");
        let other = CFString::from_str("second field");
        let text = CFString::from_str("🦊e\u{301}");
        let range = CFRange::new(2, text.length());
        let correct_state = || EditorState {
            length: 9,
            selection: CFRange::new(6, 0),
        };
        let edit = ExpectedEdit {
            app: field.clone().into(),
            field: field.clone().into(),
            after: correct_state(),
            text: text.clone(),
            range,
        };
        assert!(!edit.matches_field(
            &other,
            |_| panic!("wrong field"),
            |_, _| panic!("wrong field")
        ));
        for state in [
            None,
            Some(EditorState {
                length: 8,
                selection: CFRange::new(6, 0),
            }),
            Some(EditorState {
                length: 9,
                selection: CFRange::new(2, 4),
            }),
        ] {
            assert!(!edit.matches_field(&field, |_| state, |_, _| panic!("unconfirmed range")));
        }
        for (actual, matches) in [
            (Some(text.clone()), true),
            (Some(CFString::from_str("oops")), false),
            (None, false),
        ] {
            assert_eq!(
                edit.matches_field(
                    &field,
                    |_| Some(correct_state()),
                    |_, requested| {
                        assert_eq!(requested, range);
                        actual
                    }
                ),
                matches
            );
        }
    }

    #[test]
    fn ambiguous_replacements_and_invalid_ranges_use_normal_timing() {
        for (start, selected, added) in [
            (0, 2, 2),
            (1, 0, 0),
            (-1, 0, 1),
            (0, -1, 1),
            (2, 3, 1),
            (isize::MAX, 2, 1),
        ] {
            let state = EditorState {
                length: 4,
                selection: CFRange::new(start, selected),
            };
            assert!(state.after_paste(added).is_none());
        }
    }

    #[test]
    fn decodes_native_ax_values_and_excludes_passwords_and_non_editors() {
        let selection = CFRange::new(1, 2);
        let range = unsafe {
            CFRetained::from_raw(
                NonNull::new(AXValueCreate(
                    AX_VALUE_CF_RANGE,
                    (&raw const selection).cast(),
                ))
                .unwrap(),
            )
        };
        let size = CFNumber::new_isize(4);
        for (role, subrole, eligible) in [
            ("AXTextArea", "", true),
            ("AXTextField", "AXSearchField", true),
            ("AXTextField", "AXSecureTextField", false),
            ("AXButton", "", false),
        ] {
            let role = CFString::from_str(role);
            let subrole = CFString::from_str(subrole);
            let values = CFArray::<CFType>::from_objects(&[&role, &subrole, &size, &range]);
            let observed = EditorState::from_attributes(&values);
            assert_eq!(
                observed,
                eligible.then_some(EditorState {
                    length: 4,
                    selection
                })
            );
            let unsupported = CFArray::<CFType>::from_objects(&[&role, &subrole, &size, &subrole]);
            assert!(EditorState::from_attributes(&unsupported).is_none());
        }
    }
}
