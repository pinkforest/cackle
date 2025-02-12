//! This module extracts various bits of information from cargo metadata, such as which paths belong
//! to which crates, which are proc macros etc.

use crate::config::CrateName;
use anyhow::Context;
use anyhow::Result;
use cargo_metadata::camino::Utf8PathBuf;
use cargo_metadata::semver::Version;
use fxhash::FxHashMap;
use serde::Deserialize;
use serde::Serialize;
use std::borrow::Cow;
use std::fmt::Display;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Default, Debug)]
pub(crate) struct CrateIndex {
    pub(crate) manifest_path: PathBuf,
    pub(crate) package_infos: FxHashMap<PackageId, PackageInfo>,
    dir_to_pkg_id: FxHashMap<PathBuf, PackageId>,
    pkg_name_to_ids: FxHashMap<String, Vec<PackageId>>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PackageId {
    name: Arc<str>,
    version: Version,
    /// Whether this is the only version of this package present in the dependency tree. This is
    /// just used for display purposes. If the name isn't unique, then we display the version as
    /// well.
    name_is_unique: bool,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BuildScriptId {
    pub(crate) pkg_id: PackageId,
}

/// Identifies either the primary crate or the build script from a package.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CrateSel {
    Primary(PackageId),
    BuildScript(BuildScriptId),
}

#[derive(Debug)]
pub(crate) struct PackageInfo {
    pub(crate) directory: Utf8PathBuf,
    pub(crate) description: Option<String>,
    pub(crate) documentation: Option<String>,
    crate_name: CrateName,
    build_script_name: Option<CrateName>,
    is_proc_macro: bool,
}

/// The name of the environment variable that we use to pass a list of non-unique package names to
/// our subprocesses. These are packages that have multiple versions present in the output of cargo
/// metadata. Subprocesses need to know which packages are non-unique so that they can correctly
/// form PackageIds, which need this information so that we can only print package versions when
/// there are multiple versions of that package.
const MULTIPLE_VERSION_PKG_NAMES_ENV: &str = "CACKLE_MULTIPLE_VERSION_PKG_NAMES";

impl CrateIndex {
    pub(crate) fn new(dir: &Path) -> Result<Self> {
        let manifest_path = dir.join("Cargo.toml");
        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(&manifest_path)
            .exec()?;
        let mut mapping = CrateIndex {
            manifest_path,
            ..Self::default()
        };
        let mut name_counts = FxHashMap::default();
        for package in &metadata.packages {
            *name_counts.entry(&package.name).or_default() += 1;
        }
        for package in &metadata.packages {
            let pkg_id = PackageId {
                name: Arc::from(package.name.as_str()),
                version: package.version.clone(),
                name_is_unique: name_counts.get(&package.name) == Some(&1),
            };
            let mut is_proc_macro = false;
            for target in &package.targets {
                if target.kind.iter().any(|kind| kind == "proc-macro") {
                    is_proc_macro = true;
                }
            }
            if let Some(dir) = package.manifest_path.parent() {
                let crate_name: CrateName = package.name.as_str().into();
                mapping.package_infos.insert(
                    pkg_id.clone(),
                    PackageInfo {
                        directory: dir.to_path_buf(),
                        description: package.description.clone(),
                        documentation: package.documentation.clone(),
                        crate_name: crate_name.clone(),
                        build_script_name: Some(CrateName::for_build_script(&package.name)),
                        is_proc_macro,
                    },
                );
                mapping
                    .pkg_name_to_ids
                    .entry(package.name.clone())
                    .or_default()
                    .push(pkg_id.clone());
                mapping
                    .dir_to_pkg_id
                    .insert(dir.as_std_path().to_owned(), pkg_id.clone());
            }
        }
        for package_ids in mapping.pkg_name_to_ids.values_mut() {
            package_ids.sort_by_key(|pkg_id| pkg_id.version.clone());
        }
        Ok(mapping)
    }

    /// Adds an environment variable to `command` that allows subprocesses to determine whether a
    /// package name is unique.
    pub(crate) fn add_internal_env(&self, command: &mut std::process::Command) {
        let non_unique_names: Vec<&str> = self
            .package_ids()
            .filter_map(|id| {
                if id.name_is_unique {
                    None
                } else {
                    Some(id.name.as_ref())
                }
            })
            .collect();
        command.env(MULTIPLE_VERSION_PKG_NAMES_ENV, non_unique_names.join(","));
    }

    pub(crate) fn newest_package_id_with_name(&self, crate_name: &CrateName) -> Option<&PackageId> {
        self.pkg_name_to_ids
            .get(crate_name.as_ref())
            .and_then(|pkg_ids| pkg_ids.last())
    }

    pub(crate) fn package_info(&self, pkg_id: &PackageId) -> Option<&PackageInfo> {
        self.package_infos.get(pkg_id)
    }

    pub(crate) fn pkg_dir(&self, pkg_id: &PackageId) -> Option<&Path> {
        self.package_infos
            .get(pkg_id)
            .map(|info| info.directory.as_std_path())
    }

    pub(crate) fn package_ids(&self) -> impl Iterator<Item = &PackageId> {
        self.package_infos.keys()
    }

    pub(crate) fn proc_macros(&self) -> impl Iterator<Item = &PackageId> {
        self.package_infos.iter().filter_map(|(pkg_id, info)| {
            if info.is_proc_macro {
                Some(pkg_id)
            } else {
                None
            }
        })
    }

