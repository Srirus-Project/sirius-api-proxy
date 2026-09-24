# Protocol compilation and hot reload

## Native and dynamic codecs

The build script and runtime share the source snapshot compiler in `src/proto_source.rs`.
protox compiles the bundle, prost-build generates concrete Rust messages and pbjson-build
generates Protobuf JSON codecs. Allowlisted dispatch is compiled into the executable;
generated files stay in Cargo OUT_DIR. No external protoc is required.

At startup and reload, an exact semantic fingerprint match selects `native`; otherwise
compatible definitions select `dynamic` through prost-reflect. The fingerprint includes the
protocol label and compiled descriptors, not filesystem paths or comments. Adding fields
switches to dynamic so they are not silently lost by generated types. Rebuild and deploy
with the updated bundle to restore native dispatch. Hot reload does not compile Rust or
load executable code. Transport remains binary Protobuf over HTTP/2 gRPC in both modes.

Unknown enum numbers can require a dynamic JSON fallback within a native generation because
pbjson cannot represent them. The fallback uses the same pinned schema and validates the
input; binary decode errors still fail. `codec: native` describes the preferred path, not a
promise that every input avoids reflection. Performance differences have not been benchmarked.

## Bundle layout

```text
protocol/sirius/1.0.3/
  bundle.json                 # {"version":"1.0.3"}
  proto/
    app/...
    entity/...
    google/protobuf/descriptor.proto
```

`protocol_directory` is relative to the process working directory unless absolute. Release
archives and Docker images include the bundle. All imports must resolve inside its snapshot.
Missing/invalid definitions fail startup; a failed reload preserves the active definitions.
`bundle.json` does not change client_version, endpoint or authentication settings.
The reduced test descriptor is an independent baseline and is never a runtime input.

## Activation

Write a complete candidate bundle before requesting reload. No filesystem watcher, upload
endpoint or caller-controlled directory selection is provided.

```sh
curl --fail-with-body -H "Authorization: Bearer $SIRIUS_INTERNAL_TOKEN" \
  http://127.0.0.1:9999/internal/v1/protocol
curl --fail-with-body -X POST -H "Authorization: Bearer $SIRIUS_INTERNAL_TOKEN" \
  http://127.0.0.1:9999/internal/v1/protocol/reload
```

Responses expose version, sha256, generation, loaded_at, files, source, codec and native_sha256.
Unchanged content retains its generation. Compilation/compatibility failures return 422.
The public API token cannot reload definitions. Compilation runs off the asynchronous executor;
activation waits for current logical requests, keeping Version/Whoami/business calls on one
schema. Reloads serialize so an older candidate cannot overwrite a newer activation.

For deployment, use immutable bundle directories and atomically replace a `current` root
symlink before reload. The root link is resolved once; internal symlinks are rejected.
The compiler operates on captured source bytes, with limits of 512 proto files, 4 MiB per file,
32 MiB total and depth 16. Changing a YAML path requires restart; switching the configured
root link does not. Docker deployments can mount an external bundle at the configured path.

## Compatibility and rollback

Existing RPC signatures, authentication annotations and unary behavior must remain unchanged.
Existing reachable fields must retain numbers, names, JSON names, types, cardinality, presence,
oneof, packing and defaults; existing enum values must remain compatible. Compatible fields
and enum values may be added. New required fields, deleted fields and changed authentication
are rejected. Additional services do not automatically become HTTP routes.

This is conservative compatibility validation, not a general protocol migration engine.
A schema that added fields cannot hot-reload backward to a schema missing those fields;
stop the service and deploy the validated old executable/bundle pair for that rollback.
Activation invalidates Master version observations and marks old resource snapshots stale.
The next account query bootstraps Version again. A new protocol label must be supported and
independently validated by the asset updater; editing a label alone does not add game support.

Tests compare the independent descriptor baseline with all source/native RPCs, including
nested responses, large integers, enum behavior and optional presence. Local HTTP/2 fixtures
cover generation pinning, compatible reload, failed reload rollback, idempotence, token scopes,
resource invalidation and atomic directory switching. Dependency closure checks prohibit
unused game RPCs/types and custom options other than skip_authentication.

## Protocol families

JP manifests may omit `family` (default `jp`). Global manifests must specify
`{"version":"1.0.1","family":"global"}`. The selected region fixes the family; startup
and reload reject mismatches. Each family has an independently generated native fingerprint.
Changing family or region requires a separately configured instance, not a hot reload.
The Global route allowlist currently contains only Version and GetServerList.
