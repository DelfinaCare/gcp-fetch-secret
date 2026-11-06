//! Loading and validating the secrets YAML file.
//!
//! All validation happens up front, before anything is fetched, so a bad
//! config fails fast with a message pointing at the offending entry.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::Error;

/// One entry of the `secrets:` list in the YAML file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretParameter {
    /// Write the secret as `ENV_NAME=value` into an env file.
    /// When absent, the raw secret is the entire content of its file.
    pub env_name: Option<String>,
    /// File (relative to the output directory) to write to. When absent the
    /// default output file is used.
    pub file_name: Option<String>,
    /// Full Secret Manager version resource name, e.g.
    /// `projects/my-project/secrets/my-secret/versions/latest`.
    pub secret_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretsFile {
    secrets: Vec<SecretParameter>,
}

/// A single `NAME=value` line in an env file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvEntry {
    pub name: String,
    pub secret_id: String,
}

/// What goes into one output file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileContent {
    /// A docker `--env-file` style file with one `NAME=value` per line.
    Env(Vec<EnvEntry>),
    /// The raw bytes of a single secret.
    Raw { secret_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFile {
    pub path: PathBuf,
    pub content: FileContent,
}

impl OutputFile {
    pub fn secret_ids(&self) -> Vec<&str> {
        match &self.content {
            FileContent::Env(entries) => entries.iter().map(|e| e.secret_id.as_str()).collect(),
            FileContent::Raw { secret_id } => vec![secret_id.as_str()],
        }
    }
}

/// The validated set of files to produce, in the order they first appear in
/// the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub files: Vec<OutputFile>,
}

impl Plan {
    /// Every distinct secret id referenced by the plan, in first-seen order.
    /// The same secret used by several entries is only fetched once.
    pub fn secret_ids(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.files
            .iter()
            .flat_map(OutputFile::secret_ids)
            .filter(|id| seen.insert(*id))
            .map(str::to_owned)
            .collect()
    }
}

/// Read and validate the config at `input_file`.
pub fn load(
    input_file: &Path,
    default_output_file: &Path,
    output_dir: &Path,
) -> Result<Plan, Error> {
    let text = std::fs::read_to_string(input_file).map_err(|source| Error::ConfigRead {
        path: input_file.to_owned(),
        source,
    })?;
    parse(&text, default_output_file, output_dir).map_err(|message| Error::ConfigInvalid {
        path: input_file.to_owned(),
        message,
    })
}

/// Parse and validate config text. Returns every problem found, one per line.
pub fn parse(text: &str, default_output_file: &Path, output_dir: &Path) -> Result<Plan, String> {
    let file: SecretsFile = serde_saphyr::from_str(text).map_err(|e| e.to_string())?;
    if file.secrets.is_empty() {
        return Err("`secrets` must contain at least one entry".into());
    }

    let mut problems = Vec::new();
    // (path, entries with their index in the config)
    let mut groups: Vec<(PathBuf, Vec<(usize, &SecretParameter)>)> = Vec::new();

    for (i, secret) in file.secrets.iter().enumerate() {
        let mut ok = true;
        if let Err(p) = validate_secret_id(&secret.secret_id) {
            problems.push(format!("secrets[{i}].secret_id: {p}"));
            ok = false;
        }
        if let Some(name) = &secret.env_name
            && let Err(p) = validate_env_name(name)
        {
            problems.push(format!("secrets[{i}].env_name: {p}"));
            ok = false;
        }
        let path = match &secret.file_name {
            None => Some(default_output_file.to_owned()),
            Some(f) => match normalize_file_name(f) {
                Ok(rel) => Some(output_dir.join(rel)),
                Err(p) => {
                    problems.push(format!("secrets[{i}].file_name: {p}"));
                    None
                }
            },
        };
        let Some(path) = path.filter(|_| ok) else {
            continue;
        };
        match groups.iter_mut().find(|(p, _)| *p == path) {
            Some((_, entries)) => entries.push((i, secret)),
            None => groups.push((path, vec![(i, secret)])),
        }
    }

    let mut files = Vec::with_capacity(groups.len());
    for (path, entries) in groups {
        if let [(_, only)] = entries.as_slice()
            && only.env_name.is_none()
        {
            files.push(OutputFile {
                path,
                content: FileContent::Raw {
                    secret_id: only.secret_id.clone(),
                },
            });
            continue;
        }

        let mut names = HashSet::new();
        let mut env = Vec::with_capacity(entries.len());
        for (i, secret) in entries {
            match &secret.env_name {
                None => problems.push(format!(
                    "secrets[{i}]: has no env_name but shares {} with other secrets; \
                     a secret without env_name must be the only one written to its file",
                    path.display()
                )),
                Some(name) if !names.insert(name.as_str()) => problems.push(format!(
                    "secrets[{i}].env_name: {name} is written to {} more than once",
                    path.display()
                )),
                Some(name) => env.push(EnvEntry {
                    name: name.clone(),
                    secret_id: secret.secret_id.clone(),
                }),
            }
        }
        files.push(OutputFile {
            path,
            content: FileContent::Env(env),
        });
    }

    if problems.is_empty() {
        Ok(Plan { files })
    } else {
        Err(problems.join("\n"))
    }
}

