//! Select a tool before reading its packaged lock, without resolving its dependencies.

use std::io;
use std::sync::Arc;

use anyhow::{Context, bail};
use futures::stream::FuturesUnordered;
use futures::{StreamExt, TryStreamExt};

use uv_cache::Cache;
use uv_client::{
    BaseClientBuilder, Connectivity, MetadataFormat, RegistryClient, RegistryClientBuilder,
    VersionFiles,
};
use uv_configuration::{
    Concurrency, Constraints, HashCheckingMode, NoBinary, NoBuild, TargetTriple,
};
use uv_dispatch::BuildDispatch;
use uv_distribution::{DistributionDatabase, LocalWheel, LoweredExtraBuildDependencies};
use uv_distribution_filename::{DistExtension, DistFilename};
use uv_distribution_types::{
    ArchiveHashGroups, ArchiveHashPolicy, BuiltDist, CachedDist, Dist, Edge, Hashed, Index,
    IndexCapabilities, IndexLocations, IndexMetadataRef, IndexUrl, Name,
    NameRequirementSpecification, Node, PYPI_URL, RequirementSource, Resolution, ResolvedDist,
    SourceDist, UnresolvedRequirement,
};
use uv_lock::PylockToml;
use uv_metadata::{find_flat_dist_info, read_flat_wheel_metadata};
use uv_pep440::{Version, VersionSpecifier, VersionSpecifiers};
use uv_platform_tags::Tags;
use uv_preview::{Preview, PreviewFeature};
use uv_pypi_types::{HashAlgorithm, HashDigest, HashDigests};
use uv_python::{Interpreter, PythonEnvironment};
use uv_requirements::{RequirementsSource, RequirementsSpecification};
use uv_resolver::FlatIndex;
use uv_types::{BuildIsolation, HashStrategy, SourceTreeEditablePolicy};
use uv_workspace::WorkspaceCache;

use crate::commands::pip::loggers::SummaryResolveLogger;
use crate::commands::pip::resolution_tags;
use crate::commands::project::{EnvironmentResolution, PlatformState, resolve_environment};
use crate::commands::pylock::resolve_pylock_toml;
use crate::printer::Printer;
use crate::settings::ResolverSettings;

pub(super) fn check_preview(locked: bool, preview: Preview) -> anyhow::Result<()> {
    if locked && !preview.is_enabled(PreviewFeature::LockedTools) {
        bail!("`--locked` for tools requires the `locked-tools` preview feature");
    }
    Ok(())
}

pub(super) fn check_tool_requirement(requirement: &UnresolvedRequirement) -> anyhow::Result<()> {
    match requirement.source().as_ref() {
        RequirementSource::Registry { .. }
        | RequirementSource::Url {
            ext: DistExtension::Wheel,
            ..
        }
        | RequirementSource::Path {
            ext: DistExtension::Wheel,
            ..
        } => Ok(()),
        RequirementSource::Url {
            ext: DistExtension::Source(_),
            ..
        }
        | RequirementSource::Path {
            ext: DistExtension::Source(_),
            ..
        }
        | RequirementSource::GitDirectory { .. }
        | RequirementSource::GitPath { .. }
        | RequirementSource::Directory { .. } => {
            bail!("A wheel is required to install a tool with `--locked`")
        }
    }
}

pub(super) fn check_arguments(
    locked: bool,
    with: &[RequirementsSource],
    overrides: &[RequirementsSource],
    excludes: &[RequirementsSource],
) -> anyhow::Result<()> {
    if locked && !with.is_empty() {
        bail!("`--locked` requires a single tool package and cannot be combined with `--with`");
    }
    if locked && (!overrides.is_empty() || !excludes.is_empty()) {
        bail!("`--locked` cannot be combined with dependency overrides or exclusions");
    }
    Ok(())
}

