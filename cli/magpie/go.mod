module github.com/skua-international/magpie/cli

go 1.26.5

// Not published to a real module proxy -- lives in this same monorepo,
// regenerated from proto/ on every release (see .github/workflows/
// release.yml). Always build against whatever's actually checked out
// here, never a stale published version.
replace github.com/skua-international/magpie/generated/go => ../../generated/go

// Confirmed-live nil-pointer panic in (*Client).Write's TOCTOU race on
// c.Conn during QR login. Fix submitted upstream as a PR (not yet
// merged) -- pointed at LinkIsGrim/go-steam's fix branch in the
// meantime instead of vendoring the whole tree locally. Drop this
// replace (and the require below) once the upstream PR merges and a
// new upstream version/commit includes it.
replace github.com/0xAozora/go-steam => github.com/LinkIsGrim/go-steam v0.0.0-20260725235643-4e4d92d038ba

require (
	charm.land/bubbletea/v2 v2.0.8
	charm.land/lipgloss/v2 v2.0.5
	connectrpc.com/connect v1.20.0
	github.com/0xAozora/go-steam v0.0.0-20250414150026-b27aac88f1b8
	github.com/mdp/qrterminal/v3 v3.2.1
	github.com/skua-international/magpie/generated/go v0.0.0-00010101000000-000000000000
	github.com/spf13/cobra v1.10.2
	golang.org/x/term v0.45.0
	google.golang.org/protobuf v1.36.12
)

require (
	github.com/charmbracelet/colorprofile v0.4.3 // indirect
	github.com/charmbracelet/ultraviolet v0.0.0-20260713092251-4bee1914c0cf // indirect
	github.com/charmbracelet/x/ansi v0.11.7 // indirect
	github.com/charmbracelet/x/term v0.2.2 // indirect
	github.com/charmbracelet/x/termios v0.1.1 // indirect
	github.com/charmbracelet/x/windows v0.2.2 // indirect
	github.com/clipperhouse/displaywidth v0.11.0 // indirect
	github.com/clipperhouse/uax29/v2 v2.7.0 // indirect
	github.com/inconshreveable/mousetrap v1.1.0 // indirect
	github.com/itchio/lzma v0.0.0-20190703113020-d3e24e3e3d49 // indirect
	github.com/lucasb-eyer/go-colorful v1.4.0 // indirect
	github.com/mattn/go-runewidth v0.0.24 // indirect
	github.com/muesli/cancelreader v0.2.2 // indirect
	github.com/rivo/uniseg v0.4.7 // indirect
	github.com/spf13/pflag v1.0.10 // indirect
	github.com/xo/terminfo v0.0.0-20220910002029-abceb7e1c41e // indirect
	golang.org/x/net v0.57.0 // indirect
	golang.org/x/sync v0.22.0 // indirect
	golang.org/x/sys v0.47.0 // indirect
	rsc.io/qr v0.2.0 // indirect
)
