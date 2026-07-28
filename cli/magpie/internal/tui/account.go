package tui

import (
	"fmt"
	"strings"

	tea "charm.land/bubbletea/v2"

	"github.com/skua-international/magpie/cli/internal/auth"
)

var linkProviders = []string{"steam", "discord", "github", "google"}

type accountState struct {
	cursor  int
	linking bool // browser round trip in flight -- keys are ignored until it resolves
	result  string
	err     error
}

type accountLinkedMsg struct {
	provider string
	err      error
}

// linkAccountCmd blocks (opening the system browser and waiting on the
// local OAuth callback, see auth.LinkAccount's own doc) until the round
// trip completes -- same shape as create_server.go's kubectl-edit
// suspension, except this one is a plain blocking tea.Cmd rather than
// tea.ExecProcess, since there's no subprocess to hand the terminal to:
// the "waiting" state is just this screen's own spinner-less status
// line while the browser does its thing out-of-band.
func (m Model) linkAccountCmd(provider string) tea.Cmd {
	return func() tea.Msg {
		fresh, err := auth.LinkAccount(m.ctx, m.apiURL, provider, m.accessToken)
		if err != nil {
			return accountLinkedMsg{provider: provider, err: err}
		}
		if err := auth.Save(fresh); err != nil {
			return accountLinkedMsg{provider: provider, err: fmt.Errorf("linked, but failed to save the refreshed session: %w", err)}
		}
		return accountLinkedMsg{provider: provider}
	}
}

func (m Model) handleAccountKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	if m.account.linking {
		return m, nil
	}
	switch msg.String() {
	case "up", "k":
		if m.account.cursor > 0 {
			m.account.cursor--
		}
	case "down", "j":
		if m.account.cursor < len(linkProviders)-1 {
			m.account.cursor++
		}
	case "enter":
		provider := linkProviders[m.account.cursor]
		m.account.linking, m.account.result, m.account.err = true, "", nil
		return m, m.linkAccountCmd(provider)
	}
	return m, nil
}

func (m Model) viewAccount() string {
	var b strings.Builder
	b.WriteString(titleStyle.Render("Account") + "\n\n")
	b.WriteString("Link another login provider to this account:\n\n")
	for i, p := range linkProviders {
		b.WriteString(renderLine(p, i == m.account.cursor) + "\n")
	}
	if m.account.linking {
		b.WriteString("\nOpening browser -- complete the login there to finish linking...\n")
	} else if m.account.err != nil {
		b.WriteString("\n" + errorStyle.Render(m.account.err.Error()) + "\n")
	} else if m.account.result != "" {
		b.WriteString("\n" + m.account.result + "\n")
	}
	b.WriteString("\n" + dimStyle.Render("enter to link, esc to go back"))
	return b.String()
}