pub(super) async fn resolve(
    spec: RequirementsSpecification,
    interpreter: &Interpreter,
    python_platform: Option<&TargetTriple>,
    build_constraints: &Constraints,
    settings: &ResolverSettings,
    client_builder: &BaseClientBuilder<'_>,
    state: &PlatformState,
    concurrency: &Concurrency,
    cache: &Cache,
    workspace_cache: &WorkspaceCache,
    printer: Printer,
    preview: Preview,
) -> anyhow::Result<Resolution> {
    if spec.pylock.is_some() || !spec.source_trees.is_empty() {
        bail!("`--locked` cannot be combined with additional requirements files");
    }
    let [requirement] = spec.requirements.as_slice() else {
        bail!("`--locked` requires a single tool package and cannot be combined with `--with`");
    };
    check_tool_requirement(&requirement.requirement)?;
    let UnresolvedRequirement::Named(requirement) = &requirement.requirement else {
        bail!("Expected a named tool requirement");
    };
    let requirement = requirement.clone();
    if settings.build_options.no_binary_package(&requirement.name) {
        bail!("`--locked` requires a wheel for the tool, but binary distributions are disabled");
    }
    let mut selection_settings = settings.clone();
    selection_settings.build_options = selection_settings.build_options.combine(
        NoBinary::None,
        NoBuild::Packages(vec![requirement.name.clone()]),
    );
    if !spec.overrides.is_empty()
        || !spec.override_dependencies.is_empty()
        || !spec.excludes.is_empty()
    {
        bail!("`--locked` cannot be combined with dependency overrides or exclusions");
    }
    let client = RegistryClientBuilder::new(
        client_builder.clone().keyring(settings.keyring_provider),
        cache.clone(),
    )
    .index_locations(settings.index_locations.clone())
    .index_strategy(settings.index_strategy)
    .markers(interpreter.markers())
    .platform(interpreter.platform())
    .build()?;
    let cached_client = RegistryClientBuilder::new(
        client_builder
            .clone()
            .connectivity(Connectivity::Offline)
            .keyring(settings.keyring_provider),
        cache.clone(),
    )
    .index_locations(settings.index_locations.clone())
    .index_strategy(settings.index_strategy)
    .markers(interpreter.markers())
    .platform(interpreter.platform())
    .build()?;
    let flat_index = FlatIndex::load(&client, cache, &settings.index_locations).await?;
    let environment;
    let build_isolation = match &settings.build_isolation {
        uv_configuration::BuildIsolation::Isolate => BuildIsolation::Isolated,
        uv_configuration::BuildIsolation::Shared => {
            environment = PythonEnvironment::from_interpreter(interpreter.clone());
            BuildIsolation::Shared(&environment)
        }
        uv_configuration::BuildIsolation::SharedPackage(packages) => {
            environment = PythonEnvironment::from_interpreter(interpreter.clone());
            BuildIsolation::SharedPackage(&environment, packages)
        }
    };
    let extra_build_requires =
        LoweredExtraBuildDependencies::from_non_lowered(settings.extra_build_dependencies.clone())
            .into_inner();
    let build_hasher = HashStrategy::from_constraints(
        build_constraints,
        Some(&interpreter.to_resolver_marker_environment()),
        HashCheckingMode::Verify,
    )?;
    let dispatch = BuildDispatch::new(
        &client,
        cache,
        build_constraints,
        interpreter,
        &settings.index_locations,
        &flat_index,
        &settings.dependency_metadata,
        state.clone().into_inner(),
        settings.index_strategy,
        &settings.config_setting,
        &settings.config_settings_package,
        build_isolation,
        &extra_build_requires,
        &settings.extra_build_variables,
        settings.link_mode,
        &settings.build_options,
        &build_hasher,
        settings.exclude_newer.clone(),
        settings.sources.clone(),
        SourceTreeEditablePolicy::Tool,
        workspace_cache.clone(),
        concurrency.clone(),
        preview,
    );
    let tags = resolution_tags(None, python_platform, interpreter)?;
    let markers = crate::commands::pip::resolution_markers(None, python_platform, interpreter);
    let mut root_hashes = spec
        .constraints
        .iter()
        .filter(|constraint| {
            constraint.requirement.name == requirement.name
                && constraint
                    .requirement
                    .evaluate_markers(Some(markers.markers()), &[])
                && !constraint.hashes.is_empty()
        })
        .map(|constraint| {
            constraint
                .hashes
                .iter()
                .map(|hash| hash.parse::<uv_pypi_types::HashDigest>())
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(hashes) = requirement.hashes()? {
        root_hashes.push(HashDigests::from(hashes).to_vec());
    }
    let mut spec = spec;
    let database =
        DistributionDatabase::new(&client, &dispatch, concurrency.downloads_semaphore.clone());
    let (selected, wheel, root_hash) = loop {
        let selected = Resolution::from(
            resolve_environment(
                spec.clone().into(),
                EnvironmentResolution::Direct,
                interpreter,
                python_platform,
                SourceTreeEditablePolicy::Tool,
                build_constraints.clone(),
                &selection_settings,
                client_builder,
                state,
                Box::new(SummaryResolveLogger),
                concurrency,
                cache,
                workspace_cache,
                printer,
                preview,
            )
            .await?,
        );
        let selected_dist = selected
            .distributions()
            .next()
            .context("No compatible tool was selected")?;
        let ResolvedDist::Installable { dist, .. } = selected_dist else {
            bail!("Expected an installable tool distribution");
        };
        if let Dist::Source(_) = dist.as_ref() {
            bail!("A wheel is required to install a tool with `--locked`");
        }

        let hasher = HashStrategy::from_resolution(&selected, HashCheckingMode::Verify)?;
        let (wheel, root_hash) = verify_hash_constraints(
            &database,
            dist,
            &tags,
            hasher.archive_policy(dist.as_ref()),
            &root_hashes,
            true,
        )
        .await?;
        let wheel = CachedDist::from(wheel);

        let metadata = read_flat_wheel_metadata(wheel.filename(), wheel.path())?;
        if let Some(requires_python) = metadata.requires_python.as_ref()
            && !requires_python.contains(interpreter.python_version())
        {
            // Index metadata may omit Requires-Python, particularly for --find-links.
            // Eliminate incompatible releases before considering their packaged locks.
            let mut excluded = requirement.clone();
            if let RequirementSource::Registry { specifier, .. } = &mut excluded.source
                && dist.index().is_some()
            {
                *specifier =
                    VersionSpecifier::not_equals_version(wheel.filename().version.clone()).into();
                spec.constraints
                    .push(NameRequirementSpecification::from(excluded));
                continue;
            }
            bail!(
                "`{selected_dist}` requires Python `{requires_python}`, but the selected interpreter is Python {}",
                interpreter.python_version()
            );
        }
        break (selected, wheel, root_hash);
    };
    let selected_dist = selected
        .distributions()
        .next()
        .context("No compatible tool was selected")?;
    let dist_info = find_flat_dist_info(wheel.filename(), wheel.path())?;
    let dist_info = wheel.path().join(format!("{dist_info}.dist-info"));
    let contents = match fs_err::read_to_string(dist_info.join("pylock.toml")) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            bail!(
                "`{selected_dist}` does not contain `pylock.toml` in its `.dist-info` directory; `--locked` requires a packaged lock"
            );
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!("Failed to read the packaged lock for `{selected_dist}`")
            });
        }
    };
    let mut lock: PylockToml = toml::from_str(&contents)
        .with_context(|| format!("`{selected_dist}` contains an invalid `pylock.toml`"))?;
    if lock.has_missing_hashes() {
        bail!(
            "The packaged lock for `{selected_dist}` is missing artifact hashes; regenerate the lock before publishing the package"
        );
    }
    for extra in &requirement.extras {
        if !lock.extras.contains(extra) {
            bail!(
                "The packaged lock for `{}` does not support the extra `{extra}`",
                requirement.name
            );
        }
    }
    for group in &requirement.groups {
        if !lock.dependency_groups.contains(group) {
            bail!(
                "The packaged lock for `{}` does not support the dependency group `{group}`",
                requirement.name
            );
        }
    }
    let mut groups = lock.default_groups.clone();
    groups.extend(requirement.groups.iter().cloned());
    groups.sort_unstable();
    groups.dedup();
    let indexed_artifacts = validate_artifact_urls(
        &lock,
        &client,
        &cached_client,
        &settings.index_locations,
        concurrency,
    )
    .await
    .with_context(|| {
        format!("The packaged lock for `{selected_dist}` contains unverified artifacts")
    })?;
    // Use the user's configured index, not the publisher-supplied URL, for cache identity.
    for package in &mut lock.packages {
        package.index = Some(
            configured_index(package.index.as_ref(), &settings.index_locations)?
                .without_credentials()
                .into_owned(),
        );
    }
    let (dependencies, _) = resolve_pylock_toml(
        lock,
        &dist_info,
        interpreter,
        None,
        python_platform,
        &requirement.extras,
        &groups,
        &settings.build_options,
        Some(HashCheckingMode::Verify),
    )?;
    if dependencies
        .distributions()
        .any(|dist| dist.name() == &requirement.name)
    {
        bail!(
            "The packaged lock for `{}` must not include the tool itself",
            requirement.name
        );
    }

    let mut graph = dependencies.graph().clone();
    for node in graph.node_weights_mut() {
        let Node::Dist {
            dist,
            hashes,
            install: true,
            ..
        } = node
        else {
            continue;
        };
        let secure_hashes: Vec<_> = hashes
            .iter()
            .filter(|hash| hash.algorithm() != HashAlgorithm::Md5)
            .cloned()
            .collect();
        if secure_hashes.is_empty() {
            bail!("The packaged lock for `{selected_dist}` has no secure hash for `{dist}`");
        }
        *hashes = secure_hashes.into();
    }
    let mut dependency_hashes = std::collections::HashMap::new();
    for constraint in &spec.constraints {
        let requirement = &constraint.requirement;
        if !requirement.evaluate_markers(Some(markers.markers()), &[]) {
            continue;
        }
        let Some(Node::Dist { dist, .. }) = graph.node_weights_mut().find(|node| match node {
            Node::Dist { dist, install, .. } => *install && dist.name() == &requirement.name,
            Node::Root => false,
        }) else {
            continue;
        };
        let matches = match &requirement.source {
            RequirementSource::Registry {
                specifier, index, ..
            } => {
                index.is_none()
                    && dist
                        .version()
                        .is_some_and(|version| specifier.contains(version))
            }
            RequirementSource::Url { .. }
            | RequirementSource::GitDirectory { .. }
            | RequirementSource::GitPath { .. }
            | RequirementSource::Directory { .. }
            | RequirementSource::Path { .. } => false,
        };
        if !matches {
            bail!(
                "The packaged lock selects `{dist}`, which is incompatible with constraint `{requirement}`"
            );
        }
        if !constraint.hashes.is_empty() {
            let expected = constraint
                .hashes
                .iter()
                .map(|hash| hash.parse::<uv_pypi_types::HashDigest>())
                .collect::<Result<Vec<_>, _>>()?;
            dependency_hashes
                .entry(requirement.name.clone())
                .or_insert_with(Vec::new)
                .push(expected);
        }
    }
    let nodes = graph
        .node_indices()
        .filter_map(|index| match &graph[index] {
            Node::Dist {
                dist,
                hashes,
                install: true,
                ..
            } => Some((index, dist.clone(), hashes.clone())),
            Node::Dist { install: false, .. } | Node::Root => None,
        })
        .collect::<Vec<_>>();
    let verify_node = async |(node_index, dist, mut hashes): (_, ResolvedDist, HashDigests)| {
        let mut constraints = dependency_hashes
            .get(dist.name())
            .cloned()
            .unwrap_or_default();
        let ResolvedDist::Installable { dist, .. } = &dist else {
            bail!("Expected an installable locked dependency");
        };
        let (index, filename, file) = match dist.as_ref() {
            Dist::Built(BuiltDist::Registry(wheels)) => {
                let wheel = wheels.best_wheel();
                (
                    &wheel.index,
                    DistFilename::WheelFilename(wheel.filename.clone()),
                    &wheel.file,
                )
            }
            Dist::Source(SourceDist::Registry(sdist)) => {
                let filename = DistFilename::try_from_filename(&sdist.file.filename, &sdist.name)
                    .context("Invalid locked source distribution filename")?;
                (&sdist.index, filename, &sdist.file)
            }
            Dist::Built(BuiltDist::DirectUrl(_) | BuiltDist::Path(_) | BuiltDist::GitPath(_))
            | Dist::Source(
                SourceDist::DirectUrl(_)
                | SourceDist::GitDirectory(_)
                | SourceDist::GitPath(_)
                | SourceDist::Path(_)
                | SourceDist::Directory(_),
            ) => bail!("Expected a registry distribution for a locked dependency"),
        };
        let mut url = file.url.to_url()?;
        url.set_fragment(None);
        let mut matching = indexed_artifacts
            .iter()
            .filter(|artifact| {
                artifact.index.as_str().trim_end_matches('/')
                    == index.without_credentials().as_str().trim_end_matches('/')
                    && artifact.url == url
                    && artifact.filename == filename
            })
            .peekable();
        if matching.peek().is_none() {
            bail!("The selected artifact for `{dist}` is not listed by {index}");
        }
        for artifact in matching {
            if let Some(requires_python) = &artifact.requires_python
                && !requires_python.contains(&markers.markers().python_full_version().version)
            {
                bail!(
                    "`{dist}` requires Python `{requires_python}`, but the selected interpreter is Python {}",
                    markers.markers().python_full_version().version
                );
            }
            let index_hashes: Vec<_> = artifact
                .hashes
                .iter()
                .filter(|hash| hash.algorithm() != HashAlgorithm::Md5)
                .cloned()
                .collect();
            if !index_hashes.is_empty() {
                constraints.push(index_hashes);
            }
        }
        let (wheel, verified) = match dist.as_ref() {
            Dist::Source(_) if !constraints.is_empty() => {
                let (wheel, verified) = verify_source_hash_constraints(
                    &database,
                    dist,
                    &tags,
                    hashes.as_slice(),
                    &constraints,
                )
                .await?;
                (wheel, Some(verified))
            }
            Dist::Built(_) | Dist::Source(_) => {
                verify_hash_constraints(
                    &database,
                    dist,
                    &tags,
                    ArchiveHashPolicy::Any(hashes.as_slice()),
                    &constraints,
                    false,
                )
                .await?
            }
        };
        let wheel = CachedDist::from(wheel);
        let metadata = read_flat_wheel_metadata(wheel.filename(), wheel.path())?;
        if let Some(requires_python) = &metadata.requires_python
            && !requires_python.contains(&markers.markers().python_full_version().version)
        {
            bail!(
                "`{dist}` requires Python `{requires_python}`, but the selected interpreter is Python {}",
                markers.markers().python_full_version().version
            );
        }
        if let Some(verified) = verified {
            hashes = vec![verified].into();
        }
        Ok::<_, anyhow::Error>((node_index, hashes))
    };
    let mut nodes = nodes.into_iter();
    let mut pending = FuturesUnordered::new();
    for node in nodes.by_ref().take(concurrency.downloads) {
        pending.push(verify_node(node));
    }
    let mut verified_nodes = Vec::new();
    let mut failure = None;
    while let Some(result) = pending.next().await {
        match result {
            Ok(node) => verified_nodes.push(node),
            Err(err) if failure.is_none() => failure = Some(err),
            Err(_) => {}
        }
        // Stop starting new checks after a failure, but let pending checks finish before returning.
        if failure.is_none()
            && let Some(node) = nodes.next()
        {
            pending.push(verify_node(node));
        }
    }
    if let Some(err) = failure {
        return Err(err);
    }
    for (index, hashes) in verified_nodes {
        let Node::Dist {
            hashes: destination,
            ..
        } = &mut graph[index]
        else {
            bail!("Expected a locked dependency");
        };
        *destination = hashes;
    }

    let root = graph
        .node_indices()
        .find(|&index| match graph[index] {
            Node::Root => true,
            Node::Dist { .. } => false,
        })
        .context("Expected a root in the packaged lock")?;
    for node in selected.graph().node_weights() {
        if let Node::Dist { .. } = node {
            let mut selected = node.clone();
            if let Node::Dist { hashes, .. } = &mut selected
                && let Some(hash) = &root_hash
            {
                *hashes = vec![hash.clone()].into();
            }
            let node = graph.add_node(selected);
            graph.add_edge(root, node, Edge::Prod);
        }
    }
    Ok(Resolution::new(graph))
}

