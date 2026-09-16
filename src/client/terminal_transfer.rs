use std::collections::HashMap;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use base64::Engine as _;

use super::endpoint::{ClientEndpointId, EndpointRegistry, EndpointSendOutcome};
use crate::terminal_transfer::{
    encode_cancel, parse_action_and_id, session_error, terminal_response, TransferControl,
    TransferOperation, MAX_TRANSFER_SESSIONS, TRANSFER_DRAIN_TIMEOUT, TRANSFER_IDLE_TIMEOUT,
};

const MAX_PENDING_REPLIES: usize = 2048;
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
struct Owner {
    endpoint_id: ClientEndpointId,
    generation: u64,
    boot_id: String,
    terminal_id: String,
}

impl Owner {
    fn matches(
        &self,
        endpoint: &ClientEndpointId,
        generation: u64,
        control: &TransferControl,
    ) -> bool {
        self.endpoint_id == *endpoint
            && self.generation == generation
            && self.boot_id == control.boot_id
            && self.terminal_id == control.terminal_id
    }
}

#[derive(Debug)]
struct TransferRoute {
    owner: Owner,
    touched: Instant,
    finishing: bool,
    retired: bool,
}

#[derive(Debug)]
struct PendingReply {
    owner: Owner,
    session_id: String,
    sent: Instant,
    response: Vec<u8>,
}

#[derive(Debug, Default)]
pub(super) struct ClientTransferState {
    routes: HashMap<String, TransferRoute>,
    pending: HashMap<String, PendingReply>,
    next_request_id: u64,
    cleanup_on_drop: bool,
}

impl ClientTransferState {
    pub(super) fn new() -> Self {
        Self {
            routes: HashMap::new(),
            pending: HashMap::new(),
            next_request_id: 0,
            cleanup_on_drop: true,
        }
    }

    fn request(
        &mut self,
        owner: &Owner,
        operation: TransferOperation,
    ) -> (String, crate::protocol::ClientMessage) {
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let id = format!("terminal-transfer-{}", self.next_request_id);
        // This value consists only of serializable strings and tagged operations.
        let request =
            serde_json::json!({ "id": id, "method": "terminal.transfer", "params": operation })
                .to_string();
        (
            id,
            crate::protocol::ClientMessage::ClientShellEndpointRequest {
                boot_id: owner.boot_id.clone(),
                request,
            },
        )
    }

    fn notify_cancel(
        &mut self,
        owner: &Owner,
        id: &str,
        message: &str,
        endpoints: &mut EndpointRegistry,
    ) {
        if !endpoints.accepts(&owner.endpoint_id, owner.generation) {
            return;
        }
        let (_, request) = self.request(
            owner,
            TransferOperation::Cancel {
                terminal_id: owner.terminal_id.clone(),
                session_id: id.to_owned(),
                message: message.to_owned(),
            },
        );
        endpoints.send_to(&owner.endpoint_id, &request);
    }

    fn retire(
        &mut self,
        id: &str,
        error: Option<&str>,
        endpoints: &mut EndpointRegistry,
        output: &mut impl Write,
    ) -> io::Result<()> {
        let Some(route) = self.routes.get_mut(id).filter(|route| !route.retired) else {
            return Ok(());
        };
        route.retired = true;
        route.touched = Instant::now();
        let owner = route.owner.clone();
        self.pending.retain(|_, pending| pending.session_id != id);
        if let Some(message) = error {
            self.notify_cancel(&owner, id, message, endpoints);
        }
        output.write_all(&encode_cancel(id))?;
        output.flush()
    }

