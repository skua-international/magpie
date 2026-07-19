package tui

import (
	"os"
	"os/exec"

	tea "charm.land/bubbletea/v2"

	"github.com/skua-international/magpie/cli/internal/actions"
)

type adminActionDoneMsg struct {
	verb string // "edited the baseline config", "refreshed Steam auth"
	err  error
}

// editBaselineConfigMapCmd mirrors create_server.go's editConfigMapCmd,
// just against the chart-managed baseline instead of a per-server
// override -- same tea.ExecProcess suspend/resume, same `kubectl edit`.
func (m Model) editBaselineConfigMapCmd() tea.Cmd {
	cmd := actions.ConfigMapEditCmd(m.ctx, m.namespace, actions.BaselineConfigMapName(m.release))
	return tea.ExecProcess(cmd, func(err error) tea.Msg {
		return adminActionDoneMsg{verb: "edited the baseline config", err: err}
	})
}

// refreshSteamAuthCmd shells out to this same magpiectl binary's own
// `admin refresh-steam-auth` as a subprocess, rather than reimplementing
// its QR-code rendering and poll loop inline -- that flow does its own
// terminal output (a QR code, a "scan this" prompt) that doesn't fit
// either of this TUI's existing suspend patterns (tea.ExecProcess for a
// known interactive command like `kubectl edit`; a plain blocking
// tea.Cmd for account linking, which does its waiting silently). A
// subprocess invocation gets tea.ExecProcess's terminal handoff for
// free and reuses the real flow exactly, with zero duplication.
func (m Model) refreshSteamAuthCmd() tea.Cmd {
	exePath, err := os.Executable()
	if err != nil {
		return func() tea.Msg { return adminActionDoneMsg{verb: "refreshed Steam auth", err: err} }
	}
	cmd := exec.CommandContext(m.ctx, exePath, "admin", "refresh-steam-auth")
	return tea.ExecProcess(cmd, func(err error) tea.Msg {
		return adminActionDoneMsg{verb: "refreshed Steam auth", err: err}
	})
}

// handleAdminKey handles the Admin screen's own action keys.
func (m Model) handleAdminKey(msg tea.KeyPressMsg) (Model, tea.Cmd, bool) {
	switch msg.String() {
	case "e":
		m.status = ""
		return m, m.editBaselineConfigMapCmd(), true
	case "r":
		m.status = ""
		return m, m.refreshSteamAuthCmd(), true
	case "x":
		m.screen = screenAdminExportState
		m.adminState = adminStateState{path: "magpie-state.json"}
		return m, nil, true
	case "m":
		m.screen = screenAdminImportState
		m.adminState = adminStateState{}
		return m, nil, true
	}
	return m, nil, false
}