/// Check each source archive hash policy before its build backend can run.
async fn verify_source_hash_constraints(
    database: &DistributionDatabase<'_, BuildDispatch<'_>>,
    dist: &Dist,
    tags: &Tags,
    hashes: &[HashDigest],
    constraints: &[Vec<HashDigest>],
) -> anyhow::Result<(LocalWheel, HashDigest)> {
    let groups = ArchiveHashGroups::new(
        std::iter::once(hashes.to_vec())
            .chain(constraints.iter().cloned())
            .collect(),
    );
    let policy = ArchiveHashPolicy::AllOfAny(&groups);
    let wheel = database.get_or_build_wheel(dist, tags, policy).await?;
    if !wheel.satisfies(policy) {
        return Err(uv_distribution::Error::hash_mismatch(
            dist.to_string(),
            policy.digests(),
            wheel.hashes(),
        )
        .into());
    }
    let verified = wheel
        .hashes()
        .iter()
        .find(|hash| hashes.contains(hash))
        .context("Missing verified locked artifact hash")?
        .clone();
    Ok((wheel, verified))
}

/// Verify each constraint against the same archive as the lock, including when the constraints
/// use a different hash algorithm. Retain a verified digest to bind the later installation to it.
async fn verify_hash_constraints(
    database: &DistributionDatabase<'_, BuildDispatch<'_>>,
    dist: &Dist,
    tags: &Tags,
    policy: ArchiveHashPolicy<'_>,
    constraints: &[Vec<HashDigest>],
    generate_sha256: bool,
) -> anyhow::Result<(LocalWheel, Option<HashDigest>)> {
    let first_policy = if generate_sha256 {
        ArchiveHashPolicy::Generate
    } else if policy.requires_validation() {
        policy
    } else if let Some(first) = constraints.first() {
        ArchiveHashPolicy::Any(first)
    } else {
        ArchiveHashPolicy::Generate
    };
    let mut wheel = database
        .get_or_build_wheel(dist, tags, first_policy)
        .await?;
    if !wheel.satisfies(first_policy) {
        return Err(uv_distribution::Error::hash_mismatch(
            dist.to_string(),
            first_policy.digests(),
            wheel.hashes(),
        )
        .into());
    }
    let mut verified = wheel
        .hashes()
        .iter()
        .find(|hash| {
            first_policy.digests().contains(hash)
                || (first_policy == ArchiveHashPolicy::Generate
                    && hash.algorithm() == HashAlgorithm::Sha256)
        })
        .cloned();
    let policy_constraints = if generate_sha256 {
        match policy {
            ArchiveHashPolicy::Any(hashes) => vec![hashes],
            ArchiveHashPolicy::All(hashes) if hashes.is_empty() => vec![hashes],
            ArchiveHashPolicy::All(hashes) => hashes.iter().map(std::slice::from_ref).collect(),
            ArchiveHashPolicy::AllOfAny(groups) => {
                groups.groups().iter().map(Vec::as_slice).collect()
            }
            ArchiveHashPolicy::None | ArchiveHashPolicy::Generate => vec![],
        }
    } else {
        vec![]
    };
    for constraint in policy_constraints
        .into_iter()
        .chain(constraints.iter().map(Vec::as_slice))
    {
        if let Some(hash) = wheel.hashes().iter().find(|hash| constraint.contains(hash)) {
            if verified
                .as_ref()
                .is_none_or(|current| hash_rank(hash) > hash_rank(current))
            {
                verified = Some(hash.clone());
            }
            continue;
        }
        let binding = verified.as_ref().context("Missing verified archive hash")?;
        let mut matched = None;
        for hash in constraint {
            let expected = [binding.clone(), hash.clone()];
            let policy = ArchiveHashPolicy::All(&expected);
            if let Ok(candidate) = database.get_or_build_wheel(dist, tags, policy).await
                && candidate.satisfies(policy)
            {
                matched = Some((candidate, hash.clone()));
                break;
            }
        }
        let Some((candidate, hash)) = matched else {
            bail!("The selected artifact for `{dist}` does not match the required hashes");
        };
        wheel = candidate;
        if hash_rank(&hash) > hash_rank(binding) {
            verified = Some(hash);
        }
    }
    Ok((wheel, verified))
}