    pub(super) fn handle_control(
        &mut self,
        control: TransferControl,
        endpoint_id: ClientEndpointId,
        generation: u64,
        active: bool,
        endpoints: &mut EndpointRegistry,
        output: &mut impl Write,
    ) -> io::Result<()> {
        let Some(command) = control.decode_command() else {
            return Ok(());
        };
        let Some((action, id)) = parse_action_and_id(&command) else {
            return Ok(());
        };
        let owner = Owner {
            endpoint_id,
            generation,
            boot_id: control.boot_id.clone(),
            terminal_id: control.terminal_id.clone(),
        };
        if control.retire {
            // Control messages can overtake queued transfer data, including the start.
            if !self.routes.contains_key(&id) && self.routes.len() < MAX_TRANSFER_SESSIONS {
                self.routes.insert(
                    id.clone(),
                    TransferRoute {
                        owner: owner.clone(),
                        touched: Instant::now(),
                        finishing: false,
                        retired: true,
                    },
                );
            }
            if self.routes.get(&id).is_some_and(|route| {
                route
                    .owner
                    .matches(&owner.endpoint_id, generation, &control)
            }) {
                self.retire(&id, None, endpoints, output)?;
            }
            return Ok(());
        }
        if matches!(action.as_str(), "send" | "receive") {
            if !active
                || self.routes.contains_key(&id)
                || self.routes.len() >= MAX_TRANSFER_SESSIONS
            {
                self.notify_cancel(
                    &owner,
                    &id,
                    "outer transfer id is busy or endpoint is inactive",
                    endpoints,
                );
                return Ok(());
            }
            self.routes.insert(
                id.clone(),
                TransferRoute {
                    owner: owner.clone(),
                    touched: Instant::now(),
                    finishing: false,
                    retired: false,
                },
            );
        }
        let Some(route) = self.routes.get_mut(&id) else {
            return Ok(());
        };
        if route.retired
            || !route
                .owner
                .matches(&owner.endpoint_id, generation, &control)
        {
            return Ok(());
        }
        route.touched = Instant::now();
        route.finishing |= matches!(action.as_str(), "finish" | "cancel");
        output.write_all(&command)?;
        output.flush()
    }

    pub(super) fn handle_input(
        &mut self,
        command: &[u8],
        endpoints: &mut EndpointRegistry,
        output: &mut impl Write,
    ) -> io::Result<()> {
        let Some((_, id)) = parse_action_and_id(command) else {
            return Ok(());
        };
        if !terminal_response(command, &id) {
            return Ok(());
        }
        let Some(route) = self.routes.get(&id).filter(|route| !route.retired) else {
            return Ok(());
        };
        let owner = route.owner.clone();
        if !endpoints.accepts(&owner.endpoint_id, owner.generation)
            || self.pending.len() >= MAX_PENDING_REPLIES
        {
            return self.retire(
                &id,
                Some("transfer response connection is unavailable or congested"),
                endpoints,
                output,
            );
        }
        let (request_id, request) = self.request(
            &owner,
            TransferOperation::Reply {
                terminal_id: owner.terminal_id.clone(),
                session_id: id.clone(),
                data: base64::engine::general_purpose::STANDARD.encode(command),
            },
        );
        if endpoints.send_to(&owner.endpoint_id, &request) == EndpointSendOutcome::NotSent {
            return self.retire(&id, None, endpoints, output);
        }
        self.pending.insert(
            request_id,
            PendingReply {
                owner,
                session_id: id.clone(),
                sent: Instant::now(),
                response: Vec::new(),
            },
        );
        if let Some(route) = self.routes.get_mut(&id) {
            route.touched = Instant::now();
            route.retired |= session_error(command);
        }
        Ok(())
    }

    pub(super) fn has_response(&self, request_id: &str) -> bool {
        // Includes cancellation acknowledgements, which need no retained request state.
        request_id.starts_with("terminal-transfer-")
    }

    pub(super) fn handle_response(
        &mut self,
        origin: (&ClientEndpointId, u64, &str),
        request_id: &str,
        final_chunk: bool,
        data: &[u8],
        endpoints: &mut EndpointRegistry,
        output: &mut impl Write,
    ) -> io::Result<()> {
        let (endpoint_id, generation, boot_id) = origin;
        let Some(pending) = self.pending.get_mut(request_id) else {
            return Ok(());
        };
        if pending.owner.endpoint_id != *endpoint_id
            || pending.owner.generation != generation
            || pending.owner.boot_id != boot_id
        {
            return Ok(());
        }
        if pending.response.len().saturating_add(data.len()) <= 4096 {
            pending.response.extend_from_slice(data);
            if !final_chunk {
                return Ok(());
            }
            if serde_json::from_slice::<serde_json::Value>(&pending.response)
                .ok()
                .is_some_and(|value| {
                    value.get("id").and_then(|id| id.as_str()) == Some(request_id)
                        && value.get("result").is_some()
                        && value.get("error").is_none()
                })
            {
                self.pending.remove(request_id);
                return Ok(());
            }
        }
        let id = pending.session_id.clone();
        tracing::warn!("terminal transfer reply was rejected by its endpoint");
        self.pending.remove(request_id);
        self.retire(
            &id,
            Some("transfer response rejected by server"),
            endpoints,
            output,
        )
    }

