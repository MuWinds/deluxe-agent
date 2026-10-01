//! Runtime event delivery and GUI repaint adaptation.
//!
//! `EventSink` is the worker-facing part and carries no GUI handle. The
//! repaint bridge is a separate adapter that translates a neutral wake signal
//! into an egui repaint on the window thread.

use std::sync::mpsc::{Receiver, Sender};

use eframe::egui;
use tokio::sync::mpsc;

use crate::harness::{AgentEvent, AgentEventSink};
use crate::ipc::Event;

/// Sends runtime events to the GUI channel without owning GUI resources.
#[derive(Clone)]
pub struct EventSink {
    tx: mpsc::UnboundedSender<Event>,
    repaint_tx: Sender<()>,
}

impl EventSink {
    /// Creates an event sink and a neutral repaint wake channel.
    pub fn new(tx: mpsc::UnboundedSender<Event>) -> (Self, Receiver<()>) {
        let (repaint_tx, repaint_rx) = std::sync::mpsc::channel();
        (Self { tx, repaint_tx }, repaint_rx)
    }

    /// Publishes a UI event and asks the repaint adapter to wake the window.
    pub fn emit_ui(&self, event: Event) {
        if self.tx.send(event).is_ok() {
            let _ = self.repaint_tx.send(());
        }
    }
}

impl AgentEventSink for EventSink {
    fn emit(&self, event: AgentEvent) {
        self.emit_ui(event.into());
    }
}

/// Converts neutral worker wakeups into egui repaint requests.
pub struct RepaintSignal;

impl RepaintSignal {
    /// Starts the adapter thread and keeps it alive until the wake channel is
    /// closed.
    pub fn spawn(ctx: egui::Context, repaint_rx: Receiver<()>) {
        std::thread::spawn(move || {
            while repaint_rx.recv().is_ok() {
                ctx.request_repaint();
            }
        });
    }
}
