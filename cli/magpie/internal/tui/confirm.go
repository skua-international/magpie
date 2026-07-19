package tui

import tea "charm.land/bubbletea/v2"

// confirmKind identifies which destructive action a pending confirmState
// belongs to -- one shared "press y to confirm" gate for every delete
// across every list screen, rather than three near-identical copies of
// the same three fields and the same y/n key handling.
type confirmKind int

const (
	confirmNone confirmKind = iota
	confirmDeleteServer
	confirmDeleteModSource
	confirmDeleteMission
)

type confirmState struct {
	kind   confirmKind
	prompt string
	target string // the ID the confirmed action applies to
}

// handleConfirmKey is checked first, ahead of every other per-screen key
// handler, whenever m.confirm.kind != confirmNone -- any key besides y/Y
// cancels back to the underlying screen without side effects.
func (m Model) handleConfirmKey(msg tea.KeyPressMsg) (Model, tea.Cmd) {
	kind, target := m.confirm.kind, m.confirm.target
	m.confirm = confirmState{}

	if msg.String() != "y" && msg.String() != "Y" {
		return m, nil
	}

	switch kind {
	case confirmDeleteServer:
		return m, m.deleteServerCmd(target)
	case confirmDeleteModSource:
		return m, m.deleteModSourceCmd(target)
	case confirmDeleteMission:
		return m, m.deleteMissionCmd(target)
	}
	return m, nil
}
