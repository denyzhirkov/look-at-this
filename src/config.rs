//! User configuration. The only module that reads the environment.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::{env, fs, io};

use serde::{Deserialize, Serialize};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Bundle ids of the apps the agent may see and control.
    #[serde(default)]
    pub allowed_apps: Vec<String>,
}

pub fn default_path() -> Result<PathBuf> {
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/look-at-this/config.toml"))
}

/// A missing file is an empty config; a malformed one is an error.
pub fn load(path: &Path) -> Result<Config> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(toml::from_str(&text)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(error.into()),
    }
}

pub fn save(path: &Path, config: &Config) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, toml::to_string(config)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        env::temp_dir().join(format!("look-at-this-{}-{name}/config.toml", std::process::id()))
    }

    #[test]
    fn missing_file_is_empty_config() {
        assert_eq!(load(&temp_path("missing")).unwrap(), Config::default());
    }

    #[test]
    fn saved_config_loads_back() {
        let path = temp_path("roundtrip");
        let config = Config { allowed_apps: vec!["com.apple.TextEdit".to_string()] };
        save(&path, &config).unwrap();
        assert_eq!(load(&path).unwrap(), config);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn malformed_or_unknown_keys_fail_loud() {
        let path = temp_path("malformed");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "alowed_apps = []").unwrap();
        assert!(load(&path).is_err());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
