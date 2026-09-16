use super::*;
use crate::terminal_transfer::{
    encode_cancel, encode_failure, parse_action_and_id, session_error, terminal_response,
    TransferControl, TransferOperation, MAX_TRANSFER_COMMAND_BYTES, MAX_TRANSFER_SESSIONS,
    OSC_TRANSFER_CONTROL_KIND, TRANSFER_DRAIN_TIMEOUT, TRANSFER_IDLE_TIMEOUT,
};

/// Ownership outlives pane focus and is never transferred to another connection.
#[derive(Debug)]
pub(super) struct TransferRoute {
    terminal_id: crate::terminal::TerminalId,
    source: crate::terminal_transfer::TransferSource,
    client_id: u64,
    touched: Instant,
    finishing: bool,
    retired: bool,
}

impl HeadlessServer {
    pub(super) fn forward_terminal_transfer(
        &mut self,
        pane_id: crate::layout::PaneId,
        source: crate::terminal_transfer::TransferSource,
        command: &[u8],
    ) {
        let Some((_, pane)) = self.app.find_pane(pane_id) else {
            return;
        };
        let terminal_id = pane.attached_terminal_id.clone();
        if !self
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .is_some_and(|runtime| source.matches(&runtime.transfer_source()))
        {
            return;
        }
        let Some((action, id)) = parse_action_and_id(command) else {
            return;
        };
        let now = Instant::now();
        if matches!(action.as_str(), "send" | "receive") {
            if self.handoff_in_progress || self.shutting_down {
                self.send_transfer_failure(&terminal_id, &id, "ENOTCONN:server is stopping");
                return;
            }
            if self.transfer_routes.contains_key(&id)
                || self.transfer_routes.len() >= MAX_TRANSFER_SESSIONS
            {
                self.send_transfer_failure(
                    &terminal_id,
                    &id,
                    "EBUSY:transfer session limit or id collision",
                );
                return;
            }
            let Some(client_id) = self.foreground_client_id.filter(|id| {
                self.clients.get(id).is_some_and(|client| {
                    client.is_active_shell_client()
                        && client.terminal_transfer
                        && client.writer.is_some()
                })
            }) else {
                self.send_transfer_failure(
                    &terminal_id,
                    &id,
                    "ENOTSUP:outer client does not support transfers",
                );
                return;
            };
            self.transfer_routes.insert(
                id.clone(),
                TransferRoute {
                    terminal_id: terminal_id.clone(),
                    source: source.clone(),
                    client_id,
                    touched: now,
                    finishing: false,
                    retired: false,
                },
            );
        }
        let Some(route) = self.transfer_routes.get_mut(&id) else {
            return;
        };
        if route.terminal_id != terminal_id || !route.source.matches(&source) || route.retired {
            return;
        }
        route.touched = now;
        route.finishing |= matches!(action.as_str(), "finish" | "cancel");
        let client_id = route.client_id;
        if self
            .send_transfer_control(client_id, &terminal_id, command, false)
            .is_err()
        {
            self.retire_terminal_transfer(&id, Some("EIO:outer terminal transfer queue is full"));
        }
    }

    fn send_transfer_control(
        &self,
        client_id: u64,
        terminal_id: &crate::terminal::TerminalId,
        command: &[u8],
        retire: bool,
    ) -> Result<(), ()> {
        let writer = self
            .clients
            .get(&client_id)
            .and_then(|client| client.writer.as_ref())
            .ok_or(())?;
        let mut control =
            TransferControl::new(&self.client_shell_boot_id, terminal_id.to_string(), command);
        control.retire = retire;
        let data = serde_json::to_string(&control).map_err(|_| ())?;
        let framed = Self::frame_server_message(&ServerMessage::EndpointControl {
            kind: OSC_TRANSFER_CONTROL_KIND.to_owned(),
            data,
        })
        .map_err(|_| ())?;
        if retire {
            // Cancellation must remain deliverable after the bounded data lane fills.
            // The client tombstones the route before any queued data is processed.
            writer.control.send(framed).map_err(|_| ())
        } else {
            writer.control.try_send_transfer(framed).map_err(|_| ())
        }
    }

