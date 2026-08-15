use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use koharu_runtime::RuntimeManager;

#[allow(warnings)]
mod generated {
    #[allow(warnings)]
    pub mod types {
        include!(concat!(env!("OUT_DIR"), "/types.rs"));
    }

    #[allow(warnings)]
    pub mod llama {
        use super::types::*;
        include!(concat!(env!("OUT_DIR"), "/llama_loader.rs"));
    }

    #[allow(warnings)]
    pub mod ggml {
        use super::types::*;
        include!(concat!(env!("OUT_DIR"), "/ggml_loader.rs"));
    }

    #[allow(warnings)]
    pub mod ggml_base {
        use super::types::*;
        include!(concat!(env!("OUT_DIR"), "/ggml_base_loader.rs"));
    }

    #[allow(warnings)]
    pub mod mtmd {
        use super::types::*;
        include!(concat!(env!("OUT_DIR"), "/mtmd_loader.rs"));
    }
}

pub use generated::types::*;

struct LoadedLibraries {
    path: PathBuf,
    wants_gpu: bool,
    llama: generated::llama::llama,
    ggml: generated::ggml::ggml,
    ggml_base: generated::ggml_base::ggml_base,
    mtmd: generated::mtmd::mtmd,
}

#[cfg(target_os = "windows")]
const LIB_NAMES: [&str; 4] = ["ggml-base.dll", "ggml.dll", "llama.dll", "mtmd.dll"];

#[cfg(target_os = "linux")]
const LIB_NAMES: [&str; 4] = ["libggml-base.so", "libggml.so", "libllama.so", "libmtmd.so"];

#[cfg(target_os = "macos")]
const LIB_NAMES: [&str; 4] = [
    "libggml-base.dylib",
    "libggml.dylib",
    "libllama.dylib",
    "libmtmd.dylib",
];

static LIBRARIES: OnceLock<LoadedLibraries> = OnceLock::new();

pub fn initialize(runtime: &RuntimeManager) -> Result<()> {
    let runtime_dir = runtime.llama_directory().context(
        "failed to resolve the llama runtime directory; call `koharu_runtime::prepare()` first",
    )?;

    if !runtime_dir.exists() {
        bail!(
            "runtime directory `{}` does not exist; call `koharu_runtime::prepare()` first",
            runtime_dir.display()
        );
    }

    let dir = runtime_dir
        .canonicalize()
        .with_context(|| format!("failed to canonicalize `{}`", runtime_dir.display()))?;

    let wants_gpu = runtime.wants_gpu();
    if let Some(existing) = LIBRARIES.get() {
        if existing.path == dir && existing.wants_gpu == wants_gpu {
            return Ok(());
        }
        bail!(
            "koharu-llm is already initialized with `{}` (gpu={}) and cannot be reinitialized with `{}` (gpu={})",
            existing.path.display(),
            existing.wants_gpu,
            dir.display(),
            wants_gpu,
        );
    }

    let libraries = load_libraries(&dir, wants_gpu)?;
    register_backends(&libraries.ggml, &dir, wants_gpu)?;

    LIBRARIES
        .set(libraries)
        .map_err(|_| anyhow!("koharu-llm runtime libraries were initialized concurrently"))?;

    Ok(())
}

fn load_libraries(dir: &Path, wants_gpu: bool) -> Result<LoadedLibraries> {
    let [ggml_base_name, ggml_name, llama_name, mtmd_name] = LIB_NAMES;

    let ggml_base = load_and_bind(&dir.join(ggml_base_name), ggml_base_name, |lib| unsafe {
        generated::ggml_base::ggml_base::from_library(lib)
    })?;
    let ggml = load_and_bind(&dir.join(ggml_name), ggml_name, |lib| unsafe {
        generated::ggml::ggml::from_library(lib)
    })?;
    let llama = load_and_bind(&dir.join(llama_name), llama_name, |lib| unsafe {
        generated::llama::llama::from_library(lib)
    })?;
    let mtmd = load_and_bind(&dir.join(mtmd_name), mtmd_name, |lib| unsafe {
        generated::mtmd::mtmd::from_library(lib)
    })?;

    Ok(LoadedLibraries {
        path: dir.to_path_buf(),
        wants_gpu,
        llama,
        ggml,
        ggml_base,
        mtmd,
    })
}

fn load_and_bind<T>(
    path: &Path,
    name: &str,
    bind: impl FnOnce(libloading::Library) -> std::result::Result<T, libloading::Error>,
) -> Result<T> {
    // On Windows, DLL search paths are configured via add_runtime_search_path,
    // On Linux and macOS, we load libraries by path to ensure we get the correct ones.
    #[cfg(target_os = "windows")]
    let _ = path;
    #[cfg(target_os = "windows")]
    let library = koharu_runtime::load_library_by_name(name)
        .with_context(|| format!("failed to load `{name}`"))?;
    #[cfg(not(target_os = "windows"))]
    let library = koharu_runtime::load_library_by_path(path)
        .with_context(|| format!("failed to load `{name}`"))?;
    bind(library).with_context(|| format!("failed to bind `{name}`"))
}

