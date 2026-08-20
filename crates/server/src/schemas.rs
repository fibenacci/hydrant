//! Loading collection definitions from disk.
//!
//! Schemas live in the operator's git repository, not in an admin UI: releasing a field is a pull
//! request with a reviewer. This module is the boundary where those files become the validated
//! [`SchemaSet`] the service serves — and it refuses everything it cannot fully understand. There is
//! no partial-load mode, because a half-loaded set means a collection is either serving fields
//! nobody reviewed or silently missing.

use std::path::{Path, PathBuf};

use hydrant_core::{SchemaSet, schema::CollectionSchema};

/// Why the schema directory could not be turned into a [`SchemaSet`].
#[derive(Debug, thiserror::Error)]
pub enum SchemaLoadError {
    /// The directory could not be read.
    #[error("schema directory {path} cannot be read")]
    Directory {
        /// The directory that was tried.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// A file could not be read.
    #[error("schema file {path} cannot be read")]
    File {
        /// The file that was tried.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// A file was not a valid collection definition.
    #[error("{path} is not a valid collection definition: {reason}")]
    Invalid {
        /// The offending file.
        path: PathBuf,
        /// What the parser or the validation objected to.
        reason: String,
    },
    /// Two files defined the same collection, or a definition was rejected.
    #[error("schemas are inconsistent")]
    Set(#[from] hydrant_core::SchemaError),
    /// The directory held no schema files at all.
    #[error("no schema files in {path}: the service would serve nothing")]
    Empty {
        /// The directory that was searched.
        path: PathBuf,
    },
}

/// Reads every `*.yaml` and `*.yml` file in `directory` as a collection definition.
///
/// Files are read in name order, so the first error a broken directory reports is always the same
/// one — a set that fails differently on every boot is not something to debug under pressure.
///
/// # Errors
///
/// Returns [`SchemaLoadError`] if the directory cannot be read, a file is unparseable or invalid,
/// two files define the same collection, or nothing was found.
pub fn load(directory: &Path) -> Result<SchemaSet, SchemaLoadError> {
    let mut paths = Vec::new();
    let entries = std::fs::read_dir(directory).map_err(|source| SchemaLoadError::Directory {
        path: directory.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| SchemaLoadError::Directory {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|ext| ext == "yaml" || ext == "yml")
        {
            paths.push(path);
        }
    }
    paths.sort();

    if paths.is_empty() {
        return Err(SchemaLoadError::Empty {
            path: directory.to_path_buf(),
        });
    }

    let mut schemas = Vec::with_capacity(paths.len());
    for path in paths {
        let text = std::fs::read_to_string(&path).map_err(|source| SchemaLoadError::File {
            path: path.clone(),
            source,
        })?;
        let schema: CollectionSchema =
            serde_yaml_ng::from_str(&text).map_err(|error| SchemaLoadError::Invalid {
                path,
                reason: error.to_string(),
            })?;
        schemas.push(schema);
    }

    Ok(SchemaSet::new(schemas)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example schema shipped with the repository. If this stops parsing, the documented shape
    /// and the implementation have drifted apart.
    #[test]
    fn the_repositorys_example_schema_loads() {
        let set = load(Path::new("../../schemas")).expect("the example schema is valid");
        assert_eq!(set.len(), 1);

        let name = "catalog.product".parse().expect("valid collection name");
        let schema = set.get(&name).expect("catalog.product is defined");
        assert_eq!(schema.fields().len(), 6);
        assert_eq!(schema.filters().len(), 2);
        assert_eq!(schema.cache().shared_max_age, 300);
    }

    #[test]
    fn a_directory_without_schemas_is_refused() {
        let error = load(Path::new("src")).expect_err("no yaml files in src");
        assert!(matches!(error, SchemaLoadError::Empty { .. }), "{error}");
    }

    #[test]
    fn a_missing_directory_is_refused() {
        let error = load(Path::new("does-not-exist")).expect_err("no such directory");
        assert!(
            matches!(error, SchemaLoadError::Directory { .. }),
            "{error}"
        );
    }
}
