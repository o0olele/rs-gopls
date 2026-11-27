use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use lsp_server::{Connection, ErrorCode, Notification, RequestId, Response};
use lsp_types::{Location, Range, Uri};
use serde_json::Value;
use url::Url;

use crate::rpc_telemetry;

pub(crate) type RequestResult = std::result::Result<Value, (ErrorCode, String)>;

pub(crate) fn file_path(uri: &Uri) -> Result<PathBuf> {
    Url::parse(uri.as_str())?
        .to_file_path()
        .map_err(|()| anyhow!("expected a file URI, got {}", uri.as_str()))
}

pub(crate) fn path_uri(path: &Path) -> Result<Uri> {
    let url = Url::from_file_path(path)
        .map_err(|()| anyhow!("cannot convert {} to a file URI", path.display()))?;
    url.as_str().parse().context("parsing definition URI")
}

pub(super) fn internal_error(error: anyhow::Error) -> (ErrorCode, String) {
    (ErrorCode::InternalError, format!("{error:#}"))
}

pub(super) fn invalid_params(error: anyhow::Error) -> (ErrorCode, String) {
    (ErrorCode::InvalidParams, format!("{error:#}"))
}

pub(super) fn serialize_locations(locations: Vec<(PathBuf, Range)>) -> RequestResult {
    rpc_telemetry::serialize(|| {
        let locations = locations
            .into_iter()
            .map(|(path, range)| Ok(Location::new(path_uri(&path)?, range)))
            .collect::<Result<Vec<_>>>();
        serde_json::to_value(locations.map_err(internal_error)?)
            .map_err(|error| internal_error(error.into()))
    })
}

pub(super) fn serialize_location(location: Option<(PathBuf, Range)>) -> RequestResult {
    let Some((path, range)) = location else {
        return Ok(Value::Null);
    };
    rpc_telemetry::serialize(|| {
        let uri = path_uri(&path).map_err(internal_error)?;
        serde_json::to_value(Location::new(uri, range))
            .map_err(|error| internal_error(error.into()))
    })
}

pub(super) fn serialize_query(result: impl serde::Serialize) -> RequestResult {
    rpc_telemetry::serialize(|| {
        serde_json::to_value(result).map_err(|error| (ErrorCode::InternalError, error.to_string()))
    })
}

pub(super) fn send_response(
    connection: &Connection,
    id: RequestId,
    result: RequestResult,
) -> Result<()> {
    let response = match result {
        Ok(value) => Response {
            id,
            result: Some(value),
            error: None,
        },
        Err((code, message)) => Response::new_err(id, code as i32, message),
    };
    rpc_telemetry::responding(&response);
    connection
        .sender
        .send(response.into())
        .context("sending LSP response")
}

pub(super) fn send_notification<N: lsp_types::notification::Notification>(
    connection: &Connection,
    params: N::Params,
) -> Result<()> {
    connection
        .sender
        .send(Notification::new(N::METHOD.to_owned(), params).into())
        .with_context(|| format!("sending {}", N::METHOD))
}
