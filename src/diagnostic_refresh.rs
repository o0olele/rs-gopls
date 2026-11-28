use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use lsp_server::{Connection, Request, RequestId, Response};
use lsp_types::request::{Request as _, WorkspaceDiagnosticRefresh};
use serde_json::json;

use crate::scheduler::DiagnosticRefreshMark;
use crate::{rpc_telemetry, telemetry};

const MERGE_WINDOW: Duration = Duration::from_millis(75);
const MIN_INTERVAL: Duration = Duration::from_millis(250);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(super) struct DiagnosticRefresh {
    documents: HashMap<PathBuf, DiagnosticRefreshMark>,
    workspace_generation: Option<u64>,
    pending_since: Option<Instant>,
    last_sent: Option<Instant>,
    in_flight: Option<InFlight>,
    sequence: u64,
}

struct InFlight {
    id: RequestId,
    sequence: u64,
    sent: Instant,
}

impl DiagnosticRefresh {
    pub(super) fn document_changed(&mut self, mark: DiagnosticRefreshMark, now: Instant) {
        self.merge(now);
        if telemetry::enabled() {
            let mut fields = mark.telemetry_fields();
            fields["source"] = json!("document");
            fields["refresh_id"] = json!(self.sequence);
            fields["req_id"] = rpc_telemetry::request_id(&request_id(self.sequence));
            fields["replaced"] = json!(self.documents.contains_key(mark.path()));
            fields["coalesced"] =
                json!(!self.documents.is_empty() || self.workspace_generation.is_some());
            fields["in_flight"] = json!(self.in_flight.is_some());
            telemetry::emit("diagnostic_refresh_merge", fields);
        }
        self.documents.insert(mark.path().to_owned(), mark);
    }

    pub(super) fn workspace_changed(&mut self, generation: u64, now: Instant) {
        self.merge(now);
        telemetry::event!("diagnostic_refresh_merge", {
            "source": "workspace",
            "workspace_generation": generation,
            "refresh_id": self.sequence,
            "req_id": rpc_telemetry::request_id(&request_id(self.sequence)),
            "replaced": self.workspace_generation.is_some(),
            "coalesced": self.workspace_generation.is_some() || !self.documents.is_empty(),
            "in_flight": self.in_flight.is_some(),
        });
        self.workspace_generation = Some(generation);
    }

    fn merge(&mut self, now: Instant) {
        if self.pending_since.is_none() {
            self.sequence += 1;
            self.pending_since = Some(now);
        }
    }

    pub(super) fn response(&mut self, response: &Response) {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|pending| pending.id == response.id)
        {
            let pending = self.in_flight.take().unwrap();
            telemetry::event!("diagnostic_refresh_ack", {
                "refresh_id": pending.sequence,
                "req_id": rpc_telemetry::request_id(&pending.id),
                "ack_ok": response.error.is_none(),
                "code": response.error.as_ref().map(|error| error.code),
                "ack_ms": telemetry::elapsed(pending.sent),
            });
            if let Some(error) = &response.error {
                telemetry::event!("diagnostic_refresh_error", {
                    "refresh_id": pending.sequence,
                    "req_id": rpc_telemetry::request_id(&pending.id),
                    "code": error.code,
                });
                eprintln!(
                    "rs-gopls: diagnostic refresh returned error code {}",
                    error.code
                );
            }
        }
    }

    fn send_at(&self) -> Option<Instant> {
        self.pending_since.map(|since| {
            let merged = since + MERGE_WINDOW;
            self.last_sent
                .map_or(merged, |sent| merged.max(sent + MIN_INTERVAL))
        })
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        match &self.in_flight {
            Some(pending) => Some(pending.sent + RESPONSE_TIMEOUT),
            None => self.send_at(),
        }
    }

    pub(super) fn advance(
        &mut self,
        connection: &Connection,
        now: Instant,
        workspace_generation: u64,
        current: impl FnMut(&DiagnosticRefreshMark) -> bool,
    ) -> Result<()> {
        self.advance_with_clock(connection, now, workspace_generation, current, Instant::now)
    }

    fn advance_with_clock(
        &mut self,
        connection: &Connection,
        now: Instant,
        workspace_generation: u64,
        mut current: impl FnMut(&DiagnosticRefreshMark) -> bool,
        clock: impl FnOnce() -> Instant,
    ) -> Result<()> {
        if let Some(pending) = &self.in_flight {
            if now < pending.sent + RESPONSE_TIMEOUT {
                return Ok(());
            }
            let pending = self.in_flight.take().unwrap();
            telemetry::event!("diagnostic_refresh_timeout", {
                "refresh_id": pending.sequence,
                "req_id": rpc_telemetry::request_id(&pending.id),
                "timeout_ms": RESPONSE_TIMEOUT.as_millis(),
                "elapsed_ms": telemetry::millis(now.saturating_duration_since(pending.sent)),
            });
            eprintln!("rs-gopls: diagnostic refresh timed out");
        }
        if self.send_at().is_none_or(|deadline| now < deadline) {
            return Ok(());
        }
        let since = self.pending_since.take().unwrap();
        let workspace = self.workspace_generation.take() == Some(workspace_generation);
        let mut marks = telemetry::enabled().then(Vec::new);
        let candidate_count = self.documents.len();
        let mut documents = false;
        for (_, mark) in self.documents.drain() {
            if current(&mark) {
                documents = true;
                if let Some(marks) = &mut marks {
                    marks.push(mark.telemetry_fields());
                } else {
                    break;
                }
            }
        }
        if !workspace && !documents {
            telemetry::event!("diagnostic_refresh_drop", {
                "refresh_id": self.sequence,
                "req_id": rpc_telemetry::request_id(&request_id(self.sequence)),
                "document_count": candidate_count,
            });
            return Ok(());
        }

        let id = request_id(self.sequence);
        let sent = now.max(clock());
        let fields = marks.map(|marks| {
            json!({
                "refresh_id": self.sequence,
                "req_id": rpc_telemetry::request_id(&id),
                "documents": marks,
                "discarded_documents": candidate_count - marks.len(),
                "workspace": workspace,
                "merge_wait_ms": telemetry::millis(sent.saturating_duration_since(since)),
            })
        });
        rpc_telemetry::diagnostic_refresh(
            connection,
            Request::new(
                id.clone(),
                WorkspaceDiagnosticRefresh::METHOD.to_owned(),
                (),
            ),
            fields,
        )?;
        self.last_sent = Some(sent);
        self.in_flight = Some(InFlight {
            id,
            sequence: self.sequence,
            sent,
        });
        Ok(())
    }
}

