package actions

import "fmt"

// HumanBytes renders n as a human-readable size (e.g. "1.5GiB") -- shared
// by every list rendering across both the CLI (cmd/mods.go, missions.go,
// admin.go) and the TUI (synced_mods.go, model.go) so the two surfaces
// never format sizes differently.
func HumanBytes(n uint64) string {
	const unit = 1024
	if n < unit {
		return fmt.Sprintf("%dB", n)
	}
	div, exp := uint64(unit), 0
	for n2 := n / unit; n2 >= unit; n2 /= unit {
		div *= unit
		exp++
	}
	return fmt.Sprintf("%.1f%ciB", float64(n)/float64(div), "KMGTPE"[exp])
}
