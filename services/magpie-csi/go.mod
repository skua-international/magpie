module github.com/skua-international/magpie/services/magpie-csi

go 1.26.5

require (
	github.com/container-storage-interface/spec v1.12.0
	google.golang.org/grpc v1.82.1
)

require (
	golang.org/x/sys v0.47.0 // indirect
	golang.org/x/text v0.41.0 // indirect
	google.golang.org/genproto/googleapis/rpc v0.0.0-20260715232425-e75dac1f907d // indirect
	google.golang.org/protobuf v1.36.12-0.20260120151049-f2248ac996af // indirect
)

// Same rationale as cli/magpie's: generated/go isn't published to a real
// module proxy, it's regenerated from proto/ in this same monorepo, so
// always build against what's checked out here.
replace github.com/skua-international/magpie/generated/go => ../../generated/go

require (
	connectrpc.com/connect v1.20.0
	github.com/skua-international/magpie/generated/go v0.0.0-00010101000000-000000000000
	golang.org/x/net v0.58.0
)
