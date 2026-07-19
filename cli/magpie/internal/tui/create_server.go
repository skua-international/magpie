package tui

import (
	"fmt"
	"strings"
	"unicode"

	tea "charm.land/bubbletea/v2"

	controllerv1 "github.com/skua-international/magpie/generated/go/controller/v1"
	registryv1 "github.com/skua-international/magpie/generated/go/registry/v1"

	"github.com/skua-international/magpie/cli/internal/actions"
)

// createStep is screenServersCreate's own little wizard, one field at a
// time -- there's no bubbles/textinput or form library vendored here
// (see model.go's own hand-rolled cursor/list handling), so this follows
// the same "just handle the keys" style rather than pulling one in for
// this alone.
type createStep int

const (
	createStepName createStep = iota
	createStepPort
	createStepModSources
	createStepConfigMapConfirm
	createStepConfigMapName
	createStepWorking // ensuring/editing the ConfigMap, or submitting -- no key handling
	createStepDone
)

type createServerState struct {
	step createStep
	name string
	port string

	modSources       []*registryv1.ModSourceInfo
	modSourcesLoaded bool
	modCursor        int
	modSelected      map[string]bool

	configMap string

	err    error
	result *controllerv1.ServerInfo
}

func newCreateServerState() createServerState {
	return createServerState{modSelected: map[string]bool{}}
}

// --- messages

type createModSourcesLoadedMsg struct {
	sources []*registryv1.ModSourceInfo
	err     error
}

type configMapEnsuredMsg struct{ err error }

type configMapEditDoneMsg struct{ err error }

type serverCreatedMsg struct {
	info *controllerv1.ServerInfo
	err  error
}

// --- commands

func (m Model) loadCreateModSourcesCmd() tea.Cmd {
	return func() tea.Msg {
		sources, err := actions.ListModSources(m.ctx, m.clients)
		return createModSourcesLoadedMsg{sources: sources, err: err}
	}
}

func (m Model) ensureConfigMapCmd() tea.Cmd {
	namespace, name := m.namespace, m.create.configMap
	return func() tea.Msg {
		return configMapEnsuredMsg{err: actions.EnsureConfigMapExists(m.ctx, namespace, name)}
	}
}

// editConfigMapCmd suspends the whole TUI to run `kubectl edit` --
// tea.ExecProcess releases the terminal, runs it, then restores the
// alt-screen and resumes the Program, same mechanism any editor/pager
// launched from inside a Bubble Tea program uses.
func (m Model) editConfigMapCmd() tea.Cmd {
	cmd := actions.ConfigMapEditCmd(m.ctx, m.namespace, m.create.configMap)
	return tea.ExecProcess(cmd, func(err error) tea.Msg {
		return configMapEditDoneMsg{err: err}
	})
}

func (m Model) submitCreateServerCmd() tea.Cmd {
	params := actions.CreateServerParams{
		Name:         m.create.name,
		Port:         parsePortOr(m.create.port, 2302),
		ModSourceIDs: selectedModSourceIDs(m.create.modSources, m.create.modSelected),
		ConfigMap:    m.create.configMap,
	}
	return func() tea.Msg {
		info, err := actions.CreateServer(m.ctx, m.clients, params)
		return serverCreatedMsg{info: info, err: err}
	}
}

func selectedModSourceIDs(sources []*registryv1.ModSourceInfo, selected map[string]bool) []string {
	var ids []string
	for _, s := range sources {
		if selected[s.Id] {
			ids = append(ids, s.Id)
		}
	}
	return ids
}

func parsePortOr(s string, fallback uint32) uint32 {
	if s == "" {
		return fallback
	}
	var n uint32
	for _, r := range s {
		if !unicode.IsDigit(r) {
			return fallback
		}
		n = n*10 + uint32(r-'0')
	}
	if n == 0 {
		return fallback
	}
	return n
}

func trimLastRune(s string) string {
	if s == "" {
		return s
	}
	r := []rune(s)
	return string(r[:len(r)-1])
}

// --- key handling, called from handleKey for screenServersCreate

func (m Model) handleCreateServerKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	switch m.create.step {
	case createStepName:
		switch msg.String() {
		case "enter":
			if strings.TrimSpace(m.create.name) != "" {
				m.create.step = createStepPort
			}
		case "backspace":
			m.create.name = trimLastRune(m.create.name)
		default:
			if msg.Text != "" {
				m.create.name += msg.Text
			}
		}

	case createStepPort:
		switch msg.String() {
		case "enter":
			m.create.step = createStepModSources
			if !m.create.modSourcesLoaded {
				return m, m.loadCreateModSourcesCmd()
			}
		case "backspace":
			m.create.port = trimLastRune(m.create.port)
		default:
			if msg.Text != "" && unicode.IsDigit([]rune(msg.Text)[0]) {
				m.create.port += msg.Text
			}
		}

	case createStepModSources:
		n := len(m.create.modSources)
		switch msg.String() {
		case "up", "k":
			if m.create.modCursor > 0 {
				m.create.modCursor--
			}
		case "down", "j":
			if n > 0 && m.create.modCursor < n-1 {
				m.create.modCursor++
			}
		case "space":
			if n > 0 {
				id := m.create.modSources[m.create.modCursor].Id
				m.create.modSelected[id] = !m.create.modSelected[id]
			}
		case "enter":
			m.create.step = createStepConfigMapConfirm
		}

	case createStepConfigMapConfirm:
		switch msg.String() {
		case "y", "Y":
			m.create.configMap = m.create.name + "-config"
			m.create.step = createStepConfigMapName
		case "n", "N", "enter":
			m.create.step = createStepWorking
			return m, m.submitCreateServerCmd()
		}

	case createStepConfigMapName:
		switch msg.String() {
		case "enter":
			if strings.TrimSpace(m.create.configMap) != "" {
				m.create.step = createStepWorking
				return m, m.ensureConfigMapCmd()
			}
		case "backspace":
			m.create.configMap = trimLastRune(m.create.configMap)
		default:
			if msg.Text != "" {
				m.create.configMap += msg.Text
			}
		}

	case createStepDone:
		if msg.String() == "enter" {
			m.screen, m.cursor, m.loaded, m.err = screenServers, 0, false, nil
			return m, m.loadServersCmd()
		}
	}
	return m, nil
}