    pub(crate) fn crate_names(&self) -> impl Iterator<Item = &CrateName> {
        self.package_infos
            .values()
            .flat_map(|info| std::iter::once(&info.crate_name).chain(info.build_script_name.iter()))
    }

    /// Returns the ID of the package that contains the specified path, if any. This is used as a
    /// fallback if we can't locate a source file in the deps emitted by rustc. This can happen for
    /// example in the case of crates that compile C code, since the C code won't be in the deps
    /// file. This function however doesn't differentiate between the build script for a package and
    /// the other source files in that package, so should only be used as a fallback.
    pub(crate) fn package_id_for_path(&self, mut path: &Path) -> Option<&PackageId> {
        loop {
            if let Some(pkg_id) = self.dir_to_pkg_id.get(path) {
                return Some(pkg_id);
            }
            if let Some(parent) = path.parent() {
                path = parent;
            } else {
                return None;
            }
        }
    }
}

impl PackageId {
    pub(crate) fn from_env() -> Result<Self> {
        let name = get_env("CARGO_PKG_NAME")?;
        let version_string = get_env("CARGO_PKG_VERSION")?;
        let version = Version::parse(&version_string).with_context(|| {
            format!(
                "Package `{}` has invalid version string `{}`",
                name, version_string
            )
        })?;
        let non_unique_pkg_names = get_env(MULTIPLE_VERSION_PKG_NAMES_ENV)?;
        let name_is_unique = non_unique_pkg_names.split(',').all(|p| p != name);

        Ok(PackageId {
            name: Arc::from(name.as_str()),
            version,
            name_is_unique,
        })
    }

    pub(crate) fn version(&self) -> &Version {
        &self.version
    }

    pub(crate) fn crate_name(&self) -> Cow<str> {
        if self.name.contains('-') {
            self.name.replace('-', "_").into()
        } else {
            Cow::Borrowed(&self.name)
        }
    }
}

fn get_env(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("Failed to get environment variable {key}"))
}

impl BuildScriptId {
    pub(crate) fn from_env() -> Result<Self> {
        let pkg_id = PackageId::from_env()?;
        Ok(BuildScriptId { pkg_id })
    }
}

impl CrateSel {
    pub(crate) fn from_env() -> Result<Self> {
        let pkg_id = PackageId::from_env()?;
        let is_build_script = std::env::var("CARGO_CRATE_NAME")
            .map(|v| v.starts_with("build_script_"))
            .unwrap_or(false);
        if is_build_script {
            Ok(CrateSel::BuildScript(BuildScriptId { pkg_id }))
        } else {
            Ok(CrateSel::Primary(pkg_id))
        }
    }

    pub(crate) fn pkg_id(&self) -> &PackageId {
        match self {
            CrateSel::Primary(pkg_id) => pkg_id,
            CrateSel::BuildScript(build_script_id) => &build_script_id.pkg_id,
        }
    }
}

impl From<&PackageId> for CrateName {
    fn from(pkg_id: &PackageId) -> Self {
        CrateName(pkg_id.name.clone())
    }
}

impl Display for CrateSel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pkg_id = self.pkg_id();
        write!(f, "{}", pkg_id.name)?;
        if matches!(self, CrateSel::BuildScript(_)) {
            write!(f, ".build")?;
        }
        if !pkg_id.name_is_unique {
            write!(f, "[{}]", pkg_id.version)?;
        }
        Ok(())
    }
}

impl Display for BuildScriptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        CrateSel::BuildScript(self.clone()).fmt(f)
    }
}

impl Display for PackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        CrateSel::Primary(self.clone()).fmt(f)
    }
}

impl From<&BuildScriptId> for CrateName {
    fn from(value: &BuildScriptId) -> Self {
        CrateName::for_build_script(&value.pkg_id.name)
    }
}

impl From<&CrateSel> for CrateName {
    fn from(value: &CrateSel) -> Self {
        match value {
            CrateSel::Primary(pkg_id) => pkg_id.into(),
            CrateSel::BuildScript(build_script_id) => build_script_id.into(),
        }
    }
}

impl PackageId {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::BuildScriptId;
    use super::CrateIndex;
    use super::PackageId;
    use super::PackageInfo;
    use crate::config::CrateName;
    use cargo_metadata::semver::Version;
    use std::sync::Arc;

    pub(crate) fn pkg_id(name: &str) -> PackageId {
        PackageId {
            name: Arc::from(name),
            version: Version::new(0, 0, 0),
            name_is_unique: true,
        }
    }

    pub(crate) fn build_script_id(name: &str) -> BuildScriptId {
        BuildScriptId {
            pkg_id: pkg_id(name),
        }
    }

    pub(crate) fn index_with_package_names(package_names: &[&str]) -> Arc<CrateIndex> {
        let package_infos = package_names
            .iter()
            .map(|name| {
                (
                    pkg_id(name),
                    PackageInfo {
                        directory: Default::default(),
                        description: Default::default(),
                        documentation: Default::default(),
                        crate_name: CrateName(Arc::from(*name)),
                        build_script_name: Default::default(),
                        is_proc_macro: Default::default(),
                    },
                )
            })
            .collect();
        Arc::new(CrateIndex {
            package_infos,
            ..CrateIndex::default()
        })
    }
}
