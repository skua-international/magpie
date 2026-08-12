package actions

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	registryv1 "github.com/skua-international/magpie/generated/go/registry/v1"
)

func writeTemp(t *testing.T, contents string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "state.json")
	if err := os.WriteFile(path, []byte(contents), 0o644); err != nil {
		t.Fatalf("failed to write fixture: %v", err)
	}
	return path
}

func TestReadStateFileSchemaVersion(t *testing.T) {
	cases := []struct {
		name string
		// Only the version-bearing part matters here; the payload
		// fields are covered by the round-trip test below.
		contents string
		wantErr  bool
		// Substring the error must mention, so a version rejection
		// can't pass on some unrelated parse failure.
		wantErrContains string
	}{
		{
			name:     "current version accepted",
			contents: `{"schemaVersion": 1, "servers": [{"name": "ops"}]}`,
		},
		{
			// The pre-versioning shape, which is exactly v1 -- these
			// files exist in the wild and must stay importable.
			name:     "absent version treated as v1",
			contents: `{"servers": [{"name": "ops"}]}`,
		},
		{
			// protojson omits zero values, so an explicit 0 is
			// indistinguishable from absent and takes the same path.
			name:     "explicit zero treated as v1",
			contents: `{"schemaVersion": 0, "servers": [{"name": "ops"}]}`,
		},
		{
			name:            "future version rejected",
			contents:        `{"schemaVersion": 2, "servers": [{"name": "ops"}]}`,
			wantErr:         true,
			wantErrContains: "upgrade magpiectl",
		},
		{
			name:            "far-future version rejected",
			contents:        `{"schemaVersion": 99}`,
			wantErr:         true,
			wantErrContains: "schema version 99",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got, err := ReadStateFile(writeTemp(t, tc.contents))
			if tc.wantErr {
				if err == nil {
					t.Fatalf("ReadStateFile(%s) = %v, nil; want error", tc.contents, got)
				}
				if !strings.Contains(err.Error(), tc.wantErrContains) {
					t.Fatalf("error %q does not mention %q", err, tc.wantErrContains)
				}
				return
			}
			if err != nil {
				t.Fatalf("ReadStateFile(%s) unexpected error: %v", tc.contents, err)
			}
			if got == nil {
				t.Fatal("ReadStateFile returned nil request without an error")
			}
		})
	}
}

// The version field has to survive WriteStateFile -> ReadStateFile, which
// is the path an operator actually takes. A file written by this build
// must be readable by it.
func TestWriteStateFileRoundTrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "state.json")
	state := &registryv1.ExportStateResponse{
		SchemaVersion: StateSchemaVersion,
		Servers:       []*registryv1.ExportedServer{{Name: "ops", Port: 2302}},
		ConfigMaps:    []*registryv1.ExportedConfigMap{{Name: "baseline"}},
	}
	if err := WriteStateFile(path, state); err != nil {
		t.Fatalf("WriteStateFile: %v", err)
	}

	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("reading back: %v", err)
	}
	// Asserted against the on-disk text, not just the parsed struct:
	// the acceptance criterion is that the exported JSON carries a
	// top-level version field, and protojson omitting a zero value
	// would satisfy a struct-level check while writing nothing.
	if !strings.Contains(string(raw), `"schemaVersion"`) {
		t.Fatalf("exported JSON has no schemaVersion field:\n%s", raw)
	}

	req, err := ReadStateFile(path)
	if err != nil {
		t.Fatalf("ReadStateFile on our own output: %v", err)
	}
	if len(req.Servers) != 1 || req.Servers[0].Name != "ops" {
		t.Fatalf("payload did not survive round trip: %+v", req.Servers)
	}
	if len(req.ConfigMaps) != 1 || req.ConfigMaps[0].Name != "baseline" {
		t.Fatalf("config maps did not survive round trip: %+v", req.ConfigMaps)
	}
}
