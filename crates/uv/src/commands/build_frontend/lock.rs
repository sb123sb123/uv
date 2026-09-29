use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use uv_configuration::{DependencyGroupsWithDefaults, ExtrasSpecification, InstallOptions};
use uv_dispatch::BuildDispatch;
use uv_distribution::DistributionDatabase;
use uv_distribution_types::{
    BuildableSource, DirectorySourceUrl, MetadataHashPolicy, Requirement, SourceUrl,
};
use uv_fs::is_same_file_allow_missing;
use uv_lock::{Installable, Lock, PylockToml};
use uv_normalize::{DefaultExtras, ExtraName, PackageName};
use uv_pep440::{Version, VersionSpecifiers, release_specifiers_to_ranges};
use uv_preview::{Preview, PreviewFeature};
use uv_pypi_types::ResolutionMetadata;
use uv_static::{EnvVars, parse_boolish_environment_variable};
use uv_workspace::Workspace;
use version_ranges::Ranges;

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Pyproject {
    project: Option<Project>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Project {
    name: PackageName,
    version: Option<Version>,
    requires_python: Option<VersionSpecifiers>,
    #[serde(default)]
    dynamic: Vec<String>,
}

pub(super) struct ExportedLock {
    pub(super) path: PathBuf,
    pub(super) contents: String,
    pub(super) pylock: String,
    pub(super) required: bool,
    root: PathBuf,
    lock: Lock,
    name: PackageName,
    version: Option<Version>,
    requires_dist: Vec<Requirement>,
    provides_extra: Vec<ExtraName>,
}

impl ExportedLock {
    pub(super) fn matches_wheel(&self, metadata: ResolutionMetadata) -> Result<bool> {
        if metadata.name != self.name
            || self
                .version
                .as_ref()
                .is_some_and(|version| version != &metadata.version)
        {
            return Ok(false);
        }
        let python = metadata
            .requires_python
            .map(release_specifiers_to_ranges)
            .unwrap_or_else(Ranges::full);
        let locked_python =
            release_specifiers_to_ranges(self.lock.requires_python().specifiers().clone());
        if !python.subset_of(&locked_python) {
            return Ok(false);
        }
        let requires_dist = metadata
            .requires_dist
            .into_vec()
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        let provides_extra: BTreeSet<_> = metadata.provides_extra.iter().collect();
        let expected_extras: BTreeSet<_> = self.provides_extra.iter().collect();
        Ok(provides_extra == expected_extras
            && self
                .lock
                .matches_requirements(&self.root, &self.requires_dist, &requires_dist)?)
    }
}

/// Export the runtime dependencies of the project supplied by the distribution.
pub(super) async fn export(
    source_tree: &Path,
    workspace: Option<&Workspace>,
    database: &DistributionDatabase<'_, BuildDispatch<'_>>,
    preview: Preview,
) -> Result<Option<ExportedLock>> {
    let environment_export = parse_boolish_environment_variable(EnvVars::UV_EXPORT_LOCK)?;
    let pyproject_path = source_tree.join("pyproject.toml");
    let pyproject_contents = match fs_err::read_to_string(&pyproject_path) {
        Ok(contents) => contents,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound && environment_export == Some(true) =>
        {
            if !preview.is_enabled(PreviewFeature::LockedTools) {
                bail!("Exporting locks requires the `locked-tools` preview feature");
            }
            bail!("Cannot export a lock without project metadata");
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let raw: toml::Value = toml::from_str(&pyproject_contents)
        .with_context(|| format!("Failed to parse {}", pyproject_path.display()))?;
    let uv_build = raw
        .get("build-system")
        .and_then(|system| system.get("build-backend"))
        .and_then(toml::Value::as_str)
        == Some("uv_build");
    let configured = raw
        .get("tool")
        .and_then(|tool| tool.get("uv"))
        .and_then(|uv| uv.get("export-lock"))
        .map(|value| {
            value
                .as_bool()
                .context("The project setting `tool.uv.export-lock` must be a boolean")
        })
        .transpose()?;
    let enabled = environment_export.or(configured);
    if enabled == Some(false) || (enabled.is_none() && !uv_build) {
        return Ok(None);
    }
    if !preview.is_enabled(PreviewFeature::LockedTools) {
        if enabled == Some(true) {
            bail!("Exporting locks requires the `locked-tools` preview feature");
        }
        return Ok(None);
    }
    let result = async {
    let pyproject: Pyproject = raw.try_into()?;
    let Some(project) = pyproject.project else {
        bail!("Cannot export a lock without project metadata");
    };
    let root = workspace
        .filter(|workspace| {
            workspace.packages().values().any(|package| {
                is_same_file_allow_missing(package.root(), source_tree).unwrap_or(false)
            })
        })
        .map_or(source_tree, |workspace| workspace.install_path().as_path());
    let path = root.join("uv.lock");
    let contents = match fs_err::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound && enabled.is_none() => {
            return Ok(None);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!("Cannot export a lock: `uv.lock` was not found; run `uv lock` before building")
        }
        Err(error) => return Err(error.into()),
    };
    let lock = match Lock::from_toml(&contents) {
        Ok(lock) => lock,
        Err(error) if enabled.is_none() => {
            tracing::debug!("Not exporting an invalid `uv.lock`: {error}");
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    let package = match lock.find_by_name(&project.name) {
        Ok(Some(package)) => package,
        result if enabled.is_none() => {
            tracing::debug!(
                "Not exporting `uv.lock`: project `{}` was not uniquely found: {result:?}",
                project.name
            );
            return Ok(None);
        }
        Ok(None) => bail!("`uv.lock` does not contain `{}`", project.name),
        Err(error) => return Err(anyhow::Error::msg(error)),
    };
    let project_metadata =
        uv_pypi_types::PyProjectToml::from_toml(&pyproject_contents, pyproject_path.display())?;
    let unlowered = uv_pypi_types::RequiresDist::from_pyproject_toml(project_metadata.clone());
    let requirements = match database.requires_dist(source_tree, &project_metadata).await {
        Ok(requirements) => requirements
            .map(|requirements| (requirements.requires_dist, requirements.provides_extra)),
        Err(error) if source_tree.join("PKG-INFO").is_file() => {
            // A source distribution need not contain the other members of the original workspace.
            // Compare its static declarations directly when workspace discovery cannot lower them.
            tracing::debug!("Could not lower source distribution requirements: {error}");
            let requirements = uv_pypi_types::RequiresDist::from_pyproject_toml(project_metadata)?;
            Some((
                requirements
                    .requires_dist
                    .into_vec()
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                requirements.provides_extra,
            ))
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = if project.version.is_none()
        || requirements.is_none()
        || project
            .dynamic
            .iter()
            .any(|field| field == "requires-python")
    {
        let url = url::Url::from_directory_path(source_tree)
            .map(uv_redacted::DisplaySafeUrl::from_url)
            .map_err(|()| anyhow::anyhow!("Failed to convert project path to a URL"))?;
        let source = BuildableSource::Url(SourceUrl::Directory(DirectorySourceUrl {
            url: &url,
            install_path: source_tree,
            editable: None,
        }));
        Some(
            database
                .build_wheel_metadata(&source, MetadataHashPolicy::default())
                .await?
                .metadata,
        )
    } else {
        None
    };
    if metadata
        .as_ref()
        .is_some_and(|metadata| metadata.name != project.name)
    {
        bail!(
            "The build backend reported a different project name for `{}`",
            project.name
        );
    }
    let version = project
        .version
        .as_ref()
        .or_else(|| metadata.as_ref().map(|metadata| &metadata.version));
    if (project.version.is_some() || package.version().is_some()) && package.version() != version {
        if enabled.is_none() {
            return Ok(None);
        }
        bail!(
            "`uv.lock` does not match the version of `{}`; run `uv lock` before building",
            project.name
        );
    }
    let requirements = if let Some(requirements) = requirements {
        requirements
    } else if let Some(metadata) = metadata.as_ref() {
        let requirements =
            uv_distribution::RequiresDist::from(metadata.clone().with_force_relative(false));
        (requirements.requires_dist, requirements.provides_extra)
    } else {
        bail!("Cannot verify the project's dependency declarations against `uv.lock`");
    };
    if !lock.matches_package_requirements(root, package, &requirements.0, &requirements.1)? {
        if enabled.is_none() {
            return Ok(None);
        }
        bail!(
            "`uv.lock` does not match the dependencies of `{}`; run `uv lock` before building",
            project.name
        );
    }
    let target = BuildLock {
        root,
        lock: &lock,
        name: &project.name,
    };
    let install_options =
        InstallOptions::new(true, false, false, false, false, false, vec![], vec![]);
    let extras = ExtrasSpecification::default().with_defaults(DefaultExtras::default());
    let groups = DependencyGroupsWithDefaults::none();
    if let Some(dependency) =
        PylockToml::workspace_dependency(&target, &extras, &groups, &install_options)?
    {
        if enabled.is_none() {
            return Ok(None);
        }
        bail!("Cannot export a lock with workspace dependency `{dependency}`");
    }
    let project_python = project
        .requires_python
        .or_else(|| {
            metadata
                .as_ref()
                .and_then(|metadata| metadata.requires_python.clone())
        })
        .map(release_specifiers_to_ranges)
        .unwrap_or_else(Ranges::full);
    let locked_python = release_specifiers_to_ranges(lock.requires_python().specifiers().clone());
    if !project_python.subset_of(&locked_python) {
        if enabled.is_none() {
            return Ok(None);
        }
        bail!(
            "Cannot export a lock that does not cover the Python versions supported by `{}`",
            project.name
        );
    }
    if !lock.supported_environments().is_empty() {
        if enabled.is_none() {
            return Ok(None);
        }
        bail!("Cannot export a lock restricted to specific environments");
    }
    if enabled.is_none() && !lock.has_only_pypi_and_workspace_sources() {
        return Ok(None);
    }
    let pylock = PylockToml::from_lock(
        &target,
        root,
        &[],
        &extras,
        &groups,
        false,
        None,
        &install_options,
    )?;
    if pylock.has_missing_hashes() {
        if enabled.is_none() {
            return Ok(None);
        }
        bail!(
            "Cannot export a lock with missing artifact hashes; regenerate `uv.lock` with artifact hashes before building"
        );
    }
    if pylock.has_non_registry_sources() {
        if enabled.is_none() {
            return Ok(None);
        }
        bail!("Cannot export a lock with dependencies that are not registry distributions");
    }
    let (requires_dist, provides_extra) = match unlowered {
        Ok(requirements) => (
            requirements
                .requires_dist
                .into_vec()
                .into_iter()
                .map(Into::into)
                .collect(),
            requirements.provides_extra.into_vec(),
        ),
        Err(error) => {
            let metadata = metadata
                .as_ref()
                .with_context(|| format!("Cannot read dynamic project metadata: {error}"))?;
            (
                metadata.requires_dist.to_vec(),
                metadata.provides_extra.to_vec(),
            )
        }
    };
    Ok(Some(ExportedLock {
        path,
        contents,
        pylock: pylock.to_toml()?,
        required: enabled == Some(true),
        root: root.to_path_buf(),
        name: project.name,
        version: version.cloned(),
        requires_dist,
        provides_extra,
        lock,
    }))
    }.await;
    match result {
        Err(error) if enabled.is_none() => {
            tracing::debug!("Not exporting the lock because it could not be verified: {error}");
            Ok(None)
        }
        result => result,
    }
}

struct BuildLock<'lock> {
    root: &'lock Path,
    lock: &'lock Lock,
    name: &'lock PackageName,
}

impl<'lock> Installable<'lock> for BuildLock<'lock> {
    fn install_path(&self) -> &'lock Path {
        self.root
    }
    fn lock(&self) -> &'lock Lock {
        self.lock
    }
    fn roots(&self) -> impl Iterator<Item = &PackageName> {
        std::iter::once(self.name)
    }
    fn project_name(&self) -> Option<&PackageName> {
        Some(self.name)
    }
}
