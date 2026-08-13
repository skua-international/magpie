// Package tui is the interactive Bubble Tea app -- every screen here
// calls the exact same internal/actions functions the direct `magpie
// <resource> <verb>` subcommands do, so the two surfaces can never
// silently drift apart.
package tui

import (
	"context"
	"fmt"
	"strings"
	"time"
	"unicode"

	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"

	controllerv1 "github.com/skua-international/magpie/generated/go/controller/v1"
	registryv1 "github.com/skua-international/magpie/generated/go/registry/v1"

	"github.com/skua-international/magpie/cli/internal/actions"
	"github.com/skua-international/magpie/cli/internal/client"
)

// refreshInterval is how often a "live" list screen (Servers, Mod
// Sources, Synced Mods, Missions, Admin) silently reloads its data in
// the background -- without this, watching an ArmaServer move through
// phases (Pending -> Claiming -> Running) meant backing all the way out
// to the menu and back in just to see the current state.
const refreshInterval = 3 * time.Second

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
	screenAdminExportState
	screenAdminImportState
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
	apiURL      string
	accessToken string

	// Zero until the first tea.WindowSizeMsg arrives (sent automatically
	// on startup and on every resize) -- list rendering treats 0 as
	// "unknown, don't clamp" rather than collapsing everything to
	// nothing on the first frame.
	width, height int

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
	adminState    adminStateState
	account       accountState
}

// New builds the TUI's top-level model. namespace/release are only used
// for kubectl-based ConfigMap calls that bypass gateway entirely --
// the "create server" flow's per-server override (create_server.go) and
// the Admin screen's baseline edit (admin_actions.go) -- every RPC-backed
// screen gets its namespace from gateway's own config instead, never
// from here. apiURL/accessToken are only used by the Account
// screen's link flow (see account.go) -- accessToken is a point-in-time
// snapshot, not refreshed for the life of the TUI session.
func New(ctx context.Context, clients *client.Clients, namespace, release, apiURL, accessToken string) Model {
	return Model{
		ctx: ctx, clients: clients, namespace: namespace, release: release,
		apiURL: apiURL, accessToken: accessToken,
		screen: screenMenu, create: newCreateServerState(),
	}
}

func (m Model) Init() tea.Cmd {
	return tickCmd()
}

// tickMsg drives the background auto-refresh loop -- self-perpetuating
// (every tickMsg reschedules the next one via tickCmd in Update), so
// this fires for the lifetime of the program regardless of which screen
// is active; Update only actually reloads data for screens that have
// something live to show (see isLiveScreen).
type tickMsg struct{}

func tickCmd() tea.Cmd {
	return tea.Tick(refreshInterval, func(time.Time) tea.Msg { return tickMsg{} })
}

// isLiveScreen is true for screens showing data that can change out
// from under the operator while they're looking at it (a server's
// phase, a mod source's resolved size, ...). Wizard/form screens
// (create/add/upload, export/import, account) are deliberately excluded
// -- a background reload has no business touching in-progress input.
func isLiveScreen(s screen) bool {
	switch s {
	case screenServers, screenModSources, screenSyncedMods, screenMissions, screenAdmin:
		return true
	default:
		return false
	}
}

// reloadCmdForScreen returns whatever load command corresponds to the
// current screen's data, or nil if this screen has nothing to reload
// (mirrors the menu's own screen -> loadCmd switch in handleKey).
func (m Model) reloadCmdForScreen() tea.Cmd {
	switch m.screen {
	case screenServers:
		return m.loadServersCmd()
	case screenModSources:
		return m.loadModSourcesCmd()
	case screenSyncedMods:
		return m.loadSyncedModsCmd()
	case screenMissions:
		return m.loadMissionsCmd()
	case screenAdmin:
		return m.loadDiskUsageCmd()
	default:
		return nil
	}
}

// loadOrKeep centralizes every *LoadedMsg handler's shared "did this
// load succeed" logic: a failure on a screen's very first load shows a
// full-screen error (there's nothing better to show yet); a failure on
// a background refresh (m.loaded already true) leaves whatever's
// already displayed alone and surfaces a transient status line instead,
// so one missed poll doesn't blank out a perfectly good list. Returns
// true if the caller should go on to apply the new data.
func (m *Model) loadOrKeep(err error) bool {
	if err != nil {
		if !m.loaded {
			m.err = err
		} else {
			m.status, m.statusErr = "refresh failed: "+err.Error(), true
		}
		return false
	}
	m.loaded = true
	return true
}

