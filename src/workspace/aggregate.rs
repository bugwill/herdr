use std::collections::HashMap;

use crate::detect::AgentState;
use crate::layout::PaneId;
use crate::pane::PaneState;
use crate::terminal::{TerminalId, TerminalState};

use super::{Tab, Workspace};

/// Detail info for a single pane, used by the agent detail panel.
pub struct PaneDetail {
    pub pane_id: PaneId,
    pub tab_idx: usize,
    pub agent_kind_label: Option<String>,
    pub state: AgentState,
    pub seen: bool,
    pub last_agent_state_change_seq: Option<u64>,
    pub tokens: HashMap<String, String>,
}

fn pane_attention_priority(state: AgentState, seen: bool) -> u8 {
    match (state, seen) {
        (AgentState::Blocked, _) => 4,
        (AgentState::Idle, false) => 3,
        (AgentState::Working, _) => 2,
        (AgentState::Idle, true) => 1,
        (AgentState::Unknown, _) => 0,
    }
}

fn aggregate_panes<'a>(
    panes: impl Iterator<Item = &'a PaneState>,
    terminals: &HashMap<TerminalId, TerminalState>,
) -> (AgentState, bool, bool) {
    let mut aggregate: Option<(AgentState, bool)> = None;
    let mut all_seen = true;
    let mut has_agent = false;

    for pane in panes {
        let Some(terminal) = terminals.get(&pane.attached_terminal_id) else {
            continue;
        };
        if !terminal.is_agent_terminal() {
            continue;
        }

        has_agent = true;
        all_seen &= pane.seen;
        let candidate = (terminal.state, pane.seen);
        let replace = aggregate.is_none_or(|current| {
            pane_attention_priority(candidate.0, candidate.1)
                > pane_attention_priority(current.0, current.1)
        });
        if replace {
            aggregate = Some(candidate);
        }
    }

    let (state, _) = aggregate.unwrap_or((AgentState::Unknown, true));
    (state, all_seen, has_agent)
}

impl Tab {
    pub fn aggregate_state(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
    ) -> (AgentState, bool) {
        let (state, all_seen, has_agent) = aggregate_panes(self.panes.values(), terminals);
        if has_agent {
            (state, all_seen)
        } else {
            (AgentState::Unknown, true)
        }
    }

    fn pane_details(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        tab_idx: usize,
    ) -> Vec<PaneDetail> {
        self.layout
            .pane_ids()
            .iter()
            .filter_map(|id| {
                let pane = self.panes.get(id)?;
                let terminal = terminals.get(&pane.attached_terminal_id)?;
                let agent_kind_label = terminal.effective_agent_label().map(str::to_string);
                if terminal.agent_name.is_none() && agent_kind_label.is_none() {
                    return None;
                }
                Some(PaneDetail {
                    pane_id: *id,
                    tab_idx,
                    agent_kind_label,
                    state: terminal.state,
                    seen: pane.seen,
                    last_agent_state_change_seq: terminal.last_agent_state_change_seq,
                    tokens: terminal.metadata_tokens.values(),
                })
            })
            .collect()
    }
}

impl Workspace {
    pub fn aggregate_state(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
    ) -> (AgentState, bool) {
        let (state, all_seen, has_agent) = aggregate_panes(
            self.tabs.iter().flat_map(|tab| tab.panes.values()),
            terminals,
        );
        if has_agent {
            (state, all_seen)
        } else {
            (AgentState::Unknown, true)
        }
    }

    pub fn pane_details(&self, terminals: &HashMap<TerminalId, TerminalState>) -> Vec<PaneDetail> {
        self.tabs
            .iter()
            .enumerate()
            .flat_map(|(tab_idx, tab)| tab.pane_details(terminals, tab_idx))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Direction;

    use super::*;
    use crate::detect::Agent;

    fn terminal_for_pane(ws: &Workspace, pane_id: PaneId) -> TerminalState {
        TerminalState::new(ws.terminal_id(pane_id).unwrap().clone(), "/tmp".into())
    }

    #[test]
    fn aggregate_state_all_unknown() {
        let ws = Workspace::test_new("test");
        let mut terminals = HashMap::new();
        let root = ws.tabs[0].root_pane;
        let terminal = terminal_for_pane(&ws, root);
        terminals.insert(terminal.id.clone(), terminal);
        let (state, seen) = ws.aggregate_state(&terminals);
        assert_eq!(state, AgentState::Unknown);
        assert!(seen);
    }

    #[test]
    fn aggregate_state_priority() {
        let mut ws = Workspace::test_new("test");
        let id2 = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != id2)
            .copied()
            .unwrap();
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.detected_agent = Some(Agent::Codex);
        root_terminal.state = AgentState::Idle;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut second_terminal = terminal_for_pane(&ws, id2);
        second_terminal.detected_agent = Some(Agent::Pi);
        second_terminal.state = AgentState::Working;
        terminals.insert(second_terminal.id.clone(), second_terminal);

        let (state, seen) = ws.aggregate_state(&terminals);

        assert_eq!(state, AgentState::Working);
        assert!(seen);
    }

