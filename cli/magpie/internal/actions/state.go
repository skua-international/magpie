package actions

import (
	"fmt"
	"os"

	registryv1 "github.com/skua-international/magpie/generated/go/registry/v1"
	"google.golang.org/protobuf/encoding/protojson"
)

// StateSchemaVersion is the highest export schema this build of magpiectl
// knows how to import. Kept in lockstep with STATE_SCHEMA_VERSION in
// services/registry/src/service/state.rs, which is what actually stamps
// the field -- there's no shared constant across the Rust and Go sides to
// derive it from, so bumping one means bumping the other.
const StateSchemaVersion = 1

// legacySchemaVersion is what an absent schemaVersion is read as. The
// field was added after export-state had already shipped, so files
// written before it exist in the wild with no version at all -- and their
// shape is exactly what version 1 describes, since the field landed
// before any breaking change to that shape. Treating absent as 1 keeps
// those importable instead of forcing a re-export against a cluster the
// operator may no longer have.
//
// This is only sound while StateSchemaVersion == 1. Once it's bumped, an
// unversioned file is a v1 file being handed to an importer expecting
// v2+, and whatever migration or rejection applies to a file that says
// "1" applies to one that says nothing.
const legacySchemaVersion = 1

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
//
// Rejects a file whose schemaVersion is newer than this build supports,
// rather than importing whatever subset it happens to understand:
// protojson silently ignores unknown fields, so a future export would
// otherwise apply cleanly with its new fields dropped on the floor --
// creating servers that differ from the ones exported, with nothing said
// about it. An unversioned file is read as v1; see legacySchemaVersion.
func ReadStateFile(path string) (*registryv1.ImportStateRequest, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var exported registryv1.ExportStateResponse
	if err := protojson.Unmarshal(data, &exported); err != nil {
		return nil, err
	}

	version := exported.SchemaVersion
	if version == 0 {
		version = legacySchemaVersion
	}
	if version > StateSchemaVersion {
		return nil, fmt.Errorf(
			"%s is schema version %d, but this magpiectl only supports up to %d -- "+
				"upgrade magpiectl to import it",
			path, version, StateSchemaVersion,
		)
	}

	return &registryv1.ImportStateRequest{
		ModSources: exported.ModSources,
		ConfigMaps: exported.ConfigMaps,
		Servers:    exported.Servers,
	}, nil
}
