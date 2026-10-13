use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui_kittest::Harness;
use serde_json::{Value, json};

use crate::{PdfCraftApp, control::ControlClient, open_worker};

fn fixture(pages: usize) -> Vec<u8> {
    let mut objects = vec!["<< /Type /Catalog /Pages 2 0 R >>".to_string()];
    let kids = (0..pages).map(|page| format!("{} 0 R", 4 + 2 * page)).collect::<Vec<_>>().join(" ");
    objects.push(format!("<< /Type /Pages /Kids [{kids}] /Count {pages} /MediaBox [0 0 200 300] >>"));
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());
    for page in 0..pages {
        objects.push(format!("<< /Type /Page /Parent 2 0 R /Contents {} 0 R /Resources << /Font << /F1 3 0 R >> >> >>", 5 + 2 * page));
        let body = format!("BT /F1 24 Tf 20 150 Td (Page {}) Tj ET", page + 1);
        objects.push(format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len()));
    }
    let mut bytes = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        bytes.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    let xref = bytes.len();
    bytes.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for offset in offsets {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());
    bytes
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
fn harness() -> (Harness<'static, PdfCraftApp>, ControlClient) {
    let slot: Arc<Mutex<Option<ControlClient>>> = Arc::default();
    let saved = slot.clone();
    let mut harness = Harness::builder().with_size(egui::vec2(1000.0, 700.0)).build_eframe(move |creation| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").expect("language preference");
        *saved.lock().expect("control-client slot") = Some(app.attach_control(&creation.egui_ctx));
        app.open_bytes("existing.pdf", None, fixture(3)).expect("existing document");
        app
    });
    harness.run_steps(3);
    (harness, slot.lock().expect("control-client slot").take().expect("control client"))
}

#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
fn request(harness: &mut Harness<'static, PdfCraftApp>, client: &ControlClient, method: &str, params: Value) -> Value {
    let reply = client.send(method, params);
    for _ in 0..5 {
        harness.step();
        if let Ok(result) = reply.try_recv() {
            return result.expect("control request succeeded");
        }
    }
    panic!("{method} was not answered during the held prepare phase");
}

#[test]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
fn open_prepare_keeps_control_and_existing_tabs_live_until_atomic_commit() {
    let (mut harness, client) = harness();
    let (gate, started, release, finished) = open_worker::prepare_gate();
    harness.state_mut().open_prepare_gate = Some(gate);

    let path = std::env::temp_dir().join(format!("pdfcraft-open-worker-{}.pdf", std::process::id()));
    std::fs::write(&path, fixture(2)).expect("write test PDF");
    let open_reply = request(&mut harness, &client, "ui.open", json!({ "path": path.to_string_lossy() }));
    assert_eq!(open_reply["pending"], true);
    started.recv_timeout(Duration::from_secs(5)).expect("worker reached prepare barrier");

    let state = request(&mut harness, &client, "ui.state", json!({}));
    assert_eq!(state["documents"].as_array().map(Vec::len), Some(1));
    assert_eq!(state["documents"][0]["name"], "existing.pdf");
    assert_eq!(state["pending_opens"].as_array().map(Vec::len), Some(1));
    request(&mut harness, &client, "ui.command", json!({ "id": "page.insert_blank" }));
    assert_eq!(harness.state().session.get(harness.state().views[0].id).expect("existing tab").info.pages.len(), 4);
    assert_eq!(harness.state().views.len(), 1, "prepared state is not visible in the session");

    release.send(()).expect("release prepare barrier");
    finished.recv_timeout(Duration::from_secs(5)).expect("worker finished preparation");
    harness.step();
    let app = harness.state();
    assert_eq!(app.views.len(), 2, "one prepared document commits exactly once");
    assert_eq!(app.session.get(app.views[1].id).expect("new tab").info.pages.len(), 2);
    assert_eq!(app.active, Some(1));
    let _ = std::fs::remove_file(path);
}

