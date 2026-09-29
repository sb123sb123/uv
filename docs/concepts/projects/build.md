# Building distributions

To distribute your project to others (e.g., to upload it to an index like PyPI), you'll need to
build it into a distributable format.

Python projects are typically distributed as both source distributions (sdists) and binary
distributions (wheels). The former is typically a `.tar.gz` or `.zip` file containing the project's
source code along with some additional metadata, while the latter is a `.whl` file containing
pre-built artifacts that can be installed directly.

!!! important

    When using `uv build`, uv acts as a [build frontend](https://peps.python.org/pep-0517/#terminology-and-goals)
    and determines the Python version to use and invokes the build backend. The details of
    the builds, such as the included files and the distribution filenames, are determined by the build
    backend, as defined in [`[build-system]`](./config.md#build-systems). Information about build
    configuration can be found in the respective tool's documentation.

## Using `uv build`

`uv build` can be used to build both source distributions and binary distributions for your project.
By default, `uv build` will build the project in the current directory, and place the built
artifacts in a `dist/` subdirectory:

```console
$ uv build
$ ls dist/
example-0.1.0-py3-none-any.whl
example-0.1.0.tar.gz
```

You can build the project in a different directory by providing a path to `uv build`, e.g.,
`uv build path/to/project`.

`uv build` will first build a source distribution, and then build a binary distribution (wheel) from
that source distribution.

You can limit `uv build` to building a source distribution with `uv build --sdist`, a binary
distribution with `uv build --wheel`, or build both distributions from source with
`uv build --sdist --wheel`.

## Including a dependency lock

!!! warning "Preview feature"

    This feature is experimental and may change. Enable it with
    `--preview-features locked-tools`.

When a project uses `uv_build`, `uv build` includes its runtime dependencies as a `pylock.toml` in
its wheel if a `uv.lock` exists and its dependencies come from PyPI. The source distribution
includes `uv.lock`, so a subsequent `uv build` can include the lock when building a wheel from it.
The project itself is supplied by the wheel and is not listed in `pylock.toml`. Workspace members
may use a shared lock, but cannot have runtime dependencies on other workspace members. The lock
does not include optional extras. Locks restricted to specific environments are not supported.

Set `tool.uv.export-lock = true` to include a lock when using another build backend or package
index, or set it to `false` to disable export. `UV_EXPORT_LOCK` overrides this setting. Enabling
export requires an existing `uv.lock` with URLs and hashes for registry artifacts; Git and local
dependencies are not supported. The entire `uv.lock`, including source locations and metadata for
other workspace members, is included in the source distribution. Review its contents before
publishing it.

uv checks the lock against the workspace without network access. If the check cannot complete
offline, automatic export is skipped and explicitly enabled export fails. A source distribution may
omit other workspace members needed for this check, so a later build from that distribution may not
include the lock. Building the source distribution and wheel together uses the original workspace
for the check.

The export is performed by `uv build`. Other frontends, including `python -m build`, do not include
the lock automatically.

## Build constraints

`uv build` accepts `--build-constraint`, which can be used to constrain the versions of any build
requirements during the build process. When coupled with `--require-hashes`, uv will enforce that
the requirement used to build the project match specific, known hashes, for reproducibility.

For example, given the following `constraints.txt`:

```text
setuptools==68.2.2 --hash=sha256:b454a35605876da60632df1a60f736524eb73cc47bbc9f3f1ef1b644de74fd2a
```

Running the following would build the project with the specified version of `setuptools`, and verify
that the downloaded `setuptools` distribution matches the specified hash:

```console
$ uv build --build-constraint constraints.txt --require-hashes
```

### Project build dependency hashes

Projects can also specify build constraints in
[`build-constraint-dependencies`](../../reference/settings.md#build-constraint-dependencies). To
verify a downloaded build dependency, provide its `requirement` and `hashes` in a table:

```toml
[tool.uv]
build-constraint-dependencies = [
    { requirement = "setuptools==68.2.2", hashes = ["sha256:b454a35605876da60632df1a60f736524eb73cc47bbc9f3f1ef1b644de74fd2a"] },
    "wheel<1",
]
```

An entry without hashes can be written as a string, as shown with `wheel<1` above. uv checks the
supplied hashes when it downloads pinned build dependencies during project resolution or
installation, including builds in `uv run --with` environments. These hashes apply to build
dependencies. Packages installed in the project environment are verified against their own lockfile
hashes.

## Preventing publish to PyPI

If you have internal packages that you do not want to be published, you can mark them as private:

```toml
[project]
classifiers = ["Private :: Do Not Upload"]
```

This setting makes PyPI reject your uploaded package from publishing. It does not affect security or
privacy settings on alternative registries.

We also recommend only generating [per-project PyPI API tokens](https://pypi.org/help/#apitoken):
Without a PyPI token matching the project, it can't be accidentally published.
