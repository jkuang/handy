//! Run with cargo test --lib benchmark_return_to_send -- --ignored --nocapture
use super::*;
use crate::paste_tx::macos::{
    submit_receipt, AfterPaste, EditCheck, SendTarget, SubmitTiming, POLL_INTERVAL,
};
use crate::paste_tx::{evaluate, TxState, WaitDecision, CHORD_HOLD_MS};
use objc2_core_graphics::CGEvent;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

struct Editor {
    process: Child,
    output: BufReader<ChildStdout>,
}

impl Editor {
    fn start(dir: &std::path::Path) -> Self {
        let receiver = dir.join("editor");
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/paste_tx/macos/verified_edit/editor.m");
        assert!(Command::new("clang")
            .args([
                "-fobjc-arc",
                "-Wno-deprecated-declarations",
                "-framework",
                "AppKit",
                "-framework",
                "ApplicationServices",
                "-o"
            ])
            .arg(&receiver)
            .arg(source)
            .status()
            .unwrap()
            .success());
        let mut process = Command::new(receiver)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = BufReader::new(process.stdout.take().unwrap());
        Self { process, output }
    }

    fn request(&mut self, message: Value) -> Value {
        self.request_with_receipts(message, || panic!("unexpected pasteboard read"))
    }

    fn request_with_receipts(&mut self, message: Value, mut receipt: impl FnMut()) -> Value {
        let input = self.process.stdin.as_mut().unwrap();
        writeln!(input, "{message}").unwrap();
        input.flush().unwrap();
        let mut reply = String::new();
        loop {
            reply.clear();
            assert!(
                self.output.read_line(&mut reply).unwrap() > 0,
                "editor exited on {message}"
            );
            if reply.trim_end() == "READ" {
                receipt();
            } else {
                return serde_json::from_str(&reply).unwrap();
            }
        }
    }

    fn state(&mut self) -> Option<EditorState> {
        let reply = self.request(json!({"op":"state"}));
        let attrs = reply.as_array()?;
        let number = |i: usize| isize::try_from(attrs[i].as_i64()?).ok();
        Some(EditorState {
            length: number(0)?,
            selection: CFRange::new(number(1)?, number(2)?),
        })
    }

    fn send_return(&mut self) {
        crate::shortcut::finish_and_send::post_return_events(|event| {
            let bytes = CGEvent::new_data(None, Some(event)).unwrap().to_vec();
            self.request(json!({"op":"event", "bytes":bytes}));
        })
        .unwrap();
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        drop(self.process.stdin.take());
        let _ = self.process.wait();
    }
}

#[derive(Debug, Copy, Clone)]
enum Case {
    Immediate,
    RepeatedRead,
    DeferredInsert,
    MissingAttributes,
    Transformed,
}

