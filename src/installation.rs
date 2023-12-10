use std::{
    fmt::Display,
    io::{self, Cursor},
    path::{Path, PathBuf},
    time::Duration,
};

use crossterm::style::{Color, SetForegroundColor};
use fs_err as fs;
use indicatif::{ProgressBar, ProgressStyle};
use indoc::formatdoc;
use zip::ZipArchive;

use crate::{
    manifest::Realm,
    package_contents::PackageContents,
    package_id::PackageId,
    package_source::{PackageSourceMap, PackageSourceProvider},
    resolution::Resolve,
};

#[derive(Clone)]
pub struct InstallationContext {
    shared_dir: PathBuf,
    shared_index_dir: PathBuf,
    server_dir: PathBuf,
    server_index_dir: PathBuf,
    dev_dir: PathBuf,
    dev_index_dir: PathBuf,
}

impl InstallationContext {
    /// Create a new `InstallationContext` for the given path.
    pub fn new(project_path: &Path) -> Self {
        let shared_dir = project_path.join("packages");
        let server_dir = project_path.join("ServerPackages");
        let dev_dir = project_path.join("DevPackages");

        let shared_index_dir = shared_dir.join("_index");
        let server_index_dir = server_dir.join("_index");
        let dev_index_dir = dev_dir.join("_index");

        Self {
            shared_dir,
            shared_index_dir,
            server_dir,
            server_index_dir,
            dev_dir,
            dev_index_dir,
        }
    }

    /// Delete the existing index, if it exists.
    pub fn clean(&self) -> anyhow::Result<()> {
        fn remove_ignore_not_found(path: &Path) -> io::Result<()> {
            if let Err(err) = fs::remove_dir_all(path) {
                if err.kind() != io::ErrorKind::NotFound {
                    return Err(err);
                }
            }

            Ok(())
        }

        remove_ignore_not_found(&self.shared_dir)?;
        remove_ignore_not_found(&self.server_dir)?;
        remove_ignore_not_found(&self.dev_dir)?;

        Ok(())
    }

    /// Install all packages from the given `Resolve` into the package that this
    /// `InstallationContext` was built for.
    pub fn install(
        self,
        sources: PackageSourceMap,
        root_package_id: PackageId,
        resolved: Resolve,
    ) -> anyhow::Result<()> {
        let mut handles = Vec::new();
        let resolved_copy = resolved.clone();
        let bar = ProgressBar::new((resolved_copy.activated.len() - 1) as u64).with_style(
            ProgressStyle::with_template(
                "{spinner:.cyan.bold} {pos}/{len} [{wide_bar:.cyan/blue}]",
            )
            .unwrap()
            .tick_chars("⠁⠈⠐⠠⠄⠂ ")
            .progress_chars("#>-"),
        );
        bar.enable_steady_tick(Duration::from_millis(100));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(50)
            .enable_all()
            .build()
            .unwrap();

        for package_id in resolved_copy.activated {
            log::debug!("Installing {}...", package_id);

            let shared_deps = resolved.shared_dependencies.get(&package_id);

            // We do not need to install the root package, but we should create
            // package links for its dependencies.
            if package_id == root_package_id {
                if let Some(deps) = shared_deps {
                    self.write_root_package_links(Realm::Shared, deps, &resolved, &sources)?;
                }
            } else {
                // leaving this here for now, but we should probably remove it
                if let Some(deps) = shared_deps {
                    self.write_package_links(
                        &package_id,
                        Realm::Shared,
                        deps,
                        &resolved,
                        &sources,
                    )?;
                }

                let source_registry = resolved_copy.metadata[&package_id].source_registry.clone();
                let source_copy = sources.clone();
                let context = self.clone();
                let b = bar.clone();

                let handle = runtime.spawn_blocking(move || {
                    let package_source = source_copy.get(&source_registry).unwrap();
                    let contents = package_source.download_package(&package_id)?;
                    b.println(
                    format!(
                            "{} Downloaded {}{}",
                            SetForegroundColor(Color::DarkGreen),
                            SetForegroundColor(Color::Reset),
                            package_id
                        )
                    );
                    b.inc(1);
                    context.write_contents(&package_id, &contents, Realm::Shared)
                });

                handles.push(handle);
            }
        }

        let num_packages = handles.len();

        for handle in handles {
            runtime
                .block_on(handle)
                .expect("Package failed to be installed.")?;
        }

        bar.finish_and_clear();
        log::info!("Downloaded {} packages!", num_packages);

        Ok(())
    }

    /// Contents of a package-to-package link within the same index.
    fn link_sibling_same_index(&self, id: &PackageId, suffix: Option<&str>) -> String {
        formatdoc!(
            r#"
            return require("../../{full_name}{suffix}")
            "#,
            full_name = package_id_file_name(id),
            suffix = suffix.unwrap_or("")
        )
    }