// clampCursor keeps the cursor in bounds after a reload shrinks (or
// starts from) a list -- a background refresh can shrink the list out
// from under a cursor that was previously sitting on the last row.
func clampCursor(cursor, length int) int {
	if length == 0 {
		return 0
	}
	if cursor >= length {
		return length - 1
	}
	if cursor < 0 {
		return 0
	}
	return cursor
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
	case tea.WindowSizeMsg:
		m.width, m.height = msg.Width, msg.Height
		return m, nil

	case tickMsg:
		next := tickCmd()
		if !isLiveScreen(m.screen) {
			return m, next
		}
		if reload := m.reloadCmdForScreen(); reload != nil {
			return m, tea.Batch(reload, next)
		}
		return m, next

	case tea.KeyPressMsg:
		return m.handleKey(msg)

	case tea.PasteMsg:
		return m.handlePaste(msg), nil

	case serversLoadedMsg:
		if !m.loadOrKeep(msg.err) {
			return m, nil
		}
		m.err, m.servers = nil, msg.servers
		m.cursor = clampCursor(m.cursor, len(m.servers))
		return m, nil
	case modSourcesLoadedMsg:
		if !m.loadOrKeep(msg.err) {
			return m, nil
		}
		m.err, m.modSources = nil, msg.sources
		m.cursor = clampCursor(m.cursor, len(m.modSources))
		return m, nil
	case missionsLoadedMsg:
		if !m.loadOrKeep(msg.err) {
			return m, nil
		}
		m.err, m.missions = nil, msg.missions
		m.cursor = clampCursor(m.cursor, len(m.missions))
		return m, nil
	case diskUsageLoadedMsg:
		if !m.loadOrKeep(msg.err) {
			return m, nil
		}
		m.err, m.diskUsage = nil, msg.usage
		return m, nil
	case syncedModsLoadedMsg:
		if !m.loadOrKeep(msg.err) {
			return m, nil
		}
		m.err, m.syncedMods = nil, msg.mods
		m.cursor = clampCursor(m.cursor, len(m.syncedMods))
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

	case stateExportedMsg:
		if msg.err != nil {
			m.adminState.err = msg.err
			return m, nil
		}
		m.screen, m.adminState = screenAdmin, adminStateState{}
		m.status, m.statusErr = fmt.Sprintf(
			"exported %d mod source(s), %d ConfigMap(s), %d server(s) to %s",
			msg.modSources, msg.configMaps, msg.servers, msg.path,
		), false
		for _, w := range msg.warnings {
			m.status += "\n  warning: " + w
		}
		return m, nil

	case stateImportedMsg:
		if msg.err != nil {
			m.adminState.err = msg.err
			return m, nil
		}
		m.screen, m.adminState = screenAdmin, adminStateState{}
		m.status, m.statusErr = "import complete.", false
		for _, w := range msg.warnings {
			m.status += "\n  warning: " + w
		}
		return m, nil

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

	case screenAdminExportState, screenAdminImportState:
		switch msg.String() {
		case "ctrl+c":
			return m, tea.Quit
		case "esc":
			m.screen, m.adminState = screenAdmin, adminStateState{}
			return m, nil
		}
		return m.handleAdminStateKey(msg)

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
	case "f5":
		// On-demand counterpart to the background tick (see tickCmd) --
		// same reloadCmdForScreen, same loadOrKeep-based error handling,
		// just fired immediately instead of waiting up to
		// refreshInterval. A no-op on any screen with nothing live to
		// reload (wizards, menu).
		if reload := m.reloadCmdForScreen(); reload != nil {
			return m, reload
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

// handlePaste routes a bracketed-paste's whole content into whichever
// text field the current screen/step has focused -- bubbletea delivers a
// paste as one tea.PasteMsg, never as tea.KeyPressMsg (confirmed live:
// every text field here only ever handled KeyPressMsg, so pasting into
// any of them silently did nothing). Mirrors each screen's own per-char
// `default:` append branch in handleCreateServerKey/handleModSourcesAddKey/
// etc., just appending the whole string at once instead of one rune at a
// time.
func (m Model) handlePaste(msg tea.PasteMsg) Model {
	switch m.screen {
	case screenServersCreate:
		switch m.create.step {
		case createStepName:
			m.create.name += msg.Content
		case createStepPort:
			m.create.port += digitsOnly(msg.Content)
		case createStepConfigMapName:
			m.create.configMap += msg.Content
		}
	case screenModSourcesAdd:
		switch m.addMod.step {
		case addModStepValue:
			m.addMod.value += msg.Content
		case addModStepLocalID:
			m.addMod.localID += msg.Content
		}
	case screenMissionsUpload:
		m.uploadMission.path += msg.Content
	case screenAdminExportState, screenAdminImportState:
		m.adminState.path += msg.Content
	}
	return m
}

func digitsOnly(s string) string {
	var b strings.Builder
	for _, r := range s {
		if unicode.IsDigit(r) {
			b.WriteRune(r)
		}
	}
	return b.String()
}

// maxListRows returns how many list rows currently fit below the
// title/status and above the footer help line -- a generous fixed
// reservation rather than an exact layout budget (this is a plain-text
// TUI, not a pixel-perfect one), erring toward leaving a little slack
// rather than risking an off-by-one overflow past the bottom of the
// terminal. Returns a very large number until the first
// tea.WindowSizeMsg arrives, so nothing clamps before the real terminal
// size is known.
func (m Model) maxListRows() int {
	if m.height <= 0 {
		return 1 << 30
	}
	reserved := 6 // title + blank, blank + footer help, one line of slack
	if m.status != "" {
		reserved += strings.Count(m.status, "\n") + 2
	}
	rows := m.height - reserved
	if rows < 1 {
		rows = 1
	}
	return rows
}

// visibleWindow returns [start, end) into a total-length list, sized to
// maxRows and centered on cursor -- keeps the selected row on screen
// while scrolling through a list longer than the terminal can show at
// once, instead of either truncating past the cursor or overflowing the
// screen entirely.
func visibleWindow(cursor, total, maxRows int) (start, end int) {
	if maxRows <= 0 || total <= maxRows {
		return 0, total
	}
	start = cursor - maxRows/2
	if start < 0 {
		start = 0
	}
	end = start + maxRows
	if end > total {
		end = total
		start = end - maxRows
	}
	return start, end
}

// renderList writes a (possibly windowed) list to b, plus a "(a-b of
// n)" indicator when the window doesn't cover the whole list -- shared
// by every *LoadedMsg-backed list screen (Servers/Mod Sources/
// Missions/Synced Mods) instead of each reimplementing the same
// scroll-window arithmetic.
func (m Model) renderList(b *strings.Builder, total int, lineFor func(i int) string) {
	start, end := visibleWindow(m.cursor, total, m.maxListRows())
	for i := start; i < end; i++ {
		b.WriteString(renderLine(lineFor(i), i == m.cursor) + "\n")
	}
	if start > 0 || end < total {
		b.WriteString(dimStyle.Render(fmt.Sprintf("(%d-%d of %d)", start+1, end, total)) + "\n")
	}
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
			m.renderList(&b, len(m.servers), func(i int) string {
				s := m.servers[i]
				return fmt.Sprintf("%-20s port=%-5d phase=%-12s desired=%s", s.Id, s.Port, s.Phase.String(), s.DesiredState.String())
			})
		}
		b.WriteString("\n" + dimStyle.Render("n: new, s: start, x: stop, u: resync, d: delete, f5: refresh, esc to go back"))

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
			m.renderList(&b, len(m.modSources), func(i int) string {
				s := m.modSources[i]
				return fmt.Sprintf("%-38s kind=%-11s %s", s.Id, s.Kind.String(), actions.ModSourceLabel(s))
			})
		}
		b.WriteString("\n" + dimStyle.Render("a: add, s: sync, d: delete, f5: refresh, esc to go back"))

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
			m.renderList(&b, len(m.missions), func(i int) string {
				ms := m.missions[i]
				return fmt.Sprintf("%-38s %s", ms.Id, ms.Name)
			})
		}
		b.WriteString("\n" + dimStyle.Render("u: upload, d: delete, f5: refresh, esc to go back"))

	case screenMissionsUpload:
		b.WriteString(m.viewMissionsUpload())

	case screenAdmin:
		b.WriteString(titleStyle.Render("Admin") + "\n\n")
		if !m.loaded {
			b.WriteString("Loading...")
		} else if m.err != nil {
			b.WriteString(errorStyle.Render(m.err.Error()))
		} else if m.diskUsage != nil {
			b.WriteString(fmt.Sprintf("mods:       %s\n", actions.HumanBytes(m.diskUsage.ModsBytes)))
			b.WriteString(fmt.Sprintf("missions:   %s\n", actions.HumanBytes(m.diskUsage.MissionsBytes)))
			b.WriteString(fmt.Sprintf("game files: %s\n", actions.HumanBytes(m.diskUsage.GameFilesBytes)))
			b.WriteString(fmt.Sprintf("total:      %s\n", actions.HumanBytes(m.diskUsage.TotalBytes)))
		}
		b.WriteString("\n" + dimStyle.Render("e: edit baseline config, r: refresh Steam auth, x: export state, m: import state, f5: refresh, esc to go back"))

	case screenAdminExportState:
		b.WriteString(m.viewAdminExportState())

	case screenAdminImportState:
		b.WriteString(m.viewAdminImportState())

	case screenAccount:
		b.WriteString(m.viewAccount())
	}

	if m.confirm.kind != confirmNone {
		b.WriteString("\n\n" + errorStyle.Render(m.confirm.prompt))
	}

	view := tea.NewView(b.String())
	view.AltScreen = true
	return view
}

func renderLine(s string, selected bool) string {
	if selected {
		return selectedStyle.Render("> " + s)
	}
	return "  " + s
}
