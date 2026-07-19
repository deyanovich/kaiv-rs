//! kaiv-lsp — a thin stdio language server over the kaiv pipeline.
//! Diagnostics only: didOpen/didChange rerun the stage the document's
//! extension calls for and publish the first error as a whole-line
//! diagnostic (the pipeline is fail-fast and errors carry line
//! numbers, not columns). Full-text sync: kaiv documents are small.

mod diag;

use lsp_server::{Connection, Message, Notification};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
    PublishDiagnostics,
};
use lsp_types::{
    Diagnostic, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, PublishDiagnosticsParams, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};
use std::error::Error;

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    let (connection, io_threads) = Connection::stdio();
    let capabilities = serde_json::to_value(server_capabilities())?;
    connection.initialize(capabilities)?;
    main_loop(&connection)?;
    // The writer thread only terminates once every Sender is gone;
    // drop the connection before joining or join() never returns.
    drop(connection);
    io_threads.join()?;
    Ok(())
}

fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        ..Default::default()
    }
}

fn main_loop(connection: &Connection) -> Result<(), Box<dyn Error + Sync + Send>> {
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                // Diagnostics-only server: no requests beyond shutdown.
            }
            Message::Notification(note) => handle_notification(connection, note)?,
            Message::Response(_) => {}
        }
    }
    Ok(())
}

fn handle_notification(
    connection: &Connection,
    note: Notification,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    match note.method.as_str() {
        DidOpenTextDocument::METHOD => {
            if let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(note.params) {
                publish(connection, p.text_document.uri, &p.text_document.text)?;
            }
        }
        DidChangeTextDocument::METHOD => {
            if let Ok(mut p) = serde_json::from_value::<DidChangeTextDocumentParams>(note.params)
            {
                // FULL sync: the last change carries the whole text.
                if let Some(change) = p.content_changes.pop() {
                    publish(connection, p.text_document.uri, &change.text)?;
                }
            }
        }
        DidCloseTextDocument::METHOD => {
            if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(note.params) {
                send_diagnostics(connection, p.text_document.uri, Vec::new())?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn publish(
    connection: &Connection,
    uri: Uri,
    text: &str,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    if let Some(diagnostics) = diag::check(uri.as_str(), text) {
        send_diagnostics(connection, uri, diagnostics)?;
    }
    Ok(())
}

fn send_diagnostics(
    connection: &Connection,
    uri: Uri,
    diagnostics: Vec<Diagnostic>,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    let params = PublishDiagnosticsParams {
        uri,
        diagnostics,
        version: None,
    };
    connection
        .sender
        .send(Message::Notification(Notification::new(
            PublishDiagnostics::METHOD.to_string(),
            params,
        )))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_server::{Message, Request, RequestId};
    use serde_json::json;

    /// Drive the full JSON-RPC lifecycle over an in-memory transport:
    /// initialize, didOpen a broken document, expect one published
    /// diagnostic, then shut down cleanly.
    #[test]
    fn didopen_publishes_diagnostics() {
        let (server, client) = Connection::memory();
        let handle = std::thread::spawn(move || {
            let caps = serde_json::to_value(server_capabilities()).unwrap();
            server.initialize(caps).unwrap();
            main_loop(&server).unwrap();
        });

        client
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(1),
                "initialize".to_string(),
                json!({"capabilities": {}}),
            )))
            .unwrap();
        match client.receiver.recv().unwrap() {
            Message::Response(r) => assert!(r.error.is_none()),
            other => panic!("expected initialize response, got {other:?}"),
        }
        client
            .sender
            .send(Message::Notification(Notification::new(
                "initialized".to_string(),
                json!({}),
            )))
            .unwrap();

        client
            .sender
            .send(Message::Notification(Notification::new(
                "textDocument/didOpen".to_string(),
                json!({"textDocument": {
                    "uri": "file:///tmp/broken.kaiv",
                    "languageId": "kaiv",
                    "version": 1,
                    "text": "host=x\n!int[1;2]\n"
                }}),
            )))
            .unwrap();

        let note = loop {
            match client.receiver.recv().unwrap() {
                Message::Notification(n) if n.method == "textDocument/publishDiagnostics" => {
                    break n
                }
                _ => {}
            }
        };
        let params: PublishDiagnosticsParams = serde_json::from_value(note.params).unwrap();
        assert_eq!(params.diagnostics.len(), 1);
        assert_eq!(params.diagnostics[0].range.start.line, 1);
        assert_eq!(
            params.diagnostics[0].code,
            Some(lsp_types::NumberOrString::String(
                "INVALID_CONSTRAINT_ERROR".into()
            ))
        );

        client
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(2),
                "shutdown".to_string(),
                json!(null),
            )))
            .unwrap();
        match client.receiver.recv().unwrap() {
            Message::Response(r) => assert!(r.error.is_none()),
            other => panic!("expected shutdown response, got {other:?}"),
        }
        client
            .sender
            .send(Message::Notification(Notification::new(
                "exit".to_string(),
                json!(null),
            )))
            .unwrap();
        handle.join().unwrap();
    }
}
