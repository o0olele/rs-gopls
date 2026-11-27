use std::io;
use std::thread::{self, JoinHandle};

use crossbeam_channel::bounded;
use lsp_server::{Connection, Message};
use lsp_types::notification::{Exit, Notification as _};

use crate::rpc_telemetry;

pub(super) struct IoThreads {
    reader: JoinHandle<io::Result<()>>,
    writer: JoinHandle<io::Result<()>>,
    dropper: JoinHandle<()>,
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
    let reader = thread::Builder::new()
        .name("LspServerReader".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            while let Some(message) = Message::read(&mut stdin)? {
                let exit = matches!(&message, Message::Notification(n) if n.method == Exit::METHOD);
                rpc_telemetry::received(&message);
                reader_sender.send(message).map_err(io::Error::other)?;
                if exit {
                    break;
                }
            }
            Ok(())
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
        },
    ))
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
