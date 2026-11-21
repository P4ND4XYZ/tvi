use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
};

pub fn project_root(document: &Path, markers: &[String]) -> Result<PathBuf, String> {
    if markers
        .iter()
        .any(|name| Path::new(name).file_name() != Some(OsStr::new(name)))
    {
        return Err("root markers must be file or directory names".into());
    }
    let directory = document
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let directory = fs::canonicalize(directory)
        .map_err(|error| format!("cannot resolve document directory: {error}"))?;
    for parent in directory.ancestors() {
        for marker in markers {
            match fs::metadata(parent.join(marker)) {
                Ok(metadata) if metadata.is_file() || metadata.is_dir() => {
                    return Ok(parent.to_owned());
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("cannot check root marker: {error}")),
            }
        }
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_uses_nearest_marker_of_either_kind_or_document_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let subproject = root.join("pkg");
        let docs = subproject.join("docs");
        fs::create_dir_all(&docs)?;
        fs::write(root.join(".git"), "gitdir: other")?;
        fs::create_dir(subproject.join(".tvi-root"))?;
        let doc = docs.join("README.md");
        assert_eq!(project_root(&doc, &[".git".into()])?, root);
        assert_eq!(
            project_root(&doc, &[".git".into(), ".tvi-root".into()])?,
            subproject
        );
        assert_eq!(project_root(&doc, &[".not-a-project-marker".into()])?, docs);
        assert!(project_root(&doc, &["../.git".into()]).is_err());
        Ok(())
    }
}
