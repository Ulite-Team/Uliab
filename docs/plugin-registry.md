# Plugin registry (`crates/uliab/src/registry.rs`)

The registry client resolves `libs.ulb` `plugins {}` coordinates
(`"ulite/hello" @ "0.4.0"`) to a downloaded, verified, cached wasm
artifact. Plugin coordinates live in a separate namespace from Maven
coordinates, with a separate cache — the two never interact.

## Index format

The default index is `DEFAULT_REGISTRY` in `crates/uliab/src/lib.rs`
(a `Ulite-Team/ulb-plugins` GitHub raw path). The `index.json` schema,
which mirrors `architecture.md` §3.6:

```json
{
  "schema_version": 1,
  "plugins": {
    "ulite/hello": {
      "versions": {
        "0.4.0": {
          "abi": { "min": "0.4", "max": "0.4" },
          "artifact_url": "https://…/hello_plugin.wasm"
        }
      }
    }
  }
}
```

- `plugins` is keyed by plugin name (the coordinate).
- Each version maps to an `abi` range (inclusive) and an `artifact_url`.
- `artifact_url` may be HTTP(S), `file://`, or a relative path — relative
  URLs resolve against the index file's directory, so a filesystem index
  works without a network.

## ABI-range comparison

`AbiRange::contains(version)` is inclusive and compares dot-separated
numeric segments, so `"0.1" == "0.1.0"`. Each published row currently pins
`min == max` to the host ABI at publish time.

## Version selection

Given a `PluginSpec { name, version }`:

1. If the requested version exists **and** its declared range contains the
   host ABI (`ulb_plugin_sdk::ABI_VERSION`), that version is used.
2. Otherwise the client falls back to the **newest compatible build**
   with a warning (the "last-known-compatible" behavior of
   `architecture.md` §3.6): a plugin upgrade is opt-in, and a core ABI
   upgrade never forces a plugin upgrade.
3. If nothing matches, resolution fails with an `Incompatible` error that
   names the host ABI.

## Cache layout and verification

Artifacts cache under:

```
~/.cache/uliab/plugins/<name>/<version>/plugin.wasm
~/.cache/uliab/plugins/<name>/<version>/abi.json
```

`abi.json` records the ABI range that was verified at fetch time. A cached
build is reused only while that recorded range still contains the host
ABI; if the host outgrew it, the artifact is refetched.

**Every** downloaded artifact is verified before it is trusted: the client
instantiates the component through `PluginHost::manifest_of_bytes` and
cross-checks the manifest's `name`, `version`, and `abi-version` against
the index row. A cached artifact that fails this check is not used.

## CLI surface

- `uliab plugins list` — show what `libs.ulb` declares.
- `uliab plugins resolve` — resolve (and cache) the declared builds.

Both accept `--project`, `--registry`, and `--cache-dir` so resolution can
point at a local index and offline cache.