    pub(super) fn maintain(
        &mut self,
        now: Instant,
        endpoints: &mut EndpointRegistry,
        output: &mut impl Write,
    ) -> io::Result<()> {
        if self.routes.is_empty() {
            return Ok(());
        }
        let stale: Vec<_> = self
            .routes
            .iter()
            .filter_map(|(id, route)| {
                (!route.retired
                    && (!endpoints.accepts(&route.owner.endpoint_id, route.owner.generation)
                        || now.saturating_duration_since(route.touched)
                            >= if route.finishing {
                                TRANSFER_DRAIN_TIMEOUT
                            } else {
                                TRANSFER_IDLE_TIMEOUT
                            }))
                .then_some((id.clone(), route.finishing))
            })
            .collect();
        for (id, finished) in stale {
            self.retire(
                &id,
                (!finished).then_some("transfer endpoint disconnected or timed out"),
                endpoints,
                output,
            )?;
        }
        let overdue: Vec<_> = self
            .pending
            .values()
            .filter(|pending| now.saturating_duration_since(pending.sent) >= REPLY_TIMEOUT)
            .map(|pending| pending.session_id.clone())
            .collect();
        for id in overdue {
            self.retire(
                &id,
                Some("transfer response acknowledgement timed out"),
                endpoints,
                output,
            )?;
        }
        self.routes.retain(|_, route| {
            !route.retired || now.saturating_duration_since(route.touched) < TRANSFER_DRAIN_TIMEOUT
        });
        self.pending.retain(|_, pending| {
            self.routes.contains_key(&pending.session_id)
                && now.saturating_duration_since(pending.sent) < REPLY_TIMEOUT
        });
        Ok(())
    }
}