    fn send_transfer_failure(
        &self,
        terminal_id: &crate::terminal::TerminalId,
        id: &str,
        message: &str,
    ) {
        let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) else {
            return;
        };
        if runtime
            .try_send_bytes(Bytes::from(encode_failure(id, message)))
            .is_err()
        {
            // Do not log TrySendError<Bytes>: it includes the transfer payload.
            warn!(%terminal_id, "terminal transfer failure could not reach the PTY");
        }
    }

    fn retire_terminal_transfer(&mut self, id: &str, error: Option<&str>) {
        let Some(route) = self
            .transfer_routes
            .get_mut(id)
            .filter(|route| !route.retired)
        else {
            return;
        };
        route.retired = true;
        route.touched = Instant::now();
        let (terminal_id, source, client_id) = (
            route.terminal_id.clone(),
            route.source.clone(),
            route.client_id,
        );
        if let Some(message) = error {
            if self
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .is_some_and(|runtime| source.matches(&runtime.transfer_source()))
            {
                self.send_transfer_failure(&terminal_id, id, message);
            }
        }
        let _ = self.send_transfer_control(client_id, &terminal_id, &encode_cancel(id), true);
    }

    pub(super) fn handle_terminal_transfer_request(
        &mut self,
        client_id: u64,
        operation: TransferOperation,
    ) -> Result<(), (&'static str, String)> {
        let (terminal_id, session_id) = match &operation {
            TransferOperation::Reply {
                terminal_id,
                session_id,
                ..
            }
            | TransferOperation::Cancel {
                terminal_id,
                session_id,
                ..
            } => (terminal_id, session_id),
        };
        let Some(route) = self.transfer_routes.get(session_id) else {
            return Err((
                "transfer_unavailable",
                "transfer session is unavailable".into(),
            ));
        };
        if route.client_id != client_id || route.terminal_id.as_str() != terminal_id {
            return Err((
                "stale_transfer",
                "transfer session belongs to another terminal or client".into(),
            ));
        }
        // Late packets from a retired route are consumed, never injected or resurrected.
        if route.retired {
            return Ok(());
        }
        if !self
            .app
            .terminal_runtimes
            .get(&route.terminal_id)
            .is_some_and(|runtime| route.source.matches(&runtime.transfer_source()))
        {
            self.retire_terminal_transfer(session_id, None);
            return Err(("stale_transfer", "terminal runtime was replaced".into()));
        }
        match operation {
            TransferOperation::Reply {
                session_id, data, ..
            } => {
                if data.len() > MAX_TRANSFER_COMMAND_BYTES.div_ceil(3) * 4 {
                    return Err(("invalid_transfer", "transfer response is too large".into()));
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|_| ("invalid_transfer", "transfer response is not base64".into()))?;
                if !terminal_response(&bytes, &session_id) {
                    return Err((
                        "invalid_transfer",
                        "expected one matching OSC 5113 response".into(),
                    ));
                }
                let ended = session_error(&bytes);
                let runtime = self
                    .app
                    .terminal_runtimes
                    .get(&route.terminal_id)
                    .ok_or_else(|| {
                        (
                            "transfer_unavailable",
                            "terminal runtime is unavailable".into(),
                        )
                    })?;
                if runtime.try_send_bytes(Bytes::from(bytes)).is_err() {
                    self.retire_terminal_transfer(
                        &session_id,
                        Some("EIO:terminal transfer input queue is full"),
                    );
                    return Err((
                        "transfer_backpressure",
                        "terminal input queue is full".into(),
                    ));
                }
                if ended {
                    // An overall error or CANCELED ends the session. File-specific errors do not.
                    self.retire_terminal_transfer(&session_id, None);
                } else if let Some(route) = self.transfer_routes.get_mut(&session_id) {
                    route.touched = Instant::now();
                }
            }
            TransferOperation::Cancel {
                session_id,
                message,
                ..
            } => {
                if message.len() > 256 {
                    return Err((
                        "invalid_transfer",
                        "cancellation message is too large".into(),
                    ));
                }
                self.retire_terminal_transfer(&session_id, Some(&format!("EIO:{message}")));
            }
        }
        Ok(())
    }

    pub(super) fn maintain_terminal_transfers(&mut self, now: Instant) {
        if self.transfer_routes.is_empty() {
            return;
        }
        let expired: Vec<_> = self
            .transfer_routes
            .iter()
            .filter_map(|(id, route)| {
                (!route.retired
                    && (!self
                        .app
                        .terminal_runtimes
                        .get(&route.terminal_id)
                        .is_some_and(|runtime| route.source.matches(&runtime.transfer_source()))
                        || now.saturating_duration_since(route.touched)
                            >= if route.finishing {
                                TRANSFER_DRAIN_TIMEOUT
                            } else {
                                TRANSFER_IDLE_TIMEOUT
                            }))
                .then_some((id.clone(), route.finishing))
            })
            .collect();
        for (id, finished) in expired {
            self.retire_terminal_transfer(
                &id,
                (!finished).then_some("ETIMEDOUT:terminal transfer expired"),
            );
        }
        self.transfer_routes.retain(|_, route| {
            !route.retired || now.saturating_duration_since(route.touched) < TRANSFER_DRAIN_TIMEOUT
        });
    }

    pub(super) fn cancel_terminal_transfers_for_client(&mut self, client_id: u64) {
        let ids: Vec<_> = self
            .transfer_routes
            .iter()
            .filter_map(|(id, route)| {
                (route.client_id == client_id && !route.retired).then_some(id.clone())
            })
            .collect();
        for id in ids {
            self.retire_terminal_transfer(&id, Some("ENOTCONN:transfer client disconnected"));
        }
    }
}
