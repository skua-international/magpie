package tui

import (
	"fmt"
	"strconv"
	"strings"

	tea "charm.land/bubbletea/v2"

	registryv1 "github.com/skua-international/magpie/generated/go/registry/v1"

	"github.com/skua-international/magpie/cli/internal/actions"
)

type syncedModsLoadedMsg struct {
	mods []*registryv1.SyncedMod
	err  error
}

type invalidateModDoneMsg struct {
	modID uint64
	err   error
}

func (m Model) loadSyncedModsCmd() tea.Cmd {
	return func() tea.Msg {
		mods, err := actions.ListSyncedMods(m.ctx, m.clients)
		return syncedModsLoadedMsg{mods: mods, err: err}
	}
}

func (m Model) invalidateModCmd(modID uint64) tea.Cmd {
	return func() tea.Msg {
		err := actions.InvalidateMod(m.ctx, m.clients, modID)
		return invalidateModDoneMsg{modID: modID, err: err}
	}
}

func (m Model) selectedSyncedModID() (uint64, bool) {
	if m.cursor < 0 || m.cursor >= len(m.syncedMods) {
		return 0, false
	}
	return m.syncedMods[m.cursor].ModId, true
}

// handleSyncedModsKey handles the Synced Mods list screen's own action
// key -- checked before the generic up/down list nav.
func (m Model) handleSyncedModsKey(msg tea.KeyPressMsg) (Model, tea.Cmd, bool) {
	if msg.String() != "i" {
		return m, nil, false
	}
	modID, ok := m.selectedSyncedModID()
	if !ok {
		return m, nil, true
	}
	// Not routed through confirmState: invalidate never deletes files
	// (see modsInvalidateCmd's own doc), just clears a verification
	// cache entry -- low enough stakes not to need a confirm gate the
	// way an actual delete does.
	m.status = ""
	return m, m.invalidateModCmd(modID), true
}

func (m Model) viewSyncedMods() string {
	var b strings.Builder
	b.WriteString(titleStyle.Render("Synced Mods") + "\n\n")
	if !m.loaded {
		b.WriteString("Loading...")
	} else if m.err != nil {
		b.WriteString(errorStyle.Render(m.err.Error()))
	} else if len(m.syncedMods) == 0 {
		b.WriteString(dimStyle.Render("No synced mods."))
	} else {
		for i, mod := range m.syncedMods {
			title := mod.Title
			if title == "" {
				title = dimStyle.Render("(unresolved)")
			}
			line := fmt.Sprintf("%-12s size=%-10s %s", strconv.FormatUint(mod.ModId, 10), actions.HumanBytes(mod.SizeBytes), title)
			b.WriteString(renderLine(line, i == m.cursor) + "\n")
		}
	}
	b.WriteString("\n" + dimStyle.Render("i: invalidate verification cache, esc to go back"))
	return b.String()
}
