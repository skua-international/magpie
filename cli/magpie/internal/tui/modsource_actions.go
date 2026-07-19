package tui

import (
	"fmt"
	"strings"

	tea "charm.land/bubbletea/v2"

	"github.com/skua-international/magpie/cli/internal/actions"
)

// addModSourceState is a one-field wizard (just a Steam Workshop URL) --
// the CLI's `mods add` also supports --preset-url/--local-zip, but those
// need either a browser export or a local file path with a separate
// --local-id, awkward enough as a single text field that they're left to
// the CLI for now rather than forcing them through one.
type addModSourceState struct {
	url string
	err error
}

type modSourceActionDoneMsg struct {
	verb string // "added", "deleted", "sync started"
	id   string
	err  error
}

func (m Model) addModSourceCmd(steamURL string) tea.Cmd {
	return func() tea.Msg {
		id, err := actions.AddModSourceSteamURL(m.ctx, m.clients, steamURL)
		return modSourceActionDoneMsg{verb: "added", id: id, err: err}
	}
}

func (m Model) deleteModSourceCmd(id string) tea.Cmd {
	return func() tea.Msg {
		err := actions.DeleteModSource(m.ctx, m.clients, id)
		return modSourceActionDoneMsg{verb: "deleted", id: id, err: err}
	}
}

func (m Model) syncModSourceCmd(id string) tea.Cmd {
	return func() tea.Msg {
		jobID, err := actions.SyncModSource(m.ctx, m.clients, id)
		return modSourceActionDoneMsg{verb: "sync job " + jobID + " started", id: id, err: err}
	}
}

func (m Model) selectedModSourceID() string {
	if m.cursor < 0 || m.cursor >= len(m.modSources) {
		return ""
	}
	return m.modSources[m.cursor].Id
}

// handleModSourcesKey handles the mod sources list screen's own action
// keys, checked before the generic up/down list nav.
func (m Model) handleModSourcesKey(msg tea.KeyPressMsg) (Model, tea.Cmd, bool) {
	id := m.selectedModSourceID()
	switch msg.String() {
	case "a":
		m.screen = screenModSourcesAdd
		m.addMod = addModSourceState{}
		return m, nil, true
	case "s":
		if id == "" {
			return m, nil, true
		}
		m.status = ""
		return m, m.syncModSourceCmd(id), true
	case "d":
		if id == "" {
			return m, nil, true
		}
		m.confirm = confirmState{kind: confirmDeleteModSource, target: id, prompt: fmt.Sprintf("Delete mod source %s? [y/N]", id)}
		return m, nil, true
	}
	return m, nil, false
}

// handleModSourcesAddKey drives screenModSourcesAdd's single text field.
func (m Model) handleModSourcesAddKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "ctrl+c":
		return m, tea.Quit
	case "esc":
		m.screen = screenModSources
		return m, nil
	case "enter":
		url := strings.TrimSpace(m.addMod.url)
		if url == "" {
			return m, nil
		}
		m.status = ""
		return m, m.addModSourceCmd(url)
	case "backspace":
		m.addMod.url = trimLastRune(m.addMod.url)
	default:
		if msg.Text != "" {
			m.addMod.url += msg.Text
		}
	}
	return m, nil
}

func (m Model) viewModSourcesAdd() string {
	var b strings.Builder
	b.WriteString(titleStyle.Render("Add Mod Source") + "\n\n")
	fmt.Fprintf(&b, "steam workshop URL (mod or collection): %s%s\n", m.addMod.url, cursorSuffix(true))
	if m.addMod.err != nil {
		b.WriteString("\n" + errorStyle.Render(m.addMod.err.Error()) + "\n")
	}
	b.WriteString("\n" + dimStyle.Render("enter to add, esc to cancel"))
	return b.String()
}
