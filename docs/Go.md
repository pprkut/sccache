# Go

sccache can serve as the build cache of the go command. It does not wrap the Go
compiler: the go command keeps computing its own cache keys and asks sccache to
store and fetch the results, through the
[`GOCACHEPROG`](https://pkg.go.dev/cmd/go/internal/cacheprog) protocol. This
needs Go 1.24 or later.

```sh
export GOCACHEPROG="sccache --go-cacheprog"
go build ./...
```

`go env -w GOCACHEPROG="sccache --go-cacheprog"` makes the setting permanent for
the current user.

Entries go to the storage sccache is configured with, next to the entries of
the other compilers, and count as `Go` in `sccache --show-stats`. A hit or miss
there is one lookup by the go command, which makes several per package.

What this is good for:

* Builds that start without a Go build cache, such as package builds in a clean
  chroot or CI jobs. The standard library and dependencies shared with earlier
  builds come from the cache as long as the Go version and build flags match.
* Sharing a cache between machines through one of the remote storage backends.

Caveats:

* With `GOCACHEPROG` set, the go command does not use its own cache in
  `GOCACHE` at all. Every lookup goes through sccache.
* The go command reads cached files from disk, so each entry it asks for is
  written to a temporary directory (under `TMPDIR`) that is removed when the go
  command exits. A fully cached build still copies everything it needs there.
* An entry is sent to the server in one message. Entries that do not fit in
  `SCCACHE_MAX_FRAME_LENGTH` (8 MiB when unset) after compression are not
  cached, and are counted as cache write errors. When raising the limit, set it
  for the server and for the go command alike.
* The go command trusts what the cache returns. Anyone who can write to the
  storage can replace the result of a compilation, as with the other compilers.
* Only caching is covered. Go compilations are not distributed.