fn hash_rank(hash: &HashDigest) -> u8 {
    match hash.algorithm() {
        HashAlgorithm::Sha512 => 5,
        HashAlgorithm::Sha384 => 4,
        HashAlgorithm::Sha256 => 3,
        HashAlgorithm::Blake2b256 => 2,
        HashAlgorithm::Md5 => 1,
    }
}

fn configured_index<'a>(
    locked: Option<&uv_redacted::DisplaySafeUrl>,
    indexes: &'a IndexLocations,
) -> anyhow::Result<&'a IndexUrl> {
    let locked = locked.unwrap_or(&PYPI_URL);
    indexes
        .indexes()
        .chain(indexes.explicit_indexes())
        .find(|index| {
            index
                .url()
                .without_credentials()
                .as_str()
                .trim_end_matches('/')
                == locked.as_str().trim_end_matches('/')
        })
        .map(Index::url)
        .with_context(|| format!("Index `{locked}` from the packaged lock is not configured"))
}

/// Check the entire packaged lock against its configured indexes before accessing its artifacts.
async fn validate_artifact_urls(
    lock: &PylockToml,
    client: &RegistryClient,
    cached_client: &RegistryClient,
    indexes: &IndexLocations,
    concurrency: &Concurrency,
) -> anyhow::Result<Vec<IndexedArtifact>> {
    let capabilities = IndexCapabilities::default();
    let artifacts = futures::stream::iter(&lock.packages)
        .map(async |package| {
            let version = package.version.as_ref().with_context(|| {
                format!(
                    "`{}` must have a version to verify its files on the index",
                    package.name
                )
            })?;
            let artifacts = package.registry_artifacts().with_context(|| {
                format!(
                    "`{}=={version}` must use wheel or source distribution URLs from an index",
                    package.name
                )
            })?;

            let index = configured_index(package.index.as_ref(), indexes)?;
            if let Ok(metadata) = cached_client
                .simple_detail(
                    &package.name,
                    Some(IndexMetadataRef::from(index)),
                    &capabilities,
                    &concurrency.downloads_semaphore,
                )
                .await
                && let Ok(expected) = index_artifacts(metadata, index, &package.name, version)
                && artifacts.iter().all(|(url, filename)| {
                    let mut location = (*url).clone();
                    location.set_fragment(None);
                    expected.iter().any(|artifact| artifact.url == location && artifact.filename == *filename)
                })
            {
                return Ok(expected);
            }
            let metadata = client
                .simple_detail(
                    &package.name,
                    Some(IndexMetadataRef::from(index)),
                    &capabilities,
                    &concurrency.downloads_semaphore,
                )
                .await?;
            let expected = index_artifacts(metadata, index, &package.name, version)?;
            for (url, filename) in artifacts {
                let mut location = url.clone();
                // Simple API HTML links may include a hash fragment, which is not sent to the server.
                location.set_fragment(None);
                if !expected.iter().any(|artifact| artifact.url == location) {
                    bail!(
                        "URL for `{}=={version}` is not listed by {index}: {url}",
                        package.name,
                    );
                }
                if !expected.iter().any(|artifact| artifact.url == location && artifact.filename == filename) {
                    bail!("Filename `{filename}` for `{}=={version}` does not match the file listed at {url} by {index}", package.name);
                }
            }
            Ok::<_, anyhow::Error>(expected)
        })
        .buffer_unordered(concurrency.downloads)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(artifacts.into_iter().flatten().collect())
}

