// Package tui is the interactive Bubble Tea app -- every screen here
// calls the exact same internal/actions functions the direct `magpie
// <resource> <verb>` subcommands do, so the two surfaces can never
// silently drift apart.
package tui

import (
	"context"
	"fmt"
	"strings"

	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"

	controllerv1 "github.com/skua-international/magpie/generated/go/controller/v1"
	registryv1 "github.com/skua-international/magpie/generated/go/registry/v1"

	"github.com/skua-international/magpie/cli/internal/actions"
	"github.com/skua-international/magpie/cli/internal/client"
)

type screen int

const (
	screenMenu screen = iota
	screenServers
	screenServersCreate
	screenModSources
	screenModSourcesAdd
	screenSyncedMods
	screenMissions
	screenMissionsUpload
	screenAdmin
	screenAccount
)

var menuItems = []struct {
	label  string
	screen screen
}{
	{"Servers", screenServers},
	{"Mod Sources", screenModSources},
	{"Synced Mods", screenSyncedMods},
	{"Missions", screenMissions},
	{"Admin", screenAdmin},
	{"Account", screenAccount},
}

type Model struct {
	ctx         context.Context
	clients     *client.Clients
	namespace   string
	release     string
	identityURL string
	accessToken string

	screen    screen
	cursor    int
	err       error // list-load failure only -- replaces the whole screen's content
	loaded    bool
	status    string // ephemeral one-line result of the last action (start/stop/delete/sync/...)
	statusErr bool   // true if status is an action failure, not a success message

	servers    []*controllerv1.ServerInfo
	modSources []*registryv1.ModSourceInfo
	syncedMods []*registryv1.SyncedMod
	missions   []*registryv1.MissionInfo
	diskUsage  *registryv1.GetDiskUsageResponse

	create        createServerState
	confirm       confirmState
	addMod        addModSourceState
	uploadMission uploadMissionState
	account       accountState
}

// New builds the TUI's top-level model. namespace/release are only used
// for kubectl-based ConfigMap calls that bypass server-api entirely --
// the "create server" flow's per-server override (create_server.go) and
// the Admin screen's baseline edit (admin_actions.go) -- every RPC-backed
// screen gets its namespace from server-api's own config instead, never
// from here. identityURL/accessToken are only used by the Account
// screen's link flow (see account.go) -- accessToken is a point-in-time
// snapshot, not refreshed for the life of the TUI session.
func New(ctx context.Context, clients *client.Clients, namespace, release, identityURL, accessToken string) Model {
	return Model{
		ctx: ctx, clients: clients, namespace: namespace, release: release,
		identityURL: identityURL, accessToken: accessToken,
		screen: screenMenu, create: newCreateServerState(),
	}
}

func (m Model) Init() tea.Cmd {
	return nil
}

// --- messages carrying the result of an internal/actions call back into
// Update, since those calls are I/O and have to happen inside a tea.Cmd,
// never directly inside Update itself.

type serversLoadedMsg struct {
	servers []*controllerv1.ServerInfo
	err     error
}

type modSourcesLoadedMsg struct {
	sources []*registryv1.ModSourceInfo
	err     error
}

type missionsLoadedMsg struct {
	missions []*registryv1.MissionInfo
	err      error
}

type diskUsageLoadedMsg struct {
	usage *registryv1.GetDiskUsageResponse
	err   error
}

func (m Model) loadServersCmd() tea.Cmd {
	return func() tea.Msg {
		servers, err := actions.ListServers(m.ctx, m.clients)
		return serversLoadedMsg{servers: servers, err: err}
	}
}

func (m Model) loadModSourcesCmd() tea.Cmd {
	return func() tea.Msg {
		sources, err := actions.ListModSources(m.ctx, m.clients)
		return modSourcesLoadedMsg{sources: sources, err: err}
	}
}

func (m Model) loadMissionsCmd() tea.Cmd {
	return func() tea.Msg {
		missions, err := actions.ListMissions(m.ctx, m.clients)
		return missionsLoadedMsg{missions: missions, err: err}
	}
}

