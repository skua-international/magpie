package tui

import (
	"fmt"

	tea "charm.land/bubbletea/v2"

	"github.com/skua-international/magpie/cli/internal/actions"
)

type serverActionDoneMsg struct {
	verb string // "started", "stopped", "resynced", "deleted"
	id   string
	err  error
}

func (m Model) startServerCmd(id string) tea.Cmd {
	return func() tea.Msg {
		_, err := actions.StartServer(m.ctx, m.clients, id)
		return serverActionDoneMsg{verb: "started", id: id, err: err}
	}
}

func (m Model) stopServerCmd(id string) tea.Cmd {
	return func() tea.Msg {
		_, err := actions.StopServer(m.ctx, m.clients, id)
		return serverActionDoneMsg{verb: "stopped", id: id, err: err}
	}
}

func (m Model) updateServerCmd(id string) tea.Cmd {
	return func() tea.Msg {
		_, err := actions.UpdateServer(m.ctx, m.clients, id)
		return serverActionDoneMsg{verb: "resynced", id: id, err: err}
	}
}

func (m Model) deleteServerCmd(id string) tea.Cmd {
	return func() tea.Msg {
		err := actions.DeleteServer(m.ctx, m.clients, id)
		return serverActionDoneMsg{verb: "deleted", id: id, err: err}
	}
}

// handleServersKey handles the servers list screen's own action keys --
// checked before the generic up/down list nav in handleKey. selectedID
// returns "" (a no-op) if the list is empty or hasn't loaded yet.
func (m Model) handleServersKey(msg tea.KeyPressMsg) (Model, tea.Cmd, bool) {
	id := m.selectedServerID()
	switch msg.String() {
	case "n":
		m.screen = screenServersCreate
		m.cursor = 0
		m.create = newCreateServerState()
		return m, nil, true
	case "s":
		if id == "" {
			return m, nil, true
		}
		m.status = ""
		return m, m.startServerCmd(id), true
	case "x":
		if id == "" {
			return m, nil, true
		}
		m.status = ""
		return m, m.stopServerCmd(id), true
	case "u":
		if id == "" {
			return m, nil, true
		}
		m.status = ""
		return m, m.updateServerCmd(id), true
	case "d":
		if id == "" {
			return m, nil, true
		}
		m.confirm = confirmState{kind: confirmDeleteServer, target: id, prompt: fmt.Sprintf("Delete server %s? [y/N]", id)}
		return m, nil, true
	}
	return m, nil, false
}

func (m Model) selectedServerID() string {
	if m.cursor < 0 || m.cursor >= len(m.servers) {
		return ""
	}
	return m.servers[m.cursor].Id
}