/// Accepts `projects/P/secrets/S/versions/V` and the regional form
/// `projects/P/locations/L/secrets/S/versions/V`.
fn validate_secret_id(id: &str) -> Result<(), String> {
    let parts: Vec<&str> = id.split('/').collect();
    let shape_ok = match parts.as_slice() {
        ["projects", p, "secrets", s, "versions", v] => [p, s, v].iter().all(|x| !x.is_empty()),
        ["projects", p, "locations", l, "secrets", s, "versions", v] => {
            [p, l, s, v].iter().all(|x| !x.is_empty())
        }
        _ => false,
    };
    if shape_ok {
        Ok(())
    } else {
        Err(format!(
            "{id:?} is not a secret version name; expected \
             projects/PROJECT/secrets/SECRET/versions/VERSION (VERSION may be `latest`)"
        ))
    }
}

/// Env names end up as `NAME=value` lines, so they must not contain `=`,
/// whitespace or control characters, and must not look like a comment.
fn validate_env_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("must not be empty".into());
    }
    if name.starts_with('#') {
        return Err(format!("{name:?} must not start with '#'"));
    }
    if let Some(c) = name
        .chars()
        .find(|c| *c == '=' || c.is_whitespace() || c.is_control())
    {
        return Err(format!("{name:?} must not contain {c:?}"));
    }
    Ok(())
}

