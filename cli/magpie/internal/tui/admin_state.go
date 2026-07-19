package tui

import (
	"fmt"
	"strings"

	tea "charm.land/bubbletea/v2"

	"github.com/skua-international/magpie/cli/internal/actions"
)

// adminStateState drives both screenAdminExportState and
// screenAdminImportState -- same single "file path" field either way,
// just a different action on enter and a different default/placeholder.
type adminStateState struct {
	path string
	err  error
}

type stateExportedMsg struct {
	path       string
	modSources int
	configMaps int
	servers    int
	warnings   []string
	err        error
}

type stateImportedMsg struct {
	warnings []string
	err      error
}

func (m Model) exportStateCmd(path string) tea.Cmd {
	return func() tea.Msg {
		state, err := actions.ExportState(m.ctx, m.clients)
		if err != nil {
			return stateExportedMsg{err: err}
		}
		if err := actions.WriteStateFile(path, state); err != nil {
			return stateExportedMsg{err: err}
		}
		return stateExportedMsg{
			path:       path,
			modSources: len(state.ModSources),
			configMaps: len(state.ConfigMaps),
			servers:    len(state.Servers),
			warnings:   state.Warnings,
		}
	}
}

func (m Model) importStateCmd(path string) tea.Cmd {
	return func() tea.Msg {
		req, err := actions.ReadStateFile(path)
		if err != nil {
			return stateImportedMsg{err: err}
		}
		resp, err := actions.ImportState(m.ctx, m.clients, req)
		if err != nil {
			return stateImportedMsg{err: err}
		}
		return stateImportedMsg{warnings: resp.Warnings}
	}
}

func (m Model) handleAdminStateKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "enter":
		path := strings.TrimSpace(m.adminState.path)
		if path == "" {
			return m, nil
		}
		if m.screen == screenAdminExportState {
			return m, m.exportStateCmd(path)
		}
		return m, m.importStateCmd(path)
	case "backspace":
		m.adminState.path = trimLastRune(m.adminState.path)
	default:
		if msg.Text != "" {
			m.adminState.path += msg.Text
		}
	}
	return m, nil
}

func (m Model) viewAdminExportState() string {
	var b strings.Builder
	b.WriteString(titleStyle.Render("Export State") + "\n\n")
	fmt.Fprintf(&b, "write to file: %s%s\n", m.adminState.path, cursorSuffix(true))
	if m.adminState.err != nil {
		b.WriteString("\n" + errorStyle.Render(m.adminState.err.Error()) + "\n")
	}
	b.WriteString("\n" + dimStyle.Render("mod sources, ConfigMaps, and server specs -- enter to export, esc to cancel"))
	return b.String()
}

func (m Model) viewAdminImportState() string {
	var b strings.Builder
	b.WriteString(titleStyle.Render("Import State") + "\n\n")
	fmt.Fprintf(&b, "read from file: %s%s\n", m.adminState.path, cursorSuffix(true))
	if m.adminState.err != nil {
		b.WriteString("\n" + errorStyle.Render(m.adminState.err.Error()) + "\n")
	}
	b.WriteString("\n" + dimStyle.Render("re-creates whatever export-state produced -- enter to import, esc to cancel"))
	return b.String()
}