impl Drop for ClientTransferState {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let mut output = io::stdout().lock();
            for (id, route) in &self.routes {
                if !route.retired {
                    let _ = output.write_all(&encode_cancel(id));
                }
            }
            let _ = output.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::endpoint::{EndpointNegotiation, EndpointTransport, ProfileId};
    use super::*;
    use crate::protocol::ClientMessage;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<ClientMessage>>>);
    impl EndpointTransport for Capture {
        fn send(&mut self, message: &ClientMessage) -> io::Result<()> {
            self.0.lock().unwrap().push(message.clone());
            Ok(())
        }
    }
    fn remote() -> ClientEndpointId {
        ClientEndpointId::Ssh(ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap())
    }
    fn start(state: &mut ClientTransferState, registry: &mut EndpointRegistry, out: &mut Vec<u8>) {
        state
            .handle_control(
                TransferControl::new("boot", "term", b"\x1b]5113;ac=send;id=s;pw=sha256:keep\x07"),
                ClientEndpointId::Local,
                1,
                true,
                registry,
                out,
            )
            .unwrap();
    }

    #[test]
    fn terminal_transfer_pins_replies_across_focus_and_rejects_endpoint_collision() {
        let local = Capture::default();
        let other = Capture::default();
        let mut registry = EndpointRegistry::new(local.clone(), 1, EndpointNegotiation::default());
        registry.insert(
            remote(),
            other.clone(),
            2,
            EndpointNegotiation::default(),
            true,
        );
        let mut state = ClientTransferState::default();
        let mut out = Vec::new();
        start(&mut state, &mut registry, &mut out);
        assert!(out
            .windows(b"pw=sha256:keep\x07".len())
            .any(|bytes| bytes == b"pw=sha256:keep\x07"));
        registry.set_surface_active(&ClientEndpointId::Local, false);
        let before = out.clone();
        state
            .handle_control(
                TransferControl::new("other", "otherterm", b"\x1b]5113;ac=receive;id=s\x07"),
                remote(),
                2,
                true,
                &mut registry,
                &mut out,
            )
            .unwrap();
        assert_eq!(out, before, "colliding start must never reach host");
        assert!(
            matches!(&other.0.lock().unwrap()[0], ClientMessage::ClientShellEndpointRequest { request, .. } if request.contains("cancel"))
        );
        state
            .handle_input(
                b"\x1b]5113;ac=status;id=s;st=T0s=\x07",
                &mut registry,
                &mut out,
            )
            .unwrap();
        assert_eq!(local.0.lock().unwrap().len(), 1);
        assert_eq!(state.pending.len(), 1);
        registry.insert(
            ClientEndpointId::Local,
            local.clone(),
            3,
            EndpointNegotiation::default(),
            true,
        );
        state
            .handle_input(
                b"\x1b]5113;ac=data;id=s;d=YWJj\x07",
                &mut registry,
                &mut out,
            )
            .unwrap();
        assert_eq!(
            local.0.lock().unwrap().len(),
            1,
            "old host reply must not reach new generation"
        );
        assert!(out.ends_with(&encode_cancel("s")));
        assert!(state.pending.is_empty());
    }

    #[test]
    fn terminal_transfer_reply_errors_and_pending_limit_abort_observably() {
        let transport = Capture::default();
        let mut registry =
            EndpointRegistry::new(transport.clone(), 1, EndpointNegotiation::default());
        let mut state = ClientTransferState::default();
        let mut out = Vec::new();
        start(&mut state, &mut registry, &mut out);
        let response = b"\x1b]5113;ac=status;id=s;st=T0s=\x07";
        state
            .handle_input(response, &mut registry, &mut out)
            .unwrap();
        let id = state.pending.keys().next().unwrap().clone();
        let error =
            serde_json::json!({"id":id,"error":{"code":"transfer_backpressure"}}).to_string();
        state
            .handle_response(
                (&ClientEndpointId::Local, 1, "boot"),
                &id,
                true,
                error.as_bytes(),
                &mut registry,
                &mut out,
            )
            .unwrap();
        assert!(out.ends_with(&encode_cancel("s")));
        assert!(state.routes["s"].retired);
        assert!(transport.0.lock().unwrap().len() >= 2);
        let mut state = ClientTransferState::default();
        start(&mut state, &mut registry, &mut out);
        for _ in 0..=MAX_PENDING_REPLIES {
            state
                .handle_input(response, &mut registry, &mut out)
                .unwrap();
        }
        assert!(state.pending.is_empty());
        assert!(state.routes["s"].retired);
    }

    #[test]
    fn terminal_transfer_finish_and_cancel_keep_late_replies_out_of_new_sessions() {
        let mut registry =
            EndpointRegistry::new(Capture::default(), 1, EndpointNegotiation::default());
        let mut state = ClientTransferState::default();
        let mut out = Vec::new();
        start(&mut state, &mut registry, &mut out);
        state
            .handle_control(
                TransferControl::new("boot", "term", b"\x1b]5113;ac=finish;id=s\x07"),
                ClientEndpointId::Local,
                1,
                false,
                &mut registry,
                &mut out,
            )
            .unwrap();
        assert!(!state.routes["s"].retired);
        state
            .handle_input(
                &crate::terminal_transfer::encode_failure("s", "EIO:late commit error"),
                &mut registry,
                &mut out,
            )
            .unwrap();
        assert_eq!(
            state.pending.len(),
            1,
            "late finish error still reaches original process"
        );
        assert!(state.routes["s"].retired);
        state
            .maintain(
                Instant::now() + TRANSFER_DRAIN_TIMEOUT + Duration::from_secs(1),
                &mut registry,
                &mut out,
            )
            .unwrap();
        assert!(state.routes.is_empty());
        assert!(state.pending.is_empty());
    }

    #[test]
    fn terminal_transfer_retirement_overtaking_queued_start_cannot_reopen_host_session() {
        let mut registry =
            EndpointRegistry::new(Capture::default(), 1, EndpointNegotiation::default());
        let mut state = ClientTransferState::default();
        let mut out = Vec::new();
        let mut control = TransferControl::new("boot", "term", &encode_cancel("s"));
        control.retire = true;
        state
            .handle_control(
                control,
                ClientEndpointId::Local,
                1,
                true,
                &mut registry,
                &mut out,
            )
            .unwrap();
        start(&mut state, &mut registry, &mut out);
        assert!(
            out.is_empty(),
            "priority cancellation must prevent an earlier queued start reaching host"
        );
        assert!(state.routes["s"].retired);
    }
}
