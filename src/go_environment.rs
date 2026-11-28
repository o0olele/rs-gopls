use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::process::Command;
use std::sync::Arc;

use serde_json::Value;

use crate::workspace_roots::GoBuildEnvironmentCache;

pub(crate) type EnvironmentOverrides = BTreeMap<String, String>;

#[derive(Clone)]
pub(crate) struct GoEnvironment {
    startup: Arc<BTreeMap<OsString, OsString>>,
    variables: Arc<BTreeMap<OsString, OsString>>,
    allows_module_auto: bool,
    pub(crate) workspace_build_environments: Arc<GoBuildEnvironmentCache>,
}

impl GoEnvironment {
    pub(crate) fn capture() -> Self {
        Self::from_startup(std::env::vars_os().map(|(key, value)| (environment_key(key), value)))
    }

    fn from_startup(variables: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        let startup = Arc::new(variables.into_iter().collect());
        Self {
            startup: Arc::clone(&startup),
            variables: startup,
            allows_module_auto: false,
            workspace_build_environments: Arc::default(),
        }
    }

    pub(crate) fn with_overrides(&self, overrides: EnvironmentOverrides) -> Self {
        let overrides: BTreeMap<_, _> = overrides
            .into_iter()
            .map(|(key, value)| (environment_key(key.into()), OsString::from(value)))
            .collect();
        let nonempty = |key: &str| {
            overrides
                .get(OsStr::new(key))
                .is_some_and(|value| !value.is_empty())
        };
        let allows_module_auto = nonempty("GOPATH") && !nonempty("GO111MODULE");
        let variables = if overrides.is_empty() {
            Arc::clone(&self.startup)
        } else {
            let mut variables = self.startup.as_ref().clone();
            variables.extend(overrides);
            Arc::new(variables)
        };
        let workspace_build_environments = if variables == self.variables {
            Arc::clone(&self.workspace_build_environments)
        } else {
            Arc::default()
        };
        Self {
            startup: Arc::clone(&self.startup),
            variables,
            allows_module_auto,
            workspace_build_environments,
        }
    }

    pub(crate) fn variables(&self) -> &Arc<BTreeMap<OsString, OsString>> {
        &self.variables
    }

    pub(crate) fn get(&self, key: &str) -> Option<&OsStr> {
        let key = environment_key(key.into());
        self.variables.get(&key).map(OsString::as_os_str)
    }

    pub(crate) fn allows_module_auto(&self) -> bool {
        self.allows_module_auto
    }

    pub(crate) fn command(&self, program: impl AsRef<OsStr>) -> io::Result<Command> {
        let program = program.as_ref();
        #[cfg(windows)]
        let resolved = self.resolve_windows_program(program)?;
        #[cfg(windows)]
        let program = resolved.as_deref().unwrap_or(program);
        let mut command = Command::new(program);
        command.env_clear().envs(self.variables.iter());
        Ok(command)
    }

    #[cfg(windows)]
    fn resolve_windows_program(&self, program: &OsStr) -> io::Result<Option<OsString>> {
        use std::path::Path;

        let path = self.get("PATH");
        if path
            == self
                .startup
                .get(OsStr::new("PATH"))
                .map(OsString::as_os_str)
            || Path::new(program).file_name() != Some(program)
        {
            return Ok(None);
        }
        // Windows Command can fall through to the server's PATH after searching
        // the child PATH. Resolve configured tools before spawning the command.
        for directory in path.into_iter().flat_map(std::env::split_paths) {
            if directory.as_os_str().is_empty() {
                continue;
            }
            let mut candidate = directory.join(program);
            if candidate.extension().is_none() {
                candidate.set_extension("exe");
            }
            if candidate.is_file() {
                return Ok(Some(std::path::absolute(candidate)?.into_os_string()));
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{program:?} was not found on the configured PATH"),
        ))
    }
}

impl PartialEq for GoEnvironment {
    fn eq(&self, other: &Self) -> bool {
        self.allows_module_auto == other.allows_module_auto && self.variables == other.variables
    }
}

impl Eq for GoEnvironment {}

fn environment_key(mut key: OsString) -> OsString {
    if cfg!(windows) {
        key.make_ascii_uppercase();
    }
    key
}

