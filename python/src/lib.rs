use copyrust::{Cancellation, Compare, CopyEngine, HashAlgorithm};
use pyo3::{exceptions::PyRuntimeError, prelude::*};
use std::path::PathBuf;

#[pyclass(name = "CopyOptions", frozen, skip_from_py_object)]
#[derive(Clone)]
struct Options {
    value: copyrust::CopyOptions,
}

#[pymethods]
impl Options {
    /// Configure file selection, comparison, and optional destination cleanup.
    #[new]
    #[pyo3(signature = (*, compare="mtime", hash_algorithm="xxh3", recursive=true, preserve_mtime=true, purge=false, case_sensitive=None, exclude_dirs=Vec::new(), exclude_files=Vec::new(), include_files=Vec::new()))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        compare: &str,
        hash_algorithm: &str,
        recursive: bool,
        preserve_mtime: bool,
        purge: bool,
        case_sensitive: Option<bool>,
        exclude_dirs: Vec<String>,
        exclude_files: Vec<String>,
        include_files: Vec<String>,
    ) -> PyResult<Self> {
        let compare = match compare {
            "mtime" => Compare::Mtime,
            "hash" => Compare::Hash,
            "auto" => Compare::Auto,
            "always" => Compare::Always,
            _ => {
                return Err(pyo3::exceptions::PyValueError::new_err(
                    "compare must be mtime, hash, auto, or always",
                ));
            }
        };
        Ok(Self {
            value: copyrust::CopyOptions {
                compare,
                hash_algorithm: match hash_algorithm {
                    "xxh3" => HashAlgorithm::Xxh3,
                    "xxh64" => HashAlgorithm::Xxh64,
                    "blake3" => HashAlgorithm::Blake3,
                    _ => {
                        return Err(pyo3::exceptions::PyValueError::new_err(
                            "hash_algorithm must be xxh3, xxh64, or blake3",
                        ));
                    }
                },
                recursive,
                preserve_mtime,
                purge,
                case_sensitive: case_sensitive.unwrap_or(!cfg!(windows)),
                exclude_dirs,
                exclude_files,
                include_files,
            },
        })
    }
}

#[pyclass(name = "CopyJob", frozen, skip_from_py_object)]
#[derive(Clone)]
struct Job {
    value: copyrust::CopyJob,
}

#[pymethods]
impl Job {
    /// Map source contents into a destination using optional per-job settings.
    #[new]
    #[pyo3(signature = (source, destination, *, options=None))]
    fn new(source: PathBuf, destination: PathBuf, options: Option<PyRef<'_, Options>>) -> Self {
        Self {
            value: copyrust::CopyJob {
                source,
                destination,
                options: options
                    .map(|options| options.value.clone())
                    .unwrap_or_default(),
            },
        }
    }

    /// Return the source directory spelling supplied by the caller.
    #[getter]
    fn source(&self) -> String {
        self.value.source.to_string_lossy().into_owned()
    }

    /// Return the destination directory spelling supplied by the caller.
    #[getter]
    fn destination(&self) -> String {
        self.value.destination.to_string_lossy().into_owned()
    }
}

#[pyclass(name = "RunOptions", frozen, skip_from_py_object)]
#[derive(Clone)]
struct RunOptions {
    value: copyrust::RunOptions,
}

#[pymethods]
impl RunOptions {
    /// Bound batch-wide worker pools and configure retries and preview mode.
    #[new]
    #[pyo3(signature = (*, scan_workers=8, hash_workers=4, copy_workers=0, retries=2, progress_interval_ms=250, dry_run=false))]
    fn new(
        scan_workers: usize,
        hash_workers: usize,
        copy_workers: usize,
        retries: usize,
        progress_interval_ms: u64,
        dry_run: bool,
    ) -> Self {
        Self {
            value: copyrust::RunOptions {
                scan_workers,
                hash_workers,
                copy_workers,
                retries,
                progress_interval_ms,
                dry_run,
            },
        }
    }
}

