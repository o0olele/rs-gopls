use std::io::{self, BufRead};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, bounded};
use lsp_server::{Connection, Message};
use lsp_types::notification::{DidChangeTextDocument, Exit, Notification as _};

use crate::rpc_telemetry;

pub(super) struct IoThreads {
    reader: JoinHandle<io::Result<()>>,
    writer: JoinHandle<io::Result<()>>,
    dropper: JoinHandle<()>,
    pub(super) notification_times: NotificationTimes,
}

pub(super) struct NotificationTimes(Receiver<Instant>);

impl NotificationTimes {
    pub(super) fn selected(&self, method: &str) -> Option<Instant> {
        if method != DidChangeTextDocument::METHOD {
            return None;
        }
        // The reader sends the timestamp before transferring the LSP message.
        self.0.try_recv().ok()
    }
}

pub(super) fn stdio() -> io::Result<(Connection, IoThreads)> {
    // Keep lsp-server's rendezvous channels and background message destruction.
    let (drop_sender, drop_receiver) = bounded(0);
    let (writer_sender, writer_receiver) = bounded::<Message>(0);
    let writer = thread::Builder::new()
        .name("LspServerWriter".into())
        .spawn(move || {
            let mut stdout = io::stdout().lock();
            writer_receiver.into_iter().try_for_each(|message| {
                let result = rpc_telemetry::write_message(&message, &mut stdout);
                let _ = drop_sender.send(message);
                result
            })
        })?;
    let dropper = thread::Builder::new()
        .name("LspMessageDropper".into())
        .spawn(move || {
            drop_receiver.into_iter().for_each(drop);
        })?;
    let (reader_sender, reader_receiver) = bounded(0);
    let (change_sender, change_receiver) = bounded(1);
    let reader = thread::Builder::new()
        .name("LspServerReader".into())
        .spawn(move || {
            read_messages(
                io::stdin().lock(),
                reader_sender,
                change_sender,
                Instant::now,
            )
        })?;
    Ok((
        Connection {
            sender: writer_sender,
            receiver: reader_receiver,
        },
        IoThreads {
            reader,
            writer,
            dropper,
            notification_times: NotificationTimes(change_receiver),
        },
    ))
}

fn read_messages(
    mut input: impl BufRead,
    messages: Sender<Message>,
    change_times: Sender<Instant>,
    mut now: impl FnMut() -> Instant,
) -> io::Result<()> {
    while let Some(message) = Message::read(&mut input)? {
        let exit = match &message {
            Message::Notification(notification)
                if notification.method == DidChangeTextDocument::METHOD =>
            {
                change_times.send(now()).map_err(io::Error::other)?;
                false
            }
            Message::Notification(notification) => notification.method == Exit::METHOD,
            _ => {
                rpc_telemetry::received(&message);
                false
            }
        };
        messages.send(message).map_err(io::Error::other)?;
        if exit {
            break;
        }
    }
    Ok(())
}

impl IoThreads {
    pub(super) fn join(self) -> io::Result<()> {
        fn joined<T>(thread: JoinHandle<T>) -> T {
            thread
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        }

        joined(self.reader)?;
        joined(self.dropper);
        joined(self.writer)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::time::Duration;

    use lsp_server::Notification;
    use lsp_types::notification::DidOpenTextDocument;
    use serde_json::Value;

    use super::*;

    #[test]
    fn decoded_changes_keep_their_timestamps_in_message_order() {
        let mut frames = Vec::new();
        for method in [
            DidChangeTextDocument::METHOD,
            DidOpenTextDocument::METHOD,
            DidChangeTextDocument::METHOD,
            DidChangeTextDocument::METHOD,
            Exit::METHOD,
        ] {
            Message::Notification(Notification::new(method.to_owned(), Value::Null))
                .write(&mut frames)
                .unwrap();
        }
        let (message_sender, messages) = bounded(0);
        let (change_sender, changes) = bounded(1);
        let (first_decoded, first_receipt) = bounded(0);
        let receipt = Instant::now();
        let reader = thread::spawn(move || {
            let mut times = [
                receipt - Duration::from_millis(25),
                receipt - Duration::from_millis(50),
                receipt - Duration::from_millis(75),
            ]
            .into_iter()
            .enumerate();
            read_messages(Cursor::new(frames), message_sender, change_sender, || {
                let (index, at) = times.next().expect("one timestamp per decoded change");
                if index == 0 {
                    first_decoded.send(at).unwrap();
                }
                at
            })
        });
        let changes = NotificationTimes(changes);
        assert_eq!(
            first_receipt.recv_timeout(Duration::from_secs(5)).unwrap(),
            receipt - Duration::from_millis(25)
        );
        let mut receipts = Vec::new();
        for message in messages {
            let Message::Notification(notification) = message else {
                panic!("expected a notification");
            };
            let receipt = changes.selected(&notification.method);
            if notification.method == DidChangeTextDocument::METHOD {
                receipts.push(receipt.unwrap());
            } else {
                assert!(receipt.is_none());
            }
        }
        assert_eq!(
            receipts,
            [25, 50, 75].map(|ms| receipt - Duration::from_millis(ms))
        );
        reader.join().unwrap().unwrap();
    }
}