pub(crate) fn environment_overrides(settings: &Value) -> Option<EnvironmentOverrides> {
    ["/env", "/rs-gopls/env", "/gopls/env", "/settings/env"]
        .into_iter()
        .find_map(|pointer| settings.pointer(pointer)?.as_object())
        .map(|env| {
            env.iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_the_first_environment_object_and_only_keeps_strings() {
        let entries = json!({ "Z": "last", "NUMBER": 1, "NULL": null, "GOWORK": "", "A": "first" });
        for settings in [
            json!({ "env": entries, "rs-gopls": { "env": { "GOWORK": "ignored" } } }),
            json!({ "env": [1], "rs-gopls": { "env": entries }, "gopls": { "env": {} } }),
            json!({ "rs-gopls": { "env": false }, "gopls": { "env": entries }, "settings": { "env": {} } }),
            json!({ "env": null, "settings": { "env": entries } }),
        ] {
            let extracted: Vec<_> = environment_overrides(&settings)
                .unwrap()
                .into_iter()
                .collect();
            assert_eq!(
                extracted,
                [
                    ("A".into(), "first".into()),
                    ("GOWORK".into(), "".into()),
                    ("Z".into(), "last".into())
                ]
            );
        }
        for settings in [
            Value::Null,
            json!({}),
            json!({ "env": null }),
            json!({ "settings": { "other": 0 } }),
        ] {
            assert!(environment_overrides(&settings).is_none());
        }
        assert_eq!(
            environment_overrides(&json!({ "env": {}, "gopls": { "env": entries } })),
            Some(BTreeMap::new())
        );
    }

    #[test]
    fn overlays_and_restores_inherited_binary_environment() {
        let startup = GoEnvironment::from_startup([
            ("GOPATH".into(), "inherited".into()),
            ("GOWORK".into(), "off".into()),
            ("GOTOOLCHAIN".into(), "local".into()),
        ]);
        let env = startup.with_overrides(
            environment_overrides(&json!({ "env": { "GOPATH": "configured", "GOWORK": "" } }))
                .unwrap(),
        );
        assert!(env.allows_module_auto());
        assert_eq!(env.get("GOPATH"), Some(OsStr::new("configured")));
        assert_eq!(env.get("GOTOOLCHAIN"), Some(OsStr::new("local")));
        let command = env.command("go").unwrap();
        assert_eq!(
            command.get_envs().find(|(key, _)| *key == "GOWORK"),
            Some((OsStr::new("GOWORK"), Some(OsStr::new(""))))
        );

        let replacement =
            env.with_overrides(BTreeMap::from([("GOFLAGS".into(), "-tags=x".into())]));
        assert_eq!(replacement.get("GOPATH"), startup.get("GOPATH"));
        assert_eq!(replacement.get("GOWORK"), startup.get("GOWORK"));
        assert!(!replacement.allows_module_auto());
        let cleared = replacement.with_overrides(BTreeMap::new());
        assert!(cleared == startup);
    }

    #[test]
    fn compares_effective_variables_and_the_module_fallback_gate() {
        let startup = GoEnvironment::from_startup([
            ("GOPATH".into(), "inherited".into()),
            ("GOWORK".into(), "off".into()),
        ]);
        let unchanged = startup.with_overrides(BTreeMap::from([("GOWORK".into(), "off".into())]));
        assert!(startup == unchanged);
        let gopath =
            startup.with_overrides(BTreeMap::from([("GOPATH".into(), "inherited".into())]));
        assert_eq!(startup.variables(), gopath.variables());
        assert!(startup != gopath);
        assert!(gopath.allows_module_auto());
        for mode in ["on", "off", "auto", ""] {
            let configured = gopath.with_overrides(BTreeMap::from([
                ("GOPATH".into(), "inherited".into()),
                ("GO111MODULE".into(), mode.into()),
            ]));
            assert_eq!(configured.allows_module_auto(), mode.is_empty());
        }
    }

    #[cfg(windows)]
    #[test]
    fn configured_windows_path_wins_over_inherited_path() {
        let project = crate::test_support::TestProject::new();
        let tool = project.write("go.exe", "tool marker");
        let startup = GoEnvironment::capture();
        let configured = startup.with_overrides(BTreeMap::from([(
            "Path".into(),
            project.root.to_str().unwrap().into(),
        )]));
        assert_eq!(configured.command("go").unwrap().get_program(), tool);
        let empty = startup.with_overrides(BTreeMap::from([("PATH".into(), String::new())]));
        assert_eq!(
            empty.command("go").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
