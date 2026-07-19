package tui

import (
	"fmt"
	"strings"

	tea "charm.land/bubbletea/v2"

	"github.com/skua-international/magpie/cli/internal/actions"
)

// uploadMissionState is a single text field: a local path to a .pbo.
// Overwrite-in-place (the CLI's `missions upload --overwrite`) isn't
// exposed here -- uploading a new mission and deleting the old one from
// the same list screen covers the same outcome without a second field.
type uploadMissionState struct {
	path string
	err  error
}

type missionActionDoneMsg struct {
	verb string // "uploaded", "deleted"
	name string
	err  error
}

func (m Model) uploadMissionCmd(path string) tea.Cmd {
	return func() tea.Msg {
		info, err := actions.UploadMission(m.ctx, m.clients, path, "")
		name := path
		if info != nil {
			name = info.Name
		}
		return missionActionDoneMsg{verb: "uploaded", name: name, err: err}
	}
}

func (m Model) deleteMissionCmd(id string) tea.Cmd {
	return func() tea.Msg {
		err := actions.DeleteMission(m.ctx, m.clients, id)
		return missionActionDoneMsg{verb: "deleted", name: id, err: err}
	}
}

func (m Model) selectedMissionID() string {
	if m.cursor < 0 || m.cursor >= len(m.missions) {
		return ""
	}
	return m.missions[m.cursor].Id
}

// handleMissionsKey handles the missions list screen's own action keys,
// checked before the generic up/down list nav.
func (m Model) handleMissionsKey(msg tea.KeyPressMsg) (Model, tea.Cmd, bool) {
	id := m.selectedMissionID()
	switch msg.String() {
	case "u":
		m.screen = screenMissionsUpload
		m.uploadMission = uploadMissionState{}
		return m, nil, true
	case "d":
		if id == "" {
			return m, nil, true
		}
		m.confirm = confirmState{kind: confirmDeleteMission, target: id, prompt: fmt.Sprintf("Delete mission %s? [y/N]", id)}
		return m, nil, true
	}
	return m, nil, false
}

func (m Model) handleMissionsUploadKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch msg.String() {
	case "ctrl+c":
		return m, tea.Quit
	case "esc":
		m.screen = screenMissions
		return m, nil
	case "enter":
		path := strings.TrimSpace(m.uploadMission.path)
		if path == "" {
			return m, nil
		}
		m.status = ""
		return m, m.uploadMissionCmd(path)
	case "backspace":
		m.uploadMission.path = trimLastRune(m.uploadMission.path)
	default:
		if msg.Text != "" {
			m.uploadMission.path += msg.Text
		}
	}
	return m, nil
}

func (m Model) viewMissionsUpload() string {
	var b strings.Builder
	b.WriteString(titleStyle.Render("Upload Mission") + "\n\n")
	fmt.Fprintf(&b, "local path to .pbo: %s%s\n", m.uploadMission.path, cursorSuffix(true))
	if m.uploadMission.err != nil {
		b.WriteString("\n" + errorStyle.Render(m.uploadMission.err.Error()) + "\n")
	}
	b.WriteString("\n" + dimStyle.Render("enter to upload, esc to cancel"))
	return b.String()
}