func (m Model) loadDiskUsageCmd() tea.Cmd {
	return func() tea.Msg {
		usage, err := actions.GetDiskUsage(m.ctx, m.clients)
		return diskUsageLoadedMsg{usage: usage, err: err}
	}
}

func (m Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	if next, cmd, handled := m.handleCreateServerMsg(msg); handled {
		return next, cmd
	}

	switch msg := msg.(type) {
	case tea.KeyPressMsg:
		return m.handleKey(msg)

	case serversLoadedMsg:
		m.loaded, m.err, m.servers = true, msg.err, msg.servers
		return m, nil
	case modSourcesLoadedMsg:
		m.loaded, m.err, m.modSources = true, msg.err, msg.sources
		return m, nil
	case missionsLoadedMsg:
		m.loaded, m.err, m.missions = true, msg.err, msg.missions
		return m, nil
	case diskUsageLoadedMsg:
		m.loaded, m.err, m.diskUsage = true, msg.err, msg.usage
		return m, nil
	case syncedModsLoadedMsg:
		m.loaded, m.err, m.syncedMods = true, msg.err, msg.mods
		return m, nil

	case invalidateModDoneMsg:
		if msg.err != nil {
			m.status, m.statusErr = msg.err.Error(), true
			return m, nil
		}
		m.status, m.statusErr = fmt.Sprintf("%d invalidated", msg.modID), false
		return m, m.loadSyncedModsCmd()

	case serverActionDoneMsg:
		if msg.err != nil {
			m.status, m.statusErr = msg.err.Error(), true
			return m, nil
		}
		m.status, m.statusErr = fmt.Sprintf("%s %s", msg.id, msg.verb), false
		return m, m.loadServersCmd()

	case modSourceActionDoneMsg:
		if msg.err != nil {
			if m.screen == screenModSourcesAdd {
				m.addMod.err = msg.err
			} else {
				m.status, m.statusErr = msg.err.Error(), true
			}
			return m, nil
		}
		m.addMod = addModSourceState{}
		m.status, m.statusErr = fmt.Sprintf("%s %s", msg.id, msg.verb), false
		m.screen = screenModSources
		return m, m.loadModSourcesCmd()

	case missionActionDoneMsg:
		if msg.err != nil {
			if m.screen == screenMissionsUpload {
				m.uploadMission.err = msg.err
			} else {
				m.status, m.statusErr = msg.err.Error(), true
			}
			return m, nil
		}
		m.uploadMission = uploadMissionState{}
		m.status, m.statusErr = fmt.Sprintf("%s %s", msg.name, msg.verb), false
		m.screen = screenMissions
		return m, m.loadMissionsCmd()

	case adminActionDoneMsg:
		if msg.err != nil {
			m.status, m.statusErr = msg.err.Error(), true
			return m, nil
		}
		m.status, m.statusErr = msg.verb, false
		return m, nil

	case accountLinkedMsg:
		m.account.linking = false
		if msg.err != nil {
			m.account.err = msg.err
			return m, nil
		}
		m.account.err, m.account.result = nil, "Linked "+msg.provider+"."
		return m, nil
	}
	return m, nil
}

