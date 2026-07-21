package tui

import (
	"fmt"
	"strings"

	tea "charm.land/bubbletea/v2"

	"github.com/skua-international/magpie/cli/internal/actions"
)

// addModSourceKind picks which of AddModSourceSteamURL/HTMLURL/
// HTMLContent/LocalZip the wizard submits to -- mirrors `mods add`'s
// four mutually exclusive flags (--steam-url/--preset-url/--local-html/
// --local-zip+--local-id).
type addModSourceKind int

const (
	addModSteam addModSourceKind = iota
	addModPreset
	addModLocalHTML
	addModLocalZip
)

var addModKindLabels = []string{"Steam Workshop URL", "Preset HTML export URL", "Local HTML export file", "Local zip file"}

type addModSourceStep int

const (
	addModStepKind    addModSourceStep = iota // pick which of the four
	addModStepValue                           // URL, or local file path
	addModStepLocalID                         // only reached for addModLocalZip
)

type addModSourceState struct {
	step       addModSourceStep
	kind       addModSourceKind
	kindCursor int
	value      string // steam URL, preset URL, or local zip path depending on kind
	localID    string
	err        error
}

type modSourceActionDoneMsg struct {
	verb string // "added", "deleted", "sync started"
	id   string
	err  error
}

func (m Model) addModSourceCmd() tea.Cmd {
	kind, value, localID := m.addMod.kind, strings.TrimSpace(m.addMod.value), strings.TrimSpace(m.addMod.localID)
	return func() tea.Msg {
		var id string
		var err error
		switch kind {
		case addModSteam:
			id, err = actions.AddModSourceSteamURL(m.ctx, m.clients, value)
		case addModPreset:
			id, err = actions.AddModSourceHTMLURL(m.ctx, m.clients, value)
		case addModLocalHTML:
			id, err = actions.AddModSourceHTMLContent(m.ctx, m.clients, value)
		case addModLocalZip:
			id, err = actions.AddModSourceLocalZip(m.ctx, m.clients, localID, value)
		}
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
		err := actions.SyncModSource(m.ctx, m.clients, id)
		return modSourceActionDoneMsg{verb: "sync started", id: id, err: err}
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

// handleModSourcesAddKey drives screenModSourcesAdd's little kind-then-
// value(-then-local-id) wizard.
func (m Model) handleModSourcesAddKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch m.addMod.step {
	case addModStepKind:
		switch msg.String() {
		case "up", "k":
			if m.addMod.kindCursor > 0 {
				m.addMod.kindCursor--
			}
		case "down", "j":
			if m.addMod.kindCursor < len(addModKindLabels)-1 {
				m.addMod.kindCursor++
			}
		case "enter":
			m.addMod.kind = addModSourceKind(m.addMod.kindCursor)
			m.addMod.step = addModStepValue
		}

	case addModStepValue:
		switch msg.String() {
		case "enter":
			if strings.TrimSpace(m.addMod.value) == "" {
				return m, nil
			}
			if m.addMod.kind == addModLocalZip {
				m.addMod.step = addModStepLocalID
				return m, nil
			}
			m.status = ""
			return m, m.addModSourceCmd()
		case "backspace":
			m.addMod.value = trimLastRune(m.addMod.value)
		default:
			if msg.Text != "" {
				m.addMod.value += msg.Text
			}
		}

	case addModStepLocalID:
		switch msg.String() {
		case "enter":
			if strings.TrimSpace(m.addMod.localID) == "" {
				return m, nil
			}
			m.status = ""
			return m, m.addModSourceCmd()
		case "backspace":
			m.addMod.localID = trimLastRune(m.addMod.localID)
		default:
			if msg.Text != "" {
				m.addMod.localID += msg.Text
			}
		}
	}
	return m, nil
}

func (m Model) viewModSourcesAdd() string {
	var b strings.Builder
	b.WriteString(titleStyle.Render("Add Mod Source") + "\n\n")

	if m.addMod.step == addModStepKind {
		for i, label := range addModKindLabels {
			b.WriteString(renderLine(label, i == m.addMod.kindCursor) + "\n")
		}
		b.WriteString("\n" + dimStyle.Render("enter to select, esc to cancel"))
		return b.String()
	}

	fmt.Fprintf(&b, "kind: %s\n", addModKindLabels[m.addMod.kind])
	valueLabel := "steam workshop URL (mod or collection)"
	switch m.addMod.kind {
	case addModPreset:
		valueLabel = "preset HTML export URL"
	case addModLocalHTML:
		valueLabel = "local path to .html"
	case addModLocalZip:
		valueLabel = "local path to .zip"
	}
	fmt.Fprintf(&b, "%s: %s%s\n", valueLabel, m.addMod.value, cursorSuffix(m.addMod.step == addModStepValue))

	if m.addMod.step >= addModStepLocalID {
		fmt.Fprintf(&b, "local ID (stable, unique to this mod): %s%s\n", m.addMod.localID, cursorSuffix(true))
	}

	if m.addMod.err != nil {
		b.WriteString("\n" + errorStyle.Render(m.addMod.err.Error()) + "\n")
	}
	b.WriteString("\n" + dimStyle.Render("enter to continue, esc to cancel"))
	return b.String()
}