fn request_id(sequence: u64) -> RequestId {
    RequestId::from(format!("rs-gopls/diagnostic-refresh/{sequence}"))
}

#[cfg(test)]
mod tests {
    use lsp_server::{ErrorCode, Message, Response};

    use super::*;

    fn request(client: &Connection) -> Request {
        let Message::Request(request) = client.receiver.try_recv().unwrap() else {
            panic!("expected a refresh request");
        };
        request
    }

    #[test]
    fn coalesces_sources_and_throttles_until_the_matching_response() {
        let (server, client) = Connection::memory();
        let start = Instant::now();
        let mut refresh = DiagnosticRefresh::default();
        refresh.workspace_changed(1, start);
        refresh.workspace_changed(2, start + Duration::from_millis(50));
        refresh
            .advance(
                &server,
                start + MERGE_WINDOW - Duration::from_millis(1),
                2,
                |_| false,
            )
            .unwrap();
        assert!(client.receiver.is_empty());
        let first_due = start + MERGE_WINDOW;
        let first_sent = first_due + Duration::from_millis(20);
        refresh
            .advance_with_clock(&server, first_due, 2, |_| false, || first_sent)
            .unwrap();
        let first = request(&client);

        refresh.workspace_changed(3, first_due + Duration::from_millis(1));
        refresh.response(&Response::new_ok("unrelated".to_owned().into(), ()));
        let second_due = first_sent + MIN_INTERVAL;
        refresh.advance(&server, second_due, 3, |_| false).unwrap();
        assert!(client.receiver.is_empty());

        let error = Response::new_err(
            first.id.clone(),
            ErrorCode::InternalError as i32,
            "client refused".into(),
        );
        refresh.response(&error);
        assert_eq!(refresh.deadline(), Some(second_due));
        refresh
            .advance(&server, second_due - Duration::from_millis(1), 3, |_| false)
            .unwrap();
        assert!(client.receiver.is_empty());
        refresh.advance(&server, second_due, 3, |_| false).unwrap();
        let second = request(&client);
        assert_ne!(second.id, first.id);
        refresh.response(&Response::new_ok(first.id, ()));
        assert_eq!(refresh.deadline(), Some(second_due + RESPONSE_TIMEOUT));
        refresh.response(&Response::new_ok(second.id, ()));
        assert_eq!(refresh.deadline(), None);
        assert!(client.receiver.is_empty());
    }

    #[test]
    fn times_out_without_retry_and_discards_an_overwritten_workspace_reason() {
        let (server, client) = Connection::memory();
        let start = Instant::now();
        let mut refresh = DiagnosticRefresh::default();
        refresh.workspace_changed(1, start);
        refresh
            .advance(&server, start + MERGE_WINDOW, 2, |_| false)
            .unwrap();
        assert!(client.receiver.is_empty());
        assert_eq!(refresh.deadline(), None);

        let now = start + MIN_INTERVAL;
        refresh.workspace_changed(2, now);
        refresh
            .advance(&server, now + MERGE_WINDOW, 2, |_| false)
            .unwrap();
        let request = request(&client);
        let timeout = refresh.deadline().unwrap();
        assert_eq!(timeout, now + MERGE_WINDOW + RESPONSE_TIMEOUT);
        refresh.advance(&server, timeout, 2, |_| false).unwrap();
        assert!(client.receiver.is_empty());
        assert_eq!(refresh.deadline(), None);
        refresh.response(&Response::new_ok(request.id, ()));
        assert_eq!(refresh.deadline(), None);
    }
}