    /// Contents of a root-to-package link within the same index.
    fn link_root_same_index(&self, id: &PackageId, suffix: Option<&str>) -> String {
        formatdoc!(
            r#"
            return require("_index/{full_name}{suffix}")
            "#,
            full_name = package_id_file_name(id),
            suffix = suffix.unwrap_or("")
        )
    }

    fn write_root_package_links<'a, K: Display>(
        &self,
        root_realm: Realm,
        dependencies: impl IntoIterator<Item = (K, &'a PackageId)>,
        resolved: &Resolve,
        sources: &PackageSourceMap,
    ) -> anyhow::Result<()> {
        log::debug!("Writing root package links");

        let base_path = match root_realm {
            Realm::Shared => &self.shared_dir,
            Realm::Server => &self.server_dir,
            Realm::Dev => &self.dev_dir,
        };

        log::trace!("Creating directory {}", base_path.display());
        fs::create_dir_all(base_path)?;

        for (dep_name, dep_package_id) in dependencies {
            let path = base_path.join(format!("{}.lua", dep_name));

            let resolved_copy = resolved.clone();
            let source_registry = resolved_copy.metadata[&dep_package_id]
                .source_registry
                .clone();
            let source_copy = sources.clone();
            let package_source = source_copy.get(&source_registry).unwrap();
            let file = package_source.download_package(&dep_package_id)?;
            let archive = ZipArchive::new(Cursor::new(file.data()))?;

            // check if this archive contains either init.luau, init.lua, src/init.luau or src/init.lua, in that order.
            let mut suffix = None;

            for file_name in archive.file_names() {
                if file_name == "init.luau" || file_name == "init.lua" {
                    suffix = Some("");
                    break;
                } else if file_name == "src/init.luau" || file_name == "src/init.lua" {
                    suffix = Some("/src");
                    // don't break here, we want to prioritize files in the root of the archive
                }
            }

            let contents = self.link_root_same_index(dep_package_id, suffix);

            log::trace!("Writing {}", path.display());
            fs::write(path, contents)?;
        }

        Ok(())
    }

    fn write_package_links<'a, K: std::fmt::Display>(
        &self,
        package_id: &PackageId,
        package_realm: Realm,
        dependencies: impl IntoIterator<Item = (K, &'a PackageId)>,
        resolved: &Resolve,
        sources: &PackageSourceMap,
    ) -> anyhow::Result<()> {
        log::debug!("Writing package links for {}", package_id);

        let mut base_path = match package_realm {
            Realm::Shared => self.shared_index_dir.clone(),
            Realm::Server => self.server_index_dir.clone(),
            Realm::Dev => self.dev_index_dir.clone(),
        };

        base_path.push(package_id_file_name(package_id));

        log::trace!("Creating directory {}", base_path.display());
        fs::create_dir_all(&base_path)?;

        let resolved_copy = resolved.clone();
        let source_registry = resolved_copy.metadata[&package_id].source_registry.clone();
        let source_copy = sources.clone();
        let package_source = source_copy.get(&source_registry).unwrap();

        for (dep_name, dep_package_id) in dependencies {
            fs::create_dir_all(&base_path.join("packages"))?;
            let path = base_path.join("packages").join(format!("{}.lua", dep_name));

            // download each package, check whether the init.luau is located in the root or in a folder called /src
            let file = package_source.download_package(&dep_package_id)?;

            let archive = ZipArchive::new(Cursor::new(file.data()))?;

            // check if this archive contains either init.luau, init.lua, src/init.luau or src/init.lua, in that order.
            let mut suffix = None;

            for file_name in archive.file_names() {
                if file_name == "init.luau" || file_name == "init.lua" {
                    suffix = Some("");
                    break;
                } else if file_name == "src/init.luau" || file_name == "src/init.lua" {
                    suffix = Some("/src");
                    // don't break here, we want to prioritize files in the root of the archive
                }
            }

            let contents = self.link_sibling_same_index(dep_package_id, suffix);

            log::trace!("Writing {}", path.display());
            fs::write(path, contents)?;
        }

        Ok(())
    }

    fn write_contents(
        &self,
        package_id: &PackageId,
        contents: &PackageContents,
        realm: Realm
    ) -> anyhow::Result<()> {
        let mut path = match realm {
            Realm::Shared => self.shared_index_dir.clone(),
            Realm::Server => self.server_index_dir.clone(),
            Realm::Dev => self.dev_index_dir.clone(),
        };

        path.push(package_id_file_name(package_id));

        fs::create_dir_all(&path)?;
        contents.unpack_into_path(&path)?;

        Ok(())
    }
}

/// Creates a suitable name for use in file paths that refer to this package.
fn package_id_file_name(id: &PackageId) -> String {
    format!(
        "{}_{}@{}",
        id.name().scope(),
        id.name().name(),
        id.version()
    )
}