func (m Model) handleKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	// A pending delete confirmation always wins, on any screen -- see
	// confirm.go.
	if m.confirm.kind != confirmNone {
		return m.handleConfirmKey(msg)
	}

	// Every text-entry wizard screen has the same shape: "q"/plain esc
	// would otherwise be swallowed as global back/quit keys instead of
	// typed characters, so each gets routed here first, before any of
	// that, with only ctrl+c/esc treated specially (back to the
	// underlying list screen, not all the way out to the menu, since
	// that's the natural "cancel" target for all of these).
	switch m.screen {
	case screenServersCreate:
		switch msg.String() {
		case "ctrl+c":
			return m, tea.Quit
		case "esc":
			m.screen, m.cursor, m.loaded, m.err = screenServers, 0, false, nil
			m.create = newCreateServerState()
			return m, nil
		}
		return m.handleCreateServerKey(msg)

	case screenModSourcesAdd:
		switch msg.String() {
		case "ctrl+c":
			return m, tea.Quit
		case "esc":
			m.screen, m.addMod = screenModSources, addModSourceState{}
			return m, nil
		}
		return m.handleModSourcesAddKey(msg)

	case screenMissionsUpload:
		switch msg.String() {
		case "ctrl+c":
			return m, tea.Quit
		case "esc":
			m.screen, m.uploadMission = screenMissions, uploadMissionState{}
			return m, nil
		}
		return m.handleMissionsUploadKey(msg)

	case screenAccount:
		switch msg.String() {
		case "ctrl+c":
			return m, tea.Quit
		case "esc":
			if !m.account.linking {
				m.screen, m.cursor, m.loaded, m.err = screenMenu, 0, false, nil
			}
			return m, nil
		}
		return m.handleAccountKey(msg)
	}

	switch msg.String() {
	case "ctrl+c", "q":
		if m.screen == screenMenu {
			return m, tea.Quit
		}
		m.screen, m.cursor, m.loaded, m.err, m.status = screenMenu, 0, false, nil, ""
		return m, nil
	case "esc":
		if m.screen != screenMenu {
			m.screen, m.cursor, m.loaded, m.err, m.status = screenMenu, 0, false, nil, ""
		}
		return m, nil
	}

	// Per-screen action keys (start/stop/delete/... -- see
	// server_actions.go/modsource_actions.go/mission_actions.go) take
	// priority over the generic list nav below, since they share some of
	// the same keys the vi-style nav doesn't use ("s", "d", etc).
	switch m.screen {
	case screenServers:
		if next, cmd, handled := m.handleServersKey(msg); handled {
			return next, cmd
		}
	case screenModSources:
		if next, cmd, handled := m.handleModSourcesKey(msg); handled {
			return next, cmd
		}
	case screenMissions:
		if next, cmd, handled := m.handleMissionsKey(msg); handled {
			return next, cmd
		}
	case screenAdmin:
		if next, cmd, handled := m.handleAdminKey(msg); handled {
			return next, cmd
		}
	case screenSyncedMods:
		if next, cmd, handled := m.handleSyncedModsKey(msg); handled {
			return next, cmd
		}
	}

	if m.screen == screenMenu {
		switch msg.String() {
		case "up", "k":
			if m.cursor > 0 {
				m.cursor--
			}
		case "down", "j":
			if m.cursor < len(menuItems)-1 {
				m.cursor++
			}
		case "enter":
			m.screen = menuItems[m.cursor].screen
			m.cursor, m.loaded, m.err, m.status = 0, false, nil, ""
			switch m.screen {
			case screenServers:
				return m, m.loadServersCmd()
			case screenModSources:
				return m, m.loadModSourcesCmd()
			case screenSyncedMods:
				return m, m.loadSyncedModsCmd()
			case screenMissions:
				return m, m.loadMissionsCmd()
			case screenAdmin:
				return m, m.loadDiskUsageCmd()
			}
		}
		return m, nil
	}

	// Any other screen: up/down just moves a cursor over whatever list is
	// loaded.
	n := m.currentListLen()
	switch msg.String() {
	case "up", "k":
		if m.cursor > 0 {
			m.cursor--
		}
	case "down", "j":
		if n > 0 && m.cursor < n-1 {
			m.cursor++
		}
	}
	return m, nil
}

func (m Model) currentListLen() int {
	switch m.screen {
	case screenServers:
		return len(m.servers)
	case screenModSources:
		return len(m.modSources)
	case screenSyncedMods:
		return len(m.syncedMods)
	case screenMissions:
		return len(m.missions)
	default:
		return 0
	}
}

var (
	titleStyle    = lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("212"))
	selectedStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("212")).Bold(true)
	dimStyle      = lipgloss.NewStyle().Foreground(lipgloss.Color("243"))
	errorStyle    = lipgloss.NewStyle().Foreground(lipgloss.Color("196"))
)