struct IndexedArtifact {
    index: uv_redacted::DisplaySafeUrl,
    filename: DistFilename,
    url: uv_redacted::DisplaySafeUrl,
    requires_python: Option<Arc<VersionSpecifiers>>,
    hashes: HashDigests,
}

fn index_artifacts(
    metadata: Vec<(&IndexUrl, MetadataFormat)>,
    configured_index: &IndexUrl,
    package: &uv_normalize::PackageName,
    version: &Version,
) -> anyhow::Result<Vec<IndexedArtifact>> {
    let mut artifacts = Vec::new();
    for (index, metadata) in metadata {
        match metadata {
            MetadataFormat::Simple(metadata) => {
                for datum in metadata.iter() {
                    if rkyv::deserialize::<Version, rkyv::rancor::Error>(&datum.version)?
                        != *version
                    {
                        continue;
                    }
                    let files =
                        rkyv::deserialize::<VersionFiles, rkyv::rancor::Error>(&datum.files)?;
                    for (filename, file) in files.all(package) {
                        let mut url = file.url.to_url()?;
                        url.set_fragment(None);
                        artifacts.push(IndexedArtifact {
                            index: configured_index.without_credentials().into_owned(),
                            filename,
                            url,
                            requires_python: file.requires_python,
                            hashes: file.hashes,
                        });
                    }
                }
            }
            MetadataFormat::Flat(_) => bail!("Expected Simple API metadata from {index}"),
        }
    }
    Ok(artifacts)
}
