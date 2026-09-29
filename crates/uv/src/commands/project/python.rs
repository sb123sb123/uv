use uv_cache::Cache;
use uv_client::BaseClientBuilder;
use uv_python::{
    EnvironmentPreference, Interpreter, PythonDownloads, PythonInstallation, PythonPreference,
};
use uv_settings::PythonInstallMirrors;

use crate::commands::project::{ProjectError, WorkspacePython};
use crate::commands::reporters::PythonDownloadReporter;

/// An interpreter that satisfies the Python requirement used to select it.
///
/// Construction validates the interpreter against the workspace or frozen lockfile requirement,
/// including the selected dependency groups. Warning-only commands and existing environments
/// preserved by `--no-sync` do not use this type.
#[derive(Debug)]
pub(crate) struct CompatibleProjectPython(Interpreter);

impl CompatibleProjectPython {
    /// Validate a discovered interpreter before accepting it for a new project environment.
    pub(super) fn new(
        interpreter: Interpreter,
        workspace_python: &WorkspacePython,
    ) -> Result<Self, ProjectError> {
        workspace_python.validate(&interpreter)?;
        Ok(Self(interpreter))
    }

    /// Borrow the compatible interpreter.
    pub(super) fn interpreter(&self) -> &Interpreter {
        &self.0
    }

    /// Consume the compatible interpreter for use by the environment or resolver APIs.
    pub(super) fn into_interpreter(self) -> Interpreter {
        self.0
    }
}

impl WorkspacePython {
    /// Find or download an interpreter that satisfies this project's Python requirement.
    pub(super) async fn find_or_download(
        &self,
        environment_preference: EnvironmentPreference,
        python_preference: PythonPreference,
        python_downloads: PythonDownloads,
        client_builder: &BaseClientBuilder<'_>,
        cache: &Cache,
        reporter: &PythonDownloadReporter,
        install_mirrors: &PythonInstallMirrors,
    ) -> Result<CompatibleProjectPython, ProjectError> {
        let interpreter = PythonInstallation::find_or_download(
            self.python_request.as_ref(),
            environment_preference,
            python_preference,
            python_downloads,
            client_builder,
            cache,
            Some(reporter),
            install_mirrors.python_install_mirror.as_deref(),
            install_mirrors.pypy_install_mirror.as_deref(),
            install_mirrors.python_downloads_json_url.as_deref(),
        )
        .await?
        .into_interpreter();
        CompatibleProjectPython::new(interpreter, self)
    }
}