#[test]
fn inline_failed_tool_open_does_not_steal_an_existing_password_prompt() {
    let mut doc = pdfcraft_cos::Document::open(Arc::new(fixture(2))).unwrap();
    doc.set_encryption(&pdfcraft_cos::NewEncryption {
        algorithm: pdfcraft_cos::Algorithm::Aes256,
        user_password: "reader",
        owner_password: "owner",
        permissions: -4,
        encrypt_metadata: true,
        seed: [4; 32],
    })
    .unwrap();
    let bytes = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let path = std::env::temp_dir().join(format!("pdfcraft-inline-owner-{}-{nonce}.pdf", std::process::id()));
    assert!(!path.exists(), "the new opening attempt must fail reading");
    let mut h = Harness::builder().with_size(egui::vec2(1000.0, 700.0)).build_eframe(|_| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app.run_inline = true;
        app.open_bytes("locked.pdf", Some(path.to_string_lossy().into_owned()), bytes).unwrap();
        app.pick_override = Some(vec![path.to_string_lossy().into_owned()]);
        app
    });
    h.step();
    let cached = h.state().password_prompt.as_ref().unwrap().bytes.clone();
    assert!(h.state().password_prompt.as_ref().unwrap().then.is_none());
    h.state_mut().run_command("export.text");
    h.step();
    assert!(h.state().toast.as_ref().is_some_and(|toast| toast.0.starts_with("Couldn't read")));
    let prompt = h.state().password_prompt.as_ref().unwrap();
    assert!(Arc::ptr_eq(&prompt.bytes, &cached), "the older encrypted bytes remain available");
    assert!(prompt.then.is_none(), "a failed attempt must not attach its tool to the older prompt");
    h.state_mut().submit_password(Some("wrong".into()));
    assert!(h.state().password_prompt.as_ref().unwrap().then.is_none());
    h.state_mut().submit_password(Some("reader".into()));
    assert_eq!(h.state().views.len(), 1, "the older document can still be unlocked");
    assert!(h.state().password_prompt.is_none());
    h.run_steps(2);
    assert!(h.state().dialog.is_none(), "the failed attempt's tool never runs");

    // Accepting the current attempt must still run ordinary/conversion tools and retain
    // newly created password continuations through retries or discard them on cancellation.
    for case in ["plain", "converted", "password", "password-cancel"] {
        let path = path.with_extension(if case == "converted" { "txt" } else { "pdf" });
        let bytes = match case {
            "plain" => fixture(2),
            "converted" => b"Inline text converted to PDF".to_vec(),
            _ => cached.as_ref().clone(),
        };
        std::fs::write(&path, bytes).unwrap();
        let mut h = Harness::builder().with_size(egui::vec2(1000.0, 700.0)).build_eframe(|_| {
            let mut app = PdfCraftApp::new();
            app.set_option("language", "en").unwrap();
            app.run_inline = true;
            app.pick_override = Some(vec![path.to_string_lossy().into_owned()]);
            app
        });
        h.step();
        let defaults = h.state().persist();
        h.state_mut().run_command("export.text");
        h.step();
        if case.starts_with("password") {
            assert_eq!(h.state().password_prompt.as_ref().unwrap().then, Some("export.text"));
            h.state_mut().submit_password(Some("wrong".into()));
            assert_eq!(h.state().password_prompt.as_ref().unwrap().then, Some("export.text"));
            h.state_mut().submit_password((case == "password").then(|| "reader".into()));
        }
        assert_eq!(h.state().views.len(), usize::from(case != "password-cancel"));
        assert_eq!(matches!(h.state().dialog, Some(crate::Dialog::Export(_))), case != "password-cancel");
        assert!(h.state().password_prompt.is_none());
        assert_eq!(h.state().view_defaults.layout, PdfCraftApp::new().view_defaults.layout);
        assert_eq!(h.state().view_defaults.continuous, PdfCraftApp::new().view_defaults.continuous);
        // Persistence includes recent files; compare only the saved display defaults.
        let before: Value = serde_json::from_str(&defaults).unwrap();
        let after: Value = serde_json::from_str(&h.state().persist()).unwrap();
        assert_eq!(before["default_layout"], after["default_layout"]);
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn tool_open_continuation_follows_its_worker_and_password_retries() {
    for case in ["plain", "converted", "invalid-image", "password", "password-cancel", "wrong-password-cancel", "read-failure", "invalid-pdf"] {
        let extension = match case {
            "converted" => "txt",
            "invalid-image" => "png",
            _ => "pdf",
        };
        let path = std::env::temp_dir().join(format!("pdfcraft-tool-worker-{}-{case}.{extension}", std::process::id()));
        let encrypted = case.contains("password");
        if case != "read-failure" {
            let bytes = if encrypted {
                let mut doc = pdfcraft_cos::Document::open(Arc::new(fixture(2))).unwrap();
                doc.set_encryption(&pdfcraft_cos::NewEncryption {
                    algorithm: pdfcraft_cos::Algorithm::Aes256,
                    user_password: "reader",
                    owner_password: "owner",
                    permissions: -4,
                    encrypt_metadata: true,
                    seed: [4; 32],
                })
                .unwrap();
                pdfcraft_cos::write_full(&doc, &Default::default()).unwrap()
            } else if case == "converted" {
                b"A text file opened as a PDF".to_vec()
            } else if case == "invalid-image" {
                b"\x89PNG\r\n\x1a\n".to_vec()
            } else if case == "invalid-pdf" {
                b"not a PDF".to_vec()
            } else {
                fixture(2)
            };
            std::fs::write(&path, bytes).unwrap();
        }
        let mut h = Harness::builder().with_size(egui::vec2(1000.0, 700.0)).build_eframe(|_| {
            let mut app = PdfCraftApp::new();
            app.set_option("language", "en").unwrap();
            app.pick_override = Some(vec![path.to_string_lossy().into_owned()]);
            app
        });
        h.step();
        let (gate, started, release, finished) = open_worker::prepare_gate();
        h.state_mut().open_prepare_gate = Some(gate);
        h.state_mut().run_command("export.text");
        h.step();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(h.state().pending_opens.len(), 1);
        assert_eq!(h.state().pending_opens[0].then, Some("export.text"), "{case}: command belongs to the pending request");

        // A different document becoming active must neither run nor inherit the tool.
        h.state_mut().open_bytes("unrelated.pdf", None, fixture(1)).unwrap();
        let unrelated = h.state().views[0].id;
        h.step();
        assert!(h.state().dialog.is_none());
        release.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
        h.step();

        if encrypted {
            assert_eq!(h.state().password_prompt.as_ref().unwrap().then, Some("export.text"));
            assert!(h.state().dialog.is_none());
            if case != "password-cancel" {
                h.state_mut().submit_password(Some("wrong".into()));
                started.recv_timeout(Duration::from_secs(5)).unwrap();
                release.send(()).unwrap();
                finished.recv_timeout(Duration::from_secs(5)).unwrap();
                h.step();
                let prompt = h.state().password_prompt.as_ref().unwrap();
                assert!(prompt.error.is_some());
                assert_eq!(prompt.then, Some("export.text"));
                assert!(h.state().dialog.is_none());
            }
            if case == "password" {
                h.state_mut().submit_password(Some("reader".into()));
                started.recv_timeout(Duration::from_secs(5)).unwrap();
                release.send(()).unwrap();
                finished.recv_timeout(Duration::from_secs(5)).unwrap();
                h.step();
            } else {
                h.state_mut().submit_password(None);
            }
        }
        if matches!(case, "plain" | "converted" | "password") {
            assert_eq!(h.state().views.len(), 2);
            let app = h.state();
            let opened = &app.views[app.active.unwrap()];
            assert_ne!(opened.id, unrelated);
            assert_eq!(app.session.get(opened.id).unwrap().path.as_deref(), if case == "converted" { None } else { path.to_str() });
            assert!(matches!(app.dialog, Some(crate::Dialog::Export(_))));
            // Closing the dialog and processing more frames must not execute it again.
            h.state_mut().dialog = None;
        } else {
            assert_eq!(h.state().views.len(), 1);
            assert!(h.state().dialog.is_none());
        }
        assert!(h.state().pending_opens.is_empty());
        assert!(h.state().password_prompt.is_none());
        h.state_mut().open_bytes("later.pdf", None, fixture(1)).unwrap();
        h.run_steps(2);
        assert!(h.state().dialog.is_none(), "{case}: continuation cannot run on another open");
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn launch_layout_waits_for_cli_opens_and_preserves_restored_tabs() {
    use crate::canvas::PageLayout;
    // Controlled worker replies exercise both completion orders without scheduling races.
    for reverse in [false, true] {
        for count in [1, 2] {
            for restored in [false, true] {
                let mut app = PdfCraftApp::new();
                app.set_option("default-layout", "continuous").unwrap();
                if restored {
                    app.open_bytes("restored.pdf", None, fixture(1)).unwrap();
                }
                let offset = usize::from(restored);
                let mut replies = Vec::new();
                for token in 0..count {
                    let name = format!("cli-{token}.pdf");
                    let bytes = Arc::new(fixture(2));
                    let prepared = app.session.prepare_open(&name, None, bytes.clone(), None);
                    let (sender, receiver) = std::sync::mpsc::channel();
                    app.pending_opens.push(crate::PendingOpen {
                        token,
                        receiver,
                        name,
                        path: None,
                        stamp: None,
                        activate: true,
                        after_open: None,
                        recovered: None,
                        then: None,
                        display_override: Some((PageLayout::TwoUp, false)),
                    });
                    replies.push((
                        sender,
                        open_worker::ResultMessage { token, kind: open_worker::ResultKind::Prepared { bytes, result: Box::new(prepared) } },
                    ));
                }
                app.set_launch_layout("single").unwrap();
                if restored {
                    assert!(app.views[0].continuous(), "pending --layout must not target the restored tab");
                }
                if reverse {
                    replies.reverse();
                }
                for (index, (sender, reply)) in replies.into_iter().enumerate() {
                    let name = format!("cli-{}.pdf", reply.token);
                    sender.send(reply).unwrap();
                    app.poll_pending_opens();
                    let view = &app.views[index + offset];
                    assert_eq!(view.layout, if index + 1 == count as usize { PageLayout::Single } else { PageLayout::TwoUp });
                    assert!(!view.continuous());
                    assert_eq!(app.session.get(view.id).unwrap().name, name);
                }
                assert_eq!(app.active, Some(count as usize + offset - 1));
                if restored {
                    assert_eq!(app.views[0].layout, PageLayout::Single);
                    assert!(app.views[0].continuous());
                }
                assert_eq!(app.view_defaults.layout, PageLayout::Single);
                assert!(app.view_defaults.continuous);
                assert_eq!(serde_json::from_str::<Value>(&app.persist()).unwrap()["default_layout"], "continuous");
                // Ordinary later opens cannot inherit either launch override.
                app.open_bytes("later.pdf", None, fixture(1)).unwrap();
                assert!(app.views.last().unwrap().continuous());
            }
        }
    }
}

#[test]
fn launch_layout_failed_or_cancelled_opens_do_not_target_existing_documents() {
    use crate::canvas::PageLayout;
    for cancel in [false, true] {
        let mut app = PdfCraftApp::new();
        app.set_option("default-layout", "continuous").unwrap();
        app.open_bytes("restored.pdf", None, fixture(1)).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        app.pending_opens.push(crate::PendingOpen {
            token: 0,
            receiver,
            name: "cli.pdf".into(),
            path: None,
            stamp: None,
            activate: true,
            after_open: None,
            recovered: None,
            then: None,
            display_override: Some((PageLayout::TwoUp, false)),
        });
        app.set_launch_layout("single").unwrap();
        assert!(app.set_launch_layout("invalid").is_err());
        let kind = if cancel {
            open_worker::ResultKind::Prepared { bytes: Arc::new(fixture(1)), result: Box::new(Err(pdfcraft_render::OpenError::NeedsPassword)) }
        } else {
            open_worker::ResultKind::ReadFailed("synthetic read failure".into())
        };
        sender.send(open_worker::ResultMessage { token: 0, kind }).unwrap();
        app.poll_pending_opens();
        if cancel {
            app.submit_password(None);
        }
        assert!(app.launch_layout.is_none());
        assert_eq!(app.views.len(), 1);
        assert!(app.views[0].continuous());
        app.open_bytes("later.pdf", None, fixture(1)).unwrap();
        assert!(app.views[1].continuous());
    }
}