/// `file_name` must stay inside the output directory: no absolute paths and
/// no `..`. Returns the path with `.` components removed.
fn normalize_file_name(name: &str) -> Result<PathBuf, String> {
    let mut out = PathBuf::new();
    for component in Path::new(name).components() {
        match component {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => return Err(format!("{name:?} must not contain '..'")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("{name:?} must be relative to the output directory"));
            }
        }
    }
    if out.as_os_str().is_empty() || name.ends_with('/') {
        return Err(format!("{name:?} is not a file name"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID_A: &str = "projects/p/secrets/a/versions/latest";
    const ID_B: &str = "projects/p/secrets/b/versions/3";

    fn plan(yaml: &str) -> Result<Plan, String> {
        parse(yaml, Path::new("/out/default.env"), Path::new("/out/d"))
    }

    fn env(name: &str, id: &str) -> EnvEntry {
        EnvEntry {
            name: name.into(),
            secret_id: id.into(),
        }
    }

    #[test]
    fn readme_example() {
        let p = plan(
            "secrets:
  - env_name: DUMMY_SECRETa
    secret_id: projects/p/secrets/my-secret/versions/latest
  - env_name: DUMMY_SECRETb
    secret_id: projects/p/secrets/my-secret/versions/latest
  - env_name: DUMMY_SECRETc
    secret_id: projects/p/secrets/my-secret/versions/latest
",
        )
        .unwrap();
        let id = "projects/p/secrets/my-secret/versions/latest";
        assert_eq!(
            p.files,
            vec![OutputFile {
                path: "/out/default.env".into(),
                content: FileContent::Env(vec![
                    env("DUMMY_SECRETa", id),
                    env("DUMMY_SECRETb", id),
                    env("DUMMY_SECRETc", id),
                ]),
            }]
        );
        // Same secret referenced three times is fetched once.
        assert_eq!(p.secret_ids(), vec![id.to_string()]);
    }

    #[test]
    fn groups_by_file_in_first_seen_order() {
        let p = plan(&format!(
            "secrets:
  - {{env_name: A, secret_id: {ID_A}}}
  - {{file_name: key.json, secret_id: {ID_B}}}
  - {{env_name: B, file_name: other.env, secret_id: {ID_B}}}
  - {{env_name: C, secret_id: {ID_B}}}
  - {{env_name: D, file_name: ./other.env, secret_id: {ID_A}}}
"
        ))
        .unwrap();
        assert_eq!(
            p.files,
            vec![
                OutputFile {
                    path: "/out/default.env".into(),
                    content: FileContent::Env(vec![env("A", ID_A), env("C", ID_B)]),
                },
                OutputFile {
                    path: "/out/d/key.json".into(),
                    content: FileContent::Raw {
                        secret_id: ID_B.into()
                    },
                },
                OutputFile {
                    path: "/out/d/other.env".into(),
                    content: FileContent::Env(vec![env("B", ID_B), env("D", ID_A)]),
                },
            ]
        );
        assert_eq!(p.secret_ids(), vec![ID_A.to_string(), ID_B.to_string()]);
    }

    #[test]
    fn raw_secret_in_default_file() {
        let p = plan(&format!("secrets:\n  - secret_id: {ID_A}\n")).unwrap();
        assert_eq!(
            p.files[0].content,
            FileContent::Raw {
                secret_id: ID_A.into()
            }
        );
    }

    #[test]
    fn subdirectories_allowed() {
        let p = plan(&format!(
            "secrets:\n  - {{file_name: certs/tls.key, secret_id: {ID_A}}}\n"
        ))
        .unwrap();
        assert_eq!(p.files[0].path, PathBuf::from("/out/d/certs/tls.key"));
    }

    #[test]
    fn regional_secret_ids_allowed() {
        plan("secrets:\n  - secret_id: projects/p/locations/us-east1/secrets/s/versions/1\n")
            .unwrap();
    }

    fn assert_err(yaml: &str, needle: &str) {
        let err = plan(yaml).unwrap_err();
        assert!(err.contains(needle), "expected {needle:?} in error:\n{err}");
    }

    #[test]
    fn rejects_empty_and_missing_secrets() {
        assert_err("secrets: []\n", "at least one entry");
        assert_err("{}\n", "secrets");
        assert_err("", "");
    }

    #[test]
    fn rejects_unknown_fields() {
        // Typos like `envname` used to be silently ignored.
        assert_err(
            &format!("secrets:\n  - {{envname: A, secret_id: {ID_A}}}\n"),
            "envname",
        );
    }

    #[test]
    fn rejects_bad_secret_ids() {
        for id in [
            "my-secret",
            "projects/p/secrets/s",
            "projects/p/secrets/s/versions/",
            "projects//secrets/s/versions/1",
            "/projects/p/secrets/s/versions/1",
        ] {
            assert_err(
                &format!("secrets:\n  - secret_id: {id:?}\n"),
                "secrets[0].secret_id",
            );
        }
    }

    #[test]
    fn rejects_bad_env_names() {
        for name in ["", "A=B", "A B", "#A", "A\\nB"] {
            assert_err(
                &format!("secrets:\n  - {{env_name: \"{name}\", secret_id: {ID_A}}}\n"),
                "secrets[0].env_name",
            );
        }
    }

    #[test]
    fn rejects_file_names_escaping_output_dir() {
        for name in ["/etc/passwd", "../x", "a/../../x", ".", "dir/", ""] {
            assert_err(
                &format!("secrets:\n  - {{file_name: \"{name}\", secret_id: {ID_A}}}\n"),
                "secrets[0].file_name",
            );
        }
    }

    #[test]
    fn rejects_raw_secret_sharing_a_file() {
        assert_err(
            &format!("secrets:\n  - secret_id: {ID_A}\n  - secret_id: {ID_B}\n"),
            "must be the only one",
        );
        assert_err(
            &format!("secrets:\n  - {{env_name: A, secret_id: {ID_A}}}\n  - secret_id: {ID_B}\n"),
            "secrets[1]: has no env_name",
        );
    }

    #[test]
    fn rejects_duplicate_env_names_in_one_file() {
        assert_err(
            &format!(
                "secrets:\n  - {{env_name: A, secret_id: {ID_A}}}\n  - {{env_name: A, secret_id: {ID_B}}}\n"
            ),
            "more than once",
        );
        // ...but the same name in different files is fine.
        plan(&format!(
            "secrets:\n  - {{env_name: A, secret_id: {ID_A}}}\n  - {{env_name: A, file_name: x.env, secret_id: {ID_B}}}\n"
        ))
        .unwrap();
    }

    #[test]
    fn reports_all_problems_at_once() {
        let err = plan("secrets:\n  - secret_id: bad1\n  - secret_id: bad2\n").unwrap_err();
        assert!(
            err.contains("secrets[0]") && err.contains("secrets[1]"),
            "{err}"
        );
    }
}