fn trial(editor: &RefCell<Editor>, case: Case, enabled: bool, round: usize) -> (f64, bool) {
    let transcript = format!("dictation 🦊 {round}");
    let actual = if matches!(case, Case::Transformed) {
        transcript.to_uppercase()
    } else {
        transcript.clone()
    };
    editor
        .borrow_mut()
        .request(json!({"op":"begin", "draft":"draft 🦊 tail", "start":6,
        "length":2, "text":actual, "supportsLength":!matches!(case, Case::MissingAttributes)}));

    // The receiver starts its clock before preflight and stops it inside keyDown.
    let identity: CFRetained<CFType> = CFString::from_str("fixture field").into();
    let edit = enabled
        .then(|| editor.borrow_mut().state())
        .flatten()
        .and_then(|before| {
            ExpectedEdit::from_editor(identity.clone(), identity.clone(), before, &transcript)
        });
    let mut edit_check = EditCheck::new(edit.is_some());
    let mut tx = TxState::new();
    let injected = Instant::now();
    tx.injected_at = Some(injected);
    let mut action = AfterPaste::FinishAndSend(SendTarget {
        pid: editor.borrow().process.id() as i32,
        session: 1,
        cancel_generation: 0,
    });

    // Replay the chord hold and first-read timing observed in Handy's log.
    // AppKit reads a private pasteboard. Only a real read advances the receipt.
    std::thread::sleep(Duration::from_millis(CHORD_HOLD_MS + 7));
    let delayed = matches!(case, Case::RepeatedRead | Case::DeferredInsert);
    let first = if delayed { "read" } else { "paste" };
    editor
        .borrow_mut()
        .request_with_receipts(json!({"op":first}), || tx.record_receipt(Instant::now()));
    let first_read = tx
        .last_receipt_after_injection()
        .expect("AppKit read the promised text");
    let mut pasted = !delayed;
    let mut fast = false;
    // In Handy the waiter starts when the chord returns. The first read can
    // arrive between that point and the waiter's first 15 ms poll.
    let mut tick = injected + Duration::from_millis(CHORD_HOLD_MS) + POLL_INTERVAL;
    loop {
        std::thread::sleep(tick.saturating_duration_since(Instant::now()));
        let now = Instant::now();
        tick = now + POLL_INTERVAL;
        assert!(
            now.duration_since(injected) < Duration::from_secs(4),
            "transaction hung"
        );
        if !pasted && now.duration_since(first_read) >= Duration::from_millis(55) {
            let op = if matches!(case, Case::RepeatedRead) {
                "paste"
            } else {
                "apply_read"
            };
            editor
                .borrow_mut()
                .request_with_receipts(json!({"op":op}), || tx.record_receipt(Instant::now()));
            assert_eq!(
                tx.receipts.len(),
                if matches!(case, Case::RepeatedRead) {
                    2
                } else {
                    1
                }
            );
            pasted = true;
        }
        let settled = matches!(evaluate(&tx, Instant::now()), WaitDecision::Finish);
        let timing = if settled {
            SubmitTiming::Settlement
        } else if edit_check.due(
            Instant::now(),
            matches!(action, AfterPaste::FinishAndSend(_)) && submit_receipt(&tx).is_some(),
        ) {
            SubmitTiming::VerifiedEdit
        } else {
            continue;
        };
        let visible = timing == SubmitTiming::VerifiedEdit
            && edit.as_ref().is_some_and(|edit| {
                edit.matches_field(
                    &identity,
                    |_| editor.borrow_mut().state(),
                    |_, range| {
                        let reply = editor.borrow_mut().request(
                            json!({"op":"slice", "start":range.location, "length":range.length}),
                        );
                        Some(CFString::from_str(reply.as_str()?))
                    },
                )
            });
        let same_focus = edit.as_ref().map(|_| true);
        if action.claim_return(timing, same_focus, visible).is_some() {
            fast = timing == SubmitTiming::VerifiedEdit;
            editor.borrow_mut().send_return();
        }
        if settled {
            break;
        }
    }

    let result = editor.borrow_mut().request(json!({"op":"result"}));
    assert_eq!(result["downs"], 1);
    assert_eq!(result["ups"], 1);
    assert_eq!(result["text"], format!("draft {actual} tail"));
    if !enabled || matches!(case, Case::MissingAttributes | Case::Transformed) {
        assert!(!fast, "unverified early Return in {case:?}");
    }
    (result["ms"].as_f64().unwrap(), fast)
}

#[test]
#[ignore = "macOS AppKit receiver timing benchmark"]
fn benchmark_return_to_send() {
    let dir = tempfile::tempdir().unwrap();
    let editor = RefCell::new(Editor::start(dir.path()));
    const PAIRS: usize = 30;
    for case in [
        Case::Immediate,
        Case::RepeatedRead,
        Case::DeferredInsert,
        Case::MissingAttributes,
        Case::Transformed,
    ] {
        let mut old = Vec::new();
        let mut new = Vec::new();
        let mut savings = Vec::new();
        let mut early = 0;
        for i in 0..PAIRS {
            let mut times = [0.0; 2];
            for enabled in [i % 2 == 0, i % 2 != 0] {
                let (time, fast) = trial(&editor, case, enabled, i);
                times[usize::from(enabled)] = time;
                early += usize::from(fast);
            }
            old.push(times[0]);
            new.push(times[1]);
            savings.push(times[0] - times[1]);
        }
        for series in [&mut old, &mut new, &mut savings] {
            series.sort_by(f64::total_cmp);
        }
        let median = |v: &[f64]| (v[14] + v[15]) / 2.0;
        println!("{case:?}: {PAIRS} pairs, old p50/p95 {:.1}/{:.1} ms, new {:.1}/{:.1} ms, median paired saving {:.1} ms, early {early}/{PAIRS}", median(&old), old[28], median(&new), new[28], median(&savings));
    }
}