    #[test]
    fn aggregate_state_done_unseen_beats_working() {
        let mut ws = Workspace::test_new("test");
        let id2 = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != id2)
            .copied()
            .unwrap();
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.detected_agent = Some(Agent::Codex);
        root_terminal.state = AgentState::Idle;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut second_terminal = terminal_for_pane(&ws, id2);
        second_terminal.detected_agent = Some(Agent::Pi);
        second_terminal.state = AgentState::Working;
        terminals.insert(second_terminal.id.clone(), second_terminal);
        let root = ws.tabs[0].panes.get_mut(&root_id).unwrap();
        root.seen = false;

        let (state, seen) = ws.aggregate_state(&terminals);

        assert_eq!(state, AgentState::Idle);
        assert!(!seen);
    }

    #[test]
    fn tab_seen_requires_every_agent_pane_to_be_seen() {
        let mut ws = Workspace::test_new("test");
        let second_id = ws.test_split(Direction::Horizontal);
        let first_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != second_id)
            .copied()
            .unwrap();
        let mut terminals = HashMap::new();

        let mut first_terminal = terminal_for_pane(&ws, first_id);
        first_terminal.detected_agent = Some(Agent::Codex);
        first_terminal.state = AgentState::Idle;
        terminals.insert(first_terminal.id.clone(), first_terminal);
        let mut second_terminal = terminal_for_pane(&ws, second_id);
        second_terminal.detected_agent = Some(Agent::Pi);
        second_terminal.state = AgentState::Idle;
        terminals.insert(second_terminal.id.clone(), second_terminal);

        ws.tabs[0].panes.get_mut(&first_id).unwrap().seen = true;
        ws.tabs[0].panes.get_mut(&second_id).unwrap().seen = false;
        assert_eq!(
            ws.tabs[0].aggregate_state(&terminals),
            (AgentState::Idle, false)
        );

        ws.tabs[0].panes.get_mut(&second_id).unwrap().seen = true;
        assert_eq!(
            ws.tabs[0].aggregate_state(&terminals),
            (AgentState::Idle, true)
        );
    }

    #[test]
    fn no_agent_panes_do_not_make_tab_or_workspace_unseen() {
        let mut ws = Workspace::test_new("test");
        let second_id = ws.test_split(Direction::Horizontal);
        let first_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != second_id)
            .copied()
            .unwrap();
        let mut terminals = HashMap::new();

        let mut first_terminal = terminal_for_pane(&ws, first_id);
        first_terminal.detected_agent = Some(Agent::Codex);
        first_terminal.state = AgentState::Idle;
        terminals.insert(first_terminal.id.clone(), first_terminal);
        let mut second_terminal = terminal_for_pane(&ws, second_id);
        second_terminal.state = AgentState::Idle;
        terminals.insert(second_terminal.id.clone(), second_terminal);

        ws.tabs[0].panes.get_mut(&first_id).unwrap().seen = true;
        ws.tabs[0].panes.get_mut(&second_id).unwrap().seen = false;
        assert_eq!(
            ws.tabs[0].aggregate_state(&terminals),
            (AgentState::Idle, true)
        );
        assert_eq!(ws.aggregate_state(&terminals), (AgentState::Idle, true));
    }

    #[test]
    fn workspace_seen_requires_every_agent_tab_to_be_seen() {
        let mut ws = Workspace::test_new("test");
        let second_tab = ws.test_add_tab(Some("second"));
        let first_id = ws.tabs[0].root_pane;
        let second_id = ws.tabs[second_tab].root_pane;
        let mut terminals = HashMap::new();

        let mut first_terminal = terminal_for_pane(&ws, first_id);
        first_terminal.detected_agent = Some(Agent::Codex);
        first_terminal.state = AgentState::Idle;
        terminals.insert(first_terminal.id.clone(), first_terminal);
        let mut second_terminal = terminal_for_pane(&ws, second_id);
        second_terminal.detected_agent = Some(Agent::Pi);
        second_terminal.state = AgentState::Idle;
        terminals.insert(second_terminal.id.clone(), second_terminal);

        ws.tabs[0].panes.get_mut(&first_id).unwrap().seen = true;
        ws.tabs[second_tab].panes.get_mut(&second_id).unwrap().seen = false;
        assert_eq!(ws.aggregate_state(&terminals), (AgentState::Idle, false));

        ws.tabs[second_tab].panes.get_mut(&second_id).unwrap().seen = true;
        assert_eq!(ws.aggregate_state(&terminals), (AgentState::Idle, true));
    }

    #[test]
    fn pane_details_use_tab_vector_index_not_stable_public_tab_number() {
        let mut ws = Workspace::test_new("test");
        let removed_tab = ws.test_add_tab(Some("removed"));
        let survivor_tab = ws.test_add_tab(Some("survivor"));
        let survivor_pane = ws.tabs[survivor_tab].root_pane;
        assert!(ws.close_tab(removed_tab));

        let mut terminals = HashMap::new();
        let mut terminal = terminal_for_pane(&ws, survivor_pane);
        terminal.detected_agent = Some(Agent::Codex);
        terminals.insert(terminal.id.clone(), terminal);

        let details = ws.pane_details(&terminals);
        let survivor = details
            .iter()
            .find(|detail| detail.pane_id == survivor_pane)
            .expect("surviving tab agent should be listed");

        assert_eq!(ws.tabs[1].number, 3);
        assert_eq!(survivor.tab_idx, 1);
    }
}
