module github.com/skua-international/magpie/cli

go 1.25.5

// Not published to a real module proxy -- lives in this same monorepo,
// regenerated from proto/ on every release (see .github/workflows/
// release.yml). Always build against whatever's actually checked out
// here, never a stale published version.
replace github.com/skua-international/magpie/generated/go => ../../generated/go