// --- message handling, called from Update

func (m Model) handleCreateServerMsg(msg tea.Msg) (Model, tea.Cmd, bool) {
	switch msg := msg.(type) {
	case createModSourcesLoadedMsg:
		m.create.modSourcesLoaded = true
		m.create.modSources, m.create.err = msg.sources, msg.err
		return m, nil, true

	case configMapEnsuredMsg:
		if msg.err != nil {
			m.create.err = msg.err
			m.create.step = createStepConfigMapName
			return m, nil, true
		}
		return m, m.editConfigMapCmd(), true

	case configMapEditDoneMsg:
		// A non-nil err here just means `kubectl edit` itself failed to
		// launch/run (e.g. $EDITOR unset) -- not fatal to server
		// creation, same as the plain-CLI prompt's own warn-and-continue
		// (see cmd/armaconfig.go's maybePromptServerConfigMap). The
		// ConfigMap still exists (created by ensureConfigMapCmd right
		// before this), just possibly still empty/baseline-only.
		return m, m.submitCreateServerCmd(), true

	case serverCreatedMsg:
		m.create.result, m.create.err = msg.info, msg.err
		if msg.err != nil {
			m.create.step = createStepConfigMapConfirm // back up to a re-triable step
		} else {
			m.create.step = createStepDone
		}
		return m, nil, true
	}
	return m, nil, false
}

// --- view, called from View for screenServersCreate

func (m Model) viewCreateServer() string {
	var b strings.Builder
	b.WriteString(titleStyle.Render("New Server") + "\n\n")

	fmt.Fprintf(&b, "name: %s%s\n", m.create.name, cursorSuffix(m.create.step == createStepName))
	if m.create.step > createStepName {
		fmt.Fprintf(&b, "port: %s%s\n", portOrDefault(m.create.port), cursorSuffix(m.create.step == createStepPort))
	}

	if m.create.step > createStepPort {
		b.WriteString("\nmod sources (space to toggle, enter to continue):\n")
		if !m.create.modSourcesLoaded {
			b.WriteString("  Loading...\n")
		} else if m.create.err != nil {
			b.WriteString("  " + errorStyle.Render(m.create.err.Error()) + "\n")
		} else if len(m.create.modSources) == 0 {
			b.WriteString(dimStyle.Render("  none registered -- server will start with no mods") + "\n")
		} else if m.create.step == createStepModSources {
			for i, s := range m.create.modSources {
				box := "[ ]"
				if m.create.modSelected[s.Id] {
					box = "[x]"
				}
				line := fmt.Sprintf("%s %-38s %s", box, s.Id, s.Reference)
				b.WriteString(renderLine(line, i == m.create.modCursor) + "\n")
			}
		} else {
			b.WriteString(fmt.Sprintf("  %d selected\n", len(selectedModSourceIDs(m.create.modSources, m.create.modSelected))))
		}
	}

	if m.create.step > createStepModSources && m.create.step != createStepDone {
		b.WriteString("\nconfigure a per-server config override now? [y/N]")
		if m.create.step >= createStepConfigMapName {
			fmt.Fprintf(&b, " y\nconfig map name: %s%s\n", m.create.configMap, cursorSuffix(m.create.step == createStepConfigMapName))
		} else {
			b.WriteString("\n")
		}
	}

	switch m.create.step {
	case createStepWorking:
		b.WriteString("\nWorking...\n")
	case createStepDone:
		b.WriteString("\n")
		if m.create.result != nil {
			b.WriteString(fmt.Sprintf("created %s (port %d)\n", m.create.result.Id, m.create.result.Port))
		}
		b.WriteString(dimStyle.Render("enter to go back to the servers list"))
	}
	if m.create.err != nil && m.create.step != createStepModSources {
		b.WriteString("\n" + errorStyle.Render(m.create.err.Error()) + "\n")
	}

	b.WriteString("\n" + dimStyle.Render("esc to cancel"))
	return b.String()
}

func cursorSuffix(active bool) string {
	if active {
		return "_"
	}
	return ""
}

func portOrDefault(s string) string {
	if s == "" {
		return "2302 (default)"
	}
	return s
}
