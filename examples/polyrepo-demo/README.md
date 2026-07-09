# Polyrepo Demo

Demonstrates Curie's polyrepo support — workspace members cloned from remote
Git repositories. This example shows *multi-level recursive* cloning: a
git-sourced member can be a workspace, which itself contains git members
(at any depth).

Simulated remotes are shipped as **git bundle** files. Bundles are ordinary
files, so the Curie monorepo can track them without nested `.git` directories
(which Git would otherwise treat as broken embedded repos / gitlinks).

## Layout

```
polyrepo-demo/
├── Curie.toml                   # workspace root
├── app/                         # local member (application)
│   ├── Curie.toml
│   └── src/...
├── shared-lib-repo.bundle       # simulated remote → cloned to shared-lib/
├── shared-util-repo.bundle      # simulated remote → shared-lib/shared/
├── shared-core-repo.bundle      # simulated remote → shared-lib/shared/core/
└── shared-lib/                  # ← auto-cloned (gitignored; workspace)
    └── shared/                  #   ← nested git clone
        └── core/                #     ← deepest git clone (the library)
            ├── Curie.toml
            └── src/...
```

## How it works

The root `Curie.toml` lists `shared-lib` as a Git member pointing at a bundle:

```toml
[workspace]
members = [
    "app",
    { path = "shared-lib", git = "./shared-lib-repo.bundle", branch = "master" },
]
```

The content of `shared-lib-repo.bundle` is itself a workspace:

```toml
[workspace]
members = [
    { path = "shared", git = "../shared-util-repo.bundle", branch = "master" },
]
```

`shared-util-repo.bundle` adds one more level:

```toml
[workspace]
members = [
    { path = "core", git = "../../shared-core-repo.bundle", branch = "master" },
]
```

When you run `curie build` and `shared-lib/` does not exist, Curie clones
the bundle into `shared-lib/` (a workspace). It then recursively clones the
git members declared inside:

- `shared-util-repo.bundle` → `shared-lib/shared/`
- `shared-core-repo.bundle` → `shared-lib/shared/core/` (the actual library)

Relative bundle paths are resolved against the *declaring* workspace directory
(not the process cwd), so nested clones still find the bundles next to the
demo root.

The app depends on the deepest nested member:

```toml
# app/Curie.toml
[workspace-dependencies]
shared = { path = "../shared-lib/shared/core" }
```

## `missingMembers` policy

By default, missing Git members are cloned automatically (`missingMembers =
"clone"`).  Set `missingMembers = "error"` to get a failure with a manual
`git clone` command instead:

```toml
[workspace]
members = [...]
missingMembers = "error"
```

This policy applies at each level: the outer workspace uses the root policy
(default "clone"); each nested workspace uses its own (or the default
"clone"). Recursion works at any depth.

## Regenerating the bundles

If you need to change the simulated remote content:

```bash
cd examples/polyrepo-demo
rm -rf shared-lib   # drop previous clones

# Expand a bundle, edit, re-pack (example: leaf library)
git clone shared-core-repo.bundle shared-core-repo -b master
# …edit files under shared-core-repo/…
git -C shared-core-repo add -A
git -C shared-core-repo commit -m "update core"
git -C shared-core-repo bundle create ../shared-core-repo.bundle master
rm -rf shared-core-repo
```

Do the same for `shared-util-repo.bundle` / `shared-lib-repo.bundle` if their
`Curie.toml` (or other tracked files) change. Nested bundle paths inside those
workspaces are relative to the clone destination (`shared-lib/…`), so keep
using `../shared-util-repo.bundle` and `../../shared-core-repo.bundle`.