#[derive(Debug, PartialEq, Eq)]
enum BackendLoadPlan {
    All,
    One(PathBuf),
}

fn plan_backend_load(
    wants_gpu: bool,
    candidates: impl IntoIterator<Item = (PathBuf, i32)>,
) -> BackendLoadPlan {
    if wants_gpu {
        return BackendLoadPlan::All;
    }

    candidates
        .into_iter()
        .filter(|(path, score)| is_cpu_backend(path) && *score > 0)
        .max_by_key(|(_, score)| *score)
        .map(|(path, _)| BackendLoadPlan::One(path))
        .unwrap_or_else(|| BackendLoadPlan::One(PathBuf::new()))
}

fn is_cpu_backend(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    (name.starts_with("libggml-cpu") && (name.ends_with(".so") || name.ends_with(".dylib")))
        || (name.starts_with("ggml-cpu") && name.ends_with(".dll"))
}

fn cpu_backend_candidates(dir: &Path) -> Result<Vec<(PathBuf, i32)>> {
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("failed to list backend directory `{}`", dir.display()))?
    {
        let path = entry?.path();
        if !path.is_file() || !is_cpu_backend(&path) {
            continue;
        }
        let score = match koharu_runtime::load_library_by_path(&path) {
            Ok(library) => unsafe {
                type ScoreFn = unsafe extern "C" fn() -> i32;
                library
                    .get::<ScoreFn>(b"ggml_backend_score\0")
                    .map(|score| score())
                    .unwrap_or_else(|_| if is_base_cpu_backend(&path) { 1 } else { 0 })
            },
            Err(_) => 0,
        };
        candidates.push((path, score));
    }
    Ok(candidates)
}

fn is_base_cpu_backend(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("libggml-cpu.so" | "libggml-cpu.dylib" | "ggml-cpu.dll")
    )
}

fn register_backends(ggml: &generated::ggml::ggml, dir: &Path, wants_gpu: bool) -> Result<()> {
    let dir = dir
        .to_str()
        .ok_or_else(|| anyhow!("runtime directory is not valid UTF-8"))?;
    let plan = if wants_gpu {
        BackendLoadPlan::All
    } else {
        plan_backend_load(false, cpu_backend_candidates(Path::new(dir))?)
    };
    match plan {
        BackendLoadPlan::All => {
            let dir =
                CString::new(dir).context("runtime directory contains an interior null byte")?;
            unsafe {
                ggml.ggml_backend_load_all_from_path(dir.as_ptr());
            }
        }
        BackendLoadPlan::One(path) if path.as_os_str().is_empty() => {
            bail!("no compatible CPU backend found in `{dir}`");
        }
        BackendLoadPlan::One(path) => {
            let path = path
                .to_str()
                .context("CPU backend path is not valid UTF-8")?;
            let path =
                CString::new(path).context("CPU backend path contains an interior null byte")?;
            let registration = unsafe { ggml.ggml_backend_load(path.as_ptr()) };
            if registration.is_null() {
                bail!(
                    "failed to register CPU backend `{}`",
                    path.to_string_lossy()
                );
            }
        }
    }

    Ok(())
}

fn libraries() -> &'static LoadedLibraries {
    LIBRARIES.get().expect(
        "koharu-llm runtime libraries are not initialized; call `koharu_runtime::prepare()` first",
    )
}

fn llama_lib() -> &'static generated::llama::llama {
    &libraries().llama
}

fn ggml_lib() -> &'static generated::ggml::ggml {
    &libraries().ggml
}

fn ggml_base_lib() -> &'static generated::ggml_base::ggml_base {
    &libraries().ggml_base
}

fn mtmd_lib() -> &'static generated::mtmd::mtmd {
    &libraries().mtmd
}

#[allow(warnings)]
mod wrappers {
    use super::*;
    include!(concat!(env!("OUT_DIR"), "/wrappers.rs"));
}

pub use wrappers::*;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{BackendLoadPlan, plan_backend_load};

    #[test]
    fn cpu_plan_ignores_accelerators_and_selects_highest_scoring_cpu_plugin() {
        let candidates = [
            (PathBuf::from("libggml-vulkan.so"), 100),
            (PathBuf::from("libggml-cpu-sse42.so"), 10),
            (PathBuf::from("libggml-cpu-alderlake.so"), 20),
        ];

        assert_eq!(
            plan_backend_load(false, candidates),
            BackendLoadPlan::One(PathBuf::from("libggml-cpu-alderlake.so"))
        );
    }

    #[test]
    fn gpu_plan_keeps_all_backend_discovery() {
        let candidates = [(PathBuf::from("libggml-cpu-alderlake.so"), 20)];

        assert_eq!(plan_backend_load(true, candidates), BackendLoadPlan::All);
    }
}
