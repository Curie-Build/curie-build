# snapshot-lib source (test fixture)

Source Maven project for `com.example:snapshot-lib:1.0-SNAPSHOT`, the artifact
that `examples/snapshot-demo` depends on. Use it to publish **new** unique
snapshots into `../local-repo` when testing curie's SNAPSHOT handling.

## Publish a new snapshot

```sh
cd snapshot-lib-src
mvn deploy
```

Each `mvn deploy` writes a new timestamped build
(`snapshot-lib-1.0-YYYYMMDD.HHMMSS-N.jar`) into `../local-repo` and repoints the
version-level `maven-metadata.xml` at it — the exact layout curie resolves and
pins in `Curie.lock`.

> Note: `mvn install` is **not** what you want here — it writes a non-unique
> `1.0-SNAPSHOT` artifact into `~/.m2`, not the timestamped layout under test.

## Testing the resolver / `-U`

```sh
# 1. Publish a newer snapshot build.
cd snapshot-lib-src && mvn deploy && cd ..

# 2. Existing build honours the pinned version in Curie.lock (reproducible):
curie build

# 3. Refresh to the newest snapshot and rewrite Curie.lock:
curie build -U
```

To distinguish builds at runtime, edit the message in
`src/main/java/com/example/SnapshotLib.java` before each `mvn deploy`.
