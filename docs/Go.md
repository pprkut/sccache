# Go

There are two ways to use sccache with Go. They can be combined.

* As the [build cache of the go command](#build-cache), which is the cheaper
  and more effective way to share compilation results.
* As a [wrapper around the Go compiler](#compiler-wrapper), which is what
  distributed compilation needs.

## Build cache

sccache can serve as the build cache of the go command. Here it does not wrap
the Go compiler: the go command keeps computing its own cache keys and asks
sccache to store and fetch the results, through the
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

## Compiler wrapper

The go command can run the tools of its toolchain through another program:

```sh
go build -toolexec sccache ./...
```

or, for every build, `GOFLAGS=-toolexec=sccache` (use an absolute path if
sccache is not in `PATH`). sccache then sees each invocation of the Go
compiler, `go tool compile`, and treats it like the compilers of other
languages: it looks the result up in its cache and, on a miss, compiles
locally or on a [build server](Distributed.md). The other tools of the
toolchain (`asm`, `cgo`, `link`, ...) run as they are. A C compiler that cgo
invokes goes through sccache as well.

Compilations count as `Go compile` in `sccache --show-stats`.

The cache key covers the compiler binary, the flags, the `GO*` environment
variables the compiler can see, the source files, and of each imported package
the export data. Like the go command, sccache keys on the names the compiler
records for source files, not on where they are: with `go build -trimpath` a
package built in another directory is a hit.

Distributed compilation:

* The client packages the `compile` binary as the toolchain, and the inputs
  of each compilation: the sources, embedded files, and of each imported
  package only the export data, which is a fraction of the package archive.
* Linux clients only. The compiler is handed absolute paths that have to
  exist under the same name on the build server, which rules out clients
  whose paths get translated.
* A Go package usually compiles in a fraction of a second, so the overhead of
  shipping it to a build server weighs more than for C++ or Rust, and linking
  always happens locally. Expect a gain when the client has few cores and the
  build servers have many, and measure.

Caveats:

* The go command already skips compilations it has in its own cache, so a
  cache hit in sccache only happens for packages the go command would
  otherwise have compiled.
* When `GOCACHEPROG` is set, sccache does not store compilation results a
  second time: the go command hands them to its cache program anyway. The
  wrapper then only serves distributed compilation.
* Only one `-toolexec` program can be set, so this cannot be combined with
  another tool that uses it.
* These compilations are not cached and run locally:
  * coverage and profile-guided builds (`-coveragecfg`, `-pgoprofile`),
  * flags that make the compiler print or dump debugging information
    (`-S`, `-m`, `-d`, `-json`, `GOSSAFUNC`, ...),
  * `-linkshared` and `-dynlink`,
  * anything else sccache does not know: an unknown flag is never guessed at.
* Packages with source files generated into the go command's work directory,
  which is the case for packages using cgo, are only hits within the same
  work directory.
