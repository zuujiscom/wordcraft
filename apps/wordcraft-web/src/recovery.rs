//! Crash recovery in the browser: while the document has unsaved changes a copy is kept in
//! IndexedDB (craft-libs' `craft-web-recovery`: one store per tab session, held by a Web Lock so a
//! second tab leaves a running tab's copy alone, a write counted only once IndexedDB has committed
//! it). The desktop keeps its copies with a worker thread in a profile folder
//! (`wordcraft_engine::recovery`), which the browser doesn't have.
//!
//! Startup reads every copy before the app is built; the newest is reopened as an unsaved
//! document, the others stay stored. A copy goes when its document is saved, or replaced by
//! another one.

use craft_web_recovery::{Opened, Recovered, Snapshot, Store};
use wordcraft_ui_egui::WordApp;

/// The least time between two copies of a document being typed in (seconds): encoding runs on the
/// page.
const EVERY: f64 = 10.0;

/// What [`open`] found, for the app to be built with.
pub struct Startup {
    pub store: Store,
    pub recovered: Vec<Recovered>,
    pub warnings: Vec<String>,
}

pub async fn open() -> Startup {
    let Opened { store, recovered, warnings } = craft_web_recovery::open("wordcraft").await;
    Startup { store, recovered, warnings: warnings.iter().map(ToString::to_string).collect() }
}

/// Reopen the newest copy in `app` as an unsaved document → the store key it keeps living under
/// and a message for the status bar.
pub fn restore(app: &mut WordApp, recovered: &[Recovered]) -> Option<(u64, String, String)> {
    for r in recovered {
        let name = if r.name.contains('.') { r.name.clone() } else { format!("{}.docx", r.name) };
        match wordcraft_engine::io::open_bytes_in(&name, &r.bytes, None, &app.session.system_region) {
            Ok((doc, _)) => {
                app.session.set_document(doc);
                app.session.path = None;
                app.session.file_name = Some(r.name.clone());
                app.session.dirty = true;
                let others = recovered.len() - 1;
                let mut message = format!("Recovered “{}” from this browser. Save it to keep it.", r.name);
                if others > 0 {
                    message.push_str(&format!(" {others} older recovery cop{} kept.", if others == 1 { "y" } else { "ies" }));
                }
                return Some((app.session.document_id(), r.key.clone(), message));
            }
            Err(e) => log::warn!("recovery copy “{}” could not be opened: {e}", r.name),
        }
    }
    None
}

/// The copy of the open document, kept up to date once per frame.
pub struct WebRecovery {
    store: Store,
    /// The open document and its entry.
    current: Option<(u64, String)>,
    /// The revision last handed to the store, and the one it has committed.
    queued: Option<u64>,
    committed: Option<u64>,
    last: f64,
}

impl WebRecovery {
    pub fn new(store: Store, adopted: Option<(u64, String)>) -> Self {
        Self { store, current: adopted, queued: None, committed: None, last: f64::NEG_INFINITY }
    }

    pub fn tick(&mut self, app: &WordApp, ctx: &egui::Context) {
        let s = &app.session;
        for w in self.store.take_written() {
            if self.current.as_ref().is_some_and(|(_, k)| *k == w.key) {
                match w.result {
                    Ok(()) => self.committed = Some(w.revision),
                    Err(e) => {
                        log::warn!("recovery copy not written: {e}");
                        // Try the same revision again at the next interval.
                        self.queued = None;
                    }
                }
            }
        }
        // Another document replaced the one the copy was of.
        let id = s.document_id();
        if self.current.as_ref().is_some_and(|(d, _)| *d != id) {
            self.forget();
        }
        // A password-protected document never leaves a plain copy behind.
        if s.password.is_some() {
            self.forget();
            return;
        }
        if !s.dirty {
            self.forget();
            return;
        }
        let rev = s.rev();
        if self.queued == Some(rev) {
            return;
        }
        let now = ctx.input(|i| i.time);
        if now - self.last < EVERY {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(EVERY - (now - self.last)));
            return;
        }
        let name = s
            .file_name
            .clone()
            .or_else(|| s.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "Document.docx".into());
        let bytes = match wordcraft_engine::io::save_bytes("recovery.docx", &s.doc) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("recovery copy not encoded: {e}");
                self.last = now;
                return;
            }
        };
        let key = self.current.get_or_insert_with(|| (id, self.store.key(id))).1.clone();
        let path = s.path.as_ref().map(|p| p.to_string_lossy().to_string());
        match self.store.put(&key, rev, Snapshot { name: &name, path: path.as_deref(), meta: None, bytes: &bytes }) {
            Ok(()) => self.queued = Some(rev),
            Err(e) => log::warn!("recovery copy not stored: {e}"),
        }
        self.last = now;
    }

    /// The open document has nothing unsaved (or is gone): its copy is deleted.
    fn forget(&mut self) {
        if let Some((_, key)) = self.current.take() {
            self.store.remove(&key);
        }
        self.queued = None;
        self.committed = None;
    }
}