#[pyclass(name = "Progress", frozen)]
struct Progress {
    value: copyrust::Progress,
}

macro_rules! progress_getters {
    ($($name:ident: $kind:ty),* $(,)?) => {
        #[pymethods]
        impl Progress {
            $(
                #[doc = concat!("Return the cumulative ", stringify!($name), " counter.")]
                #[getter]
                #[allow(clippy::clone_on_copy)]
                fn $name(&self) -> $kind { self.value.$name.clone() }
            )*
            /// Serialize the event using the CLI's stable JSON field names.
            fn to_json(&self) -> PyResult<String> {
                serde_json::to_string(&self.value).map_err(|error| PyRuntimeError::new_err(format!("Failed to serialize progress: {error}")))
            }
        }
    };
}

progress_getters! {
    phase: String, elapsed_seconds: f64, scanned_files: u64, total_files: Option<u64>,
    compared_files: u64, hashed_bytes: u64, copied_files: u64, copied_bytes: u64,
    total_copy_bytes: Option<u64>, skipped_mtime: u64, skipped_hash: u64,
    skipped_same_file: u64, skipped_links: u64, deleted_files: u64, failed_files: u64,
    planned_copy_files: u64, planned_delete_files: u64, dry_run: bool, complete: bool,
}

#[pyclass(name = "Summary", frozen)]
struct Summary {
    value: copyrust::Summary,
}

#[pymethods]
impl Summary {
    /// Return whether every selected operation completed successfully.
    #[getter]
    fn success(&self) -> bool {
        self.value.success()
    }

    /// Return the final immutable progress counters.
    #[getter]
    fn progress(&self) -> Progress {
        Progress {
            value: self.value.progress.clone(),
        }
    }

    /// Return all contextual file and batch errors.
    #[getter]
    fn errors(&self) -> Vec<String> {
        self.value.errors.clone()
    }

    /// Serialize the complete result without losing error details.
    fn to_json(&self) -> PyResult<String> {
        serde_json::to_string(&self.value).map_err(|error| {
            PyRuntimeError::new_err(format!("Failed to serialize summary: {error}"))
        })
    }
}

/// Release the interpreter while native workers copy and serialize callback delivery.
#[pyfunction(name = "copy", signature = (jobs, *, options=None, on_progress=None))]
fn copy_files(
    py: Python<'_>,
    jobs: Vec<PyRef<'_, Job>>,
    options: Option<PyRef<'_, RunOptions>>,
    on_progress: Option<Py<PyAny>>,
) -> PyResult<Summary> {
    let jobs: Vec<_> = jobs.iter().map(|job| job.value.clone()).collect();
    let options = options
        .map(|options| options.value.clone())
        .unwrap_or_default();
    let result = py.detach(move || {
        CopyEngine::run(&jobs, &options, Cancellation::default(), |event| {
            Python::attach(|py| -> PyResult<()> {
                py.check_signals()?;
                if let Some(callback) = &on_progress {
                    callback.call1(
                        py,
                        (Py::new(
                            py,
                            Progress {
                                value: event.clone(),
                            },
                        )?,),
                    )?;
                }
                Ok(())
            })
            .map_err(|error| anyhow::anyhow!("Python progress callback failed: {error}"))
        })
    });
    result
        .map(|value| Summary { value })
        .map_err(|error| PyRuntimeError::new_err(format!("Copy failed: {error:#}")))
}

/// Run the shared CLI without spawning an external executable.
#[pyfunction]
fn run_cli(py: Python<'_>, args: Vec<String>) -> i32 {
    py.detach(|| copyrust_cli::Cli::run(std::iter::once("copyrust".into()).chain(args)))
}

/// Export typed native classes and functions through the Python package.
#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Options>()?;
    module.add_class::<Job>()?;
    module.add_class::<RunOptions>()?;
    module.add_class::<Progress>()?;
    module.add_class::<Summary>()?;
    module.add_function(wrap_pyfunction!(copy_files, module)?)?;
    module.add_function(wrap_pyfunction!(run_cli, module)?)?;
    Ok(())
}