func (m Model) View() tea.View {
	var b strings.Builder

	if m.status != "" {
		style := dimStyle
		if m.statusErr {
			style = errorStyle
		}
		b.WriteString(style.Render(m.status) + "\n\n")
	}

	switch m.screen {
	case screenMenu:
		b.WriteString(titleStyle.Render("magpie") + "\n\n")
		for i, item := range menuItems {
			if i == m.cursor {
				b.WriteString(selectedStyle.Render("> "+item.label) + "\n")
			} else {
				b.WriteString("  " + item.label + "\n")
			}
		}
		b.WriteString("\n" + dimStyle.Render("↑/↓ to move, enter to select, q to quit"))

	case screenServers:
		b.WriteString(titleStyle.Render("Servers") + "\n\n")
		if !m.loaded {
			b.WriteString("Loading...")
		} else if m.err != nil {
			b.WriteString(errorStyle.Render(m.err.Error()))
		} else if len(m.servers) == 0 {
			b.WriteString(dimStyle.Render("No servers."))
		} else {
			for i, s := range m.servers {
				line := fmt.Sprintf("%-20s port=%-5d phase=%-12s desired=%s", s.Id, s.Port, s.Phase.String(), s.DesiredState.String())
				b.WriteString(renderLine(line, i == m.cursor) + "\n")
			}
		}
		b.WriteString("\n" + dimStyle.Render("n: new, s: start, x: stop, u: resync, d: delete, esc to go back"))

	case screenServersCreate:
		b.WriteString(m.viewCreateServer())

	case screenModSources:
		b.WriteString(titleStyle.Render("Mod Sources") + "\n\n")
		if !m.loaded {
			b.WriteString("Loading...")
		} else if m.err != nil {
			b.WriteString(errorStyle.Render(m.err.Error()))
		} else if len(m.modSources) == 0 {
			b.WriteString(dimStyle.Render("No mod sources."))
		} else {
			for i, s := range m.modSources {
				line := fmt.Sprintf("%-38s kind=%-11s %s", s.Id, s.Kind.String(), s.Reference)
				b.WriteString(renderLine(line, i == m.cursor) + "\n")
			}
		}
		b.WriteString("\n" + dimStyle.Render("a: add, s: sync, d: delete, esc to go back"))

	case screenModSourcesAdd:
		b.WriteString(m.viewModSourcesAdd())

	case screenSyncedMods:
		b.WriteString(m.viewSyncedMods())

	case screenMissions:
		b.WriteString(titleStyle.Render("Missions") + "\n\n")
		if !m.loaded {
			b.WriteString("Loading...")
		} else if m.err != nil {
			b.WriteString(errorStyle.Render(m.err.Error()))
		} else if len(m.missions) == 0 {
			b.WriteString(dimStyle.Render("No missions."))
		} else {
			for i, ms := range m.missions {
				line := fmt.Sprintf("%-38s %s", ms.Id, ms.Name)
				b.WriteString(renderLine(line, i == m.cursor) + "\n")
			}
		}
		b.WriteString("\n" + dimStyle.Render("u: upload, d: delete, esc to go back"))

	case screenMissionsUpload:
		b.WriteString(m.viewMissionsUpload())

	case screenAdmin:
		b.WriteString(titleStyle.Render("Admin") + "\n\n")
		if !m.loaded {
			b.WriteString("Loading...")
		} else if m.err != nil {
			b.WriteString(errorStyle.Render(m.err.Error()))
		} else if m.diskUsage != nil {
			b.WriteString(fmt.Sprintf("mods:       %d bytes\n", m.diskUsage.ModsBytes))
			b.WriteString(fmt.Sprintf("missions:   %d bytes\n", m.diskUsage.MissionsBytes))
			b.WriteString(fmt.Sprintf("game files: %d bytes\n", m.diskUsage.GameFilesBytes))
			b.WriteString(fmt.Sprintf("total:      %d bytes\n", m.diskUsage.TotalBytes))
		}
		b.WriteString("\n" + dimStyle.Render("e: edit baseline config, r: refresh Steam auth, esc to go back"))

	case screenAccount:
		b.WriteString(m.viewAccount())
	}

	if m.confirm.kind != confirmNone {
		b.WriteString("\n\n" + errorStyle.Render(m.confirm.prompt))
	}

	return tea.NewView(b.String())
}

func renderLine(s string, selected bool) string {
	if selected {
		return selectedStyle.Render("> " + s)
	}
	return "  " + s
}
