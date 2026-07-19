package actions

import (
	"os"

	registryv1 "github.com/skua-international/magpie/generated/go/registry/v1"
	"google.golang.org/protobuf/encoding/protojson"
)

// WriteStateFile serializes an ExportStateResponse to path as indented
// JSON. protojson rather than plain encoding/json -- enums render as
// their string names and field names match the RPC's own wire format
// (camelCase), both easier to read/diff by hand than raw ints/snake_case
// would be, and consistent with what curl against the RPC directly would
// show.
func WriteStateFile(path string, state *registryv1.ExportStateResponse) error {
	data, err := (protojson.MarshalOptions{Multiline: true, Indent: "  "}).Marshal(state)
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0o644)
}

// ReadStateFile parses a file written by WriteStateFile (or any
// ExportState response fetched directly and saved) into what
// ImportState needs. ExportStateResponse and ImportStateRequest happen
// to share their mod_sources/config_maps/servers fields exactly, so this
// reads the export shape and repacks it rather than requiring a
// separate on-disk format for import.
func ReadStateFile(path string) (*registryv1.ImportStateRequest, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var exported registryv1.ExportStateResponse
	if err := protojson.Unmarshal(data, &exported); err != nil {
		return nil, err
	}
	return &registryv1.ImportStateRequest{
		ModSources: exported.ModSources,
		ConfigMaps: exported.ConfigMaps,
		Servers:    exported.Servers,
	}, nil
}
