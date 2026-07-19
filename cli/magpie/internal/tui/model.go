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
	screenMissions
	screenAdmin
)

var menuItems = []struct {
	label  string
	screen screen
}{
	{"Servers", screenServers},
	{"Mod Sources", screenModSources},
	{"Missions", screenMissions},
	{"Admin", screenAdmin},
}

type Model struct {
	ctx       context.Context
	clients   *client.Clients
	namespace string

	screen screen
	cursor int
	err    error
	loaded bool

	servers    []*controllerv1.ServerInfo
	modSources []*registryv1.ModSourceInfo
	missions   []*registryv1.MissionInfo
	diskUsage  *registryv1.GetDiskUsageResponse

	create createServerState
}

// New builds the TUI's top-level model. namespace is only used for the
// "create server" flow's per-server ConfigMap kubectl calls (see
// create_server.go) -- every RPC-backed screen gets its namespace from
// server-api's own config instead, never from here.
func New(ctx context.Context, clients *client.Clients, namespace string) Model {
	return Model{ctx: ctx, clients: clients, namespace: namespace, screen: screenMenu, create: newCreateServerState()}
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
	}
	return m, nil
}

func (m Model) handleKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	// The create-server wizard has text-entry steps where "q"/"esc" would
	// otherwise be swallowed as global back/quit keys instead of typed
	// characters -- routed separately, before any of that, with only
	// ctrl+c/esc treated specially (back to the servers list, not all the
	// way out to the menu, since that's the natural "cancel" target here).
	if m.screen == screenServersCreate {
		switch msg.String() {
		case "ctrl+c":
			return m, tea.Quit
		case "esc":
			m.screen, m.cursor, m.loaded, m.err = screenServers, 0, false, nil
			m.create = newCreateServerState()
			return m, nil
		}
		return m.handleCreateServerKey(msg)
	}

	switch msg.String() {
	case "ctrl+c", "q":
		if m.screen == screenMenu {
			return m, tea.Quit
		}
		m.screen, m.cursor, m.loaded, m.err = screenMenu, 0, false, nil
		return m, nil
	case "esc":
		if m.screen != screenMenu {
			m.screen, m.cursor, m.loaded, m.err = screenMenu, 0, false, nil
		}
		return m, nil
	}

	if m.screen == screenServers {
		if msg.String() == "n" {
			m.screen = screenServersCreate
			m.cursor = 0
			m.create = newCreateServerState()
			return m, nil
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
			m.cursor, m.loaded, m.err = 0, false, nil
			switch m.screen {
			case screenServers:
				return m, m.loadServersCmd()
			case screenModSources:
				return m, m.loadModSourcesCmd()
			case screenMissions:
				return m, m.loadMissionsCmd()
			case screenAdmin:
				return m, m.loadDiskUsageCmd()
			}
		}
		return m, nil
	}

	// Any other screen: up/down just moves a cursor over whatever list is
	// loaded -- selection/actions (start/stop/delete/...) land in a later
	// pass once the read-only shell here is proven out.
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
		b.WriteString("\n" + dimStyle.Render("n: new server, esc to go back"))

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
		b.WriteString("\n" + dimStyle.Render("esc to go back"))

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
		b.WriteString("\n" + dimStyle.Render("esc to go back"))

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
		b.WriteString("\n" + dimStyle.Render("esc to go back"))
	}

	return tea.NewView(b.String())
}

func renderLine(s string, selected bool) string {
	if selected {
		return selectedStyle.Render("> " + s)
	}
	return "  " + s
}
